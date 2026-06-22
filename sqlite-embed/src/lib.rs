//! Centralized FFI glue for embedded sqlink extensions.
//!
//! Each extension's `src/embed.rs` becomes a tiny module: a
//! `call_scalar(fid, args)` body (reuses the WIT path's logic)
//! plus a static `ScalarSpec` table naming the surface. This crate
//! provides:
//!
//!   * `SqlValueOwned`  the canonical arg/result type, identical
//!     in shape to the WIT-generated `bindings::SqlValue` but
//!     defined here so we don't pull wit-bindgen into the embed
//!     path.
//!   * `register_scalars(db, &[ScalarSpec], call_fn)`  registers
//!     all scalars in one call via `sqlite3_create_function_v2`,
//!     using `sqlite3_user_data` to thread (call_fn, func_id) into
//!     a single generic thunk. One thunk for every embedded
//!     extension, every scalar  no per-fn boilerplate per
//!     extension.
//!
//! Extensions opt in by declaring an `embed` cargo feature that
//! depends on `sqlite-embed` and `libsqlite3-sys`. See
//! PLAN-embed-extensions.md for the full contract.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_int, c_void};
use core::ptr;
use libsqlite3_sys as ffi;

/// SQLite scalar function flag values. Defined here as constants
/// because libsqlite3-sys gates them behind feature flags we don't
/// pull in.
const SQLITE_DETERMINISTIC: c_int = 0x000000800;
const SQLITE_UTF8: c_int = 1;

/// Owned analog of the wit-bindgen-generated `SqlValue` enum.
/// Same shape; defined here so embed-path crates don't depend on
/// wit-bindgen.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValueOwned {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// Per-scalar declaration the extension hands us. `name` MUST be
/// nul-terminated  the crate keeps the ASCII bytes around for
/// SQLite's lifetime via 'static.
pub struct ScalarSpec {
    pub func_id: u64,
    pub name: &'static [u8],
    pub num_args: i32,
    pub deterministic: bool,
}

/// Function signature the extension exposes. Same shape as the
/// WIT-generated `ScalarFunctionGuest::call`  most extensions can
/// just delegate.
pub type CallFn =
    fn(func_id: u64, args: Vec<SqlValueOwned>) -> Result<SqlValueOwned, String>;

/// Per-registration context  threaded through `sqlite3_user_data`
/// so the generic thunk knows which (extension, func_id) to invoke.
struct DispatchCtx {
    call_fn: CallFn,
    func_id: u64,
}

unsafe extern "C" fn destroy_dispatch_ctx(p: *mut c_void) {
    // Reclaim the Box we leaked in register_scalars.
    drop(alloc::boxed::Box::from_raw(p as *mut DispatchCtx));
}

/// The one generic thunk every embedded scalar registers as its
/// xFunc. Pulls the dispatch context out of sqlite3_user_data,
/// converts argv to `Vec<SqlValueOwned>`, calls the extension's
/// CallFn, writes the result (or sets an error).
unsafe extern "C" fn generic_thunk(
    ctx: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    let disp = ffi::sqlite3_user_data(ctx) as *const DispatchCtx;
    if disp.is_null() {
        ffi::sqlite3_result_null(ctx);
        return;
    }
    let call_fn = (*disp).call_fn;
    let func_id = (*disp).func_id;
    let args = collect_args(argc, argv);
    match call_fn(func_id, args) {
        Ok(v) => set_result(ctx, v),
        Err(e) => set_error(ctx, &e),
    }
}

unsafe fn collect_args(
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) -> Vec<SqlValueOwned> {
    if argc <= 0 || argv.is_null() {
        return Vec::new();
    }
    let slice = core::slice::from_raw_parts(argv, argc as usize);
    // Reuse a static capacity hint based on the last call's argc.
    // Saves the allocator at least the realloc on extensions that
    // always pass the same arity (the common case). The first call
    // pays a normal Vec::with_capacity; subsequent calls of the
    // same shape grow in-place because the existing capacity
    // matches what's about to be pushed.
    let cap = (*ARGS_CAPACITY_HINT.get()).max(argc as usize);
    let mut out = Vec::with_capacity(cap);
    for &v in slice {
        out.push(value_to_owned(v));
    }
    *ARGS_CAPACITY_HINT.get() = argc as usize;
    out
}

/// Single-threaded wasm; a static cell is safe and avoids the
/// thread_local! macro (which pulls in `std`). Tracks the largest
/// argc seen so subsequent allocations don't grow incrementally.
struct ArgsCapacityHintCell {
    inner: core::cell::UnsafeCell<usize>,
}
unsafe impl Sync for ArgsCapacityHintCell {}
impl ArgsCapacityHintCell {
    const fn new() -> Self {
        Self { inner: core::cell::UnsafeCell::new(0) }
    }
    fn get(&self) -> *mut usize {
        self.inner.get()
    }
}
static ARGS_CAPACITY_HINT: ArgsCapacityHintCell = ArgsCapacityHintCell::new();

unsafe fn value_to_owned(v: *mut ffi::sqlite3_value) -> SqlValueOwned {
    match ffi::sqlite3_value_type(v) {
        ffi::SQLITE_NULL => SqlValueOwned::Null,
        ffi::SQLITE_INTEGER => SqlValueOwned::Integer(ffi::sqlite3_value_int64(v)),
        ffi::SQLITE_FLOAT => SqlValueOwned::Real(ffi::sqlite3_value_double(v)),
        ffi::SQLITE_TEXT => {
            let p = ffi::sqlite3_value_text(v);
            if p.is_null() {
                return SqlValueOwned::Text(String::new());
            }
            let n = ffi::sqlite3_value_bytes(v) as usize;
            let bytes = core::slice::from_raw_parts(p, n);
            // SQLite's TEXT is UTF-8 by the time it's stored; if
            // not (rare), substitute the empty string rather than
            // panicking on the embed boundary.
            match core::str::from_utf8(bytes) {
                Ok(s) => SqlValueOwned::Text(s.into()),
                Err(_) => SqlValueOwned::Text(String::new()),
            }
        }
        ffi::SQLITE_BLOB => {
            let p = ffi::sqlite3_value_blob(v) as *const u8;
            if p.is_null() {
                return SqlValueOwned::Blob(Vec::new());
            }
            let n = ffi::sqlite3_value_bytes(v) as usize;
            SqlValueOwned::Blob(core::slice::from_raw_parts(p, n).to_vec())
        }
        _ => SqlValueOwned::Null,
    }
}

unsafe fn set_result(ctx: *mut ffi::sqlite3_context, v: SqlValueOwned) {
    match v {
        SqlValueOwned::Null => ffi::sqlite3_result_null(ctx),
        SqlValueOwned::Integer(n) => ffi::sqlite3_result_int64(ctx, n),
        SqlValueOwned::Real(r) => ffi::sqlite3_result_double(ctx, r),
        SqlValueOwned::Text(s) => {
            // SQLITE_TRANSIENT copies the bytes  the String can drop.
            // (We tried a custom-destructor zero-copy variant; sqlite
            // hands the destructor the data ptr, not the String
            // struct ptr, so reconstructing Box<String> would be UB.
            // The transient copy is one memcpy through sqlite3_malloc
            // which is already going through our pcache allocator.)
            ffi::sqlite3_result_text(
                ctx,
                s.as_ptr() as *const _,
                s.len() as c_int,
                ffi::SQLITE_TRANSIENT(),
            );
        }
        SqlValueOwned::Blob(b) => {
            ffi::sqlite3_result_blob(
                ctx,
                b.as_ptr() as *const _,
                b.len() as c_int,
                ffi::SQLITE_TRANSIENT(),
            );
        }
    }
}

unsafe fn set_error(ctx: *mut ffi::sqlite3_context, msg: &str) {
    ffi::sqlite3_result_error(
        ctx,
        msg.as_ptr() as *const _,
        msg.len() as c_int,
    );
}

/// Function signature for db-aware scalars  receives the live
/// `sqlite3*` so the implementation can prepare/exec sub-SQL
/// inside the same connection (define, vec0 read-paths, etc).
pub type CallFnWithDb = fn(
    db: *mut ffi::sqlite3,
    func_id: u64,
    args: Vec<SqlValueOwned>,
) -> Result<SqlValueOwned, String>;

struct DbDispatchCtx {
    call_fn: CallFnWithDb,
    func_id: u64,
}

unsafe extern "C" fn destroy_db_dispatch_ctx(p: *mut c_void) {
    drop(alloc::boxed::Box::from_raw(p as *mut DbDispatchCtx));
}

unsafe extern "C" fn db_aware_thunk(
    ctx: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    let disp = ffi::sqlite3_user_data(ctx) as *const DbDispatchCtx;
    if disp.is_null() {
        ffi::sqlite3_result_null(ctx);
        return;
    }
    let call_fn = (*disp).call_fn;
    let func_id = (*disp).func_id;
    let db = ffi::sqlite3_context_db_handle(ctx);
    let args = collect_args(argc, argv);
    match call_fn(db, func_id, args) {
        Ok(v) => set_result(ctx, v),
        Err(e) => set_error(ctx, &e),
    }
}

/// Same shape as `register_scalars` but threads the live `sqlite3*`
/// into each call. Used by extensions whose scalar bodies need to
/// prepare/exec sub-SQL (define's _define_funcs lookups, vec0 read
/// paths that pull from shadow rows, etc).
pub unsafe fn register_scalars_with_db(
    db: *mut ffi::sqlite3,
    specs: &[ScalarSpec],
    call_fn: CallFnWithDb,
) -> c_int {
    for spec in specs {
        let ctx = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(DbDispatchCtx {
            call_fn,
            func_id: spec.func_id,
        }));
        let flags = SQLITE_UTF8
            | if spec.deterministic {
                SQLITE_DETERMINISTIC
            } else {
                0
            };
        let rc = ffi::sqlite3_create_function_v2(
            db,
            spec.name.as_ptr() as *const _,
            spec.num_args,
            flags,
            ctx as *mut c_void,
            Some(db_aware_thunk),
            None,
            None,
            Some(destroy_db_dispatch_ctx),
        );
        if rc != ffi::SQLITE_OK {
            return rc;
        }
    }
    ffi::SQLITE_OK
}

/// Register every scalar in `specs` against `db`, with each
/// dispatch routed through `call_fn`. Returns SQLITE_OK or the
/// first non-OK code.
///
/// One generic thunk handles every dispatch; per-scalar context
/// is threaded through `sqlite3_user_data`. The boxed
/// `DispatchCtx` is freed by SQLite via the destroy callback when
/// the function is replaced or the connection closes.
///
/// Safety: `db` must be a live `sqlite3*` from `sqlite3_open_v2`
/// (or equivalent) and not yet closed.
pub unsafe fn register_scalars(
    db: *mut ffi::sqlite3,
    specs: &[ScalarSpec],
    call_fn: CallFn,
) -> c_int {
    for spec in specs {
        // Leak via Box::into_raw; SQLite frees it via
        // destroy_dispatch_ctx when the function is replaced or
        // the connection closes.
        let ctx = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(DispatchCtx {
            call_fn,
            func_id: spec.func_id,
        }));
        let flags = SQLITE_UTF8
            | if spec.deterministic {
                SQLITE_DETERMINISTIC
            } else {
                0
            };
        let rc = ffi::sqlite3_create_function_v2(
            db,
            spec.name.as_ptr() as *const _,
            spec.num_args,
            flags,
            ctx as *mut c_void,
            Some(generic_thunk),
            None,
            None,
            Some(destroy_dispatch_ctx),
        );
        if rc != ffi::SQLITE_OK {
            // Caller will report rc; we've already leaked the box
            // for this scalar but sqlite calls destroy on failure
            // too (per the v2 contract).
            return rc;
        }
    }
    ffi::SQLITE_OK
}

// ---------------------------------------------------------------
// Aggregate functions  Track 2 of PLAN-embed-remaining.md.
// ---------------------------------------------------------------
//
// Aggregates store per-aggregation state via `sqlite3_aggregate_context`
// (sqlite hands the function a per-call ptr slot you initialize on
// first step). The embed contract type-erases state through four
// thin function pointers per `AggregateSpec`:
//
//   * make_state  -> *mut () (Box::into_raw of the per-ext state type)
//   * step_state  +args      mutate the state
//   * final_state            consume the state, produce a SqlValueOwned
//   * destroy_state          drop the Box (called after final)
//
// One pair of generic thunks (`agg_step_thunk` / `agg_final_thunk`)
// handles every aggregate dispatch. Per-extension code provides
// the 4 fn pointers + a concrete state struct; the lib never
// touches the state type directly.

/// Per-aggregate declaration. `name` MUST be nul-terminated. The
/// four `*_state` fn pointers carry the type-erased lifecycle for
/// the per-aggregation state.
pub struct AggregateSpec {
    pub func_id: u64,
    pub name: &'static [u8],
    pub num_args: i32,
    pub deterministic: bool,
    /// Allocate a fresh per-aggregation state. The lib stores the
    /// returned `*mut ()` in sqlite3's aggregate-context slot;
    /// every subsequent step + final reads it back.
    pub make_state: unsafe fn() -> *mut (),
    /// Add one row's args to the running state.
    pub step_state: unsafe fn(state: *mut (), args: &[SqlValueOwned]) -> Result<(), String>,
    /// Compute the final value from the accumulated state. Called
    /// once. Does NOT free state  see `destroy_state`.
    pub final_state: unsafe fn(state: *mut ()) -> Result<SqlValueOwned, String>,
    /// Drop the state (typically `drop(Box::from_raw(state as *mut MyState))`).
    pub destroy_state: unsafe fn(state: *mut ()),
}

/// Threaded through `sqlite3_user_data`; holds a static reference to
/// the spec so the thunks know which agg they're dispatching for.
struct AggDispatchCtx {
    spec: &'static AggregateSpec,
}

unsafe extern "C" fn destroy_agg_dispatch_ctx(p: *mut c_void) {
    drop(alloc::boxed::Box::from_raw(p as *mut AggDispatchCtx));
}

/// xStep: called once per row. First call allocates state via
/// spec.make_state(); subsequent calls reuse the same state.
unsafe extern "C" fn agg_step_thunk(
    ctx: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    let disp = ffi::sqlite3_user_data(ctx) as *const AggDispatchCtx;
    if disp.is_null() {
        return;
    }
    let spec = (*disp).spec;
    // Request a ptr-sized slot. SQLite zeroes it on first call.
    let slot = ffi::sqlite3_aggregate_context(
        ctx,
        core::mem::size_of::<*mut ()>() as c_int,
    ) as *mut *mut ();
    if slot.is_null() {
        // OOM
        set_error(ctx, "aggregate: out of memory");
        return;
    }
    if (*slot).is_null() {
        *slot = (spec.make_state)();
    }
    let args = collect_args(argc, argv);
    if let Err(e) = (spec.step_state)(*slot, &args) {
        set_error(ctx, &e);
    }
}

/// xFinal: called once at end of aggregation, even if xStep never
/// fired (zero-row aggregation). When the slot is null/empty we
/// still produce a result by allocating a default state and
/// finalizing it  same as a fresh aggregation over no rows.
unsafe extern "C" fn agg_final_thunk(ctx: *mut ffi::sqlite3_context) {
    let disp = ffi::sqlite3_user_data(ctx) as *const AggDispatchCtx;
    if disp.is_null() {
        ffi::sqlite3_result_null(ctx);
        return;
    }
    let spec = (*disp).spec;
    // size=0 asks for the existing slot without allocating one;
    // SQLite returns null if step never fired.
    let slot = ffi::sqlite3_aggregate_context(ctx, 0) as *mut *mut ();
    let (state, owned_via_default) = if slot.is_null() || (*slot).is_null() {
        // Empty input. Use a default state to produce the "no rows"
        // result, then drop it ourselves (sqlite's slot is null so
        // nothing to free on its side).
        ((spec.make_state)(), true)
    } else {
        (*slot, false)
    };
    let result = (spec.final_state)(state);
    (spec.destroy_state)(state);
    if owned_via_default {
        // we created+destroyed the state ourselves; nothing more
    } else {
        // sqlite's slot still holds the (now-freed) ptr. Null it
        // out so any double-final attempt sees a fresh state.
        *slot = core::ptr::null_mut();
    }
    match result {
        Ok(v) => set_result(ctx, v),
        Err(e) => set_error(ctx, &e),
    }
}

/// Register every aggregate in `specs` against `db`. Returns SQLITE_OK
/// or the first non-OK code.
///
/// Safety: `db` must be a live `sqlite3*` and not yet closed.
pub unsafe fn register_aggregates(
    db: *mut ffi::sqlite3,
    specs: &'static [AggregateSpec],
) -> c_int {
    for spec in specs {
        let ctx = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(AggDispatchCtx {
            spec,
        }));
        let flags = SQLITE_UTF8
            | if spec.deterministic {
                SQLITE_DETERMINISTIC
            } else {
                0
            };
        let rc = ffi::sqlite3_create_function_v2(
            db,
            spec.name.as_ptr() as *const _,
            spec.num_args,
            flags,
            ctx as *mut c_void,
            // xFunc null  this is an aggregate
            None,
            Some(agg_step_thunk),
            Some(agg_final_thunk),
            Some(destroy_agg_dispatch_ctx),
        );
        if rc != ffi::SQLITE_OK {
            return rc;
        }
    }
    ffi::SQLITE_OK
}

// ---------------------------------------------------------------
// Virtual tables  Track 3 of PLAN-embed-remaining.md.
// ---------------------------------------------------------------
//
// Read-only eponymous vtab contract. v1 covers what `series`,
// `define`, `completion`, `text-utils`, `vec_each`, `pmtiles`,
// `time-series`, `vec0`-read-path need. NOT covered yet:
// xUpdate (writes), xBegin/xCommit/xRollback (transactions),
// xRename, shadow tables, xFindFunction. Those stay on the
// WIT loader.
//
// Lifecycle per vtab instance:
//   xConnect    -> allocate EmbedVtab { base, state }
//   xBestIndex  -> plan
//   xOpen       -> allocate EmbedCursor { base, state }
//   xFilter     -> set up cursor for a query
//   xNext/Eof/Column/Rowid (loop)
//   xClose      -> free cursor
//   xDisconnect -> free vtab
//
// State is type-erased through *mut () the same way aggregates
// thread state through sqlite3_aggregate_context.

/// One usable constraint sqlite passes to xBestIndex.
pub struct VtabConstraint {
    /// Column number in the declared schema (0-based).
    pub column: i32,
    /// Operator: SQLITE_INDEX_CONSTRAINT_EQ (2), GT (4), LE (8),
    /// LT (16), GE (32), MATCH (64), LIKE (65), GLOB (66),
    /// REGEXP (67), NE (68), ISNOT (69), ISNOTNULL (70),
    /// ISNULL (71), IS (72). Caller treats unknown values as
    /// "skip".
    pub op: u8,
    /// True if the planner can deliver this constraint to filter().
    pub usable: bool,
}

/// How best_index claims a constraint. Parallel to the input
/// constraints array.
#[derive(Default, Clone)]
pub struct VtabConstraintUsage {
    /// 1-based position in xFilter's argv[]. 0 = don't deliver.
    pub argv_index: i32,
    /// True if SQLite can skip re-checking the constraint after
    /// xFilter (because we'll always satisfy it).
    pub omit: bool,
}

/// Input + output to xBestIndex. The extension reads constraints,
/// writes usage + idx_num + estimates.
pub struct BestIndexInfo<'a> {
    pub constraints: &'a [VtabConstraint],
    pub usage: &'a mut [VtabConstraintUsage],
    /// Opaque planner key passed to xFilter so it knows which
    /// constraints got bound.
    pub idx_num: i32,
    pub estimated_cost: f64,
    pub estimated_rows: i64,
    pub order_by_consumed: bool,
}

/// Per-vtab declaration. `name` and `schema` are nul-terminated.
pub struct VtabSpec {
    pub name: &'static [u8],
    /// Argument to sqlite3_declare_vtab. Typically
    /// `b"CREATE TABLE x(col HIDDEN, ...)\0"`.
    pub schema: &'static [u8],
    /// True if SELECTable without CREATE VIRTUAL TABLE.
    pub eponymous: bool,
    /// Allocate a fresh per-vtab-instance state. Receives the
    /// declared table name (sqlite's argv[2]), the
    /// `CREATE VIRTUAL TABLE … USING name(arg1, arg2, …)` argv
    /// (raw, as the user typed them) and the live `sqlite3*` so
    /// instances that need to load data from the same db (trie,
    /// pmtiles, …) can issue queries before the cursor opens.
    /// Eponymous vtabs ignore all three.
    pub make_vtab: unsafe fn(
        table_name: &str,
        args: &[&str],
        db: *mut ffi::sqlite3,
    ) -> Result<*mut (), String>,
    pub destroy_vtab: unsafe fn(*mut ()),
    pub best_index: unsafe fn(*mut (), &mut BestIndexInfo) -> Result<(), String>,
    /// Allocate a fresh per-cursor state. Receives the live
    /// `sqlite3*` so cursors that need to issue sub-queries (e.g.
    /// completion's phase 5-7 schema enumeration) can stash it
    /// without an extra context plumbing.
    pub make_cursor: unsafe fn(vtab_state: *mut (), db: *mut ffi::sqlite3) -> *mut (),
    pub destroy_cursor: unsafe fn(*mut ()),
    pub filter: unsafe fn(
        cursor: *mut (),
        idx_num: i32,
        idx_str: Option<&str>,
        args: &[SqlValueOwned],
    ) -> Result<(), String>,
    pub next: unsafe fn(*mut ()) -> Result<(), String>,
    pub eof: unsafe fn(*mut ()) -> bool,
    pub column: unsafe fn(cursor: *mut (), col: i32) -> Result<SqlValueOwned, String>,
    pub rowid: unsafe fn(*mut ()) -> Result<i64, String>,
    /// xUpdate. None = read-only (sqlite will reject INSERT/UPDATE/
    /// DELETE against the vtab). When set, sqlite encodes the op
    /// in args (see WIT `vtab-update.update` doc); the returned
    /// i64 is the new rowid for INSERT (ignored for DELETE/UPDATE).
    pub update: Option<
        unsafe fn(
            vtab_state: *mut (),
            args: &[SqlValueOwned],
        ) -> Result<i64, String>,
    >,
    /// xBegin. None = no per-vtab transaction notification; sqlite
    /// still allows writes through xUpdate without xBegin if it's
    /// null (treats writes as auto-commit per-row).
    pub begin: Option<unsafe fn(vtab_state: *mut ()) -> Result<(), String>>,
    pub sync: Option<unsafe fn(vtab_state: *mut ()) -> Result<(), String>>,
    pub commit: Option<unsafe fn(vtab_state: *mut ()) -> Result<(), String>>,
    pub rollback: Option<unsafe fn(vtab_state: *mut ()) -> Result<(), String>>,
    /// xRename. New name is the bare identifier.
    pub rename: Option<
        unsafe fn(vtab_state: *mut (), new_name: &str) -> Result<(), String>,
    >,
    /// Nested savepoint trio. Mostly safe to leave None; sqlite
    /// falls back to xBegin/xCommit/xRollback at the outer scope.
    pub savepoint:
        Option<unsafe fn(vtab_state: *mut (), savepoint: i32) -> Result<(), String>>,
    pub release:
        Option<unsafe fn(vtab_state: *mut (), savepoint: i32) -> Result<(), String>>,
    pub rollback_to:
        Option<unsafe fn(vtab_state: *mut (), savepoint: i32) -> Result<(), String>>,
    /// xShadowName  module-level. Receives just the candidate
    /// name; returns true if the vtab owns it as a shadow table.
    /// Default-none = "this vtab owns no shadow tables".
    pub shadow_name: Option<unsafe fn(name: &str) -> bool>,
    /// xIntegrity  per-instance integrity check. `mode_flags` mirrors
    /// sqlite's `mFlags`: bit 0 set = quick check.
    pub integrity: Option<
        unsafe fn(
            vtab_state: *mut (),
            schema: &str,
            table_name: &str,
            mode_flags: u32,
        ) -> Result<(), String>,
    >,
    /// xFindFunction  override sqlite's operator-to-function
    /// mapping for this vtab. Called with `(name, n_arg)`; returns
    /// the constraint-op code sqlite should emit to xBestIndex.
    /// 0 = "we don't claim this name". Default-none =
    /// MATCH/GLOB/REGEXP/LIKE are claimed automatically (parity with
    /// the cli's `x_find_function`); set Some to override or
    /// extend.
    pub find_function: Option<
        unsafe fn(name: &str, n_arg: i32) -> i32,
    >,
}

/// Larger struct than sqlite3_vtab; sqlite only sees the first
/// `base` field by C-aliasing rules but Rust holds the extra state.
#[repr(C)]
struct EmbedVtab {
    base: ffi::sqlite3_vtab,
    state: *mut (),
    spec: &'static VtabSpec,
    db: *mut ffi::sqlite3,
}

#[repr(C)]
struct EmbedCursor {
    base: ffi::sqlite3_vtab_cursor,
    state: *mut (),
    spec: &'static VtabSpec,
}

unsafe extern "C" fn vtab_xconnect(
    db: *mut ffi::sqlite3,
    paux: *mut c_void,
    argc: c_int,
    argv: *const *const core::ffi::c_char,
    pp_vtab: *mut *mut ffi::sqlite3_vtab,
    err_msg: *mut *mut core::ffi::c_char,
) -> c_int {
    let spec = &*(paux as *const VtabSpec);
    // declare_vtab needs a nul-terminated SQL string.
    let rc = ffi::sqlite3_declare_vtab(db, spec.schema.as_ptr() as *const _);
    if rc != ffi::SQLITE_OK {
        return rc;
    }
    // sqlite hands us argv[0..3] as bookkeeping (module name, db
    // name, table name) and argv[3..] as the user's
    // `USING name(a, b, c)` args. Capture argv[2] as the declared
    // table name (vec0_refresh / vec0_delete look up instances by
    // it), then pass argv[3..] to the per-ext make_vtab.
    let mut table_name = String::new();
    let mut args_owned: Vec<String> = Vec::new();
    if !argv.is_null() {
        let slice = core::slice::from_raw_parts(argv, argc as usize);
        if argc > 2 {
            let p = slice[2];
            if !p.is_null() {
                let cstr = core::ffi::CStr::from_ptr(p);
                table_name = cstr.to_string_lossy().into_owned();
            }
        }
        if argc > 3 {
            for &p in slice.iter().skip(3) {
                if p.is_null() {
                    continue;
                }
                let cstr = core::ffi::CStr::from_ptr(p);
                args_owned.push(cstr.to_string_lossy().into_owned());
            }
        }
    }
    let args_refs: Vec<&str> = args_owned.iter().map(|s| s.as_str()).collect();
    let state = match (spec.make_vtab)(&table_name, &args_refs, db) {
        Ok(s) => s,
        Err(e) => {
            // Hand the error back to sqlite via *err_msg (it
            // expects a sqlite-allocated cstring it will free).
            let bytes = e.as_bytes();
            let buf = ffi::sqlite3_malloc((bytes.len() + 1) as c_int) as *mut core::ffi::c_char;
            if !buf.is_null() {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr() as *const core::ffi::c_char,
                    buf,
                    bytes.len(),
                );
                *buf.add(bytes.len()) = 0;
                *err_msg = buf;
            }
            return ffi::SQLITE_ERROR;
        }
    };
    let vtab = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(EmbedVtab {
        base: core::mem::zeroed(),
        state,
        spec,
        db,
    }));
    *pp_vtab = vtab as *mut ffi::sqlite3_vtab;
    ffi::SQLITE_OK
}

unsafe extern "C" fn vtab_xdisconnect(pv: *mut ffi::sqlite3_vtab) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    (spec.destroy_vtab)((*v).state);
    drop(alloc::boxed::Box::from_raw(v));
    ffi::SQLITE_OK
}

unsafe extern "C" fn vtab_xbest_index(
    pv: *mut ffi::sqlite3_vtab,
    info: *mut ffi::sqlite3_index_info,
) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    let n = (*info).nConstraint as usize;
    // Materialize constraints. The aConstraint array is sqlite-owned;
    // we read it into our types and pass a slice.
    let mut constraints: alloc::vec::Vec<VtabConstraint> = alloc::vec::Vec::with_capacity(n);
    let c_arr = (*info).aConstraint;
    for i in 0..n {
        let c = &*c_arr.add(i);
        constraints.push(VtabConstraint {
            column: c.iColumn,
            op: c.op,
            usable: c.usable != 0,
        });
    }
    let mut usage: alloc::vec::Vec<VtabConstraintUsage> = alloc::vec![VtabConstraintUsage::default(); n];
    // Split-borrow: BestIndexInfo holds &constraints + &mut usage.
    let (idx_num, estimated_cost, estimated_rows, order_by_consumed);
    {
        let mut bi = BestIndexInfo {
            constraints: &constraints,
            usage: &mut usage,
            idx_num: 0,
            estimated_cost: 1.0e9,
            estimated_rows: 1,
            order_by_consumed: false,
        };
        match (spec.best_index)((*v).state, &mut bi) {
            Ok(()) => {}
            Err(_) => return ffi::SQLITE_ERROR,
        }
        idx_num = bi.idx_num;
        estimated_cost = bi.estimated_cost;
        estimated_rows = bi.estimated_rows;
        order_by_consumed = bi.order_by_consumed;
    }
    // Copy usage back into sqlite's array.
    let u_arr = (*info).aConstraintUsage;
    for (i, u) in usage.iter().enumerate() {
        let dst = u_arr.add(i);
        (*dst).argvIndex = u.argv_index;
        (*dst).omit = if u.omit { 1 } else { 0 };
    }
    (*info).idxNum = idx_num;
    (*info).estimatedCost = estimated_cost;
    (*info).estimatedRows = estimated_rows;
    (*info).orderByConsumed = if order_by_consumed { 1 } else { 0 };
    ffi::SQLITE_OK
}

unsafe extern "C" fn vtab_xopen(
    pv: *mut ffi::sqlite3_vtab,
    pp_cursor: *mut *mut ffi::sqlite3_vtab_cursor,
) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    let cursor_state = (spec.make_cursor)((*v).state, (*v).db);
    let c = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(EmbedCursor {
        base: core::mem::zeroed(),
        state: cursor_state,
        spec,
    }));
    *pp_cursor = c as *mut ffi::sqlite3_vtab_cursor;
    ffi::SQLITE_OK
}

unsafe extern "C" fn vtab_xclose(pc: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    let c = pc as *mut EmbedCursor;
    let spec = (*c).spec;
    (spec.destroy_cursor)((*c).state);
    drop(alloc::boxed::Box::from_raw(c));
    ffi::SQLITE_OK
}

unsafe extern "C" fn vtab_xfilter(
    pc: *mut ffi::sqlite3_vtab_cursor,
    idx_num: c_int,
    idx_str: *const core::ffi::c_char,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) -> c_int {
    let c = pc as *mut EmbedCursor;
    let spec = (*c).spec;
    let idx_str_opt = if idx_str.is_null() {
        None
    } else {
        let cstr = core::ffi::CStr::from_ptr(idx_str);
        cstr.to_str().ok()
    };
    let args = collect_args(argc, argv);
    match (spec.filter)((*c).state, idx_num, idx_str_opt, &args) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn vtab_xnext(pc: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    let c = pc as *mut EmbedCursor;
    let spec = (*c).spec;
    match (spec.next)((*c).state) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn vtab_xeof(pc: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    let c = pc as *mut EmbedCursor;
    let spec = (*c).spec;
    if (spec.eof)((*c).state) { 1 } else { 0 }
}

unsafe extern "C" fn vtab_xcolumn(
    pc: *mut ffi::sqlite3_vtab_cursor,
    ctx: *mut ffi::sqlite3_context,
    col: c_int,
) -> c_int {
    let c = pc as *mut EmbedCursor;
    let spec = (*c).spec;
    match (spec.column)((*c).state, col) {
        Ok(v) => {
            set_result(ctx, v);
            ffi::SQLITE_OK
        }
        Err(e) => {
            set_error(ctx, &e);
            ffi::SQLITE_ERROR
        }
    }
}

unsafe extern "C" fn vtab_xrowid(
    pc: *mut ffi::sqlite3_vtab_cursor,
    rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    let c = pc as *mut EmbedCursor;
    let spec = (*c).spec;
    match (spec.rowid)((*c).state) {
        Ok(v) => {
            *rowid = v;
            ffi::SQLITE_OK
        }
        Err(_) => ffi::SQLITE_ERROR,
    }
}

// ── Mutating trampolines ─────────────────────────────────────
// Wired in `make_module` only if the corresponding spec field is
// Some. Each pulls the spec out of EmbedVtab and calls the
// extension's fn pointer; None fields stay null in the module
// struct, so sqlite won't invoke them.

unsafe extern "C" fn vtab_xupdate(
    pv: *mut ffi::sqlite3_vtab,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
    p_rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    let Some(update_fn) = spec.update else {
        return ffi::SQLITE_READONLY;
    };
    let args = collect_args(argc, argv);
    match update_fn((*v).state, &args) {
        Ok(new_rowid) => {
            if !p_rowid.is_null() {
                *p_rowid = new_rowid;
            }
            ffi::SQLITE_OK
        }
        Err(e) => {
            // Stash error in sqlite3_vtab->zErrMsg so the caller
            // sees a useful message. SQLite frees via sqlite3_free.
            let bytes = e.as_bytes();
            let buf = ffi::sqlite3_malloc((bytes.len() + 1) as c_int)
                as *mut core::ffi::c_char;
            if !buf.is_null() {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr() as *const core::ffi::c_char,
                    buf,
                    bytes.len(),
                );
                *buf.add(bytes.len()) = 0;
                (*pv).zErrMsg = buf;
            }
            ffi::SQLITE_ERROR
        }
    }
}

unsafe extern "C" fn vtab_xbegin(pv: *mut ffi::sqlite3_vtab) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    match spec.begin.map(|f| f((*v).state)) {
        None | Some(Ok(())) => ffi::SQLITE_OK,
        Some(Err(_)) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn vtab_xsync(pv: *mut ffi::sqlite3_vtab) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    match spec.sync.map(|f| f((*v).state)) {
        None | Some(Ok(())) => ffi::SQLITE_OK,
        Some(Err(_)) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn vtab_xcommit(pv: *mut ffi::sqlite3_vtab) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    match spec.commit.map(|f| f((*v).state)) {
        None | Some(Ok(())) => ffi::SQLITE_OK,
        Some(Err(_)) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn vtab_xrollback(pv: *mut ffi::sqlite3_vtab) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    match spec.rollback.map(|f| f((*v).state)) {
        None | Some(Ok(())) => ffi::SQLITE_OK,
        Some(Err(_)) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn vtab_xrename(
    pv: *mut ffi::sqlite3_vtab,
    z_new: *const core::ffi::c_char,
) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    let Some(rename_fn) = spec.rename else {
        return ffi::SQLITE_OK;
    };
    let new_name = if z_new.is_null() {
        ""
    } else {
        let cstr = core::ffi::CStr::from_ptr(z_new);
        cstr.to_str().unwrap_or("")
    };
    match rename_fn((*v).state, new_name) {
        Ok(()) => ffi::SQLITE_OK,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn vtab_xsavepoint(pv: *mut ffi::sqlite3_vtab, sp: c_int) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    match spec.savepoint.map(|f| f((*v).state, sp)) {
        None | Some(Ok(())) => ffi::SQLITE_OK,
        Some(Err(_)) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn vtab_xrelease(pv: *mut ffi::sqlite3_vtab, sp: c_int) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    match spec.release.map(|f| f((*v).state, sp)) {
        None | Some(Ok(())) => ffi::SQLITE_OK,
        Some(Err(_)) => ffi::SQLITE_ERROR,
    }
}

unsafe extern "C" fn vtab_xrollback_to(pv: *mut ffi::sqlite3_vtab, sp: c_int) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    match spec.rollback_to.map(|f| f((*v).state, sp)) {
        None | Some(Ok(())) => ffi::SQLITE_OK,
        Some(Err(_)) => ffi::SQLITE_ERROR,
    }
}

/// xShadowName lives on the module, not on an instance. sqlite
/// passes only the candidate name; we need to find which `VtabSpec`
/// to consult. The embed contract registers ONE VtabSpec per
/// module so we keep a per-module thread_local that
/// `register_vtabs` populates per-name.
///
/// no_std target: skip thread_local (single-thread wasm), use a
/// plain static cell with unsafe interior. Safe because the cli
/// is single-threaded.
struct ShadowOwnerCell {
    spec: core::cell::UnsafeCell<Option<&'static VtabSpec>>,
}
unsafe impl Sync for ShadowOwnerCell {}
static SHADOW_OWNER: ShadowOwnerCell = ShadowOwnerCell {
    spec: core::cell::UnsafeCell::new(None),
};

unsafe extern "C" fn vtab_xshadow_name(z_name: *const core::ffi::c_char) -> c_int {
    if z_name.is_null() {
        return 0;
    }
    let spec_opt = *SHADOW_OWNER.spec.get();
    let Some(spec) = spec_opt else {
        return 0;
    };
    let Some(check) = spec.shadow_name else {
        return 0;
    };
    let cstr = core::ffi::CStr::from_ptr(z_name);
    let Ok(name) = cstr.to_str() else {
        return 0;
    };
    if check(name) {
        1
    } else {
        0
    }
}

unsafe extern "C" fn vtab_xintegrity(
    pv: *mut ffi::sqlite3_vtab,
    z_schema: *const core::ffi::c_char,
    z_table_name: *const core::ffi::c_char,
    m_flags: c_int,
    pz_err: *mut *mut core::ffi::c_char,
) -> c_int {
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    let Some(check) = spec.integrity else {
        return ffi::SQLITE_OK;
    };
    let schema = if z_schema.is_null() {
        ""
    } else {
        core::ffi::CStr::from_ptr(z_schema).to_str().unwrap_or("")
    };
    let table_name = if z_table_name.is_null() {
        ""
    } else {
        core::ffi::CStr::from_ptr(z_table_name)
            .to_str()
            .unwrap_or("")
    };
    match check((*v).state, schema, table_name, m_flags as u32) {
        Ok(()) => ffi::SQLITE_OK,
        Err(e) => {
            if !pz_err.is_null() {
                let bytes = e.as_bytes();
                let buf = ffi::sqlite3_malloc((bytes.len() + 1) as c_int)
                    as *mut core::ffi::c_char;
                if !buf.is_null() {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr() as *const core::ffi::c_char,
                        buf,
                        bytes.len(),
                    );
                    *buf.add(bytes.len()) = 0;
                    *pz_err = buf;
                }
            }
            ffi::SQLITE_ERROR
        }
    }
}

unsafe extern "C" fn vtab_xfind_function(
    pv: *mut ffi::sqlite3_vtab,
    n_arg: c_int,
    z_name: *const core::ffi::c_char,
    px_func: *mut Option<
        unsafe extern "C" fn(*mut ffi::sqlite3_context, c_int, *mut *mut ffi::sqlite3_value),
    >,
    pp_arg: *mut *mut c_void,
) -> c_int {
    if z_name.is_null() {
        return 0;
    }
    let cstr = core::ffi::CStr::from_ptr(z_name);
    let Ok(name) = cstr.to_str() else {
        return 0;
    };
    let v = pv as *mut EmbedVtab;
    let spec = (*v).spec;
    // Built-in fallback parity with the cli's x_find_function:
    // claim MATCH/GLOB/REGEXP/LIKE so vtabs can declare those
    // constraints in best_index without needing to wire find_function
    // explicitly.
    let lower: alloc::string::String = name
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let op = match lower.as_str() {
        "match" => 64, // SQLITE_INDEX_CONSTRAINT_MATCH
        "glob" => 66,  // SQLITE_INDEX_CONSTRAINT_GLOB
        "regexp" => 67, // SQLITE_INDEX_CONSTRAINT_REGEXP
        "like" => 65,  // SQLITE_INDEX_CONSTRAINT_LIKE
        _ => 0,
    };
    let op = if let Some(custom) = spec.find_function {
        let custom_op = custom(&lower, n_arg);
        if custom_op != 0 {
            custom_op
        } else {
            op
        }
    } else {
        op
    };
    if op == 0 {
        return 0;
    }
    if !px_func.is_null() {
        // Sqlite needs a function pointer even when xFindFunction
        // returns a constraint-op code. The pointer is never
        // called because best_index consumes the constraint.
        *px_func = Some(vtab_xfind_function_stub);
    }
    if !pp_arg.is_null() {
        *pp_arg = core::ptr::null_mut();
    }
    op
}

unsafe extern "C" fn vtab_xfind_function_stub(
    ctx: *mut ffi::sqlite3_context,
    _argc: c_int,
    _argv: *mut *mut ffi::sqlite3_value,
) {
    set_error(ctx, "vtab operator stub called  best_index should have consumed it");
}

/// One module per registered vtab. Boxed + leaked  it lives for
/// the lifetime of the sqlite3 connection. SQLite copies the module
/// pointer; no destroy is wired because we don't reclaim memory
/// across vtab module unregister (none of our exts do that).
unsafe fn make_module(spec: &'static VtabSpec) -> ffi::sqlite3_module {
    let mut m: ffi::sqlite3_module = core::mem::zeroed();
    // iVersion 2 unlocks the xSavepoint / xRelease / xRollbackTo
    // slots; safe to use even for read-only vtabs (sqlite just
    // ignores the null pointers).
    m.iVersion = 2;
    if spec.eponymous {
        // Eponymous: no xCreate (sqlite uses xConnect on first ref).
        m.xCreate = None;
    } else {
        m.xCreate = Some(vtab_xconnect);
    }
    m.xConnect = Some(vtab_xconnect);
    m.xBestIndex = Some(vtab_xbest_index);
    m.xDisconnect = Some(vtab_xdisconnect);
    m.xDestroy = Some(vtab_xdisconnect);
    m.xOpen = Some(vtab_xopen);
    m.xClose = Some(vtab_xclose);
    m.xFilter = Some(vtab_xfilter);
    m.xNext = Some(vtab_xnext);
    m.xEof = Some(vtab_xeof);
    m.xColumn = Some(vtab_xcolumn);
    m.xRowid = Some(vtab_xrowid);
    if spec.update.is_some() {
        m.xUpdate = Some(vtab_xupdate);
    }
    if spec.begin.is_some() {
        m.xBegin = Some(vtab_xbegin);
    }
    if spec.sync.is_some() {
        m.xSync = Some(vtab_xsync);
    }
    if spec.commit.is_some() {
        m.xCommit = Some(vtab_xcommit);
    }
    if spec.rollback.is_some() {
        m.xRollback = Some(vtab_xrollback);
    }
    if spec.rename.is_some() {
        m.xRename = Some(vtab_xrename);
    }
    if spec.savepoint.is_some() {
        m.xSavepoint = Some(vtab_xsavepoint);
    }
    if spec.release.is_some() {
        m.xRelease = Some(vtab_xrelease);
    }
    if spec.rollback_to.is_some() {
        m.xRollbackTo = Some(vtab_xrollback_to);
    }
    if spec.shadow_name.is_some() {
        m.xShadowName = Some(vtab_xshadow_name);
    }
    if spec.integrity.is_some() {
        m.xIntegrity = Some(vtab_xintegrity);
    }
    // xFindFunction has a sensible default (MATCH/GLOB/REGEXP/LIKE),
    // so wire it unconditionally  vtabs can opt out by returning 0
    // from a custom find_function. iVersion 3 is required to expose
    // xIntegrity; bump.
    m.xFindFunction = Some(vtab_xfind_function);
    if spec.integrity.is_some() {
        m.iVersion = 3;
    }
    m
}

/// Register every vtab in `specs` against `db`. Returns SQLITE_OK
/// or the first non-OK code. Each module + its aux pointer live for
/// the lifetime of the connection (leaked Box; sqlite never calls
/// the destroy callback on the aux because we use the v1 module
/// registration, not v2).
///
/// Safety: `db` must be a live `sqlite3*` and not yet closed.
pub unsafe fn register_vtabs(
    db: *mut ffi::sqlite3,
    specs: &'static [VtabSpec],
) -> c_int {
    for spec in specs {
        // Leak the module struct  sqlite holds the pointer for
        // the connection's lifetime.
        let module = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(make_module(spec)));
        let name_bytes = spec.name;
        let rc = ffi::sqlite3_create_module_v2(
            db,
            name_bytes.as_ptr() as *const _,
            module,
            spec as *const VtabSpec as *mut c_void,
            None,
        );
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        // xShadowName has no context, so the trampoline reads
        // the active spec from a single-writer cell. Last writer
        // wins  v1 caveat documented in `SHADOW_OWNER`.
        if spec.shadow_name.is_some() {
            *SHADOW_OWNER.spec.get() = Some(spec);
        }
    }
    ffi::SQLITE_OK
}

/// Re-export `sqlite3` so extension embed.rs files don't need a
/// direct libsqlite3-sys dep on top of sqlite-embed. (They CAN
/// declare one if they need other ffi symbols, but for most a
/// `use sqlite_embed::ffi::sqlite3;` suffices.)
pub mod ffi_reexport {
    pub use libsqlite3_sys::sqlite3;
}

// ---------------------------------------------------------------
// In-process SPI emulation  used by db-aware scalars (define,
// vec0 read paths, ...) so embed.rs files don't have to redo the
// prepare/bind/step/finalize dance for every helper. Roughly
// mirrors the WIT `spi::execute` / `spi::execute_batch` surface
// but talks straight to libsqlite3-sys.
// ---------------------------------------------------------------

unsafe fn bind_value(
    stmt: *mut ffi::sqlite3_stmt,
    idx: c_int,
    v: &SqlValueOwned,
) -> c_int {
    match v {
        SqlValueOwned::Null => ffi::sqlite3_bind_null(stmt, idx),
        SqlValueOwned::Integer(n) => ffi::sqlite3_bind_int64(stmt, idx, *n),
        SqlValueOwned::Real(r) => ffi::sqlite3_bind_double(stmt, idx, *r),
        SqlValueOwned::Text(s) => ffi::sqlite3_bind_text(
            stmt,
            idx,
            s.as_ptr() as *const _,
            s.len() as c_int,
            ffi::SQLITE_TRANSIENT(),
        ),
        SqlValueOwned::Blob(b) => ffi::sqlite3_bind_blob(
            stmt,
            idx,
            b.as_ptr() as *const _,
            b.len() as c_int,
            ffi::SQLITE_TRANSIENT(),
        ),
    }
}

unsafe fn column_to_owned(stmt: *mut ffi::sqlite3_stmt, col: c_int) -> SqlValueOwned {
    match ffi::sqlite3_column_type(stmt, col) {
        ffi::SQLITE_NULL => SqlValueOwned::Null,
        ffi::SQLITE_INTEGER => SqlValueOwned::Integer(ffi::sqlite3_column_int64(stmt, col)),
        ffi::SQLITE_FLOAT => SqlValueOwned::Real(ffi::sqlite3_column_double(stmt, col)),
        ffi::SQLITE_TEXT => {
            let p = ffi::sqlite3_column_text(stmt, col);
            if p.is_null() {
                return SqlValueOwned::Text(String::new());
            }
            let n = ffi::sqlite3_column_bytes(stmt, col) as usize;
            let bytes = core::slice::from_raw_parts(p, n);
            match core::str::from_utf8(bytes) {
                Ok(s) => SqlValueOwned::Text(s.into()),
                Err(_) => SqlValueOwned::Text(String::new()),
            }
        }
        ffi::SQLITE_BLOB => {
            let p = ffi::sqlite3_column_blob(stmt, col) as *const u8;
            if p.is_null() {
                return SqlValueOwned::Blob(Vec::new());
            }
            let n = ffi::sqlite3_column_bytes(stmt, col) as usize;
            SqlValueOwned::Blob(core::slice::from_raw_parts(p, n).to_vec())
        }
        _ => SqlValueOwned::Null,
    }
}

unsafe fn last_error(db: *mut ffi::sqlite3) -> String {
    let p = ffi::sqlite3_errmsg(db);
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    let bytes = core::slice::from_raw_parts(p as *const u8, len);
    String::from_utf8_lossy(bytes).into_owned()
}

/// Run a SQL string with no parameters and no result rows
/// (statements like CREATE/INSERT/DELETE). Equivalent to the WIT
/// `spi::execute_batch`.
///
/// Safety: `db` must be a live `sqlite3*`.
pub unsafe fn exec_batch(db: *mut ffi::sqlite3, sql: &str) -> Result<(), String> {
    let mut cstr: Vec<u8> = Vec::with_capacity(sql.len() + 1);
    cstr.extend_from_slice(sql.as_bytes());
    cstr.push(0);
    let mut err: *mut core::ffi::c_char = core::ptr::null_mut();
    let rc = ffi::sqlite3_exec(
        db,
        cstr.as_ptr() as *const _,
        None,
        core::ptr::null_mut(),
        &mut err,
    );
    if rc != ffi::SQLITE_OK {
        let msg = if err.is_null() {
            last_error(db)
        } else {
            let cstr = core::ffi::CStr::from_ptr(err);
            let s = cstr.to_string_lossy().into_owned();
            ffi::sqlite3_free(err as *mut c_void);
            s
        };
        return Err(msg);
    }
    Ok(())
}

/// Prepare + bind + step a parameterized SQL statement, returning
/// all rows. Equivalent to the WIT `spi::execute`.
///
/// Safety: `db` must be a live `sqlite3*`.
pub unsafe fn exec_query(
    db: *mut ffi::sqlite3,
    sql: &str,
    params: &[SqlValueOwned],
) -> Result<Vec<Vec<SqlValueOwned>>, String> {
    let mut cstr: Vec<u8> = Vec::with_capacity(sql.len() + 1);
    cstr.extend_from_slice(sql.as_bytes());
    cstr.push(0);
    let mut stmt: *mut ffi::sqlite3_stmt = core::ptr::null_mut();
    let rc = ffi::sqlite3_prepare_v2(
        db,
        cstr.as_ptr() as *const _,
        -1,
        &mut stmt,
        core::ptr::null_mut(),
    );
    if rc != ffi::SQLITE_OK {
        return Err(last_error(db));
    }
    for (i, p) in params.iter().enumerate() {
        let rc = bind_value(stmt, (i + 1) as c_int, p);
        if rc != ffi::SQLITE_OK {
            let msg = last_error(db);
            ffi::sqlite3_finalize(stmt);
            return Err(msg);
        }
    }
    let mut rows: Vec<Vec<SqlValueOwned>> = Vec::new();
    loop {
        let rc = ffi::sqlite3_step(stmt);
        if rc == ffi::SQLITE_DONE {
            break;
        }
        if rc != ffi::SQLITE_ROW {
            let msg = last_error(db);
            ffi::sqlite3_finalize(stmt);
            return Err(msg);
        }
        let n = ffi::sqlite3_column_count(stmt);
        let mut row = Vec::with_capacity(n as usize);
        for c in 0..n {
            row.push(column_to_owned(stmt, c));
        }
        rows.push(row);
    }
    ffi::sqlite3_finalize(stmt);
    Ok(rows)
}
