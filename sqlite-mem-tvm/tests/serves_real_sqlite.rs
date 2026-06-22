//! Integration test: register our `sqlite3_mem_methods` BEFORE
//! any sqlite3 initialization, then drive a real connection
//! through it. Every SQLite allocation routes through our
//! trampolines  schema, statement bytecode, b-tree pages,
//! result buffers, lookaside, all of it.
//!
//! What this catches: any size-header bookkeeping bug (xSize
//! returning wrong values would corrupt sqlite's allocator
//! accounting), realloc behavior (sqlite grows result buffers
//! and string accumulators), and alignment regressions
//! (sqlite-internal structs have alignment expectations).
//!
//! Runs in its own integration-test binary (one #[test] per
//! file, sharing process state through the `Once`). SQLite's
//! sqlite3_config(SQLITE_CONFIG_MALLOC, ...) is only legal
//! before sqlite3_initialize  sharing a process with other
//! tests that called sqlite3_initialize would fail with
//! SQLITE_MISUSE.

use std::sync::Once;

use sqlite_component_core::db::{Connection, StepResult, Value};

static INSTALL: Once = Once::new();

fn install_once() {
    INSTALL.call_once(|| {
        sqlite_mem_tvm::install().expect("install mem methods before initialize");
    });
}

#[test]
fn mem_methods_serve_a_basic_workload() {
    install_once();

    let c = Connection::open_in_memory().expect("open in-memory db");

    // Schema + multi-row INSERT  forces sqlite to allocate
    // through xMalloc/xRealloc for table metadata, statement
    // bytecode, prepared statement structs, b-tree pages
    // (when the default pcache is used).
    c.execute_batch(
        "CREATE TABLE numbers(n INTEGER PRIMARY KEY, label TEXT); \
         INSERT INTO numbers VALUES \
            (1, 'one'),(2, 'two'),(3, 'three'),(4, 'four'),(5, 'five');",
    )
    .expect("seed numbers table");

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

    // After the workload, malloc/free counters should both be
    // non-zero (allocator was driven) and bytes_outstanding
    // should be reasonable (a few MB for the open connection +
    // 5 rows of metadata).
    let (mallocs, frees, reallocs, bytes_outstanding, bytes_lifetime) =
        sqlite_mem_tvm::diagnostics();
    assert!(mallocs > 0, "xMalloc should have been called");
    assert!(frees > 0, "xFree should have been called");
    // reallocs is allowed to be 0  small workloads might never
    // resize  but lifetime bytes should reflect allocator
    // pressure.
    let _ = reallocs;
    assert!(
        bytes_lifetime > 1024,
        "expected > 1 KB of allocator traffic, got {bytes_lifetime}"
    );
    // bytes_outstanding should be > 0 while the connection is
    // still alive (schema + statement state).
    assert!(
        bytes_outstanding > 0,
        "should have live allocations while conn is open"
    );
    eprintln!(
        "PASS: malloc={mallocs} free={frees} realloc={reallocs} \
         outstanding={bytes_outstanding} lifetime={bytes_lifetime}"
    );
}

#[test]
fn realloc_heavy_string_aggregation() {
    install_once();
    let c = Connection::open_in_memory().expect("open");

    // group_concat builds up a string by repeatedly reallocating
    // its accumulator. With 1000 small rows we should see the
    // realloc counter climb meaningfully.
    c.execute_batch(
        "CREATE TABLE t(x TEXT);
         WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<1000) \
         INSERT INTO t SELECT 'item-' || n FROM seq;",
    )
    .expect("seed 1000 rows");

    let (_, _, reallocs_before, _, _) = sqlite_mem_tvm::diagnostics();

    let mut s = c
        .prepare("SELECT group_concat(x) FROM t")
        .expect("prepare group_concat");
    match s.step().expect("step") {
        StepResult::Row => match s.column_value(0) {
            Value::Text(t) => {
                // group_concat with 1000 items separated by commas
                // produces a string in the 10s of KB. Confirm it.
                assert!(
                    t.len() > 5_000,
                    "expected > 5 KB string, got {} bytes",
                    t.len()
                );
                assert!(t.starts_with("item-1,"), "got: {}", &t[..30]);
            }
            other => panic!("got: {other:?}"),
        },
        StepResult::Done => panic!("group_concat returned no row"),
    }
    let (_, _, reallocs_after, _, _) = sqlite_mem_tvm::diagnostics();
    assert!(
        reallocs_after > reallocs_before,
        "group_concat over 1000 rows should have triggered reallocs (before={reallocs_before} after={reallocs_after})"
    );
}
