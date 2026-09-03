//! Branch-scoped row views: `dolt_history_<t>`, `dolt_at_<t>`, `dolt_blame_<t>`, `dolt_schemas`.
//!
//! H1–H4. doltlite builds one vtable module per user table at connect
//! (`atRegisterOne` in `doltlite_at.c` registers at/history/diff together).
//! The pure rows here take an explicit table name; the engine glue registers
//! the per-table modules. All row content comes from the `VcRead` provider.

use std::collections::HashMap;

use super::vtab_log::{VcConstraint, VcOp, VcPlan, VcRead, VcValue};
use crate::model::{CommitId, VersionError, VersionResult};

/// One `dolt_history_<t>` row: the stored row plus its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub values: Vec<VcValue>,
    pub hash: String,
    pub committer: String,
    pub date: String,
    /// The `start_ref` TVF argument (or head label) this walk started from.
    /// Renders as the hidden `start_ref` column; the engine glue binds it.
    pub start: String,
}

/// One `dolt_at_<t>` row: the stored row at one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtRow {
    pub values: Vec<VcValue>,
}

/// One `dolt_blame_<t>` row: primary-key values plus the newest commit
/// that carried the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameRow {
    pub pk_values: Vec<VcValue>,
    pub hash: String,
    pub date: String,
    pub committer: String,
    pub email: String,
    pub message: String,
}

/// One `dolt_schemas` row: a stored view or trigger definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaRow {
    pub kind: String,
    pub name: String,
    pub fragment: String,
}

/// A view or trigger definition backing `dolt_schemas`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaObject {
    pub kind: String,
    pub name: String,
    pub sql: String,
}

/// Rows for `dolt_history_<t>('start_ref')`: every version of every row of
/// `table`, newest commit first. The bare form starts at the branch head.
/// Unknown table is `table not found: NAME`; a bad ref fails to resolve.
/// A `WORKING`/`STAGED` start labels its rows with that literal hash.
pub fn history_rows(
    provider: &dyn VcRead,
    table: &str,
    start_ref: Option<&str>,
) -> VersionResult<Vec<HistoryRow>> {
    if provider.table_columns(table).is_none() {
        return Err(VersionError::TableNotFound(table.to_string()));
    }
    let (tip, label) = match start_ref {
        None => (provider.head()?, provider.head_label()),
        Some(spec) => {
            let id = provider.resolve(spec)?;
            (id, spec.to_string())
        }
    };
    let commits = super::vtab_log::commit_chain(provider, tip)?;
    let mut out = Vec::new();
    for id in commits {
        let (hash, committer, date) = commit_stamp(provider, &id)?;
        let Some(rows) = provider.table_rows(table, &id) else {
            continue;
        };
        for row in rows {
            out.push(HistoryRow {
                values: row.values,
                hash: hash.clone(),
                committer: committer.clone(),
                date: date.clone(),
                start: label.clone(),
            });
        }
    }
    Ok(out)
}

/// Rows for `dolt_at_<t>('ref')`: the table exactly as it stood at `rev`.
/// The revision argument is mandatory: `None` is the `ref required` error,
/// never an implicit HEAD read.
pub fn at_rows(provider: &dyn VcRead, table: &str, rev: Option<&str>) -> VersionResult<Vec<AtRow>> {
    let Some(spec) = rev else {
        return Err(VersionError::AtRefRequired(table.to_string()));
    };
    if provider.table_columns(table).is_none() {
        return Err(VersionError::TableNotFound(table.to_string()));
    }
    let id = provider.resolve(spec)?;
    let Some(rows) = provider.table_rows(table, &id) else {
        return Ok(Vec::new());
    };
    Ok(rows
        .into_iter()
        .map(|row| AtRow { values: row.values })
        .collect())
}

/// Rows for `dolt_blame_<t>`: for each primary-key value, the newest commit
/// carrying that row. Row identity is the text form of the PK values.
/// A table with no primary key is an error, never a silent full-row guess.
pub fn blame_rows(provider: &dyn VcRead, table: &str) -> VersionResult<Vec<BlameRow>> {
    if provider.table_columns(table).is_none() {
        return Err(VersionError::TableNotFound(table.to_string()));
    }
    let pk = provider
        .table_pk(table)
        .filter(|pk| !pk.is_empty())
        .ok_or_else(|| VersionError::TableHasNoPrimaryKey(table.to_string()))?;
    let columns = provider.table_columns(table).unwrap_or_default();
    let positions: Vec<usize> = pk
        .iter()
        .map(|name| columns.iter().position(|c| c == name))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| VersionError::TableHasNoPrimaryKey(table.to_string()))?;
    let tip = provider.head()?;
    let commits = super::vtab_log::commit_chain(provider, tip)?;
    let mut claimed: HashMap<String, BlameRow> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for id in commits {
        let view = provider.commit_view(&id)?;
        let Some(rows) = provider.table_rows(table, &id) else {
            continue;
        };
        for row in rows {
            let key = positions
                .iter()
                .map(|p| cell_text(row.values.get(*p)))
                .collect::<Vec<_>>()
                .join("\x1f");
            if claimed.contains_key(&key) {
                continue;
            }
            order.push(key.clone());
            claimed.insert(
                key,
                BlameRow {
                    pk_values: positions
                        .iter()
                        .map(|p| row.values.get(*p).cloned().unwrap_or(VcValue::Null))
                        .collect(),
                    hash: view.id.to_hex(),
                    date: view.timestamp.to_string(),
                    committer: view.name.clone(),
                    email: view.email.clone(),
                    message: view.message.clone(),
                },
            );
        }
    }
    Ok(order
        .into_iter()
        .map(|k| claimed.remove(&k).expect("blame key claimed above"))
        .collect())
}

/// Rows for `dolt_schemas`: stored views and triggers, in name order.
/// `extra` and `sql_mode` always read NULL at the glue layer.
pub fn schemas_rows(objects: &[SchemaObject]) -> Vec<SchemaRow> {
    let mut rows: Vec<SchemaRow> = objects
        .iter()
        .map(|o| SchemaRow {
            kind: o.kind.clone(),
            name: o.name.clone(),
            fragment: o.sql.clone(),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// Declared schemas, column order 1:1 with doltlite's `htBuildSchema`,
/// `atBuildSchema`, `blameBuildSchema`, and `zSchemasSchema`.
pub fn history_schema(table: &str, columns: &[String]) -> String {
    format!(
        "CREATE TABLE dolt_history_{table} ({cols}commit_hash TEXT, committer TEXT, commit_date TEXT, start_ref TEXT HIDDEN)",
        cols = columns
            .iter()
            .map(|c| format!("\"{c}\" TEXT, "))
            .collect::<String>(),
    )
}

pub fn at_schema(table: &str, columns: &[String]) -> String {
    format!(
        "CREATE TABLE dolt_at_{table} ({cols}commit_ref TEXT HIDDEN)",
        cols = columns
            .iter()
            .map(|c| format!("\"{c}\" TEXT, "))
            .collect::<String>(),
    )
}

pub fn blame_schema(table: &str, pk: &[String]) -> String {
    format!(
        "CREATE TABLE dolt_blame_{table} ({cols}\"commit\" TEXT, commit_date TEXT, committer TEXT, email TEXT, message TEXT)",
        cols = pk
            .iter()
            .map(|c| format!("\"{c}\" TEXT, "))
            .collect::<String>(),
    )
}

pub const DOLT_SCHEMAS_SCHEMA: &str =
    "CREATE TABLE dolt_schemas (type TEXT, name TEXT, fragment TEXT, extra TEXT, sql_mode TEXT)";

/// Pushdown plans. History consumes `commit_hash EQ` and the hidden
/// `start_ref EQ`; `at` slices on the hidden `commit_ref EQ` and costs 1e12
/// without it (doltlite's mandatory-ref cost); blame probes a single PK
/// column; schemas consume `type EQ` and `name EQ`.
pub fn plan_history(
    live_columns: usize,
    constraints: &[VcConstraint],
    row_estimate: u32,
) -> VcPlan {
    plan_ref_pair(
        constraints,
        live_columns as u32,
        live_columns as u32 + 3,
        "hash",
        "start",
        row_estimate,
    )
}

pub fn plan_at(live_columns: usize, constraints: &[VcConstraint], row_estimate: u32) -> VcPlan {
    let mut omit = vec![false; constraints.len()];
    let mut argv = Vec::new();
    for (i, c) in constraints.iter().enumerate() {
        if c.usable && c.op == VcOp::Eq && c.column == live_columns as u32 {
            argv.push(c.column);
            omit[i] = true;
            return VcPlan {
                idx_num: 1,
                idx_str: Some("ref".to_string()),
                omit,
                argv,
                cost: 10.0,
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
        cost: 1e12,
        rows: row_estimate,
        order_consumed: false,
    }
}

pub fn plan_blame(pk_columns: usize, constraints: &[VcConstraint], row_estimate: u32) -> VcPlan {
    let mut omit = vec![false; constraints.len()];
    let mut argv = Vec::new();
    if pk_columns == 1 {
        for (i, c) in constraints.iter().enumerate() {
            if c.usable && c.op == VcOp::Eq && c.column == 0 {
                argv.push(0);
                omit[i] = true;
                return VcPlan {
                    idx_num: 1,
                    idx_str: Some("pk".to_string()),
                    omit,
                    argv,
                    cost: 10.0,
                    rows: 1,
                    order_consumed: false,
                };
            }
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

pub fn plan_schemas(constraints: &[VcConstraint], row_estimate: u32) -> VcPlan {
    plan_ref_pair(constraints, 0, 1, "type", "name", row_estimate)
}

/// Text form of one primary-key cell for blame keying. Typed prefixes keep
/// `Null` apart from `Text("")` and integers apart from their text twins.
fn cell_text(value: Option<&VcValue>) -> String {
    match value {
        Some(VcValue::Text(s)) => format!("t:{s}"),
        Some(VcValue::Integer(i)) => format!("i:{i}"),
        _ => "n:".to_string(),
    }
}

/// Stamp for one history commit: snapshots read as their literal name.
fn commit_stamp(provider: &dyn VcRead, id: &CommitId) -> VersionResult<(String, String, String)> {
    if *id == super::vtab_log::working_id() {
        return Ok(("WORKING".to_string(), String::new(), String::new()));
    }
    if *id == super::vtab_log::staged_id() {
        return Ok(("STAGED".to_string(), String::new(), String::new()));
    }
    let view = provider.commit_view(id)?;
    Ok((view.id.to_hex(), view.name, view.timestamp.to_string()))
}

/// Shared two-probe planner: `first_col EQ` then `second_col EQ`.
fn plan_ref_pair(
    constraints: &[VcConstraint],
    first_col: u32,
    second_col: u32,
    first_name: &str,
    second_name: &str,
    row_estimate: u32,
) -> VcPlan {
    let mut omit = vec![false; constraints.len()];
    let mut argv = Vec::new();
    for (i, c) in constraints.iter().enumerate() {
        if !c.usable || c.op != VcOp::Eq {
            continue;
        }
        if c.column == first_col && !argv.contains(&first_col) {
            argv.push(first_col);
            omit[i] = true;
            return VcPlan {
                idx_num: 1,
                idx_str: Some(first_name.to_string()),
                omit,
                argv,
                cost: 10.0,
                rows: 1,
                order_consumed: false,
            };
        }
        if c.column == second_col && !argv.contains(&second_col) {
            argv.push(second_col);
            omit[i] = true;
            return VcPlan {
                idx_num: 2,
                idx_str: Some(second_name.to_string()),
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

#[cfg(test)]
mod tests {
    use super::super::vtab_log::{MemSnapshotBuilder, MemVcRead, VcValue};
    use super::*;

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
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
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
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
        );
        (p, c0, c1)
    }

    #[test]
    fn history_lists_every_version_newest_first() {
        let (p, c0, c1) = seeded();
        let rows = history_rows(&p, "t", None).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].hash, c1.to_hex());
        assert_eq!(rows[0].committer, "Bea");
        assert_eq!(rows[0].date, "200");
        assert_eq!(rows[0].start, "main");
        assert_eq!(rows[2].hash, c0.to_hex());
        assert_eq!(rows[2].committer, "Ada");
        assert_eq!(rows[2].start, "main");
    }

    #[test]
    fn history_start_ref_slices() {
        let (p, c0, _) = seeded();
        let rows = history_rows(&p, "t", Some(&c0.to_hex())).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.hash == c0.to_hex()));
        assert!(rows.iter().all(|r| r.start == c0.to_hex()));
    }

    #[test]
    fn history_unknown_table_errors() {
        let (p, _, _) = seeded();
        assert_eq!(
            history_rows(&p, "ghost", None).unwrap_err().to_string(),
            "table not found: ghost"
        );
    }

    #[test]
    fn history_working_start_labels_working() {
        let (mut p, _, _) = seeded();
        let mut tables = HashMap::new();
        tables.insert(
            "t".to_string(),
            MemSnapshotBuilder::new(
                &["id", "v"],
                &["id"],
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            )
            .row(vec![VcValue::Integer(9), VcValue::Text("w".into())]),
        );
        p.set_working(tables);
        let rows = history_rows(&p, "t", Some("WORKING")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hash, "WORKING");
        assert_eq!(rows[0].start, "WORKING");
    }

    #[test]
    fn at_returns_exact_snapshot() {
        let (p, c0, _) = seeded();
        let rows = at_rows(&p, "t", Some(&c0.to_hex())).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].values,
            vec![VcValue::Integer(1), VcValue::Text("a".into())]
        );
    }

    #[test]
    fn at_without_ref_errors() {
        let (p, _, _) = seeded();
        assert_eq!(
            at_rows(&p, "t", None).unwrap_err().to_string(),
            "ref required: dolt_at_t needs a revision argument"
        );
    }

    #[test]
    fn at_unknown_table_errors_before_resolve() {
        let (p, c0, _) = seeded();
        assert_eq!(
            at_rows(&p, "ghost", Some(&c0.to_hex()))
                .unwrap_err()
                .to_string(),
            "table not found: ghost"
        );
    }

    #[test]
    fn blame_attributes_newest_commit_per_key() {
        let (p, c0, c1) = seeded();
        let rows = blame_rows(&p, "t").unwrap();
        assert_eq!(rows.len(), 3);
        let by_key: HashMap<String, &BlameRow> = rows
            .iter()
            .map(|r| (cell_text(r.pk_values.first()), r))
            .collect();
        assert_eq!(by_key["i:1"].hash, c1.to_hex());
        assert_eq!(by_key["i:1"].committer, "Bea");
        assert_eq!(by_key["i:1"].message, "second");
        assert_eq!(by_key["i:2"].hash, c0.to_hex());
        assert_eq!(by_key["i:3"].hash, c1.to_hex());
    }

    #[test]
    fn blame_without_pk_errors() {
        let mut p = MemVcRead::new("main");
        let c0 = p.push_commit(vec![], "Ada", "a@x", "first", 100);
        p.put_snapshot(
            "heap",
            &["v"],
            &[],
            c0,
            vec![vec![VcValue::Text("x".into())]],
            "CREATE TABLE heap (v TEXT)",
        );
        assert_eq!(
            blame_rows(&p, "heap").unwrap_err().to_string(),
            "table has no primary key: heap"
        );
    }

    #[test]
    fn schemas_lists_views_and_triggers_sorted() {
        let rows = schemas_rows(&[
            SchemaObject {
                kind: "trigger".into(),
                name: "z_trg".into(),
                sql: "CREATE TRIGGER z_trg ...".into(),
            },
            SchemaObject {
                kind: "view".into(),
                name: "a_view".into(),
                sql: "CREATE VIEW a_view AS SELECT 1".into(),
            },
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "a_view");
        assert_eq!(rows[0].kind, "view");
        assert_eq!(rows[1].name, "z_trg");
    }

    #[test]
    fn schema_strings_match_doltlite_order() {
        assert_eq!(
            history_schema("t", &["id".to_string(), "v".to_string()]),
            "CREATE TABLE dolt_history_t (\"id\" TEXT, \"v\" TEXT, commit_hash TEXT, committer TEXT, commit_date TEXT, start_ref TEXT HIDDEN)"
        );
        assert_eq!(
            at_schema("t", &["id".to_string()]),
            "CREATE TABLE dolt_at_t (\"id\" TEXT, commit_ref TEXT HIDDEN)"
        );
        assert_eq!(
            blame_schema("t", &["id".to_string()]),
            "CREATE TABLE dolt_blame_t (\"id\" TEXT, \"commit\" TEXT, commit_date TEXT, committer TEXT, email TEXT, message TEXT)"
        );
        assert_eq!(
            DOLT_SCHEMAS_SCHEMA,
            "CREATE TABLE dolt_schemas (type TEXT, name TEXT, fragment TEXT, extra TEXT, sql_mode TEXT)"
        );
    }

    #[test]
    fn plan_at_without_ref_costs_1e12() {
        let plan = plan_at(2, &[], 10);
        assert_eq!(plan.idx_num, 0);
        assert_eq!(plan.cost, 1e12);
        let cs = vec![VcConstraint {
            column: 2,
            op: VcOp::Eq,
            usable: true,
        }];
        let probe = plan_at(2, &cs, 10);
        assert_eq!(probe.idx_num, 1);
        assert!(probe.omit[0]);
    }

    #[test]
    fn plan_history_probes_hash_then_start() {
        let cs = vec![VcConstraint {
            column: 2,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_history(2, &cs, 9).idx_num, 1);
        let cs = vec![VcConstraint {
            column: 5,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_history(2, &cs, 9).idx_num, 2);
    }

    #[test]
    fn plan_blame_probes_single_pk_only() {
        let cs = vec![VcConstraint {
            column: 0,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_blame(1, &cs, 9).idx_num, 1);
        assert_eq!(plan_blame(2, &cs, 9).idx_num, 0);
    }

    #[test]
    fn plan_schemas_probes_type_then_name() {
        let cs = vec![VcConstraint {
            column: 0,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_schemas(&cs, 4).idx_num, 1);
        let cs = vec![VcConstraint {
            column: 1,
            op: VcOp::Eq,
            usable: true,
        }];
        assert_eq!(plan_schemas(&cs, 4).idx_num, 2);
    }

    #[test]
    fn new_error_strings_are_exact() {
        assert_eq!(
            VersionError::TableHasNoPrimaryKey("heap".into()).to_string(),
            "table has no primary key: heap"
        );
        assert_eq!(
            VersionError::AtRefRequired("t".into()).to_string(),
            "ref required: dolt_at_t needs a revision argument"
        );
    }
}
