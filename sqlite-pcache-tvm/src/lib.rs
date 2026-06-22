//! Custom `sqlite3_pcache_methods2` implementation.
//!
//! Path D shadow-pool design (PLAN-tvm-integration Phase 1.1).
//! The eviction + LRU + flush logic lives in `cache::ShadowCache`;
//! the cold-storage abstraction lives in `region::Region`. This
//! file is just the SQLite-facing surface — eleven `extern "C"`
//! trampolines + `install()`.
//!
//! ## Backend selection
//!
//! The default build uses `region::InProcRegion` (`HashMap<u32,
//! Vec<u8>>` keyed by `key * sz_page`) — same memory budget as
//! before Phase 1.1, but now goes through the full shadow-pool
//! machinery so the eviction + flush paths are exercised in
//! every build. The `tvm` cargo feature swaps in a wit-bindgen-
//! backed region against `tvm:memory@0.1.0`; that backend is the
//! Path D destination state. For now, building with `--features
//! tvm` is wasm32-only (the wit-bindgen guest code is wasm32-
//! only), and the host-side test of that path lives in the
//! follow-up commit.
//!
//! ## Registration
//!
//! `install()` calls `sqlite3_config(SQLITE_CONFIG_PCACHE2, …)`.
//! SQLite requires that call to land **before** `sqlite3_initialize`
//! — same constraint as `SQLITE_CONFIG_LOG`. The atomic gate
//! makes repeat calls no-ops.

#![allow(non_snake_case)]

use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use libsqlite3_sys as ffi;
use libsqlite3_sys::{sqlite3_pcache, sqlite3_pcache_methods2, sqlite3_pcache_page};

// Lightweight runtime counters for the Phase 1.3 capacity test
// and any future diagnostics. The probe exposes these via WIT
// exports; the host reads them to confirm SQLite is actually
// driving the pcache, the shadow pool sized correctly, etc.
static N_FETCH: AtomicU32 = AtomicU32::new(0);
static N_UNPIN: AtomicU32 = AtomicU32::new(0);
static LAST_CACHESIZE: AtomicU32 = AtomicU32::new(0);
static LAST_SHADOW_COUNT: AtomicU32 = AtomicU32::new(0);

/// (fetch count, unpin count, last xCachesize arg, last observed
/// shadow_count). Diagnostics for the Phase 1.3 capacity test and
/// any future correctness probes  the host inspects these to
/// confirm SQLite drove the pcache as expected.
pub fn cache_diagnostics() -> (u32, u32, u32, u32) {
    (
        N_FETCH.load(Ordering::Relaxed),
        N_UNPIN.load(Ordering::Relaxed),
        LAST_CACHESIZE.load(Ordering::Relaxed),
        LAST_SHADOW_COUNT.load(Ordering::Relaxed),
    )
}

pub mod cache;
pub mod region;

// On wasm32 the cold tier is always the wit-bindgen-backed
// `tvm:memory` region  there's no reason to pick the in-proc
// fallback when the target is wasm. The in-proc backend stays
// available on native (where the wit-bindgen extern blocks
// wouldn't link anyway) for the unit-test path.
#[cfg(target_arch = "wasm32")]
pub mod wit_tvm_region;

#[cfg(target_arch = "wasm32")]
type ActiveRegion = wit_tvm_region::WitTvmRegion;
#[cfg(not(target_arch = "wasm32"))]
type ActiveRegion = region::InProcRegion;

type ActiveCache = cache::ShadowCache<ActiveRegion>;

fn new_active_cache(sz_page: c_int, sz_extra: c_int, purgeable: bool) -> ActiveCache {
    cache::ShadowCache::new(sz_page, sz_extra, purgeable, ActiveRegion::default())
}

// ---------------------------------------------------------------
// Trampolines. SQLite calls these with raw pointers; each one
// casts back to &mut ActiveCache and forwards to the impl.
// ---------------------------------------------------------------

unsafe extern "C" fn x_init(_arg: *mut c_void) -> c_int {
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_shutdown(_arg: *mut c_void) {
    // No global state to release.
}

unsafe extern "C" fn x_create(
    sz_page: c_int,
    sz_extra: c_int,
    purgeable: c_int,
) -> *mut sqlite3_pcache {
    let cache = Box::new(new_active_cache(sz_page, sz_extra, purgeable != 0));
    Box::into_raw(cache) as *mut sqlite3_pcache
}

unsafe extern "C" fn x_cachesize(p: *mut sqlite3_pcache, n: c_int) {
    LAST_CACHESIZE.store(n as u32, Ordering::Relaxed);
    let cache = &mut *(p as *mut ActiveCache);
    cache.set_cachesize(n);
}

unsafe extern "C" fn x_pagecount(p: *mut sqlite3_pcache) -> c_int {
    let cache = &*(p as *const ActiveCache);
    cache.page_count()
}

unsafe extern "C" fn x_fetch(
    p: *mut sqlite3_pcache,
    key: c_uint,
    create_flag: c_int,
) -> *mut sqlite3_pcache_page {
    N_FETCH.fetch_add(1, Ordering::Relaxed);
    let cache = &mut *(p as *mut ActiveCache);
    LAST_SHADOW_COUNT.store(cache.shadow_count(), Ordering::Relaxed);
    cache.fetch(key, create_flag)
}

unsafe extern "C" fn x_unpin(
    p: *mut sqlite3_pcache,
    page: *mut sqlite3_pcache_page,
    discard: c_int,
) {
    N_UNPIN.fetch_add(1, Ordering::Relaxed);
    let cache = &mut *(p as *mut ActiveCache);
    cache.unpin(page, discard != 0)
}

unsafe extern "C" fn x_rekey(
    p: *mut sqlite3_pcache,
    page: *mut sqlite3_pcache_page,
    old_key: c_uint,
    new_key: c_uint,
) {
    let cache = &mut *(p as *mut ActiveCache);
    cache.rekey(page, old_key, new_key)
}

unsafe extern "C" fn x_truncate(p: *mut sqlite3_pcache, limit: c_uint) {
    let cache = &mut *(p as *mut ActiveCache);
    cache.truncate(limit)
}

unsafe extern "C" fn x_destroy(p: *mut sqlite3_pcache) {
    // SAFETY: p came from `x_create`'s Box::into_raw. Reclaim it.
    drop(Box::from_raw(p as *mut ActiveCache));
}

unsafe extern "C" fn x_shrink(_p: *mut sqlite3_pcache) {
    // Best-effort eviction hint. We don't enforce a hard budget
    // above shadow_capacity (xCachesize is the budget), so
    // there's nothing additional to shrink. The TVM backend may
    // map this to a tvm:memory.demote-region call once landed.
}

/// Sync newtype around the methods table. `sqlite3_pcache_methods2`
/// holds a `*mut c_void` (`pArg`) so it's not auto-Sync; the value
/// we use is `null_mut()` and the table is otherwise read-only
/// after construction, so a manual `unsafe impl Sync` is sound.
#[repr(transparent)]
struct MethodsTable(sqlite3_pcache_methods2);
unsafe impl Sync for MethodsTable {}

static METHODS: MethodsTable = MethodsTable(sqlite3_pcache_methods2 {
    iVersion: 1,
    pArg: ptr::null_mut(),
    xInit: Some(x_init),
    xShutdown: Some(x_shutdown),
    xCreate: Some(x_create),
    xCachesize: Some(x_cachesize),
    xPagecount: Some(x_pagecount),
    xFetch: Some(x_fetch),
    xUnpin: Some(x_unpin),
    xRekey: Some(x_rekey),
    xTruncate: Some(x_truncate),
    xDestroy: Some(x_destroy),
    xShrink: Some(x_shrink),
});

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register this crate's pcache2 impl as SQLite's page cache. Must
/// run *before* `sqlite3_initialize` per SQLite's
/// `SQLITE_CONFIG_PCACHE2` contract (any later change requires
/// `sqlite3_shutdown` first).
///
/// Safe to call multiple times — subsequent calls are no-ops.
pub fn install() -> Result<(), InstallError> {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let rc = unsafe {
        ffi::sqlite3_config(
            ffi::SQLITE_CONFIG_PCACHE2,
            &METHODS.0 as *const _ as *const c_void,
        )
    };
    if rc != ffi::SQLITE_OK {
        INSTALLED.store(false, Ordering::SeqCst);
        return Err(InstallError {
            code: rc,
            message: "sqlite3_config(SQLITE_CONFIG_PCACHE2) failed; \
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
