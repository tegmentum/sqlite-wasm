//! WIT-import shim implementing `sqlite_vfs_tvm::opfs::OpfsBackend`
//! against `sqlink:wasm/opfs-host`. Registered with the VFS in
//! `tvm_cold_tier_init()` so the `"opfs"` VFS calls land in the JS
//! host.
//!
//! Under JSPI the imports listed in the host's `asyncImports` list
//! (sqlink-composed.js's `ASYNC_IMPORTS`) suspend the wasm guest
//! until the host's Promise resolves. From this Rust frame's
//! perspective the call is a normal synchronous function — JSPI
//! transparently parks and resumes the stack.
//!
//! Native targets compile out this module entirely (the `cfg`
//! gate on `mod opfs_backend` in lib.rs); the cas connection on
//! native opens against the host filesystem, not the opfs VFS.

use crate::bindings::sqlink::wasm::opfs_host;
use sqlite_vfs_tvm::opfs::{OpfsBackend, OpfsErrorCode};

/// Zero-sized type — all state lives host-side, indexed by the
/// u64 handles the host returns from `open`.
pub struct WitOpfsBackend;

fn map_err(e: opfs_host::OpfsError) -> OpfsErrorCode {
    match e.code {
        opfs_host::OpfsErrorCode::Io => OpfsErrorCode::Io,
        opfs_host::OpfsErrorCode::NotFound => OpfsErrorCode::NotFound,
        opfs_host::OpfsErrorCode::Full => OpfsErrorCode::Full,
        opfs_host::OpfsErrorCode::Invalid => OpfsErrorCode::Invalid,
    }
}

impl OpfsBackend for WitOpfsBackend {
    fn open(&self, path: &str, create: bool) -> Result<u64, OpfsErrorCode> {
        opfs_host::open(path, create).map_err(map_err)
    }
    fn read(&self, handle: u64, offset: u64, len: u32) -> Result<Vec<u8>, OpfsErrorCode> {
        opfs_host::read(handle, offset, len).map_err(map_err)
    }
    fn write(&self, handle: u64, offset: u64, data: &[u8]) -> Result<u32, OpfsErrorCode> {
        opfs_host::write(handle, offset, data).map_err(map_err)
    }
    fn truncate(&self, handle: u64, size: u64) -> Result<(), OpfsErrorCode> {
        opfs_host::truncate(handle, size).map_err(map_err)
    }
    fn sync(&self, handle: u64) -> Result<(), OpfsErrorCode> {
        opfs_host::sync(handle).map_err(map_err)
    }
    fn size(&self, handle: u64) -> Result<u64, OpfsErrorCode> {
        opfs_host::size(handle).map_err(map_err)
    }
    fn close(&self, handle: u64) -> Result<(), OpfsErrorCode> {
        opfs_host::close(handle).map_err(map_err)
    }
    fn delete(&self, path: &str) -> Result<(), OpfsErrorCode> {
        opfs_host::delete(path).map_err(map_err)
    }
}
