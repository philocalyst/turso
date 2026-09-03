//! Cherry-pick, revert, and rebase over `VcStore`, with history assertions
//! through `MemVcRead` for the log surface.

use turso_versioning::commit::ancestors;
use turso_versioning::conflicts::ResolveSide;
use turso_versioning::replay::{MergeResult, RebaseStep};
use turso_versioning::staging::VcStore;
use turso_versioning::vtab_log::{VcRead, VcRow, VcValue};
use turso_versioning::CommitId;

fn store() -> VcStore {
    let mut s = VcStore::new("main");
    s.config_set("user.name", "Ada");
    s.config_set("user.email", "ada@example.com");
    s
}

/// The working content as the map `execute_rebase` restores on `--abort`.
fn work_map(
    s: &VcStore,
) -> std::collections::HashMap<String, turso_versioning::staging::TableSnapshot> {
    s.work_snapshots().into_iter().collect()
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

fn commit(s: &mut VcStore, msg: &str) -> CommitId {
    if !s.tables().contains(&"t".to_string()) {
        s.track_table("t");
    }
    s.dolt_add(&["t"]).unwrap();
    s.dolt_commit(msg, None, false, false).unwrap()
}

/// main: seed then main work; feature: feat work. Disjoint row edits so a
/// cherry-pick and a rebase both stay clean.
fn clean_history() -> (VcStore, CommitId, CommitId) {
    let mut s = store();
    apply(&mut s, &[(1, "a"), (2, "b")]);
    commit(&mut s, "seed");
    s.create_branch("feature").unwrap();
    s.checkout("feature").unwrap();
    apply(&mut s, &[(1, "a"), (2, "b"), (3, "feat")]);
    let feature = commit(&mut s, "feature work");
    s.checkout("main").unwrap();
    apply(&mut s, &[(1, "a"), (2, "b"), (4, "main")]);
    let main = commit(&mut s, "main work");
    (s, feature, main)
}

#[test]
fn cherry_pick_single_commit_replays_content() {
    let (mut s, feature, _main) = clean_history();
    match s.cherry_pick(&feature.to_hex(), None).unwrap() {
        MergeResult::Committed(id) => {
            assert_eq!(s.get_commit(&id).unwrap().parents.len(), 1);
            let rows = s.table_rows("t", &id).unwrap();
            assert!(rows.iter().any(|r| r.values.contains(&VcValue::Integer(3))));
        }
        other => panic!("expected committed, got {other:?}"),
    }
}

#[test]
fn cherry_pick_initial_commit_is_whole_tree_add() {
    let mut s = store();
    // Feature branch holds the only content; its initial commit has no parent.
    apply(&mut s, &[(1, "a")]);
    commit(&mut s, "seed");
    s.create_branch("feature").unwrap();
    s.checkout("feature").unwrap();
    apply(&mut s, &[(1, "a"), (2, "feat")]);
    let initial = commit(&mut s, "feat initial");
    s.checkout("main").unwrap();
    match s.cherry_pick(&initial.to_hex(), None).unwrap() {
        MergeResult::Committed(id) => {
            let rows = s.table_rows("t", &id).unwrap();
            assert!(rows.iter().any(|r| r.values.contains(&VcValue::Integer(2))));
        }
        other => panic!("expected committed, got {other:?}"),
    }
}

#[test]
fn revert_undoes_a_commit() {
    let (mut s, _feature, main) = clean_history();
    match s.revert(&main.to_hex(), None).unwrap() {
        MergeResult::Committed(id) => {
            assert_eq!(
                s.get_commit(&id).unwrap().meta.message,
                "Revert \"main work\""
            );
            // Row 4 (added by main work) is gone after the revert.
            let rows = s.table_rows("t", &id).unwrap();
            assert!(rows
                .iter()
                .all(|r| !r.values.contains(&VcValue::Integer(4))));
            assert!(rows.iter().any(|r| r.values.contains(&VcValue::Integer(1))));
        }
        other => panic!("expected committed, got {other:?}"),
    }
}

#[test]
fn revert_merge_commit_needs_parent_selector() {
    let mut s = store();
    apply(&mut s, &[(1, "a")]);
    commit(&mut s, "seed");
    s.create_branch("other").unwrap();
    s.checkout("other").unwrap();
    apply(&mut s, &[(1, "b")]);
    commit(&mut s, "other work");
    s.checkout("main").unwrap();
    s.merge_branch("other", false, false, None).unwrap();
    let merge_tip = s.head_commit().unwrap();
    assert_eq!(s.get_commit(&merge_tip).unwrap().parents.len(), 2);
    assert_eq!(
        s.revert(&merge_tip.to_hex(), None).unwrap_err().to_string(),
        "cherry-pick of merge commit needs -m PARENT (got 2 parents)"
    );
    // With -m 1 the revert applies against the first parent.
    assert!(matches!(
        s.revert(&merge_tip.to_hex(), Some(0)).unwrap(),
        MergeResult::Committed(_)
    ));
}

#[test]
fn rebase_replays_commits_onto_new_base() {
    let (mut s, _feature, _main) = clean_history();
    s.checkout("feature").unwrap();
    let head_before = s.head_commit().unwrap();
    let new_head = s.rebase_onto("main").unwrap();
    assert_ne!(new_head, head_before);
    // The replayed commit keeps its message and sits on the new base.
    let commit = s.get_commit(&new_head).unwrap();
    assert_eq!(commit.meta.message, "feature work");
    let chain = ancestors(s.commit_store(), new_head).unwrap();
    assert!(chain.contains(&new_head));
    // History walks the replayed chain from the store directly.
    let rows = turso_versioning::vtab_log::log_rows(&s, None).unwrap();
    assert!(rows.iter().any(|r| r.message == "feature work"));
}

#[test]
fn rebase_drop_and_squash_plan() {
    let mut s = store();
    apply(&mut s, &[(1, "a")]);
    commit(&mut s, "seed");
    s.create_branch("feature").unwrap();
    s.checkout("feature").unwrap();
    apply(&mut s, &[(1, "b")]);
    let c1 = commit(&mut s, "first");
    apply(&mut s, &[(1, "c")]);
    let c2 = commit(&mut s, "second");
    s.checkout("main").unwrap();
    let main_tip = s.head_commit().unwrap();
    s.checkout("feature").unwrap();
    let head0 = s.head_commit().unwrap();
    // Drop c1 and squash c2: one commit, message "second".
    let steps = vec![RebaseStep::Drop(c1), RebaseStep::Squash(c2)];
    let new_head = s
        .execute_rebase(steps, main_tip, head0, None, work_map(&s))
        .unwrap();
    let commit = s.get_commit(&new_head).unwrap();
    assert_eq!(commit.meta.message, "second");
    assert_eq!(commit.parents, vec![main_tip]);
    let rows = s.table_rows("t", &new_head).unwrap();
    assert_eq!(rows[0].values[1], VcValue::Text("c".into()));
}

#[test]
fn rebase_fixup_discards_message() {
    let mut s = store();
    apply(&mut s, &[(1, "a")]);
    commit(&mut s, "seed");
    s.create_branch("feature").unwrap();
    s.checkout("feature").unwrap();
    apply(&mut s, &[(1, "b")]);
    let c1 = commit(&mut s, "keep me");
    apply(&mut s, &[(1, "c")]);
    let c2 = commit(&mut s, "discard me");
    s.checkout("main").unwrap();
    let main_tip = s.head_commit().unwrap();
    s.checkout("feature").unwrap();
    let head0 = s.head_commit().unwrap();
    let steps = vec![RebaseStep::Pick(c1), RebaseStep::Fixup(c2)];
    let new_head = s
        .execute_rebase(steps, main_tip, head0, None, work_map(&s))
        .unwrap();
    let commit = s.get_commit(&new_head).unwrap();
    assert_eq!(commit.meta.message, "keep me");
}

#[test]
fn merge_no_commit_then_single_parent_squash_commit() {
    let (mut s, _feature, _main) = clean_history();
    // --squash merges with a single parent and no merge commit.
    match s.merge_branch("feature", true, false, None).unwrap() {
        MergeResult::Committed(id) => {
            let commit = s.get_commit(&id).unwrap();
            assert_eq!(commit.parents.len(), 1);
        }
        other => panic!("expected committed, got {other:?}"),
    }
    // --no-commit leaves the merge open for a manual commit.
    s.checkout("main").unwrap();
    s.create_branch("other").unwrap();
    s.checkout("other").unwrap();
    apply(&mut s, &[(1, "a"), (2, "b"), (5, "other")]);
    commit(&mut s, "other work");
    s.checkout("main").unwrap();
    assert_eq!(
        s.merge_branch("other", false, true, None).unwrap(),
        MergeResult::Applied
    );
    s.set_now(60);
    let id = s.dolt_commit("finish merge", None, false, false).unwrap();
    assert_eq!(s.get_commit(&id).unwrap().parents.len(), 2);
}

#[test]
fn conflict_rows_resolve_into_history() {
    let mut s = store();
    apply(&mut s, &[(1, "base")]);
    commit(&mut s, "seed");
    s.create_branch("feature").unwrap();
    s.checkout("feature").unwrap();
    apply(&mut s, &[(1, "theirs")]);
    commit(&mut s, "feature work");
    s.checkout("main").unwrap();
    apply(&mut s, &[(1, "ours")]);
    commit(&mut s, "main work");
    assert!(matches!(
        s.merge_branch("feature", false, false, None).unwrap(),
        MergeResult::Conflicts { .. }
    ));
    let entry = s.conflict_entries()[0].clone();
    s.resolve_conflict("t", &entry.pk, ResolveSide::Theirs)
        .unwrap();
    s.set_now(70);
    let id = s.dolt_commit("merged", None, false, false).unwrap();
    let rows = s.table_rows("t", &id).unwrap();
    assert_eq!(rows[0].values[1], VcValue::Text("theirs".into()));
}
