//! File-storage abstraction. Each open VFS "file" maps to a
//! `Box<dyn FileStorage>` here. Phase 4.0 ships the in-process
//! `Vec<u8>` impl; Phase 4.1 will plug in a `WitTvmStorage`
//! backed by `tvm:memory` regions, behind a cargo feature
//! exactly the same shape as `sqlite-pcache-tvm`'s
//! `region::Region` / `wit_tvm_region::WitTvmRegion` split.

use libsqlite3_sys as ffi;

/// SQLite error codes the storage layer surfaces. Trampoline
/// translates Result<_, c_int> directly into SQLite return codes
/// (`SQLITE_OK`, `SQLITE_IOERR`, `SQLITE_IOERR_SHORT_READ`).
pub type StorageResult<T> = Result<T, std::os::raw::c_int>;

/// Per-file storage. `&self` for `read` and `size` because the
/// SQLite-facing trampoline only holds a shared lock during
/// reads; `&mut self` for `write` and `truncate` because those
/// mutate the file. The actual locking lives in the trampoline
/// (a `parking_lot::Mutex<dyn FileStorage>` around this), so
/// impls don't have to do their own.
pub trait FileStorage: Send {
    /// Read exactly `buf.len()` bytes starting at `offset`. If
    /// the read would extend past the end of the file, the
    /// trampoline returns `SQLITE_IOERR_SHORT_READ` — implementations
    /// signal that by returning Err(SQLITE_IOERR_SHORT_READ) and
    /// filling the prefix with whatever's available.
    fn read(&self, offset: u64, buf: &mut [u8]) -> StorageResult<()>;

    /// Write `buf` starting at `offset`. The file grows if
    /// `offset + buf.len() > size()`.
    fn write(&mut self, offset: u64, buf: &[u8]) -> StorageResult<()>;

    /// Set the file's size to exactly `size` bytes (grow with
    /// zeros, shrink by truncation).
    fn truncate(&mut self, size: u64) -> StorageResult<()>;

    /// Current file size in bytes.
    fn size(&self) -> u64;
}

/// Default in-process backend: a `Vec<u8>` per file. Grows on
/// demand from writes past end-of-file.
pub struct InProcStorage {
    bytes: Vec<u8>,
}

impl InProcStorage {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }
}

impl Default for InProcStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl FileStorage for InProcStorage {
    fn read(&self, offset: u64, buf: &mut [u8]) -> StorageResult<()> {
        let offset = offset as usize;
        let len = buf.len();
        let file_len = self.bytes.len();
        if offset >= file_len {
            // Reading past EOF  zero-fill, signal short read.
            buf.fill(0);
            return Err(ffi::SQLITE_IOERR_SHORT_READ);
        }
        let avail = file_len - offset;
        if avail >= len {
            buf.copy_from_slice(&self.bytes[offset..offset + len]);
            Ok(())
        } else {
            // Partial read available  copy what we have, zero the
            // rest, signal short read per SQLite contract.
            buf[..avail].copy_from_slice(&self.bytes[offset..file_len]);
            buf[avail..].fill(0);
            Err(ffi::SQLITE_IOERR_SHORT_READ)
        }
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> StorageResult<()> {
        let offset = offset as usize;
        let end = offset + buf.len();
        if end > self.bytes.len() {
            self.bytes.resize(end, 0);
        }
        self.bytes[offset..end].copy_from_slice(buf);
        Ok(())
    }

    fn truncate(&mut self, size: u64) -> StorageResult<()> {
        let size = size as usize;
        // resize() handles both grow (zero-fill) and shrink in one
        // call, which is the contract we want.
        self.bytes.resize(size, 0);
        Ok(())
    }

    fn size(&self) -> u64 {
        self.bytes.len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_then_write_round_trip() {
        let mut s = InProcStorage::new();
        s.write(0, b"hello world").unwrap();
        let mut buf = [0u8; 11];
        s.read(0, &mut buf).unwrap();
        assert_eq!(&buf, b"hello world");
        assert_eq!(s.size(), 11);
    }

    #[test]
    fn write_past_eof_grows_the_file() {
        let mut s = InProcStorage::new();
        s.write(8, b"end").unwrap();
        assert_eq!(s.size(), 11, "write at offset 8 + 3 bytes should grow to 11");
        let mut buf = [0u8; 11];
        s.read(0, &mut buf).unwrap();
        // First 8 bytes filled with zeros, last 3 are "end".
        assert_eq!(&buf[..8], &[0u8; 8]);
        assert_eq!(&buf[8..], b"end");
    }

    #[test]
    fn read_past_eof_short_reads() {
        let mut s = InProcStorage::new();
        s.write(0, b"ab").unwrap();
        let mut buf = [0xff; 4];
        let r = s.read(0, &mut buf);
        assert!(r.is_err());
        assert_eq!(r.unwrap_err(), ffi::SQLITE_IOERR_SHORT_READ);
        assert_eq!(&buf, b"ab\0\0", "available bytes preserved, tail zeroed");
    }

    #[test]
    fn truncate_grows_with_zeros_shrinks_in_place() {
        let mut s = InProcStorage::new();
        s.write(0, b"hello").unwrap();
        s.truncate(8).unwrap();
        assert_eq!(s.size(), 8);
        let mut buf = [0xff; 8];
        s.read(0, &mut buf).unwrap();
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(&buf[5..], &[0u8; 3], "grown bytes should be zero");
        s.truncate(3).unwrap();
        assert_eq!(s.size(), 3);
        let mut buf = [0u8; 3];
        s.read(0, &mut buf).unwrap();
        assert_eq!(&buf, b"hel");
    }
}
