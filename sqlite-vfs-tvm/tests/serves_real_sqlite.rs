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

/// Regression for the path-3 trap: SQLite's pager opens, closes,
/// and `xDelete`s the rollback journal repeatedly while a single
/// connection drives a workload (one cycle per commit). Before
/// the multi-memory slice recycling landed, every cycle consumed
/// a fresh `SLICE_BYTES` window in pool 2, walking the bump
/// cursor past the pool's `--max-pages` ceiling after just a
/// couple of commits and trapping the next write OOB.
///
/// The in-process `InProcStorage` path doesn't have the pool /
/// max-pages constraint, so the corresponding wasm32 failure mode
/// can't surface here. Instead we assert the FILES table
/// stabilises across many transactions — if the journal entry
/// were getting recreated each cycle, native `file_count()`
/// would stay flat (each xDelete removes it from the table)
/// **but** on wasm32 the slice cursor would walk forward. The
/// behaviour we want is the same in both: journal lifecycle is
/// stable across transactions, the FILES table doesn't keep
/// growing, and reads after many cycles still return the right
/// rows.
#[test]
fn many_transactions_do_not_leak_journal_slices() {
    let _g = TEST_STATE_MUTEX.lock();
    install_once();

    let before = sqlite_vfs_tvm::file_count();
    let c = Connection::open_in_memory().expect("open_in_memory");
    c.execute_batch("CREATE TABLE t(x INTEGER);").expect("create");

    // Each separate execute_batch + commit cycle through the
    // pager runs the open/write/close/delete dance on the
    // journal. Drive enough cycles that a fresh-slice-per-cycle
    // bug would compound visibly (and would have tripped the
    // wasm32 pool ceiling several times over).
    for i in 0..16 {
        let sql = format!("INSERT INTO t VALUES ({i});");
        c.execute_batch(&sql).expect("insert in its own txn");
    }

    let mut s = c.prepare("SELECT count(*), sum(x) FROM t").expect("prepare");
    match s.step().expect("step") {
        StepResult::Row => {
            let count = match s.column_value(0) {
                Value::Integer(n) => n,
                other => panic!("count not integer: {other:?}"),
            };
            let sum = match s.column_value(1) {
                Value::Integer(n) => n,
                other => panic!("sum not integer: {other:?}"),
            };
            assert_eq!(count, 16, "all 16 inserts should be visible");
            assert_eq!(sum, (0..16).sum::<i64>());
        }
        StepResult::Done => panic!("no row from count query"),
    }

    drop(s);
    drop(c);

    // After the connection drops, DELETEONCLOSE on the anon main
    // db should sweep its journal too, leaving the FILES table
    // back where it started.
    let after = sqlite_vfs_tvm::file_count();
    assert_eq!(after, before, "main db close should sweep auxiliaries; before={before} after={after}");
}

/// Verifies the WAL substrate (file-locking bookkeeping +
/// xShm* family on iVersion=2 io_methods). On a tvm-mem-backed
/// connection, `PRAGMA journal_mode=WAL` must report "wal" — the
/// substrate having any gap (missing shm map, broken lock
/// transition, wrong iVersion) collapses to a fallback mode
/// (typically "delete") and the assertion catches it.
///
/// After WAL is engaged, INSERTs append frames to the -wal file
/// and update the wal-index in the -shm file. We then read the
/// rows back to confirm WAL-mode reads see uncheckpointed frames
/// correctly. Without xShmMap the wal-index can't be set up and
/// the connection refuses WAL.
#[test]
fn pragma_journal_mode_wal_engages_wal_substrate() {
    let _g = TEST_STATE_MUTEX.lock();
    install_once();

    let c = Connection::open("/wal-substrate.db", OpenFlags::DEFAULT)
        .expect("open against tvm-mem");

    // Engage WAL. The cli echoes the resulting mode; if any piece
    // of the substrate is missing SQLite silently keeps the old
    // mode (usually "delete"), so we read it back and assert.
    let mut s = c
        .prepare("PRAGMA journal_mode=WAL")
        .expect("prepare pragma");
    let mode = match s.step().expect("step pragma") {
        StepResult::Row => match s.column_value(0) {
            Value::Text(t) => t,
            other => panic!("pragma column should be text, got {other:?}"),
        },
        StepResult::Done => panic!("pragma returned no row"),
    };
    drop(s);
    assert_eq!(
        mode, "wal",
        "tvm-mem VFS should support WAL after the iVersion=2 + xShm* substrate; got {mode:?}"
    );

    // Drive a few commits — each appends frames to the -wal file
    // and updates the wal-index in the -shm file. If xShmMap /
    // xShmLock are broken the inserts would have failed by here.
    c.execute_batch(
        "CREATE TABLE w(v INTEGER); \
         INSERT INTO w VALUES (1); \
         INSERT INTO w VALUES (2); \
         INSERT INTO w VALUES (3);",
    )
    .expect("insert during WAL mode");

    // Read back through the same connection. In WAL mode reads
    // see uncheckpointed frames via the wal-index lookup; a
    // broken shm would surface as missing rows or wrong values.
    let mut s = c.prepare("SELECT sum(v) FROM w").expect("prepare sum");
    let sum = match s.step().expect("step sum") {
        StepResult::Row => match s.column_value(0) {
            Value::Integer(n) => n,
            other => panic!("sum should be integer, got {other:?}"),
        },
        StepResult::Done => panic!("sum returned no row"),
    };
    assert_eq!(sum, 6, "1+2+3 should be 6 in WAL mode");

    // Confirm the FILES table now contains the main db plus the
    // WAL auxiliaries (-wal, -shm). At least 2 files (main + one
    // wal aux) must be present; usually 3 (main + -wal + -shm).
    let files = sqlite_vfs_tvm::file_count();
    assert!(
        files >= 2,
        "WAL mode should have created -wal / -shm auxiliaries; FILES count is {files}"
    );
    eprintln!("PASS: journal_mode=wal engaged; FILES holds {files} entries");
}
