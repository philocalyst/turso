//! Ref-listing rows for `dolt_branches` and `dolt_tags`: one row per local
//! branch and per tag, with the active branch and working dirtiness flagged.
//! Fails without the `vtab_refs` module; passes with it.

use turso_versioning::staging::VcStore;
use turso_versioning::vtab_log::{VcRow, VcValue};
use turso_versioning::vtab_refs::{branches_rows, tags_rows};

const SCHEMA: &str = "";

fn seeded_store() -> VcStore {
    let mut s = VcStore::new("main");
    s.config_set("user.name", "Ada");
    s.config_set("user.email", "ada@example.com");
    s.track_table("t1");
    s.dolt_add(&["t1"]).unwrap();
    s.set_now(1);
    s.dolt_commit("seed", None, false, false).unwrap();
    s
}

fn work_rows(v: &str) -> Vec<VcRow> {
    vec![VcRow::new(vec![
        VcValue::Integer(1),
        VcValue::Text(v.to_string()),
    ])]
}

#[test]
fn branches_rows_list_local_branches_with_tip_and_active_flag() {
    let mut s = seeded_store();
    s.create_branch("feature").unwrap();
    let rows = branches_rows(&s);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "feature");
    assert_eq!(rows[1].name, "main");
    assert_eq!(rows[0].hash, rows[1].hash);
    assert!(rows[1].hash.len() == 40, "hash is 40-hex: {}", rows[1].hash);
    assert_eq!(rows[1].latest_commit_message, "seed");
    assert_eq!(rows[0].latest_commit_message, "seed");
    assert!(!rows[0].branch, "feature is not active");
    assert!(rows[1].branch, "main is active");
    assert_eq!(rows[0].remote, "");
    assert!(!rows[0].dirty);
    assert!(!rows[1].dirty);
}

#[test]
fn branches_rows_dirty_flags_active_branch_working_changes() {
    let mut s = seeded_store();
    s.create_branch("feature").unwrap();
    s.apply_work(
        "t1",
        vec!["id".to_string(), "v".to_string()],
        vec!["id".to_string()],
        work_rows("changed"),
        SCHEMA.to_string(),
    );
    let rows = branches_rows(&s);
    assert!(!rows[0].dirty, "feature row stays clean");
    assert!(rows[1].dirty, "main carries the working change");
}

#[test]
fn branches_rows_detached_clears_active_flag() {
    let mut s = seeded_store();
    let tip = s.head_commit().unwrap();
    s.create_branch("feature").unwrap();
    s.open_detached(tip);
    let rows = branches_rows(&s);
    assert!(
        rows.iter().all(|r| !r.branch),
        "no active branch while detached"
    );
}

#[test]
fn branches_rows_branch_without_commits_renders_empty_tip() {
    let s = VcStore::new("main");
    let rows = branches_rows(&s);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "main");
    assert_eq!(rows[0].hash, "");
    assert_eq!(rows[0].latest_commit_message, "");
    assert!(rows[0].branch);
}

#[test]
fn tags_rows_list_tags_with_hash_and_empty_message() {
    let mut s = seeded_store();
    let tip = s.head_commit().unwrap();
    s.create_tag("v1", tip).unwrap();
    s.create_tag("v2", tip).unwrap();
    let rows = tags_rows(&s);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].tag_name, "v1");
    assert_eq!(rows[1].tag_name, "v2");
    assert_eq!(rows[0].tag_hash, tip.to_hex());
    assert_eq!(rows[0].message, "");
    s.delete_tag("v1").unwrap();
    assert_eq!(tags_rows(&s).len(), 1);
}

#[test]
fn tags_rows_empty_before_any_tag() {
    let s = seeded_store();
    assert!(tags_rows(&s).is_empty());
}
