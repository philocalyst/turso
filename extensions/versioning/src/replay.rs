//! Cherry-pick, revert, rebase, and the merge drivers (rubric §5).
//!
//! Every driver is a three-way merge over the store's committed snapshots.
//! Merge/cherry-pick/revert share one snapshot-merge core; rebase replays a
//! plan of those same merges onto a new base and commits each clean step.

use std::collections::{HashMap, HashSet};

use crate::commit::{ancestors, merge_base, Commit, CommitMeta, CommitStore};
use crate::conflicts::{ConflictEntry, ConflictKind};
use crate::merge::three_way_row_merge;
use crate::merge_schema::{merge_schema, parse_schema, ColIR, SchemaDecision, SchemaIR};
use crate::model::{CommitId, RootHash, VersionError, VersionResult};
use crate::refs::RefName;
use crate::staging::{TableSnapshot, VcStore};
use crate::vtab_log::{VcRow, VcValue};

/// Result of one merge/cherry-pick/revert/rebase driver call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    /// A commit was created; carries its id.
    Committed(CommitId),
    /// Conflicts were recorded; counts feed the report string.
    Conflicts { rows: usize, tables: usize },
    /// A schema conflict blocked the data merge; the table and detail feed the
    /// `schema conflict in table: NAME (DETAIL)` report.
    SchemaConflict { table: String, detail: String },
    /// The working set was touched without a commit (`--no-commit`, `--abort`).
    Applied,
}

/// One table's merge status for `dolt_merge_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeStatusRow {
    pub table: String,
    pub rows_merged: i64,
    pub rows_conflicted: i64,
    pub schema_conflict: i64,
    pub state: String,
}

/// Per-table counts a merge recorded, for the status vtable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MergeTableStats {
    pub rows_merged: i64,
    pub rows_conflicted: i64,
    pub schema_conflict: bool,
}

/// State of an open merge: the side being merged, the pre-merge working set
/// for `--abort`, and the per-table stats for `dolt_merge_status`.
#[derive(Debug, Clone)]
pub struct MergeState {
    pub(crate) theirs: CommitId,
    pub(crate) work_before: HashMap<String, TableSnapshot>,
    pub(crate) stats: HashMap<String, MergeTableStats>,
}

impl MergeState {
    /// Status rows in table order, all stamped with the state label.
    pub(crate) fn status_rows(&self, state: &str) -> Vec<MergeStatusRow> {
        let mut tables: Vec<String> = self.stats.keys().cloned().collect();
        tables.sort();
        tables
            .into_iter()
            .map(|table| {
                let s = &self.stats[&table];
                MergeStatusRow {
                    table,
                    rows_merged: s.rows_merged,
                    rows_conflicted: s.rows_conflicted,
                    schema_conflict: s.schema_conflict as i64,
                    state: state.to_string(),
                }
            })
            .collect()
    }
}

/// One step of a rebase plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseStep {
    Pick(CommitId),
    Drop(CommitId),
    Reword(CommitId, String),
    Squash(CommitId),
    Fixup(CommitId),
}

/// State of a rebase paused on a conflict, so `--continue` and `--abort` can
/// resume or abandon it. The rebase working set holds the replay accumulated
/// so far; `--continue` commits the resolved rebase as the replayed version
/// of the conflicting commit, then replays the remaining steps. `work_before`
/// is the working set from before the rebase started, restored by `--abort`.
#[derive(Debug, Clone)]
pub struct RebaseState {
    pub(crate) original_head: CommitId,
    pub(crate) onto: CommitId,
    pub(crate) steps: Vec<RebaseStep>,
    pub(crate) replay_head: CommitId,
    pub(crate) rebase_work: HashMap<String, TableSnapshot>,
    pub(crate) combined_message: Option<String>,
    pub(crate) conflicted_hash: String,
    pub(crate) work_before: HashMap<String, TableSnapshot>,
}

impl VcStore {
    /// Merge a branch into the current HEAD. Clean merges create a commit
    /// (two parents, or one for `--squash`); `--no-commit` stops at the
    /// working set; conflicts record entries and refuse to move HEAD. An
    /// explicit `-m N` parent on a merge commit picks that parent's snapshot
    /// as the merge base instead of the computed LCA.
    pub fn merge_branch(
        &mut self,
        theirs: &str,
        squash: bool,
        no_commit: bool,
        parent: Option<usize>,
    ) -> VersionResult<MergeResult> {
        self.guard_write()?;
        self.guard_no_replay_in_progress()?;
        let ours = self.head_commit().ok_or(VersionError::NothingToCommit)?;
        let theirs_id = self.resolve(theirs)?;
        let work_before = self.work.clone();
        let theirs_commit = self.commits.get_commit(&theirs_id);
        let base = if let (Some(commit), Some(p)) = (theirs_commit, parent) {
            if p >= commit.parents.len() {
                return Err(VersionError::CherryPickMergeNeedsParent(
                    commit.parents.len(),
                ));
            }
            Some(commit.parents[p])
        } else {
            merge_base(&self.commits, ours, theirs_id)?
        };
        let base_snap = base
            .and_then(|b| self.snapshots.get(&b).cloned())
            .unwrap_or_default();
        let ours_snap = self.snapshots.get(&ours).cloned().unwrap_or_default();
        let theirs_snap = self.snapshots.get(&theirs_id).cloned().unwrap_or_default();
        let merged = merge_snapshots(&base_snap, &ours_snap, &theirs_snap)?;
        if let Some((table, detail)) = merged.schema_conflict() {
            self.record_merge_state(merged, theirs_id, work_before);
            return Ok(MergeResult::SchemaConflict { table, detail });
        }
        if !merged.conflicts.is_empty() {
            let rows = merged.conflicts.len();
            let tables = distinct_conflicted_tables(&merged.conflicts);
            self.record_merge_state(merged, theirs_id, work_before);
            return Ok(MergeResult::Conflicts { rows, tables });
        }
        if no_commit {
            self.work = merged.work;
            let changed: Vec<String> = self.work.keys().cloned().collect();
            self.stage_names(&changed);
            self.mark_dirty(&changed);
            self.merge_state = Some(MergeState {
                theirs: theirs_id,
                work_before,
                stats: merged.stats,
            });
            return Ok(MergeResult::Applied);
        }
        let msg = format!("Merge branch '{theirs}' into '{}'", self.head_branch());
        let parents = if squash {
            vec![ours]
        } else {
            vec![ours, theirs_id]
        };
        self.work = merged.work;
        let changed: Vec<String> = self.work.keys().cloned().collect();
        self.mark_dirty(&changed);
        let id = self.create_commit_with_work(parents, &msg)?;
        self.merge_state = None;
        Ok(MergeResult::Committed(id))
    }

    /// Refuse starting a second replay while a merge, rebase, or conflict set
    /// is already active: overlapping drivers would clobber each other's
    /// working set and conflict state.
    fn guard_no_replay_in_progress(&self) -> VersionResult<()> {
        if self.merge_state.is_some() {
            return Err(VersionError::MergeInProgress);
        }
        if self.rebase_state.is_some() {
            return Err(VersionError::RebaseInProgress);
        }
        if !self.conflicts.is_empty() {
            return Err(VersionError::MergeInProgress);
        }
        Ok(())
    }

    /// Record a conflicted merge's working set, conflict entries, and merge
    /// state (shared by the row-conflict and schema-conflict paths).
    fn record_merge_state(
        &mut self,
        merged: MergedTables,
        theirs: CommitId,
        work_before: HashMap<String, TableSnapshot>,
    ) {
        self.conflicts.extend(merged.conflicts);
        self.work = merged.work;
        let changed: Vec<String> = self.work.keys().cloned().collect();
        self.stage_names(&changed);
        self.mark_dirty(&changed);
        self.merge_state = Some(MergeState {
            theirs,
            work_before,
            stats: merged.stats,
        });
    }

    /// Abort the open merge: restore the pre-merge HEAD and working set and
    /// drop the recorded conflicts.
    pub fn merge_abort(&mut self) -> VersionResult<()> {
        self.guard_write()?;
        let Some(ms) = self.merge_state.take() else {
            return Err(VersionError::NoMergeInProgress);
        };
        self.work = ms.work_before;
        let restored: Vec<String> = self.work.keys().cloned().collect();
        self.mark_dirty(&restored);
        self.conflicts.clear();
        self.clear_staging();
        Ok(())
    }

    /// Status rows for `dolt_merge_status`; empty when no merge is open.
    pub fn merge_status_rows(&self) -> Vec<MergeStatusRow> {
        match &self.merge_state {
            Some(ms) => {
                let state = if self.conflicts.is_empty() {
                    "clean"
                } else {
                    "conflicted"
                };
                ms.status_rows(state)
            }
            None => Vec::new(),
        }
    }

    /// Replay one commit onto the current HEAD (first-parent base, or a
    /// chosen parent for merges). An initial commit replays as a whole-tree
    /// add. Returns the new commit, or the conflict report on failure.
    pub fn cherry_pick(&mut self, spec: &str, parent: Option<usize>) -> VersionResult<MergeResult> {
        self.guard_write()?;
        self.guard_no_replay_in_progress()?;
        let commit_id = self.resolve(spec)?;
        let commit = self
            .commits
            .get_commit(&commit_id)
            .ok_or_else(|| VersionError::CommitNotFound(commit_id.to_hex()))?;
        if commit.parents.len() > 1 && parent.is_none() {
            return Err(VersionError::CherryPickMergeNeedsParent(
                commit.parents.len(),
            ));
        }
        if parent.is_some_and(|p| p >= commit.parents.len()) {
            return Err(VersionError::CherryPickMergeNeedsParent(
                commit.parents.len(),
            ));
        }
        let base = if commit.parents.is_empty() {
            None
        } else {
            Some(commit.parents[parent.unwrap_or(0)])
        };
        let ours = self.head_commit().ok_or(VersionError::NothingToCommit)?;
        let base_snap = base
            .and_then(|b| self.snapshots.get(&b).cloned())
            .unwrap_or_default();
        let ours_snap = self.snapshots.get(&ours).cloned().unwrap_or_default();
        let theirs_snap = self.snapshots.get(&commit_id).cloned().unwrap_or_default();
        let merged = merge_snapshots(&base_snap, &ours_snap, &theirs_snap)?;
        if let Some((table, detail)) = merged.schema_conflict() {
            self.work = merged.work;
            let changed: Vec<String> = self.work.keys().cloned().collect();
            self.stage_names(&changed);
            self.mark_dirty(&changed);
            self.conflicts.extend(merged.conflicts);
            return Ok(MergeResult::SchemaConflict { table, detail });
        }
        if !merged.conflicts.is_empty() {
            let rows = merged.conflicts.len();
            let tables = distinct_conflicted_tables(&merged.conflicts);
            self.conflicts.extend(merged.conflicts);
            self.work = merged.work;
            let changed: Vec<String> = self.work.keys().cloned().collect();
            self.stage_names(&changed);
            self.mark_dirty(&changed);
            return Ok(MergeResult::Conflicts { rows, tables });
        }
        self.work = merged.work;
        let changed: Vec<String> = self.work.keys().cloned().collect();
        self.mark_dirty(&changed);
        let id = self.create_commit_with_work(vec![ours], &commit.meta.message)?;
        Ok(MergeResult::Committed(id))
    }

    /// Replay the inverse of a commit: the base is the commit itself and the
    /// other side is its first parent, so the merged working set undoes it.
    /// Reverting an initial commit empties its tables. A merge commit needs
    /// `-m`, same as cherry-pick.
    pub fn revert(&mut self, spec: &str, parent: Option<usize>) -> VersionResult<MergeResult> {
        self.guard_write()?;
        self.guard_no_replay_in_progress()?;
        let commit_id = self.resolve(spec)?;
        let commit = self
            .commits
            .get_commit(&commit_id)
            .ok_or_else(|| VersionError::CommitNotFound(commit_id.to_hex()))?;
        if commit.parents.len() > 1 && parent.is_none() {
            return Err(VersionError::CherryPickMergeNeedsParent(
                commit.parents.len(),
            ));
        }
        if parent.is_some_and(|p| p >= commit.parents.len()) {
            return Err(VersionError::CherryPickMergeNeedsParent(
                commit.parents.len(),
            ));
        }
        let theirs = if commit.parents.is_empty() {
            None
        } else {
            Some(commit.parents[parent.unwrap_or(0)])
        };
        let ours = self.head_commit().ok_or(VersionError::NothingToCommit)?;
        let base_snap = self.snapshots.get(&commit_id).cloned().unwrap_or_default();
        let ours_snap = self.snapshots.get(&ours).cloned().unwrap_or_default();
        let theirs_snap = theirs
            .and_then(|t| self.snapshots.get(&t).cloned())
            .unwrap_or_default();
        let merged = merge_snapshots(&base_snap, &ours_snap, &theirs_snap)?;
        if let Some((table, detail)) = merged.schema_conflict() {
            self.work = merged.work;
            let changed: Vec<String> = self.work.keys().cloned().collect();
            self.stage_names(&changed);
            self.mark_dirty(&changed);
            self.conflicts.extend(merged.conflicts);
            return Ok(MergeResult::SchemaConflict { table, detail });
        }
        if !merged.conflicts.is_empty() {
            let rows = merged.conflicts.len();
            let tables = distinct_conflicted_tables(&merged.conflicts);
            self.conflicts.extend(merged.conflicts);
            self.work = merged.work;
            let changed: Vec<String> = self.work.keys().cloned().collect();
            self.stage_names(&changed);
            self.mark_dirty(&changed);
            return Ok(MergeResult::Conflicts { rows, tables });
        }
        let msg = format!("Revert \"{}\"", commit.meta.message);
        self.work = merged.work;
        let changed: Vec<String> = self.work.keys().cloned().collect();
        self.mark_dirty(&changed);
        let id = self.create_commit_with_work(vec![ours], &msg)?;
        Ok(MergeResult::Committed(id))
    }

    /// Start a rebase onto `onto`: the plan is one `pick` per commit on the
    /// current branch's first-parent chain that is not already on `onto`,
    /// oldest first. Replaying conflicts abort the whole rebase atomically.
    pub fn rebase_onto(&mut self, onto_spec: &str) -> VersionResult<CommitId> {
        self.guard_write()?;
        self.guard_no_replay_in_progress()?;
        let onto = self.resolve(onto_spec)?;
        let head = self.head_commit().ok_or(VersionError::NothingToCommit)?;
        let steps = self.build_plan(head, onto)?;
        let work_before = self.work.clone();
        let final_head = self.execute_rebase(steps, onto, head, None, work_before)?;
        self.rebase_state = None;
        Ok(final_head)
    }

    /// Build the rebase plan onto `onto` and pause before replaying any of it,
    /// so the caller can edit the plan (drop/reword/squash/fixup) and then
    /// `--continue` applies the edited version. Returns the plan's step count.
    pub fn rebase_plan(&mut self, onto_spec: &str) -> VersionResult<usize> {
        self.guard_write()?;
        self.guard_no_replay_in_progress()?;
        let onto = self.resolve(onto_spec)?;
        let head = self.head_commit().ok_or(VersionError::NothingToCommit)?;
        let steps = self.build_plan(head, onto)?;
        let count = steps.len();
        self.rebase_state = Some(RebaseState {
            original_head: head,
            onto,
            steps,
            replay_head: onto,
            rebase_work: HashMap::new(),
            combined_message: None,
            conflicted_hash: String::new(),
            work_before: self.work.clone(),
        });
        Ok(count)
    }

    /// Replace the paused rebase's plan with `steps`, so `--continue` replays
    /// the edited plan (dropped steps never run).
    pub fn rebase_edit_plan(&mut self, steps: Vec<RebaseStep>) -> VersionResult<()> {
        self.guard_write()?;
        let Some(state) = self.rebase_state.as_mut() else {
            return Err(VersionError::NoRebaseInProgress);
        };
        state.steps = steps;
        Ok(())
    }

    /// Resume a paused rebase after the user resolved the conflicts. The
    /// resolved rebase working set commits as the replayed conflicting
    /// commit, then the remaining steps replay. A plan-paused rebase (no
    /// conflicting commit) replays the stored plan from the start.
    pub fn rebase_continue(&mut self) -> VersionResult<CommitId> {
        self.guard_write()?;
        let Some(st) = self.rebase_state.take() else {
            return Err(VersionError::NoRebaseInProgress);
        };
        if !self.conflicts.is_empty() {
            let hash = st.conflicted_hash.clone();
            self.rebase_state = Some(st);
            return Err(VersionError::RebaseConflict(hash));
        }
        let plan_paused = st.conflicted_hash.is_empty();
        let start_head = if plan_paused {
            st.replay_head
        } else {
            let msg = st
                .combined_message
                .clone()
                .unwrap_or_else(|| self.message_of(&st.conflicted_hash));
            self.work = st.rebase_work.clone();
            let new_head = self.put_replay_commit(vec![st.replay_head], &msg, &st.rebase_work)?;
            self.work = st.rebase_work;
            let changed: Vec<String> = self.work.keys().cloned().collect();
            self.mark_dirty(&changed);
            new_head
        };
        let final_head = self.execute_rebase(
            st.steps,
            st.onto,
            st.original_head,
            Some(start_head),
            st.work_before.clone(),
        )?;
        self.rebase_state = None;
        Ok(final_head)
    }

    /// Abandon a paused rebase: restore the pre-rebase HEAD and working set,
    /// drop the recorded conflicts and any resolution pending SQL application.
    pub fn rebase_abort(&mut self) -> VersionResult<()> {
        self.guard_write()?;
        let Some(st) = self.rebase_state.take() else {
            return Err(VersionError::NoRebaseInProgress);
        };
        self.refs
            .compare_and_swap(
                &RefName::branch(self.head_branch()),
                self.head_commit(),
                st.original_head,
            )
            .map_err(|_| VersionError::DatabaseLocked)?;
        self.work = st.work_before;
        let restored: Vec<String> = self.work.keys().cloned().collect();
        self.mark_dirty(&restored);
        self.conflicts.clear();
        self.pending_resolve.clear();
        self.clear_staging();
        Ok(())
    }

    /// The plan for `onto`: every first-parent commit of `head` that `onto`
    /// does not already contain, oldest first.
    fn build_plan(&self, head: CommitId, onto: CommitId) -> VersionResult<Vec<RebaseStep>> {
        let chain = ancestors(&self.commits, head)?;
        let onto_ancestors: HashSet<CommitId> =
            ancestors(&self.commits, onto)?.into_iter().collect();
        let mut picks: Vec<CommitId> = chain
            .into_iter()
            .filter(|c| !onto_ancestors.contains(c))
            .collect();
        picks.reverse();
        Ok(picks.into_iter().map(RebaseStep::Pick).collect())
    }

    /// Replay the steps onto `onto`, committing each clean group. Any conflict
    /// restores the pre-rebase HEAD and working set, records the conflict, and
    /// returns `rebase conflict at <hash>: resolve and --continue`. Public for
    /// the plan-table rebase form and for tests that drive custom plans.
    pub fn execute_rebase(
        &mut self,
        steps: Vec<RebaseStep>,
        onto: CommitId,
        original_head: CommitId,
        start_head: Option<CommitId>,
        work_before: HashMap<String, TableSnapshot>,
    ) -> VersionResult<CommitId> {
        let mut replay_head = start_head.unwrap_or(onto);
        let mut rebase_work: HashMap<String, TableSnapshot> = HashMap::new();
        let mut combined_message: Option<String> = None;
        // The previous member of a squash/fixup group: a folding step merges
        // against it so the group's diffs accumulate instead of each one being
        // judged against the group anchor's parent.
        let mut group_base: Option<CommitId> = None;
        let mut i = 0;
        while i < steps.len() {
            let step = &steps[i];
            match step {
                RebaseStep::Drop(_) => {
                    i += 1;
                    continue;
                }
                RebaseStep::Pick(c)
                | RebaseStep::Reword(c, _)
                | RebaseStep::Squash(c)
                | RebaseStep::Fixup(c) => {
                    let commit = self
                        .commits
                        .get_commit(c)
                        .ok_or_else(|| VersionError::CommitNotFound(c.to_hex()))?;
                    let is_folding = matches!(step, RebaseStep::Squash(_) | RebaseStep::Fixup(_));
                    let base = if is_folding {
                        group_base
                    } else if commit.parents.is_empty() {
                        None
                    } else {
                        Some(commit.parents[0])
                    };
                    let base_snap = base
                        .and_then(|b| self.snapshots.get(&b).cloned())
                        .unwrap_or_default();
                    // A folding step merges against the accumulated rebase so
                    // its diff stacks on the group's applied content, not on
                    // the last committed replay tip (which has not advanced).
                    let replay_snap = if is_folding {
                        rebase_work.clone()
                    } else {
                        self.snapshots
                            .get(&replay_head)
                            .cloned()
                            .unwrap_or_default()
                    };
                    let theirs_snap = self.snapshots.get(c).cloned().unwrap_or_default();
                    let merged = merge_snapshots(&base_snap, &replay_snap, &theirs_snap)?;
                    if !merged.conflicts.is_empty() {
                        let schema_blocked = merged.schema_conflict();
                        self.conflicts.extend(merged.conflicts);
                        self.work.clone_from(&work_before);
                        // The conflicted step's merged content becomes the
                        // rebase the user resolves into; the SQL working set
                        // is untouched until --continue commits it.
                        self.rebase_state = Some(RebaseState {
                            original_head,
                            onto,
                            steps: steps[i + 1..].to_vec(),
                            replay_head,
                            rebase_work: merged.work,
                            combined_message: combined_message.clone(),
                            conflicted_hash: c.to_hex(),
                            work_before,
                        });
                        if let Some((table, detail)) = schema_blocked {
                            return Err(VersionError::SchemaConflict(table, detail));
                        }
                        return Err(VersionError::RebaseConflict(c.to_hex()));
                    }
                    rebase_work = merged.work;
                    match step {
                        RebaseStep::Reword(_, msg) => combined_message = Some(msg.clone()),
                        RebaseStep::Pick(_) => combined_message = Some(commit.meta.message.clone()),
                        RebaseStep::Squash(_) => {
                            combined_message = Some(match combined_message.take() {
                                Some(prev) => format!("{prev}\n\n{}", commit.meta.message),
                                None => commit.meta.message.clone(),
                            });
                        }
                        RebaseStep::Fixup(_) => {}
                        RebaseStep::Drop(_) => unreachable!(),
                    }
                    group_base = Some(*c);
                    // A pick/reword starts a group; a following squash/fixup
                    // folds into it. A group ends at the next non-folding step.
                    let group_continues = i + 1 < steps.len()
                        && matches!(steps[i + 1], RebaseStep::Squash(_) | RebaseStep::Fixup(_));
                    if !group_continues {
                        let msg = combined_message
                            .take()
                            .unwrap_or_else(|| commit.meta.message.clone());
                        replay_head =
                            self.put_replay_commit(vec![replay_head], &msg, &rebase_work)?;
                        self.work = rebase_work.clone();
                        group_base = None;
                    }
                    i += 1;
                }
            }
        }
        // The branch tip advances only after every step committed cleanly.
        self.refs
            .compare_and_swap(
                &RefName::branch(self.head_branch()),
                Some(original_head),
                replay_head,
            )
            .map_err(|_| VersionError::DatabaseLocked)?;
        self.work = rebase_work;
        let changed: Vec<String> = self.work.keys().cloned().collect();
        self.mark_dirty(&changed);
        self.clear_staging();
        Ok(replay_head)
    }

    /// Create a commit in the store and record the working-set content as its
    /// snapshots, advancing the branch tip through the compare-and-swap of the ref.
    pub(crate) fn create_commit_with_work(
        &mut self,
        parents: Vec<CommitId>,
        msg: &str,
    ) -> VersionResult<CommitId> {
        let parent = parents.first().copied();
        let id = self.put_replay_commit(parents, msg, &self.work.clone())?;
        self.refs
            .compare_and_swap(&RefName::branch(self.head_branch()), parent, id)
            .map_err(|_| VersionError::DatabaseLocked)?;
        self.clear_staging();
        self.conflicts.clear();
        self.violations.clear();
        self.rebase_state = None;
        Ok(id)
    }

    /// Write a commit plus its table snapshots into the store without moving
    /// the branch ref; the caller owns the ref advance.
    fn put_replay_commit(
        &mut self,
        parents: Vec<CommitId>,
        msg: &str,
        content: &HashMap<String, TableSnapshot>,
    ) -> VersionResult<CommitId> {
        let (name, email) = self.author()?;
        let first_parent = parents.first().copied();
        let commit = Commit {
            parents,
            root: RootHash([0u8; 20]),
            meta: CommitMeta {
                name,
                email,
                message: msg.to_string(),
                timestamp: self.now_ts(),
            },
        };
        let id = self.commits.put_commit(commit);
        // Same rule as plain commits: start from the first parent's full
        // snapshot, overlay only what this replay changed.
        self.seed_snapshot_from_parent(id, first_parent);
        for (table, snap) in content {
            self.record_snapshot(
                id,
                table,
                snap.columns.clone(),
                snap.pk.clone(),
                snap.rows.clone(),
                snap.schema_sql.clone(),
            );
        }
        Ok(id)
    }

    fn message_of(&self, hash: &str) -> String {
        match CommitId::from_hex(hash)
            .ok()
            .and_then(|id| self.commits.get_commit(&id))
        {
            Some(c) => c.meta.message,
            None => hash.to_string(),
        }
    }

    /// The lowest common ancestor of two commits, for `dolt_merge_base`.
    pub fn merge_base_of(&self, a: CommitId, b: CommitId) -> VersionResult<Option<CommitId>> {
        merge_base(&self.commits, a, b)
    }

    /// Run the constraint detectors over the working content and record the
    /// violations for the commit gate; returns how many were found.
    pub fn verify_constraints(&mut self) -> VersionResult<usize> {
        let mut tables = Vec::with_capacity(self.work.len());
        for (name, snap) in &self.work {
            tables.push(crate::constraints::MergedTable {
                name: name.clone(),
                schema: schema_of(name, snap)?,
                rows: snap.rows.clone(),
            });
        }
        let merged = crate::constraints::MergedWork { tables };
        let schemas: Vec<SchemaIR> = merged.tables.iter().map(|t| t.schema.clone()).collect();
        let violations = crate::constraints::verify_constraints(&merged, &schemas);
        let count = violations.len();
        self.violations = violations;
        Ok(count)
    }
}

/// How many distinct tables produced conflicts, for the report string.
fn distinct_conflicted_tables(conflicts: &[ConflictEntry]) -> usize {
    let mut tables: Vec<&str> = conflicts.iter().map(|c| c.table.as_str()).collect();
    tables.sort_unstable();
    tables.dedup();
    tables.len()
}

/// The merged working set and conflict entries for one base/ours/theirs
/// snapshot triple. Presence is decided here: a table missing from both sides
/// is gone; everything else goes through the schema matrix and the row merge.
pub(crate) fn merge_snapshots(
    base: &HashMap<String, TableSnapshot>,
    ours: &HashMap<String, TableSnapshot>,
    theirs: &HashMap<String, TableSnapshot>,
) -> VersionResult<MergedTables> {
    let mut names: Vec<String> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .cloned()
        .collect();
    names.sort();
    names.dedup();

    let mut work = HashMap::new();
    let mut conflicts = Vec::new();
    let mut stats = HashMap::new();
    for name in names {
        let b = base.get(&name);
        let o = ours.get(&name);
        let t = theirs.get(&name);
        let mut stat = MergeTableStats::default();
        if o.is_none() && t.is_none() {
            continue;
        }
        let b_ir = match b {
            Some(s) => Some(schema_of(&name, s)?),
            None => None,
        };
        let o_ir = match o {
            Some(s) => Some(schema_of(&name, s)?),
            None => None,
        };
        let t_ir = match t {
            Some(s) => Some(schema_of(&name, s)?),
            None => None,
        };
        match merge_schema(b_ir.as_ref(), o_ir.as_ref(), t_ir.as_ref()) {
            SchemaDecision::Conflict(detail) => {
                stat.schema_conflict = true;
                // Keep ours-side image in work so the table doesn't vanish
                // during a schema conflict; the user resolves by
                // --ours/--theirs which restores the chosen side fully.
                if let Some(o) = o {
                    work.insert(name.clone(), o.clone());
                }
                conflicts.push(ConflictEntry {
                    table: name.clone(),
                    pk: Vec::new(),
                    base: None,
                    ours: None,
                    theirs: None,
                    kind: ConflictKind::Schema(detail),
                    ours_schema: o.cloned(),
                    theirs_schema: t.cloned(),
                });
            }
            SchemaDecision::Clean(ir) => {
                let columns: Vec<String> = ir.columns.iter().map(|c| c.name.clone()).collect();
                let pk = ir.pk.clone();
                let b_rows = project(b.map(|s| s.rows.as_slice()).unwrap_or(&[]), b, &columns);
                let o_rows = project(o.map(|s| s.rows.as_slice()).unwrap_or(&[]), o, &columns);
                let t_rows = project(t.map(|s| s.rows.as_slice()).unwrap_or(&[]), t, &columns);
                let outcome = three_way_row_merge(&b_rows, &o_rows, &t_rows, &pk, &columns);
                let mut keyed: std::collections::BTreeMap<Vec<VcValue>, VcRow> =
                    outcome.rows.into_iter().collect();
                // A conflicted key keeps its pre-resolution image so the
                // working set never silently drops a row the user must pick.
                for (_, c) in &outcome.conflicts {
                    if let Some(row) = &c.ours {
                        keyed.insert(c.pk.clone(), row.clone());
                    }
                }
                let rows: Vec<VcRow> = keyed.into_values().collect();
                work.insert(
                    name.clone(),
                    TableSnapshot {
                        columns: columns.clone(),
                        pk: pk.clone(),
                        rows,
                        schema_sql: merged_sql(&ir, &columns),
                    },
                );
                stat.rows_merged = work[&name].rows.len() as i64;
                stat.rows_conflicted = outcome.conflicts.len() as i64;
                for (_, c) in outcome.conflicts {
                    conflicts.push(ConflictEntry {
                        table: name.clone(),
                        pk: c.pk,
                        base: c.base,
                        ours: c.ours,
                        theirs: c.theirs,
                        kind: ConflictKind::Rows,
                        ours_schema: None,
                        theirs_schema: None,
                    });
                }
            }
        }
        stats.insert(name, stat);
    }
    Ok(MergedTables {
        work,
        conflicts,
        stats,
    })
}

/// The merged working set, conflict entries, and per-table counts.
pub(crate) struct MergedTables {
    pub work: HashMap<String, TableSnapshot>,
    pub conflicts: Vec<ConflictEntry>,
    pub stats: HashMap<String, MergeTableStats>,
}

impl MergedTables {
    /// The first schema conflict's table and detail, if any. Schema conflicts
    /// block the whole merge: the caller reports `schema conflict in table:
    /// NAME (DETAIL)` and keeps the conflict entries for `--ours`/`--theirs`
    /// resolution.
    pub(crate) fn schema_conflict(&self) -> Option<(String, String)> {
        self.conflicts.iter().find_map(|c| match &c.kind {
            ConflictKind::Schema(detail) => Some((c.table.clone(), detail.clone())),
            ConflictKind::Rows => None,
        })
    }
}

/// Re-order one side's rows into the merged column order by name, filling
/// missing cells with NULL.
fn project(rows: &[VcRow], snap: Option<&TableSnapshot>, to: &[String]) -> Vec<VcRow> {
    let from: Vec<String> = snap
        .map(|s| s.columns.clone())
        .unwrap_or_else(|| to.to_vec());
    if from == to {
        return rows.to_vec();
    }
    rows.iter()
        .map(|row| {
            VcRow::new(
                to.iter()
                    .map(|col| {
                        from.iter()
                            .position(|c| c == col)
                            .and_then(|i| row.values.get(i).cloned())
                            .unwrap_or(VcValue::Null)
                    })
                    .collect(),
            )
        })
        .collect()
}

/// A `SchemaIR` for a snapshot: parse the stored SQL when possible. A
/// non-empty SQL that will not parse is corruption, not a blank table shape;
/// fabricating a TEXT schema would silently merge the wrong columns, so this
/// bails instead. Only an empty SQL string (tests that drive content without
/// a schema) falls back to a minimal shape from the columns and pk the
/// snapshot carries.
pub(crate) fn schema_of(name: &str, snap: &TableSnapshot) -> VersionResult<SchemaIR> {
    if snap.schema_sql.trim().is_empty() {
        return Ok(SchemaIR {
            table: name.to_string(),
            columns: snap
                .columns
                .iter()
                .map(|c| ColIR {
                    name: c.clone(),
                    decl: "TEXT".to_string(),
                    notnull: false,
                    dflt: None,
                    pk_pos: None,
                })
                .collect(),
            pk: snap.pk.clone(),
            sql: snap.schema_sql.clone(),
            constraints: Vec::new(),
            strict: false,
        });
    }
    parse_schema(&snap.schema_sql).ok_or_else(|| VersionError::InvalidSchema(name.to_string()))
}

/// The merged `CREATE TABLE` SQL: base-order columns plus a table-level PK
/// only when no column carries an inline one.
fn merged_sql(ir: &SchemaIR, columns: &[String]) -> String {
    let inline_pk = ir
        .columns
        .iter()
        .any(|c| c.decl.to_uppercase().contains("PRIMARY KEY"));
    let mut parts: Vec<String> = ir
        .columns
        .iter()
        .map(|c| format!("\"{}\" {}", c.name, c.decl))
        .collect();
    if !ir.pk.is_empty() && !inline_pk {
        let pk = ir
            .pk
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("PRIMARY KEY ({pk})"));
    }
    for constraint in &ir.constraints {
        parts.push(constraint.clone());
    }
    let mut sql = format!("CREATE TABLE \"{}\" ({})", ir.table, parts.join(", "));
    if ir.strict {
        sql.push_str(" STRICT");
    }
    let _ = columns;
    sql
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::staging::VcStore;
    use crate::vtab_log::VcValue;

    fn configured_store() -> VcStore {
        let mut s = VcStore::new("main");
        s.config_set("user.name", "Ada");
        s.config_set("user.email", "ada@example.com");
        s
    }

    /// Commit working content as a commit at the current head, advancing it.
    fn commit_work(s: &mut VcStore, msg: &str) -> CommitId {
        let tables: Vec<String> = s.work_tables_content();
        for t in &tables {
            if !s.tables().contains(t) {
                s.track_table(t);
            }
        }
        s.dolt_add(&tables.iter().map(|t| t.as_str()).collect::<Vec<_>>())
            .unwrap();
        s.dolt_commit(msg, None, false, false).unwrap()
    }

    fn seed_diverged_branches() -> (VcStore, CommitId, CommitId, CommitId) {
        let mut s = configured_store();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("base".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("keep".into())]),
            ],
            String::new(),
        );
        let seed = commit_work(&mut s, "seed");
        s.create_branch("feature").unwrap();
        // feature side changes row 1 and adds row 3.
        s.checkout("feature").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("theirs".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("keep".into())]),
                VcRow::new(vec![VcValue::Integer(3), VcValue::Text("feat".into())]),
            ],
            String::new(),
        );
        let feature = commit_work(&mut s, "feature work");
        // main side changes row 1 differently and adds row 4.
        s.checkout("main").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("ours".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("keep".into())]),
                VcRow::new(vec![VcValue::Integer(4), VcValue::Text("main".into())]),
            ],
            String::new(),
        );
        let main = commit_work(&mut s, "main work");
        (s, seed, feature, main)
    }

    fn seed_clean_feature() -> (VcStore, CommitId, CommitId) {
        let mut s = configured_store();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("b".into())]),
            ],
            String::new(),
        );
        commit_work(&mut s, "seed");
        s.create_branch("feature").unwrap();
        s.checkout("feature").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("b".into())]),
                VcRow::new(vec![VcValue::Integer(3), VcValue::Text("feat".into())]),
            ],
            String::new(),
        );
        let feature = commit_work(&mut s, "feature work");
        s.checkout("main").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("b".into())]),
                VcRow::new(vec![VcValue::Integer(4), VcValue::Text("main".into())]),
            ],
            String::new(),
        );
        let main = commit_work(&mut s, "main work");
        (s, feature, main)
    }

    #[test]
    fn merge_branch_clean_fast_path_takes_ours() {
        // feature has not moved past base, so ours wins with zero conflicts.
        let mut s = configured_store();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("a".into()),
            ])],
            String::new(),
        );
        commit_work(&mut s, "seed");
        s.create_branch("feature").unwrap();
        s.checkout("feature").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("f".into()),
            ])],
            String::new(),
        );
        commit_work(&mut s, "feat");
        s.checkout("main").unwrap();
        match s.merge_branch("feature", false, false, None).unwrap() {
            MergeResult::Committed(_) => {}
            other => panic!("expected committed, got {other:?}"),
        }
        assert!(s.conflicts.is_empty());
    }

    #[test]
    fn merge_branch_divergent_row_is_conflict_and_resolvable() {
        let (mut s, _seed, _feature, _main) = seed_diverged_branches();
        let result = s.merge_branch("feature", false, false, None).unwrap();
        let MergeResult::Conflicts { rows, tables } = result else {
            panic!("expected conflict, got {result:?}");
        };
        assert_eq!(rows, 1);
        assert_eq!(tables, 1);
        assert_eq!(s.conflicts.len(), 1);
        assert_eq!(s.conflicts[0].table, "t");
        // Commit is refused while the conflict remains.
        s.set_now(10);
        assert_eq!(
            s.dolt_commit("x", None, false, false)
                .unwrap_err()
                .to_string(),
            "cannot commit: unresolved merge conflicts"
        );
        // Resolve to theirs unblocks commit.
        let entry = s.conflict_entries()[0].clone();
        s.resolve_conflict("t", &entry.pk, crate::conflicts::ResolveSide::Theirs)
            .unwrap();
        assert!(s.conflicts.is_empty());
        let id = s.dolt_commit("merged", None, false, false).unwrap();
        assert!(id != CommitId([0; 20]));
        // The merge commit carries both parents.
        assert_eq!(s.commits.get_commit(&id).unwrap().parents.len(), 2);
    }

    #[test]
    fn merge_no_commit_stops_before_commit() {
        let (mut s, _feature, _main) = seed_clean_feature();
        let result = s.merge_branch("feature", false, true, None).unwrap();
        assert_eq!(result, MergeResult::Applied);
        assert!(s.merge_state.is_some());
        let head = s.head_commit().unwrap();
        s.set_now(11);
        let id = s.dolt_commit("merged", None, false, false).unwrap();
        assert!(id != head);
        assert_eq!(s.commits.get_commit(&id).unwrap().parents.len(), 2);
    }

    #[test]
    fn merge_abort_restores_working_set_and_drops_conflicts() {
        let (mut s, _seed, _feature, _main) = seed_diverged_branches();
        let work_before = s.work.clone();
        let result = s.merge_branch("feature", false, false, None).unwrap();
        assert!(matches!(result, MergeResult::Conflicts { .. }));
        assert_eq!(s.conflicts.len(), 1);
        s.merge_abort().unwrap();
        assert!(s.conflicts.is_empty());
        assert_eq!(s.work, work_before);
        assert!(s.merge_state.is_none());
    }

    #[test]
    fn merge_abort_without_merge_errors() {
        let mut s = configured_store();
        assert_eq!(
            s.merge_abort().unwrap_err().to_string(),
            "no merge in progress"
        );
    }

    #[test]
    fn merge_status_rows_reflect_conflicts() {
        let (mut s, _seed, _feature, _main) = seed_diverged_branches();
        s.merge_branch("feature", false, false, None).unwrap();
        let rows = s.merge_status_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].table, "t");
        assert_eq!(rows[0].rows_conflicted, 1);
        assert_eq!(rows[0].state, "conflicted");
    }

    #[test]
    fn cherry_pick_applies_commit_and_commits() {
        let (mut s, feature, _main) = seed_clean_feature();
        match s.cherry_pick(&feature.to_hex(), None).unwrap() {
            MergeResult::Committed(id) => {
                let commit = s.commits.get_commit(&id).unwrap();
                assert_eq!(commit.meta.message, "feature work");
                assert_eq!(commit.parents.len(), 1);
            }
            other => panic!("expected committed, got {other:?}"),
        }
        // The replayed table has the feature row.
        let tip = s.head_commit().unwrap();
        let rows = s.snapshots[&tip]["t"].rows.clone();
        assert!(rows.iter().any(|r| r.values.contains(&VcValue::Integer(3))));
    }

    #[test]
    fn cherry_pick_merge_commit_needs_parent() {
        let mut s = configured_store();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("a".into()),
            ])],
            String::new(),
        );
        commit_work(&mut s, "seed");
        s.create_branch("other").unwrap();
        s.checkout("other").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("b".into()),
            ])],
            String::new(),
        );
        commit_work(&mut s, "other work");
        s.checkout("main").unwrap();
        s.merge_branch("other", false, false, None).unwrap();
        let tip = s.head_commit().unwrap();
        assert_eq!(s.commits.get_commit(&tip).unwrap().parents.len(), 2);
        let err = s.cherry_pick(&tip.to_hex(), None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cherry-pick of merge commit needs -m PARENT (got 2 parents)"
        );
    }

    #[test]
    fn revert_inverts_a_commit() {
        let (mut s, feature, main) = seed_clean_feature();
        // Reverting main's own commit removes the row main added.
        match s.revert(&main.to_hex(), None).unwrap() {
            MergeResult::Committed(id) => {
                let commit = s.commits.get_commit(&id).unwrap();
                assert_eq!(commit.meta.message, "Revert \"main work\"");
            }
            other => panic!("expected committed, got {other:?}"),
        }
        let tip = s.head_commit().unwrap();
        let rows = s.snapshots[&tip]["t"].rows.clone();
        assert!(rows
            .iter()
            .all(|r| r.values.contains(&VcValue::Integer(1))
                || r.values.contains(&VcValue::Integer(2))));
        assert!(rows
            .iter()
            .all(|r| !r.values.contains(&VcValue::Integer(4))));
        let _ = feature;
    }

    #[test]
    fn rebase_onto_replays_picks() {
        let (mut s, _feature, _main) = seed_clean_feature();
        // feature has one commit on top of seed; main has one on top of seed.
        // Replaying feature onto main applies its commit cleanly.
        s.checkout("feature").unwrap();
        let head = s.head_commit().unwrap();
        let new_head = s.rebase_onto("main").unwrap();
        assert!(new_head != head);
        // The replayed chain now sits on main.
        let chain = ancestors(&s.commits, new_head).unwrap();
        assert!(chain.contains(&new_head));
        assert!(s.rebase_state.is_none());
        assert!(s.conflicts.is_empty());
    }

    #[test]
    fn rebase_conflict_is_atomic_and_continue_resumes() {
        let (mut s, _seed, _feature, _main) = seed_diverged_branches();
        // The feature branch changed row 1 from "theirs"; main changed it to
        // "ours". Replaying feature's commit onto main conflicts.
        s.checkout("feature").unwrap();
        let head_before = s.head_commit().unwrap();
        let err = s.rebase_onto("main").unwrap_err();
        assert!(
            err.to_string().starts_with("rebase conflict at "),
            "{}",
            err
        );
        assert!(err.to_string().ends_with(": resolve and --continue"));
        // Atomic: the branch tip never moved.
        assert_eq!(s.head_commit().unwrap(), head_before);
        assert!(s.rebase_state.is_some());
        assert_eq!(s.conflicts.len(), 1);
        // Resolve then continue.
        let entry = s.conflict_entries()[0].clone();
        s.resolve_conflict("t", &entry.pk, crate::conflicts::ResolveSide::Ours)
            .unwrap();
        let new_head = s.rebase_continue().unwrap();
        assert!(s.rebase_state.is_none());
        let chain = ancestors(&s.commits, new_head).unwrap();
        assert!(chain.contains(&new_head));
    }

    #[test]
    fn rebase_continue_without_rebase_errors() {
        let mut s = configured_store();
        assert_eq!(
            s.rebase_continue().unwrap_err().to_string(),
            "no rebase in progress"
        );
        assert_eq!(
            s.rebase_abort().unwrap_err().to_string(),
            "no rebase in progress"
        );
    }

    #[test]
    fn rebase_abort_restores_original_head() {
        let (mut s, _seed, _feature, _main) = seed_diverged_branches();
        s.checkout("feature").unwrap();
        let head_before = s.head_commit().unwrap();
        assert!(s.rebase_onto("main").is_err());
        s.rebase_abort().unwrap();
        assert_eq!(s.head_commit().unwrap(), head_before);
        assert!(s.conflicts.is_empty());
        assert!(s.rebase_state.is_none());
    }

    #[test]
    fn merge_bails_on_unparsable_schema_sql() {
        let mut s = configured_store();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("a".into()),
            ])],
            String::new(),
        );
        commit_work(&mut s, "seed");
        s.create_branch("feature").unwrap();
        s.checkout("feature").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("b".into()),
            ])],
            String::new(),
        );
        commit_work(&mut s, "feature work");
        s.checkout("main").unwrap();
        // Corrupt the committed snapshot's schema SQL on the main side: a
        // non-empty string that will not parse must abort the merge rather
        // than silently fabricate a TEXT schema.
        let main_tip = s.head_commit().unwrap();
        let snap = s.snapshots[&main_tip]["t"].clone();
        let mut broken = snap;
        broken.schema_sql = "NOT A CREATE TABLE".to_string();
        s.snapshots
            .get_mut(&main_tip)
            .unwrap()
            .insert("t".to_string(), broken);
        let err = s.merge_branch("feature", false, false, None).unwrap_err();
        assert_eq!(err.to_string(), "invalid schema for table: t");
    }

    #[test]
    fn checkout_refused_while_merge_open() {
        let (mut s, _seed, _feature, _main) = seed_diverged_branches();
        let result = s.merge_branch("feature", false, false, None).unwrap();
        assert!(matches!(result, MergeResult::Conflicts { .. }));
        let err = s.checkout("feature").unwrap_err();
        assert_eq!(err.to_string(), "merge already in progress");
    }

    #[test]
    fn rebase_abort_restores_pre_rebase_work_after_resolve() {
        let (mut s, _seed, _feature, _main) = seed_diverged_branches();
        s.checkout("feature").unwrap();
        // The checkout shim rewrites the working set to the branch's committed
        // content; mirror it so the pre-rebase working set is feature's.
        s.sync_work_to_head();
        let work_before = s.work.clone();
        assert!(s.rebase_onto("main").is_err());
        // Resolving a conflicted row mutates the working set; --abort must put
        // the pre-rebase working set back regardless.
        let entry = s.conflict_entries()[0].clone();
        s.resolve_conflict("t", &entry.pk, crate::conflicts::ResolveSide::Ours)
            .unwrap();
        assert_ne!(s.work, work_before);
        s.rebase_abort().unwrap();
        assert_eq!(s.work, work_before);
        assert!(s.conflicts.is_empty());
        assert!(s.rebase_state.is_none());
    }

    #[test]
    fn second_merge_while_conflicted_is_refused_and_work_preserved() {
        let (mut s, _seed, _feature, _main) = seed_diverged_branches();
        let result = s.merge_branch("feature", false, false, None).unwrap();
        assert!(matches!(result, MergeResult::Conflicts { .. }));
        let work_before = s.work.clone();
        let err = s.merge_branch("feature", false, false, None).unwrap_err();
        assert_eq!(err.to_string(), "merge already in progress");
        assert_eq!(s.work, work_before);
        assert_eq!(s.conflicts.len(), 1);
    }

    #[test]
    fn cherry_pick_refused_while_conflicted() {
        let (mut s, _seed, feature, _main) = seed_diverged_branches();
        let result = s.merge_branch("feature", false, false, None).unwrap();
        assert!(matches!(result, MergeResult::Conflicts { .. }));
        let err = s.cherry_pick(&feature.to_hex(), None).unwrap_err();
        assert_eq!(err.to_string(), "merge already in progress");
    }

    #[test]
    fn merge_with_m_parent_selector_uses_chosen_parent_base() {
        let mut s = configured_store();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("seed".into()),
            ])],
            String::new(),
        );
        commit_work(&mut s, "seed");
        // feature commits two rows on top of seed.
        s.create_branch("feature").unwrap();
        s.checkout("feature").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("seed".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("f1".into())]),
            ],
            String::new(),
        );
        let c1 = commit_work(&mut s, "f1");
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("seed".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("f1".into())]),
                VcRow::new(vec![VcValue::Integer(3), VcValue::Text("f2".into())]),
            ],
            String::new(),
        );
        commit_work(&mut s, "f2");
        // main adds one row, then merges feature into a merge commit.
        s.checkout("main").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("seed".into())]),
                VcRow::new(vec![VcValue::Integer(4), VcValue::Text("main".into())]),
            ],
            String::new(),
        );
        commit_work(&mut s, "main work");
        let merge_commit = match s.merge_branch("feature", false, false, None).unwrap() {
            MergeResult::Committed(id) => id,
            other => panic!("expected committed, got {other:?}"),
        };
        let parents = s.get_commit(&merge_commit).unwrap().parents;
        assert_eq!(parents.len(), 2);
        // A third branch merges the merge commit with -m 1, using parent 1
        // (main's own tip) as the base: a clean fast-path merge.
        s.create_branch("topic").unwrap();
        s.checkout("topic").unwrap();
        let result = s
            .merge_branch(&merge_commit.to_hex(), false, false, Some(0))
            .unwrap();
        assert!(matches!(result, MergeResult::Committed(_)));
        let _ = c1;
    }

    #[test]
    fn rebase_plan_pause_edit_drop_replays_single_commit() {
        let mut s = configured_store();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("a".into()),
            ])],
            String::new(),
        );
        commit_work(&mut s, "seed");
        s.create_branch("feature").unwrap();
        s.checkout("feature").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("first".into())]),
            ],
            String::new(),
        );
        let c1 = commit_work(&mut s, "first");
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("first".into())]),
                VcRow::new(vec![VcValue::Integer(3), VcValue::Text("second".into())]),
            ],
            String::new(),
        );
        let c2 = commit_work(&mut s, "second");
        s.checkout("main").unwrap();
        let main_tip = s.head_commit().unwrap();
        s.checkout("feature").unwrap();
        let head0 = s.head_commit().unwrap();
        // Pause with the plan, edit it to drop the first commit, then continue.
        let count = s.rebase_plan("main").unwrap();
        assert_eq!(count, 2);
        let steps = vec![RebaseStep::Drop(c1), RebaseStep::Pick(c2)];
        s.rebase_edit_plan(steps).unwrap();
        let new_head = s.rebase_continue().unwrap();
        assert_ne!(new_head, head0);
        // Only one replay commit sits on main's tip: c1 was dropped.
        let commit = s.get_commit(&new_head).unwrap();
        assert_eq!(commit.meta.message, "second");
        assert_eq!(commit.parents, vec![main_tip]);
        let rows = s.snapshots[&new_head]["t"].rows.clone();
        assert!(rows.iter().any(|r| r.values.contains(&VcValue::Integer(3))));
        assert!(rows
            .iter()
            .all(|r| !r.values.contains(&VcValue::Integer(2))));
    }

    #[test]
    fn rebase_squash_folds_messages_into_one_commit() {
        let mut s = configured_store();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("a".into()),
            ])],
            String::new(),
        );
        commit_work(&mut s, "seed");
        s.create_branch("feature").unwrap();
        s.checkout("feature").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("b".into()),
            ])],
            String::new(),
        );
        let c1 = commit_work(&mut s, "first");
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("c".into()),
            ])],
            String::new(),
        );
        let c2 = commit_work(&mut s, "second");
        // Rebase with a manual squash plan onto main.
        s.checkout("main").unwrap();
        let main_tip = s.head_commit().unwrap();
        s.checkout("feature").unwrap();
        let steps = vec![RebaseStep::Pick(c1), RebaseStep::Squash(c2)];
        let head0 = s.head_commit().unwrap();
        let new_head = s
            .execute_rebase(steps, main_tip, head0, None, s.work.clone())
            .unwrap();
        let commit = s.commits.get_commit(&new_head).unwrap();
        assert_eq!(commit.meta.message, "first\n\nsecond");
        assert_eq!(commit.parents, vec![main_tip]);
    }

    #[test]
    fn ours_deleted_theirs_survives_in_schema_conflict() {
        let mut s = configured_store();
        // Seed: base has table "t" with schema SQL.
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("base".into()),
            ])],
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
        );
        let seed = commit_work(&mut s, "seed");
        s.create_branch("feature").unwrap();
        // Feature modifies the table (adds a column).
        s.checkout("feature").unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string(), "extra".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("base".into()),
                VcValue::Text("feat".into()),
            ])],
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, extra TEXT)".to_string(),
        );
        commit_work(&mut s, "feature work");
        // Main drops the table and creates a commit without "t", so the
        // merge base (seed) has "t" but main's HEAD snapshot does not.
        s.checkout("main").unwrap();
        s.work.remove("t");
        // Add a dummy table so there's something to commit.
        s.apply_work(
            "dummy",
            vec!["id".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![VcValue::Integer(1)])],
            String::new(),
        );
        if !s.tables().contains(&"dummy".to_string()) {
            s.track_table("dummy");
        }
        let main_tables: Vec<String> = s.work_tables_content();
        s.dolt_add(&main_tables.iter().map(|t| t.as_str()).collect::<Vec<_>>())
            .unwrap();
        s.set_now(10);
        s.dolt_commit("drop t", None, false, false).unwrap();
        // Merge feature into main: base(seed) has "t", main deleted it,
        // theirs modified it → schema conflict.
        let result = s.merge_branch("feature", false, false, None);
        match result.unwrap() {
            MergeResult::SchemaConflict { table, detail } => {
                assert_eq!(table, "t");
                assert!(detail.contains("table deleted on ours"));
            }
            other => panic!("expected schema conflict, got {other:?}"),
        }
        // The conflict entry carries theirs_schema so --theirs can restore it.
        let entries = s.conflict_entries();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].theirs_schema.is_some(),
            "theirs_schema must be preserved for --theirs resolution"
        );
        let theirs_snap = entries[0].theirs_schema.as_ref().unwrap();
        assert!(
            theirs_snap.schema_sql.contains("extra"),
            "theirs schema must carry the modified table"
        );
        let _ = seed;
    }
}
