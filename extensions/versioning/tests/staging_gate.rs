//! Staging gate integration coverage: commit gates, reset modes, branch
//! advance, detached guard (S1–S4, R4).

use turso_versioning::model::{CommitId, VersionError};
use turso_versioning::session::SessionBranch;
use turso_versioning::staging::{TableState, VcStore};

fn configured_store() -> VcStore {
    let mut s = VcStore::new("main");
    s.config_set("user.name", "Ada");
    s.config_set("user.email", "ada@example.com");
    s.track_table("t1");
    s.dolt_add(&["t1"]).unwrap();
    s.set_now(1);
    s.dolt_commit("first", None, false, false).unwrap();
    s
}

/// Divergent branches so a merge records a real conflict.
fn diverged_store() -> VcStore {
    use turso_versioning::vtab_log::{VcRow, VcValue};
    let mut s = configured_store();
    s.apply_work(
        "t1",
        vec!["id".to_string(), "v".to_string()],
        vec!["id".to_string()],
        vec![
            VcRow::new(vec![VcValue::Integer(1), VcValue::Text("base".into())]),
            VcRow::new(vec![VcValue::Integer(2), VcValue::Text("keep".into())]),
        ],
        String::new(),
    );
    s.dolt_add(&["t1"]).unwrap();
    s.set_now(2);
    s.dolt_commit("seed", None, false, false).unwrap();
    s.create_branch("feature").unwrap();
    s.checkout("feature").unwrap();
    s.apply_work(
        "t1",
        vec!["id".to_string(), "v".to_string()],
        vec!["id".to_string()],
        vec![
            VcRow::new(vec![VcValue::Integer(1), VcValue::Text("theirs".into())]),
            VcRow::new(vec![VcValue::Integer(2), VcValue::Text("keep".into())]),
        ],
        String::new(),
    );
    s.dolt_add(&["t1"]).unwrap();
    s.set_now(3);
    s.dolt_commit("feature work", None, false, false).unwrap();
    s.checkout("main").unwrap();
    s.apply_work(
        "t1",
        vec!["id".to_string(), "v".to_string()],
        vec!["id".to_string()],
        vec![
            VcRow::new(vec![VcValue::Integer(1), VcValue::Text("ours".into())]),
            VcRow::new(vec![VcValue::Integer(2), VcValue::Text("keep".into())]),
        ],
        String::new(),
    );
    s.dolt_add(&["t1"]).unwrap();
    s.set_now(4);
    s.dolt_commit("main work", None, false, false).unwrap();
    s
}

#[test]
fn commit_refused_with_unresolved_conflicts() {
    let mut s = diverged_store();
    let result = s.merge_branch("feature", false, false, None).unwrap();
    assert!(matches!(
        result,
        turso_versioning::replay::MergeResult::Conflicts { .. }
    ));
    // Conflicts refuse even with force.
    let err = s.dolt_commit("second", None, false, true).unwrap_err();
    assert_eq!(err, VersionError::Conflicts);
    assert_eq!(err.to_string(), "cannot commit: unresolved merge conflicts");
}

#[test]
fn commit_refused_with_constraint_violations() {
    let mut s = configured_store();
    // Duplicate pk in the working set records a real unique violation.
    s.apply_work(
        "t1",
        vec!["id".to_string(), "v".to_string()],
        vec!["id".to_string()],
        vec![
            turso_versioning::vtab_log::VcRow::new(vec![
                turso_versioning::vtab_log::VcValue::Integer(1),
                turso_versioning::vtab_log::VcValue::Text("a".into()),
            ]),
            turso_versioning::vtab_log::VcRow::new(vec![
                turso_versioning::vtab_log::VcValue::Integer(1),
                turso_versioning::vtab_log::VcValue::Text("b".into()),
            ]),
        ],
        "CREATE TABLE t1 (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
    );
    s.verify_constraints().unwrap();
    s.dolt_add(&["t1"]).unwrap();
    s.set_now(2);
    let err = s.dolt_commit("second", None, false, false).unwrap_err();
    assert_eq!(err, VersionError::Violations);
    assert_eq!(
        err.to_string(),
        "cannot commit: constraint violations remain"
    );
}

#[test]
fn commit_force_bypasses_violations() {
    let mut s = configured_store();
    s.apply_work(
        "t1",
        vec!["id".to_string(), "v".to_string()],
        vec!["id".to_string()],
        vec![
            turso_versioning::vtab_log::VcRow::new(vec![
                turso_versioning::vtab_log::VcValue::Integer(1),
                turso_versioning::vtab_log::VcValue::Text("a".into()),
            ]),
            turso_versioning::vtab_log::VcRow::new(vec![
                turso_versioning::vtab_log::VcValue::Integer(1),
                turso_versioning::vtab_log::VcValue::Text("b".into()),
            ]),
        ],
        "CREATE TABLE t1 (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
    );
    s.verify_constraints().unwrap();
    s.dolt_add(&["t1"]).unwrap();
    s.set_now(2);
    let id = s.dolt_commit("second", None, false, true).unwrap();
    assert_eq!(s.head_commit(), Some(id));
}

#[test]
fn commit_refused_when_nothing_staged() {
    let mut s = VcStore::new("main");
    s.config_set("user.name", "Ada");
    s.config_set("user.email", "ada@example.com");
    let err = s.dolt_commit("empty", None, false, false).unwrap_err();
    assert_eq!(err, VersionError::NothingToCommit);
    assert_eq!(err.to_string(), "nothing to commit");
}

#[test]
fn commit_requires_author() {
    let mut s = VcStore::new("main");
    s.track_table("t1");
    s.dolt_add(&["t1"]).unwrap();
    s.set_now(1);
    let err = s.dolt_commit("no author", None, false, false).unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid author: user.name and user.email must be set"
    );
}

#[test]
fn commit_author_override_bypasses_config() {
    let mut s = VcStore::new("main");
    s.track_table("t1");
    s.dolt_add(&["t1"]).unwrap();
    s.set_now(1);
    let id = s
        .dolt_commit(
            "override",
            Some(("Grace", "grace@example.com")),
            false,
            false,
        )
        .unwrap();
    assert_eq!(s.head_commit(), Some(id));
}

#[test]
fn happy_path_commit_advances_branch_and_clears_staging() {
    let mut s = configured_store();
    let first = s.head_commit().unwrap();
    s.dolt_add(&["t1"]).unwrap();
    s.set_now(2);
    let second = s.dolt_commit("second", None, false, false).unwrap();
    assert_ne!(second, first);
    assert_eq!(s.head_commit(), Some(second));
    assert!(s.status().is_empty());
}

#[test]
fn reset_soft_keeps_working_changes() {
    let mut s = configured_store();
    s.dolt_add(&["t1"]).unwrap();
    s.reset_soft().unwrap();
    assert!(s.status().iter().any(|r| r.table == "t1" && !r.staged));
}

#[test]
fn reset_hard_discards_uncommitted() {
    let mut s = configured_store();
    s.dolt_add(&["t1"]).unwrap();
    s.reset_hard().unwrap();
    assert!(s.status().is_empty());
}

#[test]
fn clean_keeps_tracked_table_after_reset_soft() {
    let mut s = configured_store();
    s.dolt_add(&["t1"]).unwrap();
    s.reset_soft().unwrap();
    s.clean().unwrap();
    assert!(s.status().iter().any(|r| r.table == "t1" && !r.staged));
}

#[test]
fn table_not_found_on_add() {
    let mut s = VcStore::new("main");
    let err = s.dolt_add(&["ghost"]).unwrap_err();
    assert_eq!(err, VersionError::TableNotFound("ghost".to_string()));
    assert_eq!(err.to_string(), "table not found: ghost");
}

#[test]
fn track_table_counts_as_working_change() {
    let mut s = VcStore::new("main");
    s.track_table("t1");
    assert!(s.status().iter().any(|r| r.table == "t1" && !r.staged));
}

#[test]
fn add_all_stages_new_and_modified() {
    let mut s = VcStore::new("main");
    s.track_table("t1");
    s.track_table("t2");
    s.add_all().unwrap();
    assert!(s.status().iter().all(|r| r.staged));
}

#[test]
fn status_labels_new_and_modified() {
    let mut s = VcStore::new("main");
    s.track_table("brand_new");
    s.dolt_add(&["brand_new"]).unwrap();
    let rows = s.status();
    let row = rows.iter().find(|r| r.table == "brand_new").unwrap();
    assert_eq!(row.status.to_string(), "new table");
    s.set_table_state("brand_new", TableState::Modified);
    assert_eq!(s.status()[0].status.to_string(), "modified");
}

#[test]
fn detached_guard_blocks_writes() {
    let mut session = SessionBranch::new("main");
    session.open_detached(CommitId([0x11; 20]));
    assert_eq!(
        session.guard_write().unwrap_err().to_string(),
        "cannot write in detached HEAD state"
    );
    session.checkout("dev");
    assert!(session.guard_write().is_ok());
}

#[test]
fn detached_vcstore_hides_branch_and_blocks_writes() {
    let mut s = configured_store();
    let tip = s.head_commit().unwrap();
    s.open_detached(tip);
    assert!(s.active_branch().is_none());
    assert_eq!(
        s.dolt_add(&["t1"]).unwrap_err().to_string(),
        "cannot write in detached HEAD state"
    );
    assert_eq!(s.resolve("HEAD").unwrap(), tip);
    s.checkout("main").unwrap();
    assert_eq!(s.active_branch(), Some("main"));
    assert!(s.dolt_add(&["t1"]).is_ok());
}

#[test]
fn savepoint_matrix_preserves_staging() {
    let s = configured_store();
    assert!(s.preserve_staging_on("COMMIT"));
    assert!(s.preserve_staging_on("ROLLBACK"));
    assert!(s.preserve_staging_on("RELEASE"));
    assert!(s.savepoint_sealed("SAVEPOINT"));
    assert!(s.savepoint_sealed("ROLLBACK TO"));
    // RELEASE ends the savepoint, lifting the seal.
    assert!(!s.savepoint_sealed("RELEASE"));
    assert!(!s.savepoint_sealed("BEGIN"));
}
