//! Integration test: register `opfs` VFS with a host-side
//! MemBackend (so the test runs without a real OPFS), open a real
//! SQLite connection through it, and verify the bytes round-trip.
//! This is the "did the VFS surface compile and actually serve real
//! SQLite" assertion for the v1.5 round 4 opfs work — the browser
//! polyfill replaces MemBackend with a real navigator.storage
//! implementation in production.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use parking_lot::Mutex as PlMutex;

use sqlite_component_core::db::{Connection, OpenFlags, StepResult, Value};
use sqlite_vfs_tvm::opfs::{OpfsBackend, OpfsErrorCode};

static TEST_LOCK: Mutex<()> = Mutex::new(());

struct MemBackend {
    files: PlMutex<HashMap<u64, Vec<u8>>>,
    paths: PlMutex<HashMap<String, u64>>,
    next: AtomicU64,
}

impl MemBackend {
    fn new() -> Self {
        Self {
            files: PlMutex::new(HashMap::new()),
            paths: PlMutex::new(HashMap::new()),
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

#[test]
fn opfs_vfs_serves_a_real_workload() {
    let _g = TEST_LOCK.lock();
    sqlite_vfs_tvm::opfs::register_backend(Box::new(MemBackend::new()));
    let _ = sqlite_vfs_tvm::opfs::install();

    let c = Connection::open_with_vfs(
        "/cas.db",
        OpenFlags::DEFAULT,
        Some(sqlite_vfs_tvm::opfs::name()),
    )
    .expect("open through opfs VFS");

    c.execute_batch(
        "CREATE TABLE bundles(id INTEGER PRIMARY KEY, name TEXT); \
         INSERT INTO bundles VALUES (1, 'myset'),(2, 'other');",
    )
    .expect("seed bundles");

    let mut s = c
        .prepare("SELECT id, name FROM bundles ORDER BY id")
        .expect("prepare select");
    let mut rows: Vec<(i64, String)> = Vec::new();
    loop {
        match s.step().unwrap() {
            StepResult::Row => {
                let id = match s.column_value(0) {
                    Value::Integer(i) => i,
                    other => panic!("unexpected id: {other:?}"),
                };
                let name = match s.column_value(1) {
                    Value::Text(t) => t,
                    other => panic!("unexpected name: {other:?}"),
                };
                rows.push((id, name));
            }
            StepResult::Done => break,
        }
    }
    assert_eq!(
        rows,
        vec![(1, "myset".to_string()), (2, "other".to_string())]
    );
}
