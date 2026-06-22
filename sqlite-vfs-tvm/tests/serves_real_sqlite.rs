//! Integration test: register `tvm-mem` as the default VFS,
//! open a real SQLite connection at any path (it's not a path
//! to anything on the host filesystem  it's a key into our
//! in-memory file table), run a workload. If the trampoline
//! signatures, lifetime model, or io_methods table are wrong,
//! SQLite will assert, segfault, or return wrong data.
//!
//! This is the Phase 4.0 load-bearing assertion: real SQLite
//! drove our VFS for a CREATE-INSERT-SELECT workload end to
//! end. Schema, b-tree pages, write-ahead changes  every byte
//! flowed through `vfs_open`/`io_read`/`io_write`.
//!
//! Runs in its own integration-test binary so the
//! `install_as_default()` call doesn't leak between tests:
//! sqlite3_vfs_register's `make_default=1` mutates a process-
//! wide default-VFS pointer that subsequent sqlite3_open_v2
//! calls observe.

use std::sync::{Mutex, Once};

use sqlite_component_core::db::{Connection, OpenFlags, StepResult, Value};

static INSTALL: Once = Once::new();

/// All integration tests in this file touch the process-global
/// `FILES` table inside sqlite-vfs-tvm; cargo's parallel runner
/// would race their `file_count()` assertions. Same pattern the
/// lib tests use.
static TEST_STATE_MUTEX: Mutex<()> = Mutex::new(());

fn install_once() {
    INSTALL.call_once(|| {
        sqlite_vfs_tvm::install_as_default()
            .expect("install tvm-mem as default VFS");
    });
}

#[test]
fn vfs_serves_a_basic_workload() {
    let _g = TEST_STATE_MUTEX.lock();
    install_once();

    // Path is arbitrary  it's just a key into the in-memory
    // file table, not a real filesystem path. Routing through
    // the VFS happens because we're the default.
    let c = Connection::open("/probe.db", OpenFlags::DEFAULT)
        .expect("open against tvm-mem VFS");

    c.execute_batch(
        "CREATE TABLE numbers(n INTEGER PRIMARY KEY, label TEXT); \
         INSERT INTO numbers VALUES \
            (1, 'one'),(2, 'two'),(3, 'three'),(4, 'four'),(5, 'five');",
    )
    .expect("seed numbers");

    let mut s = c
        .prepare("SELECT n, label FROM numbers ORDER BY n")
        .expect("prepare select");
    let mut rows: Vec<(i64, String)> = Vec::new();
    loop {
        match s.step().expect("step") {
            StepResult::Row => {
                let n = match s.column_value(0) {
                    Value::Integer(i) => i,
                    other => panic!("col 0 should be integer, got {other:?}"),
                };
                let label = match s.column_value(1) {
                    Value::Text(t) => t,
                    other => panic!("col 1 should be text, got {other:?}"),
                };
                rows.push((n, label));
            }
            StepResult::Done => break,
        }
    }
    assert_eq!(
        rows,
        vec![
            (1, "one".to_string()),
            (2, "two".to_string()),
            (3, "three".to_string()),
            (4, "four".to_string()),
            (5, "five".to_string()),
        ]
    );

    // After the workload, the VFS should hold at least the main
    // db. (Rollback journal too if we're not in WAL mode
    // SQLite removes it after each commit but the journal name
    // bounces in and out.) The MAIN db must be present.
    let file_count = sqlite_vfs_tvm::file_count();
    let bytes = sqlite_vfs_tvm::bytes_in_use();
    assert!(
        file_count >= 1,
        "expected at least the main db in the VFS, got {file_count} files"
    );
    assert!(
        bytes >= 4096,
        "expected at least one sqlite page (4 KB), got {bytes} bytes"
    );
    eprintln!("PASS: VFS holds {file_count} file(s), {bytes} bytes total");
}

#[test]
fn data_persists_across_close_and_reopen() {
    let _g = TEST_STATE_MUTEX.lock();
    install_once();

    // First connection: write some rows.
    {
        let c = Connection::open("/persist.db", OpenFlags::DEFAULT).expect("open #1");
        c.execute_batch(
            "CREATE TABLE t(v INTEGER); \
             INSERT INTO t VALUES (10),(20),(30);",
        )
        .expect("seed");
    } // dropped  SQLite closes the connection, our xClose fires

    // Second connection at the same path: storage should be shared
    // through the FILES table.
    let c = Connection::open("/persist.db", OpenFlags::DEFAULT).expect("open #2");
    let mut s = c
        .prepare("SELECT sum(v) FROM t")
        .expect("prepare sum");
    match s.step().expect("step") {
        StepResult::Row => match s.column_value(0) {
            Value::Integer(n) => assert_eq!(n, 60, "10+20+30 should be 60, got {n}"),
            other => panic!("sum should be integer, got {other:?}"),
        },
        StepResult::Done => panic!("sum query returned no row"),
    }
}

#[test]
fn open_in_memory_routes_through_tvm_mem_when_registered() {
    let _g = TEST_STATE_MUTEX.lock();
    install_once();

    // Snapshot the count before  earlier tests in this file
    // share the process and may have left files behind.
    let before = sqlite_vfs_tvm::file_count();

    // Drop scope so the connection drops and DELETEONCLOSE
    // fires, removing the synthetic in-mem path from the FILES
    // table.
    {
        let c = Connection::open_in_memory().expect("open_in_memory routed");
        // Use it like any other db. If routing went to sqlite's
        // memdb VFS (the fallback) by mistake, file_count
        // wouldn't budge above `before`.
        c.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1),(2),(3);")
            .expect("seed");

        let during = sqlite_vfs_tvm::file_count();
        assert!(
            during > before,
            "tvm-mem should hold the anonymous in-mem db while open; before={before} during={during}"
        );
    }
    // After drop, DELETEONCLOSE should remove the anonymous file.
    let after = sqlite_vfs_tvm::file_count();
    assert_eq!(
        after, before,
        "DELETEONCLOSE should clean up the anonymous in-mem db after the connection drops; before={before} after={after}"
    );
}
