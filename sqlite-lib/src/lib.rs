//! sqlite-lib: programmatic SQLite-in-WASM library.
//!
//! Targets the `sqlite-library` world — exports the full
//! `sqlite:extension/*` SPI surface (so a compose-time consumer can
//! satisfy an extension's spi imports with this component) plus the
//! `sqlink:wasm/low-level`, `sqlink:wasm/high-level`, and
//! `sqlink:wasm/library` interfaces for callers that want to embed
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

mod host_vtabs;
#[cfg(target_arch = "wasm32")]
mod opfs_backend;
mod state;

use std::cell::RefCell;

use bindings::exports::sqlite::extension::config::Guest as ConfigGuest;
use bindings::exports::sqlite::extension::logging::{Guest as LoggingGuest, LogLevel};
use bindings::exports::sqlite::extension::spi::{
    self as spi, Guest as SpiGuest, QueryResult as SpiQueryResult,
    SqlValue as SpiSqlValue, SqliteError as SpiSqliteError,
};
use bindings::exports::sqlink::wasm::high_level::{
    Connection, DatabaseError as HlDatabaseError, ExecResult, Guest as HighLevelGuest,
    GuestConnection, GuestStatement, OpenMode, QueryResult as HlQueryResult, Statement,
    Value as HlValue,
};
use bindings::exports::sqlink::wasm::library::Guest as LibraryGuest;
use bindings::exports::sqlink::wasm::low_level::{
    ColumnType, DbHandle, Guest as LowLevelGuest, OpenFlags, ResultCode, StmtHandle,
};

use state::State;

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::new());
}

/// Install the wasm32 cold-tier providers (multi-memory pcache
/// + multi-memory VFS) exactly once, before any
/// `db::Connection::open*` call lands. Pcache install order is
/// load-bearing: SQLite requires `SQLITE_CONFIG_PCACHE2` to be
/// set before `sqlite3_initialize` (which is implicitly fired
/// by the first connection). VFS registration is order-free,
/// so we register but do NOT make it the default — the WASI VFS
/// stays default so file paths still route through the host's
/// `wasi:filesystem`. Browser-side composition (which has no
/// wasi:filesystem) flips the default later via
/// `sqlite_vfs_tvm::install_as_default`.
///
/// On native targets the cold-tier install calls are no-ops
/// (both crates fall back to their in-proc implementations).
#[cfg(target_arch = "wasm32")]
fn tvm_cold_tier_init() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = sqlite_pcache_tvm::install();
        let _ = sqlite_vfs_tvm::install();
        // v1.5 round 4: register the `opfs` VFS so
        // `shared_cas_conn` can open against navigator.storage's OPFS
        // root. Backend is registered with sqlite-vfs-tvm BEFORE the
        // VFS install — the install only allocates the VFS table;
        // first use calls into the backend.
        sqlite_vfs_tvm::opfs::register_backend(Box::new(opfs_backend::WitOpfsBackend));
        let _ = sqlite_vfs_tvm::opfs::install();
    });
}
#[cfg(not(target_arch = "wasm32"))]
fn tvm_cold_tier_init() {}

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

// ── @1.0.0 wit-value payload bridging ──
// The `sql-value::wit-value` arm (new in sqlite:extension@1.0.0)
// carries a canonical-CBOR-encoded WIT record plus its 32-byte
// `type-id` shape hash and a diagnostic symbolic name. The SPI /
// host-marshaling layers preserve the payload's full identity
// (per `db::Value`'s doc-comment); only the actual SQLite C
// bind/column boundary flattens it to a BLOB. These helpers bridge
// the WIT-bindings payload (type-id as `list<u8>`) to the core
// `db::WitValuePayload` (type-id as a fixed `[u8; 32]`), padding /
// truncating to 32 bytes — the host validates length on its own
// boundary, so internal code can rely on the fixed size.
fn db_payload_to_wit_type_id(id: [u8; 32]) -> Vec<u8> {
    id.to_vec()
}

fn wit_type_id_to_db(id: Vec<u8>) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = id.len().min(32);
    out[..n].copy_from_slice(&id[..n]);
    out
}

fn db_to_spi_value(v: db::Value) -> SpiSqlValue {
    match v {
        db::Value::Null => SpiSqlValue::Null,
        db::Value::Integer(i) => SpiSqlValue::Integer(i),
        db::Value::Real(r) => SpiSqlValue::Real(r),
        db::Value::Text(s) => SpiSqlValue::Text(s),
        db::Value::Blob(b) => SpiSqlValue::Blob(b),
        db::Value::WitValue(p) => SpiSqlValue::WitValue(
            bindings::exports::sqlite::extension::types::WitValuePayload {
                type_id: db_payload_to_wit_type_id(p.type_id),
                bytes: p.bytes,
                symbolic_name: p.symbolic_name,
            },
        ),
    }
}

fn spi_value_to_db(v: SpiSqlValue) -> db::Value {
    match v {
        SpiSqlValue::Null => db::Value::Null,
        SpiSqlValue::Integer(i) => db::Value::Integer(i),
        SpiSqlValue::Real(r) => db::Value::Real(r),
        SpiSqlValue::Text(s) => db::Value::Text(s),
        SpiSqlValue::Blob(b) => db::Value::Blob(b),
        SpiSqlValue::WitValue(p) => db::Value::WitValue(db::WitValuePayload {
            type_id: wit_type_id_to_db(p.type_id),
            bytes: p.bytes,
            symbolic_name: p.symbolic_name,
        }),
    }
}

// The "default connection" shared between sqlite:extension/spi and
// sqlink:wasm/high-level.default-connection. SPI calls used to open
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

/// Shim around `db::Connection::open*` that runs the wasm32
/// cold-tier install path (pcache2 + VFS register) exactly once
/// before SQLite gets initialized. Native builds skip the
/// install; wasm32 builds make sure pcache2 is wired before
/// the implicit `sqlite3_initialize` the first connection
/// triggers.
fn open_in_memory_with_init() -> Result<db::Connection, db::Error> {
    tvm_cold_tier_init();
    db::Connection::open_in_memory()
}
fn open_with_init(path: &str, flags: db::OpenFlags) -> Result<db::Connection, db::Error> {
    tvm_cold_tier_init();
    db::Connection::open(path, flags)
}

fn shared_conn() -> std::rc::Rc<RefCell<db::Connection>> {
    SHARED_CONN.with(|c| {
        let mut g = c.borrow_mut();
        if g.is_none() {
            tvm_cold_tier_init();
            let conn = db::Connection::open_in_memory()
                .expect("open in-memory connection for shared SPI/high-level default");
            *g = Some(std::rc::Rc::new(RefCell::new(conn)));
        }
        g.as_ref().unwrap().clone()
    })
}

// ────────────── CAS-cache connection ──────────────
//
// Separate-from-user-data connection serving `dispatch-bridge.
// bridged-execute-cas`. The browser composed runtime's
// `sqlite:extension/bundles` polyfill routes every bundle CRUD
// through this connection so the bundle registry can persist
// independently of the user's data db.
//
// Substrate intent:
//   * native: opens `~/.cache/sqlink/cas.db` with the default
//     VFS. Native dispatch in practice goes through the
//     sqlink-host's direct rusqlite connection (cas-cache's
//     `bundles_exec` free functions), not through this bridge
//     entry — but the bridge entry still works on native so
//     embedders that wire only the composed cli (not the
//     full native host) get a functional cas db.
//   * wasm32: TEMPORARY `:memory:` until the OPFS-backed VFS
//     lands. Schema survives the page lifetime but NOT a
//     reload. The browser polyfill re-runs `INSTALL_SCHEMA` on
//     every fresh load to compensate.
thread_local! {
    static SHARED_CAS_CONN: RefCell<Option<std::rc::Rc<RefCell<db::Connection>>>>
        = const { RefCell::new(None) };
}

fn shared_cas_conn() -> std::rc::Rc<RefCell<db::Connection>> {
    SHARED_CAS_CONN.with(|c| {
        let mut g = c.borrow_mut();
        if g.is_none() {
            tvm_cold_tier_init();
            #[cfg(target_arch = "wasm32")]
            let conn = {
                // v1.5 round 4: open through the `opfs` VFS so the cas
                // db persists across page reloads. The path is the OPFS
                // path the host's WIT impl materializes — leading slash
                // stripped by the host since OPFS doesn't have a true
                // root concept beyond the per-origin storage root.
                // Bytes flow through sqlite-vfs-tvm/src/opfs.rs's IO
                // methods, which call into the WIT-imported opfs-host
                // interface; under JSPI those imports suspend the wasm
                // guest until the host's Promise resolves.
                db::Connection::open_with_vfs(
                    "/sqlink/cas.db",
                    db::OpenFlags::DEFAULT,
                    Some(sqlite_vfs_tvm::opfs::name()),
                )
                .expect("open cas.db via opfs VFS")
            };
            #[cfg(not(target_arch = "wasm32"))]
            let conn = {
                let path = cas_db_path_native();
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                db::Connection::open(&path, db::OpenFlags::DEFAULT)
                    .expect("open ~/.cache/sqlink/cas.db for bridged-execute-cas")
            };
            *g = Some(std::rc::Rc::new(RefCell::new(conn)));
        }
        g.as_ref().unwrap().clone()
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn cas_db_path_native() -> String {
    // Mirror sqlite-cas-cache's path convention without adding a
    // dep on it here — the cas-cache crate isn't in sqlite-lib's
    // dep graph and shouldn't be (sqlite-lib is the lower layer).
    if let Ok(env) = std::env::var("SQLINK_CACHE_DIR") {
        return format!("{env}/cas.db");
    }
    if let Ok(home) = std::env::var("HOME") {
        return format!("{home}/.cache/sqlink/cas.db");
    }
    // Last-ditch fallback: cwd-local cas.db. Better than panic.
    "cas.db".to_string()
}

fn cas_with<R>(f: impl FnOnce(&db::Connection) -> R) -> R {
    let rc = shared_cas_conn();
    let conn = rc.borrow();
    f(&conn)
}

fn spi_with<R>(f: impl FnOnce(&db::Connection) -> R) -> R {
    let rc = shared_conn();
    let conn = rc.borrow();
    f(&conn)
}

impl SpiGuest for SqliteLib {
    fn execute(sql: String, params: Vec<SpiSqlValue>) -> Result<SpiQueryResult, SpiSqliteError> {
        spi_with(|conn| {
            let mut stmt = conn.prepare(&sql).map_err(|e| spi_db_err(e.clone()))?;
            let columns = stmt.column_names();
            let dbs: Vec<db::Value> = params.into_iter().map(spi_value_to_db).collect();
            stmt.bind_all(&dbs).map_err(|e| spi_db_err(e.clone()))?;
            let rows_vals = stmt.collect_rows().map_err(|e| spi_db_err(e.clone()))?;
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
        spi_with(|conn| {
            let mut stmt = conn.prepare(&sql).map_err(|e| spi_db_err(e.clone()))?;
            let dbs: Vec<db::Value> = params.into_iter().map(spi_value_to_db).collect();
            stmt.bind_all(&dbs).map_err(|e| spi_db_err(e.clone()))?;
            let rows_vals = stmt.collect_rows().map_err(|e| spi_db_err(e.clone()))?;
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
        let dst = open_with_init(&dst_path, db::OpenFlags::DEFAULT).map_err(spi_db_err)?;
        spi_with(|src| src.backup_into(&src_db, &dst, &dst_db).map_err(spi_db_err))
    }

    fn restore_from(
        src_path: String,
        src_db: String,
        dst_db: String,
    ) -> Result<(), SpiSqliteError> {
        let src = open_with_init(&src_path, db::OpenFlags::READONLY).map_err(spi_db_err)?;
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
        // a fresh in-memory db; anything else is a file path.
        let new_conn = if path.is_empty() || path == ":memory:" {
            open_in_memory_with_init().map_err(spi_db_err)?
        } else {
            open_with_init(&path, db::OpenFlags::DEFAULT).map_err(spi_db_err)?
        };
        SHARED_CONN.with(|c| {
            *c.borrow_mut() = Some(std::rc::Rc::new(RefCell::new(new_conn)));
        });
        Ok(())
    }
}

// =========================================================================
// sqlite:extension/wal-frames
// =========================================================================
//
// Browser-side this is the wasm-component (sqlite-lib) impl of the
// wal-frames host interface. The composed runtime imports it as
// part of the widened `minimal` world; the native sqlink loader
// has its own impl that reads the host-side `<db_path>-wal`
// directly (see host/src/lib.rs).
//
// Until vfs-tvm grows WAL support (#437), browser-side reads of
// the WAL file have nowhere to land  the data lives in TVM
// memory, not on a filesystem the wasm code can `std::fs::read`.
// We honor the WIT contract by returning the documented sentinel
// (None / SQLITE_NOTFOUND) rather than panicking, which keeps
// extensions that import wal-frames loadable in the browser even
// before #437 ships.

use bindings::exports::sqlite::extension::wal_frames::Guest as WalFramesGuest;

impl WalFramesGuest for SqliteLib {
    fn get_wal_header(_db_name: String) -> Result<Option<Vec<u8>>, SpiSqliteError> {
        // No WAL sidecar reachable from inside the wasm sandbox.
        // The native sqlink host returns the real bytes; the
        // browser-composed runtime returns None until vfs-tvm
        // grows the matching read path (#437).
        Ok(None)
    }

    fn read_frames(
        _db_name: String,
        _start_frame: u32,
        _n_frames: u32,
    ) -> Result<Vec<u8>, SpiSqliteError> {
        Err(SpiSqliteError {
            code: 12, // SQLITE_NOTFOUND
            extended_code: 12,
            message: "wal-frames.read-frames: no WAL access from sqlite-lib \
                      browser-side composed runtime (pending vfs-tvm #437)"
                .to_string(),
        })
    }
}

// =========================================================================
// sqlite:extension/s3-base
// =========================================================================
//
// Browser-side stubs for the host-resident S3 SPI. The composed
// runtime exports the s3-base interface so worlds that import it
// (every world, post #440) link cleanly against sqlite-lib, but
// every method returns a structured "not implemented" error  the
// real impl waits for the JS polyfill bridge (fetch + SigV4)
// follow-up to #437. The wal-archive extension's primary off-box
// sink ships native-first.
//
// The error variant we emit is `internal(...)` rather than a brand-
// new "not-implemented" variant so the browser-side stub stays
// strictly inside the WIT contract  callers can match on the
// existing error shape and don't need to special-case browser
// deployment.

use bindings::exports::sqlite::extension::s3_base::{
    Guest as S3BaseGuest, S3Credentials, S3EndpointConfig, S3Error, S3GetObjectOptions,
    S3GetObjectOutput, S3HeadObjectOutput, S3ListObjectsOptions, S3ListObjectsOutput,
    S3PutObjectOptions, S3PutObjectOutput,
};
use bindings::exports::sqlite::extension::build::{
    BuildOut, Guest as BuildGuest,
};

const S3_BROWSER_STUB: &str =
    "s3-base: not implemented in sqlite-lib browser-composed runtime \
     (pending fetch+SigV4 polyfill bridge follow-up to #437)";

impl S3BaseGuest for SqliteLib {
    fn get_object(
        _endpoint: S3EndpointConfig,
        _credentials: S3Credentials,
        _bucket: String,
        _key: String,
        _options: Option<S3GetObjectOptions>,
    ) -> Result<S3GetObjectOutput, S3Error> {
        Err(S3Error::Internal(S3_BROWSER_STUB.to_string()))
    }

    fn put_object(
        _endpoint: S3EndpointConfig,
        _credentials: S3Credentials,
        _bucket: String,
        _key: String,
        _body: Vec<u8>,
        _options: Option<S3PutObjectOptions>,
    ) -> Result<S3PutObjectOutput, S3Error> {
        Err(S3Error::Internal(S3_BROWSER_STUB.to_string()))
    }

    fn delete_object(
        _endpoint: S3EndpointConfig,
        _credentials: S3Credentials,
        _bucket: String,
        _key: String,
    ) -> Result<(), S3Error> {
        Err(S3Error::Internal(S3_BROWSER_STUB.to_string()))
    }

    fn head_object(
        _endpoint: S3EndpointConfig,
        _credentials: S3Credentials,
        _bucket: String,
        _key: String,
    ) -> Result<S3HeadObjectOutput, S3Error> {
        Err(S3Error::Internal(S3_BROWSER_STUB.to_string()))
    }

    fn list_objects(
        _endpoint: S3EndpointConfig,
        _credentials: S3Credentials,
        _bucket: String,
        _options: Option<S3ListObjectsOptions>,
    ) -> Result<S3ListObjectsOutput, S3Error> {
        Err(S3Error::Internal(S3_BROWSER_STUB.to_string()))
    }

    fn copy_object(
        _endpoint: S3EndpointConfig,
        _credentials: S3Credentials,
        _source_bucket: String,
        _source_key: String,
        _dest_bucket: String,
        _dest_key: String,
    ) -> Result<S3PutObjectOutput, S3Error> {
        Err(S3Error::Internal(S3_BROWSER_STUB.to_string()))
    }
}

// =========================================================================
// sqlite:extension/build
// =========================================================================
//
// Browser-side stub for the host-resident build SPI. Wasm
// sandboxes can't spawn cargo, so this returns SQLITE_PERM with
// a clear "not supported in the browser runtime" message. The
// contract is exported (rather than omitted) so worlds that
// import build link cleanly against sqlite-lib  the bundle-cli
// extension's baked-binary path is a native-only flow by design
// (PLAN-bundles.md).
impl BuildGuest for SqliteLib {
    fn spawn_build(
        _crate_root: String,
        _target_triple: Option<String>,
        _env: Vec<(String, String)>,
        _cargo_package: Option<String>,
        _features: Vec<String>,
    ) -> Result<BuildOut, SpiSqliteError> {
        Err(SpiSqliteError {
            code: 23, // SQLITE_PERM
            extended_code: 23,
            message:
                "build.spawn-build: not supported in sqlite-lib browser-composed \
                 runtime (process spawn unavailable inside the wasm sandbox)"
                    .to_string(),
        })
    }
}

// =========================================================================
// sqlite:extension/bundles
// =========================================================================
//
// Browser-side stub for the host-resident bundle registry SPI. The
// cas-cache that backs bundles natively lives at
// `~/.cache/sqlink/cas.sqlite` (host-managed). The wasm sandbox has
// no access to that store; every method returns SQLITE_PERM with a
// clear "not supported in the browser runtime" message. The contract
// is still exported so worlds that import bundles link cleanly
// against sqlite-lib  the bundle-cli extension's metadata path is
// native-only by design (PLAN-bundles.md #446 v1; a future
// browser-side variant could implement IndexedDB-backed bundles).
use bindings::exports::sqlite::extension::bundles::{
    BundleDetail, BundleMember, BundleSummary, GcPolicy,
    Guest as BundlesGuest,
};

const BUNDLES_BROWSER_STUB: &str =
    "bundles: not supported in sqlite-lib browser-composed runtime \
     (no cas-cache reachable from the wasm sandbox; native deployment \
     only in v1)";

fn bundles_perm_err() -> SpiSqliteError {
    SpiSqliteError {
        code: 23, // SQLITE_PERM
        extended_code: 23,
        message: BUNDLES_BROWSER_STUB.to_string(),
    }
}

impl BundlesGuest for SqliteLib {
    fn bundle_save(
        _name: Option<String>,
        _set_hash: String,
        _members: Vec<BundleMember>,
    ) -> Result<u64, SpiSqliteError> {
        Err(bundles_perm_err())
    }

    fn bundle_find_by_name(
        _name: String,
    ) -> Result<Option<BundleSummary>, SpiSqliteError> {
        Err(bundles_perm_err())
    }

    fn bundle_find_by_hash_prefix(
        _prefix: String,
    ) -> Result<Vec<BundleSummary>, SpiSqliteError> {
        Err(bundles_perm_err())
    }

    fn bundle_list() -> Result<Vec<BundleSummary>, SpiSqliteError> {
        Err(bundles_perm_err())
    }

    fn bundle_show(_id: u64) -> Result<BundleDetail, SpiSqliteError> {
        Err(bundles_perm_err())
    }

    fn bundle_delete(_id: u64) -> Result<(), SpiSqliteError> {
        Err(bundles_perm_err())
    }

    fn bundle_gc(_policy: GcPolicy) -> Result<Vec<u64>, SpiSqliteError> {
        Err(bundles_perm_err())
    }

    fn bundle_record_binary(
        _id: u64,
        _target_triple: String,
        _binary_path: String,
    ) -> Result<(), SpiSqliteError> {
        Err(bundles_perm_err())
    }

    fn bundle_touch(_id: u64) {
        // touch is fire-and-forget; nothing to record in the stub
    }

    // ── @1.0.0 bundle-alias SPI (host-spi.wit additions) ──
    // Aliases are a cas-cache registry concern; the browser stub has
    // no store to bind them in, so each returns the same SQLITE_PERM
    // sentinel as every other bundles method.
    fn bundle_add_alias(
        _bundle_id: u64,
        _alias: String,
    ) -> Result<(), SpiSqliteError> {
        Err(bundles_perm_err())
    }

    fn bundle_remove_alias(_alias: String) -> Result<bool, SpiSqliteError> {
        Err(bundles_perm_err())
    }

    fn bundle_aliases(
        _bundle_id: u64,
    ) -> Result<Vec<String>, SpiSqliteError> {
        Err(bundles_perm_err())
    }
}

// =========================================================================
// sqlink:wasm/low-level
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
        let path = if filename.is_empty() || filename == ":memory:" {
            ":memory:".to_string()
        } else {
            filename
        };
        let conn = if path == ":memory:" {
            open_in_memory_with_init()
        } else {
            open_with_init(&path, ll_open_flags(flags))
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
// sqlink:wasm/high-level
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
        // The high-level surface (sqlink:wasm/high-level) has no
        // wit-value arm — its value variant predates @1.0.0. At this
        // boundary the typed record flattens to a BLOB carrying the
        // canonical-CBOR bytes, matching the documented SQLite
        // column-store behavior in `db::Value`'s contract.
        db::Value::WitValue(p) => HlValue::Blob(p.bytes),
    }
}

impl HighLevelGuest for SqliteLib {
    type Connection = HlConnection;
    type Statement = HlStatement;

    fn version() -> String { db::version() }
    fn version_number() -> i32 { db::version_number() }
    fn open_memory() -> Result<Connection, HlDatabaseError> {
        match open_in_memory_with_init() {
            Ok(c) => Ok(Connection::new(HlConnection { conn: std::rc::Rc::new(RefCell::new(c)) })),
            Err(e) => Err(hl_err(&e)),
        }
    }
    fn open_file(path: String) -> Result<Connection, HlDatabaseError> {
        match open_with_init(&path, db::OpenFlags::DEFAULT) {
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
        let conn = match mode {
            OpenMode::Memory => open_in_memory_with_init(),
            OpenMode::ReadOnly => open_with_init(&path, db::OpenFlags::READONLY),
            _ => open_with_init(&path, db::OpenFlags::DEFAULT),
        };
        HlConnection {
            conn: std::rc::Rc::new(RefCell::new(
                conn.unwrap_or_else(|_| open_in_memory_with_init().unwrap()),
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
        let out_rows: Vec<bindings::exports::sqlink::wasm::high_level::Row> = rows_vals
            .into_iter()
            .map(|r| bindings::exports::sqlink::wasm::high_level::Row {
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
        let out_rows: Vec<bindings::exports::sqlink::wasm::high_level::Row> = rows_vals
            .into_iter()
            .map(|r| bindings::exports::sqlink::wasm::high_level::Row {
                columns: r.into_iter().map(db_to_hl_value).collect(),
            })
            .collect();
        Ok(HlQueryResult { column_names, rows: out_rows })
    }

    fn step(&self) -> Result<Option<bindings::exports::sqlink::wasm::high_level::Row>, HlDatabaseError> {
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
        Ok(buf.pop_front().map(|raw| bindings::exports::sqlink::wasm::high_level::Row {
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
// sqlink:wasm/library
// Forwards load-extension calls to the host's extension-loader.
// The library interface's policy/metadata types are structural twins
// of the canonical sqlite:extension types — see wit/library.wit for
// why they aren't `use`-imported. Conversion is mechanical.
// =========================================================================

mod lib_load {
    use super::bindings;
    use bindings::exports::sqlink::wasm::library::{
        AggregateFunctionSpec as LibAggSpec, Capability as LibCap, CollationSpec as LibCollSpec,
        DnsPolicy as LibDnsPolicy, FsPolicy as LibFsPolicy, FunctionFlags as LibFlags,
        HttpMethod as LibMethod, HttpPolicy as LibHttpPolicy, LoadOptions as LibOpts,
        LoaderError as LibLoaderError, Manifest as LibManifest,
        ScalarFunctionSpec as LibScalarSpec,
    };
    use bindings::sqlite::extension::metadata as md;
    use bindings::sqlite::extension::policy as pol;
    use bindings::sqlite::extension::types as ty;
    use bindings::sqlink::wasm::extension_loader as loader;

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
            LibCap::WalFrames => pol::Capability::WalFrames,
            LibCap::S3 => pol::Capability::S3,
            LibCap::SpawnBuild => pol::Capability::SpawnBuild,
            LibCap::Bundles => pol::Capability::Bundles,
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
            pol::Capability::WalFrames => LibCap::WalFrames,
            pol::Capability::S3 => LibCap::S3,
            pol::Capability::SpawnBuild => LibCap::SpawnBuild,
            pol::Capability::Bundles => LibCap::Bundles,
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
            optional_capabilities: m.optional_capabilities.into_iter().map(cap_to_lib).collect(),
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
        opts: bindings::exports::sqlink::wasm::library::LoadOptions,
    ) -> Result<
        bindings::exports::sqlink::wasm::library::Manifest,
        bindings::exports::sqlink::wasm::library::LoaderError,
    > {
        let pol_opts = lib_load::opts_to_pol(opts);
        bindings::sqlink::wasm::extension_loader::load_extension(&path, &pol_opts)
            .map(lib_load::manifest_to_lib)
            .map_err(lib_load::loader_err_to_lib)
    }

    fn load_extension_from_uri(
        uri: String,
        opts: bindings::exports::sqlink::wasm::library::LoadOptions,
    ) -> Result<
        bindings::exports::sqlink::wasm::library::Manifest,
        bindings::exports::sqlink::wasm::library::LoaderError,
    > {
        let pol_opts = lib_load::opts_to_pol(opts);
        bindings::sqlink::wasm::extension_loader::load_extension_from_uri(&uri, &pol_opts)
            .map(lib_load::manifest_to_lib)
            .map_err(lib_load::loader_err_to_lib)
    }

    fn unload_extension(
        name: String,
    ) -> Result<(), bindings::exports::sqlink::wasm::library::LoaderError> {
        bindings::sqlink::wasm::extension_loader::unload_extension(&name)
            .map_err(lib_load::loader_err_to_lib)
    }
}

// =========================================================================
// sqlink:wasm/dispatch-bridge
//
// Lets the host install a sqlite3 scalar-function trampoline on
// sqlite-lib's internal default connection. The trampoline's body
// calls back out via the imported `dispatch.scalar-call`.
//
// Why this exists: in the native scenario the host owns the
// sqlite3* connection that serves BOTH spi.execute and
// spi-loader.register-scalar, so `sqlite3_create_function_v2` from
// register-scalar is immediately visible to the SQL the cli runs
// next. In the composed `cli + sqlite-lib` browser scenario the
// cli's spi.execute is served by sqlite-lib's in-WASM connection
// but spi-loader.register-scalar is served by the JS host the JS
// host needs a path to inject a function into sqlite-lib's
// connection. This export is that path.
//
// Note on re-entry: the trampoline runs inside sqlite3_step under
// a `RefCell::borrow` on shared_conn (held by `spi_with`). If the
// imported `dispatch.scalar-call` re-enters this component via
// spi.execute (e.g. a host-implemented scalar runs `SELECT ...`
// internally) the second borrow_mut will panic. Today none of the
// smoke matrix's scalars do SPI re-entry; uuid(), regexp_match(),
// etc are pure functions. Aggregates / vtabs that do recursive SPI
// will need a `try_borrow` fallback or a re-entrant SPI shape; out
// of scope for v1.
// =========================================================================

mod host_scalars {
    use super::bindings;
    use super::shared_conn;
    use super::spi_db_err;
    use crate::db;
    use bindings::exports::sqlink::wasm::dispatch_bridge::Guest as DispatchBridgeGuest;
    use bindings::exports::sqlink::wasm::dispatch_bridge_cas::Guest as DispatchBridgeCasGuest;
    use bindings::exports::sqlite::extension::spi::{
        Guest as SpiGuest, QueryResult as SpiQueryResult, SqlValue as SpiSqlValue,
        SqliteError as SpiSqliteError,
    };
    use bindings::sqlite::extension::types::SqlValue as ImpSqlValue;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// `db::Value`  imported-side `SqlValue` (the type the
    /// `dispatch.scalar-call` import wants). Mirrors the existing
    /// `db_to_spi_value` but targets the import-side variant,
    /// which wit-bindgen treats as a distinct type from the
    /// export-side one even though the WIT shape is the same.
    fn db_to_imp_value(v: db::Value) -> ImpSqlValue {
        use bindings::sqlite::extension::types::WitValuePayload as ImpWitValuePayload;
        match v {
            db::Value::Null => ImpSqlValue::Null,
            db::Value::Integer(i) => ImpSqlValue::Integer(i),
            db::Value::Real(r) => ImpSqlValue::Real(r),
            db::Value::Text(s) => ImpSqlValue::Text(s),
            db::Value::Blob(b) => ImpSqlValue::Blob(b),
            db::Value::WitValue(p) => ImpSqlValue::WitValue(ImpWitValuePayload {
                type_id: super::db_payload_to_wit_type_id(p.type_id),
                bytes: p.bytes,
                symbolic_name: p.symbolic_name,
            }),
        }
    }

    fn imp_value_to_db(v: ImpSqlValue) -> db::Value {
        match v {
            ImpSqlValue::Null => db::Value::Null,
            ImpSqlValue::Integer(i) => db::Value::Integer(i),
            ImpSqlValue::Real(r) => db::Value::Real(r),
            ImpSqlValue::Text(s) => db::Value::Text(s),
            ImpSqlValue::Blob(b) => db::Value::Blob(b),
            ImpSqlValue::WitValue(p) => db::Value::WitValue(db::WitValuePayload {
                type_id: super::wit_type_id_to_db(p.type_id),
                bytes: p.bytes,
                symbolic_name: p.symbolic_name,
            }),
        }
    }

    /// Kinds of host-resident registrations a single extension can
    /// own on sqlite-lib's connection. `unregister-extension` walks
    /// every entry under the ext-name key and routes to the right
    /// SQLite removal API (`remove_function` for scalar/aggregate,
    /// `remove_collation` for collation, the appropriate
    /// `*_hook(None)` / `set_authorizer(None)` call for the hook
    /// variants, but only if THIS ext-name is the one currently
    /// owning the slot — see `HOOK_OWNERS`).
    #[derive(Clone)]
    enum HostRegistration {
        Scalar { name: String, num_args: i32 },
        Aggregate { name: String, num_args: i32 },
        Collation { name: String },
        /// One of the four singleton-per-connection slots
        /// (authorizer, update-hook, commit-hook, rollback-hook).
        /// `unregister-extension` only clears the slot if the
        /// currently-owning ext-name (`HOOK_OWNERS`) matches.
        Hook { kind: HookKind },
    }

    /// The singleton-per-connection hook slots. SQLite allows only
    /// one of each per connection; v1 dispatch-bridge semantics are
    /// last-write-wins.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub(super) enum HookKind {
        Authorizer,
        UpdateHook,
        CommitHook,
        RollbackHook,
        WalHook,
    }

    thread_local! {
        /// Per-extension list of registrations. Used by
        /// `unregister-extension` to walk + drop everything installed
        /// under a given ext-name. Indexed by ext_name because the JS
        /// host's `.unload` flow only carries the extension name, not
        /// the per-function (name, arity, kind) list at unload time.
        static REGISTRY: RefCell<HashMap<String, Vec<HostRegistration>>> =
            RefCell::new(HashMap::new());

        /// Monotonic counter handed out as `context-id` by the
        /// aggregate trampoline's `init()` callback. Each pending
        /// aggregation gets a fresh id the host uses to key
        /// per-aggregation state across step / finalize / value /
        /// inverse calls.
        static AGG_CTX_COUNTER: AtomicU64 = const { AtomicU64::new(1) };

        /// Which ext-name currently owns each singleton hook slot
        /// (authorizer, update-hook, commit-hook, rollback-hook).
        /// `None` = slot empty. v1 last-write-wins: re-registering
        /// the same kind from a different ext-name overwrites the
        /// previous owner; the previous owner's REGISTRY entry is
        /// stale but harmless (unregister-extension on the previous
        /// owner finds nothing to clear because HOOK_OWNERS no
        /// longer points to it).
        static HOOK_OWNERS: RefCell<HashMap<HookKind, String>> =
            RefCell::new(HashMap::new());
    }

    fn misuse_err(msg: impl Into<String>) -> SpiSqliteError {
        SpiSqliteError {
            code: libsqlite3_sys::SQLITE_MISUSE,
            extended_code: libsqlite3_sys::SQLITE_MISUSE,
            message: msg.into(),
        }
    }

    pub(super) fn register_host_scalar(
        ext_name: String,
        name: String,
        num_args: i32,
        func_id: u64,
    ) -> Result<(), SpiSqliteError> {
        if name.is_empty() {
            return Err(misuse_err("register-host-scalar: empty function name"));
        }
        if ext_name.is_empty() {
            return Err(misuse_err("register-host-scalar: empty extension name"));
        }

        // Build the trampoline. Captures ext_name + func_id; on
        // call, marshals SqlValue args, invokes the imported
        // `dispatch.scalar-call`, and threads the result back.
        // Cloned because `db::Connection::create_scalar_function`
        // wants a 'static closure.
        let ext_name_cb = ext_name.clone();
        let name_cb = name.clone();
        let callback = move |args: &[db::Value]| -> Result<db::Value, db::Error> {
            let imp_args: Vec<ImpSqlValue> =
                args.iter().cloned().map(db_to_imp_value).collect();
            match bindings::sqlink::wasm::dispatch::scalar_call(
                &ext_name_cb,
                func_id,
                &imp_args,
            ) {
                Ok(v) => Ok(imp_value_to_db(v)),
                Err(msg) => Err(db::Error {
                    code: libsqlite3_sys::SQLITE_ERROR,
                    extended_code: libsqlite3_sys::SQLITE_ERROR,
                    message: format!(
                        "extension `{}` scalar `{}` (func-id {}): {}",
                        ext_name_cb, name_cb, func_id, msg
                    ),
                }),
            }
        };

        let rc = shared_conn();
        let conn = rc.borrow();
        conn.create_scalar_function(
            &name,
            num_args,
            db::FunctionFlags::UTF8 | db::FunctionFlags::DETERMINISTIC,
            callback,
        )
        .map_err(spi_db_err)?;
        drop(conn);

        REGISTRY.with(|r| {
            r.borrow_mut()
                .entry(ext_name)
                .or_default()
                .push(HostRegistration::Scalar { name, num_args });
        });
        Ok(())
    }

    /// Trampoline implementation of `db::Aggregate<u64>` that
    /// forwards every step/finalize call out via the imported
    /// `dispatch.aggregate-*`. The state type `S = u64` is the
    /// per-aggregation `context-id` SQLite's
    /// `sqlite3_aggregate_context` keeps for the lifetime of the
    /// aggregation; `init()` pulls a fresh id from
    /// `AGG_CTX_COUNTER` so the JS host can key state by it.
    struct HostAggregate {
        ext_name: String,
        name: String,
        func_id: u64,
    }

    fn agg_to_db_err(ext: &str, name: &str, func_id: u64, kind: &str, msg: String) -> db::Error {
        db::Error {
            code: libsqlite3_sys::SQLITE_ERROR,
            extended_code: libsqlite3_sys::SQLITE_ERROR,
            message: format!(
                "extension `{}` aggregate `{}` {} (func-id {}): {}",
                ext, name, kind, func_id, msg
            ),
        }
    }

    impl db::Aggregate<u64> for HostAggregate {
        fn init(&self) -> u64 {
            AGG_CTX_COUNTER.with(|c| c.fetch_add(1, Ordering::Relaxed))
        }

        fn step(&self, ctx: &mut u64, args: &[db::Value]) -> Result<(), db::Error> {
            let imp_args: Vec<ImpSqlValue> =
                args.iter().cloned().map(db_to_imp_value).collect();
            bindings::sqlink::wasm::dispatch::aggregate_step(
                &self.ext_name,
                self.func_id,
                *ctx,
                &imp_args,
            )
            .map_err(|msg| agg_to_db_err(&self.ext_name, &self.name, self.func_id, "step", msg))
        }

        fn finalize(&self, ctx: Option<u64>) -> Result<db::Value, db::Error> {
            // If init was never called (no rows in the aggregation),
            // SQLite still fires xFinal — synthesize a fresh context
            // so the host's dispatch.aggregate-finalize sees a stable
            // id even for the empty case. The host's impl is expected
            // to treat an unknown context-id as "no state, return the
            // identity value for this aggregate".
            let ctx_id = ctx.unwrap_or_else(|| {
                AGG_CTX_COUNTER.with(|c| c.fetch_add(1, Ordering::Relaxed))
            });
            bindings::sqlink::wasm::dispatch::aggregate_finalize(
                &self.ext_name,
                self.func_id,
                ctx_id,
            )
            .map(imp_value_to_db)
            .map_err(|msg| {
                agg_to_db_err(&self.ext_name, &self.name, self.func_id, "finalize", msg)
            })
        }
    }

    impl db::WindowAggregate<u64> for HostAggregate {
        fn value(&self, ctx: &u64) -> Result<db::Value, db::Error> {
            bindings::sqlink::wasm::dispatch::aggregate_value(
                &self.ext_name,
                self.func_id,
                *ctx,
            )
            .map(imp_value_to_db)
            .map_err(|msg| agg_to_db_err(&self.ext_name, &self.name, self.func_id, "value", msg))
        }

        fn inverse(&self, ctx: &mut u64, args: &[db::Value]) -> Result<(), db::Error> {
            let imp_args: Vec<ImpSqlValue> =
                args.iter().cloned().map(db_to_imp_value).collect();
            bindings::sqlink::wasm::dispatch::aggregate_inverse(
                &self.ext_name,
                self.func_id,
                *ctx,
                &imp_args,
            )
            .map_err(|msg| {
                agg_to_db_err(&self.ext_name, &self.name, self.func_id, "inverse", msg)
            })
        }
    }

    pub(super) fn register_host_aggregate(
        ext_name: String,
        name: String,
        num_args: i32,
        func_id: u64,
        is_window: bool,
    ) -> Result<(), SpiSqliteError> {
        if name.is_empty() {
            return Err(misuse_err("register-host-aggregate: empty function name"));
        }
        if ext_name.is_empty() {
            return Err(misuse_err("register-host-aggregate: empty extension name"));
        }

        let agg = HostAggregate {
            ext_name: ext_name.clone(),
            name: name.clone(),
            func_id,
        };

        let rc = shared_conn();
        let conn = rc.borrow();
        let flags = db::FunctionFlags::UTF8 | db::FunctionFlags::DIRECTONLY;
        if is_window {
            conn.create_window_function(&name, num_args, flags, agg)
                .map_err(spi_db_err)?;
        } else {
            conn.create_aggregate_function(&name, num_args, flags, agg)
                .map_err(spi_db_err)?;
        }
        drop(conn);

        REGISTRY.with(|r| {
            r.borrow_mut()
                .entry(ext_name)
                .or_default()
                .push(HostRegistration::Aggregate { name, num_args });
        });
        Ok(())
    }

    pub(super) fn register_host_collation(
        ext_name: String,
        name: String,
        collation_id: u64,
    ) -> Result<(), SpiSqliteError> {
        if name.is_empty() {
            return Err(misuse_err("register-host-collation: empty collation name"));
        }
        if ext_name.is_empty() {
            return Err(misuse_err("register-host-collation: empty extension name"));
        }

        // Trampoline: marshal both strings to the WIT-side strings
        // and forward to dispatch.collation-compare. Stateless: no
        // per-comparison context, so we just call.
        let ext_name_cb = ext_name.clone();
        let compare = move |a: &str, b: &str| -> std::cmp::Ordering {
            let rc =
                bindings::sqlink::wasm::dispatch::collation_compare(&ext_name_cb, collation_id, a, b);
            if rc < 0 {
                std::cmp::Ordering::Less
            } else if rc > 0 {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        };

        let rc = shared_conn();
        let conn = rc.borrow();
        conn.create_collation(&name, compare).map_err(spi_db_err)?;
        drop(conn);

        REGISTRY.with(|r| {
            r.borrow_mut()
                .entry(ext_name)
                .or_default()
                .push(HostRegistration::Collation { name });
        });
        Ok(())
    }

    /// Map a SQLite auth-action code (libsqlite3_sys::SQLITE_*) to
    /// the WIT-side `AuthAction` enum the dispatch interface uses.
    /// Unrecognized codes fall back to `Function` — the safest
    /// "permit unless deny" bucket, since the host's authorizer
    /// will see them as `function` and decide based on its own
    /// policy. The cli's stderr-logging authorizer cares about the
    /// raw action code (passed elsewhere); the WIT-side enum is
    /// the contract for dynamically-loaded extensions.
    fn imp_auth_action(code: i32) -> bindings::sqlite::extension::types::AuthAction {
        use bindings::sqlite::extension::types::AuthAction as A;
        use libsqlite3_sys::*;
        match code {
            SQLITE_CREATE_INDEX => A::CreateIndex,
            SQLITE_CREATE_TABLE => A::CreateTable,
            SQLITE_CREATE_TEMP_INDEX => A::CreateTempIndex,
            SQLITE_CREATE_TEMP_TABLE => A::CreateTempTable,
            SQLITE_CREATE_TEMP_TRIGGER => A::CreateTempTrigger,
            SQLITE_CREATE_TEMP_VIEW => A::CreateTempView,
            SQLITE_CREATE_TRIGGER => A::CreateTrigger,
            SQLITE_CREATE_VIEW => A::CreateView,
            SQLITE_DELETE => A::Delete,
            SQLITE_DROP_INDEX => A::DropIndex,
            SQLITE_DROP_TABLE => A::DropTable,
            SQLITE_DROP_TEMP_INDEX => A::DropTempIndex,
            SQLITE_DROP_TEMP_TABLE => A::DropTempTable,
            SQLITE_DROP_TEMP_TRIGGER => A::DropTempTrigger,
            SQLITE_DROP_TEMP_VIEW => A::DropTempView,
            SQLITE_DROP_TRIGGER => A::DropTrigger,
            SQLITE_DROP_VIEW => A::DropView,
            SQLITE_INSERT => A::Insert,
            SQLITE_PRAGMA => A::Pragma,
            SQLITE_READ => A::Read,
            SQLITE_SELECT => A::Select,
            SQLITE_TRANSACTION => A::Transaction,
            SQLITE_UPDATE => A::Update,
            SQLITE_ATTACH => A::Attach,
            SQLITE_DETACH => A::Detach,
            SQLITE_ALTER_TABLE => A::AlterTable,
            SQLITE_REINDEX => A::Reindex,
            SQLITE_ANALYZE => A::Analyze,
            SQLITE_CREATE_VTABLE => A::CreateVtable,
            SQLITE_DROP_VTABLE => A::DropVtable,
            SQLITE_FUNCTION => A::Function,
            SQLITE_SAVEPOINT => A::Savepoint,
            SQLITE_RECURSIVE => A::Recursive,
            _ => A::Function,
        }
    }

    fn imp_auth_result_to_db(
        r: bindings::sqlite::extension::types::AuthResult,
    ) -> db::AuthResult {
        use bindings::sqlite::extension::types::AuthResult as A;
        match r {
            A::Ok => db::AuthResult::Allow,
            A::Deny => db::AuthResult::Deny,
            A::Ignore => db::AuthResult::Ignore,
        }
    }

    fn imp_update_op(action: db::UpdateAction) -> bindings::sqlite::extension::types::UpdateOperation {
        use bindings::sqlite::extension::types::UpdateOperation as U;
        match action {
            db::UpdateAction::Insert => U::Insert,
            db::UpdateAction::Update => U::Update,
            db::UpdateAction::Delete => U::Delete,
            // SQLite only ever emits INSERT/UPDATE/DELETE codes
            // through update_hook today; the WIT enum has no
            // "unknown" bucket. Treat unknown as Update — the host's
            // hook impl sees a stable shape and can ignore.
            db::UpdateAction::Unknown => U::Update,
        }
    }

    pub(super) fn register_host_authorizer(
        ext_name: String,
    ) -> Result<(), SpiSqliteError> {
        if ext_name.is_empty() {
            return Err(misuse_err("register-host-authorizer: empty extension name"));
        }
        let ext_name_cb = ext_name.clone();
        let callback = move |action: std::os::raw::c_int,
                             arg1: Option<String>,
                             arg2: Option<String>,
                             arg3: Option<String>,
                             arg4: Option<String>|
              -> db::AuthResult {
            let wit_action = imp_auth_action(action);
            // dispatch.authorize WIT signature is
            //   (ext-name, action, arg1, arg2, database, trigger) -> auth-result
            // sqlite's authorizer C callback receives 4 strings whose
            // meaning depends on the action; the 3rd is conventionally
            // "database name" and the 4th "inner trigger or view name".
            let r = bindings::sqlink::wasm::dispatch::authorize(
                &ext_name_cb,
                wit_action,
                arg1.as_deref(),
                arg2.as_deref(),
                arg3.as_deref(),
                arg4.as_deref(),
            );
            imp_auth_result_to_db(r)
        };

        let rc = shared_conn();
        let conn = rc.borrow();
        conn.set_authorizer(Some(callback)).map_err(spi_db_err)?;
        drop(conn);

        HOOK_OWNERS.with(|o| {
            o.borrow_mut().insert(HookKind::Authorizer, ext_name.clone());
        });
        REGISTRY.with(|r| {
            r.borrow_mut()
                .entry(ext_name)
                .or_default()
                .push(HostRegistration::Hook {
                    kind: HookKind::Authorizer,
                });
        });
        Ok(())
    }

    pub(super) fn register_host_update_hook(
        ext_name: String,
    ) -> Result<(), SpiSqliteError> {
        if ext_name.is_empty() {
            return Err(misuse_err("register-host-update-hook: empty extension name"));
        }
        let ext_name_cb = ext_name.clone();
        let callback = move |action: db::UpdateAction, db: &str, table: &str, rowid: i64| {
            bindings::sqlink::wasm::dispatch::on_update(
                &ext_name_cb,
                imp_update_op(action),
                db,
                table,
                rowid,
            );
        };

        let rc = shared_conn();
        let conn = rc.borrow();
        conn.update_hook(Some(callback));
        drop(conn);

        HOOK_OWNERS.with(|o| {
            o.borrow_mut().insert(HookKind::UpdateHook, ext_name.clone());
        });
        REGISTRY.with(|r| {
            r.borrow_mut()
                .entry(ext_name)
                .or_default()
                .push(HostRegistration::Hook {
                    kind: HookKind::UpdateHook,
                });
        });
        Ok(())
    }

    pub(super) fn register_host_commit_hook(
        ext_name: String,
    ) -> Result<(), SpiSqliteError> {
        if ext_name.is_empty() {
            return Err(misuse_err("register-host-commit-hook: empty extension name"));
        }
        let ext_name_cb = ext_name.clone();
        // SQLite's commit-hook returns non-zero to ABORT the commit.
        // dispatch.on-commit's WIT return is `bool` where:
        //   true  = allow commit (return 0 to sqlite)
        //   false = abort commit (return non-zero to sqlite)
        // db::Connection::commit_hook takes `Fn() -> bool` where the
        // returned bool IS the "abort" flag (matches sqlite's raw
        // semantics, not the WIT semantics). Map between the two.
        let callback = move || -> bool {
            let allow = bindings::sqlink::wasm::dispatch::on_commit(&ext_name_cb);
            !allow
        };

        let rc = shared_conn();
        let conn = rc.borrow();
        conn.commit_hook(Some(callback));
        drop(conn);

        HOOK_OWNERS.with(|o| {
            o.borrow_mut().insert(HookKind::CommitHook, ext_name.clone());
        });
        REGISTRY.with(|r| {
            r.borrow_mut()
                .entry(ext_name)
                .or_default()
                .push(HostRegistration::Hook {
                    kind: HookKind::CommitHook,
                });
        });
        Ok(())
    }

    pub(super) fn register_host_rollback_hook(
        ext_name: String,
    ) -> Result<(), SpiSqliteError> {
        if ext_name.is_empty() {
            return Err(misuse_err(
                "register-host-rollback-hook: empty extension name",
            ));
        }
        let ext_name_cb = ext_name.clone();
        let callback = move || {
            bindings::sqlink::wasm::dispatch::on_rollback(&ext_name_cb);
        };

        let rc = shared_conn();
        let conn = rc.borrow();
        conn.rollback_hook(Some(callback));
        drop(conn);

        HOOK_OWNERS.with(|o| {
            o.borrow_mut().insert(HookKind::RollbackHook, ext_name.clone());
        });
        REGISTRY.with(|r| {
            r.borrow_mut()
                .entry(ext_name)
                .or_default()
                .push(HostRegistration::Hook {
                    kind: HookKind::RollbackHook,
                });
        });
        Ok(())
    }

    pub(super) fn register_host_wal_hook(
        ext_name: String,
        hook_id: u64,
    ) -> Result<(), SpiSqliteError> {
        if ext_name.is_empty() {
            return Err(misuse_err(
                "register-host-wal-hook: empty extension name",
            ));
        }
        let ext_name_cb = ext_name.clone();
        // db::Connection::wal_hook takes `Fn(&str, i32) -> i32` and
        // returns the raw sqlite result code. dispatch.wal-hook
        // matches that contract directly (returns s32, SQLITE_OK
        // for normal continuation).
        let callback = move |db_name: &str, n_frames: i32| -> i32 {
            // n_frames is a C int from sqlite; the dispatch WIT signature
            // uses u32 (frames count is always non-negative in practice).
            // Clamp negative values to 0 defensively.
            let frames_u32 = u32::try_from(n_frames).unwrap_or(0);
            bindings::sqlink::wasm::dispatch::wal_hook(
                &ext_name_cb,
                hook_id,
                db_name,
                frames_u32,
            )
        };

        let rc = shared_conn();
        let conn = rc.borrow();
        conn.wal_hook(Some(callback));
        drop(conn);

        HOOK_OWNERS.with(|o| {
            o.borrow_mut().insert(HookKind::WalHook, ext_name.clone());
        });
        REGISTRY.with(|r| {
            r.borrow_mut()
                .entry(ext_name)
                .or_default()
                .push(HostRegistration::Hook {
                    kind: HookKind::WalHook,
                });
        });
        Ok(())
    }

    /// Clear a singleton hook slot on the shared connection. Only
    /// clears the slot if `ext_name` is the current owner — if
    /// another extension's register-host-* call came in between
    /// the registration and unregister-extension, the new owner's
    /// trampoline stays installed.
    fn clear_hook_if_owner(conn: &db::Connection, kind: HookKind, ext_name: &str) {
        let is_owner = HOOK_OWNERS.with(|o| {
            o.borrow()
                .get(&kind)
                .is_some_and(|owner| owner == ext_name)
        });
        if !is_owner {
            return;
        }
        match kind {
            HookKind::Authorizer => {
                let _ = conn.set_authorizer(None::<
                    fn(
                        std::os::raw::c_int,
                        Option<String>,
                        Option<String>,
                        Option<String>,
                        Option<String>,
                    ) -> db::AuthResult,
                >);
            }
            HookKind::UpdateHook => {
                conn.update_hook(None::<fn(db::UpdateAction, &str, &str, i64)>);
            }
            HookKind::CommitHook => {
                conn.commit_hook(None::<fn() -> bool>);
            }
            HookKind::RollbackHook => {
                conn.rollback_hook(None::<fn()>);
            }
            HookKind::WalHook => {
                conn.wal_hook(None::<fn(&str, i32) -> i32>);
            }
        }
        HOOK_OWNERS.with(|o| {
            o.borrow_mut().remove(&kind);
        });
    }

    pub(super) fn unregister_extension(ext_name: String) {
        let entries = REGISTRY.with(|r| r.borrow_mut().remove(&ext_name));
        let Some(entries) = entries else {
            return;
        };
        let rc = shared_conn();
        let conn = rc.borrow();
        for entry in entries {
            // Best-effort removal. SQLITE_ERROR (no such function /
            // collation) is benign here the connection may have
            // been reopened via spi.open-db between register and
            // unregister, in which case there's nothing to remove.
            match entry {
                HostRegistration::Scalar { name, num_args }
                | HostRegistration::Aggregate { name, num_args } => {
                    let _ = conn.remove_function(&name, num_args);
                }
                HostRegistration::Collation { name } => {
                    let _ = conn.remove_collation(&name);
                }
                HostRegistration::Hook { kind } => {
                    clear_hook_if_owner(&conn, kind, &ext_name);
                }
            }
        }
    }

    impl DispatchBridgeGuest for super::SqliteLib {
        fn bridged_execute(
            sql: String,
            params: Vec<SpiSqlValue>,
        ) -> Result<SpiQueryResult, SpiSqliteError> {
            <super::SqliteLib as SpiGuest>::execute(sql, params)
        }

        fn register_host_scalar(
            ext_name: String,
            name: String,
            num_args: i32,
            func_id: u64,
        ) -> Result<(), SpiSqliteError> {
            register_host_scalar(ext_name, name, num_args, func_id)
        }

        fn register_host_aggregate(
            ext_name: String,
            name: String,
            num_args: i32,
            func_id: u64,
            is_window: bool,
        ) -> Result<(), SpiSqliteError> {
            register_host_aggregate(ext_name, name, num_args, func_id, is_window)
        }

        fn register_host_collation(
            ext_name: String,
            name: String,
            collation_id: u64,
        ) -> Result<(), SpiSqliteError> {
            register_host_collation(ext_name, name, collation_id)
        }

        fn register_host_authorizer(ext_name: String) -> Result<(), SpiSqliteError> {
            register_host_authorizer(ext_name)
        }

        fn register_host_update_hook(ext_name: String) -> Result<(), SpiSqliteError> {
            register_host_update_hook(ext_name)
        }

        fn register_host_commit_hook(ext_name: String) -> Result<(), SpiSqliteError> {
            register_host_commit_hook(ext_name)
        }

        fn register_host_rollback_hook(ext_name: String) -> Result<(), SpiSqliteError> {
            register_host_rollback_hook(ext_name)
        }

        fn register_host_wal_hook(
            ext_name: String,
            hook_id: u64,
        ) -> Result<(), SpiSqliteError> {
            register_host_wal_hook(ext_name, hook_id)
        }

        fn register_host_vtab(
            ext_name: String,
            name: String,
            vtab_id: u64,
            eponymous: bool,
            mutable: bool,
            batched: bool,
        ) -> Result<(), SpiSqliteError> {
            super::host_vtabs::register_host_vtab(
                ext_name, name, vtab_id, eponymous, mutable, batched,
            )
        }

        fn unregister_extension(ext_name: String) {
            super::host_vtabs::unregister_host_vtabs(&ext_name);
            unregister_extension(ext_name);
        }
    }

    // `dispatch-bridge-cas` is the CAS-cache slice split out of
    // `dispatch-bridge`. Composed binary keeps serving the same SQL
    // surface against the in-WASM `shared_cas_conn`; native sqlink-
    // host has its own `impl dispatch_bridge_cas::Host` against
    // `Cache::with_bundles_conn`. Body mirrors the pre-split
    // `bridged_execute_cas` body verbatim — encoding shape matches
    // `SpiGuest::execute` for caller-side reuse of param/row
    // marshaling helpers.
    impl DispatchBridgeCasGuest for super::SqliteLib {
        fn bridged_execute_cas(
            sql: String,
            params: Vec<SpiSqlValue>,
        ) -> Result<SpiQueryResult, SpiSqliteError> {
            super::cas_with(|conn| {
                let mut stmt = conn.prepare(&sql).map_err(|e| super::spi_db_err(e.clone()))?;
                let columns = stmt.column_names();
                let dbs: Vec<super::db::Value> =
                    params.into_iter().map(super::spi_value_to_db).collect();
                stmt.bind_all(&dbs).map_err(|e| super::spi_db_err(e.clone()))?;
                let rows_vals = stmt
                    .collect_rows()
                    .map_err(|e| super::spi_db_err(e.clone()))?;
                let rows: Vec<Vec<SpiSqlValue>> = rows_vals
                    .into_iter()
                    .map(|r| r.into_iter().map(super::db_to_spi_value).collect())
                    .collect();
                Ok(SpiQueryResult {
                    columns,
                    rows,
                    changes: conn.changes(),
                    last_insert_rowid: conn.last_insert_rowid(),
                })
            })
        }
    }
}

bindings::export!(SqliteLib with_types_in bindings);
