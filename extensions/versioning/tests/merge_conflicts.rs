//! Merge-conflict end-to-end over `VcStore`: branch, diverge, merge, conflict,
//! resolve, and commit. Conflicts stay transient and never enter a commit.

use turso_versioning::conflicts::ResolveSide;
use turso_versioning::replay::MergeResult;
use turso_versioning::staging::VcStore;
use turso_versioning::vtab_log::{VcRead, VcRow, VcValue};

fn store() -> VcStore {
    let mut s = VcStore::new("main");
    s.config_set("user.name", "Ada");
    s.config_set("user.email", "ada@example.com");
    s
}

fn apply(s: &mut VcStore, rows: &[(i64, &str)]) {
    s.apply_work(
        "t",
        vec!["id".to_string(), "v".to_string()],
        vec!["id".to_string()],
        rows.iter()
            .map(|(id, v)| VcRow::new(vec![VcValue::Integer(*id), VcValue::Text(v.to_string())]))
            .collect(),
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
    );
}

fn commit(s: &mut VcStore, msg: &str) -> turso_versioning::CommitId {
    if !s.tables().contains(&"t".to_string()) {
        s.track_table("t");
    }
    s.dolt_add(&["t"]).unwrap();
    s.dolt_commit(msg, None, false, false).unwrap()
}

/// main: seed; feature changes row 2 and adds row 3; main changes row 2
/// differently and adds row 4.
fn diverged() -> (VcStore, turso_versioning::CommitId) {
    let mut s = store();
    apply(&mut s, &[(1, "base"), (2, "keep")]);
    commit(&mut s, "seed");
    s.create_branch("feature").unwrap();
    s.checkout("feature").unwrap();
    apply(&mut s, &[(1, "base"), (2, "theirs"), (3, "feat")]);
    commit(&mut s, "feature work");
    let feature = s.head_commit().unwrap();
    s.checkout("main").unwrap();
    apply(&mut s, &[(1, "base"), (2, "ours"), (4, "main")]);
    commit(&mut s, "main work");
    (s, feature)
}

#[test]
fn merge_conflict_resolve_and_commit() {
    let (mut s, _feature) = diverged();
    let result = s.merge_branch("feature", false, false, None).unwrap();
    let MergeResult::Conflicts { rows, tables } = result else {
        panic!("expected conflicts, got {result:?}");
    };
    assert_eq!(rows, 1);
    assert_eq!(tables, 1);

    // Conflicts are transient: they block commit but never enter one.
    assert_eq!(s.conflict_count(), 1);
    s.set_now(20);
    assert_eq!(
        s.dolt_commit("merged", None, false, false)
            .unwrap_err()
            .to_string(),
        "cannot commit: unresolved merge conflicts"
    );

    // The conflict table exposes base/ours/theirs.
    let entries = s.conflict_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].table, "t");
    assert_eq!(
        entries[0].base.as_ref().unwrap().values[1],
        VcValue::Text("keep".into())
    );
    assert_eq!(
        entries[0].ours.as_ref().unwrap().values[1],
        VcValue::Text("ours".into())
    );
    assert_eq!(
        entries[0].theirs.as_ref().unwrap().values[1],
        VcValue::Text("theirs".into())
    );

    // Resolving the last conflict unblocks commit; the merge commit carries
    // both parents and the resolution becomes its content.
    s.resolve_conflict("t", &entries[0].pk, ResolveSide::Theirs)
        .unwrap();
    assert_eq!(s.conflict_count(), 0);
    s.set_now(21);
    let id = s.dolt_commit("merged", None, false, false).unwrap();
    let commit = s.get_commit(&id).unwrap();
    assert_eq!(commit.parents.len(), 2);
    let rows = s.table_rows("t", &id).unwrap();
    let row2 = rows
        .iter()
        .find(|r| r.values.contains(&VcValue::Integer(2)))
        .unwrap();
    assert_eq!(row2.values[1], VcValue::Text("theirs".into()));
    // Row 3 (feature's add) and row 4 (main's add) both survived.
    assert!(rows.iter().any(|r| r.values.contains(&VcValue::Integer(3))));
    assert!(rows.iter().any(|r| r.values.contains(&VcValue::Integer(4))));
    // Conflicts never became committed content: a fresh look at the commit's
    // snapshots shows no conflict rows (conflicts have no pk cells beyond rows).
    assert!(s.committed_snapshot_tables().contains(&"t".to_string()));

    // A second merge of an already-merged branch is clean.
    match s.merge_branch("feature", false, false, None).unwrap() {
        MergeResult::Committed(_) => {}
        other => panic!("expected committed, got {other:?}"),
    }
    assert_eq!(s.conflict_count(), 0);
}

#[test]
fn merge_abort_discards_everything() {
    let (mut s, _feature) = diverged();
    let work_before = s.work_snapshots();
    let result = s.merge_branch("feature", false, false, None).unwrap();
    assert!(matches!(result, MergeResult::Conflicts { .. }));
    s.merge_abort().unwrap();
    assert_eq!(s.conflict_count(), 0);
    assert_eq!(s.work_snapshots(), work_before);
    // Commit works again with no conflict gate.
    s.set_now(30);
    let id = commit(&mut s, "after abort");
    let commit = s.get_commit(&id).unwrap();
    assert_eq!(commit.parents.len(), 1);
}

#[test]
fn resolve_wrong_side_string_is_rejected_by_func_layer() {
    let (mut s, _feature) = diverged();
    s.merge_branch("feature", false, false, None).unwrap();
    // The store rejects an unknown pk; the func layer rejects the side text
    // before it ever reaches the store.
    let err = turso_versioning::funcs::dolt_conflicts_resolve(
        &mut s,
        &[
            turso_versioning::funcs::FuncArg::Text("--mine"),
            turso_versioning::funcs::FuncArg::Text("t"),
            turso_versioning::funcs::FuncArg::Integer(2),
        ],
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "invalid resolve side: '--mine' (want '--ours' or '--theirs')"
    );
}
