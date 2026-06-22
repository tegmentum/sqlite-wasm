//! Custom `sqlite3_mem_methods` implementation.
//!
//! ## Layered plan (PLAN-tvm-integration Phase 2)
//!
//! 1. **Phase 2.0 (this code):** the seven `sqlite3_mem_methods`
//!    trampolines + a size-header allocator backed by the Rust
//!    global allocator. Registration via
//!    `sqlite3_config(SQLITE_CONFIG_MALLOC, …)` before
//!    `sqlite3_initialize` (same boot-order constraint as
//!    `SQLITE_CONFIG_LOG` and `SQLITE_CONFIG_PCACHE2`).
//!
//! 2. **Phase 2.1 (deferred):** TVM-backed allocator. Structurally
//!    blocked: SQLite uses raw C pointers throughout the
//!    sqlite3_mem_methods boundary  every allocation leaks out
//!    as a `*mut c_void` that the caller dereferences. TVM's
//!    `tvm:memory/manager.alloc` returns an opaque `handle`, not
//!    a pointer into default memory. Phase 1's shadow-pool
//!    workaround (fetch into a default-memory slot, flush on
//!    unpin) doesn't apply because allocations have no pin/unpin
//!    moments  every read or write of an allocated region is a
//!    raw deref, with no boundary where we could route through
//!    TVM's read/write methods. See PLAN-tvm-integration.md
//!    Phase 2 for the architectural finding and what API change
//!    would unblock it.
//!
//! ## Allocator design
//!
//! Each allocation gets a `HEADER_SIZE`-byte prefix holding the
//! original requested size, so `xSize(ptr)` and `xFree(ptr)` can
//! walk back from the user-visible pointer:
//!
//! ```text
//!   [ size: u64 (HEADER_SIZE bytes) | user payload ]
//!                                   ^ ptr returned to SQLite
//! ```
//!
//! `HEADER_SIZE` is 16 bytes so the user pointer keeps 16-byte
//! alignment (matches what most libc mallocs return and what
//! SQLite expects for its general-purpose buffers). The header
//! stores the original `size: c_int` SQLite requested  not the
//! over-allocated total  so `xSize` returns the same value
//! every time for a given pointer, satisfying SQLite's contract
//! that "xSize() should return the number of bytes available
//! starting at the given allocation."

#![allow(non_snake_case)]

use std::alloc::{alloc, dealloc, Layout};
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use libsqlite3_sys as ffi;
use libsqlite3_sys::sqlite3_mem_methods;

/// 16 bytes  alignment-friendly and big enough for the size u64.
/// User pointer ends up at `raw + HEADER_SIZE`, which is also
/// 16-aligned given the layout we pass to `alloc`.
const HEADER_SIZE: usize = 16;

/// Allocation alignment. Matches what libc malloc returns on
/// 64-bit platforms; SQLite's documentation says nothing
/// stronger is required.
const ALIGN: usize = 16;

/// Lightweight runtime counters. Exposed via `diagnostics()`
/// so embedders can sanity-check that SQLite is actually
/// driving this allocator and observe its byte traffic.
static N_MALLOC: AtomicU64 = AtomicU64::new(0);
static N_FREE: AtomicU64 = AtomicU64::new(0);
static N_REALLOC: AtomicU64 = AtomicU64::new(0);
static BYTES_OUTSTANDING: AtomicU64 = AtomicU64::new(0);
static BYTES_LIFETIME: AtomicU64 = AtomicU64::new(0);

/// (mallocs, frees, reallocs, bytes_outstanding, bytes_lifetime).
/// Counters never reset; `bytes_lifetime` always grows;
/// `bytes_outstanding` is the live byte count at the moment of
/// the call (deltas applied per malloc/realloc/free).
pub fn diagnostics() -> (u64, u64, u64, u64, u64) {
    (
        N_MALLOC.load(Ordering::Relaxed),
        N_FREE.load(Ordering::Relaxed),
        N_REALLOC.load(Ordering::Relaxed),
        BYTES_OUTSTANDING.load(Ordering::Relaxed),
        BYTES_LIFETIME.load(Ordering::Relaxed),
    )
}

unsafe fn alloc_with_header(size: c_int) -> *mut c_void {
    if size <= 0 {
        return ptr::null_mut();
    }
    let user_size = size as usize;
    let total = HEADER_SIZE + user_size;
    let layout = match Layout::from_size_align(total, ALIGN) {
        Ok(l) => l,
        Err(_) => return ptr::null_mut(),
    };
    let raw = alloc(layout);
    if raw.is_null() {
        return ptr::null_mut();
    }
    // Write the user-requested size into the header so xSize and
    // xFree can recover it.
    ptr::write(raw as *mut u64, user_size as u64);
    N_MALLOC.fetch_add(1, Ordering::Relaxed);
    BYTES_OUTSTANDING.fetch_add(user_size as u64, Ordering::Relaxed);
    BYTES_LIFETIME.fetch_add(user_size as u64, Ordering::Relaxed);
    raw.add(HEADER_SIZE) as *mut c_void
}

/// Recover (raw_pointer, user_size) for an allocation given the
/// user-visible pointer SQLite holds. SAFETY: `user_ptr` must
/// have come from `alloc_with_header` and not yet been freed.
unsafe fn header_for(user_ptr: *mut c_void) -> (*mut u8, usize) {
    let raw = (user_ptr as *mut u8).sub(HEADER_SIZE);
    let user_size = *(raw as *const u64) as usize;
    (raw, user_size)
}

unsafe extern "C" fn x_init(_arg: *mut c_void) -> c_int {
    // No global state to initialize  counters are atomic with
    // pre-initialized values, the global allocator is up by the
    // time we're called.
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_shutdown(_arg: *mut c_void) {
    // No global state to release.
}

unsafe extern "C" fn x_malloc(size: c_int) -> *mut c_void {
    alloc_with_header(size)
}

unsafe extern "C" fn x_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let (raw, user_size) = header_for(p);
    let total = HEADER_SIZE + user_size;
    let layout = match Layout::from_size_align(total, ALIGN) {
        Ok(l) => l,
        Err(_) => return, // shouldn't happen — same layout we alloc'd with
    };
    dealloc(raw, layout);
    N_FREE.fetch_add(1, Ordering::Relaxed);
    BYTES_OUTSTANDING.fetch_sub(user_size as u64, Ordering::Relaxed);
}

unsafe extern "C" fn x_realloc(p: *mut c_void, new_size: c_int) -> *mut c_void {
    // SQLite's xRealloc contract:
    //   - p == NULL behaves like xMalloc(new_size)
    //   - new_size <= 0 behaves like xFree(p) and returns NULL
    //   - otherwise: try to grow/shrink p to new_size; on
    //     failure, return NULL and leave p untouched
    if p.is_null() {
        return alloc_with_header(new_size);
    }
    if new_size <= 0 {
        x_free(p);
        return ptr::null_mut();
    }
    let (raw, old_user_size) = header_for(p);
    let new_user_size = new_size as usize;

    // Allocate fresh, copy, free old. The Rust global allocator
    // has a `realloc` primitive but it'd require us to track the
    // total (header + payload) size differently  the size we
    // stored is the user size, not the total. Simpler to do an
    // alloc + memcpy + free.
    let new_total = HEADER_SIZE + new_user_size;
    let layout = match Layout::from_size_align(new_total, ALIGN) {
        Ok(l) => l,
        Err(_) => return ptr::null_mut(),
    };
    let new_raw = alloc(layout);
    if new_raw.is_null() {
        return ptr::null_mut();
    }
    ptr::write(new_raw as *mut u64, new_user_size as u64);
    let copy_bytes = old_user_size.min(new_user_size);
    ptr::copy_nonoverlapping(
        raw.add(HEADER_SIZE),
        new_raw.add(HEADER_SIZE),
        copy_bytes,
    );

    let old_layout = match Layout::from_size_align(HEADER_SIZE + old_user_size, ALIGN) {
        Ok(l) => l,
        Err(_) => return ptr::null_mut(),
    };
    dealloc(raw, old_layout);

    N_REALLOC.fetch_add(1, Ordering::Relaxed);
    // Net byte change: new - old.
    if new_user_size > old_user_size {
        let delta = (new_user_size - old_user_size) as u64;
        BYTES_OUTSTANDING.fetch_add(delta, Ordering::Relaxed);
        BYTES_LIFETIME.fetch_add(delta, Ordering::Relaxed);
    } else {
        let delta = (old_user_size - new_user_size) as u64;
        BYTES_OUTSTANDING.fetch_sub(delta, Ordering::Relaxed);
    }
    new_raw.add(HEADER_SIZE) as *mut c_void
}

unsafe extern "C" fn x_size(p: *mut c_void) -> c_int {
    if p.is_null() {
        return 0;
    }
    let (_, user_size) = header_for(p);
    user_size as c_int
}

unsafe extern "C" fn x_roundup(size: c_int) -> c_int {
    // SQLite uses xRoundup to determine the actual allocation
    // size for a given request  e.g. when budgeting its
    // lookaside arena. We round up to the alignment so callers
    // can rely on the alignment guarantee.
    if size <= 0 {
        return 0;
    }
    let aligned = (size as usize + (ALIGN - 1)) & !(ALIGN - 1);
    aligned as c_int
}

/// Sync newtype around the methods table. `sqlite3_mem_methods`
/// holds a `*mut c_void` (`pAppData`) so it's not auto-Sync;
/// we use `null_mut()` and the table is otherwise read-only
/// after construction, so a manual `unsafe impl Sync` is sound.
#[repr(transparent)]
struct MethodsTable(sqlite3_mem_methods);
unsafe impl Sync for MethodsTable {}

static METHODS: MethodsTable = MethodsTable(sqlite3_mem_methods {
    xMalloc: Some(x_malloc),
    xFree: Some(x_free),
    xRealloc: Some(x_realloc),
    xSize: Some(x_size),
    xRoundup: Some(x_roundup),
    xInit: Some(x_init),
    xShutdown: Some(x_shutdown),
    pAppData: ptr::null_mut(),
});

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register this crate's allocator as SQLite's `sqlite3_malloc`
/// family. Must run *before* `sqlite3_initialize` per SQLite's
/// `SQLITE_CONFIG_MALLOC` contract (any later change requires
/// `sqlite3_shutdown` first).
///
/// Safe to call multiple times — subsequent calls are no-ops.
pub fn install() -> Result<(), InstallError> {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let rc = unsafe {
        ffi::sqlite3_config(
            ffi::SQLITE_CONFIG_MALLOC,
            &METHODS.0 as *const _ as *const c_void,
        )
    };
    if rc != ffi::SQLITE_OK {
        INSTALLED.store(false, Ordering::SeqCst);
        return Err(InstallError {
            code: rc,
            message: "sqlite3_config(SQLITE_CONFIG_MALLOC) failed; \
                      must be called before sqlite3_initialize"
                .to_string(),
        });
    }
    Ok(())
}

/// Failure shape returned by `install`. The numeric code is the
/// raw SQLite return; the message names the likely boot-order
/// violation.
#[derive(Debug)]
pub struct InstallError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for InstallError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_malloc_size_free() {
        // Direct trampoline test  doesn't touch SQLite. Asserts
        // size header bookkeeping is consistent.
        unsafe {
            let p = x_malloc(128);
            assert!(!p.is_null());
            assert_eq!(x_size(p), 128, "xSize should return original request");
            x_free(p);
        }
    }

    #[test]
    fn realloc_preserves_bytes_up_to_min_of_old_and_new() {
        unsafe {
            let p = x_malloc(8);
            assert!(!p.is_null());
            // Write a recognizable pattern.
            ptr::write(p as *mut u64, 0xDEADBEEF_CAFEBABE);
            // Grow.
            let p2 = x_realloc(p, 32);
            assert!(!p2.is_null());
            assert_eq!(x_size(p2), 32);
            assert_eq!(
                ptr::read(p2 as *const u64),
                0xDEADBEEF_CAFEBABE,
                "realloc must preserve the prefix"
            );
            // Shrink.
            let p3 = x_realloc(p2, 4);
            assert!(!p3.is_null());
            assert_eq!(x_size(p3), 4);
            // First 4 bytes of the pattern are 0xCAFEBABE (little-endian
            // low bytes). Confirm shrink kept them.
            assert_eq!(ptr::read(p3 as *const u32), 0xCAFEBABE);
            x_free(p3);
        }
    }

    #[test]
    fn realloc_null_is_malloc() {
        unsafe {
            let p = x_realloc(ptr::null_mut(), 64);
            assert!(!p.is_null());
            assert_eq!(x_size(p), 64);
            x_free(p);
        }
    }

    #[test]
    fn realloc_zero_is_free() {
        unsafe {
            let p = x_malloc(32);
            let p2 = x_realloc(p, 0);
            assert!(p2.is_null());
            // p is now freed; don't free again
        }
    }

    #[test]
    fn xroundup_aligns_to_ALIGN() {
        unsafe {
            assert_eq!(x_roundup(1), ALIGN as c_int);
            assert_eq!(x_roundup(ALIGN as c_int), ALIGN as c_int);
            assert_eq!(x_roundup(ALIGN as c_int + 1), (ALIGN * 2) as c_int);
        }
    }
}
