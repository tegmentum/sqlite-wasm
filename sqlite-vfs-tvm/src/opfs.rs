//! `"opfs"` VFS — file bytes live in the JS host's Origin Private
//! File System (OPFS).
//!
//! Architecture: each VFS operation (xRead/xWrite/xSync/...) trampolines
//! through a Rust-side `OpfsBackend` trait whose impl lives in
//! sqlite-lib. The sqlite-lib impl is a thin shim over the
//! wit-bindgen-generated `sqlite::wasm::opfs_host` imports. Putting
//! the trait here (instead of wiring sqlite-vfs-tvm directly to
//! the WIT) keeps sqlite-vfs-tvm free of any wit-bindgen
//! dependency — it still tests cleanly on native, and the
//! browser composition wires the real implementation at runtime
//! via `register(backend)`.
//!
//! From the wasm guest's POV the WIT-imported calls are async (they
//! are listed in `asyncImports` for the runtime-bindgen JSPI
//! transpile). JSPI suspends the entire stack on each call — the
//! Rust frame holding the `parking_lot::Mutex` guard suspends with
//! it — and resumes synchronously when the host's Promise resolves.
//! The SQLite trampoline sees a synchronous call return; SQLite has
//! no idea it suspended.

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use libsqlite3_sys as ffi;
use libsqlite3_sys::{
    sqlite3_file, sqlite3_filename, sqlite3_int64, sqlite3_io_methods, sqlite3_vfs,
};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

/// Coarse error class the host can surface. Mirrors
/// `opfs-host.opfs-error-code` in the WIT. The trampoline maps each
/// to a SQLITE_* code.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OpfsErrorCode {
    Io,
    NotFound,
    Full,
    Invalid,
}

impl OpfsErrorCode {
    fn to_sqlite(self) -> c_int {
        match self {
            OpfsErrorCode::Io => ffi::SQLITE_IOERR,
            OpfsErrorCode::NotFound => ffi::SQLITE_CANTOPEN,
            OpfsErrorCode::Full => ffi::SQLITE_FULL,
            OpfsErrorCode::Invalid => ffi::SQLITE_MISUSE,
        }
    }
}

/// Trait the sqlite-lib browser bindings implement against the WIT
/// `opfs-host` imports. Native targets supply a no-op backend
/// (`UnsupportedBackend`) so the `"opfs"` VFS still registers and
/// returns SQLITE_CANTOPEN on first use — keeps the native build
/// linkable without forcing every embedder to wire OPFS.
pub trait OpfsBackend: Send + Sync {
    fn open(&self, path: &str, create: bool) -> Result<u64, OpfsErrorCode>;
    fn read(&self, handle: u64, offset: u64, len: u32) -> Result<Vec<u8>, OpfsErrorCode>;
    fn write(&self, handle: u64, offset: u64, data: &[u8]) -> Result<u32, OpfsErrorCode>;
    fn truncate(&self, handle: u64, size: u64) -> Result<(), OpfsErrorCode>;
    fn sync(&self, handle: u64) -> Result<(), OpfsErrorCode>;
    fn size(&self, handle: u64) -> Result<u64, OpfsErrorCode>;
    fn close(&self, handle: u64) -> Result<(), OpfsErrorCode>;
    fn delete(&self, path: &str) -> Result<(), OpfsErrorCode>;
}

/// Backend installed when the host hasn't called `register_backend`.
/// Every op returns NotFound so a misconfigured composition fails
/// loudly at first open instead of silently corrupting an
/// in-memory pretend-file.
struct UnsupportedBackend;
impl OpfsBackend for UnsupportedBackend {
    fn open(&self, _: &str, _: bool) -> Result<u64, OpfsErrorCode> {
        Err(OpfsErrorCode::NotFound)
    }
    fn read(&self, _: u64, _: u64, _: u32) -> Result<Vec<u8>, OpfsErrorCode> {
        Err(OpfsErrorCode::Io)
    }
    fn write(&self, _: u64, _: u64, _: &[u8]) -> Result<u32, OpfsErrorCode> {
        Err(OpfsErrorCode::Io)
    }
    fn truncate(&self, _: u64, _: u64) -> Result<(), OpfsErrorCode> {
        Err(OpfsErrorCode::Io)
    }
    fn sync(&self, _: u64) -> Result<(), OpfsErrorCode> {
        Err(OpfsErrorCode::Io)
    }
    fn size(&self, _: u64) -> Result<u64, OpfsErrorCode> {
        Err(OpfsErrorCode::Io)
    }
    fn close(&self, _: u64) -> Result<(), OpfsErrorCode> {
        Ok(())
    }
    fn delete(&self, _: &str) -> Result<(), OpfsErrorCode> {
        Ok(())
    }
}

/// Process-global backend slot. Replaced once at composition wire-up
/// time via `register_backend`. The `static mut` is gated by an
/// `AtomicBool` so re-registration is a no-op after the first call.
static BACKEND_INSTALLED: AtomicBool = AtomicBool::new(false);
static BACKEND: Lazy<Mutex<Box<dyn OpfsBackend>>> =
    Lazy::new(|| Mutex::new(Box::new(UnsupportedBackend) as Box<dyn OpfsBackend>));

/// Install the host-side backend. Idempotent — only the first call
/// takes effect.
pub fn register_backend(backend: Box<dyn OpfsBackend>) {
    if BACKEND_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    *BACKEND.lock() = backend;
}

fn with_backend<R>(f: impl FnOnce(&dyn OpfsBackend) -> R) -> R {
    let g = BACKEND.lock();
    f(&**g)
}

// ───────────── VFS / IO method tables ─────────────

const VFS_NAME_C: &[u8] = b"opfs\0";
const VFS_NAME_STR: &str = "opfs";
const MAX_PATHNAME: c_int = 1024;

/// Per-file state. Keyed by an OPFS handle u64 the host assigns at
/// open; auxiliary metadata (path, delete-on-close, lock level) lives
/// alongside.
struct OpfsFileInner {
    handle: u64,
    name: String,
    delete_on_close: bool,
    lock_level: c_int,
}

#[repr(C)]
struct OpfsFile {
    base: sqlite3_file,
    inner: *mut OpfsFileInner,
}

// In-memory bookkeeping for files we've heard of via xOpen. Used by
// xAccess so SQLite's "does the journal exist" probe answers
// correctly without re-opening — and by xDelete since the host's
// path-based delete is the canonical authority.
type FileTable = HashMap<String, ()>;
static FILES_SEEN: Lazy<Mutex<FileTable>> = Lazy::new(|| Mutex::new(HashMap::new()));
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn io_close(file: *mut sqlite3_file) -> c_int {
    let tf = file as *mut OpfsFile;
    let inner_ptr = (*tf).inner;
    if inner_ptr.is_null() {
        return ffi::SQLITE_OK;
    }
    let inner = Box::from_raw(inner_ptr);
    let _ = with_backend(|b| b.close(inner.handle));
    if inner.delete_on_close {
        let _ = with_backend(|b| b.delete(&inner.name));
        FILES_SEEN.lock().remove(&inner.name);
    }
    (*tf).inner = ptr::null_mut();
    ffi::SQLITE_OK
}

unsafe extern "C" fn io_read(
    file: *mut sqlite3_file,
    buf: *mut c_void,
    amt: c_int,
    ofst: sqlite3_int64,
) -> c_int {
    let tf = &*(file as *mut OpfsFile);
    if tf.inner.is_null() || amt <= 0 || buf.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &*tf.inner;
    let want = amt as usize;
    let result = with_backend(|b| b.read(inner.handle, ofst as u64, amt as u32));
    match result {
        Ok(bytes) => {
            let dst = std::slice::from_raw_parts_mut(buf as *mut u8, want);
            let got = bytes.len();
            let copy = got.min(want);
            dst[..copy].copy_from_slice(&bytes[..copy]);
            if got < want {
                // Zero-fill the tail per SQLite's short-read contract.
                for b in &mut dst[copy..] {
                    *b = 0;
                }
                ffi::SQLITE_IOERR_SHORT_READ
            } else {
                ffi::SQLITE_OK
            }
        }
        Err(e) => e.to_sqlite(),
    }
}

unsafe extern "C" fn io_write(
    file: *mut sqlite3_file,
    buf: *const c_void,
    amt: c_int,
    ofst: sqlite3_int64,
) -> c_int {
    let tf = &*(file as *mut OpfsFile);
    if tf.inner.is_null() || amt <= 0 || buf.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &*tf.inner;
    let slice = std::slice::from_raw_parts(buf as *const u8, amt as usize);
    match with_backend(|b| b.write(inner.handle, ofst as u64, slice)) {
        Ok(n) if n as usize == slice.len() => ffi::SQLITE_OK,
        Ok(_) => ffi::SQLITE_IOERR_WRITE,
        Err(e) => e.to_sqlite(),
    }
}

unsafe extern "C" fn io_truncate(file: *mut sqlite3_file, size: sqlite3_int64) -> c_int {
    let tf = &*(file as *mut OpfsFile);
    if tf.inner.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &*tf.inner;
    match with_backend(|b| b.truncate(inner.handle, size as u64)) {
        Ok(()) => ffi::SQLITE_OK,
        Err(e) => e.to_sqlite(),
    }
}

unsafe extern "C" fn io_sync(file: *mut sqlite3_file, _flags: c_int) -> c_int {
    let tf = &*(file as *mut OpfsFile);
    if tf.inner.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &*tf.inner;
    match with_backend(|b| b.sync(inner.handle)) {
        Ok(()) => ffi::SQLITE_OK,
        Err(e) => e.to_sqlite(),
    }
}

unsafe extern "C" fn io_file_size(
    file: *mut sqlite3_file,
    p_size: *mut sqlite3_int64,
) -> c_int {
    let tf = &*(file as *mut OpfsFile);
    if tf.inner.is_null() || p_size.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let inner = &*tf.inner;
    match with_backend(|b| b.size(inner.handle)) {
        Ok(sz) => {
            *p_size = sz as sqlite3_int64;
            ffi::SQLITE_OK
        }
        Err(e) => e.to_sqlite(),
    }
}

unsafe extern "C" fn io_lock(file: *mut sqlite3_file, level: c_int) -> c_int {
    let tf = file as *mut OpfsFile;
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
    let tf = file as *mut OpfsFile;
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
    if op == ffi::SQLITE_FCNTL_LOCKSTATE {
        let tf = file as *mut OpfsFile;
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
    4096
}

unsafe extern "C" fn io_device_characteristics(_file: *mut sqlite3_file) -> c_int {
    // OPFS is durable after sync; default characteristics suffice.
    ffi::SQLITE_IOCAP_SAFE_APPEND | ffi::SQLITE_IOCAP_SEQUENTIAL
}

#[repr(transparent)]
struct IoMethods(sqlite3_io_methods);
unsafe impl Sync for IoMethods {}

static IO_METHODS: IoMethods = IoMethods(sqlite3_io_methods {
    // iVersion=1: no shm support. OPFS-backed cas db runs in rollback
    // journal mode (not WAL), so xShm* are not required. The cas
    // connection sqlite-lib opens against this VFS uses
    // SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE without explicit
    // WAL, so the default journal_mode (delete) applies.
    iVersion: 1,
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
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
});

// ───────────── VFS-level callbacks ─────────────

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

    let explicit_delete = (flags & ffi::SQLITE_OPEN_DELETEONCLOSE) != 0;
    let create = (flags & ffi::SQLITE_OPEN_CREATE) != 0;

    let (name, delete_on_close) = if z_name.is_null() {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        (format!("/__opfs_tmp_{n}"), true)
    } else {
        let s = match CStr::from_ptr(z_name as *const c_char).to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return ffi::SQLITE_IOERR,
        };
        (s, explicit_delete)
    };

    let handle = match with_backend(|b| b.open(&name, create)) {
        Ok(h) => h,
        Err(e) => return e.to_sqlite(),
    };

    FILES_SEEN.lock().insert(name.clone(), ());

    let inner = Box::into_raw(Box::new(OpfsFileInner {
        handle,
        name,
        delete_on_close,
        lock_level: ffi::SQLITE_LOCK_NONE,
    }));

    let tf = file as *mut OpfsFile;
    (*tf).base.pMethods = &IO_METHODS.0;
    (*tf).inner = inner;

    if !p_out_flags.is_null() {
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
        Ok(s) => s.to_string(),
        Err(_) => return ffi::SQLITE_IOERR,
    };
    FILES_SEEN.lock().remove(&name);
    match with_backend(|b| b.delete(&name)) {
        Ok(()) => ffi::SQLITE_OK,
        Err(e) => e.to_sqlite(),
    }
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
        Ok(s) => s.to_string(),
        Err(_) => return ffi::SQLITE_IOERR,
    };
    // Best-effort: probe by attempting open(create=false); the
    // host's NotFound surfaces as "does not exist".
    let exists = match with_backend(|b| b.open(&name, false)) {
        Ok(h) => {
            let _ = with_backend(|b| b.close(h));
            true
        }
        Err(OpfsErrorCode::NotFound) => false,
        Err(_) => FILES_SEEN.lock().contains_key(&name),
    };
    *p_res_out = if exists { 1 } else { 0 };
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
    let src = CStr::from_ptr(z_name);
    let bytes = src.to_bytes_with_nul();
    let cap = n_out as usize;
    if cap == 0 {
        return ffi::SQLITE_IOERR;
    }
    let copy_len = bytes.len().min(cap);
    ptr::copy_nonoverlapping(bytes.as_ptr(), z_out as *mut u8, copy_len);
    *z_out.add(copy_len - 1) = 0;
    ffi::SQLITE_OK
}

unsafe extern "C" fn vfs_dlopen(_vfs: *mut sqlite3_vfs, _z: *const c_char) -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn vfs_dlerror(_vfs: *mut sqlite3_vfs, _n: c_int, _msg: *mut c_char) {}

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
    static SEED: AtomicU64 = AtomicU64::new(0xA1B2_C3D4_E5F6_0718);
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
    0
}

unsafe extern "C" fn vfs_current_time(_vfs: *mut sqlite3_vfs, p_now: *mut f64) -> c_int {
    if p_now.is_null() {
        return ffi::SQLITE_ERROR;
    }
    *p_now = 2_451_544.5;
    ffi::SQLITE_OK
}

unsafe extern "C" fn vfs_get_last_error(
    _vfs: *mut sqlite3_vfs,
    _n: c_int,
    _msg: *mut c_char,
) -> c_int {
    0
}

unsafe extern "C" fn vfs_current_time_int64(
    _vfs: *mut sqlite3_vfs,
    p_now: *mut sqlite3_int64,
) -> c_int {
    if p_now.is_null() {
        return ffi::SQLITE_ERROR;
    }
    *p_now = (2_451_544.5 * 86_400_000.0) as sqlite3_int64;
    ffi::SQLITE_OK
}

#[repr(transparent)]
struct VfsTable(sqlite3_vfs);
unsafe impl Sync for VfsTable {}

static mut VFS: VfsTable = VfsTable(sqlite3_vfs {
    iVersion: 2,
    szOsFile: std::mem::size_of::<OpfsFile>() as c_int,
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
    xCurrentTimeInt64: Some(vfs_current_time_int64),
    xSetSystemCall: None,
    xGetSystemCall: None,
    xNextSystemCall: None,
});

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register the `"opfs"` VFS with SQLite. Safe to call multiple
/// times — subsequent calls are no-ops.
pub fn install() -> Result<(), crate::InstallError> {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let rc = unsafe {
        let vfs_ptr = &raw mut VFS.0;
        ffi::sqlite3_vfs_register(vfs_ptr, 0)
    };
    if rc != ffi::SQLITE_OK {
        INSTALLED.store(false, Ordering::SeqCst);
        return Err(crate::InstallError {
            code: rc,
            message: "sqlite3_vfs_register failed for opfs vfs".to_string(),
        });
    }
    Ok(())
}

/// VFS name to pass to `sqlite3_open_v2`.
pub fn name() -> &'static str {
    VFS_NAME_STR
}

/// True iff `"opfs"` is currently registered.
pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simple in-memory backend for native unit tests.
    struct MemBackend {
        files: Mutex<HashMap<u64, Vec<u8>>>,
        paths: Mutex<HashMap<String, u64>>,
        next: AtomicU64,
    }

    impl MemBackend {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
                paths: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
            }
        }
    }

    impl OpfsBackend for MemBackend {
        fn open(&self, path: &str, create: bool) -> Result<u64, OpfsErrorCode> {
            let mut paths = self.paths.lock();
            if let Some(&h) = paths.get(path) {
                return Ok(h);
            }
            if !create {
                return Err(OpfsErrorCode::NotFound);
            }
            let h = self.next.fetch_add(1, Ordering::Relaxed);
            paths.insert(path.to_string(), h);
            self.files.lock().insert(h, Vec::new());
            Ok(h)
        }
        fn read(&self, h: u64, off: u64, len: u32) -> Result<Vec<u8>, OpfsErrorCode> {
            let files = self.files.lock();
            let bytes = files.get(&h).ok_or(OpfsErrorCode::Invalid)?;
            let off = off as usize;
            let len = len as usize;
            if off >= bytes.len() {
                return Ok(Vec::new());
            }
            let end = (off + len).min(bytes.len());
            Ok(bytes[off..end].to_vec())
        }
        fn write(&self, h: u64, off: u64, data: &[u8]) -> Result<u32, OpfsErrorCode> {
            let mut files = self.files.lock();
            let bytes = files.get_mut(&h).ok_or(OpfsErrorCode::Invalid)?;
            let off = off as usize;
            let end = off + data.len();
            if end > bytes.len() {
                bytes.resize(end, 0);
            }
            bytes[off..end].copy_from_slice(data);
            Ok(data.len() as u32)
        }
        fn truncate(&self, h: u64, size: u64) -> Result<(), OpfsErrorCode> {
            let mut files = self.files.lock();
            files
                .get_mut(&h)
                .ok_or(OpfsErrorCode::Invalid)?
                .resize(size as usize, 0);
            Ok(())
        }
        fn sync(&self, _h: u64) -> Result<(), OpfsErrorCode> {
            Ok(())
        }
        fn size(&self, h: u64) -> Result<u64, OpfsErrorCode> {
            Ok(self.files.lock().get(&h).map(|v| v.len() as u64).unwrap_or(0))
        }
        fn close(&self, _h: u64) -> Result<(), OpfsErrorCode> {
            Ok(())
        }
        fn delete(&self, path: &str) -> Result<(), OpfsErrorCode> {
            if let Some(h) = self.paths.lock().remove(path) {
                self.files.lock().remove(&h);
            }
            Ok(())
        }
    }

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn install_test_backend() {
        // register_backend is idempotent; for tests we directly swap
        // (bypassing the AtomicBool gate so each test gets a fresh
        // MemBackend).
        *BACKEND.lock() = Box::new(MemBackend::new());
        BACKEND_INSTALLED.store(true, Ordering::SeqCst);
        FILES_SEEN.lock().clear();
    }

    #[test]
    fn open_write_read_close_round_trip() {
        let _g = TEST_LOCK.lock();
        install_test_backend();
        let mut slab: Vec<u8> = vec![0; std::mem::size_of::<OpfsFile>()];
        let name = std::ffi::CString::new("/probe.db").unwrap();
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
        let payload = b"hello opfs";
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
    }

    #[test]
    fn missing_file_open_without_create_returns_cantopen() {
        let _g = TEST_LOCK.lock();
        install_test_backend();
        let mut slab: Vec<u8> = vec![0; std::mem::size_of::<OpfsFile>()];
        let name = std::ffi::CString::new("/nope.db").unwrap();
        let rc = unsafe {
            vfs_open(
                ptr::null_mut(),
                name.as_ptr() as sqlite3_filename,
                slab.as_mut_ptr() as *mut sqlite3_file,
                ffi::SQLITE_OPEN_READWRITE,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, ffi::SQLITE_CANTOPEN);
    }
}
