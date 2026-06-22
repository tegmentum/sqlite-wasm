//! Integration test: register our pcache2 BEFORE any sqlite3
//! initialization, then drive a real connection through it. If
//! the trampolines + lifetime model + eviction logic are wrong,
//! sqlite will assert, segfault, or return wrong data; this test
//! catches all three.
//!
//! Two layers:
//!
//! 1. `pcache2_serves_an_in_memory_workload` — a basic schema-and-
//!    select round-trip. Exercises xCreate, xFetch (create=1 and
//!    create=0), xUnpin, xPagecount, xDestroy with the SQLite
//!    default cache size. Catches "the trampolines are
//!    structurally wrong" bugs.
//!
//! 2. `pcache2_survives_shadow_overflow` — forces SQLite to use a
//!    tiny cache (PRAGMA cache_size=4) on a workload whose working
//!    set is wider than 4 pages. SQLite repeatedly evicts and
//!    re-fetches pages; if our cold-tier flush+promote is wrong,
//!    the query returns stale or zero bytes instead of real row
//!    data. The 4-page cap is below any realistic SQLite workload
//!    so eviction fires constantly.
//!
//! Runs in its own integration-test binary (one #[test] per file
//! ... well, two — but they share a process and one-time-init
//! state). sqlite3_config(SQLITE_CONFIG_PCACHE2, …) is only legal
//! before sqlite3_initialize; sharing a process with other tests
//! that already called sqlite3_initialize would fail with
//! SQLITE_MISUSE, so this file is the only place the install
//! happens. Both tests below install + open via the same boot
//! ordering.

use std::sync::Once;

use sqlite_component_core::db::{Connection, StepResult, Value};

static INSTALL: Once = Once::new();

fn install_once() {
    INSTALL.call_once(|| {
        sqlite_pcache_tvm::install().expect("install pcache2 before initialize");
    });
}

#[test]
fn pcache2_serves_an_in_memory_workload() {
    install_once();

    let c = Connection::open_in_memory().expect("open in-memory db");

    // Schema + rows. Forces several xCreate/xFetch calls — sqlite
    // pages in schema + b-tree pages for the new table.
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

    let mut s = c
        .prepare("SELECT count(*) FROM numbers")
        .expect("prepare count");
    match s.step().expect("step count") {
        StepResult::Row => match s.column_value(0) {
            Value::Integer(n) => assert_eq!(n, 5, "expected 5 rows, got {n}"),
            other => panic!("count should be integer, got {other:?}"),
        },
        StepResult::Done => panic!("count query returned no row"),
    }
}

#[test]
fn pcache2_survives_shadow_overflow() {
    install_once();

    let c = Connection::open_in_memory().expect("open in-memory db");

    // Force a tiny cache (4 pages, half the SQLite minimum of 10
    // gets the same effect since SQLite enforces a floor — but
    // negative values are interpreted as 1024-byte units in
    // PRAGMA, so use a small positive page count and accept the
    // floor). Any value low enough to force eviction is fine; we
    // need pages > shadow capacity below to actually exercise
    // eviction.
    c.execute_batch("PRAGMA cache_size = 4;")
        .expect("set tiny cache_size");

    // Build a workload with enough pages to overflow a 4-page cache.
    // A few hundred rows in a single table won't fit in 4 pages
    // (each page is 4 KB by default → ~64 32-byte rows per page),
    // so seeded with 1000+ rows we get >15 pages of data + index +
    // schema.
    c.execute_batch(
        "CREATE TABLE big(id INTEGER PRIMARY KEY, payload TEXT);",
    )
    .expect("create big");
    for batch in 0..20 {
        let mut sql = String::from("INSERT INTO big(payload) VALUES ");
        for i in 0..50 {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("('row-{}-{}-padding-to-fill-space')", batch, i));
        }
        sql.push(';');
        c.execute_batch(&sql).expect("insert batch");
    }

    // Full scan — SQLite walks every b-tree page. With cache_size=4
    // this forces continual eviction + re-fetch. If our flush/
    // promote is buggy the rows will come back as zeros or stale
    // payload values.
    let mut s = c
        .prepare("SELECT count(*), sum(length(payload)) FROM big")
        .expect("prepare agg");
    match s.step().expect("step agg") {
        StepResult::Row => {
            let count = match s.column_value(0) {
                Value::Integer(n) => n,
                other => panic!("count should be integer, got {other:?}"),
            };
            let total_bytes = match s.column_value(1) {
                Value::Integer(n) => n,
                other => panic!("sum should be integer, got {other:?}"),
            };
            assert_eq!(count, 1000, "expected 1000 rows after 20*50 inserts, got {count}");
            // Each payload string starts with "row-" and ends with
            // "-padding-to-fill-space" — variable middle but
            // bounded length. Just assert non-zero, which is the
            // structural anti-corruption check (zeroed pages would
            // give length 0).
            assert!(
                total_bytes > 1000,
                "sum(length(payload)) should be huge after eviction round-trips; got {total_bytes}  pages came back zeroed?"
            );
        }
        StepResult::Done => panic!("count(*) returned no row"),
    }

    // Re-read specific rows to confirm round-trip via cold tier.
    // After the full scan above, most pages are in cold storage
    // (only 4 in shadow at any time). Fetching a specific row
    // forces a promote-from-cold for whichever page it lives on.
    let mut s = c
        .prepare("SELECT payload FROM big WHERE id = 1")
        .expect("prepare lookup");
    match s.step().expect("step lookup") {
        StepResult::Row => match s.column_value(0) {
            Value::Text(t) => assert!(
                t.starts_with("row-0-0"),
                "first row should start with 'row-0-0', got {t:?}  cold-tier promote returned wrong bytes?"
            ),
            other => panic!("payload should be text, got {other:?}"),
        },
        StepResult::Done => panic!("WHERE id=1 returned no row"),
    }
}
