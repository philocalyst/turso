//! Per-connection version-control wiring for the `dolt_*` scalar functions.
//!
//! Each dolt_* function is registered as a ScalarFunc via the extension API,
//! with a per-connection `VcState` boxed as the opaque function context. The
//! state owns the `VcStore` (which carries the session-branch state: active
//! branch name plus the detached pin, per item 5) and a companion
//! `SessionBranch` that mirrors it for qualified-path opens. The connection
//! layer owns the state; the pager never sees it.
//!
//! Mutating `dolt_*` operations are refused by `VcStore::guard_write` inside
//! the store methods, and the commit path holds the graph lock across the
//! plan→compare-and-swap window so a stale-tip race surfaces as `database is locked`.

use std::ffi::CString;
use std::sync::{Arc, Mutex};

use crate::{
    ContextDestructor, ExtensionApi, ResultCode, ScalarFunction, Value, ValueDestructor, ValueType,
};

use turso_versioning::funcs::{self, FuncArg, FuncValue};
use turso_versioning::model::{CommitId, VersionError, VersionResult};
use turso_versioning::session::SessionBranch;
use turso_versioning::staging::VcStore;
use turso_versioning::vtab_log::{VcRow, VcValue};

/// A borrowed extension-connection pointer. The owning core connection keeps
/// it alive; the pointer is only dereferenced while that connection is alive,
/// so sharing it across threads is safe.
#[derive(Clone, Copy)]
pub struct ConnRef(*const crate::vtabs::Conn);

unsafe impl Send for ConnRef {}
unsafe impl Sync for ConnRef {}

impl ConnRef {
    pub fn new(ptr: *const crate::vtabs::Conn) -> Self {
        ConnRef(ptr)
    }

    pub fn ptr(self) -> *const crate::vtabs::Conn {
        self.0
    }
}

/// Owned extension-connection handle for the write-path shims. The core
/// connection keeps one; the handle owns the FFI connection plus the boxed
/// `Weak<core::Connection>` its `_ctx` points to. Both are only touched while
/// the owning connection is alive.
pub struct ConnHandle {
    conn: Box<crate::vtabs::Conn>,
    weak: *mut std::ffi::c_void,
}

unsafe impl Send for ConnHandle {}
unsafe impl Sync for ConnHandle {}

impl ConnHandle {
    /// Own the FFI connection and the raw weak-box pointer it wraps.
    pub fn new(conn: Box<crate::vtabs::Conn>, weak: *mut std::ffi::c_void) -> Self {
        ConnHandle { conn, weak }
    }

    /// The FFI connection pointer the shims use.
    pub fn ptr(&self) -> *const crate::vtabs::Conn {
        self.conn.as_ref() as *const crate::vtabs::Conn
    }

    /// The boxed `Weak<core::Connection>` the handle wraps, for core to free.
    pub fn weak_ptr(&self) -> *mut std::ffi::c_void {
        self.weak
    }
}

/// Per-connection version-control state.
pub struct VcState {
    store: Mutex<VcStore>,
    session: Mutex<SessionBranch>,
    graph: Mutex<()>,
    /// The owning connection, so the write-path shims can capture SQL content
    /// into the store and write merged content back. Borrowed from the core
    /// connection, which owns the handle; `None` before it is attached.
    conn: Mutex<Option<ConnRef>>,
}

impl std::fmt::Debug for VcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VcState").finish_non_exhaustive()
    }
}

impl VcState {
    pub fn new() -> Arc<Self> {
        Arc::new(VcState {
            store: Mutex::new(VcStore::new("main")),
            session: Mutex::new(SessionBranch::new("main")),
            graph: Mutex::new(()),
            conn: Mutex::new(None),
        })
    }

    /// Attach the owning connection's extension handle, so write-path shims
    /// can read and write the SQL tables. The core connection owns it and
    /// keeps it alive; this pointer is borrowed.
    pub fn set_conn(&self, conn: ConnRef) {
        *self.conn.lock().unwrap() = Some(conn);
    }

    /// Run a closure against the connection handle, if attached.
    fn with_conn<R>(&self, f: impl FnOnce(&crate::vtabs::Conn) -> R) -> Option<R> {
        let conn = (*self.conn.lock().unwrap())?;
        Some(f(unsafe { &*conn.ptr() }))
    }

    /// Capture the current SQL content of every tracked table into the store's
    /// working set. Skipped while a rebase is paused or conflicts are recorded
    /// (the store's working set then carries the merged state, and SQL is
    /// behind it); a clean open merge still captures, so a `--no-commit`
    /// merge followed by direct SQL edits picks those edits up on commit.
    pub fn sync_sql_to_work(&self) -> VersionResult<()> {
        let (names, blocked) = {
            let store = self.store.lock().unwrap();
            (
                store.tables(),
                store.is_rebase_open() || store.has_conflicts(),
            )
        };
        if blocked || names.is_empty() {
            return Ok(());
        }
        let Some(conn) = self.with_conn(|c| c as *const _) else {
            return Ok(());
        };
        let mut captured: Vec<(String, TableCapture)> = Vec::new();
        for t in &names {
            captured.push((t.clone(), read_table(unsafe { &*conn }, t)?));
        }
        let mut store = self.store.lock().unwrap();
        for (table, (columns, pk, rows, schema_sql)) in captured {
            store.apply_work(&table, columns, pk, rows, schema_sql);
        }
        Ok(())
    }

    /// Point the working set at the active branch's committed content.
    pub fn sync_work_to_head(&self) {
        self.store.lock().unwrap().sync_work_to_head();
    }

    /// Write the store's dirty working tables back to the SQL tables, and apply
    /// any pending conflict resolutions row by row. Called after
    /// merge/replay/resolve so `SELECT * FROM t` reflects the merged state;
    /// plain commits leave SQL and work identical, so no-op there. The pending
    /// resolutions are drained here so the map cannot grow unboundedly.
    ///
    /// The whole write-back is wrapped in an SQL SAVEPOINT so a failure on any
    /// table rolls back every table to the pre-write-back state. On store-side
    /// failure the dirty tables are restored (the SQL tables were rolled back,
    /// so the store must re-attempt the full write on the next call).
    pub fn sync_work_to_sql(&self) -> VersionResult<()> {
        let Some(conn) = self.with_conn(|c| c as *const _) else {
            return Ok(());
        };
        let conn = unsafe { &*conn };
        let (resolutions, changes) = {
            let mut store = self.store.lock().unwrap();
            (store.take_pending_resolve(), store.take_work_changes())
        };
        // Save copies so a write-back failure can restore the drained state.
        let saved_resolutions = resolutions.clone();
        let dirty_names: Vec<String> = changes.iter().map(|(t, _)| t.clone()).collect();
        #[allow(clippy::arc_with_non_send_sync)]
        let connection = Arc::new(crate::Connection::new(conn as *const crate::vtabs::Conn));
        execute_sql(&connection, "SAVEPOINT turso_vc_sync", Vec::new())?;
        let result = sync_write_back(&connection, &resolutions, &changes);
        match result {
            Ok(()) => {
                execute_sql(&connection, "RELEASE turso_vc_sync", Vec::new())?;
            }
            Err(e) => {
                let _ = execute_sql(&connection, "ROLLBACK TO turso_vc_sync", Vec::new());
                let _ = execute_sql(&connection, "RELEASE turso_vc_sync", Vec::new());
                // Restore the drained pending-resolve and dirty flags so a
                // retry can re-attempt the write-back. The SQL tables are
                // rolled back by the SAVEPOINT; the store working set is the
                // source of truth and must remain retryable.
                let mut store = self.store.lock().unwrap();
                store.restore_pending_resolve(saved_resolutions);
                store.restore_dirty_work(dirty_names);
                return Err(e);
            }
        }
        Ok(())
    }

    /// Record a table as known to version control (the CREATE TABLE hook).
    pub fn track_table(&self, name: &str) {
        self.store.lock().unwrap().track_table(name);
    }

    /// Attach to a named branch, clearing any detached pin.
    pub fn open_branch(&self, name: &str) -> VersionResult<()> {
        self.store.lock().unwrap().checkout(name)?;
        self.session.lock().unwrap().open_branch(name);
        Ok(())
    }

    /// Open an immutable snapshot at `snapshot`, refusing writes (R4).
    pub fn open_detached(&self, snapshot: CommitId) {
        self.store.lock().unwrap().open_detached(snapshot);
        self.session.lock().unwrap().open_detached(snapshot);
    }

    /// `None` while detached, so `dolt_active_branch()` reads as NULL.
    pub fn active_branch(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap()
            .active_branch()
            .map(|r| r.name.clone())
    }

    /// Writes are refused on a detached snapshot (R4).
    pub fn guard_write(&self) -> VersionResult<()> {
        self.store.lock().unwrap().guard_write()?;
        self.session.lock().unwrap().guard_write()
    }

    /// Read the live store under its lock. Vtable cursors materialize rows
    /// through this; the closure must be short, pure, and never call back
    /// into SQL (the lock is held, and reentrancy would deadlock).
    pub fn with_store<R>(&self, f: impl FnOnce(&VcStore) -> R) -> R {
        f(&self.store.lock().unwrap())
    }
}

/// Run a `dolt_*` dispatch against the store and render the outcome as a
/// `Value`. Errors surface as `Value::error_with_message`; the SQL layer
/// prefixes them with `Extension error: `, which the conformance error
/// matcher matches by substring. A lost compare-and-swap race (`DatabaseLocked`) is the one
/// case that must reach clients as SQLITE_BUSY rather than SQLITE_ERROR, so it
/// gets a Busy-coded error value; everything else stays on CustomError.
fn dispatch(state: &VcState, f: impl FnOnce(&mut VcStore) -> VersionResult<FuncValue>) -> Value {
    let mut store = state.store.lock().unwrap();
    match f(&mut store) {
        Ok(value) => func_value_to_value(value),
        Err(VersionError::DatabaseLocked) => Value::error_with_code_message(
            ResultCode::Busy,
            VersionError::DatabaseLocked.to_string(),
        ),
        Err(e) => Value::error_with_message(e.to_string()),
    }
}

fn args_slice<'a>(argc: i32, argv: *const Value) -> &'a [Value] {
    if argv.is_null() || argc <= 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(argv, argc as usize) }
    }
}

fn func_arg(v: &Value) -> FuncArg<'_> {
    match v.value_type() {
        ValueType::Text => FuncArg::Text(v.to_text().unwrap_or("")),
        ValueType::Integer => FuncArg::Integer(v.to_integer().unwrap_or(0)),
        _ => FuncArg::Null,
    }
}

fn func_value_to_value(v: FuncValue) -> Value {
    match v {
        FuncValue::Text(s) => Value::from_text(s),
        FuncValue::Integer(i) => Value::from_integer(i),
        FuncValue::Null => Value::null(),
    }
}

macro_rules! vc_shim {
    ($name:ident, $op:path) => {
        unsafe extern "C" fn $name(
            context: usize,
            argc: i32,
            argv: *const Value,
            _cd: Option<ContextDestructor>,
            _vd: Option<ValueDestructor>,
        ) -> Value {
            let state = unsafe { &*(context as *const VcState) };
            let args = args_slice(argc, argv);
            let fargs: Vec<FuncArg> = args.iter().map(func_arg).collect();
            dispatch(state, |store| $op(store, &fargs))
        }
    };
}

// All store-backed functions take `&mut VcStore`; guard_write runs inside the
// store methods, so read-only and mutating calls share one shim shape.
vc_shim!(dolt_add_shim, funcs::dolt_add);
vc_shim!(dolt_branch_shim, funcs::dolt_branch);
vc_shim!(dolt_tag_shim, funcs::dolt_tag);
vc_shim!(dolt_active_branch_shim, funcs::dolt_active_branch);
vc_shim!(dolt_hashof_shim, funcs::dolt_hashof);
vc_shim!(dolt_hashof_table_shim, funcs::dolt_hashof_table);
vc_shim!(dolt_hashof_db_shim, funcs::dolt_hashof_db);
vc_shim!(dolt_config_shim, funcs::dolt_config);
vc_shim!(dolt_status_shim, funcs::dolt_status);
vc_shim!(dolt_reset_shim, funcs::dolt_reset);
vc_shim!(dolt_clean_shim, funcs::dolt_clean);
vc_shim!(dolt_merge_base_shim, funcs::dolt_merge_base);

/// A checkout switches the working set to the new branch's committed content,
/// so the SQL tables rewrite to that branch's state before any capture.
unsafe extern "C" fn dolt_checkout_shim(
    context: usize,
    argc: i32,
    argv: *const Value,
    _cd: Option<ContextDestructor>,
    _vd: Option<ValueDestructor>,
) -> Value {
    let state = unsafe { &*(context as *const VcState) };
    let args = args_slice(argc, argv);
    let fargs: Vec<FuncArg> = args.iter().map(func_arg).collect();
    let result = dispatch(state, |store| funcs::dolt_checkout(store, &fargs));
    state.sync_work_to_head();
    if let Err(e) = state.sync_work_to_sql() {
        if result.value_type() != ValueType::Error {
            return Value::error_with_message(e.to_string());
        }
    }
    result
}

/// A write-path shim that first captures the current SQL content into the
/// store and, once the operation released the store lock, writes the merged
/// working set back to SQL. A write-back failure surfaces as the statement's
/// error unless the operation itself already failed.
macro_rules! vc_sync_shim {
    ($name:ident, $op:path, $capture:expr, $apply:expr) => {
        unsafe extern "C" fn $name(
            context: usize,
            argc: i32,
            argv: *const Value,
            _cd: Option<ContextDestructor>,
            _vd: Option<ValueDestructor>,
        ) -> Value {
            let state = unsafe { &*(context as *const VcState) };
            if $capture {
                if let Err(e) = state.sync_sql_to_work() {
                    return Value::error_with_message(e.to_string());
                }
            }
            let args = args_slice(argc, argv);
            let fargs: Vec<FuncArg> = args.iter().map(func_arg).collect();
            let result = dispatch(state, |store| $op(store, &fargs));
            if $apply {
                if let Err(e) = state.sync_work_to_sql() {
                    if result.value_type() != ValueType::Error {
                        return Value::error_with_message(e.to_string());
                    }
                }
            }
            result
        }
    };
}

vc_sync_shim!(dolt_merge_shim, funcs::dolt_merge, true, true);
vc_sync_shim!(dolt_cherry_pick_shim, funcs::dolt_cherry_pick, true, true);
vc_sync_shim!(dolt_revert_shim, funcs::dolt_revert, true, true);
vc_sync_shim!(dolt_rebase_shim, funcs::dolt_rebase, true, true);
vc_sync_shim!(
    dolt_conflicts_resolve_shim,
    funcs::dolt_conflicts_resolve,
    false,
    true
);
vc_sync_shim!(
    dolt_verify_constraints_shim,
    funcs::dolt_verify_constraints,
    true,
    false
);

/// The commit shim additionally holds the graph lock  across the plan→compare-and-swap
/// window. The in-memory store's own mutex already serializes the compare-and-swap; the
/// graph lock mirrors the future shared-store shape where the ref tip lives
/// outside this connection. SQL content is captured first so the new commit's
/// snapshots carry the real rows.
unsafe extern "C" fn dolt_commit_shim(
    context: usize,
    argc: i32,
    argv: *const Value,
    _cd: Option<ContextDestructor>,
    _vd: Option<ValueDestructor>,
) -> Value {
    let state = unsafe { &*(context as *const VcState) };
    if let Err(e) = state.sync_sql_to_work() {
        return Value::error_with_message(e.to_string());
    }
    let _graph = state.graph.lock().unwrap();
    let args = args_slice(argc, argv);
    let fargs: Vec<FuncArg> = args.iter().map(func_arg).collect();
    dispatch(state, |store| funcs::dolt_commit(store, &fargs))
}

// Stateless functions only see args.
macro_rules! vc_stateless_shim {
    ($name:ident, $op:path) => {
        unsafe extern "C" fn $name(
            _context: usize,
            argc: i32,
            argv: *const Value,
            _cd: Option<ContextDestructor>,
            _vd: Option<ValueDestructor>,
        ) -> Value {
            let args = args_slice(argc, argv);
            let fargs: Vec<FuncArg> = args.iter().map(func_arg).collect();
            match $op(&fargs) {
                Ok(value) => func_value_to_value(value),
                Err(e) => Value::error_with_message(e.to_string()),
            }
        }
    };
}

vc_stateless_shim!(dolt_version_shim, funcs::dolt_version);
vc_stateless_shim!(doltlite_engine_shim, funcs::doltlite_engine);

unsafe extern "C" fn drop_vc_state(context: usize) {
    if context != 0 {
        drop(unsafe { Arc::from_raw(context as *const VcState) });
    }
}

/// Register every `dolt_*` function against `api`, each sharing `state`.
/// Each registration boxes its own `Arc` clone as the opaque context; the
/// drop shim releases it exactly once when the function is unregistered.
pub fn register_vc_functions(api: &ExtensionApi, state: Arc<VcState>) {
    let functions: &[(&str, ScalarFunction)] = &[
        ("dolt_add", dolt_add_shim),
        ("dolt_commit", dolt_commit_shim),
        ("dolt_branch", dolt_branch_shim),
        ("dolt_checkout", dolt_checkout_shim),
        ("dolt_tag", dolt_tag_shim),
        ("dolt_active_branch", dolt_active_branch_shim),
        ("dolt_hashof", dolt_hashof_shim),
        ("dolt_hashof_table", dolt_hashof_table_shim),
        ("dolt_hashof_db", dolt_hashof_db_shim),
        ("dolt_config", dolt_config_shim),
        ("dolt_status", dolt_status_shim),
        ("dolt_reset", dolt_reset_shim),
        ("dolt_clean", dolt_clean_shim),
        ("dolt_merge", dolt_merge_shim),
        ("dolt_merge_base", dolt_merge_base_shim),
        ("dolt_cherry_pick", dolt_cherry_pick_shim),
        ("dolt_revert", dolt_revert_shim),
        ("dolt_rebase", dolt_rebase_shim),
        ("dolt_conflicts_resolve", dolt_conflicts_resolve_shim),
        ("dolt_verify_constraints", dolt_verify_constraints_shim),
        ("dolt_version", dolt_version_shim),
        ("doltlite_engine", doltlite_engine_shim),
    ];
    for (name, shim) in functions {
        let Ok(cname) = CString::new(*name) else {
            continue;
        };
        let context = Arc::into_raw(state.clone()) as usize;
        let rc = unsafe {
            (api.register_scalar_function)(
                api.ctx,
                cname.as_ptr(),
                -1,
                false,
                context,
                *shim,
                Some(drop_vc_state),
                None,
            )
        };
        if !rc.is_ok() {
            drop(unsafe { Arc::from_raw(context as *const VcState) });
        }
    }
}

// ============================================================================
// SQL <-> working-set sync helpers. The connection handle is borrowed from the
// owning core connection; statements are prepared and stepped, never run via
// the statement-run shortcut, so a nested call from inside a `SELECT dolt_*()`
// statement stays on the same connection.
// ============================================================================

/// One captured table: columns, pk, rows, and create SQL.
type TableCapture = (Vec<String>, Vec<String>, Vec<VcRow>, String);

/// Quote a table name for use inside an SQL identifier: double any embedded
/// double quote, the SQLite escape for a quoted identifier.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Read one table's columns, pk, rows, and create SQL from the connection.
/// A tracked table missing from SQL is an error, not a silent skip: capturing
/// an empty snapshot would let the merge fabricate a phantom table.
#[allow(clippy::arc_with_non_send_sync)]
fn read_table(conn: &crate::vtabs::Conn, table: &str) -> VersionResult<TableCapture> {
    let connection = Arc::new(crate::Connection::new(conn as *const crate::vtabs::Conn));
    let mut stmt = connection
        .prepare(&format!("PRAGMA table_info({})", quote_ident(table)))
        .map_err(|_| VersionError::TableNotFound(table.to_string()))?;
    let mut columns = Vec::new();
    let mut pk_ranked: Vec<(usize, String)> = Vec::new();
    while let crate::StepResult::Row = stmt.step() {
        let row = stmt.get_row();
        if let Some(name) = row.get(1).and_then(|v| v.to_text_coerced()) {
            columns.push(name.clone());
            let rank = row.get(5).and_then(|v| v.to_integer()).unwrap_or(0) as usize;
            if rank > 0 {
                pk_ranked.push((rank, name));
            }
        }
    }
    if columns.is_empty() {
        return Err(VersionError::TableNotFound(table.to_string()));
    }
    pk_ranked.sort_by_key(|(rank, _)| *rank);
    let pk: Vec<String> = pk_ranked.into_iter().map(|(_, name)| name).collect();
    let mut stmt = connection
        .prepare("SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?")
        .map_err(|_| VersionError::TableNotFound(table.to_string()))?;
    stmt.bind_at(
        std::num::NonZeroUsize::new(1).unwrap(),
        Value::from_text(table.to_string()),
    );
    let mut schema_sql = String::new();
    while let crate::StepResult::Row = stmt.step() {
        if let Some(sql) = stmt.get_row().first().and_then(|v| v.to_text_coerced()) {
            schema_sql = sql;
        }
    }
    let mut stmt = connection
        .prepare(&format!("SELECT * FROM {}", quote_ident(table)))
        .map_err(|_| VersionError::TableNotFound(table.to_string()))?;
    let mut rows = Vec::new();
    while let crate::StepResult::Row = stmt.step() {
        rows.push(VcRow::new(stmt.get_row().iter().map(value_to_vc).collect()));
    }
    Ok((columns, pk, rows, schema_sql))
}

/// Replace one table's SQL content with the snapshot: drop the old rows (or
/// Write one table's snapshot directly, without a per-table SAVEPOINT.
/// The caller (sync_write_back) holds an outer SAVEPOINT that covers the
/// whole write-back, so a failure here rolls back every table atomically.
#[allow(clippy::arc_with_non_send_sync)]
fn write_table_snapshot(
    connection: &Arc<crate::Connection>,
    table: &str,
    snap: &turso_versioning::staging::TableSnapshot,
) -> VersionResult<()> {
    write_table_body(connection, table, snap)
}

/// The delete-or-create and insert steps of a table rewrite.
fn write_table_body(
    connection: &Arc<crate::Connection>,
    table: &str,
    snap: &turso_versioning::staging::TableSnapshot,
) -> VersionResult<()> {
    let exists = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name = ?")
        .map_err(|_| VersionError::WorkWrite(format!("table {table}")))?;
    exists.bind_at(
        std::num::NonZeroUsize::new(1).unwrap(),
        Value::from_text(table.to_string()),
    );
    let table_exists = matches!(exists.step(), crate::StepResult::Row);
    drop(exists);
    if table_exists {
        execute_sql(
            connection,
            &format!("DELETE FROM {}", quote_ident(table)),
            Vec::new(),
        )?;
    } else {
        let sql = if !snap.schema_sql.is_empty() {
            snap.schema_sql.clone()
        } else {
            let cols = snap
                .columns
                .iter()
                .map(|c| format!("{} TEXT", quote_ident(c)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("CREATE TABLE {} ({cols})", quote_ident(table))
        };
        execute_sql(connection, &sql, Vec::new())?;
    }
    if snap.rows.is_empty() {
        return Ok(());
    }
    let cols = snap
        .columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = vec!["?"; snap.columns.len()].join(", ");
    let insert = format!(
        "INSERT INTO {} ({cols}) VALUES ({placeholders})",
        quote_ident(table)
    );
    for row in &snap.rows {
        let args: Vec<Value> = row.values.iter().map(vc_to_value).collect();
        execute_sql(connection, &insert, args)?;
    }
    Ok(())
}

/// Write-back helper: apply resolutions then write dirty tables. Called from
/// within the outer SAVEPOINT of `sync_work_to_sql`.
fn sync_write_back(
    connection: &Arc<crate::Connection>,
    resolutions: &[(String, Vec<VcValue>, Option<VcRow>)],
    changes: &[(String, turso_versioning::staging::TableSnapshot)],
) -> VersionResult<()> {
    for (table, pk, image) in resolutions {
        apply_resolution_image_conn(connection, table, pk, image.as_ref())?;
    }
    for (table, snap) in changes {
        write_table_snapshot(connection, table, snap)?;
    }
    Ok(())
}

/// Apply one resolved conflict's row image using an existing connection,
/// for use inside the outer SAVEPOINT of `sync_work_to_sql`.
fn apply_resolution_image_conn(
    connection: &Arc<crate::Connection>,
    table: &str,
    pk: &[VcValue],
    image: Option<&VcRow>,
) -> VersionResult<()> {
    let mut stmt = connection
        .prepare(&format!("PRAGMA table_info({})", quote_ident(table)))
        .map_err(|_| VersionError::WorkWrite(format!("resolve image for {table}")))?;
    let mut columns = Vec::new();
    let mut pk_ranked: Vec<(usize, String)> = Vec::new();
    while let crate::StepResult::Row = stmt.step() {
        let row = stmt.get_row();
        if let Some(name) = row.get(1).and_then(|v| v.to_text_coerced()) {
            columns.push(name.clone());
            let rank = row.get(5).and_then(|v| v.to_integer()).unwrap_or(0) as usize;
            if rank > 0 {
                pk_ranked.push((rank, name));
            }
        }
    }
    pk_ranked.sort_by_key(|(rank, _)| *rank);
    let pk_names: Vec<String> = pk_ranked.into_iter().map(|(_, name)| name).collect();
    let positions: Vec<usize> = pk_names
        .iter()
        .filter_map(|name| columns.iter().position(|c| c == name))
        .collect();
    if positions.len() != pk.len() {
        return Err(VersionError::WorkWrite(format!(
            "resolve image for {table} has {} pk columns, want {}",
            positions.len(),
            pk.len()
        )));
    }
    let where_clause = positions
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(" AND ");
    let delete = format!("DELETE FROM {} WHERE {}", quote_ident(table), where_clause);
    let pk_args: Vec<Value> = pk.iter().map(vc_to_value).collect();
    execute_sql(connection, &delete, pk_args)?;
    if let Some(image) = image {
        let cols_list = columns
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let placeholders = vec!["?"; columns.len()].join(", ");
        let insert = format!(
            "INSERT INTO {} ({cols_list}) VALUES ({placeholders})",
            quote_ident(table)
        );
        let args: Vec<Value> = image.values.iter().map(vc_to_value).collect();
        execute_sql(connection, &insert, args)?;
    }
    Ok(())
}

/// Prepare, bind, and step a statement to completion.
fn execute_sql(
    connection: &Arc<crate::Connection>,
    sql: &str,
    args: Vec<Value>,
) -> VersionResult<()> {
    let stmt = connection
        .prepare(sql)
        .map_err(|_| VersionError::WorkWrite(sql.to_string()))?;
    for (i, v) in args.into_iter().enumerate() {
        stmt.bind_at(std::num::NonZeroUsize::new(i + 1).unwrap(), v);
    }
    loop {
        match stmt.step() {
            crate::StepResult::Row => {}
            crate::StepResult::Done => return Ok(()),
            _ => return Err(VersionError::WorkWrite(sql.to_string())),
        }
    }
}

fn value_to_vc(v: &Value) -> VcValue {
    match v.value_type() {
        ValueType::Integer => VcValue::Integer(v.to_integer().unwrap_or(0)),
        ValueType::Text => VcValue::Text(v.to_text().unwrap_or("").to_string()),
        _ => VcValue::Null,
    }
}

fn vc_to_value(v: &VcValue) -> Value {
    match v {
        VcValue::Null => Value::null(),
        VcValue::Integer(i) => Value::from_integer(*i),
        VcValue::Text(s) => Value::from_text(s.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn func_arg_conversion() {
        assert_eq!(
            func_arg(&Value::from_text("hi".into())),
            FuncArg::Text("hi")
        );
        assert_eq!(func_arg(&Value::from_integer(7)), FuncArg::Integer(7));
        assert_eq!(func_arg(&Value::null()), FuncArg::Null);
    }

    #[test]
    fn func_value_conversion() {
        let text = func_value_to_value(FuncValue::Text("x".into()));
        assert_eq!(text.to_text(), Some("x"));
        let int = func_value_to_value(FuncValue::Integer(3));
        assert_eq!(int.to_integer(), Some(3));
        assert_eq!(
            func_value_to_value(FuncValue::Null).value_type(),
            ValueType::Null
        );
    }

    #[test]
    fn dispatch_renders_errors_as_messages() {
        let state = VcState::new();
        let value = dispatch(&state, |store| {
            funcs::dolt_add(store, &[FuncArg::Text("ghost")])
        });
        let (code, msg) = value.to_error_details().unwrap();
        assert_eq!(code, crate::ResultCode::CustomError);
        assert_eq!(msg.as_deref(), Some("table not found: ghost"));

        let busy = dispatch(&state, |_store| Err(VersionError::DatabaseLocked));
        let (code, msg) = busy.to_error_details().unwrap();
        assert_eq!(code, crate::ResultCode::Busy);
        assert_eq!(msg.as_deref(), Some("database is locked"));
    }

    #[test]
    fn vc_state_tracks_tables_and_detaches() {
        let state = VcState::new();
        state.track_table("t1");
        assert_eq!(state.active_branch().as_deref(), Some("main"));
        assert!(state.guard_write().is_ok());
        let tip = CommitId([0x11; 20]);
        state.open_detached(tip);
        assert!(state.active_branch().is_none());
        assert_eq!(
            state.guard_write().unwrap_err().to_string(),
            "cannot write in detached HEAD state"
        );
        state.open_branch("main").unwrap();
        assert_eq!(state.active_branch().as_deref(), Some("main"));
        assert!(state.guard_write().is_ok());
    }
}
