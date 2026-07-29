//! Custom `sqlite3_vfs` implementation. Phase 4 of the TVM track
//! in PLAN-tvm-integration.md.
//!
//! ## Backend selection
//!
//! Native builds use `storage::InProcStorage` (a `Vec<u8>` per
//! file) so trampoline behavior is unit-testable without a wasm
//! runtime. wasm32 builds use
//! `multi_memory_storage::MultiMemoryStorage`: file bytes live
//! in pool 2 of the `tvm-guest-mm` shell, accessed through the
//! `tvm-guest-mm-rt` dispatch helpers. The `tvm-mm-link` step
//! at sqlite-lib build time bakes the pool memories into the
//! merged module.
//!
//! Registration is NOT boot-order-constrained the way
//! `sqlite3_config(...)` is, so `install()` / `install_as_default()`
//! can run at any point before the first `sqlite3_open_v2`
//! against our VFS name.
//!
//! ## Lifetime model
//!
//! SQLite allocates one `sqlite3_file`-shaped slab per
//! `xOpen` call (sized by `sqlite3_vfs.szOsFile`). We layout
//! our `TvmFile` there as a `#[repr(C)]` struct whose first
//! field is the bare `sqlite3_file` (the `pMethods` pointer)
//! followed by a heap pointer to a `TvmFileInner` we own.
//! `xClose` reclaims the inner. The outer `sqlite3_file` slab
//! is freed by SQLite once `xClose` returns SQLITE_OK.
//!
//! ## Path → storage routing
//!
//! `xOpen(name, ...)` looks up `name` in a process-global
//! `HashMap<String, Arc<Mutex<dyn FileStorage>>>`. If absent
//! and `SQLITE_OPEN_CREATE` is in the flags, we allocate a
//! fresh `InProcStorage` and register it. Multiple opens of
//! the same path share storage (which is what SQLite expects
//! for the main db + its journal). `xDelete(name)` removes the
//! entry; temp files (NULL name) get a synthetic unique key
//! that's deleted on `xClose`.

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use libsqlite3_sys as ffi;
use libsqlite3_sys::{
    sqlite3_file, sqlite3_filename, sqlite3_int64, sqlite3_io_methods, sqlite3_vfs,
};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

pub mod storage;

/// `"opfs"` VFS — file bytes live in the JS host's OPFS via the
/// WIT-imported `sqlite:wasm/opfs-host` interface. Backend lives in
/// sqlite-lib (which holds the wit-bindgen import shims); this
/// crate just registers the VFS table and trampolines into a
/// dyn-trait callback. Native sqlite-lib supplies a stub backend
/// so the VFS still registers, returning SQLITE_CANTOPEN on use.
pub mod opfs;

// On wasm32 the file storage is normally a multi-memory pool-backed
// storage: file bytes live in pool 2 of the `tvm-guest-mm`
// shell, accessed through the `tvm-guest-mm-rt` dispatch
// helpers. The in-proc Vec<u8> backend stays the default on
// native (where the pool helpers don't exist) for the
// unit-test path. The `single-memory` feature forces the in-proc
// backend even on wasm32; this is the browser flavor where jco
// can't yet transpile multi-memory inner modules.
#[cfg(all(target_arch = "wasm32", not(feature = "single-memory")))]
pub mod multi_memory_storage;

use storage::FileStorage;

/// Construct a fresh backend at xOpen time. Returns Err if the
/// TVM-backed variant fails to create its region; the InProc
/// variant is infallible. Trampoline maps the error to
/// SQLITE_IOERR.
#[cfg(any(not(target_arch = "wasm32"), feature = "single-memory"))]
fn make_storage() -> Result<Box<dyn FileStorage>, c_int> {
    Ok(Box::new(storage::InProcStorage::new()))
}

#[cfg(all(target_arch = "wasm32", not(feature = "single-memory")))]
fn make_storage() -> Result<Box<dyn FileStorage>, c_int> {
    match multi_memory_storage::MultiMemoryStorage::new() {
        Ok(s) => Ok(Box::new(s)),
        Err(_) => Err(ffi::SQLITE_IOERR),
    }
}

/// VFS name. The bytes must be NUL-terminated because we hand a
/// `*const c_char` to SQLite and SQLite uses it verbatim.
const VFS_NAME_C: &[u8] = b"tvm-mem\0";
const VFS_NAME_STR: &str = "tvm-mem";

/// SQLite expects xFullPathname to write up to this many bytes
/// into the caller-supplied buffer. We don't translate paths
/// (the input IS the canonical name), so this can be any
/// reasonable upper bound  256 mirrors most VFS impls.
const MAX_PATHNAME: c_int = 256;

/// Process-global file table. Multiple `xOpen` calls against
/// the same path return handles to the same storage  that's
/// how SQLite's journal + main db coordinate. The inner
/// `Box<dyn FileStorage>` is there because the trait object
/// itself is unsized; the surrounding `Mutex<Box<...>>` serialises
/// access (SQLite's threading mode for an in-memory file is
/// single-connection per access anyway).
type FileTable = HashMap<String, Arc<Mutex<Box<dyn FileStorage>>>>;
static FILES: Lazy<Mutex<FileTable>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Monotonic counter for synthesizing temp-file names when
/// SQLite calls xOpen with a NULL filename.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Reserved name prefix for ephemeral in-memory databases
/// synthesized by `core::db::open_in_memory` when routing through
/// `tvm-mem`. Used by `xOpen` to recognise main-db opens that
/// own auxiliary files (`-journal` / `-wal` / `-shm`) and by
/// `io_close` to sweep those auxiliaries when the owner closes.
const TVM_ANON_PREFIX: &str = "/__tvm_mem_anon_";

/// Suffixes SQLite appends to a main db's filename to derive the
/// auxiliary file names (rollback journal, WAL, shared-memory).
/// We never carry delete-on-close for these — auxiliaries follow
/// the main db's lifecycle, which is enforced by the prefix
/// sweep in `io_close`.
const AUX_SUFFIXES: &[&str] = &["-journal", "-wal", "-shm"];

/// Per-file state we own. The outer `sqlite3_file` SQLite
/// allocates is just the `pMethods` pointer + our inner ptr.
struct TvmFileInner {
    storage: Arc<Mutex<Box<dyn FileStorage>>>,
    name: String,
    delete_on_close: bool,
    /// True for `-journal` / `-wal` / `-shm` files attached to
    /// an owning main db. Used to suppress the prefix-sweep in
    /// `io_close` so the auxiliary's close doesn't try to walk
    /// the FILES table looking for further children.
    is_auxiliary: bool,
    /// Current SQLite lock level for this open file. Tracks the
    /// state SQLite expects across `xLock` / `xUnlock` calls
    /// (`SQLITE_LOCK_NONE` through `SQLITE_LOCK_EXCLUSIVE`).
    ///
    /// We're single-process / single-threaded inside the wasm
    /// guest so contention is impossible — but SQLite still
    /// asserts consistent transitions (e.g. SHARED before
    /// RESERVED, RESERVED before EXCLUSIVE), and WAL mode in
    /// particular probes the level via `xCheckReservedLock` +
    /// `SQLITE_FCNTL_LOCKSTATE`. Track it locally so those
    /// probes report something coherent.
    lock_level: c_int,
    /// Shared-memory state for `-shm` opens, allocated lazily on
    /// the first `xShmMap` call. None for non-shm files. SQLite
    /// uses this for the WAL index; see the `ShmState` struct
    /// below for the per-file region + lock bookkeeping.
    shm: Option<Box<ShmState>>,
}

/// Per-file shared-memory state used by SQLite's WAL index.
///
/// SQLite's WAL implementation maps the `-shm` file in fixed-size
/// regions (32 KiB by default) and uses 8 lock slots
/// (`SQLITE_SHM_NLOCK`) for coordination. Single-process means
/// the lock counts are bookkeeping only — there is nothing to
/// contend with — but SQLite asserts consistent transitions, so
/// we track them.
///
/// Lifetimes: each `Box<[u8]>` region is owned by the file's
/// `ShmState`; the pointer we hand back via `xShmMap` is valid
/// for the lifetime of that owning Box. SQLite documents that
/// region pointers stay live until the matching `xShmUnmap`, and
/// `xShmUnmap` runs only at file close, so a file's
/// `ShmState.regions` Vec must not be reallocated in-place across
/// xShmMap calls. Since `Box<[u8]>` is heap-allocated and the
/// inner bytes don't move when the Vec grows, this contract is
/// satisfied.
struct ShmState {
    /// Region index → owned byte buffer. Sparse: entries may be
    /// `None` if SQLite asked about a region without extending
    /// it. Region size is the `pgsz` SQLite passes on the first
    /// successful `xShmMap` for that region (uniform per file).
    regions: Vec<Option<Box<[u8]>>>,
    /// Per-slot lock state. Single-process makes contention
    /// impossible; we still track for SQLite's transition
    /// assertions.
    locks: [ShmLockState; 8],
}

/// State of one of SQLite's 8 shm-lock slots.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum ShmLockState {
    /// No lock held.
    Unlocked,
    /// Shared lock held by `count` openers. Single-process is
    /// always 1 in practice; the field exists to satisfy any
    /// future code that nests shared locks.
    Shared(u32),
    /// Exclusive lock held.
    Exclusive,
}

impl ShmState {
    fn new() -> Self {
        Self {
            regions: Vec::new(),
            locks: [ShmLockState::Unlocked; 8],
        }
    }
}

/// `#[repr(C)]` so the `base` field is at offset 0 — SQLite
/// hands us a `*mut sqlite3_file` and we cast it to
/// `*mut TvmFile`. Same layout pattern is what every other
/// VFS impl uses.
#[repr(C)]
struct TvmFile {
    base: sqlite3_file,
    inner: *mut TvmFileInner,
}

// ---------------------------------------------------------------
// IO methods (per-file callbacks).
// ---------------------------------------------------------------

unsafe extern "C" fn io_close(file: *mut sqlite3_file) -> c_int {
    let tf = file as *mut TvmFile;
    let inner_ptr = (*tf).inner;
    if !inner_ptr.is_null() {
        // SAFETY: inner_ptr came from `Box::into_raw` in io_open.
        let inner = Box::from_raw(inner_ptr);
        if inner.delete_on_close {
            let mut files = FILES.lock();
            files.remove(&inner.name);
            // If this was a main db file with the anon prefix, sweep
            // any auxiliary files (journal/wal/shm) that share the
            // base name. SQLite opens/closes auxiliaries multiple
            // times across the main db's lifetime, so we keep them
            // around until the owner closes; doing the sweep here
            // matches the comment in `vfs_open` ("aux files should
            // share the main db's lifecycle") without the bug of
            // freeing them on every cycle (which leaked one slice of
            // pool 2 per close/reopen).
            if inner.name.starts_with(TVM_ANON_PREFIX) && !inner.is_auxiliary {
                let prefix = format!("{}-", inner.name);
                files.retain(|k, _| !k.starts_with(&prefix));
            }
        }
        // dropped here
        (*tf).inner = ptr::null_mut();
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn io_read(
    file: *mut sqlite3_file,
    buf: *mut c_void,
    amt: c_int,
    ofst: sqlite3_int64,
) -> c_int {
    let tf = &*(file as *mut TvmFile);
    if tf.inner.is_null() || amt <= 0 || buf.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &*tf.inner;
    let storage = inner.storage.lock();
    let slice = std::slice::from_raw_parts_mut(buf as *mut u8, amt as usize);
    match storage.read(ofst as u64, slice) {
        Ok(()) => ffi::SQLITE_OK,
        Err(code) => code,
    }
}

unsafe extern "C" fn io_write(
    file: *mut sqlite3_file,
    buf: *const c_void,
    amt: c_int,
    ofst: sqlite3_int64,
) -> c_int {
    let tf = &*(file as *mut TvmFile);
    if tf.inner.is_null() || amt <= 0 || buf.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &*tf.inner;
    let mut storage = inner.storage.lock();
    let slice = std::slice::from_raw_parts(buf as *const u8, amt as usize);
    match storage.write(ofst as u64, slice) {
        Ok(()) => ffi::SQLITE_OK,
        Err(code) => code,
    }
}

unsafe extern "C" fn io_truncate(file: *mut sqlite3_file, size: sqlite3_int64) -> c_int {
    let tf = &*(file as *mut TvmFile);
    if tf.inner.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &*tf.inner;
    let mut storage = inner.storage.lock();
    match storage.truncate(size as u64) {
        Ok(()) => ffi::SQLITE_OK,
        Err(code) => code,
    }
}

unsafe extern "C" fn io_sync(_file: *mut sqlite3_file, _flags: c_int) -> c_int {
    // In-memory storage  no durable backing to flush to.
    // SQLite's "durability" guarantee is vacuous here; document
    // it for callers expecting crash safety.
    ffi::SQLITE_OK
}

unsafe extern "C" fn io_file_size(
    file: *mut sqlite3_file,
    p_size: *mut sqlite3_int64,
) -> c_int {
    let tf = &*(file as *mut TvmFile);
    if tf.inner.is_null() || p_size.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &*tf.inner;
    let storage = inner.storage.lock();
    *p_size = storage.size() as sqlite3_int64;
    ffi::SQLITE_OK
}

unsafe extern "C" fn io_lock(file: *mut sqlite3_file, level: c_int) -> c_int {
    // Single-process / single-threaded inside the wasm guest, so
    // no other connection can be holding a competing lock.
    // SQLite still asserts consistent transitions — `xLock` is
    // documented as monotonic (it never decreases the level), and
    // WAL probes the level via SQLITE_FCNTL_LOCKSTATE — so track
    // the value per-file.
    let tf = file as *mut TvmFile;
    if tf.is_null() || (*tf).inner.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &mut *(*tf).inner;
    if level > inner.lock_level {
        inner.lock_level = level;
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn io_unlock(file: *mut sqlite3_file, level: c_int) -> c_int {
    // Monotonic in the other direction: xUnlock never increases
    // the level. Drop to the requested level.
    let tf = file as *mut TvmFile;
    if tf.is_null() || (*tf).inner.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &mut *(*tf).inner;
    if level < inner.lock_level {
        inner.lock_level = level;
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn io_check_reserved_lock(
    _file: *mut sqlite3_file,
    p_res_out: *mut c_int,
) -> c_int {
    // Single-process — nobody else can possibly hold RESERVED on
    // this file, so always report 0. SQLite uses this to decide
    // whether to escalate from SHARED to EXCLUSIVE during a
    // commit; 0 says "go ahead."
    if !p_res_out.is_null() {
        *p_res_out = 0;
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn io_file_control(
    file: *mut sqlite3_file,
    op: c_int,
    arg: *mut c_void,
) -> c_int {
    // Honor SQLITE_FCNTL_LOCKSTATE so callers (including SQLite's
    // own diagnostic paths) can read the current lock level. Any
    // other file-control is unsupported on this VFS.
    if op == ffi::SQLITE_FCNTL_LOCKSTATE {
        let tf = file as *mut TvmFile;
        if tf.is_null() || (*tf).inner.is_null() || arg.is_null() {
            return ffi::SQLITE_IOERR;
        }
        let inner = &*(*tf).inner;
        *(arg as *mut c_int) = inner.lock_level;
        return ffi::SQLITE_OK;
    }
    ffi::SQLITE_NOTFOUND
}

unsafe extern "C" fn io_sector_size(_file: *mut sqlite3_file) -> c_int {
    // 4096 is the sqlite3 default sector hint  fine for an
    // in-memory backend where the concept is moot anyway.
    4096
}

unsafe extern "C" fn io_device_characteristics(_file: *mut sqlite3_file) -> c_int {
    // SAFE_APPEND = appends are atomic; SEQUENTIAL = io is
    // ordered. Both are trivially true for an in-memory file.
    // ATOMIC means writes of arbitrary size are atomic, which
    // is also true since we have no torn writes.
    ffi::SQLITE_IOCAP_ATOMIC | ffi::SQLITE_IOCAP_SAFE_APPEND | ffi::SQLITE_IOCAP_SEQUENTIAL
}

// ---------------------------------------------------------------
// Shared-memory IO methods (xShm*) — back the WAL index.
// ---------------------------------------------------------------

unsafe extern "C" fn io_shm_map(
    file: *mut sqlite3_file,
    region_idx: c_int,
    region_size: c_int,
    extend: c_int,
    pp_out: *mut *mut c_void,
) -> c_int {
    // SQLite asks "give me a pointer to region N, size region_size,
    // creating it if `extend` is true." Allocate (or look up) a
    // heap-owned Box<[u8]> per region. The pointer we hand back is
    // stable for the file's lifetime because:
    //   - `regions` is a Vec<Option<Box<[u8]>>>; growing the Vec
    //     reallocates the outer storage of the Vec, NOT the
    //     heap-owned Box payloads. The bytes the pointer addresses
    //     stay put.
    //   - Boxes are freed only at xShmUnmap (i.e. at file close).
    if pp_out.is_null() || file.is_null() || region_idx < 0 || region_size <= 0 {
        return ffi::SQLITE_IOERR;
    }
    *pp_out = ptr::null_mut();

    let tf = file as *mut TvmFile;
    if (*tf).inner.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &mut *(*tf).inner;
    let shm = inner.shm.get_or_insert_with(|| Box::new(ShmState::new()));
    let idx = region_idx as usize;

    // Grow the regions Vec sparsely (None) up to and including idx.
    if idx >= shm.regions.len() {
        shm.regions.resize_with(idx + 1, || None);
    }

    if shm.regions[idx].is_some() {
        // SAFETY: just confirmed Some. Hand back the address of
        // the boxed slice's payload bytes.
        let r = shm.regions[idx].as_mut().unwrap();
        *pp_out = r.as_mut_ptr() as *mut c_void;
        return ffi::SQLITE_OK;
    }

    if extend == 0 {
        // SQLite asked "does this region exist already?" and the
        // answer is no. Returning SQLITE_OK with a NULL pointer
        // is the documented signal — SQLite then knows the region
        // doesn't exist yet without us synthesizing one.
        return ffi::SQLITE_OK;
    }

    let bytes = vec![0u8; region_size as usize].into_boxed_slice();
    shm.regions[idx] = Some(bytes);
    *pp_out = shm.regions[idx].as_mut().unwrap().as_mut_ptr() as *mut c_void;
    ffi::SQLITE_OK
}

unsafe extern "C" fn io_shm_lock(
    file: *mut sqlite3_file,
    offset: c_int,
    n: c_int,
    flags: c_int,
) -> c_int {
    // SQLite uses 8 lock slots (SQLITE_SHM_NLOCK) on the shm file.
    // `offset` is the starting slot, `n` the count. `flags` is a
    // bitmask: (LOCK | UNLOCK) × (SHARED | EXCLUSIVE).
    //
    // Single-process means no real contention is possible. We
    // still track per-slot state so SQLite's invariants check out:
    // a SHARED lock can stack on SHARED but not on EXCLUSIVE;
    // EXCLUSIVE requires UNLOCKED.
    if file.is_null() || offset < 0 || n <= 0 || offset + n > 8 {
        return ffi::SQLITE_IOERR;
    }
    let tf = file as *mut TvmFile;
    if (*tf).inner.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &mut *(*tf).inner;
    let shm = inner.shm.get_or_insert_with(|| Box::new(ShmState::new()));

    let is_lock = (flags & ffi::SQLITE_SHM_LOCK) != 0;
    let is_unlock = (flags & ffi::SQLITE_SHM_UNLOCK) != 0;
    let is_shared = (flags & ffi::SQLITE_SHM_SHARED) != 0;
    let is_exclusive = (flags & ffi::SQLITE_SHM_EXCLUSIVE) != 0;

    if is_lock == is_unlock {
        // Exactly one of LOCK / UNLOCK must be set.
        return ffi::SQLITE_IOERR;
    }
    if is_shared == is_exclusive {
        // Exactly one of SHARED / EXCLUSIVE must be set.
        return ffi::SQLITE_IOERR;
    }

    let start = offset as usize;
    let end = (offset + n) as usize;

    if is_lock {
        // Two-pass: validate first, then mutate. SQLite expects
        // an all-or-nothing lock acquisition (any slot in the
        // range busy → return SQLITE_BUSY without partial state).
        for i in start..end {
            match (is_exclusive, shm.locks[i]) {
                (true, ShmLockState::Unlocked) => {}
                (false, ShmLockState::Unlocked | ShmLockState::Shared(_)) => {}
                _ => return ffi::SQLITE_BUSY,
            }
        }
        for i in start..end {
            shm.locks[i] = if is_exclusive {
                ShmLockState::Exclusive
            } else {
                match shm.locks[i] {
                    ShmLockState::Unlocked => ShmLockState::Shared(1),
                    ShmLockState::Shared(c) => ShmLockState::Shared(c + 1),
                    ShmLockState::Exclusive => unreachable!(),
                }
            };
        }
    } else {
        // UNLOCK. Drop exclusive → unlocked, or decrement shared
        // (last shared holder → unlocked). Releasing an already
        // unlocked slot is documented as a no-op (SQLite uses
        // unlock-on-shutdown without prior knowledge of state).
        for i in start..end {
            shm.locks[i] = match shm.locks[i] {
                ShmLockState::Exclusive => ShmLockState::Unlocked,
                ShmLockState::Shared(1) | ShmLockState::Shared(0) => ShmLockState::Unlocked,
                ShmLockState::Shared(c) => ShmLockState::Shared(c - 1),
                ShmLockState::Unlocked => ShmLockState::Unlocked,
            };
        }
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn io_shm_barrier(_file: *mut sqlite3_file) {
    // Memory-ordering fence. Single-threaded inside the wasm guest
    // makes this strictly cosmetic, but SeqCst documents intent
    // and is free at the wasm-runtime level.
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

unsafe extern "C" fn io_shm_unmap(file: *mut sqlite3_file, _delete_flag: c_int) -> c_int {
    // Drop all shm regions for this file. SQLite calls this exactly
    // once per file at close (the wal-index's owning open). The
    // `delete_flag` distinguishes "remove the underlying shm file"
    // from "release this open's mapping" — we have no persistent
    // shm file (it lives in-process memory), so both reduce to
    // dropping the regions Vec.
    if file.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let tf = file as *mut TvmFile;
    if (*tf).inner.is_null() {
        return ffi::SQLITE_OK;
    }
    let inner = &mut *(*tf).inner;
    inner.shm = None;
    ffi::SQLITE_OK
}

#[repr(transparent)]
struct IoMethods(sqlite3_io_methods);
unsafe impl Sync for IoMethods {}

static IO_METHODS: IoMethods = IoMethods(sqlite3_io_methods {
    // iVersion=2 turns on the xShm* family that SQLite needs to
    // set up the wal-index when a connection enters WAL journal
    // mode. The xFetch / xUnfetch (mmap) fields stay None — we
    // don't expose mmap-backed I/O — but iVersion=2 only requires
    // the shm slots to be valid; iVersion=3 is the floor for
    // requiring xFetch / xUnfetch to be set.
    iVersion: 2,
    xClose: Some(io_close),
    xRead: Some(io_read),
    xWrite: Some(io_write),
    xTruncate: Some(io_truncate),
    xSync: Some(io_sync),
    xFileSize: Some(io_file_size),
    xLock: Some(io_lock),
    xUnlock: Some(io_unlock),
    xCheckReservedLock: Some(io_check_reserved_lock),
    xFileControl: Some(io_file_control),
    xSectorSize: Some(io_sector_size),
    xDeviceCharacteristics: Some(io_device_characteristics),
    xShmMap: Some(io_shm_map),
    xShmLock: Some(io_shm_lock),
    xShmBarrier: Some(io_shm_barrier),
    xShmUnmap: Some(io_shm_unmap),
    // xFetch / xUnfetch are mmap I/O, gated on iVersion=3. We
    // don't support memory-mapped pages (everything routes
    // through xRead / xWrite) so these stay None and we cap at
    // iVersion=2.
    xFetch: None,
    xUnfetch: None,
});

// ---------------------------------------------------------------
// VFS-level callbacks.
// ---------------------------------------------------------------

unsafe extern "C" fn vfs_open(
    _vfs: *mut sqlite3_vfs,
    z_name: sqlite3_filename,
    file: *mut sqlite3_file,
    flags: c_int,
    p_out_flags: *mut c_int,
) -> c_int {
    if file.is_null() {
        return ffi::SQLITE_IOERR;
    }
    // NULL filename = SQLite wants a temp file. Synthesize a
    // unique name + flag for delete-on-close so we don't
    // accumulate temps. Named opens also get delete-on-close
    // when the caller passes SQLITE_OPEN_DELETEONCLOSE — used
    // by core::db::open_in_memory to get an ephemeral tvm-mem
    // db that cleans itself up when the connection drops.
    //
    // Files under the path prefix `/__tvm_mem_anon_` that do
    // NOT end in `-journal` / `-wal` / `-shm` are main-db opens
    // for `core::db::open_in_memory`'s synthetic names; we
    // force delete-on-close so the slice gets dropped when the
    // db connection closes. The auxiliary files for such a db
    // (`/__tvm_mem_anon_N-journal` etc.) share the main db's
    // lifecycle, but SQLite opens/closes them many times during
    // normal pager activity — so we mark them `is_auxiliary`
    // (not delete-on-close) and let the main db's `io_close`
    // sweep them via the prefix walk in FILES.
    let explicit_delete = (flags & ffi::SQLITE_OPEN_DELETEONCLOSE) != 0;
    let (name, delete_on_close, is_auxiliary) = if z_name.is_null() {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        (format!("__tvm_tmp_{n}"), true, false)
    } else {
        let s = match CStr::from_ptr(z_name as *const c_char).to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return ffi::SQLITE_IOERR,
        };
        let anon_main = s.starts_with(TVM_ANON_PREFIX)
            && !AUX_SUFFIXES.iter().any(|suf| s.ends_with(suf));
        let aux = s.starts_with(TVM_ANON_PREFIX)
            && AUX_SUFFIXES.iter().any(|suf| s.ends_with(suf));
        // Main-db opens with the anon prefix get delete-on-close so
        // a dropped in-memory db cleans up its slice + its
        // auxiliaries (handled by the prefix sweep in `io_close`).
        // Auxiliary files (`-journal` / `-wal` / `-shm`) do NOT
        // carry delete-on-close from the prefix alone — SQLite
        // opens/closes auxiliaries multiple times during normal
        // operation, and each "close" used to free the FILES entry,
        // forcing the next open to allocate a fresh pool-2 slice
        // (leaking 256 MiB per cycle until the pool's max_pages
        // ceiling traps the third write).
        (s, explicit_delete || anon_main, aux)
    };

    let storage = {
        let mut files = FILES.lock();
        if let Some(existing) = files.get(&name) {
            existing.clone()
        } else if (flags & ffi::SQLITE_OPEN_CREATE) != 0 {
            let s = match make_storage() {
                Ok(s) => s,
                Err(code) => return code,
            };
            let fresh: Arc<Mutex<Box<dyn FileStorage>>> = Arc::new(Mutex::new(s));
            files.insert(name.clone(), fresh.clone());
            fresh
        } else {
            // SQLite asked for an existing file but it doesn't
            // exist. SQLITE_CANTOPEN is the documented signal
            // ("unable to open"; the caller may retry with CREATE).
            return ffi::SQLITE_CANTOPEN;
        }
    };

    let inner = Box::into_raw(Box::new(TvmFileInner {
        storage,
        name,
        delete_on_close,
        is_auxiliary,
        lock_level: ffi::SQLITE_LOCK_NONE,
        shm: None,
    }));

    // Initialize the sqlite3_file slab. SAFETY: SQLite passes us
    // a slab of size sqlite3_vfs.szOsFile (== sizeof(TvmFile)).
    let tf = file as *mut TvmFile;
    (*tf).base.pMethods = &IO_METHODS.0;
    (*tf).inner = inner;

    if !p_out_flags.is_null() {
        // Echo the flags back  we honor whatever SQLite asked
        // for, including SQLITE_OPEN_READWRITE etc.
        *p_out_flags = flags;
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn vfs_delete(
    _vfs: *mut sqlite3_vfs,
    z_name: *const c_char,
    _sync_dir: c_int,
) -> c_int {
    if z_name.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let name = match CStr::from_ptr(z_name).to_str() {
        Ok(s) => s,
        Err(_) => return ffi::SQLITE_IOERR,
    };
    FILES.lock().remove(name);
    ffi::SQLITE_OK
}

unsafe extern "C" fn vfs_access(
    _vfs: *mut sqlite3_vfs,
    z_name: *const c_char,
    _flags: c_int,
    p_res_out: *mut c_int,
) -> c_int {
    if z_name.is_null() || p_res_out.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let name = match CStr::from_ptr(z_name).to_str() {
        Ok(s) => s,
        Err(_) => return ffi::SQLITE_IOERR,
    };
    *p_res_out = if FILES.lock().contains_key(name) { 1 } else { 0 };
    ffi::SQLITE_OK
}

unsafe extern "C" fn vfs_full_pathname(
    _vfs: *mut sqlite3_vfs,
    z_name: *const c_char,
    n_out: c_int,
    z_out: *mut c_char,
) -> c_int {
    if z_name.is_null() || z_out.is_null() {
        return ffi::SQLITE_IOERR;
    }
    // No filesystem  the canonical name IS the input. Copy it
    // verbatim (truncating if needed to fit MAX_PATHNAME).
    let src = CStr::from_ptr(z_name);
    let bytes = src.to_bytes_with_nul();
    let cap = n_out as usize;
    if cap == 0 {
        return ffi::SQLITE_IOERR;
    }
    let copy_len = bytes.len().min(cap);
    ptr::copy_nonoverlapping(bytes.as_ptr(), z_out as *mut u8, copy_len);
    // Force NUL termination in case truncation cut the trailing
    // byte off.
    *z_out.add(copy_len - 1) = 0;
    ffi::SQLITE_OK
}

unsafe extern "C" fn vfs_dlopen(_vfs: *mut sqlite3_vfs, _z: *const c_char) -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn vfs_dlerror(_vfs: *mut sqlite3_vfs, _n: c_int, _msg: *mut c_char) {
    // No dynamic linking through this VFS.
}

unsafe extern "C" fn vfs_dlsym(
    _vfs: *mut sqlite3_vfs,
    _handle: *mut c_void,
    _z: *const c_char,
) -> Option<unsafe extern "C" fn(arg1: *mut sqlite3_vfs, arg2: *mut c_void, zSymbol: *const c_char)>
{
    None
}

unsafe extern "C" fn vfs_dlclose(_vfs: *mut sqlite3_vfs, _h: *mut c_void) {}

unsafe extern "C" fn vfs_randomness(
    _vfs: *mut sqlite3_vfs,
    n_byte: c_int,
    z_out: *mut c_char,
) -> c_int {
    if z_out.is_null() || n_byte <= 0 {
        return 0;
    }
    // Cheap not-cryptographic randomness  good enough for
    // SQLite's auxiliary uses (rowid randomization, etc.). We
    // pull from a monotonically advancing counter XOR'd with
    // an address-bits seed; this is what the original sqlite3
    // os_other.c demo VFS does too.
    static SEED: AtomicU64 = AtomicU64::new(0x9E3779B97F4A7C15);
    let mut s = SEED.fetch_add(1, Ordering::Relaxed);
    let bytes = std::slice::from_raw_parts_mut(z_out as *mut u8, n_byte as usize);
    for chunk in bytes.chunks_mut(8) {
        s = s.wrapping_mul(0x5DEECE66D).wrapping_add(0xB);
        let src = s.to_le_bytes();
        let take = chunk.len().min(8);
        chunk[..take].copy_from_slice(&src[..take]);
    }
    n_byte
}

unsafe extern "C" fn vfs_sleep(_vfs: *mut sqlite3_vfs, _micros: c_int) -> c_int {
    // SQLite calls this when waiting for a busy lock to clear.
    // Our locks are no-ops so we have nothing to wait for.
    0
}

unsafe extern "C" fn vfs_current_time(_vfs: *mut sqlite3_vfs, p_now: *mut f64) -> c_int {
    if p_now.is_null() {
        return ffi::SQLITE_ERROR;
    }
    // SQLite uses Julian Day Number. Skip clock dependency by
    // returning a fixed plausible value (2000-01-01 J2000). The
    // pcache + b-tree paths don't depend on accurate clocks for
    // an in-memory db.
    *p_now = 2_451_544.5;
    ffi::SQLITE_OK
}

unsafe extern "C" fn vfs_get_last_error(
    _vfs: *mut sqlite3_vfs,
    _n: c_int,
    _msg: *mut c_char,
) -> c_int {
    // We don't track per-thread errno-style state. Return 0 to
    // tell SQLite "no further error info available."
    0
}

unsafe extern "C" fn vfs_current_time_int64(
    _vfs: *mut sqlite3_vfs,
    p_now: *mut sqlite3_int64,
) -> c_int {
    if p_now.is_null() {
        return ffi::SQLITE_ERROR;
    }
    // SQLite stores the time as Julian Day × 86_400_000
    // milliseconds. Hand back the same fixed J2000 we use for
    // vfs_current_time so the two stay consistent.
    *p_now = (2_451_544.5 * 86_400_000.0) as sqlite3_int64;
    ffi::SQLITE_OK
}

#[repr(transparent)]
struct VfsTable(sqlite3_vfs);
unsafe impl Sync for VfsTable {}

// `static mut` because `sqlite3_vfs_register` mutates the
// linked-list `pNext` field on the struct we hand it. A
// read-only static is the SIGBUS we hit on first registration.
// The mutability is single-threaded-by-construction: we register
// once at install() and don't touch the struct afterwards.
static mut VFS: VfsTable = VfsTable(sqlite3_vfs {
    // Bumping to iVersion=2 so we can supply the int64 clock
    // SQLite expects modern VFS impls to provide it, and the
    // bindgen layout includes the field anyway.
    iVersion: 2,
    szOsFile: std::mem::size_of::<TvmFile>() as c_int,
    mxPathname: MAX_PATHNAME,
    pNext: ptr::null_mut(),
    zName: VFS_NAME_C.as_ptr() as *const c_char,
    pAppData: ptr::null_mut(),
    xOpen: Some(vfs_open),
    xDelete: Some(vfs_delete),
    xAccess: Some(vfs_access),
    xFullPathname: Some(vfs_full_pathname),
    xDlOpen: Some(vfs_dlopen),
    xDlError: Some(vfs_dlerror),
    xDlSym: Some(vfs_dlsym),
    xDlClose: Some(vfs_dlclose),
    xRandomness: Some(vfs_randomness),
    xSleep: Some(vfs_sleep),
    xCurrentTime: Some(vfs_current_time),
    xGetLastError: Some(vfs_get_last_error),
    // iVersion=2 fields:
    xCurrentTimeInt64: Some(vfs_current_time_int64),
    // iVersion=3 fields (system-call interception)  not supported.
    xSetSystemCall: None,
    xGetSystemCall: None,
    xNextSystemCall: None,
});

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register `tvm-mem` as a named VFS  callable later via
/// `sqlite3_open_v2(name, db, flags, "tvm-mem")` or `Connection`
/// helpers that accept a VFS name. Does NOT become the default;
/// existing opens that pass NULL for the VFS name keep going
/// through whatever was already default (typically `wasivfs`
/// on wasm32, the unix VFS natively).
///
/// Safe to call multiple times — subsequent calls are no-ops.
pub fn install() -> Result<(), InstallError> {
    install_internal(false)
}

/// Same as `install` but also marks the VFS as the SQLite
/// process-wide default. Subsequent `sqlite3_open_v2(path, db,
/// flags, NULL)` (i.e. without an explicit VFS name) routes
/// through `tvm-mem`. Useful for tests and for embedders who
/// want everything in TVM by default.
pub fn install_as_default() -> Result<(), InstallError> {
    install_internal(true)
}

fn install_internal(make_default: bool) -> Result<(), InstallError> {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    // SAFETY: `static mut VFS` is mutated only here and only
    // once  the `INSTALLED` atomic-swap above gates re-entry.
    // SQLite then takes ownership of the linked-list pointer and
    // we never touch the struct again.
    let rc = unsafe {
        let vfs_ptr = &raw mut VFS.0;
        ffi::sqlite3_vfs_register(vfs_ptr, if make_default { 1 } else { 0 })
    };
    if rc != ffi::SQLITE_OK {
        INSTALLED.store(false, Ordering::SeqCst);
        return Err(InstallError {
            code: rc,
            message: "sqlite3_vfs_register failed".to_string(),
        });
    }
    Ok(())
}

/// True iff the VFS is currently registered. Useful in tests
/// that expect a single global install across multiple
/// `#[test]` functions in the same process.
pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

/// Name to pass to `sqlite3_open_v2` (or similar VFS-name-aware
/// APIs) to route through this implementation.
pub fn name() -> &'static str {
    VFS_NAME_STR
}

/// Phase 4.0 diagnostic. Number of distinct logical files the
/// VFS currently holds.
pub fn file_count() -> usize {
    FILES.lock().len()
}

/// Phase 4.0 diagnostic. Sum of bytes across every live file in
/// the VFS. Useful for "did the working set actually go through
/// the VFS layer" assertions.
pub fn bytes_in_use() -> u64 {
    let files = FILES.lock();
    files
        .values()
        .map(|s| s.lock().size())
        .sum()
}

/// Drop every file the VFS holds. Test-only helper  production
/// embedders should call SQLite's own cleanup paths.
pub fn clear_for_tests() {
    FILES.lock().clear();
    TEMP_COUNTER.store(0, Ordering::Relaxed);
}

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
    use std::ffi::CString;

    /// State-mutating tests share the process-global `FILES`
    /// table; without this mutex cargo's parallel test runner
    /// races them and `file_count` assertions flap. The
    /// trampoline + storage tests below take this lock before
    /// touching the table.
    static TEST_STATE_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn vfs_constants_are_sane() {
        // szOsFile must match what we cast to.
        let (sz_os, name_ptr) = unsafe { (VFS.0.szOsFile, VFS.0.zName) };
        assert_eq!(sz_os as usize, std::mem::size_of::<TvmFile>());
        // Name pointer must end at a NUL.
        let cstr = unsafe { CStr::from_ptr(name_ptr) };
        assert_eq!(cstr.to_str().unwrap(), "tvm-mem");
    }

    #[test]
    fn diagnostics_start_empty() {
        let _g = TEST_STATE_MUTEX.lock();
        clear_for_tests();
        assert_eq!(file_count(), 0);
        assert_eq!(bytes_in_use(), 0);
    }

    #[test]
    fn temp_counter_increments_per_synthetic_name() {
        clear_for_tests();
        let a = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let b = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        assert_ne!(a, b);
    }

    /// Drive open + write + read directly through the trampolines
    /// (no real SQLite). Catches any signature / lifetime regression
    /// before they manifest as a SQLite assertion.
    #[test]
    fn open_write_read_close_round_trip() {
        let _g = TEST_STATE_MUTEX.lock();
        clear_for_tests();
        // Allocate a slab the size SQLite would.
        let mut slab: Vec<u8> = vec![0; std::mem::size_of::<TvmFile>()];
        let name = CString::new("/probe.db").unwrap();
        let mut out_flags: c_int = 0;
        let rc = unsafe {
            vfs_open(
                ptr::null_mut(),
                name.as_ptr() as sqlite3_filename,
                slab.as_mut_ptr() as *mut sqlite3_file,
                ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE,
                &mut out_flags,
            )
        };
        assert_eq!(rc, ffi::SQLITE_OK);

        let file = slab.as_mut_ptr() as *mut sqlite3_file;
        let payload = b"hello vfs";
        let rc = unsafe { io_write(file, payload.as_ptr() as *const c_void, payload.len() as c_int, 0) };
        assert_eq!(rc, ffi::SQLITE_OK);

        let mut readback = vec![0u8; payload.len()];
        let rc = unsafe {
            io_read(
                file,
                readback.as_mut_ptr() as *mut c_void,
                payload.len() as c_int,
                0,
            )
        };
        assert_eq!(rc, ffi::SQLITE_OK);
        assert_eq!(&readback, payload);

        let mut size: sqlite3_int64 = 0;
        let rc = unsafe { io_file_size(file, &mut size) };
        assert_eq!(rc, ffi::SQLITE_OK);
        assert_eq!(size as usize, payload.len());

        let rc = unsafe { io_close(file) };
        assert_eq!(rc, ffi::SQLITE_OK);

        // After close, the FILES table still holds the storage
        // (delete_on_close=false for named files), so a reopen
        // sees the data.
        assert_eq!(file_count(), 1);
        assert_eq!(bytes_in_use(), payload.len() as u64);
    }

    #[test]
    fn temp_file_is_deleted_on_close() {
        let _g = TEST_STATE_MUTEX.lock();
        clear_for_tests();
        let mut slab: Vec<u8> = vec![0; std::mem::size_of::<TvmFile>()];
        let rc = unsafe {
            vfs_open(
                ptr::null_mut(),
                ptr::null(),  // NULL = temp file
                slab.as_mut_ptr() as *mut sqlite3_file,
                ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, ffi::SQLITE_OK);
        assert_eq!(file_count(), 1);
        unsafe { io_close(slab.as_mut_ptr() as *mut sqlite3_file) };
        assert_eq!(file_count(), 0, "temp file should be removed on close");
    }
}
