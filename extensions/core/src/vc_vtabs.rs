//! Live engine glue for the version-control virtual tables.
//!
//! Thin by design: every row comes from the pure `turso_versioning` readers
//! over the connection's `VcStore` (reached through the cursor's connection,
//! never a global). Schemas and pushdown plans are the pure layer's; this
//! file only maps constraints, renders `Value`s, and surfaces errors.
//!
//! table-valued-function arguments arrive as hidden-column equality constraints in hidden
//! order, so `dolt_diff_stat('a','b')` reads as `from_ref='a', to_ref='b'`.

use std::sync::Arc;

use turso_versioning::vtab_diff::{
    diff_rows, diff_stat_rows, diff_summary_rows, diff_table_rows, diff_table_schema, patch_rows,
    plan_diff, plan_refs, schema_diff_rows, split_range, DOLT_DIFF_SCHEMA, DOLT_DIFF_STAT_SCHEMA,
    DOLT_DIFF_SUMMARY_SCHEMA, DOLT_PATCH_SCHEMA, DOLT_SCHEMA_DIFF_SCHEMA,
};
use turso_versioning::vtab_history::{
    at_rows, at_schema, blame_rows, blame_schema, history_rows, history_schema, plan_schemas,
    schemas_rows, SchemaObject,
};
use turso_versioning::vtab_log::{
    log_probe, log_rows, plan_log, VcConstraint, VcOp, VcOrderBy, VcPlan, VcRow, VcValue,
    DOLT_LOG_SCHEMA,
};

use crate::versioning::VcState;
use crate::{
    Connection, ConstraintInfo, ConstraintOp, ConstraintUsage, ExtensionApi, IndexInfo,
    OrderByInfo, ResultCode, StepResult, VTabCursor, VTabKind, VTabModule, VTabModuleDerive,
    VTable, Value,
};

/// Declared schemas for the merge/conflict/violation surfaces.
const DOLT_MERGE_STATUS_SCHEMA: &str =
    "CREATE TABLE dolt_merge_status (table_name TEXT, rows_merged INTEGER, rows_conflicted INTEGER, schema_conflict INTEGER, state TEXT)";

/// The conflicts union renders every table's rows as text cells; per-table
/// `dolt_conflicts_<t>` vtables carry the live column names instead.
const DOLT_CONFLICTS_SCHEMA: &str =
    "CREATE TABLE dolt_conflicts (table_name TEXT, pk_values TEXT, base_values TEXT, our_values TEXT, their_values TEXT, conflict_type TEXT)";

const DOLT_CONSTRAINT_VIOLATIONS_SCHEMA: &str =
    "CREATE TABLE dolt_constraint_violations (table_name TEXT, violation_type TEXT, pk_cols TEXT, details TEXT)";

/// Register every VC vtable module against `api`. Cursor state flows through
/// the opening connection, so registration itself needs no store handle.
pub fn register_vc_vtabs(api: &ExtensionApi) {
    unsafe {
        DoltLogModule::register_DoltLogModule(api);
        DoltSchemasModule::register_DoltSchemasModule(api);
        DoltDiffModule::register_DoltDiffModule(api);
        DoltDiffStatModule::register_DoltDiffStatModule(api);
        DoltDiffSummaryModule::register_DoltDiffSummaryModule(api);
        DoltSchemaDiffModule::register_DoltSchemaDiffModule(api);
        DoltPatchModule::register_DoltPatchModule(api);
        DoltMergeStatusModule::register_DoltMergeStatusModule(api);
        DoltConflictsModule::register_DoltConflictsModule(api);
        DoltConstraintViolationsModule::register_DoltConstraintViolationsModule(api);
        DoltBranchesModule::register_DoltBranchesModule(api);
        DoltTagsModule::register_DoltTagsModule(api);
        DoltRemotesModule::register_DoltRemotesModule(api);
        DoltRemoteBranchesModule::register_DoltRemoteBranchesModule(api);
    }
}

/// Register the per-table version-control modules for one user table:
/// `dolt_history_<t>`, `dolt_at_<t>`, `dolt_blame_<t>`, `dolt_diff_<t>`, and
/// `dolt_conflicts_<t>`. Registration carries the connection, so each module's
/// `create` resolves the table's live columns from the schema.
pub fn register_table_modules(api: &ExtensionApi, table: &str) {
    unsafe {
        register_history_t(api, &format!("dolt_history_{table}"));
        register_at_t(api, &format!("dolt_at_{table}"));
        register_blame_t(api, &format!("dolt_blame_{table}"));
        register_diff_t(api, &format!("dolt_diff_{table}"));
        register_conflicts_t(api, &format!("dolt_conflicts_{table}"));
        register_constraint_violations_t(api, &format!("dolt_constraint_violations_{table}"));
    }
}

/// Wire one per-table module under a runtime name, referencing the
/// derive-generated FFI functions (their `NAME` constant cannot vary).
macro_rules! per_table_module {
    ($name:expr, $create:path, $open:path, $close:path, $filter:path, $column:path,
     $next:path, $eof:path, $update:path, $rowid:path, $destroy:path, $best_idx:path,
     $begin:path, $rollback:path, $commit:path, $rename:path) => {{
        // `into_raw` hands the name to `register_vtab_module`, which frees it
        // through `CString::from_raw`; `as_ptr` would dangle before the call.
        let cname = std::ffi::CString::new($name).unwrap().into_raw();
        crate::VTabModuleImpl {
            name: cname,
            readonly: true,
            create: $create,
            open: $open,
            close: $close,
            filter: $filter,
            column: $column,
            next: $next,
            eof: $eof,
            update: $update,
            rowid: $rowid,
            destroy: $destroy,
            best_idx: $best_idx,
            begin: $begin,
            rollback: $rollback,
            commit: $commit,
            rename: $rename,
        }
    }};
}

unsafe fn register_history_t(api: &ExtensionApi, name: &str) {
    let module = per_table_module!(
        name,
        DoltHistoryTModule::create_DoltHistoryTModule,
        DoltHistoryTModule::open_DoltHistoryTModule,
        DoltHistoryTModule::close_DoltHistoryTModule,
        DoltHistoryTModule::filter_DoltHistoryTModule,
        DoltHistoryTModule::column_DoltHistoryTModule,
        DoltHistoryTModule::next_DoltHistoryTModule,
        DoltHistoryTModule::eof_DoltHistoryTModule,
        DoltHistoryTModule::update_DoltHistoryTModule,
        DoltHistoryTModule::rowid_DoltHistoryTModule,
        DoltHistoryTModule::destroy_DoltHistoryTModule,
        DoltHistoryTModule::best_idx_DoltHistoryTModule,
        DoltHistoryTModule::begin_DoltHistoryTModule,
        DoltHistoryTModule::rollback_DoltHistoryTModule,
        DoltHistoryTModule::commit_DoltHistoryTModule,
        DoltHistoryTModule::rename_DoltHistoryTModule
    );
    (api.register_vtab_module)(api.ctx, module.name, module, VTabKind::TableValuedFunction);
}

unsafe fn register_at_t(api: &ExtensionApi, name: &str) {
    let module = per_table_module!(
        name,
        DoltAtTModule::create_DoltAtTModule,
        DoltAtTModule::open_DoltAtTModule,
        DoltAtTModule::close_DoltAtTModule,
        DoltAtTModule::filter_DoltAtTModule,
        DoltAtTModule::column_DoltAtTModule,
        DoltAtTModule::next_DoltAtTModule,
        DoltAtTModule::eof_DoltAtTModule,
        DoltAtTModule::update_DoltAtTModule,
        DoltAtTModule::rowid_DoltAtTModule,
        DoltAtTModule::destroy_DoltAtTModule,
        DoltAtTModule::best_idx_DoltAtTModule,
        DoltAtTModule::begin_DoltAtTModule,
        DoltAtTModule::rollback_DoltAtTModule,
        DoltAtTModule::commit_DoltAtTModule,
        DoltAtTModule::rename_DoltAtTModule
    );
    (api.register_vtab_module)(api.ctx, module.name, module, VTabKind::TableValuedFunction);
}

unsafe fn register_blame_t(api: &ExtensionApi, name: &str) {
    let module = per_table_module!(
        name,
        DoltBlameTModule::create_DoltBlameTModule,
        DoltBlameTModule::open_DoltBlameTModule,
        DoltBlameTModule::close_DoltBlameTModule,
        DoltBlameTModule::filter_DoltBlameTModule,
        DoltBlameTModule::column_DoltBlameTModule,
        DoltBlameTModule::next_DoltBlameTModule,
        DoltBlameTModule::eof_DoltBlameTModule,
        DoltBlameTModule::update_DoltBlameTModule,
        DoltBlameTModule::rowid_DoltBlameTModule,
        DoltBlameTModule::destroy_DoltBlameTModule,
        DoltBlameTModule::best_idx_DoltBlameTModule,
        DoltBlameTModule::begin_DoltBlameTModule,
        DoltBlameTModule::rollback_DoltBlameTModule,
        DoltBlameTModule::commit_DoltBlameTModule,
        DoltBlameTModule::rename_DoltBlameTModule
    );
    (api.register_vtab_module)(api.ctx, module.name, module, VTabKind::TableValuedFunction);
}

unsafe fn register_diff_t(api: &ExtensionApi, name: &str) {
    let module = per_table_module!(
        name,
        DoltDiffTModule::create_DoltDiffTModule,
        DoltDiffTModule::open_DoltDiffTModule,
        DoltDiffTModule::close_DoltDiffTModule,
        DoltDiffTModule::filter_DoltDiffTModule,
        DoltDiffTModule::column_DoltDiffTModule,
        DoltDiffTModule::next_DoltDiffTModule,
        DoltDiffTModule::eof_DoltDiffTModule,
        DoltDiffTModule::update_DoltDiffTModule,
        DoltDiffTModule::rowid_DoltDiffTModule,
        DoltDiffTModule::destroy_DoltDiffTModule,
        DoltDiffTModule::best_idx_DoltDiffTModule,
        DoltDiffTModule::begin_DoltDiffTModule,
        DoltDiffTModule::rollback_DoltDiffTModule,
        DoltDiffTModule::commit_DoltDiffTModule,
        DoltDiffTModule::rename_DoltDiffTModule
    );
    (api.register_vtab_module)(api.ctx, module.name, module, VTabKind::TableValuedFunction);
}

unsafe fn register_conflicts_t(api: &ExtensionApi, name: &str) {
    let module = per_table_module!(
        name,
        DoltConflictsTModule::create_DoltConflictsTModule,
        DoltConflictsTModule::open_DoltConflictsTModule,
        DoltConflictsTModule::close_DoltConflictsTModule,
        DoltConflictsTModule::filter_DoltConflictsTModule,
        DoltConflictsTModule::column_DoltConflictsTModule,
        DoltConflictsTModule::next_DoltConflictsTModule,
        DoltConflictsTModule::eof_DoltConflictsTModule,
        DoltConflictsTModule::update_DoltConflictsTModule,
        DoltConflictsTModule::rowid_DoltConflictsTModule,
        DoltConflictsTModule::destroy_DoltConflictsTModule,
        DoltConflictsTModule::best_idx_DoltConflictsTModule,
        DoltConflictsTModule::begin_DoltConflictsTModule,
        DoltConflictsTModule::rollback_DoltConflictsTModule,
        DoltConflictsTModule::commit_DoltConflictsTModule,
        DoltConflictsTModule::rename_DoltConflictsTModule
    );
    (api.register_vtab_module)(api.ctx, module.name, module, VTabKind::TableValuedFunction);
}

unsafe fn register_constraint_violations_t(api: &ExtensionApi, name: &str) {
    let module = per_table_module!(
        name,
        DoltConstraintViolationsTModule::create_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::open_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::close_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::filter_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::column_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::next_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::eof_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::update_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::rowid_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::destroy_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::best_idx_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::begin_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::rollback_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::commit_DoltConstraintViolationsTModule,
        DoltConstraintViolationsTModule::rename_DoltConstraintViolationsTModule
    );
    (api.register_vtab_module)(api.ctx, module.name, module, VTabKind::TableValuedFunction);
}

/// Which vtable a cursor scans. Stored at open; `filter` matches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VcKind {
    Log,
    Schemas,
    Diff,
    DiffStat,
    DiffSummary,
    SchemaDiff,
    Patch,
    MergeStatus,
    Conflicts,
    Violations,
    Branches,
    Tags,
    Remotes,
    RemoteBranches,
    HistoryT,
    AtT,
    BlameT,
    DiffT,
    ConflictsT,
    ViolationsT,
}

/// Shared table handle: the kind selects schema, plan, and row builder.
struct VcTable {
    kind: VcKind,
}

/// Shared cursor: materialized rows plus the error-delivery state.
///
/// A failed `filter` cannot return its message through `ResultCode`, so the
/// cursor reports not-EOF once with zero rows; the engine's first `column`
/// call then surfaces the exact message, and `next` ends the scan.
struct VcCursor {
    kind: VcKind,
    table: String,
    state: Option<Arc<VcState>>,
    conn: Option<Arc<Connection>>,
    rows: Vec<Vec<Cell>>,
    pos: usize,
    error: Option<String>,
    delivered: bool,
}

impl VcCursor {
    fn result_kind(&self) -> VcKind {
        self.kind
    }
}

impl VTable for VcTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        Ok(VcCursor {
            kind: self.kind,
            table: String::new(),
            state: conn.as_ref().and_then(|c| c.versioning()),
            conn,
            rows: Vec::new(),
            pos: 0,
            error: None,
            delivered: false,
        })
    }
}

impl VTabCursor for VcCursor {
    type Error = String;

    fn filter(&mut self, args: &[Value], idx_info: Option<(&str, i32)>) -> ResultCode {
        let (idx_num, idx_str) = idx_info
            .map(|(s, n)| (n, s.to_string()))
            .unwrap_or((0, String::new()));
        let materialized = build_rows(
            self.result_kind(),
            &self.table,
            self.state.clone(),
            self.conn.clone(),
            idx_num,
            &idx_str,
            args,
        );
        match materialized {
            Ok(rows) => {
                self.rows = rows;
                self.error = None;
            }
            Err(msg) => {
                self.rows = Vec::new();
                self.error = Some(msg);
            }
        }
        self.pos = 0;
        self.delivered = false;
        // The engine drives the scan from these return codes: EOF here skips
        // the scan body, EOF from `next` ends it. An error still reports OK
        // so the first `column` call surfaces the exact message.
        if self.error.is_none() && self.rows.is_empty() {
            return ResultCode::EOF;
        }
        ResultCode::OK
    }

    fn rowid(&self) -> i64 {
        self.pos as i64 + 1
    }

    fn column(&self, idx: u32) -> Result<Value, Self::Error> {
        if let Some(msg) = &self.error {
            if !self.delivered {
                return Err(msg.clone());
            }
            return Ok(Value::null());
        }
        Ok(self
            .rows
            .get(self.pos)
            .and_then(|r| r.get(idx as usize))
            .map(cell_to_value)
            .unwrap_or_default())
    }

    fn eof(&self) -> bool {
        if self.error.is_some() && !self.delivered {
            return false;
        }
        self.pos >= self.rows.len()
    }

    fn next(&mut self) -> ResultCode {
        if self.error.is_some() {
            self.delivered = true;
            return ResultCode::Error;
        }
        if self.pos + 1 >= self.rows.len() {
            self.pos = self.rows.len();
            return ResultCode::EOF;
        }
        self.pos += 1;
        ResultCode::OK
    }
}

/// Build one scan's rows. Errors carry the exact versioning message, which
/// the cursor delivers through its first `column` call.
fn build_rows(
    kind: VcKind,
    table: &str,
    state: Option<Arc<VcState>>,
    conn: Option<Arc<Connection>>,
    idx_num: i32,
    idx_str: &str,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    match kind {
        VcKind::Log => build_log(state, idx_num, args),
        VcKind::Schemas => build_schemas(conn, idx_num, args),
        VcKind::Diff => build_diff(state, idx_num, args),
        VcKind::DiffStat => build_refs(state, idx_str, args, build_stat),
        VcKind::DiffSummary => build_refs(state, idx_str, args, build_summary),
        VcKind::SchemaDiff => build_refs(state, idx_str, args, build_schema_diff),
        VcKind::Patch => build_patch(state, idx_str, args),
        VcKind::MergeStatus => build_merge_status(state),
        VcKind::Conflicts => build_conflicts(state, idx_num, args),
        VcKind::Violations => build_violations(state, idx_num, args),
        VcKind::Branches => build_branches(state, conn),
        VcKind::Tags => build_tags(state),
        VcKind::Remotes => build_remotes(state),
        VcKind::RemoteBranches => build_remote_branches(state, idx_num, args),
        VcKind::HistoryT => build_history_t(state, table, idx_str, args),
        VcKind::AtT => build_at_t(state, table, idx_str, args),
        VcKind::BlameT => build_blame_t(state, table, idx_str, args),
        VcKind::DiffT => build_diff_t(state, table, idx_str, args),
        VcKind::ConflictsT => build_conflicts_t(state, conn, table),
        VcKind::ViolationsT => build_violations_t(state, table),
    }
}

fn build_log(
    state: Option<Arc<VcState>>,
    idx_num: i32,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    state.with_store(|store| {
        let rows = match idx_num {
            1 => {
                let hash = arg_text(args, 0).unwrap_or_default();
                log_probe(store, &hash)
                    .map(|row| row.into_iter().collect())
                    .map_err(err_string)?
            }
            2 => {
                let spec = arg_text(args, 0).unwrap_or_default();
                log_rows(store, Some(&spec)).map_err(err_string)?
            }
            // A fresh branch with no commits yet reads as empty history, not
            // an error. Explicit revisions still resolve (and fail) above.
            _ if store.head_commit().is_none() && store.active_branch().is_some() => Vec::new(),
            _ => log_rows(store, None).map_err(err_string)?,
        };
        Ok(rows
            .into_iter()
            .map(|r| {
                vec![
                    text(r.hash),
                    text(r.committer),
                    text(r.email),
                    text(r.date),
                    text(r.message),
                    text(r.revision),
                ]
            })
            .collect())
    })
}

fn build_schemas(
    conn: Option<Arc<Connection>>,
    idx_num: i32,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(conn) = conn else {
        return Ok(Vec::new());
    };
    let mut stmt = conn
        .prepare("SELECT type, name, sql FROM sqlite_schema WHERE type IN ('view','trigger') ORDER BY name")
        .map_err(|_| "failed to read schema".to_string())?;
    let mut objects = Vec::new();
    loop {
        match stmt.step() {
            StepResult::Row => {
                let row = stmt.get_row();
                let kind = row
                    .first()
                    .and_then(|v| v.to_text_coerced())
                    .unwrap_or_default();
                let name = row
                    .get(1)
                    .and_then(|v| v.to_text_coerced())
                    .unwrap_or_default();
                let sql = row
                    .get(2)
                    .and_then(|v| v.to_text_coerced())
                    .unwrap_or_default();
                objects.push(SchemaObject { kind, name, sql });
            }
            StepResult::Done => break,
            _ => return Err("failed to read schema".to_string()),
        }
    }
    let mut rows = schemas_rows(&objects);
    match idx_num {
        1 => retain(&mut rows, args, 0, |r| r.kind.clone()),
        2 => retain(&mut rows, args, 0, |r| r.name.clone()),
        _ => {}
    }
    Ok(rows
        .into_iter()
        .map(|r| {
            vec![
                text(r.kind),
                text(r.name),
                text(r.fragment),
                Cell::Null,
                Cell::Null,
            ]
        })
        .collect())
}

fn build_diff(
    state: Option<Arc<VcState>>,
    idx_num: i32,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    state.with_store(|store| {
        let mut rows = diff_rows(store).map_err(err_string)?;
        match idx_num {
            1 => retain(&mut rows, args, 0, |r| r.hash.clone()),
            2 => retain(&mut rows, args, 0, |r| r.table_name.clone()),
            _ => {}
        }
        Ok(rows
            .into_iter()
            .map(|r| {
                vec![
                    text(r.hash),
                    text(r.committer),
                    text(r.email),
                    text(r.date),
                    text(r.message),
                    int(r.data_change as i64),
                    int(r.schema_change as i64),
                    text(r.table_name),
                ]
            })
            .collect())
    })
}

fn build_stat(
    store: &turso_versioning::staging::VcStore,
    from: &str,
    to: &str,
    table: Option<&str>,
) -> Result<Vec<Vec<Cell>>, String> {
    let rows = diff_stat_rows(store, from, to, table).map_err(err_string)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            vec![
                text(r.table.clone()),
                int(r.rows_unmodified),
                int(r.rows_added),
                int(r.rows_deleted),
                int(r.rows_modified),
                int(r.cells_added),
                int(r.cells_deleted),
                int(r.cells_modified),
                int(r.old_rows),
                int(r.new_rows),
                int(r.old_cells),
                int(r.new_cells),
                text(from.to_string()),
                text(to.to_string()),
                text(table.unwrap_or("").to_string()),
            ]
        })
        .collect())
}

fn build_summary(
    store: &turso_versioning::staging::VcStore,
    from: &str,
    to: &str,
    table: Option<&str>,
) -> Result<Vec<Vec<Cell>>, String> {
    let rows = diff_summary_rows(store, from, to, table).map_err(err_string)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            vec![
                text(r.from_table.clone()),
                text(r.to_table.clone()),
                text(r.diff_type.clone()),
                int(r.data_change as i64),
                int(r.schema_change as i64),
                text(from.to_string()),
                text(to.to_string()),
                text(table.unwrap_or("").to_string()),
            ]
        })
        .collect())
}

fn build_schema_diff(
    store: &turso_versioning::staging::VcStore,
    from: &str,
    to: &str,
    table: Option<&str>,
) -> Result<Vec<Vec<Cell>>, String> {
    if from.is_empty() || to.is_empty() {
        return Ok(Vec::new());
    }
    let rows = schema_diff_rows(store, from, to, table).map_err(err_string)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            vec![
                text(r.from_table.clone()),
                text(r.to_table.clone()),
                text(r.from_create.clone()),
                text(r.to_create.clone()),
                text(from.to_string()),
                text(to.to_string()),
                text(table.unwrap_or("").to_string()),
            ]
        })
        .collect())
}

fn build_patch(
    state: Option<Arc<VcState>>,
    idx_str: &str,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    let bound = bind_ref_args(idx_str, args);
    let (from, to, table) = match bound.as_slice() {
        [from] if from.contains("..") => (from.clone(), String::new(), None),
        [from] => (from.clone(), from.clone(), None),
        [from, to] => (from.clone(), to.clone(), None),
        [from, to, table] => (from.clone(), to.clone(), Some(table.clone())),
        _ => return Ok(Vec::new()),
    };
    state.with_store(|store| {
        let (from, to) = if to.is_empty() {
            split_range(store, &from).map_err(err_string)?
        } else {
            (from, to)
        };
        let echo: Vec<Cell> = [from.clone(), to.clone(), table.clone().unwrap_or_default()]
            .into_iter()
            .map(text)
            .collect();
        let rows = patch_rows(store, &from, &to, table.as_deref()).map_err(err_string)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let mut out = vec![
                    int(r.order),
                    text(r.from_commit.clone()),
                    text(r.to_commit.clone()),
                    text(r.table.clone()),
                    text(r.diff_type.clone()),
                    text(r.statement.clone()),
                ];
                out.extend(echo.clone());
                out
            })
            .collect())
    })
}

/// Shared `from_ref`/`to_ref`/`tbl` table-valued-function path for stat, summary, and schema
/// diff. Both refs are required; a missing side reads as zero rows.
type RefsBuilder = fn(
    &turso_versioning::staging::VcStore,
    &str,
    &str,
    Option<&str>,
) -> Result<Vec<Vec<Cell>>, String>;

fn build_refs(
    state: Option<Arc<VcState>>,
    idx_str: &str,
    args: &[Value],
    build: RefsBuilder,
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    let bound = bind_ref_args(idx_str, args);
    let (from, to, table) = match bound.as_slice() {
        [from, to] => (from.clone(), to.clone(), None),
        [from, to, table] => (from.clone(), to.clone(), Some(table.clone())),
        _ => return Ok(Vec::new()),
    };
    if from.is_empty() || to.is_empty() {
        return Ok(Vec::new());
    }
    state.with_store(|store| build(store, &from, &to, table.as_deref()))
}

// ============================================================================
// Merge status / conflicts / violations builders.
// ============================================================================

fn build_merge_status(state: Option<Arc<VcState>>) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    state.with_store(|store| {
        Ok(store
            .merge_status_rows()
            .into_iter()
            .map(|r| {
                vec![
                    text(r.table),
                    int(r.rows_merged),
                    int(r.rows_conflicted),
                    int(r.schema_conflict),
                    text(r.state),
                ]
            })
            .collect())
    })
}

fn build_branches(
    state: Option<Arc<VcState>>,
    conn: Option<Arc<Connection>>,
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    // `dirty` mirrors the SQL tables, so refresh the working set before the
    // scan reports cleanliness. Failures read as a clean tree rather than an
    // error: the listing itself stays useful.
    if conn.is_some() {
        let _ = state.sync_sql_to_work();
    }
    state.with_store(|store| {
        Ok(turso_versioning::vtab_refs::branches_rows(store)
            .into_iter()
            .map(|r| {
                vec![
                    text(r.name),
                    text(r.hash),
                    text(r.latest_commit_message),
                    text(r.remote),
                    int(r.branch as i64),
                    int(r.dirty as i64),
                ]
            })
            .collect())
    })
}

fn build_tags(state: Option<Arc<VcState>>) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    state.with_store(|store| {
        Ok(turso_versioning::vtab_refs::tags_rows(store)
            .into_iter()
            .map(|r| vec![text(r.tag_name), text(r.tag_hash), text(r.message)])
            .collect())
    })
}

fn build_remotes(state: Option<Arc<VcState>>) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    state.with_store(|store| {
        Ok(turso_versioning::vtab_refs::remotes_rows(store)
            .into_iter()
            .map(|row| {
                vec![
                    text(row.name),
                    text(row.url),
                    text(row.fetch_specs),
                    text(row.params),
                ]
            })
            .collect())
    })
}

fn build_remote_branches(
    state: Option<Arc<VcState>>,
    idx_num: i32,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    let prefix = (idx_num == 1).then(|| arg_text(args, 0)).flatten();
    state.with_store(|store| {
        Ok(
            turso_versioning::vtab_refs::remote_branches_rows(store, prefix.as_deref())
                .into_iter()
                .map(|row| {
                    vec![
                        text(row.name),
                        text(row.hash),
                        text(row.latest_commit_message),
                        text(prefix.clone().unwrap_or_default()),
                    ]
                })
                .collect(),
        )
    })
}

fn build_conflicts(
    state: Option<Arc<VcState>>,
    idx_num: i32,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    state.with_store(|store| {
        let mut entries = store.conflict_entries();
        if idx_num == 1 {
            if let Some(want) = arg_text(args, 0) {
                entries.retain(|e| e.table == want);
            }
        }
        Ok(entries
            .into_iter()
            .map(|e| {
                let kind = match &e.kind {
                    turso_versioning::conflicts::ConflictKind::Rows => "rows",
                    turso_versioning::conflicts::ConflictKind::Schema(_) => "schema",
                };
                vec![
                    text(e.table.clone()),
                    text(join_values(&e.pk)),
                    text(row_text(e.base.as_ref())),
                    text(row_text(e.ours.as_ref())),
                    text(row_text(e.theirs.as_ref())),
                    text(kind.to_string()),
                ]
            })
            .collect())
    })
}

fn build_violations(
    state: Option<Arc<VcState>>,
    idx_num: i32,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    state.with_store(|store| {
        let mut violations = store.violation_entries();
        if idx_num == 1 {
            if let Some(want) = arg_text(args, 0) {
                violations.retain(|v| v.table == want);
            }
        }
        Ok(violations
            .into_iter()
            .map(|v| {
                vec![
                    text(v.table.clone()),
                    text(v.kind.to_string()),
                    text(join_values(&v.row_pk)),
                    text(v.detail.clone()),
                ]
            })
            .collect())
    })
}

// ============================================================================
// Per-table builders. Rows mirror the declared schema exactly, hidden columns
// included (the engine hides them from `SELECT *`).
// ============================================================================

fn build_history_t(
    state: Option<Arc<VcState>>,
    table: &str,
    idx_str: &str,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    let bound = bind_ref_cols(idx_str, args);
    let start_ref = bound
        .iter()
        .max_by_key(|(col, _)| *col)
        .map(|(_, v)| v.clone());
    state.with_store(|store| {
        let rows = history_rows(store, table, start_ref.as_deref()).map_err(err_string)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let mut cells: Vec<Cell> = r.values.iter().map(value_to_cell).collect();
                cells.push(text(r.hash));
                cells.push(text(r.committer));
                cells.push(text(r.date));
                cells.push(text(r.start));
                cells
            })
            .collect())
    })
}

fn build_at_t(
    state: Option<Arc<VcState>>,
    table: &str,
    idx_str: &str,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    let bound = bind_ref_cols(idx_str, args);
    let rev = bound
        .iter()
        .max_by_key(|(col, _)| *col)
        .map(|(_, v)| v.clone());
    state.with_store(|store| {
        let rows = at_rows(store, table, rev.as_deref()).map_err(err_string)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let mut cells: Vec<Cell> = r.values.iter().map(value_to_cell).collect();
                cells.push(text(rev.clone().unwrap_or_default()));
                cells
            })
            .collect())
    })
}

fn build_blame_t(
    state: Option<Arc<VcState>>,
    table: &str,
    _idx_str: &str,
    _args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    state.with_store(|store| {
        let rows = blame_rows(store, table).map_err(err_string)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let mut cells: Vec<Cell> = r.pk_values.iter().map(value_to_cell).collect();
                cells.push(text(r.hash));
                cells.push(text(r.date));
                cells.push(text(r.committer));
                cells.push(text(r.email));
                cells.push(text(r.message));
                cells
            })
            .collect())
    })
}

fn build_diff_t(
    state: Option<Arc<VcState>>,
    table: &str,
    idx_str: &str,
    args: &[Value],
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    let bound = bind_ref_cols(idx_str, args);
    let mut cols: Vec<u32> = bound.iter().map(|(col, _)| *col).collect();
    cols.sort_unstable();
    // The two trailing hidden columns are `from_ref` then `to_ref`, so the
    // lower column is `from` and the higher is `to`.
    let (from, to) = match cols.as_slice() {
        [from_col, to_col] if *from_col < *to_col => {
            let from = bound
                .iter()
                .find(|(c, _)| c == from_col)
                .map(|(_, v)| v.clone());
            let to = bound
                .iter()
                .find(|(c, _)| c == to_col)
                .map(|(_, v)| v.clone());
            (from, to)
        }
        _ => (None, None),
    };
    state.with_store(|store| {
        let rows =
            diff_table_rows(store, table, from.as_deref(), to.as_deref()).map_err(err_string)?;
        let from_echo = from.clone().unwrap_or_default();
        let to_echo = to.clone().unwrap_or_default();
        Ok(rows
            .into_iter()
            .map(|r| {
                // An added or deleted row lacks one side's cells, so the row
                // would be short of the declared width; pad each side to the
                // live column count with NULLs so every later column keeps its
                // schema index.
                let live = r.to.len().max(r.from.len());
                let mut cells: Vec<Cell> = r.to.iter().map(value_to_cell).collect();
                cells.resize(live, Cell::Null);
                cells.push(text(r.to_commit));
                cells.push(text(r.to_date));
                cells.extend(r.from.iter().map(value_to_cell));
                cells.resize(live * 2 + 2, Cell::Null);
                cells.push(text(r.from_commit));
                cells.push(text(r.from_date));
                cells.push(text(diff_type_text(&r.diff_type)));
                cells.push(text(from_echo.clone()));
                cells.push(text(to_echo.clone()));
                cells
            })
            .collect())
    })
}

fn build_conflicts_t(
    state: Option<Arc<VcState>>,
    conn: Option<Arc<Connection>>,
    table: &str,
) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    // A schema conflict carries no row cells, so its output row is short of
    // the declared width; pad it with NULLs so every row fills its columns.
    let width = conn
        .as_ref()
        .map(|c| {
            let raw = c.conn_ptr();
            let cols = table_columns(Some(unsafe { &*raw }), table);
            cols.len() * 4 + 1
        })
        .unwrap_or(0);
    state.with_store(|store| {
        let entries = store.conflict_entries();
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        for e in entries.iter().filter(|e| e.table == table) {
            let mut cells: Vec<Cell> = e.pk.iter().map(value_to_cell).collect();
            cells.extend(row_cells(e.base.as_ref()));
            cells.extend(row_cells(e.ours.as_ref()));
            cells.extend(row_cells(e.theirs.as_ref()));
            let kind = match &e.kind {
                turso_versioning::conflicts::ConflictKind::Rows => "rows",
                turso_versioning::conflicts::ConflictKind::Schema(_) => "schema",
            };
            cells.push(text(kind.to_string()));
            if width > cells.len() {
                cells.resize(width, Cell::Null);
            }
            rows.push(cells);
        }
        Ok(rows)
    })
}

fn build_violations_t(state: Option<Arc<VcState>>, table: &str) -> Result<Vec<Vec<Cell>>, String> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    state.with_store(|store| {
        let violations = store.violation_entries();
        Ok(violations
            .into_iter()
            .filter(|v| v.table == table)
            .map(|v| {
                vec![
                    text(v.table.clone()),
                    text(v.kind.to_string()),
                    text(join_values(&v.row_pk)),
                    text(v.detail.clone()),
                ]
            })
            .collect())
    })
}

fn diff_type_text(ty: &turso_versioning::vtab_diff::DiffType) -> String {
    match ty {
        turso_versioning::vtab_diff::DiffType::Added => "added".to_string(),
        turso_versioning::vtab_diff::DiffType::Modified => "modified".to_string(),
        turso_versioning::vtab_diff::DiffType::Deleted => "deleted".to_string(),
    }
}

/// A plan that consumes one column's equality as a probe.
fn plan_equality(constraints: &[VcConstraint], column: u32, estimate: u32) -> VcPlan {
    let mut omit = vec![false; constraints.len()];
    for (i, c) in constraints.iter().enumerate() {
        if c.usable && c.op == VcOp::Eq && c.column == column {
            omit[i] = true;
            return VcPlan {
                idx_num: 1,
                idx_str: None,
                omit,
                argv: vec![column],
                cost: 10.0,
                rows: estimate,
                order_consumed: false,
            };
        }
    }
    VcPlan {
        idx_num: 0,
        idx_str: None,
        omit,
        argv: Vec::new(),
        cost: 1000.0 + estimate as f64,
        rows: estimate,
        order_consumed: false,
    }
}

fn value_to_cell(v: &VcValue) -> Cell {
    match v {
        VcValue::Null => Cell::Null,
        VcValue::Integer(i) => Cell::Int(*i),
        VcValue::Real(bits) => Cell::Float(f64::from_bits(*bits)),
        VcValue::Text(s) => Cell::Text(s.clone()),
        VcValue::Blob(bytes) => Cell::Blob(bytes.clone()),
    }
}

fn value_text(v: &VcValue) -> String {
    match v {
        VcValue::Null => "NULL".to_string(),
        VcValue::Integer(i) => i.to_string(),
        VcValue::Real(bits) => f64::from_bits(*bits).to_string(),
        VcValue::Text(s) => s.clone(),
        VcValue::Blob(bytes) => blob_text(bytes),
    }
}

fn blob_text(bytes: &[u8]) -> String {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("X'{hex}'")
}

fn join_values(values: &[VcValue]) -> String {
    values.iter().map(value_text).collect::<Vec<_>>().join(",")
}

fn row_text(row: Option<&VcRow>) -> String {
    row.map(|r| join_values(&r.values)).unwrap_or_default()
}

fn row_cells(row: Option<&VcRow>) -> Vec<Cell> {
    row.map(|r| r.values.iter().map(value_to_cell).collect())
        .unwrap_or_default()
}

// ============================================================================
// Helpers. Below every caller so the file reads top-down.
// ============================================================================

fn text(s: String) -> Cell {
    Cell::Text(s)
}

fn int(i: i64) -> Cell {
    Cell::Int(i)
}

fn cell_to_value(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::null(),
        Cell::Int(i) => Value::from_integer(*i),
        Cell::Float(value) => Value::from_float(*value),
        Cell::Text(s) => Value::from_text(s.clone()),
        Cell::Blob(bytes) => Value::from_blob(bytes.clone()),
    }
}

/// One materialized output cell. `turso_ext::Value` is not cloneable (it owns
/// FFI memory), so cursors store cells and render values on demand.
#[derive(Debug, Clone)]
enum Cell {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

fn err_string(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn arg_text(args: &[Value], i: usize) -> Option<String> {
    args.get(i).and_then(|v| v.to_text_coerced())
}

/// Keep rows whose projected field equals the bound argument. The planner
/// marks consumed constraints `omit`, so the engine skips its own recheck
/// and the glue is the only enforcement left.
fn retain<T>(rows: &mut Vec<T>, args: &[Value], i: usize, field: impl Fn(&T) -> String) {
    if let Some(want) = arg_text(args, i) {
        rows.retain(|r| field(r) == want);
    }
}

/// Map filter arguments back to their hidden columns using the planner's
/// `name:col,...` tag. Positional table-valued-function arguments arrive in hidden order, so
/// the tag columns and the values line up one-to-one.
fn bind_ref_args(idx_str: &str, args: &[Value]) -> Vec<String> {
    let cols = idx_str.split(':').nth(1).unwrap_or("");
    let n = cols.split(',').filter(|c| !c.is_empty()).count();
    args.iter()
        .take(n)
        .filter_map(|v| v.to_text_coerced())
        .collect()
}

/// Bind filter args to the columns named by a `tag:col,col` idx_str. The
/// engine passes args in constraint order, and the tag lists the consumed
/// columns in that same order, so each value maps back to its column.
fn bind_ref_cols(idx_str: &str, args: &[Value]) -> Vec<(u32, String)> {
    let cols = idx_str
        .split(':')
        .nth(1)
        .unwrap_or("")
        .split(',')
        .filter(|c| !c.is_empty())
        .filter_map(|c| c.parse::<u32>().ok())
        .collect::<Vec<_>>();
    args.iter()
        .zip(cols)
        .filter_map(|(v, col)| v.to_text_coerced().map(|s| (col, s)))
        .collect()
}

/// Plan a per-table scan's reference binding. The hidden reference columns
/// are always the last columns of the schema, but `best_index` cannot see the
/// live column count, so it binds usable equality constraints on the highest
/// column indices — exactly the trailing hidden references.
fn plan_per_table_refs(
    constraints: &[VcConstraint],
    hidden: usize,
    tag: &str,
    estimate: u32,
) -> VcPlan {
    let mut omit = vec![false; constraints.len()];
    let mut picked: Vec<(usize, u32)> = Vec::new();
    for (i, c) in constraints.iter().enumerate() {
        if c.usable && c.op == VcOp::Eq {
            picked.push((i, c.column));
        }
    }
    picked.sort_by_key(|(_, col)| std::cmp::Reverse(*col));
    picked.truncate(hidden);
    if picked.is_empty() {
        return VcPlan {
            idx_num: 0,
            idx_str: None,
            omit,
            argv: Vec::new(),
            cost: 1e12,
            rows: estimate,
            order_consumed: false,
        };
    }
    picked.sort_by_key(|(i, _)| *i);
    let cols = picked
        .iter()
        .map(|(_, col)| col.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let argv: Vec<u32> = picked.iter().map(|(_, col)| *col).collect();
    for (i, _) in &picked {
        omit[*i] = true;
    }
    VcPlan {
        idx_num: 1,
        idx_str: Some(format!("{tag}:{cols}")),
        omit,
        argv,
        cost: 10.0,
        rows: estimate,
        order_consumed: false,
    }
}

/// Plan a blame scan: a single-column primary key probes on column 0.
fn plan_blame_probe(constraints: &[VcConstraint], estimate: u32) -> VcPlan {
    let mut omit = vec![false; constraints.len()];
    for (i, c) in constraints.iter().enumerate() {
        if c.usable && c.op == VcOp::Eq && c.column == 0 {
            omit[i] = true;
            return VcPlan {
                idx_num: 1,
                idx_str: Some("pk:0".to_string()),
                omit,
                argv: vec![0],
                cost: 10.0,
                rows: 1,
                order_consumed: false,
            };
        }
    }
    VcPlan {
        idx_num: 0,
        idx_str: None,
        omit,
        argv: Vec::new(),
        cost: 1000.0 + estimate as f64,
        rows: estimate,
        order_consumed: false,
    }
}

fn vc_constraints(constraints: &[ConstraintInfo]) -> Vec<VcConstraint> {
    constraints
        .iter()
        .map(|c| VcConstraint {
            column: c.column_index,
            op: if c.op == ConstraintOp::Eq {
                VcOp::Eq
            } else {
                VcOp::Other
            },
            usable: c.usable,
        })
        .collect()
}

fn vc_order(order_by: &[OrderByInfo]) -> Vec<VcOrderBy> {
    order_by
        .iter()
        .map(|o| VcOrderBy {
            column: o.column_index,
            desc: o.desc,
        })
        .collect()
}

fn index_info(plan: &VcPlan, total: usize) -> IndexInfo {
    let mut next = 1u32;
    let mut usages = Vec::with_capacity(total);
    for omit in plan.omit.iter() {
        if *omit {
            usages.push(ConstraintUsage {
                argv_index: Some(next),
                omit: true,
            });
            next += 1;
        } else {
            usages.push(ConstraintUsage {
                argv_index: None,
                omit: false,
            });
        }
    }
    IndexInfo {
        idx_num: plan.idx_num,
        idx_str: plan.idx_str.clone(),
        order_by_consumed: plan.order_consumed,
        estimated_cost: plan.cost,
        estimated_rows: plan.rows,
        constraint_usages: usages,
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltLogModule;

impl VTabModule for DoltLogModule {
    type Table = DoltLogTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_log";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((DOLT_LOG_SCHEMA.to_string(), DoltLogTable))
    }
}

struct DoltLogTable;

impl VTable for DoltLogTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable { kind: VcKind::Log }.open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_log(&vc_constraints(constraints), &vc_order(order_by), 1024);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltSchemasModule;

impl VTabModule for DoltSchemasModule {
    type Table = DoltSchemasTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_schemas";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((
            turso_versioning::vtab_history::DOLT_SCHEMAS_SCHEMA.to_string(),
            DoltSchemasTable,
        ))
    }
}

struct DoltSchemasTable;

impl VTable for DoltSchemasTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::Schemas,
        }
        .open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_schemas(&vc_constraints(constraints), 64);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltDiffModule;

impl VTabModule for DoltDiffModule {
    type Table = DoltDiffTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_diff";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((DOLT_DIFF_SCHEMA.to_string(), DoltDiffTable))
    }
}

struct DoltDiffTable;

impl VTable for DoltDiffTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable { kind: VcKind::Diff }.open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_diff(&vc_constraints(constraints), 1024);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltDiffStatModule;

impl VTabModule for DoltDiffStatModule {
    type Table = DoltDiffStatTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_diff_stat";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((DOLT_DIFF_STAT_SCHEMA.to_string(), DoltDiffStatTable))
    }
}

struct DoltDiffStatTable;

impl VTable for DoltDiffStatTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::DiffStat,
        }
        .open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_refs(&vc_constraints(constraints), &[12, 13, 14], 64);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltDiffSummaryModule;

impl VTabModule for DoltDiffSummaryModule {
    type Table = DoltDiffSummaryTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_diff_summary";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((DOLT_DIFF_SUMMARY_SCHEMA.to_string(), DoltDiffSummaryTable))
    }
}

struct DoltDiffSummaryTable;

impl VTable for DoltDiffSummaryTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::DiffSummary,
        }
        .open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_refs(&vc_constraints(constraints), &[5, 6, 7], 64);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltSchemaDiffModule;

impl VTabModule for DoltSchemaDiffModule {
    type Table = DoltSchemaDiffTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_schema_diff";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((DOLT_SCHEMA_DIFF_SCHEMA.to_string(), DoltSchemaDiffTable))
    }
}

struct DoltSchemaDiffTable;

impl VTable for DoltSchemaDiffTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::SchemaDiff,
        }
        .open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_refs(&vc_constraints(constraints), &[4, 5, 6], 64);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltPatchModule;

impl VTabModule for DoltPatchModule {
    type Table = DoltPatchTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_patch";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((DOLT_PATCH_SCHEMA.to_string(), DoltPatchTable))
    }
}

struct DoltPatchTable;

impl VTable for DoltPatchTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::Patch,
        }
        .open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_refs(&vc_constraints(constraints), &[6, 7, 8], 64);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltMergeStatusModule;

impl VTabModule for DoltMergeStatusModule {
    type Table = DoltMergeStatusTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_merge_status";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((DOLT_MERGE_STATUS_SCHEMA.to_string(), DoltMergeStatusTable))
    }
}

struct DoltMergeStatusTable;

impl VTable for DoltMergeStatusTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::MergeStatus,
        }
        .open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_equality(&vc_constraints(constraints), 0, 8);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltConflictsModule;

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltBranchesModule;

impl VTabModule for DoltBranchesModule {
    type Table = DoltBranchesTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_branches";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((
            turso_versioning::vtab_refs::DOLT_BRANCHES_SCHEMA.to_string(),
            DoltBranchesTable,
        ))
    }
}

struct DoltBranchesTable;

impl VTable for DoltBranchesTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::Branches,
        }
        .open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_equality(&vc_constraints(constraints), 0, 8);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltTagsModule;

impl VTabModule for DoltTagsModule {
    type Table = DoltTagsTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_tags";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((
            turso_versioning::vtab_refs::DOLT_TAGS_SCHEMA.to_string(),
            DoltTagsTable,
        ))
    }
}

struct DoltTagsTable;

impl VTable for DoltTagsTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable { kind: VcKind::Tags }.open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_equality(&vc_constraints(constraints), 0, 8);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltRemotesModule;

impl VTabModule for DoltRemotesModule {
    type Table = DoltRemotesTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_remotes";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((
            turso_versioning::vtab_refs::DOLT_REMOTES_SCHEMA.to_string(),
            DoltRemotesTable,
        ))
    }
}

struct DoltRemotesTable;

impl VTable for DoltRemotesTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::Remotes,
        }
        .open(conn)
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltRemoteBranchesModule;

impl VTabModule for DoltRemoteBranchesModule {
    type Table = DoltRemoteBranchesTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_remote_branches";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((
            turso_versioning::vtab_refs::DOLT_REMOTE_BRANCHES_SCHEMA.to_string(),
            DoltRemoteBranchesTable,
        ))
    }
}

struct DoltRemoteBranchesTable;

impl VTable for DoltRemoteBranchesTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::RemoteBranches,
        }
        .open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_equality(&vc_constraints(constraints), 3, 8);
        Ok(index_info(&plan, constraints.len()))
    }
}

impl VTabModule for DoltConflictsModule {
    type Table = DoltConflictsTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_conflicts";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((DOLT_CONFLICTS_SCHEMA.to_string(), DoltConflictsTable))
    }
}

struct DoltConflictsTable;

impl VTable for DoltConflictsTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::Conflicts,
        }
        .open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_equality(&vc_constraints(constraints), 0, 16);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltConstraintViolationsModule;

impl VTabModule for DoltConstraintViolationsModule {
    type Table = DoltConstraintViolationsTable;
    const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
    const NAME: &'static str = "dolt_constraint_violations";

    fn create(_args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
        Ok((
            DOLT_CONSTRAINT_VIOLATIONS_SCHEMA.to_string(),
            DoltConstraintViolationsTable,
        ))
    }
}

struct DoltConstraintViolationsTable;

impl VTable for DoltConstraintViolationsTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        VcTable {
            kind: VcKind::Violations,
        }
        .open(conn)
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_equality(&vc_constraints(constraints), 0, 16);
        Ok(index_info(&plan, constraints.len()))
    }
}

// ============================================================================
// Per-table modules: `dolt_history_<t>`, `dolt_at_<t>`, `dolt_blame_<t>`,
// `dolt_diff_<t>`, `dolt_conflicts_<t>`. Their `create` resolves `t`'s live
// columns through the registering connection, so the declared schema matches
// the table. Registered by `register_table_modules` when the table appears.
// ============================================================================

/// The user table name a per-table module was created for.
fn module_table(name: &str) -> String {
    for prefix in [
        "dolt_constraint_violations_",
        "dolt_history_",
        "dolt_at_",
        "dolt_blame_",
        "dolt_diff_",
        "dolt_conflicts_",
    ] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    name.to_string()
}

/// Quote a table name for use inside an SQL identifier, doubling any embedded
/// double quote.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The table's columns, read from the registering connection.
fn table_columns(conn: Option<&crate::Conn>, table: &str) -> Vec<String> {
    let Some(conn) = conn else {
        return Vec::new();
    };
    #[allow(clippy::arc_with_non_send_sync)]
    let connection = Arc::new(crate::Connection::new(conn as *const crate::Conn));
    let Ok(mut stmt) = connection.prepare(&format!("PRAGMA table_info({})", quote_ident(table)))
    else {
        return Vec::new();
    };
    let mut columns = Vec::new();
    loop {
        match stmt.step() {
            StepResult::Row => {
                let row = stmt.get_row();
                if let Some(name) = row.get(1).and_then(|v| v.to_text_coerced()) {
                    columns.push(name);
                }
            }
            StepResult::Done => break,
            _ => break,
        }
    }
    columns
}

/// The table's primary-key columns, read from the registering connection.
fn table_pk(conn: Option<&crate::Conn>, table: &str) -> Vec<String> {
    let Some(conn) = conn else {
        return Vec::new();
    };
    #[allow(clippy::arc_with_non_send_sync)]
    let connection = Arc::new(crate::Connection::new(conn as *const crate::Conn));
    let Ok(mut stmt) = connection.prepare(&format!("PRAGMA table_info({})", quote_ident(table)))
    else {
        return Vec::new();
    };
    let mut ranked: Vec<(usize, String)> = Vec::new();
    loop {
        match stmt.step() {
            StepResult::Row => {
                let row = stmt.get_row();
                let name = row
                    .get(1)
                    .and_then(|v| v.to_text_coerced())
                    .unwrap_or_default();
                let rank = row.get(5).and_then(|v| v.to_integer()).unwrap_or(0) as usize;
                if rank > 0 {
                    ranked.push((rank, name));
                }
            }
            StepResult::Done => break,
            _ => break,
        }
    }
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, name)| name).collect()
}

macro_rules! per_table_module_impl {
    ($module_ty:ty, $table_ty:ty, $name_prefix:expr, $schema_fn:path, $cols:expr) => {
        impl VTabModule for $module_ty {
            type Table = $table_ty;
            const VTAB_KIND: VTabKind = VTabKind::TableValuedFunction;
            const NAME: &'static str = $name_prefix;

            fn create(args: &[Value]) -> Result<(String, Self::Table), ResultCode> {
                Self::create_with_conn("", None, args)
            }

            fn create_with_conn(
                name: &str,
                conn: Option<&crate::Conn>,
                _args: &[Value],
            ) -> Result<(String, Self::Table), ResultCode> {
                let table = module_table(name);
                let cols = $cols(conn, &table); // Without the live table's columns the schema would declare an
                                                // empty row; refuse instead of producing that garbage. The
                                                // registering connection carries the table, so a bare
                                                // `CREATE VIRTUAL TABLE ... USING dolt_history_t` (no table
                                                // behind it) fails here too.
                if cols.is_empty() {
                    return Err(ResultCode::Error);
                }
                Ok(($schema_fn(&table, &cols), <$table_ty>::new(table)))
            }
        }
    };
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltHistoryTModule;
per_table_module_impl!(
    DoltHistoryTModule,
    DoltHistoryTTable,
    "dolt_history_",
    history_schema,
    table_columns
);

#[derive(Debug, Default)]
struct DoltHistoryTTable {
    table: String,
}
impl DoltHistoryTTable {
    fn new(table: String) -> Self {
        DoltHistoryTTable { table }
    }
}

impl VTable for DoltHistoryTTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        Ok(VcCursor {
            kind: VcKind::HistoryT,
            table: self.table.clone(),
            state: conn.as_ref().and_then(|c| c.versioning()),
            conn,
            rows: Vec::new(),
            pos: 0,
            error: None,
            delivered: false,
        })
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_per_table_refs(&vc_constraints(constraints), 1, "start", 64);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltAtTModule;
per_table_module_impl!(
    DoltAtTModule,
    DoltAtTTable,
    "dolt_at_",
    at_schema,
    table_columns
);

#[derive(Debug, Default)]
struct DoltAtTTable {
    table: String,
}
impl DoltAtTTable {
    fn new(table: String) -> Self {
        DoltAtTTable { table }
    }
}

impl VTable for DoltAtTTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        Ok(VcCursor {
            kind: VcKind::AtT,
            table: self.table.clone(),
            state: conn.as_ref().and_then(|c| c.versioning()),
            conn,
            rows: Vec::new(),
            pos: 0,
            error: None,
            delivered: false,
        })
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_per_table_refs(&vc_constraints(constraints), 1, "ref", 64);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltBlameTModule;
per_table_module_impl!(
    DoltBlameTModule,
    DoltBlameTTable,
    "dolt_blame_",
    blame_schema,
    table_pk
);

#[derive(Debug, Default)]
struct DoltBlameTTable {
    table: String,
}
impl DoltBlameTTable {
    fn new(table: String) -> Self {
        DoltBlameTTable { table }
    }
}

impl VTable for DoltBlameTTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        Ok(VcCursor {
            kind: VcKind::BlameT,
            table: self.table.clone(),
            state: conn.as_ref().and_then(|c| c.versioning()),
            conn,
            rows: Vec::new(),
            pos: 0,
            error: None,
            delivered: false,
        })
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_blame_probe(&vc_constraints(constraints), 64);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltDiffTModule;
per_table_module_impl!(
    DoltDiffTModule,
    DoltDiffTTable,
    "dolt_diff_",
    diff_table_schema,
    table_columns
);

#[derive(Debug, Default)]
struct DoltDiffTTable {
    table: String,
}
impl DoltDiffTTable {
    fn new(table: String) -> Self {
        DoltDiffTTable { table }
    }
}

impl VTable for DoltDiffTTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        Ok(VcCursor {
            kind: VcKind::DiffT,
            table: self.table.clone(),
            state: conn.as_ref().and_then(|c| c.versioning()),
            conn,
            rows: Vec::new(),
            pos: 0,
            error: None,
            delivered: false,
        })
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_per_table_refs(&vc_constraints(constraints), 2, "refs", 64);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltConflictsTModule;
per_table_module_impl!(
    DoltConflictsTModule,
    DoltConflictsTTable,
    "dolt_conflicts_",
    conflicts_table_schema,
    table_columns
);

/// The per-table conflicts schema: pk columns, then base/our/their rows,
/// then the conflict type.
pub fn conflicts_table_schema(table: &str, columns: &[String]) -> String {
    let side = |prefix: &str| {
        columns
            .iter()
            .map(|c| format!("\"{prefix}_{c}\" TEXT, "))
            .collect::<String>()
    };
    format!(
        "CREATE TABLE dolt_conflicts_{table} ({pk}{base}{our}{their}conflict_type TEXT)",
        pk = columns
            .iter()
            .map(|c| format!("\"{c}\" TEXT, "))
            .collect::<String>(),
        base = side("base"),
        our = side("our"),
        their = side("their"),
    )
}

#[derive(Debug, VTabModuleDerive, Default)]
struct DoltConstraintViolationsTModule;
per_table_module_impl!(
    DoltConstraintViolationsTModule,
    DoltConstraintViolationsTTable,
    "dolt_constraint_violations_",
    fixed_violations_schema,
    table_columns
);

/// The per-table violations schema is the union's four columns; only the
/// filtering differs (this module's rows are the violations of one table).
fn fixed_violations_schema(_table: &str, _columns: &[String]) -> String {
    DOLT_CONSTRAINT_VIOLATIONS_SCHEMA.to_string()
}

#[derive(Debug, Default)]
struct DoltConstraintViolationsTTable {
    table: String,
}
impl DoltConstraintViolationsTTable {
    fn new(table: String) -> Self {
        DoltConstraintViolationsTTable { table }
    }
}

impl VTable for DoltConstraintViolationsTTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        Ok(VcCursor {
            kind: VcKind::ViolationsT,
            table: self.table.clone(),
            state: conn.as_ref().and_then(|c| c.versioning()),
            conn,
            rows: Vec::new(),
            pos: 0,
            error: None,
            delivered: false,
        })
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = plan_equality(&vc_constraints(constraints), 0, 16);
        Ok(index_info(&plan, constraints.len()))
    }
}

#[derive(Debug, Default)]
struct DoltConflictsTTable {
    table: String,
}
impl DoltConflictsTTable {
    fn new(table: String) -> Self {
        DoltConflictsTTable { table }
    }
}

impl VTable for DoltConflictsTTable {
    type Cursor = VcCursor;
    type Error = String;

    fn open(&self, conn: Option<Arc<Connection>>) -> Result<Self::Cursor, Self::Error> {
        Ok(VcCursor {
            kind: VcKind::ConflictsT,
            table: self.table.clone(),
            state: conn.as_ref().and_then(|c| c.versioning()),
            conn,
            rows: Vec::new(),
            pos: 0,
            error: None,
            delivered: false,
        })
    }

    fn best_index(
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let plan = VcPlan {
            idx_num: 0,
            idx_str: None,
            omit: vec![false; constraints.len()],
            argv: Vec::new(),
            cost: 1000.0,
            rows: 8,
            order_consumed: false,
        };
        Ok(index_info(&plan, constraints.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constraint(column: u32) -> ConstraintInfo {
        ConstraintInfo {
            column_index: column,
            op: ConstraintOp::Eq,
            usable: true,
            index: 0,
        }
    }

    #[test]
    fn log_best_index_picks_hash_probe() {
        let info = DoltLogTable::best_index(&[constraint(0)], &[]).unwrap();
        assert_eq!(info.idx_num, 1);
        assert_eq!(info.constraint_usages.len(), 1);
        assert_eq!(info.constraint_usages[0].argv_index, Some(1));
        assert!(info.constraint_usages[0].omit);
    }

    #[test]
    fn log_best_index_ignores_non_eq() {
        let info = DoltLogTable::best_index(
            &[ConstraintInfo {
                column_index: 0,
                op: ConstraintOp::Gt,
                usable: true,
                index: 0,
            }],
            &[],
        )
        .unwrap();
        assert_eq!(info.idx_num, 0);
    }

    #[test]
    fn stat_best_index_consumes_both_refs() {
        let info = DoltDiffStatTable::best_index(&[constraint(12), constraint(13)], &[]).unwrap();
        assert_eq!(info.idx_num, 1);
        assert_eq!(info.idx_str.as_deref(), Some("refs:12,13"));
        let rows: Vec<u32> = info
            .constraint_usages
            .iter()
            .filter_map(|u| u.argv_index)
            .collect();
        assert_eq!(rows, vec![1, 2]);
    }

    #[test]
    fn cursor_delivers_exact_error_message() {
        let cursor = VcCursor {
            kind: VcKind::Log,
            table: String::new(),
            state: None,
            conn: None,
            rows: Vec::new(),
            pos: 0,
            error: Some("branch not found: nope".to_string()),
            delivered: false,
        };
        assert!(!cursor.eof());
        assert_eq!(
            cursor.column(0).unwrap_err(),
            "branch not found: nope".to_string()
        );
    }

    #[test]
    fn cursor_error_fires_on_every_column() {
        let cursor = VcCursor {
            kind: VcKind::Log,
            table: String::new(),
            state: None,
            conn: None,
            rows: Vec::new(),
            pos: 0,
            error: Some("branch not found: nope".to_string()),
            delivered: false,
        };
        for idx in 0..6 {
            assert_eq!(
                cursor.column(idx).unwrap_err(),
                "branch not found: nope".to_string()
            );
        }
        assert!(!cursor.eof());
    }

    #[test]
    fn cursor_without_state_reads_empty() {
        let table = VcTable { kind: VcKind::Log };
        let mut cursor = table.open(None).unwrap();
        assert_eq!(cursor.filter(&[], None), ResultCode::EOF);
        assert!(cursor.eof());
        let mut diff = VcTable { kind: VcKind::Diff }.open(None).unwrap();
        assert_eq!(diff.filter(&[], None), ResultCode::EOF);
        assert!(diff.eof());
    }

    #[test]
    fn cursor_next_ends_at_last_row() {
        let mut cursor = VcCursor {
            kind: VcKind::Log,
            table: String::new(),
            state: None,
            conn: None,
            rows: vec![vec![Cell::Int(1)], vec![Cell::Int(2)]],
            pos: 0,
            error: None,
            delivered: false,
        };
        assert!(!cursor.eof());
        assert_eq!(cursor.next(), ResultCode::OK);
        assert_eq!(cursor.next(), ResultCode::EOF);
        assert!(cursor.eof());
    }

    #[test]
    fn bind_ref_args_maps_positional_args() {
        let args = vec![
            Value::from_text("a".to_string()),
            Value::from_text("b".to_string()),
        ];
        assert_eq!(
            bind_ref_args("refs:12,13", &args),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(bind_ref_args("", &args).is_empty());
    }
}
