//! Host-resident virtual-table modules on sqlite-lib's shared
//! connection.
//!
//! Mirror of `host_scalars` for vtabs. The composed browser scenario
//! is the driver: the JS host owns the loaded extension's `vtab` /
//! `vtab-update` exports, but `spi.execute` is served by sqlite-lib's
//! in-WASM connection — the JS host has no way to install an
//! `sqlite3_module` on that connection directly.
//!
//! `register_host_vtab` is the bridge. It installs a static
//! `sqlite3_module` (one of three flavours: read-only, eponymous,
//! mutable) on the shared connection via
//! `sqlite3_create_module_v2`, threading the `(ext-name, vtab-id)`
//! pair through `pAux`. Every xMethod trampoline re-enters the host
//! via the matching `dispatch.vtab-*` import.
//!
//! The shape mirrors `host/src/vtab.rs` (the wasmtime-side host)
//! almost verbatim — only the dispatch crossing layer differs.
//! Native: a `block_in_place` + `block_on` bridge around an async
//! wasmtime call into the loaded-extension's vtab export. Here:
//! a direct WIT import call that the JS host fields synchronously
//! and routes back to the transpiled extension instance.

use core::ffi::{c_char, c_int, c_void};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::ptr;

use libsqlite3_sys as ffi;

use crate::bindings::exports::sqlite::extension::spi::SqliteError as SpiSqliteError;
use crate::bindings::sqlink::wasm::dispatch as disp;
use crate::bindings::sqlite::extension::types::SqlValue as ImpSqlValue;
use crate::bindings::sqlite::extension::vtab as wv;
use crate::shared_conn;

// ─────────── ModuleAux ───────────
//
// Each `sqlite3_create_module_v2` call gets a heap-allocated
// ModuleAux as its `pAux`; xCreate / xConnect recover it via the
// `p_aux` argument. `x_destroy_aux` (passed as the destructor) frees
// it when the module is unregistered. Same shape as
// `host/src/vtab.rs`'s ModuleAux.
struct ModuleAux {
    ext_name: String,
    vtab_id: u64,
    eponymous: bool,
    batched: bool,
}

/// Instance handle stored alongside `sqlite3_vtab`'s base. Lets
/// every trampoline that takes a `*mut sqlite3_vtab` recover the
/// (ext-name, vtab-id, instance-id) triple it needs to dispatch.
#[repr(C)]
struct WasmVtab {
    base: ffi::sqlite3_vtab,
    instance_id: u64,
}

#[repr(C)]
struct WasmVtabCursor {
    base: ffi::sqlite3_vtab_cursor,
    cursor_id: u64,
}

#[derive(Clone)]
struct InstanceMeta {
    ext_name: String,
    vtab_id: u64,
    batched: bool,
}

#[derive(Clone)]
struct CursorMeta {
    ext_name: String,
    vtab_id: u64,
    batched: bool,
}

/// One cached row pulled in by `vtab-fetch-batch`. Stored in WIT
/// `sql-value` form so xColumn can route it through
/// `wit_to_sqlite3_result` without an extra conversion.
struct BatchRow {
    rowid: i64,
    columns: Vec<ImpSqlValue>,
}

#[derive(Default)]
struct BatchCache {
    rows: Vec<BatchRow>,
    idx: usize,
    eof_seen: bool,
}

/// Default fetch-batch size. Mirrors host/src/vtab.rs's BATCH_SIZE.
/// Amortizes the WIT crossing cost across enough rows to keep the
/// per-row marginal well below per-call overhead.
const BATCH_SIZE: u32 = 64;

thread_local! {
    /// Per-vtab-instance metadata, keyed by instance-id. xCreate /
    /// xConnect insert; xDisconnect / xDestroy remove.
    static INSTANCES: RefCell<HashMap<u64, InstanceMeta>> =
        RefCell::new(HashMap::new());

    /// Per-cursor metadata, keyed by cursor-id. xOpen inserts;
    /// xClose removes.
    static CURSORS: RefCell<HashMap<u64, CursorMeta>> =
        RefCell::new(HashMap::new());

    /// Per-cursor row cache for batched vtabs. xFilter primes;
    /// xNext refills when exhausted; xColumn / xRowid serve.
    static BATCH_CACHES: RefCell<HashMap<u64, BatchCache>> =
        RefCell::new(HashMap::new());

    /// Monotonic instance-id counter. Handed out by xCreate /
    /// xConnect; threaded through every per-instance dispatch.
    static NEXT_INSTANCE: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };

    /// Monotonic cursor-id counter. Handed out by xOpen; threaded
    /// through every per-cursor dispatch.
    static NEXT_CURSOR: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };

    /// Names of registered vtab modules keyed by ext-name. The
    /// caller-facing `host_scalars::unregister_extension` walks this
    /// to know which module names to drop on unload.
    pub(super) static MODULE_NAMES: RefCell<HashMap<String, Vec<String>>> =
        RefCell::new(HashMap::new());
}

fn alloc_instance_id(meta: InstanceMeta) -> u64 {
    let id = NEXT_INSTANCE.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    INSTANCES.with(|m| m.borrow_mut().insert(id, meta));
    id
}

fn alloc_cursor_id(meta: CursorMeta) -> u64 {
    let id = NEXT_CURSOR.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    CURSORS.with(|m| m.borrow_mut().insert(id, meta));
    id
}

fn instance_meta(id: u64) -> Option<InstanceMeta> {
    INSTANCES.with(|m| m.borrow().get(&id).cloned())
}

fn cursor_meta(id: u64) -> Option<CursorMeta> {
    CURSORS.with(|m| m.borrow().get(&id).cloned())
}

fn drop_instance(id: u64) {
    INSTANCES.with(|m| {
        m.borrow_mut().remove(&id);
    });
}

fn drop_cursor(id: u64) {
    CURSORS.with(|m| {
        m.borrow_mut().remove(&id);
    });
}

fn drop_batch_cache(cursor_id: u64) {
    BATCH_CACHES.with(|m| {
        m.borrow_mut().remove(&cursor_id);
    });
}

/// Pull a fresh block of rows from the extension and stash in the
/// per-cursor batch cache. Returns true if a non-empty batch
/// landed; false on empty (EOF) or error.
fn refill_batch(meta: &CursorMeta, cursor_id: u64) -> bool {
    let rows = match disp::vtab_fetch_batch(&meta.ext_name, meta.vtab_id, cursor_id, BATCH_SIZE) {
        Ok(r) => r,
        Err(_) => {
            BATCH_CACHES.with(|m| {
                let mut bc = m.borrow_mut();
                let entry = bc.entry(cursor_id).or_default();
                entry.eof_seen = true;
            });
            return false;
        }
    };
    BATCH_CACHES.with(|m| {
        let mut bc = m.borrow_mut();
        let entry = bc.entry(cursor_id).or_default();
        entry.idx = 0;
        if rows.is_empty() {
            entry.rows.clear();
            entry.eof_seen = true;
            false
        } else {
            entry.rows = rows
                .into_iter()
                .map(|r| BatchRow {
                    rowid: r.rowid,
                    columns: r.columns,
                })
                .collect();
            true
        }
    })
}

// ─────────── Marshaling helpers (C ↔ WIT) ───────────

unsafe fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

unsafe fn argv_to_strings(argc: c_int, argv: *const *const c_char) -> Vec<String> {
    let mut out = Vec::with_capacity(argc.max(0) as usize);
    for i in 0..argc {
        let p = *argv.add(i as usize);
        out.push(cstr_to_string(p));
    }
    out
}

unsafe fn set_err(p_err: *mut *mut c_char, msg: &str) {
    if p_err.is_null() {
        return;
    }
    let cs = match CString::new(msg) {
        Ok(c) => c,
        Err(_) => CString::new("vtab error (non-UTF8 message)").unwrap(),
    };
    let bytes = cs.as_bytes_with_nul();
    let buf = ffi::sqlite3_malloc(bytes.len() as c_int) as *mut c_char;
    if buf.is_null() {
        return;
    }
    ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, bytes.len());
    *p_err = buf;
}

fn map_constraint_op(op: u8) -> wv::ConstraintOp {
    match op as i32 {
        ffi::SQLITE_INDEX_CONSTRAINT_EQ => wv::ConstraintOp::Eq,
        ffi::SQLITE_INDEX_CONSTRAINT_GT => wv::ConstraintOp::Gt,
        ffi::SQLITE_INDEX_CONSTRAINT_LE => wv::ConstraintOp::Le,
        ffi::SQLITE_INDEX_CONSTRAINT_LT => wv::ConstraintOp::Lt,
        ffi::SQLITE_INDEX_CONSTRAINT_GE => wv::ConstraintOp::Ge,
        ffi::SQLITE_INDEX_CONSTRAINT_NE => wv::ConstraintOp::Ne,
        ffi::SQLITE_INDEX_CONSTRAINT_MATCH => wv::ConstraintOp::Match,
        ffi::SQLITE_INDEX_CONSTRAINT_LIKE => wv::ConstraintOp::Like,
        ffi::SQLITE_INDEX_CONSTRAINT_REGEXP => wv::ConstraintOp::Regexp,
        ffi::SQLITE_INDEX_CONSTRAINT_GLOB => wv::ConstraintOp::Glob,
        ffi::SQLITE_INDEX_CONSTRAINT_ISNULL => wv::ConstraintOp::IsNull,
        ffi::SQLITE_INDEX_CONSTRAINT_ISNOTNULL => wv::ConstraintOp::IsNotNull,
        ffi::SQLITE_INDEX_CONSTRAINT_LIMIT => wv::ConstraintOp::Limit,
        ffi::SQLITE_INDEX_CONSTRAINT_OFFSET => wv::ConstraintOp::Offset,
        ffi::SQLITE_INDEX_CONSTRAINT_FUNCTION => wv::ConstraintOp::Function,
        // Unknown op  fall back to Function. The guest's
        // best_index can report unsupported via its plan shape.
        _ => wv::ConstraintOp::Function,
    }
}

unsafe fn sqlite3_value_to_wit(v: *mut ffi::sqlite3_value) -> ImpSqlValue {
    let ty = ffi::sqlite3_value_type(v);
    match ty {
        ffi::SQLITE_INTEGER => ImpSqlValue::Integer(ffi::sqlite3_value_int64(v)),
        ffi::SQLITE_FLOAT => ImpSqlValue::Real(ffi::sqlite3_value_double(v)),
        ffi::SQLITE_TEXT => {
            let p = ffi::sqlite3_value_text(v);
            let n = ffi::sqlite3_value_bytes(v) as usize;
            let bytes = std::slice::from_raw_parts(p, n);
            ImpSqlValue::Text(String::from_utf8_lossy(bytes).into_owned())
        }
        ffi::SQLITE_BLOB => {
            let p = ffi::sqlite3_value_blob(v) as *const u8;
            let n = ffi::sqlite3_value_bytes(v) as usize;
            let bytes = std::slice::from_raw_parts(p, n);
            ImpSqlValue::Blob(bytes.to_vec())
        }
        _ => ImpSqlValue::Null,
    }
}

unsafe fn wit_to_sqlite3_result(ctx: *mut ffi::sqlite3_context, v: ImpSqlValue) {
    match v {
        ImpSqlValue::Null => ffi::sqlite3_result_null(ctx),
        ImpSqlValue::Integer(i) => ffi::sqlite3_result_int64(ctx, i),
        ImpSqlValue::Real(r) => ffi::sqlite3_result_double(ctx, r),
        ImpSqlValue::Text(s) => {
            let bytes = s.as_bytes();
            ffi::sqlite3_result_text(
                ctx,
                bytes.as_ptr() as *const c_char,
                bytes.len() as c_int,
                ffi::SQLITE_TRANSIENT(),
            );
        }
        ImpSqlValue::Blob(b) => {
            ffi::sqlite3_result_blob(
                ctx,
                b.as_ptr() as *const c_void,
                b.len() as c_int,
                ffi::SQLITE_TRANSIENT(),
            );
        }
        // @1.0.0 wit-value arm. The SQLite C surface has no typed-
        // record equivalent; at this result boundary the payload
        // flattens to a BLOB carrying its canonical-CBOR bytes (the
        // type-id + symbolic name don't survive a SQLite column
        // store, by design — see db::Value's contract).
        ImpSqlValue::WitValue(p) => {
            ffi::sqlite3_result_blob(
                ctx,
                p.bytes.as_ptr() as *const c_void,
                p.bytes.len() as c_int,
                ffi::SQLITE_TRANSIENT(),
            );
        }
    }
}

// ─────────── xMethod trampolines ───────────

unsafe extern "C" fn x_create(
    db: *mut ffi::sqlite3,
    p_aux: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
    pp_vtab: *mut *mut ffi::sqlite3_vtab,
    p_err: *mut *mut c_char,
) -> c_int {
    create_or_connect(db, p_aux, argc, argv, pp_vtab, p_err, false)
}

unsafe extern "C" fn x_connect(
    db: *mut ffi::sqlite3,
    p_aux: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
    pp_vtab: *mut *mut ffi::sqlite3_vtab,
    p_err: *mut *mut c_char,
) -> c_int {
    create_or_connect(db, p_aux, argc, argv, pp_vtab, p_err, true)
}

unsafe fn create_or_connect(
    db: *mut ffi::sqlite3,
    p_aux: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
    pp_vtab: *mut *mut ffi::sqlite3_vtab,
    p_err: *mut *mut c_char,
    is_connect: bool,
) -> c_int {
    let aux = &*(p_aux as *const ModuleAux);
    let args = argv_to_strings(argc, argv);
    // SQLite's argv layout: [0]=module name, [1]=database name,
    // [2]=table name, [3..]=user-supplied args.
    let db_name = args.get(1).cloned().unwrap_or_default();
    let table_name = args.get(2).cloned().unwrap_or_default();
    let user_args: Vec<String> = args.into_iter().skip(3).collect();

    let instance_id = alloc_instance_id(InstanceMeta {
        ext_name: aux.ext_name.clone(),
        vtab_id: aux.vtab_id,
        batched: aux.batched,
    });

    let result = if is_connect || aux.eponymous {
        disp::vtab_connect(
            &aux.ext_name,
            aux.vtab_id,
            instance_id,
            &db_name,
            &table_name,
            &user_args,
        )
    } else {
        disp::vtab_create(
            &aux.ext_name,
            aux.vtab_id,
            instance_id,
            &db_name,
            &table_name,
            &user_args,
        )
    };
    let schema = match result {
        Ok(s) => s,
        Err(e) => {
            drop_instance(instance_id);
            set_err(p_err, &e);
            return ffi::SQLITE_ERROR;
        }
    };

    let schema_c = match CString::new(schema) {
        Ok(c) => c,
        Err(_) => {
            drop_instance(instance_id);
            set_err(p_err, "vtab schema contained NUL");
            return ffi::SQLITE_ERROR;
        }
    };
    let rc = ffi::sqlite3_declare_vtab(db, schema_c.as_ptr());
    if rc != ffi::SQLITE_OK {
        drop_instance(instance_id);
        return rc;
    }

    let vtab = Box::new(WasmVtab {
        base: ffi::sqlite3_vtab {
            pModule: ptr::null(),
            nRef: 0,
            zErrMsg: ptr::null_mut(),
        },
        instance_id,
    });
    *pp_vtab = Box::into_raw(vtab) as *mut ffi::sqlite3_vtab;
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_disconnect(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    let wv = Box::from_raw(p_vtab as *mut WasmVtab);
    if let Some(meta) = instance_meta(wv.instance_id) {
        let _ = disp::vtab_disconnect(&meta.ext_name, meta.vtab_id, wv.instance_id);
    }
    drop_instance(wv.instance_id);
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_destroy(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    let wv = Box::from_raw(p_vtab as *mut WasmVtab);
    if let Some(meta) = instance_meta(wv.instance_id) {
        let _ = disp::vtab_destroy(&meta.ext_name, meta.vtab_id, wv.instance_id);
    }
    drop_instance(wv.instance_id);
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_best_index(
    p_vtab: *mut ffi::sqlite3_vtab,
    p_info: *mut ffi::sqlite3_index_info,
) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    let info = &mut *p_info;

    let constraints: Vec<wv::Constraint> = (0..info.nConstraint as usize)
        .map(|i| {
            let c = info.aConstraint.add(i);
            wv::Constraint {
                column: (*c).iColumn,
                op: map_constraint_op((*c).op),
                usable: (*c).usable != 0,
            }
        })
        .collect();
    let orderbys: Vec<wv::Orderby> = (0..info.nOrderBy as usize)
        .map(|i| {
            let o = info.aOrderBy.add(i);
            wv::Orderby {
                column: (*o).iColumn,
                desc: (*o).desc != 0,
            }
        })
        .collect();
    let wit_info = wv::IndexInfo {
        constraints,
        orderbys,
        col_used: info.colUsed,
    };

    let plan = match disp::vtab_best_index(&meta.ext_name, meta.vtab_id, wv.instance_id, &wit_info) {
        Ok(p) => p,
        Err(e) => {
            let msg = CString::new(e).unwrap_or_else(|_| CString::new("best_index").unwrap());
            let bytes = msg.as_bytes_with_nul();
            let buf = ffi::sqlite3_malloc(bytes.len() as c_int) as *mut c_char;
            if !buf.is_null() {
                ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, bytes.len());
                (*p_vtab).zErrMsg = buf;
            }
            return ffi::SQLITE_ERROR;
        }
    };

    for (i, usage) in plan.constraint_usage.iter().enumerate() {
        if i >= info.nConstraint as usize {
            break;
        }
        let u = info.aConstraintUsage.add(i);
        (*u).argvIndex = usage.argv_index;
        (*u).omit = if usage.omit { 1 } else { 0 };
    }
    info.idxNum = plan.idx_num;
    if let Some(s) = plan.idx_str {
        if let Ok(c) = CString::new(s) {
            let bytes = c.as_bytes_with_nul();
            let buf = ffi::sqlite3_malloc(bytes.len() as c_int) as *mut c_char;
            if !buf.is_null() {
                ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, bytes.len());
                info.idxStr = buf;
                info.needToFreeIdxStr = 1;
            }
        }
    }
    info.estimatedCost = plan.estimated_cost;
    info.estimatedRows = plan.estimated_rows;
    info.orderByConsumed = if plan.orderby_consumed { 1 } else { 0 };
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_open(
    p_vtab: *mut ffi::sqlite3_vtab,
    pp_cursor: *mut *mut ffi::sqlite3_vtab_cursor,
) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    let cursor_id = alloc_cursor_id(CursorMeta {
        ext_name: meta.ext_name.clone(),
        vtab_id: meta.vtab_id,
        batched: meta.batched,
    });
    if let Err(_e) = disp::vtab_open(&meta.ext_name, meta.vtab_id, wv.instance_id, cursor_id) {
        drop_cursor(cursor_id);
        return ffi::SQLITE_ERROR;
    }
    let cursor = Box::new(WasmVtabCursor {
        base: ffi::sqlite3_vtab_cursor { pVtab: p_vtab },
        cursor_id,
    });
    *pp_cursor = Box::into_raw(cursor) as *mut ffi::sqlite3_vtab_cursor;
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_close(p_cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    let c = Box::from_raw(p_cursor as *mut WasmVtabCursor);
    if let Some(meta) = cursor_meta(c.cursor_id) {
        let _ = disp::vtab_close(&meta.ext_name, meta.vtab_id, c.cursor_id);
    }
    drop_batch_cache(c.cursor_id);
    drop_cursor(c.cursor_id);
    ffi::SQLITE_OK
}

unsafe extern "C" fn x_filter(
    p_cursor: *mut ffi::sqlite3_vtab_cursor,
    idx_num: c_int,
    idx_str: *const c_char,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) -> c_int {
    let c = &*(p_cursor as *mut WasmVtabCursor);
    let meta = match cursor_meta(c.cursor_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    let idx_str_owned = if idx_str.is_null() {
        None
    } else {
        Some(cstr_to_string(idx_str))
    };
    let mut args = Vec::with_capacity(argc as usize);
    for i in 0..argc as usize {
        args.push(sqlite3_value_to_wit(*argv.add(i)));
    }
    match disp::vtab_filter(
        &meta.ext_name,
        meta.vtab_id,
        c.cursor_id,
        idx_num,
        idx_str_owned.as_deref(),
        &args,
    ) {
        Ok(()) => {
            if meta.batched {
                BATCH_CACHES.with(|m| {
                    let mut bc = m.borrow_mut();
                    let entry = bc.entry(c.cursor_id).or_default();
                    entry.rows.clear();
                    entry.idx = 0;
                    entry.eof_seen = false;
                });
                let _ = refill_batch(&meta, c.cursor_id);
            }
            ffi::SQLITE_OK
        }
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_next(p_cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    let c = &*(p_cursor as *mut WasmVtabCursor);
    let meta = match cursor_meta(c.cursor_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    if meta.batched {
        let need_refill = BATCH_CACHES.with(|m| {
            let mut bc = m.borrow_mut();
            let entry = bc.entry(c.cursor_id).or_default();
            entry.idx += 1;
            entry.idx >= entry.rows.len() && !entry.eof_seen
        });
        if need_refill {
            let _ = refill_batch(&meta, c.cursor_id);
        }
        return ffi::SQLITE_OK;
    }
    match disp::vtab_next(&meta.ext_name, meta.vtab_id, c.cursor_id) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_eof(p_cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    let c = &*(p_cursor as *mut WasmVtabCursor);
    let meta = match cursor_meta(c.cursor_id) {
        Some(m) => m,
        None => return 1,
    };
    if meta.batched {
        return BATCH_CACHES.with(|m| {
            let bc = m.borrow();
            match bc.get(&c.cursor_id) {
                None => 1,
                Some(entry) => {
                    if entry.idx < entry.rows.len() {
                        0
                    } else if entry.eof_seen {
                        1
                    } else {
                        // Cache exhausted but EOF not yet seen.
                        // Returning 1 here would terminate the scan
                        // prematurely; return not-EOF so xNext gets
                        // a chance to refill on the next iteration.
                        0
                    }
                }
            }
        });
    }
    if disp::vtab_eof(&meta.ext_name, meta.vtab_id, c.cursor_id) {
        1
    } else {
        0
    }
}

unsafe extern "C" fn x_column(
    p_cursor: *mut ffi::sqlite3_vtab_cursor,
    ctx: *mut ffi::sqlite3_context,
    col: c_int,
) -> c_int {
    let c = &*(p_cursor as *mut WasmVtabCursor);
    let meta = match cursor_meta(c.cursor_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    if meta.batched {
        let cached = BATCH_CACHES.with(|m| {
            let bc = m.borrow();
            bc.get(&c.cursor_id).and_then(|entry| {
                entry
                    .rows
                    .get(entry.idx)
                    .map(|row| (row.columns.get(col as usize).cloned(), true))
            })
        });
        match cached {
            Some((Some(v), _)) => {
                wit_to_sqlite3_result(ctx, v);
                ffi::SQLITE_OK
            }
            Some((None, _)) => {
                // Column out of range  return NULL (matches
                // sqlite's behavior for HIDDEN columns past the
                // explicit schema).
                ffi::sqlite3_result_null(ctx);
                ffi::SQLITE_OK
            }
            None => ffi::SQLITE_ERROR,
        }
    } else {
        match disp::vtab_column(&meta.ext_name, meta.vtab_id, c.cursor_id, col) {
            Ok(v) => {
                wit_to_sqlite3_result(ctx, v);
                ffi::SQLITE_OK
            }
            Err(_) => ffi::SQLITE_ERROR,
        }
    }
}

unsafe extern "C" fn x_rowid(
    p_cursor: *mut ffi::sqlite3_vtab_cursor,
    p_rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    let c = &*(p_cursor as *mut WasmVtabCursor);
    let meta = match cursor_meta(c.cursor_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    if meta.batched {
        return BATCH_CACHES.with(|m| {
            let bc = m.borrow();
            match bc.get(&c.cursor_id) {
                Some(entry) => match entry.rows.get(entry.idx) {
                    Some(row) => {
                        *p_rowid = row.rowid;
                        ffi::SQLITE_OK
                    }
                    None => ffi::SQLITE_ERROR,
                },
                None => ffi::SQLITE_ERROR,
            }
        });
    }
    match disp::vtab_rowid(&meta.ext_name, meta.vtab_id, c.cursor_id) {
        Ok(r) => {
            *p_rowid = r;
            ffi::SQLITE_OK
        }
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_destroy_aux(p: *mut c_void) {
    if !p.is_null() {
        drop(Box::from_raw(p as *mut ModuleAux));
    }
}

// ─────────── Mutating trampolines (iVersion 2 module) ───────────

unsafe extern "C" fn x_update(
    p_vtab: *mut ffi::sqlite3_vtab,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
    p_rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    let mut args = Vec::with_capacity(argc as usize);
    for i in 0..argc as usize {
        args.push(sqlite3_value_to_wit(*argv.add(i)));
    }
    match disp::vtab_update(&meta.ext_name, meta.vtab_id, wv.instance_id, &args) {
        Ok(rowid) => {
            *p_rowid = rowid;
            ffi::SQLITE_OK
        }
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_begin(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    match disp::vtab_begin(&meta.ext_name, meta.vtab_id, wv.instance_id) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_sync(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    match disp::vtab_sync(&meta.ext_name, meta.vtab_id, wv.instance_id) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_commit(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    match disp::vtab_commit(&meta.ext_name, meta.vtab_id, wv.instance_id) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_rollback(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    match disp::vtab_rollback(&meta.ext_name, meta.vtab_id, wv.instance_id) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_rename(p_vtab: *mut ffi::sqlite3_vtab, z_new: *const c_char) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    let new_name = cstr_to_string(z_new);
    match disp::vtab_rename(&meta.ext_name, meta.vtab_id, wv.instance_id, &new_name) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_savepoint(p_vtab: *mut ffi::sqlite3_vtab, sp: c_int) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    match disp::vtab_savepoint(&meta.ext_name, meta.vtab_id, wv.instance_id, sp) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_release(p_vtab: *mut ffi::sqlite3_vtab, sp: c_int) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    match disp::vtab_release(&meta.ext_name, meta.vtab_id, wv.instance_id, sp) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn x_rollback_to(p_vtab: *mut ffi::sqlite3_vtab, sp: c_int) -> c_int {
    let wv = &*(p_vtab as *mut WasmVtab);
    let meta = match instance_meta(wv.instance_id) {
        Some(m) => m,
        None => return ffi::SQLITE_INTERNAL,
    };
    match disp::vtab_rollback_to(&meta.ext_name, meta.vtab_id, wv.instance_id, sp) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

// ─────────── Module templates ───────────

const MODULE: ffi::sqlite3_module = ffi::sqlite3_module {
    iVersion: 1,
    xCreate: Some(x_create),
    xConnect: Some(x_connect),
    xBestIndex: Some(x_best_index),
    xDisconnect: Some(x_disconnect),
    xDestroy: Some(x_destroy),
    xOpen: Some(x_open),
    xClose: Some(x_close),
    xFilter: Some(x_filter),
    xNext: Some(x_next),
    xEof: Some(x_eof),
    xColumn: Some(x_column),
    xRowid: Some(x_rowid),
    xUpdate: None,
    xBegin: None,
    xSync: None,
    xCommit: None,
    xRollback: None,
    xFindFunction: None,
    xRename: None,
    xSavepoint: None,
    xRelease: None,
    xRollbackTo: None,
    xShadowName: None,
    xIntegrity: None,
};

/// Eponymous variant: xCreate = NULL. SQLite treats this as a
/// table-valued function the module name itself is usable in FROM
/// without a prior `CREATE VIRTUAL TABLE`.
const MODULE_EPONYMOUS: ffi::sqlite3_module = ffi::sqlite3_module {
    iVersion: 1,
    xCreate: None,
    xConnect: Some(x_connect),
    xBestIndex: Some(x_best_index),
    xDisconnect: Some(x_disconnect),
    xDestroy: Some(x_destroy),
    xOpen: Some(x_open),
    xClose: Some(x_close),
    xFilter: Some(x_filter),
    xNext: Some(x_next),
    xEof: Some(x_eof),
    xColumn: Some(x_column),
    xRowid: Some(x_rowid),
    xUpdate: None,
    xBegin: None,
    xSync: None,
    xCommit: None,
    xRollback: None,
    xFindFunction: None,
    xRename: None,
    xSavepoint: None,
    xRelease: None,
    xRollbackTo: None,
    xShadowName: None,
    xIntegrity: None,
};

/// Mutable variant: iVersion=2, xUpdate + the transactional and
/// savepoint slots populated. xShadowName / xIntegrity are kept
/// NULL for v1 — they are rarely-exercised optional slots and
/// adding them is a clean follow-up if a real consumer needs them.
const MODULE_MUTABLE: ffi::sqlite3_module = ffi::sqlite3_module {
    iVersion: 2,
    xCreate: Some(x_create),
    xConnect: Some(x_connect),
    xBestIndex: Some(x_best_index),
    xDisconnect: Some(x_disconnect),
    xDestroy: Some(x_destroy),
    xOpen: Some(x_open),
    xClose: Some(x_close),
    xFilter: Some(x_filter),
    xNext: Some(x_next),
    xEof: Some(x_eof),
    xColumn: Some(x_column),
    xRowid: Some(x_rowid),
    xUpdate: Some(x_update),
    xBegin: Some(x_begin),
    xSync: Some(x_sync),
    xCommit: Some(x_commit),
    xRollback: Some(x_rollback),
    xFindFunction: None,
    xRename: Some(x_rename),
    xSavepoint: Some(x_savepoint),
    xRelease: Some(x_release),
    xRollbackTo: Some(x_rollback_to),
    xShadowName: None,
    xIntegrity: None,
};

// ─────────── Public entry points ───────────

/// Trampoline-side helper to produce the wit-side sqlite-error
/// shape from a free-form message.
fn err_sqlite(msg: impl Into<String>) -> SpiSqliteError {
    SpiSqliteError {
        code: libsqlite3_sys::SQLITE_MISUSE,
        extended_code: libsqlite3_sys::SQLITE_MISUSE,
        message: msg.into(),
    }
}

/// Install a vtab module on sqlite-lib's shared connection. See the
/// `register-host-vtab` WIT comment for full semantics. Stage B of
/// the composed browser vtab-dispatch surface.
pub(super) fn register_host_vtab(
    ext_name: String,
    name: String,
    vtab_id: u64,
    eponymous: bool,
    mutable: bool,
    batched: bool,
) -> Result<(), SpiSqliteError> {
    if name.is_empty() {
        return Err(err_sqlite("register-host-vtab: empty module name"));
    }
    if ext_name.is_empty() {
        return Err(err_sqlite("register-host-vtab: empty extension name"));
    }

    // Heap-allocated, raw-ptr-borrowed aux. sqlite3_create_module_v2
    // owns it after success (drops via x_destroy_aux on
    // unregistration). On failure we reclaim the Box ourselves.
    let aux = Box::into_raw(Box::new(ModuleAux {
        ext_name: ext_name.clone(),
        vtab_id,
        eponymous,
        batched,
    })) as *mut c_void;
    let name_c = CString::new(name.as_str())
        .map_err(|e| err_sqlite(format!("register-host-vtab: bad name: {e}")))?;

    // Mutable always wins over eponymous if both are flagged —
    // eponymous mutable vtabs aren't a known shape; pick the
    // mutable template, which is the more conservative choice.
    let module_ptr: *const ffi::sqlite3_module = if mutable {
        &MODULE_MUTABLE
    } else if eponymous {
        &MODULE_EPONYMOUS
    } else {
        &MODULE
    };

    let rc = shared_conn();
    let conn = rc.borrow();
    let db = conn.raw_handle();
    // SAFETY: db is a live sqlite3*; aux is a raw pointer to a
    // freshly-leaked Box that x_destroy_aux will reclaim;
    // module_ptr is a 'static reference to one of the three
    // const module templates above.
    let rc = unsafe {
        ffi::sqlite3_create_module_v2(db, name_c.as_ptr(), module_ptr, aux, Some(x_destroy_aux))
    };
    drop(conn);
    if rc != ffi::SQLITE_OK {
        // sqlite3_create_module_v2 documents that on failure the
        // destructor is NOT invoked; reclaim the Box.
        unsafe {
            drop(Box::from_raw(aux as *mut ModuleAux));
        }
        return Err(SpiSqliteError {
            code: rc,
            extended_code: rc,
            message: format!("sqlite3_create_module_v2 failed: rc={rc}"),
        });
    }

    MODULE_NAMES.with(|m| {
        m.borrow_mut().entry(ext_name).or_default().push(name);
    });
    Ok(())
}

/// Drop every vtab module registered under `ext_name` by overriding
/// each one with a NULL module of the same name. SQLite has no
/// first-class "remove module" API; a null re-registration disables
/// the previous entry. Mirror of `host/src/vtab.rs::
/// unregister_vtab_module`.
pub(super) fn unregister_host_vtabs(ext_name: &str) {
    let names = MODULE_NAMES.with(|m| m.borrow_mut().remove(ext_name));
    let Some(names) = names else {
        return;
    };
    let rc = shared_conn();
    let conn = rc.borrow();
    let db = conn.raw_handle();
    for name in names {
        let Ok(name_c) = CString::new(name) else {
            continue;
        };
        unsafe {
            // Return value ignored: best-effort cleanup. If the
            // connection has already been replaced (e.g. by spi.
            // open-db) there's nothing to detach; SQLITE_MISUSE is
            // benign.
            let _ = ffi::sqlite3_create_module_v2(
                db,
                name_c.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                None,
            );
        }
    }
}
