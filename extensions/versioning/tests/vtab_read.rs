//! O3 read-path matrix: three commits across two branches, one table.
//!
//! Fails without `vtab_log`/`vtab_history`/`vtab_diff`; passes with them.
use std::collections::HashMap;

use turso_versioning::model::CommitId;
use turso_versioning::vtab_diff::{
    diff_stat_rows, diff_summary_rows, diff_table_rows, patch_rows, plan_diff_table,
    schema_diff_rows, DiffType,
};
use turso_versioning::vtab_history::{at_rows, blame_rows, history_rows};
use turso_versioning::vtab_log::{
    log_probe, log_rows, plan_log, MemSnapshotBuilder, MemVcRead, VcConstraint, VcOp, VcOrderBy,
    VcValue,
};

const SCHEMA: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)";

/// main: c0 -> c1 ; dev branches at c0 -> c2. Head is dev.
fn graph() -> (MemVcRead, CommitId, CommitId, CommitId) {
    let mut p = MemVcRead::new("main");
    let c0 = p.push_commit(vec![], "Ada", "a@x", "base", 100);
    p.put_snapshot(
        "t",
        &["id", "v"],
        &["id"],
        c0,
        vec![vec![VcValue::Integer(1), VcValue::Text("a".into())]],
        SCHEMA,
    );
    let c1 = p.push_commit(vec![c0], "Ada", "a@x", "main-work", 200);
    p.put_snapshot(
        "t",
        &["id", "v"],
        &["id"],
        c1,
        vec![
            vec![VcValue::Integer(1), VcValue::Text("a".into())],
            vec![VcValue::Integer(2), VcValue::Text("b".into())],
        ],
        SCHEMA,
    );
    p.set_branch_tip("dev", c0);
    p.set_head_branch("dev");
    let c2 = p.push_commit(vec![c0], "Bea", "b@x", "dev-work", 300);
    p.put_snapshot(
        "t",
        &["id", "v"],
        &["id"],
        c2,
        vec![vec![VcValue::Integer(1), VcValue::Text("z".into())]],
        SCHEMA,
    );
    (p, c0, c1, c2)
}

#[test]
fn matrix_log_covers_both_branches() {
    let (p, c0, _, c2) = graph();
    let rows = log_rows(&p, None).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].hash, c2.to_hex());
    assert_eq!(rows[1].hash, c0.to_hex());
    let dev_only = log_rows(&p, Some("main...dev")).unwrap();
    assert_eq!(dev_only.len(), 1);
    assert_eq!(dev_only[0].hash, c2.to_hex());
    let main_only = log_rows(&p, Some("dev..main")).unwrap();
    assert_eq!(main_only.len(), 1);
    assert_eq!(main_only[0].message, "main-work");
    let probe = log_probe(&p, &c0.to_hex()).unwrap().unwrap();
    assert_eq!(probe.message, "base");
}

#[test]
fn matrix_history_at_blame_agree() {
    let (p, c0, _, _) = graph();
    let history = history_rows(&p, "t", None).unwrap();
    assert_eq!(history.len(), 2);
    let at = at_rows(&p, "t", Some(&c0.to_hex())).unwrap();
    assert_eq!(at.len(), 1);
    assert_eq!(
        at[0].values,
        vec![VcValue::Integer(1), VcValue::Text("a".into())]
    );
    let blame = blame_rows(&p, "t").unwrap();
    assert_eq!(blame.len(), 1);
    assert_eq!(blame[0].committer, "Bea");
    assert_eq!(
        at_rows(&p, "t", None).unwrap_err().to_string(),
        format!("ref required: dolt_at_t needs a revision argument")
    );
}

#[test]
fn matrix_diff_stat_patch_agree() {
    let (p, c0, _, c2) = graph();
    let diffs = diff_table_rows(&p, "t", Some(&c0.to_hex()), Some(&c2.to_hex())).unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].diff_type, DiffType::Modified);
    let stats = diff_stat_rows(&p, &c0.to_hex(), &c2.to_hex(), None).unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].rows_modified, 1);
    assert_eq!(stats[0].cells_modified, 1);
    let summary = diff_summary_rows(&p, &c0.to_hex(), &c2.to_hex(), None).unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].diff_type, "modified");
    let patch = patch_rows(&p, &c0.to_hex(), &c2.to_hex(), None).unwrap();
    assert_eq!(patch.len(), 1);
    assert_eq!(
        patch[0].statement,
        "UPDATE \"t\" SET \"v\" = 'z' WHERE \"id\" = 1;"
    );
    assert!(schema_diff_rows(&p, &c0.to_hex(), &c2.to_hex(), None)
        .unwrap()
        .is_empty());
}

#[test]
fn matrix_plans_pick_probes() {
    let hash = vec![VcConstraint {
        column: 0,
        op: VcOp::Eq,
        usable: true,
    }];
    assert_eq!(plan_log(&hash, &[], 3).idx_num, 1);
    let ordered = plan_log(
        &[],
        &[VcOrderBy {
            column: 3,
            desc: true,
        }],
        3,
    );
    assert!(ordered.order_consumed);
    let to = vec![VcConstraint {
        column: 2,
        op: VcOp::Eq,
        usable: true,
    }];
    assert_eq!(plan_diff_table(2, &to, 3).idx_num, 1);
}

#[test]
fn matrix_working_snapshot_flows_through() {
    let (mut p, _, _, c2) = graph();
    let mut tables = HashMap::new();
    tables.insert(
        "t".to_string(),
        MemSnapshotBuilder::new(&["id", "v"], &["id"], SCHEMA)
            .row(vec![VcValue::Integer(1), VcValue::Text("w".into())]),
    );
    p.set_working(tables);
    let diffs = diff_table_rows(&p, "t", Some(&c2.to_hex()), Some("WORKING")).unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].to_commit, "WORKING");
    let history = history_rows(&p, "t", Some("WORKING")).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].hash, "WORKING");
}
