//! Row diffs and patch generation: `dolt_diff`, `dolt_diff_<t>`,
//! `dolt_diff_stat`, `dolt_diff_summary`, `dolt_schema_diff`, `dolt_patch`.
//!
//! D1–D6. doltlite computes these over prolly trees; here the same shapes come
//! from the `VcRead` snapshots. Row identity is the primary key when known,
//! otherwise the ordinal position. Unknown tables diff as zero rows, never an
//! error. Patch statements are ordered and `;`-terminated so the documented
//! recipe (`WHERE diff_type='data' ORDER BY statement_order`) replays.

use std::collections::{HashMap, HashSet};

use super::vtab_log::{VcConstraint, VcOp, VcPlan, VcRead, VcRow, VcValue};
use crate::model::{CommitId, VersionResult};

/// Added, deleted, or changed between two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffType {
    Added,
    Deleted,
    Modified,
}

impl DiffType {
    pub fn label(self) -> &'static str {
        match self {
            DiffType::Added => "added",
            DiffType::Deleted => "deleted",
            DiffType::Modified => "modified",
        }
    }
}

/// One `dolt_diff_<t>` row: the `to` and `from` versions of one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffTableRow {
    pub to: Vec<VcValue>,
    pub from: Vec<VcValue>,
    pub to_commit: String,
    pub to_date: String,
    pub from_commit: String,
    pub from_date: String,
    pub diff_type: DiffType,
}

/// One `dolt_diff` row: per-commit, per-table change flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub hash: String,
    pub committer: String,
    pub email: String,
    pub date: String,
    pub message: String,
    pub data_change: bool,
    pub schema_change: bool,
    pub table_name: String,
}

/// One `dolt_diff_stat` row: row and cell counts between two revisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffStatRow {
    pub table: String,
    pub rows_unmodified: i64,
    pub rows_added: i64,
    pub rows_deleted: i64,
    pub rows_modified: i64,
    pub cells_added: i64,
    pub cells_deleted: i64,
    pub cells_modified: i64,
    pub old_rows: i64,
    pub new_rows: i64,
    pub old_cells: i64,
    pub new_cells: i64,
}

/// One `dolt_diff_summary` row: how one table changed between revisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSummaryRow {
    pub from_table: String,
    pub to_table: String,
    pub diff_type: String,
    pub data_change: bool,
    pub schema_change: bool,
}

/// One `dolt_schema_diff` row: create statements on both sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDiffRow {
    pub from_table: String,
    pub to_table: String,
    pub from_create: String,
    pub to_create: String,
}

/// One `dolt_patch` row: a single ordered executable statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchRow {
    pub order: i64,
    pub from_commit: String,
    pub to_commit: String,
    pub table: String,
    pub diff_type: String,
    pub statement: String,
}

/// Rows for `dolt_diff`: `STAGED` (HEAD→staged) then `WORKING` (staged→work)
/// pseudo-commits first, then one row per changed table per commit,
/// newest commit first. Tables are advisory: only tracked names appear.
/// With no commits yet only the snapshot rows appear, never an error.
pub fn diff_rows(provider: &dyn VcRead) -> VersionResult<Vec<DiffRow>> {
    let mut out = Vec::new();
    let tables = provider.tables();
    let head = provider.head().ok();
    let staged_set: HashSet<String> = provider.staged_tables().into_iter().collect();
    let working_set: HashSet<String> = provider.working_tables().into_iter().collect();
    if provider.has_snapshot(&super::vtab_log::staged_id()) || !staged_set.is_empty() {
        push_snapshot_diff(
            provider,
            &tables,
            &staged_set,
            head.as_ref(),
            &super::vtab_log::staged_id(),
            "STAGED",
            &mut out,
        );
    }
    if provider.has_snapshot(&super::vtab_log::working_id()) || !working_set.is_empty() {
        push_snapshot_diff(
            provider,
            &tables,
            &working_set,
            Some(&super::vtab_log::staged_id()),
            &super::vtab_log::working_id(),
            "WORKING",
            &mut out,
        );
    }
    let Some(tip) = head else {
        return Ok(out);
    };
    for id in super::vtab_log::commit_chain(provider, tip)? {
        let view = provider.commit_view(&id)?;
        let parents = provider.parents(&id)?;
        let from = parents.into_iter().next();
        for table in &tables {
            let (data_change, schema_change) = table_changed(provider, table, from.as_ref(), &id);
            if !data_change && !schema_change {
                continue;
            }
            out.push(DiffRow {
                hash: view.id.to_hex(),
                committer: view.name.clone(),
                email: view.email.clone(),
                date: view.timestamp.to_string(),
                message: view.message.clone(),
                data_change,
                schema_change,
                table_name: table.clone(),
            });
        }
    }
    Ok(out)
}

/// Rows for `dolt_diff_<t>`: bare form diffs every commit against its first
/// parent (root against the empty table); the TVF slice diffs one pair.
/// Unknown tables read as zero rows. Commits where the table is absent on
/// both sides contribute nothing.
pub fn diff_table_rows(
    provider: &dyn VcRead,
    table: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> VersionResult<Vec<DiffTableRow>> {
    let mut out = Vec::new();
    match (from, to) {
        (None, None) => {
            let Some(tip) = provider.head().ok() else {
                return Ok(out);
            };
            for id in super::vtab_log::commit_chain(provider, tip)? {
                let parents = provider.parents(&id)?;
                emit_table_pair(provider, table, parents.into_iter().next(), id, &mut out);
            }
        }
        _ => {
            let to_id = match to {
                Some(spec) => provider.resolve(spec)?,
                None => provider.head()?,
            };
            let from_id = match from {
                Some(spec) => Some(provider.resolve(spec)?),
                None => None,
            };
            emit_table_pair(provider, table, from_id, to_id, &mut out);
        }
    }
    Ok(out)
}

/// Rows for `dolt_diff_stat(from, to[, table])`: counts between two revisions.
/// A missing table filter matches every table; tables absent on both sides
/// are skipped.
pub fn diff_stat_rows(
    provider: &dyn VcRead,
    from_spec: &str,
    to_spec: &str,
    table_filter: Option<&str>,
) -> VersionResult<Vec<DiffStatRow>> {
    let from_id = provider.resolve(from_spec)?;
    let to_id = provider.resolve(to_spec)?;
    let mut tables: HashSet<String> = provider.tables().into_iter().collect();
    if let Some(name) = table_filter {
        tables.retain(|t| t == name);
    }
    let mut names: Vec<String> = tables.into_iter().collect();
    names.sort();
    let mut out = Vec::new();
    for table in names {
        let old = provider.table_rows(&table, &from_id);
        let next = provider.table_rows(&table, &to_id);
        if old.is_none() && next.is_none() {
            continue;
        }
        out.push(stat_for_pair(provider, &table, &from_id, &to_id));
    }
    Ok(out)
}

/// Rows for `dolt_diff_summary(from, to[, table])`: one row per table with a
/// data or schema change. A table gained with identical columns and rows to a
/// simultaneously dropped table reads as `renamed`.
pub fn diff_summary_rows(
    provider: &dyn VcRead,
    from_spec: &str,
    to_spec: &str,
    table_filter: Option<&str>,
) -> VersionResult<Vec<DiffSummaryRow>> {
    let from_id = provider.resolve(from_spec)?;
    let to_id = provider.resolve(to_spec)?;
    let mut from_tables: HashSet<String> = HashSet::new();
    let mut to_tables: HashSet<String> = HashSet::new();
    for table in provider.tables() {
        if provider.table_rows(&table, &from_id).is_some() {
            from_tables.insert(table.clone());
        }
        if provider.table_rows(&table, &to_id).is_some() {
            to_tables.insert(table.clone());
        }
    }
    let dropped: Vec<String> = from_tables.difference(&to_tables).cloned().collect();
    let added: Vec<String> = to_tables.difference(&from_tables).cloned().collect();
    let renamed = detect_rename(provider, &dropped, &added, &from_id, &to_id);
    let mut names: Vec<String> = from_tables.union(&to_tables).cloned().collect();
    names.sort();
    let mut out = Vec::new();
    for table in names {
        if table_filter.is_some_and(|f| f != table) {
            continue;
        }
        let in_from = from_tables.contains(&table);
        let in_to = to_tables.contains(&table);
        if in_from && in_to {
            let (data_change, schema_change) =
                table_changed(provider, &table, Some(&from_id), &to_id);
            if !data_change && !schema_change {
                continue;
            }
            out.push(DiffSummaryRow {
                from_table: table.clone(),
                to_table: table.clone(),
                diff_type: "modified".to_string(),
                data_change,
                schema_change,
            });
        } else if in_to {
            out.push(DiffSummaryRow {
                from_table: renamed.get(&table).cloned().unwrap_or_default(),
                to_table: table.clone(),
                diff_type: if renamed.contains_key(&table) {
                    "renamed".to_string()
                } else {
                    "added".to_string()
                },
                data_change: true,
                schema_change: true,
            });
        } else {
            if renamed.values().any(|d| d == &table) {
                continue;
            }
            out.push(DiffSummaryRow {
                from_table: table.clone(),
                to_table: String::new(),
                diff_type: "dropped".to_string(),
                data_change: true,
                schema_change: true,
            });
        }
    }
    Ok(out)
}

/// Rows for `dolt_schema_diff(from, to[, table])`: create statements on both
/// sides; the absent side reads as an empty string.
pub fn schema_diff_rows(
    provider: &dyn VcRead,
    from_spec: &str,
    to_spec: &str,
    table_filter: Option<&str>,
) -> VersionResult<Vec<SchemaDiffRow>> {
    let from_id = provider.resolve(from_spec)?;
    let to_id = provider.resolve(to_spec)?;
    let mut names = provider.tables();
    names.sort();
    let mut out = Vec::new();
    for table in names {
        if table_filter.is_some_and(|f| f != table) {
            continue;
        }
        let from_create = provider
            .table_schema_sql(&table, &from_id)
            .unwrap_or_default();
        let to_create = provider
            .table_schema_sql(&table, &to_id)
            .unwrap_or_default();
        if from_create == to_create {
            continue;
        }
        out.push(SchemaDiffRow {
            from_table: table.clone(),
            to_table: table.clone(),
            from_create,
            to_create,
        });
    }
    Ok(out)
}

/// Rows for `dolt_patch(from, to[, table])`: ordered executable statements.
/// Schema statements come first per table (sorted by table name), then data
/// statements in row order. Every statement ends with `;`.
pub fn patch_rows(
    provider: &dyn VcRead,
    from_spec: &str,
    to_spec: &str,
    table_filter: Option<&str>,
) -> VersionResult<Vec<PatchRow>> {
    let from_id = provider.resolve(from_spec)?;
    let to_id = provider.resolve(to_spec)?;
    let from_label = short_label(provider, &from_id, from_spec);
    let to_label = short_label(provider, &to_id, to_spec);
    let mut names = provider.tables();
    names.sort();
    let mut out = Vec::new();
    let mut order = 0;
    for table in names {
        if table_filter.is_some_and(|f| f != table) {
            continue;
        }
        let from_create = provider
            .table_schema_sql(&table, &from_id)
            .unwrap_or_default();
        let to_create = provider
            .table_schema_sql(&table, &to_id)
            .unwrap_or_default();
        let from_gone = provider.table_rows(&table, &from_id).is_none();
        let to_gone = provider.table_rows(&table, &to_id).is_none();
        if from_gone && to_gone {
            continue;
        }
        if from_create != to_create {
            let statement = if to_gone {
                format!("DROP TABLE \"{table}\";")
            } else {
                format!("{to_create};")
            };
            out.push(PatchRow {
                order,
                from_commit: from_label.clone(),
                to_commit: to_label.clone(),
                table: table.clone(),
                diff_type: "schema".to_string(),
                statement,
            });
            order += 1;
            if to_gone {
                continue;
            }
        }
        if to_gone {
            continue;
        }
        for row in diff_table_pair(provider, &table, Some(from_id), to_id) {
            let Some(statement) = data_statement(&table, provider, &row) else {
                continue;
            };
            out.push(PatchRow {
                order,
                from_commit: from_label.clone(),
                to_commit: to_label.clone(),
                table: table.clone(),
                diff_type: "data".to_string(),
                statement,
            });
            order += 1;
        }
    }
    Ok(out)
}

/// Declared schemas, column order 1:1 with doltlite's `diffSchema`,
/// `buildDiffSchema`, `dstSchema`, `dssSchema`, `sdSchema`, `patchSchemaSql`.
pub const DOLT_DIFF_SCHEMA: &str =
    "CREATE TABLE dolt_diff (commit_hash TEXT, committer TEXT, email TEXT, date TEXT, message TEXT, data_change INTEGER, schema_change INTEGER, table_name TEXT)";

pub fn diff_table_schema(table: &str, columns: &[String]) -> String {
    let side = |prefix: &str| {
        columns
            .iter()
            .map(|c| format!("\"{prefix}_{c}\" TEXT, "))
            .collect::<String>()
    };
    format!(
        "CREATE TABLE dolt_diff_{table} ({to}to_commit TEXT, to_commit_date TEXT, {from}from_commit TEXT, from_commit_date TEXT, diff_type TEXT, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN)",
        to = side("to"),
        from = side("from"),
    )
}

pub const DOLT_DIFF_STAT_SCHEMA: &str =
    "CREATE TABLE dolt_diff_stat (table_name TEXT, rows_unmodified INTEGER, rows_added INTEGER, rows_deleted INTEGER, rows_modified INTEGER, cells_added INTEGER, cells_deleted INTEGER, cells_modified INTEGER, old_row_count INTEGER, new_row_count INTEGER, old_cell_count INTEGER, new_cell_count INTEGER, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN, tbl TEXT HIDDEN)";

pub const DOLT_DIFF_SUMMARY_SCHEMA: &str =
    "CREATE TABLE dolt_diff_summary (from_table_name TEXT, to_table_name TEXT, diff_type TEXT, data_change INTEGER, schema_change INTEGER, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN, tbl TEXT HIDDEN)";

pub const DOLT_SCHEMA_DIFF_SCHEMA: &str =
    "CREATE TABLE dolt_schema_diff (from_table_name TEXT, to_table_name TEXT, from_create_statement TEXT, to_create_statement TEXT, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN, table_name TEXT HIDDEN)";

pub const DOLT_PATCH_SCHEMA: &str =
    "CREATE TABLE dolt_patch (statement_order INTEGER, from_commit_hash TEXT, to_commit_hash TEXT, table_name TEXT, diff_type TEXT, statement TEXT, arg1 TEXT HIDDEN, arg2 TEXT HIDDEN, arg3 TEXT HIDDEN)";

/// Pushdown plans. `dolt_diff` probes `commit_hash EQ` and `table_name EQ`;
/// `dolt_diff_<t>` probes `to_commit`/`from_commit` EQ, both, the hidden
/// `from_ref`+`to_ref` slice, or a lone `from_ref`; stat/summary/schema/patch
/// consume the `from_ref`/`to_ref`/`tbl` triple.
pub fn plan_diff(constraints: &[VcConstraint], row_estimate: u32) -> VcPlan {
    let mut omit = vec![false; constraints.len()];
    let mut argv = Vec::new();
    for (i, c) in constraints.iter().enumerate() {
        if !c.usable || c.op != VcOp::Eq {
            continue;
        }
        if c.column == 0 && !argv.contains(&0) {
            argv.push(0);
            omit[i] = true;
            return VcPlan {
                idx_num: 1,
                idx_str: Some("hash".to_string()),
                omit,
                argv,
                cost: 10.0,
                rows: 1,
                order_consumed: false,
            };
        }
        if c.column == 7 && !argv.contains(&7) {
            argv.push(7);
            omit[i] = true;
            return VcPlan {
                idx_num: 2,
                idx_str: Some("table".to_string()),
                omit,
                argv,
                cost: 100.0,
                rows: row_estimate,
                order_consumed: false,
            };
        }
    }
    VcPlan {
        idx_num: 0,
        idx_str: None,
        omit,
        argv,
        cost: 1000.0 + row_estimate as f64,
        rows: row_estimate,
        order_consumed: false,
    }
}

pub fn plan_diff_table(
    live_columns: usize,
    constraints: &[VcConstraint],
    row_estimate: u32,
) -> VcPlan {
    let n = live_columns as u32;
    let mut omit = vec![false; constraints.len()];
    let mut argv: Vec<u32> = Vec::new();
    let mut take = |idx: usize, col: u32, plan_num: i32, name: &str| -> VcPlan {
        // `take` runs only on constraints the scan above already probed, so
        // the checks below always pass; they stay as explicit guards rather
        // than panics so a future caller change degrades to a full scan.
        if !(constraints[idx].usable
            && constraints[idx].op == VcOp::Eq
            && constraints[idx].column == col)
        {
            return VcPlan {
                idx_num: 0,
                idx_str: None,
                omit: omit.clone(),
                argv: argv.clone(),
                cost: 1000.0 + row_estimate as f64,
                rows: row_estimate,
                order_consumed: false,
            };
        }
        argv.push(col);
        omit[idx] = true;
        VcPlan {
            idx_num: plan_num,
            idx_str: Some(name.to_string()),
            omit: omit.clone(),
            argv: argv.clone(),
            cost: 10.0,
            rows: row_estimate.min(1024),
            order_consumed: false,
        }
    };
    let mut to_at = None;
    let mut from_at = None;
    let mut slice_at: Vec<usize> = Vec::new();
    for (i, c) in constraints.iter().enumerate() {
        if !c.usable || c.op != VcOp::Eq {
            continue;
        }
        if c.column == n {
            to_at = Some(i);
        } else if c.column == n + 1 + n + 1 {
            from_at = Some(i);
        } else if c.column == n + 1 + n + 4 || c.column == n + 1 + n + 5 {
            slice_at.push(i);
        }
    }
    match (to_at, from_at) {
        (Some(t), Some(f)) => {
            omit[t] = true;
            omit[f] = true;
            argv.extend([n, n + 1 + n + 1]);
            return VcPlan {
                idx_num: 3,
                idx_str: Some(format!("both:{n},{}", n + 1 + n + 1)),
                omit,
                argv,
                cost: 10.0,
                rows: row_estimate.min(1024),
                order_consumed: false,
            };
        }
        (Some(t), None) => return take(t, n, 1, "to"),
        (None, Some(f)) => {
            return take(f, n + 1 + n + 1, 2, "from");
        }
        (None, None) => {}
    }
    if slice_at.len() == 2 {
        for i in &slice_at {
            omit[*i] = true;
            argv.push(constraints[*i].column);
        }
        let tag = argv
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",");
        return VcPlan {
            idx_num: 4,
            idx_str: Some(format!("slice:{tag}")),
            omit,
            argv,
            cost: 20.0,
            rows: row_estimate.min(4096),
            order_consumed: false,
        };
    }
    if slice_at.len() == 1 {
        return take(slice_at[0], constraints[slice_at[0]].column, 5, "range");
    }
    VcPlan {
        idx_num: 0,
        idx_str: None,
        omit,
        argv,
        cost: 1000.0 + row_estimate as f64,
        rows: row_estimate,
        order_consumed: false,
    }
}

pub fn plan_refs(constraints: &[VcConstraint], hidden: &[u32], row_estimate: u32) -> VcPlan {
    let from_col = hidden.first().copied().unwrap_or(u32::MAX);
    let to_col = hidden.get(1).copied().unwrap_or(u32::MAX);
    let tbl_col = hidden.get(2).copied();
    let mut omit = vec![false; constraints.len()];
    let mut argv: Vec<u32> = Vec::new();
    for (i, c) in constraints.iter().enumerate() {
        if !c.usable || c.op != VcOp::Eq {
            continue;
        }
        if c.column == from_col || c.column == to_col || Some(c.column) == tbl_col {
            if !argv.contains(&c.column) {
                argv.push(c.column);
            }
            omit[i] = true;
        }
    }
    let has_from = argv.contains(&from_col);
    let has_to = argv.contains(&to_col);
    if has_from && has_to {
        let mut cols = vec![from_col, to_col];
        if tbl_col.is_some_and(|t| argv.contains(&t)) {
            cols.push(tbl_col.expect("tbl column checked above"));
        }
        let tag = cols
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",");
        VcPlan {
            idx_num: 1,
            idx_str: Some(format!("refs:{tag}")),
            omit,
            argv,
            cost: 20.0,
            rows: row_estimate,
            order_consumed: false,
        }
    } else {
        VcPlan {
            idx_num: 0,
            idx_str: None,
            omit: vec![false; constraints.len()],
            argv: Vec::new(),
            cost: 1000.0 + row_estimate as f64,
            rows: row_estimate,
            order_consumed: false,
        }
    }
}

/// Split one range spec into `(from, to)` endpoint strings for the TVF
/// single-argument forms (`dolt_patch('a..b')`, `dolt_schema_diff('a..b')`).
/// `a...b` starts at the merge base; a bare revision diffs against itself
/// (zero rows).
pub fn split_range(provider: &dyn VcRead, spec: &str) -> VersionResult<(String, String)> {
    use crate::refs::Revision;
    let revision = crate::refs::parse_revision(spec)?;
    match revision {
        Revision::Range(left, right) => {
            super::vtab_log::reject_nested(&left, spec)?;
            super::vtab_log::reject_nested(&right, spec)?;
            Ok((left.to_string(), right.to_string()))
        }
        Revision::SymmetricDifference(left, right) => {
            super::vtab_log::reject_nested(&left, spec)?;
            super::vtab_log::reject_nested(&right, spec)?;
            let left_id = provider.resolve(&left.to_string())?;
            let right_id = provider.resolve(&right.to_string())?;
            let base = super::vtab_log::merge_base_of(provider, left_id, right_id)?;
            match base {
                Some(id) => Ok((id.to_hex(), right.to_string())),
                None => Ok((left.to_string(), right.to_string())),
            }
        }
        single => Ok((single.to_string(), single.to_string())),
    }
}

/// Did `table` change between `from` (None = empty) and `to`?
fn table_changed(
    provider: &dyn VcRead,
    table: &str,
    from: Option<&CommitId>,
    to: &CommitId,
) -> (bool, bool) {
    let old_rows = from.and_then(|id| provider.table_rows(table, id));
    let new_rows = provider.table_rows(table, to);
    let data_change = old_rows != new_rows;
    let old_schema = from.and_then(|id| provider.table_schema_sql(table, id));
    let new_schema = provider.table_schema_sql(table, to);
    (data_change, old_schema != new_schema)
}

/// One STAGED/WORKING pseudo-commit row per changed table. The snapshot
/// compare decides when both sides have snapshots; a table sitting in the
/// uncommitted set with no snapshots on either side still reads as a data
/// change, because reaching the set means CREATE (the only writer tracked
/// before O4), which always differs from its parent.
fn push_snapshot_diff(
    provider: &dyn VcRead,
    tables: &[String],
    uncommitted: &HashSet<String>,
    from: Option<&CommitId>,
    to: &CommitId,
    label: &str,
    out: &mut Vec<DiffRow>,
) {
    let mut names: Vec<String> = tables.to_vec();
    for name in uncommitted {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    for table in &names {
        let (data_change, schema_change) = table_changed(provider, table, from, to);
        if !data_change && !schema_change && !uncommitted.contains(table) {
            continue;
        }
        out.push(DiffRow {
            hash: label.to_string(),
            committer: String::new(),
            email: String::new(),
            date: String::new(),
            message: String::new(),
            data_change: data_change || uncommitted.contains(table),
            schema_change,
            table_name: table.clone(),
        });
    }
}

/// Diff one commit pair for one table, appending to `out`.
fn emit_table_pair(
    provider: &dyn VcRead,
    table: &str,
    from: Option<CommitId>,
    to: CommitId,
    out: &mut Vec<DiffTableRow>,
) {
    out.extend(diff_table_pair(provider, table, from, to));
}

/// Pair-diff one commit pair for one table. Shared because the vtable, stat,
/// summary, and patch paths must agree row-for-row: a second implementation
/// would drift on keying or ordering.
fn diff_table_pair(
    provider: &dyn VcRead,
    table: &str,
    from: Option<CommitId>,
    to: CommitId,
) -> Vec<DiffTableRow> {
    let old = from
        .as_ref()
        .and_then(|id| provider.table_rows(table, id))
        .unwrap_or_default();
    let next = provider.table_rows(table, &to).unwrap_or_default();
    if from.is_some() && provider.table_rows(table, &to).is_none() && old.is_empty() {
        return Vec::new();
    }
    // Rectangular snapshots (see `added_row`): every row carries exactly the
    // table's columns. A ragged provider is a bug, not data — catch it in
    // debug builds instead of emitting misaligned statements.
    if let Some(columns) = provider.table_columns(table) {
        debug_assert!(
            old.iter()
                .chain(next.iter())
                .all(|r| r.values.len() == columns.len()),
            "ragged snapshot for table {table}"
        );
    }
    let to_stamp = stamp_of(provider, &to);
    let from_stamp = from
        .as_ref()
        .map(|id| stamp_of(provider, id))
        .unwrap_or_else(|| ("".to_string(), "".to_string()));
    match provider.table_pk(table).filter(|pk| !pk.is_empty()) {
        Some(pk) => keyed_diff(provider, table, &pk, old, next, from_stamp, to_stamp),
        None => positional_diff(old, next, from_stamp, to_stamp),
    }
}

fn stamp_of(provider: &dyn VcRead, id: &CommitId) -> (String, String) {
    if *id == super::vtab_log::working_id() {
        return ("WORKING".to_string(), String::new());
    }
    if *id == super::vtab_log::staged_id() {
        return ("STAGED".to_string(), String::new());
    }
    match provider.commit_view(id) {
        Ok(view) => (view.id.to_hex(), view.timestamp.to_string()),
        Err(_) => (id.to_hex(), String::new()),
    }
}

/// Keyed diff on the primary-key columns; other columns decide `modified`.
fn keyed_diff(
    provider: &dyn VcRead,
    table: &str,
    pk: &[String],
    old: Vec<VcRow>,
    next: Vec<VcRow>,
    from_stamp: (String, String),
    to_stamp: (String, String),
) -> Vec<DiffTableRow> {
    let columns = provider.table_columns(table).unwrap_or_default();
    let positions: Vec<usize> = pk
        .iter()
        .filter_map(|name| columns.iter().position(|c| c == name))
        .collect();
    let key_of = |row: &VcRow| {
        positions
            .iter()
            .map(|p| cell_key(row.values.get(*p)))
            .collect::<Vec<_>>()
            .join("\x1f")
    };
    let mut old_map: HashMap<String, VcRow> = HashMap::new();
    let mut old_order: Vec<String> = Vec::new();
    for row in old {
        let key = key_of(&row);
        if !old_map.contains_key(&key) {
            old_order.push(key.clone());
        }
        old_map.insert(key, row);
    }
    let mut out = Vec::new();
    let mut seen_new: HashSet<String> = HashSet::new();
    for row in next {
        let key = key_of(&row);
        seen_new.insert(key.clone());
        match old_map.get(&key) {
            None => out.push(added_row(&row, &from_stamp, &to_stamp)),
            Some(before) if before.values.len() != row.values.len() => {
                // Same key but different widths cannot render as one UPDATE:
                // snapshots are rectangular, so this is corruption-grade
                // input, and a delete+add pair replays exactly.
                out.push(deleted_row(before, &from_stamp, &to_stamp));
                out.push(added_row(&row, &from_stamp, &to_stamp));
            }
            Some(before) if *before != row => out.push(DiffTableRow {
                to: row.values,
                from: before.values.clone(),
                to_commit: to_stamp.0.clone(),
                to_date: to_stamp.1.clone(),
                from_commit: from_stamp.0.clone(),
                from_date: from_stamp.1.clone(),
                diff_type: DiffType::Modified,
            }),
            _ => {}
        }
    }
    for key in old_order {
        if !seen_new.contains(&key) {
            out.push(DiffTableRow {
                to: Vec::new(),
                from: old_map.remove(&key).map(|r| r.values).unwrap_or_default(),
                to_commit: to_stamp.0.clone(),
                to_date: to_stamp.1.clone(),
                from_commit: from_stamp.0.clone(),
                from_date: from_stamp.1.clone(),
                diff_type: DiffType::Deleted,
            });
        }
    }
    out
}

/// Position-wise diff for tables without a primary key.
fn positional_diff(
    old: Vec<VcRow>,
    next: Vec<VcRow>,
    from_stamp: (String, String),
    to_stamp: (String, String),
) -> Vec<DiffTableRow> {
    let mut out = Vec::new();
    let shared = old.len().min(next.len());
    for i in 0..shared {
        if old[i] != next[i] {
            out.push(DiffTableRow {
                to: next[i].values.clone(),
                from: old[i].values.clone(),
                to_commit: to_stamp.0.clone(),
                to_date: to_stamp.1.clone(),
                from_commit: from_stamp.0.clone(),
                from_date: from_stamp.1.clone(),
                diff_type: DiffType::Modified,
            });
        }
    }
    for row in next.into_iter().skip(shared) {
        out.push(DiffTableRow {
            to: row.values,
            from: Vec::new(),
            to_commit: to_stamp.0.clone(),
            to_date: to_stamp.1.clone(),
            from_commit: from_stamp.0.clone(),
            from_date: from_stamp.1.clone(),
            diff_type: DiffType::Added,
        });
    }
    for row in old.into_iter().skip(shared) {
        out.push(DiffTableRow {
            to: Vec::new(),
            from: row.values,
            to_commit: to_stamp.0.clone(),
            to_date: to_stamp.1.clone(),
            from_commit: from_stamp.0.clone(),
            from_date: from_stamp.1.clone(),
            diff_type: DiffType::Deleted,
        });
    }
    out
}

fn cell_key(value: Option<&VcValue>) -> String {
    match value {
        Some(VcValue::Text(s)) => format!("t:{s}"),
        Some(VcValue::Integer(i)) => format!("i:{i}"),
        Some(VcValue::Real(bits)) => format!("r:{bits:016x}"),
        Some(VcValue::Blob(bytes)) => format!("b:{}", hex::encode(bytes)),
        Some(VcValue::Null) | None => "n:".to_string(),
    }
}

/// Snapshots are rectangular: every row carries exactly the table's columns.
/// The pair diff never pads or truncates, so a ragged snapshot surfaces here
/// as a delete+add instead of a misaligned UPDATE.
fn added_row(
    row: &VcRow,
    from_stamp: &(String, String),
    to_stamp: &(String, String),
) -> DiffTableRow {
    DiffTableRow {
        to: row.values.clone(),
        from: Vec::new(),
        to_commit: to_stamp.0.clone(),
        to_date: to_stamp.1.clone(),
        from_commit: from_stamp.0.clone(),
        from_date: from_stamp.1.clone(),
        diff_type: DiffType::Added,
    }
}

fn deleted_row(
    row: &VcRow,
    from_stamp: &(String, String),
    to_stamp: &(String, String),
) -> DiffTableRow {
    DiffTableRow {
        to: Vec::new(),
        from: row.values.clone(),
        to_commit: to_stamp.0.clone(),
        to_date: to_stamp.1.clone(),
        from_commit: from_stamp.0.clone(),
        from_date: from_stamp.1.clone(),
        diff_type: DiffType::Deleted,
    }
}

/// Counts for one table pair. Added rows contribute their width to
/// `cells_added`, deleted rows to `cells_deleted`, and modified rows their
/// changed-cell count to `cells_modified`.
fn stat_for_pair(
    provider: &dyn VcRead,
    table: &str,
    from_id: &CommitId,
    to_id: &CommitId,
) -> DiffStatRow {
    let old = provider.table_rows(table, from_id).unwrap_or_default();
    let next = provider.table_rows(table, to_id).unwrap_or_default();
    let width = provider.table_columns(table).map(|c| c.len()).unwrap_or(0) as i64;
    let mut stat = DiffStatRow {
        table: table.to_string(),
        rows_unmodified: 0,
        rows_added: 0,
        rows_deleted: 0,
        rows_modified: 0,
        cells_added: 0,
        cells_deleted: 0,
        cells_modified: 0,
        old_rows: old.len() as i64,
        new_rows: next.len() as i64,
        old_cells: old.len() as i64 * width,
        new_cells: next.len() as i64 * width,
    };
    for row in diff_table_pair(provider, table, Some(*from_id), *to_id) {
        match row.diff_type {
            DiffType::Added => {
                stat.rows_added += 1;
                stat.cells_added += width;
            }
            DiffType::Deleted => {
                stat.rows_deleted += 1;
                stat.cells_deleted += width;
            }
            DiffType::Modified => {
                stat.rows_modified += 1;
                stat.cells_modified += changed_cells(&row.from, &row.to);
            }
        }
    }
    stat.rows_unmodified = stat.new_rows - stat.rows_added - stat.rows_modified;
    stat
}

fn changed_cells(from: &[VcValue], to: &[VcValue]) -> i64 {
    from.iter().zip(to.iter()).filter(|(a, b)| a != b).count() as i64
        + (to.len().saturating_sub(from.len())) as i64
}
/// Rename guess: exactly one dropped and one added table with identical
/// columns and rows reads as a rename of dropped → added. The create
/// statements are deliberately excluded: a rename changes the table name
/// inside the SQL by construction, so text-compare would never match.
fn detect_rename(
    provider: &dyn VcRead,
    dropped: &[String],
    added: &[String],
    from_id: &CommitId,
    to_id: &CommitId,
) -> HashMap<String, String> {
    if dropped.len() != 1 || added.len() != 1 {
        return HashMap::new();
    }
    let (old_name, new_name) = (&dropped[0], &added[0]);
    let same_columns = provider.table_columns(old_name) == provider.table_columns(new_name)
        && provider.table_columns(old_name).is_some();
    let same_rows = provider.table_rows(old_name, from_id) == provider.table_rows(new_name, to_id);
    if same_columns && same_rows {
        HashMap::from([(new_name.clone(), old_name.clone())])
    } else {
        HashMap::new()
    }
}

/// Short commit label for patch rows: full hex (doltlite uses full hashes).
fn short_label(provider: &dyn VcRead, id: &CommitId, spec: &str) -> String {
    if *id == super::vtab_log::working_id() {
        return "WORKING".to_string();
    }
    if *id == super::vtab_log::staged_id() {
        return "STAGED".to_string();
    }
    match provider.commit_view(id) {
        Ok(view) => view.id.to_hex(),
        Err(_) => spec.to_string(),
    }
}

/// One data statement for a pair-diff row. `None` only for degenerate input
/// a SQL-built snapshot can never produce (zero columns, empty filter keys):
/// callers skip those rows, and the rectangular `debug_assert` in
/// `diff_table_pair` catches the provider bug in tests.
fn data_statement(table: &str, provider: &dyn VcRead, row: &DiffTableRow) -> Option<String> {
    let columns = provider.table_columns(table).unwrap_or_default();
    let pk = provider.table_pk(table).unwrap_or_default();
    match row.diff_type {
        DiffType::Added => {
            let names = columns
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let values = row.to.iter().map(literal).collect::<Vec<_>>().join(", ");
            Some(format!(
                "INSERT INTO \"{table}\" ({names}) VALUES ({values});"
            ))
        }
        DiffType::Deleted => {
            let filter = where_clause(&columns, &pk, &row.from)?;
            Some(format!("DELETE FROM \"{table}\" WHERE {filter};"))
        }
        DiffType::Modified => {
            let pk_positions: Vec<usize> = pk
                .iter()
                .filter_map(|name| columns.iter().position(|c| c == name))
                .collect();
            let mut sets = Vec::new();
            for (i, (new, old)) in row.to.iter().zip(row.from.iter()).enumerate() {
                if new == old || pk_positions.contains(&i) {
                    continue;
                }
                let name = columns.get(i)?;
                sets.push(format!("\"{name}\" = {}", literal(new)));
            }
            if sets.is_empty() {
                return None;
            }
            let filter = where_clause(&columns, &pk, &row.from)?;
            Some(format!(
                "UPDATE \"{table}\" SET {} WHERE {filter};",
                sets.join(", ")
            ))
        }
    }
}

fn where_clause(columns: &[String], pk: &[String], values: &[VcValue]) -> Option<String> {
    let keys: Vec<(String, VcValue)> = if pk.is_empty() {
        columns
            .iter()
            .zip(values.iter())
            .map(|(c, v)| (c.clone(), v.clone()))
            .collect()
    } else {
        pk.iter()
            .filter_map(|name| {
                columns
                    .iter()
                    .position(|c| c == name)
                    .and_then(|i| values.get(i).map(|v| (name.clone(), v.clone())))
            })
            .collect()
    };
    if keys.is_empty() {
        return None;
    }
    Some(
        keys.iter()
            .map(|(name, value)| match value {
                VcValue::Null => format!("\"{name}\" IS NULL"),
                _ => format!("\"{name}\" = {}", literal(value)),
            })
            .collect::<Vec<_>>()
            .join(" AND "),
    )
}

fn literal(value: &VcValue) -> String {
    match value {
        VcValue::Null => "NULL".to_string(),
        VcValue::Integer(i) => i.to_string(),
        VcValue::Real(bits) => f64::from_bits(*bits).to_string(),
        VcValue::Text(s) => format!("'{}'", s.replace('\'', "''")),
        VcValue::Blob(bytes) => format!("X'{}'", hex::encode(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::vtab_log::{MemSnapshotBuilder, MemVcRead, VcValue};
    use super::*;

    const SCHEMA: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)";

    fn seeded() -> (MemVcRead, CommitId, CommitId) {
        let mut p = MemVcRead::new("main");
        let c0 = p.push_commit(vec![], "Ada", "a@x", "first", 100);
        p.put_snapshot(
            "t",
            &["id", "v"],
            &["id"],
            c0,
            vec![
                vec![VcValue::Integer(1), VcValue::Text("a".into())],
                vec![VcValue::Integer(2), VcValue::Text("b".into())],
            ],
            SCHEMA,
        );
        let c1 = p.push_commit(vec![c0], "Bea", "b@x", "second", 200);
        p.put_snapshot(
            "t",
            &["id", "v"],
            &["id"],
            c1,
            vec![
                vec![VcValue::Integer(1), VcValue::Text("a2".into())],
                vec![VcValue::Integer(3), VcValue::Text("c".into())],
            ],
            SCHEMA,
        );
        (p, c0, c1)
    }

    #[test]
    fn diff_table_slice_labels_added_modified_deleted() {
        let (p, c0, c1) = seeded();
        let rows = diff_table_rows(&p, "t", Some(&c0.to_hex()), Some(&c1.to_hex())).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].diff_type, DiffType::Modified);
        assert_eq!(
            rows[0].to,
            vec![VcValue::Integer(1), VcValue::Text("a2".into())]
        );
        assert_eq!(
            rows[0].from,
            vec![VcValue::Integer(1), VcValue::Text("a".into())]
        );
        assert_eq!(rows[0].to_commit, c1.to_hex());
        assert_eq!(rows[0].from_commit, c0.to_hex());
        assert_eq!(rows[1].diff_type, DiffType::Added);
        assert!(rows[1].from.is_empty());
        assert_eq!(rows[2].diff_type, DiffType::Deleted);
        assert!(rows[2].to.is_empty());
    }

    #[test]
    fn diff_table_bare_walks_history() {
        let (p, _, c1) = seeded();
        let rows = diff_table_rows(&p, "t", None, None).unwrap();
        assert!(rows.iter().any(|r| r.to_commit == c1.to_hex()));
        assert!(rows
            .iter()
            .all(|r| r.diff_type != DiffType::Modified || r.to_commit == c1.to_hex()));
    }

    #[test]
    fn diff_table_unknown_table_reads_empty() {
        let (p, c0, c1) = seeded();
        let rows = diff_table_rows(&p, "ghost", Some(&c0.to_hex()), Some(&c1.to_hex())).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn diff_table_working_endpoint() {
        let (mut p, _, c1) = seeded();
        let mut tables = HashMap::new();
        tables.insert(
            "t".to_string(),
            MemSnapshotBuilder::new(&["id", "v"], &["id"], SCHEMA)
                .row(vec![VcValue::Integer(1), VcValue::Text("a3".into())]),
        );
        p.set_working(tables);
        let rows = diff_table_rows(&p, "t", Some(&c1.to_hex()), Some("WORKING")).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].to_commit, "WORKING");
        assert_eq!(rows[0].from_commit, c1.to_hex());
    }

    #[test]
    fn diff_table_no_pk_diffs_by_position() {
        let mut p = MemVcRead::new("main");
        let c0 = p.push_commit(vec![], "A", "a@x", "c0", 1);
        p.put_snapshot(
            "heap",
            &["v"],
            &[],
            c0,
            vec![vec![VcValue::Text("x".into())]],
            "CREATE TABLE heap (v TEXT)",
        );
        let c1 = p.push_commit(vec![c0], "A", "a@x", "c1", 2);
        p.put_snapshot(
            "heap",
            &["v"],
            &[],
            c1,
            vec![
                vec![VcValue::Text("y".into())],
                vec![VcValue::Text("z".into())],
            ],
            "CREATE TABLE heap (v TEXT)",
        );
        let rows = diff_table_rows(&p, "heap", Some(&c0.to_hex()), Some(&c1.to_hex())).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].diff_type, DiffType::Modified);
        assert_eq!(rows[1].diff_type, DiffType::Added);
    }

    #[test]
    fn diff_stat_counts_cells() {
        let (p, c0, c1) = seeded();
        let rows = diff_stat_rows(&p, &c0.to_hex(), &c1.to_hex(), None).unwrap();
        assert_eq!(rows.len(), 1);
        let s = &rows[0];
        assert_eq!(s.table, "t");
        assert_eq!(s.rows_added, 1);
        assert_eq!(s.rows_deleted, 1);
        assert_eq!(s.rows_modified, 1);
        assert_eq!(s.rows_unmodified, 0);
        assert_eq!(s.cells_added, 2);
        assert_eq!(s.cells_deleted, 2);
        assert_eq!(s.cells_modified, 1);
        assert_eq!(s.old_rows, 2);
        assert_eq!(s.new_rows, 2);
        assert_eq!(s.old_cells, 4);
        assert_eq!(s.new_cells, 4);
    }

    #[test]
    fn diff_stat_table_filter() {
        let (p, c0, c1) = seeded();
        assert_eq!(
            diff_stat_rows(&p, &c0.to_hex(), &c1.to_hex(), Some("t"))
                .unwrap()
                .len(),
            1
        );
        assert!(
            diff_stat_rows(&p, &c0.to_hex(), &c1.to_hex(), Some("ghost"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn diff_summary_modified_and_rename() {
        let (p, c0, c1) = seeded();
        let rows = diff_summary_rows(&p, &c0.to_hex(), &c1.to_hex(), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].diff_type, "modified");
        assert!(rows[0].data_change);
        assert!(!rows[0].schema_change);

        let mut q = MemVcRead::new("main");
        let d0 = q.push_commit(vec![], "A", "a@x", "c0", 1);
        q.put_snapshot(
            "old",
            &["id"],
            &["id"],
            d0,
            vec![vec![VcValue::Integer(1)]],
            "CREATE TABLE old (id INTEGER PRIMARY KEY)",
        );
        let d1 = q.push_commit(vec![d0], "A", "a@x", "c1", 2);
        q.put_snapshot(
            "new",
            &["id"],
            &["id"],
            d1,
            vec![vec![VcValue::Integer(1)]],
            "CREATE TABLE old (id INTEGER PRIMARY KEY)",
        );
        let rows = diff_summary_rows(&q, &d0.to_hex(), &d1.to_hex(), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].diff_type, "renamed");
        assert_eq!(rows[0].from_table, "old");
        assert_eq!(rows[0].to_table, "new");

        let mut r = MemVcRead::new("main");
        let e0 = r.push_commit(vec![], "A", "a@x", "c0", 1);
        r.put_snapshot(
            "old",
            &["id"],
            &["id"],
            e0,
            vec![vec![VcValue::Integer(1)]],
            "CREATE TABLE old (id INTEGER PRIMARY KEY)",
        );
        let e1 = r.push_commit(vec![e0], "A", "a@x", "c1", 2);
        r.put_snapshot(
            "new",
            &["id", "v"],
            &["id"],
            e1,
            vec![vec![VcValue::Integer(1), VcValue::Text("x".into())]],
            "CREATE TABLE new (id INTEGER PRIMARY KEY, v TEXT)",
        );
        let rows = diff_summary_rows(&r, &e0.to_hex(), &e1.to_hex(), None).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.diff_type == "added"));
        assert!(rows.iter().any(|row| row.diff_type == "dropped"));
    }

    #[test]
    fn schema_diff_reports_changed_create() {
        let mut p = MemVcRead::new("main");
        let c0 = p.push_commit(vec![], "A", "a@x", "c0", 1);
        p.put_snapshot(
            "t",
            &["id"],
            &["id"],
            c0,
            vec![vec![VcValue::Integer(1)]],
            "CREATE TABLE t (id INTEGER PRIMARY KEY)",
        );
        let c1 = p.push_commit(vec![c0], "A", "a@x", "c1", 2);
        p.put_snapshot(
            "t",
            &["id", "v"],
            &["id"],
            c1,
            vec![vec![VcValue::Integer(1), VcValue::Text("x".into())]],
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
        );
        let rows = schema_diff_rows(&p, &c0.to_hex(), &c1.to_hex(), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].from_create,
            "CREATE TABLE t (id INTEGER PRIMARY KEY)"
        );
        assert_eq!(
            rows[0].to_create,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"
        );
        assert!(schema_diff_rows(&p, &c1.to_hex(), &c1.to_hex(), None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn patch_is_ordered_and_semicolon_terminated() {
        let (p, c0, c1) = seeded();
        let rows = patch_rows(&p, &c0.to_hex(), &c1.to_hex(), None).unwrap();
        assert_eq!(rows.len(), 3);
        let orders: Vec<i64> = rows.iter().map(|r| r.order).collect();
        assert_eq!(orders, vec![0, 1, 2]);
        assert!(rows.iter().all(|r| r.statement.ends_with(';')));
        assert!(rows.iter().all(|r| r.diff_type == "data"));
        assert_eq!(
            rows[0].statement,
            "UPDATE \"t\" SET \"v\" = 'a2' WHERE \"id\" = 1;"
        );
        assert_eq!(
            rows[1].statement,
            "INSERT INTO \"t\" (\"id\", \"v\") VALUES (3, 'c');"
        );
        assert_eq!(rows[2].statement, "DELETE FROM \"t\" WHERE \"id\" = 2;");
        assert_eq!(rows[0].from_commit, c0.to_hex());
        assert_eq!(rows[0].to_commit, c1.to_hex());
    }

    #[test]
    fn patch_escapes_quotes_and_nulls() {
        let mut p = MemVcRead::new("main");
        let c0 = p.push_commit(vec![], "A", "a@x", "c0", 1);
        p.put_snapshot("t", &["id", "v"], &["id"], c0, vec![], SCHEMA);
        let c1 = p.push_commit(vec![c0], "A", "a@x", "c1", 2);
        p.put_snapshot(
            "t",
            &["id", "v"],
            &["id"],
            c1,
            vec![vec![VcValue::Integer(1), VcValue::Text("o'b".into())]],
            SCHEMA,
        );
        let rows = patch_rows(&p, &c0.to_hex(), &c1.to_hex(), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].statement,
            "INSERT INTO \"t\" (\"id\", \"v\") VALUES (1, 'o''b');"
        );
    }
    #[test]
    fn diff_rows_flags_changed_tables() {
        let (p, _, c1) = seeded();
        let rows = diff_rows(&p).unwrap();
        let tip_rows: Vec<&DiffRow> = rows.iter().filter(|r| r.hash == c1.to_hex()).collect();
        assert_eq!(tip_rows.len(), 1);
        assert!(tip_rows[0].data_change);
        assert!(!tip_rows[0].schema_change);
        assert_eq!(tip_rows[0].table_name, "t");
        assert_eq!(tip_rows[0].committer, "Bea");
    }

    #[test]
    fn diff_rows_lists_staged_table_without_commits() {
        use crate::staging::VcStore;
        let mut store = VcStore::new("main");
        store.track_table("t1");
        store.dolt_add(&["t1"]).unwrap();
        let rows = diff_rows(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hash, "STAGED");
        assert_eq!(rows[0].table_name, "t1");
        assert!(rows[0].data_change);
        store.reset_soft().unwrap();
        let rows = diff_rows(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hash, "WORKING");
    }
    #[test]
    fn schemas_match_doltlite_order() {
        assert_eq!(
            DOLT_DIFF_SCHEMA,
            "CREATE TABLE dolt_diff (commit_hash TEXT, committer TEXT, email TEXT, date TEXT, message TEXT, data_change INTEGER, schema_change INTEGER, table_name TEXT)"
        );
        assert_eq!(
            diff_table_schema("t", &["id".to_string(), "v".to_string()]),
            "CREATE TABLE dolt_diff_t (\"to_id\" TEXT, \"to_v\" TEXT, to_commit TEXT, to_commit_date TEXT, \"from_id\" TEXT, \"from_v\" TEXT, from_commit TEXT, from_commit_date TEXT, diff_type TEXT, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN)"
        );
        assert_eq!(
            DOLT_DIFF_STAT_SCHEMA,
            "CREATE TABLE dolt_diff_stat (table_name TEXT, rows_unmodified INTEGER, rows_added INTEGER, rows_deleted INTEGER, rows_modified INTEGER, cells_added INTEGER, cells_deleted INTEGER, cells_modified INTEGER, old_row_count INTEGER, new_row_count INTEGER, old_cell_count INTEGER, new_cell_count INTEGER, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN, tbl TEXT HIDDEN)"
        );
        assert_eq!(
            DOLT_DIFF_SUMMARY_SCHEMA,
            "CREATE TABLE dolt_diff_summary (from_table_name TEXT, to_table_name TEXT, diff_type TEXT, data_change INTEGER, schema_change INTEGER, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN, tbl TEXT HIDDEN)"
        );
        assert_eq!(
            DOLT_SCHEMA_DIFF_SCHEMA,
            "CREATE TABLE dolt_schema_diff (from_table_name TEXT, to_table_name TEXT, from_create_statement TEXT, to_create_statement TEXT, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN, table_name TEXT HIDDEN)"
        );
        assert_eq!(
            DOLT_PATCH_SCHEMA,
            "CREATE TABLE dolt_patch (statement_order INTEGER, from_commit_hash TEXT, to_commit_hash TEXT, table_name TEXT, diff_type TEXT, statement TEXT, arg1 TEXT HIDDEN, arg2 TEXT HIDDEN, arg3 TEXT HIDDEN)"
        );
    }

    #[test]
    fn plan_diff_table_probes() {
        let cs = vec![VcConstraint {
            column: 2,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_diff_table(2, &cs, 9).idx_num, 1);
        let cs = vec![VcConstraint {
            column: 6,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_diff_table(2, &cs, 9).idx_num, 2);
        let cs = vec![
            VcConstraint {
                column: 2,
                op: VcOp::Eq,
                usable: true,
            },
            VcConstraint {
                column: 6,
                op: VcOp::Eq,
                usable: true,
            },
        ];
        let plan = plan_diff_table(2, &cs, 9);
        assert_eq!(plan.idx_num, 3);
        assert_eq!(plan.omit, vec![true, true]);
        let cs = vec![
            VcConstraint {
                column: 9,
                op: VcOp::Eq,
                usable: true,
            },
            VcConstraint {
                column: 10,
                op: VcOp::Eq,
                usable: true,
            },
        ];
        assert_eq!(plan_diff_table(2, &cs, 9).idx_num, 4);
        let cs = vec![VcConstraint {
            column: 9,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_diff_table(2, &cs, 9).idx_num, 5);
        assert_eq!(plan_diff_table(2, &[], 9).idx_num, 0);
    }

    #[test]
    fn plan_columns_come_from_the_schema_not_memory() {
        // The planner's probe columns must match the schema string the engine
        // parses: derive the positions here instead of hard-coding them.
        let schema = diff_table_schema("t", &["id".to_string(), "v".to_string()]);
        let cols: Vec<&str> = schema
            .trim_start_matches("CREATE TABLE dolt_diff_t (")
            .trim_end_matches(')')
            .split(", ")
            .collect();
        let pos = |name: &str| cols.iter().position(|c| c.starts_with(name)).unwrap() as u32;
        let cs = vec![VcConstraint {
            column: pos("to_commit"),
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_diff_table(2, &cs, 9).idx_num, 1);
        let cs = vec![VcConstraint {
            column: pos("from_commit"),
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_diff_table(2, &cs, 9).idx_num, 2);
    }

    #[test]
    fn plan_diff_and_refs() {
        let cs = vec![VcConstraint {
            column: 0,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_diff(&cs, 9).idx_num, 1);
        let cs = vec![VcConstraint {
            column: 7,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_diff(&cs, 9).idx_num, 2);
        let cs = vec![
            VcConstraint {
                column: 12,
                op: VcOp::Eq,
                usable: true,
            },
            VcConstraint {
                column: 13,
                op: VcOp::Eq,
                usable: true,
            },
        ];
        let plan = plan_refs(&cs, &[12, 13, 14], 9);
        assert_eq!(plan.idx_num, 1);
        assert_eq!(plan.idx_str.as_deref(), Some("refs:12,13"));
        assert_eq!(plan_refs(&[], &[12, 13, 14], 9).idx_num, 0);
        let one = vec![VcConstraint {
            column: 12,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_refs(&one, &[12, 13, 14], 9).idx_num, 0);
    }

    #[test]
    fn split_range_forms() {
        let (p, c0, c1) = seeded();
        assert_eq!(
            split_range(&p, &format!("{}..{}", c0.to_hex(), c1.to_hex())).unwrap(),
            (c0.to_hex(), c1.to_hex())
        );
        let (from, to) = split_range(&p, &format!("{}...{}", c0.to_hex(), c1.to_hex())).unwrap();
        assert_eq!(from, c0.to_hex());
        assert_eq!(to, c1.to_hex());
        let (same, _) = split_range(&p, "HEAD").unwrap();
        assert_eq!(same, "HEAD");
    }

    #[test]
    fn split_range_rejects_nested() {
        let (p, _, _) = seeded();
        assert_eq!(
            split_range(&p, "a..b..c").unwrap_err().to_string(),
            "invalid revision spec: 'a..b..c'"
        );
    }
}
