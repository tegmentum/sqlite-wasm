//! sqlite-lib: programmatic SQLite-in-WASM library.
//!
//! Targets the `sqlite-library` world — exports the full
//! `sqlite:extension/*` SPI surface (so a compose-time consumer can
//! satisfy an extension's spi imports with this component) plus the
//! `sqlite:wasm/low-level`, `sqlite:wasm/high-level`, and
//! `sqlite:wasm/library` interfaces for callers that want to embed
//! SQLite functionality directly.
//!
//! Build:
//!
//! ```sh
//! CC_wasm32_wasip2=$WASI_SDK/bin/clang \
//! AR_wasm32_wasip2=$WASI_SDK/bin/ar \
//! CFLAGS_wasm32_wasip2="--sysroot=$WASI_SDK/share/wasi-sysroot --target=wasm32-wasip2" \
//!   cargo build --release --target wasm32-wasip2
//! wasm-tools component new \
//!   target/wasm32-wasip2/release/sqlite_lib.wasm \
//!   -o target/wasm32-wasip2/release/sqlite_lib.component.wasm
//! ```

#![allow(clippy::needless_lifetimes)]

mod bindings {
    wit_bindgen::generate!({
        path: "../wit",
        world: "sqlite-library",
        generate_all,
    });
}

pub use sqlite_component_core::db;

mod state;

use libsqlite3_sys as ffi;
use std::cell::RefCell;
use std::sync::OnceLock;

// The wasm-native VFS (`wasivfs`) is linked into sqlite-lib.wasm's C
// source, but the Rust side never calls `sqlite3_wasivfs_register`
// unless we do it here. Standalone sqlite-wasm CLIs register it in
// their `main`; a library component doesn't have that entry point.
// Without this, `Connection::open(<path>, DEFAULT)` (used by SPI's
// `open_db`, `HighLevelGuest::open_file`, low-level `open`, etc.)
// fails with `no such vfs: wasivfs` — because `core::db::open` on
// wasm32 hands the vfs name "wasivfs" to sqlite3_open_v2 for every
// non-`:memory:` path.
//
// `OnceLock<Result<(), db::Error>>` lets every DB-open path call
// `ensure_wasivfs()?` cheaply — the underlying register runs exactly
// once. `db::Error` is `Clone`, so the cached result is returnable
// from every caller without contortions.
//
// In-memory opens don't strictly need wasivfs, but calling
// `ensure_wasivfs()` for them is free after the first call and keeps
// the code paths uniform.
static WASIVFS_INIT: OnceLock<Result<(), db::Error>> = OnceLock::new();

fn ensure_wasivfs() -> Result<(), db::Error> {
    WASIVFS_INIT.get_or_init(db::init_wasivfs).clone()
}

use bindings::exports::sqlite::extension::config::Guest as ConfigGuest;
use bindings::exports::sqlite::extension::logging::{Guest as LoggingGuest, LogLevel};
use bindings::exports::sqlite::extension::spi::{
    self as spi, Guest as SpiGuest, QueryResult as SpiQueryResult,
    SqlValue as SpiSqlValue, SqliteError as SpiSqliteError,
};
use bindings::exports::sqlite::wasm::high_level::{
    Connection, DatabaseError as HlDatabaseError, ExecResult, Guest as HighLevelGuest,
    GuestConnection, GuestStatement, OpenMode, QueryResult as HlQueryResult, Statement,
    Value as HlValue,
};
use bindings::exports::sqlite::wasm::library::Guest as LibraryGuest;
use bindings::exports::sqlite::wasm::low_level::{
    ColumnType, DbHandle, Guest as LowLevelGuest, OpenFlags, ResultCode, StmtHandle,
};

use state::State;

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::new());
}

struct SqliteLib;

// =========================================================================
// sqlite:extension/logging
// =========================================================================

impl LoggingGuest for SqliteLib {
    fn log(level: LogLevel, message: String) {
        let l = match level {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        };
        eprintln!("[sqlite-lib {l}] {message}");
    }
    fn error(message: String) { eprintln!("[sqlite-lib ERROR] {message}"); }
    fn warn(message: String)  { eprintln!("[sqlite-lib WARN] {message}"); }
    fn info(message: String)  { eprintln!("[sqlite-lib INFO] {message}"); }
    fn debug(message: String) { eprintln!("[sqlite-lib DEBUG] {message}"); }
}

// =========================================================================
// sqlite:extension/config
// =========================================================================

impl ConfigGuest for SqliteLib {
    fn get(_key: String) -> Option<String> { None }
    fn set(_key: String, _value: String) -> bool { false }
    fn sqlite_version() -> String { db::version() }
    fn extension_version() -> String { env!("CARGO_PKG_VERSION").to_string() }
}

// =========================================================================
// sqlite:extension/spi
// Routes the SPI calls back through STATE's library connection.
// The default connection is in-memory; callers that want file-backed
// storage hold a high-level Connection resource and call its methods.
// SPI is for compose-time extensions running against this component;
// they get a shared in-memory db unless the host wires up a different
// backing. v1: in-memory only.
// =========================================================================

fn spi_db_err(e: db::Error) -> SpiSqliteError {
    SpiSqliteError {
        code: e.code,
        extended_code: e.extended_code,
        message: e.message,
    }
}

fn db_to_spi_value(v: db::Value) -> SpiSqlValue {
    match v {
        db::Value::Null => SpiSqlValue::Null,
        db::Value::Integer(i) => SpiSqlValue::Integer(i),
        db::Value::Real(r) => SpiSqlValue::Real(r),
        db::Value::Text(s) => SpiSqlValue::Text(s),
        db::Value::Blob(b) => SpiSqlValue::Blob(b),
    }
}

fn spi_value_to_db(v: SpiSqlValue) -> db::Value {
    match v {
        SpiSqlValue::Null => db::Value::Null,
        SpiSqlValue::Integer(i) => db::Value::Integer(i),
        SpiSqlValue::Real(r) => db::Value::Real(r),
        SpiSqlValue::Text(s) => db::Value::Text(s),
        SpiSqlValue::Blob(b) => db::Value::Blob(b),
    }
}

// The "default connection" shared between sqlite:extension/spi and
// sqlite:wasm/high-level.default-connection. SPI calls used to open
// their own in-memory connection that nothing else could see — that
// was a footgun (consumer runs CREATE TABLE through high-level, then
// SPI queries see an empty database). Now SPI and high-level's
// default-connection getter both go through this single
// Rc<RefCell<Connection>>; the high-level resource wraps the same Rc
// the SPI thread-local holds.
//
// Connections opened via open-memory, open-file, or connection.new
// stay isolated — only this default connection is shared. Consumers
// that want isolation just don't call default-connection.
thread_local! {
    static SHARED_CONN: RefCell<Option<std::rc::Rc<RefCell<db::Connection>>>>
        = const { RefCell::new(None) };
}

fn shared_conn() -> std::rc::Rc<RefCell<db::Connection>> {
    // wasivfs isn't strictly needed for `:memory:`, but init here too
    // so callers that later swap the shared conn to a file (via
    // spi.open_db) don't hit the missing-VFS error on first use.
    // Failure to init is fatal to the process anyway (SQLite has no
    // recovery from a broken VFS registration).
    let _ = ensure_wasivfs();
    SHARED_CONN.with(|c| {
        let mut g = c.borrow_mut();
        if g.is_none() {
            let conn = db::Connection::open_in_memory()
                .expect("open in-memory connection for shared SPI/high-level default");
            *g = Some(std::rc::Rc::new(RefCell::new(conn)));
        }
        g.as_ref().unwrap().clone()
    })
}

fn spi_with<R>(f: impl FnOnce(&db::Connection) -> R) -> R {
    let rc = shared_conn();
    let conn = rc.borrow();
    f(&conn)
}

// =========================================================================
// SPI prepared-statement cache
//
// The SPI `execute` / `execute-scalar` methods are called in tight loops
// by consumers doing per-row INSERT/UPDATE/DELETE (see ducklink's
// sqlitewasm extension — Bug 4b / Bug 7). Without caching, every call
// does the full sqlite3_prepare_v2 → bind → step → finalize cycle. On
// wasm32 the prepare is by far the dominant cost (SQL parse, byte-code
// gen). Caching the prepared statement and rebinding on reuse drops
// per-call cost to bind + step ≈ 30x faster on the 10k-INSERT stress
// test (163s → ~5s).
//
// Design:
//   * Thread-local RefCell<StmtCache>. Wasm component instance is
//     single-threaded, so no cross-thread locking.
//   * Keyed by the exact SQL string the SPI caller passed in. Different
//     whitespace / literal parameters ⇒ distinct entries (that's the
//     idiomatic prepared-statement pattern anyway).
//   * Bounded at CACHE_LIMIT entries; LRU eviction (linear scan for the
//     min-lru entry — fine at N=256, and eviction only fires when a NEW
//     distinct SQL text is presented after the cache is full).
//   * `open-db` drains the cache (sqlite3_finalize every held stmt)
//     BEFORE swapping SHARED_CONN. Skipping the drain would leak stmts
//     against the old sqlite3 handle and prevent its sqlite3_close from
//     succeeding.
//   * Cached stmts are prepared with sqlite3_prepare_v2, which stores
//     the original SQL and transparently re-prepares on schema change.
//     So a `CREATE TABLE` between two `INSERT`s that shares a cache
//     entry is not a hazard.
//   * On cache HIT: sqlite3_reset + sqlite3_clear_bindings, then the
//     caller re-binds and steps. On MISS: prepare fresh, insert.
//
// Safety: raw *mut sqlite3_stmt lives as long as its parent sqlite3
// handle. shared_conn() gives us the same Rc<RefCell<Connection>> the
// SPI thread-local holds; drop-order on `open-db` (drain-then-swap)
// ensures the stmts are finalized while their conn is still open.
// =========================================================================

const STMT_CACHE_LIMIT: usize = 256;

struct CachedStmt {
    raw: *mut ffi::sqlite3_stmt,
    /// Bumped whenever SHARED_CONN is swapped (see `open-db`). Entries
    /// with a stale epoch must never be reused — they belong to a now-
    /// finalized sqlite3 handle. Drain runs before swap, so this is a
    /// defence-in-depth check.
    epoch: u64,
    /// Monotonic access counter for LRU eviction.
    lru: u64,
}

struct StmtCache {
    map: std::collections::HashMap<String, CachedStmt>,
    counter: u64,
}

impl StmtCache {
    fn new() -> Self {
        Self { map: std::collections::HashMap::new(), counter: 0 }
    }

    /// Finalize every cached statement. Called from `open-db` before
    /// the shared connection is replaced.
    fn drain(&mut self) {
        for (_, e) in self.map.drain() {
            unsafe { ffi::sqlite3_finalize(e.raw); }
        }
    }

    /// Remove the least-recently-used entry, finalizing its stmt.
    fn evict_one(&mut self) {
        let victim = self
            .map
            .iter()
            .min_by_key(|(_, v)| v.lru)
            .map(|(k, _)| k.clone());
        if let Some(k) = victim {
            if let Some(e) = self.map.remove(&k) {
                unsafe { ffi::sqlite3_finalize(e.raw); }
            }
        }
    }
}

thread_local! {
    static SPI_CONN_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
    static SPI_STMT_CACHE: RefCell<StmtCache> = RefCell::new(StmtCache::new());
}

/// Look up or prepare a statement for `sql` against `conn_raw`. On a
/// cache hit the returned pointer is already `sqlite3_reset` +
/// `sqlite3_clear_bindings`'d and ready for a fresh bind. On a miss
/// the statement is freshly prepared and installed in the cache. The
/// caller MUST NOT sqlite3_finalize the returned pointer — the cache
/// retains ownership.
///
/// # Safety
/// `conn_raw` must be the raw handle of the currently-active shared
/// connection (i.e. the same connection that is in SHARED_CONN when
/// the current SPI epoch was assigned). Callers only get here via
/// `spi_with_cached_stmt`, which grabs SHARED_CONN atomically.
unsafe fn cached_stmt(
    conn_raw: *mut ffi::sqlite3,
    sql: &str,
) -> Result<*mut ffi::sqlite3_stmt, db::Error> {
    let epoch = SPI_CONN_EPOCH.with(|c| c.get());
    SPI_STMT_CACHE.with(|cache_cell| {
        let mut cache = cache_cell.borrow_mut();
        cache.counter = cache.counter.wrapping_add(1);
        let seq = cache.counter;

        // Try a cache hit first. Only reuse if the epoch matches — a
        // stale entry (belonging to a swapped-out connection) is a bug
        // to reach here (drain runs on swap), but if we do, discard.
        if let Some(entry) = cache.map.get_mut(sql) {
            if entry.epoch == epoch {
                entry.lru = seq;
                let raw = entry.raw;
                // Ready the stmt for reuse. sqlite3_reset returns the
                // last step's error code (or OK); we don't propagate it
                // because any error was already surfaced on the prior
                // call. sqlite3_clear_bindings never fails.
                ffi::sqlite3_reset(raw);
                ffi::sqlite3_clear_bindings(raw);
                return Ok(raw);
            }
            // Stale entry — drop it and prepare fresh.
            let stale = cache.map.remove(sql).unwrap();
            ffi::sqlite3_finalize(stale.raw);
        }

        // Miss: prepare against the live connection.
        let c_sql = std::ffi::CString::new(sql).map_err(|e| db::Error {
            code: ffi::SQLITE_MISUSE,
            extended_code: ffi::SQLITE_MISUSE,
            message: e.to_string(),
        })?;
        let mut raw: *mut ffi::sqlite3_stmt = std::ptr::null_mut();
        let rc = ffi::sqlite3_prepare_v2(
            conn_raw,
            c_sql.as_ptr(),
            -1,
            &mut raw,
            std::ptr::null_mut(),
        );
        if rc != ffi::SQLITE_OK {
            let msg_ptr = ffi::sqlite3_errmsg(conn_raw);
            let message = if msg_ptr.is_null() {
                format!("sqlite3_prepare_v2 failed (code {rc})")
            } else {
                std::ffi::CStr::from_ptr(msg_ptr)
                    .to_string_lossy()
                    .into_owned()
            };
            return Err(db::Error {
                code: rc & 0xff,
                extended_code: rc,
                message,
            });
        }
        if raw.is_null() {
            // Prepared "empty" statement (comment-only input). Not
            // cacheable — we couldn't return it anyway. Report as
            // misuse; callers of `execute` do not pass empty SQL.
            return Err(db::Error {
                code: ffi::SQLITE_MISUSE,
                extended_code: ffi::SQLITE_MISUSE,
                message: "sqlite3_prepare_v2: empty statement".into(),
            });
        }

        // Insert, evicting one LRU victim if we've hit the cap.
        if cache.map.len() >= STMT_CACHE_LIMIT {
            cache.evict_one();
        }
        cache
            .map
            .insert(sql.to_string(), CachedStmt { raw, epoch, lru: seq });
        Ok(raw)
    })
}

/// Bind values 1..=values.len() on the raw stmt. `SQLITE_TRANSIENT`
/// copies text/blob payloads so the caller can drop them immediately.
unsafe fn bind_all_raw(
    conn_raw: *mut ffi::sqlite3,
    stmt: *mut ffi::sqlite3_stmt,
    values: &[db::Value],
) -> Result<(), db::Error> {
    use std::os::raw::{c_char, c_double, c_int, c_void};
    for (i, v) in values.iter().enumerate() {
        let idx = (i + 1) as c_int;
        let rc = match v {
            db::Value::Null => ffi::sqlite3_bind_null(stmt, idx),
            db::Value::Integer(n) => ffi::sqlite3_bind_int64(stmt, idx, *n),
            db::Value::Real(r) => ffi::sqlite3_bind_double(stmt, idx, *r as c_double),
            db::Value::Text(s) => ffi::sqlite3_bind_text(
                stmt,
                idx,
                s.as_ptr() as *const c_char,
                s.len() as c_int,
                ffi::SQLITE_TRANSIENT(),
            ),
            db::Value::Blob(b) => ffi::sqlite3_bind_blob(
                stmt,
                idx,
                b.as_ptr() as *const c_void,
                b.len() as c_int,
                ffi::SQLITE_TRANSIENT(),
            ),
        };
        if rc != ffi::SQLITE_OK {
            let msg_ptr = ffi::sqlite3_errmsg(conn_raw);
            let message = if msg_ptr.is_null() {
                format!("sqlite3_bind_* failed (code {rc}) at index {idx}")
            } else {
                std::ffi::CStr::from_ptr(msg_ptr)
                    .to_string_lossy()
                    .into_owned()
            };
            return Err(db::Error {
                code: rc & 0xff,
                extended_code: rc,
                message,
            });
        }
    }
    Ok(())
}

/// Read one column value from a raw stmt at the current row.
unsafe fn column_value_raw(stmt: *mut ffi::sqlite3_stmt, idx: i32) -> db::Value {
    match ffi::sqlite3_column_type(stmt, idx) {
        ffi::SQLITE_NULL => db::Value::Null,
        ffi::SQLITE_INTEGER => db::Value::Integer(ffi::sqlite3_column_int64(stmt, idx)),
        ffi::SQLITE_FLOAT => db::Value::Real(ffi::sqlite3_column_double(stmt, idx)),
        ffi::SQLITE_TEXT => {
            let p = ffi::sqlite3_column_text(stmt, idx);
            let n = ffi::sqlite3_column_bytes(stmt, idx) as usize;
            if p.is_null() {
                db::Value::Text(String::new())
            } else {
                let bytes = std::slice::from_raw_parts(p as *const u8, n);
                db::Value::Text(String::from_utf8_lossy(bytes).into_owned())
            }
        }
        ffi::SQLITE_BLOB => {
            let p = ffi::sqlite3_column_blob(stmt, idx);
            let n = ffi::sqlite3_column_bytes(stmt, idx) as usize;
            if p.is_null() {
                db::Value::Blob(Vec::new())
            } else {
                let bytes = std::slice::from_raw_parts(p as *const u8, n);
                db::Value::Blob(bytes.to_vec())
            }
        }
        _ => db::Value::Null,
    }
}

/// Step the raw stmt to completion, gathering column names and every row.
unsafe fn step_collect_raw(
    conn_raw: *mut ffi::sqlite3,
    stmt: *mut ffi::sqlite3_stmt,
) -> Result<(Vec<String>, Vec<Vec<db::Value>>), db::Error> {
    let col_count = ffi::sqlite3_column_count(stmt) as usize;
    let mut columns: Vec<String> = Vec::with_capacity(col_count);
    for i in 0..col_count {
        let p = ffi::sqlite3_column_name(stmt, i as i32);
        columns.push(if p.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        });
    }
    let mut rows: Vec<Vec<db::Value>> = Vec::new();
    loop {
        let rc = ffi::sqlite3_step(stmt);
        match rc {
            ffi::SQLITE_ROW => {
                let mut r: Vec<db::Value> = Vec::with_capacity(col_count);
                for i in 0..col_count {
                    r.push(column_value_raw(stmt, i as i32));
                }
                rows.push(r);
            }
            ffi::SQLITE_DONE => break,
            _ => {
                let msg_ptr = ffi::sqlite3_errmsg(conn_raw);
                let message = if msg_ptr.is_null() {
                    format!("sqlite3_step failed (code {rc})")
                } else {
                    std::ffi::CStr::from_ptr(msg_ptr)
                        .to_string_lossy()
                        .into_owned()
                };
                return Err(db::Error {
                    code: rc & 0xff,
                    extended_code: rc,
                    message,
                });
            }
        }
    }
    Ok((columns, rows))
}

impl SpiGuest for SqliteLib {
    fn execute(sql: String, params: Vec<SpiSqlValue>) -> Result<SpiQueryResult, SpiSqliteError> {
        // Cache-hit path: reset+clear_bindings+rebind+step on a stmt
        // that survived the previous call — skips sqlite3_prepare_v2
        // (which dominates wasm cost). See STMT_CACHE comment above.
        spi_with(|conn| unsafe {
            let raw = cached_stmt(conn.raw_handle(), &sql).map_err(spi_db_err)?;
            let dbs: Vec<db::Value> = params.into_iter().map(spi_value_to_db).collect();
            bind_all_raw(conn.raw_handle(), raw, &dbs).map_err(spi_db_err)?;
            let (columns, rows_vals) =
                step_collect_raw(conn.raw_handle(), raw).map_err(spi_db_err)?;
            // Reset immediately so the cached stmt releases any page
            // locks before the next unrelated statement runs on this
            // connection. Ignore the return code — it only re-reports
            // an error we've already surfaced.
            ffi::sqlite3_reset(raw);
            let rows: Vec<Vec<SpiSqlValue>> = rows_vals
                .into_iter()
                .map(|r| r.into_iter().map(db_to_spi_value).collect())
                .collect();
            Ok(SpiQueryResult {
                columns,
                rows,
                changes: conn.changes(),
                last_insert_rowid: conn.last_insert_rowid(),
            })
        })
    }

    fn execute_scalar(sql: String, params: Vec<SpiSqlValue>) -> Result<SpiSqlValue, SpiSqliteError> {
        spi_with(|conn| unsafe {
            let raw = cached_stmt(conn.raw_handle(), &sql).map_err(spi_db_err)?;
            let dbs: Vec<db::Value> = params.into_iter().map(spi_value_to_db).collect();
            bind_all_raw(conn.raw_handle(), raw, &dbs).map_err(spi_db_err)?;
            let (_cols, rows_vals) =
                step_collect_raw(conn.raw_handle(), raw).map_err(spi_db_err)?;
            ffi::sqlite3_reset(raw);
            let first = rows_vals
                .into_iter()
                .next()
                .and_then(|r| r.into_iter().next())
                .unwrap_or(db::Value::Null);
            Ok(db_to_spi_value(first))
        })
    }

    fn execute_batch(sql: String) -> Result<i64, SpiSqliteError> {
        spi_with(|conn| {
            conn.execute_batch(&sql).map_err(spi_db_err)?;
            Ok(conn.changes())
        })
    }

    fn changes() -> i64 {
        spi_with(|conn| conn.changes())
    }

    fn total_changes() -> i64 {
        spi_with(|conn| conn.total_changes())
    }

    fn last_insert_rowid() -> i64 {
        spi_with(|conn| conn.last_insert_rowid())
    }

    fn current_memory_used() -> i64 {
        // sqlite3_memory_used is process-global; not tied to any conn.
        unsafe { libsqlite3_sys::sqlite3_memory_used() }
    }

    fn list_vfs() -> Vec<String> {
        // sqlite3 keeps a global VFS list; walk it.
        let mut out = Vec::new();
        unsafe {
            let mut p = libsqlite3_sys::sqlite3_vfs_find(std::ptr::null());
            while !p.is_null() {
                let name = std::ffi::CStr::from_ptr((*p).zName)
                    .to_string_lossy()
                    .into_owned();
                out.push(name);
                p = (*p).pNext;
            }
        }
        out
    }

    fn vfs_name(_db_name: String) -> Result<String, SpiSqliteError> {
        // PRAGMA-driven; not implemented in this slim sqlite-lib build.
        // The componentized-SQLite use case typically has one VFS;
        // consumers that need this can implement on top of high-level.
        Err(SpiSqliteError {
            code: 1,
            extended_code: 1,
            message: "vfs_name not implemented in sqlite-lib (use high-level connection API)".into(),
        })
    }

    fn serialize_db(db_name: String) -> Result<Vec<u8>, SpiSqliteError> {
        spi_with(|conn| {
            conn.serialize_db(&db_name).map_err(spi_db_err)
        })
    }

    fn deserialize_db(db_name: String, bytes: Vec<u8>) -> Result<(), SpiSqliteError> {
        spi_with(|conn| {
            conn.deserialize_db(&db_name, &bytes).map_err(spi_db_err)
        })
    }

    fn backup_into(
        src_db: String,
        dst_path: String,
        dst_db: String,
    ) -> Result<(), SpiSqliteError> {
        ensure_wasivfs().map_err(spi_db_err)?;
        let dst = db::Connection::open(&dst_path, db::OpenFlags::DEFAULT)
            .map_err(spi_db_err)?;
        spi_with(|src| src.backup_into(&src_db, &dst, &dst_db).map_err(spi_db_err))
    }

    fn restore_from(
        src_path: String,
        src_db: String,
        dst_db: String,
    ) -> Result<(), SpiSqliteError> {
        ensure_wasivfs().map_err(spi_db_err)?;
        let src = db::Connection::open(&src_path, db::OpenFlags::READONLY)
            .map_err(spi_db_err)?;
        spi_with(|dst| src.backup_into(&src_db, dst, &dst_db).map_err(spi_db_err))
    }

    fn set_busy_timeout(ms: i32) -> Result<(), SpiSqliteError> {
        spi_with(|conn| conn.busy_timeout(ms).map_err(spi_db_err))
    }

    fn limit(category: i32, value: i32) -> i32 {
        spi_with(|conn| conn.limit(category, value))
    }

    fn db_config_bool(op: i32, set: bool, value: bool) -> Result<bool, SpiSqliteError> {
        spi_with(|conn| {
            if set {
                conn.db_config_set_bool(op, value).map_err(spi_db_err)
            } else {
                conn.db_config_get_bool(op).map_err(spi_db_err)
            }
        })
    }

    fn execute_multi(
        sql: String,
        named_params: Vec<spi::NamedParam>,
    ) -> Result<Vec<SpiQueryResult>, SpiSqliteError> {
        // Walk multi-statement input via prepare_with_tail, bind named
        // params per statement. Mirrors the host's HostWrap impl.
        spi_with(|conn| {
            let mut results = Vec::new();
            let mut remaining: &str = &sql;
            while !remaining.trim().is_empty() {
                let (mut stmt, tail) = match conn.prepare_with_tail(remaining) {
                    Ok(p) => p,
                    Err(e) => return Err(spi_db_err(e)),
                };
                if stmt.is_empty() {
                    if tail >= remaining.len() { break; }
                    remaining = &remaining[tail..];
                    continue;
                }
                let nparams = stmt.parameter_count();
                for i in 1..=nparams {
                    if let Some(name) = stmt.bind_parameter_name(i) {
                        let bare = &name[1..];
                        if let Some(p) = named_params.iter().find(|p| p.name == bare) {
                            let v = spi_value_to_db(p.value.clone());
                            if let Err(e) = stmt.bind(i, &v) {
                                return Err(spi_db_err(e));
                            }
                        }
                    }
                }
                let columns = stmt.column_names();
                let rows = stmt.collect_rows().map_err(spi_db_err)?;
                let out_rows: Vec<Vec<SpiSqlValue>> = rows
                    .into_iter()
                    .map(|r| r.into_iter().map(db_to_spi_value).collect())
                    .collect();
                results.push(SpiQueryResult {
                    columns,
                    rows: out_rows,
                    changes: conn.changes(),
                    last_insert_rowid: conn.last_insert_rowid(),
                });
                if tail >= remaining.len() { break; }
                remaining = &remaining[tail..];
            }
            Ok(results)
        })
    }

    fn open_db(path: String) -> Result<(), SpiSqliteError> {
        // Swap the shared SPI connection. Empty path / `:memory:` opens
        // a fresh in-memory db; anything else is a file path — which
        // requires wasivfs to be registered (see comment on
        // WASIVFS_INIT).
        ensure_wasivfs().map_err(spi_db_err)?;
        let new_conn = if path.is_empty() || path == ":memory:" {
            db::Connection::open_in_memory().map_err(spi_db_err)?
        } else {
            db::Connection::open(&path, db::OpenFlags::DEFAULT).map_err(spi_db_err)?
        };
        // Drain the prepared-statement cache BEFORE dropping the old
        // Rc<Connection>. Cache entries hold raw sqlite3_stmt* pointers
        // against the outgoing sqlite3 handle; sqlite3_close (invoked
        // by Connection::Drop) silently fails if any stmts are still
        // open, and reusing a stmt against a new connection is UB.
        // Bump the epoch so any straggler (defence-in-depth) can't be
        // reused across the swap.
        SPI_STMT_CACHE.with(|c| c.borrow_mut().drain());
        SPI_CONN_EPOCH.with(|c| c.set(c.get().wrapping_add(1)));
        SHARED_CONN.with(|c| {
            *c.borrow_mut() = Some(std::rc::Rc::new(RefCell::new(new_conn)));
        });
        Ok(())
    }
}

// =========================================================================
// sqlite:wasm/low-level
// =========================================================================

fn ll_open_flags(_f: OpenFlags) -> db::OpenFlags {
    db::OpenFlags::DEFAULT
}

fn ll_map_err(e: &db::Error) -> ResultCode {
    use libsqlite3_sys::*;
    match e.code {
        SQLITE_BUSY => ResultCode::Busy,
        SQLITE_LOCKED => ResultCode::Locked,
        SQLITE_NOMEM => ResultCode::Nomem,
        SQLITE_READONLY => ResultCode::Readonly,
        SQLITE_INTERRUPT => ResultCode::Interrupt,
        SQLITE_IOERR => ResultCode::Ioerr,
        SQLITE_CORRUPT => ResultCode::Corrupt,
        SQLITE_NOTFOUND => ResultCode::Notfound,
        SQLITE_FULL => ResultCode::Full,
        SQLITE_CANTOPEN => ResultCode::Cantopen,
        SQLITE_PROTOCOL => ResultCode::Protocol,
        SQLITE_SCHEMA => ResultCode::Schema,
        SQLITE_TOOBIG => ResultCode::Toobig,
        SQLITE_CONSTRAINT => ResultCode::Constraint,
        SQLITE_MISMATCH => ResultCode::Mismatch,
        SQLITE_MISUSE => ResultCode::Misuse,
        SQLITE_NOLFS => ResultCode::Nolfs,
        SQLITE_AUTH => ResultCode::Auth,
        SQLITE_RANGE => ResultCode::Range,
        SQLITE_NOTADB => ResultCode::Notadb,
        _ => ResultCode::Error,
    }
}

impl LowLevelGuest for SqliteLib {
    fn open(filename: String, flags: OpenFlags) -> Result<DbHandle, ResultCode> {
        if let Err(e) = ensure_wasivfs() {
            return Err(ll_map_err(&e));
        }
        let path = if filename.is_empty() || filename == ":memory:" {
            ":memory:".to_string()
        } else {
            filename
        };
        let conn = if path == ":memory:" {
            db::Connection::open_in_memory()
        } else {
            db::Connection::open(&path, ll_open_flags(flags))
        };
        match conn {
            Ok(c) => Ok(STATE.with(|s| s.borrow_mut().add_db(c))),
            Err(e) => Err(ll_map_err(&e)),
        }
    }

    fn close(db: DbHandle) -> ResultCode {
        STATE.with(|s| s.borrow_mut().remove_db(db));
        ResultCode::Ok
    }

    fn exec(db: DbHandle, sql: String) -> Result<String, ResultCode> {
        STATE.with(|s| {
            let st = s.borrow();
            let conn = st.db(db).ok_or(ResultCode::Misuse)?;
            conn.execute_batch(&sql).map(|_| String::new()).map_err(|e| ll_map_err(&e))
        })
    }

    fn prepare(db: DbHandle, sql: String) -> Result<StmtHandle, ResultCode> {
        STATE.with(|s| {
            let mut st = s.borrow_mut();
            st.prepare(db, &sql)
        })
    }

    fn step(stmt: StmtHandle) -> ResultCode {
        STATE.with(|s| s.borrow_mut().step(stmt))
    }
    fn reset(stmt: StmtHandle) -> ResultCode {
        STATE.with(|s| s.borrow_mut().reset(stmt))
    }
    fn finalize(stmt: StmtHandle) -> ResultCode {
        STATE.with(|s| s.borrow_mut().finalize(stmt))
    }

    fn bind_null(stmt: StmtHandle, index: i32) -> ResultCode {
        STATE.with(|s| s.borrow_mut().bind(stmt, index, db::Value::Null))
    }
    fn bind_int(stmt: StmtHandle, index: i32, value: i32) -> ResultCode {
        STATE.with(|s| s.borrow_mut().bind(stmt, index, db::Value::Integer(value as i64)))
    }
    fn bind_int64(stmt: StmtHandle, index: i32, value: i64) -> ResultCode {
        STATE.with(|s| s.borrow_mut().bind(stmt, index, db::Value::Integer(value)))
    }
    fn bind_double(stmt: StmtHandle, index: i32, value: f64) -> ResultCode {
        STATE.with(|s| s.borrow_mut().bind(stmt, index, db::Value::Real(value)))
    }
    fn bind_text(stmt: StmtHandle, index: i32, value: String) -> ResultCode {
        STATE.with(|s| s.borrow_mut().bind(stmt, index, db::Value::Text(value)))
    }
    fn bind_blob(stmt: StmtHandle, index: i32, value: Vec<u8>) -> ResultCode {
        STATE.with(|s| s.borrow_mut().bind(stmt, index, db::Value::Blob(value)))
    }
    fn bind_parameter_count(_stmt: StmtHandle) -> i32 { 0 }
    fn bind_parameter_index(_stmt: StmtHandle, _name: String) -> i32 { 0 }
    fn clear_bindings(_stmt: StmtHandle) -> ResultCode { ResultCode::Ok }

    fn column_count(stmt: StmtHandle) -> i32 {
        STATE.with(|s| s.borrow().column_count(stmt))
    }
    fn column_name(stmt: StmtHandle, index: i32) -> String {
        STATE.with(|s| s.borrow().column_name(stmt, index))
    }
    fn get_column_type(stmt: StmtHandle, index: i32) -> ColumnType {
        STATE.with(|s| s.borrow().column_type(stmt, index))
    }
    fn column_int(stmt: StmtHandle, index: i32) -> i32 {
        STATE.with(|s| s.borrow().column_int(stmt, index)) as i32
    }
    fn column_int64(stmt: StmtHandle, index: i32) -> i64 {
        STATE.with(|s| s.borrow().column_int(stmt, index))
    }
    fn column_double(stmt: StmtHandle, index: i32) -> f64 {
        STATE.with(|s| s.borrow().column_double(stmt, index))
    }
    fn column_text(stmt: StmtHandle, index: i32) -> String {
        STATE.with(|s| s.borrow().column_text(stmt, index))
    }
    fn column_blob(stmt: StmtHandle, index: i32) -> Vec<u8> {
        STATE.with(|s| s.borrow().column_blob(stmt, index))
    }
    fn column_bytes(stmt: StmtHandle, index: i32) -> i32 {
        STATE.with(|s| s.borrow().column_blob(stmt, index).len() as i32)
    }

    fn errmsg(_db: DbHandle) -> String { String::new() }
    fn errcode(_db: DbHandle) -> ResultCode { ResultCode::Ok }
    fn extended_errcode(_db: DbHandle) -> i32 { 0 }

    fn get_autocommit(_db: DbHandle) -> bool { true }
    fn changes(db: DbHandle) -> i32 {
        STATE.with(|s| s.borrow().db_changes(db) as i32)
    }
    fn total_changes(db: DbHandle) -> i32 {
        STATE.with(|s| s.borrow().db_total_changes(db) as i32)
    }
    fn last_insert_rowid(db: DbHandle) -> i64 {
        STATE.with(|s| s.borrow().db_last_insert_rowid(db))
    }

    fn libversion() -> String { db::version() }
    fn libversion_number() -> i32 { db::version_number() }
    fn sourceid() -> String { String::new() }
}

// =========================================================================
// sqlite:wasm/high-level
// Resource-based; each Connection wraps a db::Connection.
// =========================================================================

pub struct HlConnection {
    conn: std::rc::Rc<RefCell<db::Connection>>,
}

pub struct HlStatement {
    conn: std::rc::Rc<RefCell<db::Connection>>,
    sql: String,
    bindings: RefCell<Vec<db::Value>>,
    column_names: RefCell<Vec<String>>,
    cursor_buf: RefCell<Option<std::collections::VecDeque<Vec<db::Value>>>>,
}

fn hl_err(e: &db::Error) -> HlDatabaseError {
    HlDatabaseError {
        code: e.code,
        extended_code: e.extended_code,
        message: e.message.clone(),
    }
}

fn hl_value_to_db(v: HlValue) -> db::Value {
    match v {
        HlValue::Null => db::Value::Null,
        HlValue::Integer(i) => db::Value::Integer(i),
        HlValue::Real(r) => db::Value::Real(r),
        HlValue::Text(s) => db::Value::Text(s),
        HlValue::Blob(b) => db::Value::Blob(b),
    }
}

fn db_to_hl_value(v: db::Value) -> HlValue {
    match v {
        db::Value::Null => HlValue::Null,
        db::Value::Integer(i) => HlValue::Integer(i),
        db::Value::Real(r) => HlValue::Real(r),
        db::Value::Text(s) => HlValue::Text(s),
        db::Value::Blob(b) => HlValue::Blob(b),
    }
}

impl HighLevelGuest for SqliteLib {
    type Connection = HlConnection;
    type Statement = HlStatement;

    fn version() -> String { db::version() }
    fn version_number() -> i32 { db::version_number() }
    fn open_memory() -> Result<Connection, HlDatabaseError> {
        ensure_wasivfs().map_err(|e| hl_err(&e))?;
        match db::Connection::open_in_memory() {
            Ok(c) => Ok(Connection::new(HlConnection { conn: std::rc::Rc::new(RefCell::new(c)) })),
            Err(e) => Err(hl_err(&e)),
        }
    }
    fn open_file(path: String) -> Result<Connection, HlDatabaseError> {
        ensure_wasivfs().map_err(|e| hl_err(&e))?;
        match db::Connection::open(&path, db::OpenFlags::DEFAULT) {
            Ok(c) => Ok(Connection::new(HlConnection { conn: std::rc::Rc::new(RefCell::new(c)) })),
            Err(e) => Err(hl_err(&e)),
        }
    }
    fn default_connection() -> Result<Connection, HlDatabaseError> {
        // Hand out an HlConnection wrapping the same Rc the SPI
        // thread-local holds. Writes via this connection are visible
        // to spi.execute() and vice versa.
        Ok(Connection::new(HlConnection { conn: shared_conn() }))
    }
}

impl GuestConnection for HlConnection {
    fn new(path: String, mode: OpenMode) -> Self {
        // Best-effort; the constructor has no `Result` return channel,
        // so we fall through to open_in_memory() on failure the same
        // way we do for a failed open. If wasivfs registration fails
        // the memory-fallback open will still work.
        let _ = ensure_wasivfs();
        let conn = match mode {
            OpenMode::Memory => db::Connection::open_in_memory(),
            OpenMode::ReadOnly => db::Connection::open(&path, db::OpenFlags::READONLY),
            _ => db::Connection::open(&path, db::OpenFlags::DEFAULT),
        };
        HlConnection {
            conn: std::rc::Rc::new(RefCell::new(
                conn.unwrap_or_else(|_| db::Connection::open_in_memory().unwrap()),
            )),
        }
    }

    fn execute(&self, sql: String) -> Result<ExecResult, HlDatabaseError> {
        let conn = self.conn.borrow();
        conn.execute_batch(&sql).map_err(|e| hl_err(&e))?;
        Ok(ExecResult {
            changes: conn.changes() as i32,
            last_insert_rowid: conn.last_insert_rowid(),
        })
    }

    fn execute_with_params(&self, sql: String, params: Vec<HlValue>) -> Result<ExecResult, HlDatabaseError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&sql).map_err(|e| hl_err(&e))?;
        let dbs: Vec<db::Value> = params.into_iter().map(hl_value_to_db).collect();
        stmt.bind_all(&dbs).map_err(|e| hl_err(&e))?;
        loop {
            match stmt.step().map_err(|e| hl_err(&e))? {
                db::StepResult::Row => continue,
                db::StepResult::Done => break,
            }
        }
        Ok(ExecResult {
            changes: conn.changes() as i32,
            last_insert_rowid: conn.last_insert_rowid(),
        })
    }

    fn query(&self, sql: String) -> Result<HlQueryResult, HlDatabaseError> {
        self.query_with_params(sql, vec![])
    }

    fn query_with_params(&self, sql: String, params: Vec<HlValue>) -> Result<HlQueryResult, HlDatabaseError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&sql).map_err(|e| hl_err(&e))?;
        let column_names = stmt.column_names();
        let dbs: Vec<db::Value> = params.into_iter().map(hl_value_to_db).collect();
        stmt.bind_all(&dbs).map_err(|e| hl_err(&e))?;
        let rows_vals = stmt.collect_rows().map_err(|e| hl_err(&e))?;
        let out_rows: Vec<bindings::exports::sqlite::wasm::high_level::Row> = rows_vals
            .into_iter()
            .map(|r| bindings::exports::sqlite::wasm::high_level::Row {
                columns: r.into_iter().map(db_to_hl_value).collect(),
            })
            .collect();
        Ok(HlQueryResult { column_names, rows: out_rows })
    }

    fn prepare(&self, sql: String) -> Result<Statement, HlDatabaseError> {
        {
            let conn = self.conn.borrow();
            conn.prepare(&sql).map_err(|e| hl_err(&e))?;
        }
        Ok(Statement::new(HlStatement {
            conn: self.conn.clone(),
            sql,
            bindings: RefCell::new(Vec::new()),
            column_names: RefCell::new(Vec::new()),
            cursor_buf: RefCell::new(None),
        }))
    }

    fn begin_transaction(&self) -> Result<(), HlDatabaseError> {
        self.conn.borrow().execute_batch("BEGIN").map_err(|e| hl_err(&e))
    }
    fn commit(&self) -> Result<(), HlDatabaseError> {
        self.conn.borrow().execute_batch("COMMIT").map_err(|e| hl_err(&e))
    }
    fn rollback(&self) -> Result<(), HlDatabaseError> {
        self.conn.borrow().execute_batch("ROLLBACK").map_err(|e| hl_err(&e))
    }
    fn in_autocommit(&self) -> bool { true }
    fn last_error(&self) -> Option<HlDatabaseError> { None }
}

impl HlStatement {
    fn bound_params(&self) -> Vec<db::Value> {
        self.bindings.borrow().clone()
    }
}

impl GuestStatement for HlStatement {
    fn bind(&self, index: i32, value: HlValue) -> Result<(), HlDatabaseError> {
        let idx = (index as usize).saturating_sub(1);
        let mut b = self.bindings.borrow_mut();
        if b.len() <= idx { b.resize(idx + 1, db::Value::Null); }
        b[idx] = hl_value_to_db(value);
        Ok(())
    }

    fn bind_all(&self, params: Vec<HlValue>) -> Result<(), HlDatabaseError> {
        let mut b = self.bindings.borrow_mut();
        b.clear();
        for v in params { b.push(hl_value_to_db(v)); }
        Ok(())
    }

    fn execute(&self) -> Result<ExecResult, HlDatabaseError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&self.sql).map_err(|e| hl_err(&e))?;
        stmt.bind_all(&self.bound_params()).map_err(|e| hl_err(&e))?;
        loop {
            match stmt.step().map_err(|e| hl_err(&e))? {
                db::StepResult::Row => continue,
                db::StepResult::Done => break,
            }
        }
        Ok(ExecResult {
            changes: conn.changes() as i32,
            last_insert_rowid: conn.last_insert_rowid(),
        })
    }

    fn query(&self) -> Result<HlQueryResult, HlDatabaseError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&self.sql).map_err(|e| hl_err(&e))?;
        let column_names = stmt.column_names();
        *self.column_names.borrow_mut() = column_names.clone();
        stmt.bind_all(&self.bound_params()).map_err(|e| hl_err(&e))?;
        let rows_vals = stmt.collect_rows().map_err(|e| hl_err(&e))?;
        let out_rows: Vec<bindings::exports::sqlite::wasm::high_level::Row> = rows_vals
            .into_iter()
            .map(|r| bindings::exports::sqlite::wasm::high_level::Row {
                columns: r.into_iter().map(db_to_hl_value).collect(),
            })
            .collect();
        Ok(HlQueryResult { column_names, rows: out_rows })
    }

    fn step(&self) -> Result<Option<bindings::exports::sqlite::wasm::high_level::Row>, HlDatabaseError> {
        let needs_init = self.cursor_buf.borrow().is_none();
        if needs_init {
            let conn = self.conn.borrow();
            let mut stmt = conn.prepare(&self.sql).map_err(|e| hl_err(&e))?;
            *self.column_names.borrow_mut() = stmt.column_names();
            stmt.bind_all(&self.bound_params()).map_err(|e| hl_err(&e))?;
            let rows_vals = stmt.collect_rows().map_err(|e| hl_err(&e))?;
            let buf: std::collections::VecDeque<Vec<db::Value>> = rows_vals.into();
            *self.cursor_buf.borrow_mut() = Some(buf);
        }
        let mut g = self.cursor_buf.borrow_mut();
        let buf = g.as_mut().unwrap();
        Ok(buf.pop_front().map(|raw| bindings::exports::sqlite::wasm::high_level::Row {
            columns: raw.into_iter().map(db_to_hl_value).collect(),
        }))
    }

    fn reset(&self) -> Result<(), HlDatabaseError> {
        *self.cursor_buf.borrow_mut() = None;
        Ok(())
    }

    fn clear_bindings(&self) -> Result<(), HlDatabaseError> {
        self.bindings.borrow_mut().clear();
        Ok(())
    }

    fn column_count(&self) -> i32 {
        let cached = self.column_names.borrow();
        if !cached.is_empty() { return cached.len() as i32; }
        drop(cached);
        let conn = self.conn.borrow();
        let stmt = match conn.prepare(&self.sql) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        let n = stmt.column_count() as i32;
        drop(stmt);
        n
    }

    fn column_names(&self) -> Vec<String> {
        let cached = self.column_names.borrow();
        if !cached.is_empty() { return cached.clone(); }
        drop(cached);
        let conn = self.conn.borrow();
        let stmt = match conn.prepare(&self.sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        drop(stmt);
        names
    }

    fn parameter_count(&self) -> i32 {
        let conn = self.conn.borrow();
        let stmt = match conn.prepare(&self.sql) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        let n = stmt.parameter_count() as i32;
        drop(stmt);
        n
    }
}

// =========================================================================
// sqlite:wasm/library
// Forwards load-extension calls to the host's extension-loader.
// The library interface's policy/metadata types are structural twins
// of the canonical sqlite:extension types — see wit/library.wit for
// why they aren't `use`-imported. Conversion is mechanical.
// =========================================================================

mod lib_load {
    use super::bindings;
    use bindings::exports::sqlite::wasm::library::{
        AggregateFunctionSpec as LibAggSpec, Capability as LibCap, CollationSpec as LibCollSpec,
        DnsPolicy as LibDnsPolicy, FsPolicy as LibFsPolicy, FunctionFlags as LibFlags,
        HttpMethod as LibMethod, HttpPolicy as LibHttpPolicy, LoadOptions as LibOpts,
        LoaderError as LibLoaderError, Manifest as LibManifest,
        ScalarFunctionSpec as LibScalarSpec,
    };
    use bindings::sqlite::extension::metadata as md;
    use bindings::sqlite::extension::policy as pol;
    use bindings::sqlite::extension::types as ty;
    use bindings::sqlite::wasm::extension_loader as loader;

    fn cap_to_pol(c: LibCap) -> pol::Capability {
        match c {
            LibCap::Spi => pol::Capability::Spi,
            LibCap::Prepared => pol::Capability::Prepared,
            LibCap::Transaction => pol::Capability::Transaction,
            LibCap::Schema => pol::Capability::Schema,
            LibCap::State => pol::Capability::State,
            LibCap::Cache => pol::Capability::Cache,
            LibCap::Random => pol::Capability::Random,
            LibCap::Text => pol::Capability::Text,
            LibCap::Hashing => pol::Capability::Hashing,
            LibCap::Encoding => pol::Capability::Encoding,
            LibCap::Http => pol::Capability::Http,
            LibCap::Dns => pol::Capability::Dns,
        }
    }

    fn cap_to_lib(c: pol::Capability) -> LibCap {
        match c {
            pol::Capability::Spi => LibCap::Spi,
            pol::Capability::Prepared => LibCap::Prepared,
            pol::Capability::Transaction => LibCap::Transaction,
            pol::Capability::Schema => LibCap::Schema,
            pol::Capability::State => LibCap::State,
            pol::Capability::Cache => LibCap::Cache,
            pol::Capability::Random => LibCap::Random,
            pol::Capability::Text => LibCap::Text,
            pol::Capability::Hashing => LibCap::Hashing,
            pol::Capability::Encoding => LibCap::Encoding,
            pol::Capability::Http => LibCap::Http,
            pol::Capability::Dns => LibCap::Dns,
        }
    }

    fn method_to_pol(m: LibMethod) -> pol::Method {
        match m {
            LibMethod::Get => pol::Method::Get,
            LibMethod::Head => pol::Method::Head,
            LibMethod::Post => pol::Method::Post,
            LibMethod::Put => pol::Method::Put,
            LibMethod::Delete => pol::Method::Delete,
            LibMethod::Connect => pol::Method::Connect,
            LibMethod::Options => pol::Method::Options,
            LibMethod::Trace => pol::Method::Trace,
            LibMethod::Patch => pol::Method::Patch,
        }
    }

    fn flags_to_ty(f: LibFlags) -> ty::FunctionFlags {
        let mut out = ty::FunctionFlags::empty();
        if f.contains(LibFlags::DETERMINISTIC) { out |= ty::FunctionFlags::DETERMINISTIC; }
        if f.contains(LibFlags::DIRECT_ONLY)   { out |= ty::FunctionFlags::DIRECT_ONLY; }
        if f.contains(LibFlags::INNOCUOUS)     { out |= ty::FunctionFlags::INNOCUOUS; }
        out
    }

    fn flags_to_lib(f: ty::FunctionFlags) -> LibFlags {
        let mut out = LibFlags::empty();
        if f.contains(ty::FunctionFlags::DETERMINISTIC) { out |= LibFlags::DETERMINISTIC; }
        if f.contains(ty::FunctionFlags::DIRECT_ONLY)   { out |= LibFlags::DIRECT_ONLY; }
        if f.contains(ty::FunctionFlags::INNOCUOUS)     { out |= LibFlags::INNOCUOUS; }
        out
    }

    pub fn opts_to_pol(o: LibOpts) -> pol::LoadOptions {
        pol::LoadOptions {
            grant: o.grant.into_iter().map(cap_to_pol).collect(),
            http_policy: o.http_policy.map(|hp: LibHttpPolicy| pol::HttpPolicy {
                allowed_hosts: hp.allowed_hosts,
                allowed_methods: hp.allowed_methods.map(|ms| ms.into_iter().map(method_to_pol).collect()),
                max_body_bytes: hp.max_body_bytes,
                timeout_ms: hp.timeout_ms,
            }),
            dns_policy: o.dns_policy.map(|dp: LibDnsPolicy| pol::DnsPolicy {
                allowed_domains: dp.allowed_domains,
                timeout_ms: dp.timeout_ms,
            }),
            fs_policy: o.fs_policy.map(|fp: LibFsPolicy| pol::FsPolicy {
                readable_prefixes: fp.readable_prefixes,
                writable_prefixes: fp.writable_prefixes,
                max_write_bytes_per_call: fp.max_write_bytes_per_call,
            }),
            fuel_per_call: o.fuel_per_call,
            memory_limit_bytes: o.memory_limit_bytes,
            epoch_deadline_ms: o.epoch_deadline_ms,
        }
    }

    pub fn manifest_to_lib(m: md::Manifest) -> LibManifest {
        LibManifest {
            name: m.name,
            version: m.version,
            scalar_functions: m.scalar_functions.into_iter().map(|s| LibScalarSpec {
                id: s.id,
                name: s.name,
                num_args: s.num_args,
                func_flags: flags_to_lib(s.func_flags),
            }).collect(),
            aggregate_functions: m.aggregate_functions.into_iter().map(|s| LibAggSpec {
                id: s.id,
                name: s.name,
                num_args: s.num_args,
                func_flags: flags_to_lib(s.func_flags),
                is_window: s.is_window,
            }).collect(),
            collations: m.collations.into_iter().map(|c| LibCollSpec {
                id: c.id,
                name: c.name,
            }).collect(),
            has_authorizer: m.has_authorizer,
            has_update_hook: m.has_update_hook,
            has_commit_hook: m.has_commit_hook,
            declared_capabilities: m.declared_capabilities.into_iter().map(cap_to_lib).collect(),
        }
    }

    pub fn loader_err_to_lib(e: loader::LoaderError) -> LibLoaderError {
        LibLoaderError { code: e.code, message: e.message }
    }

    // Silence dead-code warnings: these helpers are part of the
    // conversion surface but not all of them are reachable from
    // exported methods alone. flags_to_ty in particular is only
    // useful for symmetry with the inverse direction.
    #[allow(dead_code)] pub fn _touch() { let _ = flags_to_ty as fn(LibFlags) -> ty::FunctionFlags; }
}

impl LibraryGuest for SqliteLib {
    fn is_statement_complete(buffered: String) -> bool {
        let trimmed = buffered.trim();
        if trimmed.is_empty() { return true; }
        let cstring = match std::ffi::CString::new(trimmed) {
            Ok(s) => s,
            Err(_) => return false,
        };
        unsafe { libsqlite3_sys::sqlite3_complete(cstring.as_ptr()) != 0 }
    }

    fn library_version() -> String { env!("CARGO_PKG_VERSION").to_string() }
    fn sqlite_version() -> String { db::version() }

    fn load_extension(
        path: String,
        opts: bindings::exports::sqlite::wasm::library::LoadOptions,
    ) -> Result<
        bindings::exports::sqlite::wasm::library::Manifest,
        bindings::exports::sqlite::wasm::library::LoaderError,
    > {
        let pol_opts = lib_load::opts_to_pol(opts);
        bindings::sqlite::wasm::extension_loader::load_extension(&path, &pol_opts)
            .map(lib_load::manifest_to_lib)
            .map_err(lib_load::loader_err_to_lib)
    }

    fn load_extension_from_uri(
        uri: String,
        opts: bindings::exports::sqlite::wasm::library::LoadOptions,
    ) -> Result<
        bindings::exports::sqlite::wasm::library::Manifest,
        bindings::exports::sqlite::wasm::library::LoaderError,
    > {
        let pol_opts = lib_load::opts_to_pol(opts);
        bindings::sqlite::wasm::extension_loader::load_extension_from_uri(&uri, &pol_opts)
            .map(lib_load::manifest_to_lib)
            .map_err(lib_load::loader_err_to_lib)
    }

    fn unload_extension(
        name: String,
    ) -> Result<(), bindings::exports::sqlite::wasm::library::LoaderError> {
        bindings::sqlite::wasm::extension_loader::unload_extension(&name)
            .map_err(lib_load::loader_err_to_lib)
    }
}

bindings::export!(SqliteLib with_types_in bindings);
