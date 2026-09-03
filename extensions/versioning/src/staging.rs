//! Branch-scoped working/staged sets and the `dolt_*` state machine.
//!
//! S1–S4: dolt_add/add_all/commit/status/reset/clean/config, commit gates,
//! and the savepoint seal/preserve matrix.

use std::collections::{HashMap, HashSet};

use crate::commit::{Commit, CommitMeta, CommitStore, MemCommitStore};
use crate::conflicts::ConflictEntry;
use crate::constraints::{MergedWork, Violation};
use crate::model::{CommitId, RootHash, VersionError, VersionResult};
use crate::refs::{MemRefStore, RefError, RefName, RefStore};
use crate::vtab_log::{VcCommitView, VcRead, VcRow, VcValue};

/// Lifecycle state a table can report through `dolt_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableState {
    NewTable,
    Modified,
    Deleted,
    Renamed,
}

impl std::fmt::Display for TableState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

impl TableState {
    pub fn label(self) -> &'static str {
        match self {
            TableState::NewTable => "new table",
            TableState::Modified => "modified",
            TableState::Deleted => "deleted",
            TableState::Renamed => "renamed",
        }
    }
}

/// One row of `dolt_status` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    pub table: String,
    pub staged: bool,
    pub status: TableState,
}

/// One committed table snapshot. O4 fills these through `record_snapshot`;
/// until then the map is empty and diff/history reads see metadata only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableSnapshot {
    pub columns: Vec<String>,
    pub pk: Vec<String>,
    pub rows: Vec<VcRow>,
    pub schema_sql: String,
}

/// Version-control state for one session: commit store, ref store, active
/// branch, staging, conflict/violation gates, and per-connection config.
///
/// A detached session pins a commit snapshot (R4): every mutating operation
/// is refused until a checkout reattaches it to a named branch.
pub struct VcStore {
    pub(crate) commits: MemCommitStore,
    pub(crate) refs: MemRefStore,
    head: String,
    branches: HashSet<String>,
    detached: Option<CommitId>,
    staging: StagingSet,
    /// Transient conflict rows. Never written into a commit, never durable.
    pub(crate) conflicts: Vec<ConflictEntry>,
    /// Detected constraint violations over the working set.
    pub(crate) violations: Vec<Violation>,
    /// Working content of the tracked tables. The glue keeps it in sync with
    /// the SQL tables; merge/replay read the committed snapshots, produce this
    /// content, and the glue writes it back.
    pub(crate) work: HashMap<String, TableSnapshot>,
    /// Row images chosen by `--ours`/`--theirs` resolution, waiting for the
    /// glue to apply them to the working SQL tables.
    pub(crate) pending_resolve: HashMap<String, HashMap<Vec<VcValue>, Option<VcRow>>>,
    /// Tables whose working content the glue must write back to SQL after a
    /// merge/replay/resolve (SQL itself already matches after a plain commit).
    pub(crate) dirty_work: HashSet<String>,
    /// Merged/replayed content awaiting SQL application (working-set apply for
    /// `--no-commit` and rebase `--continue`).
    pub(crate) pending: Option<MergedWork>,
    /// A rebase that paused on a conflict.
    pub(crate) rebase_state: Option<crate::replay::RebaseState>,
    /// An in-progress merge for `dolt_merge_status`.
    pub(crate) merge_state: Option<crate::replay::MergeState>,
    /// Tables the last `dolt_commit` wrote, so the glue can capture their SQL
    /// content into the new commit's snapshots.
    pub(crate) last_commit_tables: Vec<String>,
    config: HashMap<String, String>,
    tables: HashSet<String>,
    states: HashMap<String, TableState>,
    pub(crate) snapshots: HashMap<CommitId, HashMap<String, TableSnapshot>>,
    now: i64,
}

impl VcStore {
    pub fn new(head: &str) -> Self {
        let mut branches = HashSet::new();
        branches.insert(head.to_string());
        VcStore {
            commits: MemCommitStore::new(),
            refs: MemRefStore::new(),
            head: head.to_string(),
            branches,
            detached: None,
            staging: StagingSet::new(),
            conflicts: Vec::new(),
            violations: Vec::new(),
            work: HashMap::new(),
            pending_resolve: HashMap::new(),
            dirty_work: HashSet::new(),
            pending: None,
            rebase_state: None,
            merge_state: None,
            last_commit_tables: Vec::new(),
            config: HashMap::new(),
            tables: HashSet::new(),
            states: HashMap::new(),
            snapshots: HashMap::new(),
            now: 0,
        }
    }

    /// Clock override for deterministic commit ids in tests.
    pub fn set_now(&mut self, now: i64) {
        self.now = now;
    }

    /// The current clock value, for replay commits.
    pub(crate) fn now_ts(&self) -> i64 {
        self.now
    }

    /// The active branch's name.
    pub(crate) fn head_branch(&self) -> &str {
        &self.head
    }

    /// The configured author, or the invalid-author error.
    pub(crate) fn author(&self) -> VersionResult<(String, String)> {
        match (
            self.config.get("user.name").cloned(),
            self.config.get("user.email").cloned(),
        ) {
            (Some(name), Some(email)) => Ok((name, email)),
            _ => Err(VersionError::InvalidAuthor),
        }
    }

    /// Record the working content of one table (the glue's SQL capture hook;
    /// tests call it directly).
    pub fn apply_work(
        &mut self,
        table: &str,
        columns: Vec<String>,
        pk: Vec<String>,
        rows: Vec<VcRow>,
        schema_sql: String,
    ) {
        self.work.insert(
            table.to_string(),
            TableSnapshot {
                columns,
                pk,
                rows,
                schema_sql,
            },
        );
    }

    /// The working content of one table.
    pub fn work_table(&self, table: &str) -> Option<&TableSnapshot> {
        self.work.get(table)
    }

    /// Names of the tables with working content, sorted.
    pub fn work_tables_content(&self) -> Vec<String> {
        let mut names: Vec<String> = self.work.keys().cloned().collect();
        names.sort();
        names
    }

    /// Tables whose working content the glue must write back to SQL, drained.
    pub fn take_work_changes(&mut self) -> Vec<(String, TableSnapshot)> {
        let mut tables: Vec<String> = self.dirty_work.iter().cloned().collect();
        tables.sort();
        self.dirty_work.clear();
        tables
            .into_iter()
            .filter_map(|t| self.work.get(&t).map(|s| (t, s.clone())))
            .collect()
    }

    /// Mark tables as needing a write-back to SQL after a merge/replay.
    pub(crate) fn mark_dirty(&mut self, tables: &[String]) {
        self.dirty_work.extend(tables.iter().cloned());
    }

    /// Restore pending conflict resolutions drained before a failed write-back
    /// so a retry can re-attempt applying them to SQL.
    pub fn restore_pending_resolve(&mut self, entries: Vec<(String, Vec<VcValue>, Option<VcRow>)>) {
        for (table, pk, image) in entries {
            self.pending_resolve
                .entry(table)
                .or_default()
                .insert(pk, image);
        }
    }

    /// Restore dirty flags drained before a failed write-back so the next
    /// `take_work_changes` call can re-attempt the write-back.
    pub fn restore_dirty_work(&mut self, tables: Vec<String>) {
        self.dirty_work.extend(tables);
    }

    /// Move tables into the staged set (merge/replay apply them to the working
    /// SQL set, so a follow-up `dolt_commit` picks them up).
    pub(crate) fn stage_names(&mut self, tables: &[String]) {
        for table in tables {
            self.staging.stage(table);
        }
    }

    /// Drop every staged and working table name without touching content.
    pub(crate) fn clear_staging(&mut self) {
        self.staging.clear_all();
    }

    /// Pin this session at `snapshot`, refusing further writes (R4).
    pub fn open_detached(&mut self, snapshot: CommitId) {
        self.detached = Some(snapshot);
    }

    /// The pinned snapshot while detached.
    pub fn detached_snapshot(&self) -> Option<CommitId> {
        self.detached
    }

    /// Writes are refused on a detached snapshot (R4).
    pub fn guard_write(&self) -> VersionResult<()> {
        if self.detached.is_some() {
            return Err(VersionError::DetachedHeadWrite);
        }
        Ok(())
    }

    /// Record a table as known to version control. A freshly created table is
    /// also an uncommitted working change (S1), so it lands in the working
    /// set until `dolt_add` stages it.
    pub fn track_table(&mut self, name: &str) {
        self.tables.insert(name.to_string());
        self.staging.stage_working(name);
    }

    pub fn tables(&self) -> Vec<String> {
        self.tables.iter().cloned().collect()
    }

    pub fn set_table_state(&mut self, table: &str, state: TableState) {
        self.states.insert(table.to_string(), state);
    }

    pub fn head_commit(&self) -> Option<CommitId> {
        self.refs.get(&RefName::branch(&self.head))
    }

    /// Look up one commit by id (integration tests inspect the graph).
    pub fn get_commit(&self, id: &CommitId) -> Option<Commit> {
        self.commits.get_commit(id)
    }

    /// Every commit, unsorted (integration tests build history views).
    pub fn commit_entries(&self) -> Vec<(CommitId, Commit)> {
        self.commits.entries()
    }

    /// The underlying commit store, for graph walks.
    pub fn commit_store(&self) -> &crate::commit::MemCommitStore {
        &self.commits
    }

    /// How many conflicts are currently recorded.
    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }

    /// Whether a merge is open (the store's working set is the merged state).
    pub fn is_merge_open(&self) -> bool {
        self.merge_state.is_some()
    }

    /// Whether a rebase is paused on a conflict.
    pub fn is_rebase_open(&self) -> bool {
        self.rebase_state.is_some()
    }

    /// Whether any conflict is currently recorded.
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// The recorded constraint violations, for the violations vtables.
    pub fn violation_entries(&self) -> Vec<Violation> {
        self.violations.clone()
    }

    /// The current working content of every table, sorted by name.
    pub fn work_snapshots(&self) -> Vec<(String, TableSnapshot)> {
        let mut tables: Vec<String> = self.work.keys().cloned().collect();
        tables.sort();
        tables
            .into_iter()
            .map(|t| (t.clone(), self.work[&t].clone()))
            .collect()
    }

    /// Replace the working content with the active branch's committed
    /// snapshots, so a checkout rewrites the SQL tables to that branch's state.
    pub fn sync_work_to_head(&mut self) {
        let head = self.head_commit();
        self.work = head
            .and_then(|id| self.snapshots.get(&id).cloned())
            .unwrap_or_default();
        let names: Vec<String> = self.work.keys().cloned().collect();
        self.mark_dirty(&names);
    }

    pub fn status(&self) -> Vec<StatusRow> {
        self.staging.status(&self.states)
    }

    /// Tables with uncommitted changes not yet staged.
    pub fn working_tables(&self) -> Vec<String> {
        self.staging.working_tables()
    }

    /// S1: stage working changes for the given tables.
    pub fn dolt_add(&mut self, tables: &[&str]) -> VersionResult<()> {
        self.guard_write()?;
        for table in tables {
            if !self.tables.contains(*table) {
                return Err(VersionError::TableNotFound(table.to_string()));
            }
            self.staging.stage(table);
        }
        Ok(())
    }

    /// S1: stage everything, including new tables.
    pub fn add_all(&mut self) -> VersionResult<()> {
        self.guard_write()?;
        let all = self.staging.working_tables();
        self.staging.stage_all(&all);
        Ok(())
    }

    /// S2: commit the staged set. Gates fire in doltlite's order: conflicts
    /// always refuse, violations refuse unless `force` bypasses them, then
    /// nothing-to-commit.
    ///
    /// `all` mirrors the `-A` flag: stage the full working set first. The
    /// author comes from the override or `user.name`/`user.email` config.
    /// The branch tip advances only through the ref compare-and-swap; a racing writer surfaces as
    /// `database is locked`.
    pub fn dolt_commit(
        &mut self,
        msg: &str,
        author_override: Option<(&str, &str)>,
        all: bool,
        force: bool,
    ) -> VersionResult<CommitId> {
        self.guard_write()?;
        if !self.conflicts.is_empty() {
            return Err(VersionError::Conflicts);
        }
        if !self.violations.is_empty() && !force {
            return Err(VersionError::Violations);
        }
        if all {
            let working = self.staging.working_tables();
            self.staging.stage_all(&working);
        }
        if !self.staging.has_staged() {
            return Err(VersionError::NothingToCommit);
        }
        // The glue captures SQL content for these tables under the new commit.
        self.last_commit_tables = self.staging.staged_tables();
        let (name, email) = match author_override {
            Some((name, email)) => (name.to_string(), email.to_string()),
            None => match (
                self.config.get("user.name").cloned(),
                self.config.get("user.email").cloned(),
            ) {
                (Some(name), Some(email)) => (name, email),
                _ => return Err(VersionError::InvalidAuthor),
            },
        };
        // A merge commit carries the merged side as its second parent. The
        // open merge either stopped before committing (`--no-commit`) or is a
        // conflicted merge the user resolved and is now finishing.
        let parent = self.head_commit();
        let mut parents: Vec<CommitId> = parent.into_iter().collect();
        if let Some(ms) = &self.merge_state {
            if !parents.contains(&ms.theirs) {
                parents.push(ms.theirs);
            }
        }
        let commit = Commit {
            parents: parents.clone(),
            // O3 computes real root hashes; a zero root keeps O2 commit ids
            // stable for tests until then.
            root: RootHash([0u8; 20]),
            meta: CommitMeta {
                name,
                email,
                message: msg.to_string(),
                timestamp: self.now,
            },
        };
        let id = self.commits.put_commit(commit);
        match self
            .refs
            .compare_and_swap(&RefName::branch(&self.head), parents.first().copied(), id)
        {
            Ok(()) => {}
            Err(RefError::Busy) => return Err(VersionError::DatabaseLocked),
        }
        // Record the working content under the new commit so the committed
        // snapshots carry rows for history/at/diff reads. Tables the glue
        // captures separately overwrite these through `record_snapshot`.
        let captured: Vec<(String, TableSnapshot)> = self
            .last_commit_tables
            .iter()
            .filter_map(|table| self.work.get(table).map(|s| (table.clone(), s.clone())))
            .collect();
        for (table, snap) in captured {
            self.record_snapshot(
                id,
                &table,
                snap.columns.clone(),
                snap.pk.clone(),
                snap.rows.clone(),
                snap.schema_sql.clone(),
            );
        }
        self.staging.clear_staged();
        if force {
            // The violations described the pre-commit working set, which is
            // now committed; recording them past the commit would be stale.
            self.violations.clear();
        }
        self.merge_state = None;
        self.rebase_state = None;
        self.pending = None;
        Ok(id)
    }

    /// S3: `dolt_reset --soft` — unstage everything, keep working changes.
    pub fn reset_soft(&mut self) -> VersionResult<()> {
        self.guard_write()?;
        let staged = self.staging.staged_tables();
        for table in &staged {
            self.staging.unstage(table);
        }
        Ok(())
    }

    /// S3: `dolt_reset --hard` — discard all uncommitted changes.
    pub fn reset_hard(&mut self) -> VersionResult<()> {
        self.guard_write()?;
        self.staging.clear_all();
        Ok(())
    }

    /// S3: `dolt_clean` — drop untracked tables only. A tracked table stays
    /// even when it is modified and sitting in the working set.
    pub fn clean(&mut self) -> VersionResult<()> {
        self.guard_write()?;
        self.staging.discard_untracked(&self.tables);
        Ok(())
    }

    pub fn config_get(&self, key: &str) -> Option<String> {
        self.config.get(key).cloned()
    }

    pub fn config_set(&mut self, key: &str, value: &str) {
        self.config.insert(key.to_string(), value.to_string());
    }

    /// R1: create a branch at the current HEAD.
    pub fn create_branch(&mut self, name: &str) -> VersionResult<()> {
        self.guard_write()?;
        if self.branches.contains(name) {
            return Err(VersionError::BranchAlreadyExists(name.to_string()));
        }
        if let Some(head) = self.head_commit() {
            self.refs.set(&RefName::branch(name), head);
        }
        self.branches.insert(name.to_string());
        Ok(())
    }

    /// R1: delete a branch. Deleting the active branch is allowed; it leaves
    /// the active branch with no commit and no ref (doltlite does not reserve
    /// the active branch name).
    pub fn delete_branch(&mut self, name: &str) -> VersionResult<()> {
        self.guard_write()?;
        if !self.branches.remove(name) {
            return Err(VersionError::BranchNotFound(name.to_string()));
        }
        self.refs.delete(&RefName::branch(name));
        Ok(())
    }

    pub fn list_branches(&self) -> Vec<String> {
        let mut names: Vec<String> = self.branches.iter().cloned().collect();
        names.sort();
        names
    }

    /// R3: switch the session to another branch. Also reattaches a detached
    /// session (R4). A checkout while a merge or rebase is open, or while
    /// conflicts are recorded, is refused: switching branches would carry the
    /// open operation's working set across to a branch it was not started on.
    pub fn checkout(&mut self, name: &str) -> VersionResult<()> {
        if !self.branches.contains(name) {
            return Err(VersionError::BranchNotFound(name.to_string()));
        }
        if self.merge_state.is_some() {
            return Err(VersionError::MergeInProgress);
        }
        if self.rebase_state.is_some() {
            return Err(VersionError::RebaseInProgress);
        }
        if !self.conflicts.is_empty() {
            return Err(VersionError::MergeInProgress);
        }
        self.head = name.to_string();
        self.detached = None;
        Ok(())
    }

    /// `None` while detached, so `dolt_active_branch()` reads as NULL.
    pub fn active_branch(&self) -> Option<&str> {
        if self.detached.is_some() {
            return None;
        }
        Some(&self.head)
    }

    /// R1: tag a commit.
    pub fn create_tag(&mut self, name: &str, at: CommitId) -> VersionResult<()> {
        self.guard_write()?;
        self.refs.set(&RefName::tag(name), at);
        Ok(())
    }

    /// R1: delete a tag.
    pub fn delete_tag(&mut self, name: &str) -> VersionResult<()> {
        self.guard_write()?;
        if !self.refs.delete(&RefName::tag(name)) {
            return Err(VersionError::TagNotFound(name.to_string()));
        }
        Ok(())
    }

    /// Record one table snapshot at one commit (O4's capture hook).
    pub fn record_snapshot(
        &mut self,
        at: CommitId,
        table: &str,
        columns: Vec<String>,
        pk: Vec<String>,
        rows: Vec<VcRow>,
        schema_sql: String,
    ) {
        self.snapshots.entry(at).or_default().insert(
            table.to_string(),
            TableSnapshot {
                columns,
                pk,
                rows,
                schema_sql,
            },
        );
    }

    /// All table names present in any committed snapshot (conflict detection
    /// never writes committed content, so conflicts are absent by construction).
    pub fn committed_snapshot_tables(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .snapshots
            .values()
            .flat_map(|m| m.keys().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Newest-first commit views for the log path (timestamp order).
    pub fn log_views(&self) -> Vec<(CommitId, Commit)> {
        let mut entries = self.commits.entries();
        entries.sort_by(|a, b| {
            b.1.meta
                .timestamp
                .cmp(&a.1.meta.timestamp)
                .then_with(|| a.0.to_hex().cmp(&b.0.to_hex()))
        });
        entries
    }

    /// R5: resolve a revision string against this session's HEAD.
    pub fn resolve(&self, spec: &str) -> VersionResult<CommitId> {
        let revision = crate::refs::parse_revision(spec)?;
        self.resolve_revision(&revision)
    }

    /// Resolve a parsed revision against this session's state.
    ///
    /// WORKING/STAGED resolve to their placeholder ids while the matching set is
    /// non-empty (same rule as `MemVcRead`, which keys on snapshot presence;
    /// this store keys on set membership because snapshots arrive with O4).
    /// Readers serve the matching set or nothing; the ids never enter the
    /// commit store, so resolving them through `commit()` stays an error.
    /// `WORKING~N` and friends stay unresolvable: snapshots have no history.
    pub fn resolve_revision(&self, revision: &crate::refs::Revision) -> VersionResult<CommitId> {
        use crate::refs::Revision;
        use crate::vtab_log::{staged_id, working_id};
        match revision {
            Revision::Working if !self.staging.working_tables().is_empty() => Ok(working_id()),
            Revision::Staged if !self.staging.staged_tables().is_empty() => Ok(staged_id()),
            _ => {
                let head = self.detached.or_else(|| self.head_commit());
                crate::commit::resolve_revision(
                    &self.commits,
                    &self.refs,
                    revision,
                    head,
                    None,
                    None,
                )
            }
        }
    }

    /// S4: is staging sealed (invisible) while the SQL session is inside a
    /// savepoint? Caller contract: the SQL layer calls this on each
    /// savepoint event to decide whether `dolt_*` sees the uncommitted sets.
    /// Savepoint matrix (doltlite #592–#616): staging is branch-scoped and
    /// survives SQL COMMIT/ROLLBACK, entering a savepoint seals it, and
    /// RELEASE lifts the seal again — the savepoint is gone, so staging is
    /// visible and untouched.
    pub fn savepoint_sealed(&self, sql_state: &str) -> bool {
        matches!(sql_state, "SAVEPOINT" | "ROLLBACK TO")
    }

    /// S4: does the SQL event leave staging intact? COMMIT/ROLLBACK/RELEASE
    /// must not touch the working/staged sets (see savepoint matrix
    /// #592–#616); only ROLLBACK TO rewinds staging to the savepoint state.
    pub fn preserve_staging_on(&self, event: &str) -> bool {
        matches!(event, "COMMIT" | "ROLLBACK" | "RELEASE")
    }
}

/// `VcStore` as a `VcRead` provider: the engine glue reads the live session
/// through this instead of reaching into store fields. Uncommitted snapshots
/// have no commit ids, so `table_rows` serves them as empty until O4 records
/// real snapshots; metadata (log, diff flags, schemas) is exact today.
impl VcRead for VcStore {
    fn parents(&self, id: &CommitId) -> VersionResult<Vec<CommitId>> {
        self.commits
            .get_commit(id)
            .map(|c| c.parents)
            .ok_or_else(|| VersionError::CommitNotFound(id.to_hex()))
    }

    fn commit_view(&self, id: &CommitId) -> VersionResult<VcCommitView> {
        self.commits
            .get_commit(id)
            .map(|c| VcCommitView {
                id: *id,
                name: c.meta.name,
                email: c.meta.email,
                message: c.meta.message,
                timestamp: c.meta.timestamp,
            })
            .ok_or_else(|| VersionError::CommitNotFound(id.to_hex()))
    }

    fn head(&self) -> VersionResult<CommitId> {
        self.detached
            .or_else(|| self.head_commit())
            .ok_or_else(|| VersionError::BranchNotFound(self.head.clone()))
    }

    fn head_label(&self) -> String {
        self.active_branch().unwrap_or("HEAD").to_string()
    }

    fn resolve(&self, spec: &str) -> VersionResult<CommitId> {
        VcStore::resolve(self, spec)
    }

    fn table_columns(&self, table: &str) -> Option<Vec<String>> {
        let tip = self.detached.or_else(|| self.head_commit())?;
        self.snapshots
            .get(&tip)?
            .get(table)
            .map(|s| s.columns.clone())
    }

    fn table_pk(&self, table: &str) -> Option<Vec<String>> {
        let tip = self.detached.or_else(|| self.head_commit())?;
        let pk = self.snapshots.get(&tip)?.get(table)?.pk.clone();
        if pk.is_empty() {
            None
        } else {
            Some(pk)
        }
    }

    fn table_rows(&self, table: &str, at: &CommitId) -> Option<Vec<VcRow>> {
        self.snapshots.get(at)?.get(table).map(|s| s.rows.clone())
    }

    fn table_schema_sql(&self, table: &str, at: &CommitId) -> Option<String> {
        self.snapshots
            .get(at)?
            .get(table)
            .map(|s| s.schema_sql.clone())
    }

    fn tables(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tables.iter().cloned().collect();
        for per_commit in self.snapshots.values() {
            for name in per_commit.keys() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        names.sort();
        names
    }

    fn commit_total(&self) -> usize {
        self.commits.entries().len()
    }

    fn staged_tables(&self) -> Vec<String> {
        self.staging.staged_tables()
    }

    fn working_tables(&self) -> Vec<String> {
        self.staging.working_tables()
    }

    fn has_snapshot(&self, id: &CommitId) -> bool {
        use crate::vtab_log::{staged_id, working_id};
        if *id == working_id() {
            !self.staging.working_tables().is_empty()
        } else if *id == staged_id() {
            !self.staging.staged_tables().is_empty()
        } else {
            self.snapshots.contains_key(id)
        }
    }
}

/// Branch-scoped table names, split into a working set and a staged set.
///
/// A table lives in at most one set at a time: `stage` moves it from working
/// to staged, `unstage` moves it back, `discard` drops it from both.
#[derive(Debug, Default)]
pub struct StagingSet {
    working: HashSet<String>,
    staged: HashSet<String>,
}

impl StagingSet {
    pub fn new() -> Self {
        StagingSet {
            working: HashSet::new(),
            staged: HashSet::new(),
        }
    }

    pub fn stage(&mut self, table: &str) {
        self.working.remove(table);
        self.staged.insert(table.to_string());
    }

    pub fn unstage(&mut self, table: &str) {
        self.staged.remove(table);
        self.working.insert(table.to_string());
    }

    pub fn discard(&mut self, table: &str) {
        self.staged.remove(table);
        self.working.remove(table);
    }

    /// Insert directly into the working set without staging. Used by the
    /// CREATE TABLE hook so a new table starts as an uncommitted change.
    pub fn stage_working(&mut self, table: &str) {
        self.working.insert(table.to_string());
    }

    pub fn stage_all(&mut self, tables: &[String]) {
        for table in tables {
            self.stage(table);
        }
    }

    pub fn has_staged(&self) -> bool {
        !self.staged.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.working.is_empty()
    }

    pub fn staged_tables(&self) -> Vec<String> {
        let mut names: Vec<String> = self.staged.iter().cloned().collect();
        names.sort();
        names
    }

    pub fn working_tables(&self) -> Vec<String> {
        let mut names: Vec<String> = self.working.iter().cloned().collect();
        names.sort();
        names
    }

    pub fn clear_staged(&mut self) {
        self.staged.clear();
    }

    pub fn clear_all(&mut self) {
        self.staged.clear();
        self.working.clear();
    }

    /// Drop working tables the store does not know about — `dolt_clean` only
    /// removes untracked tables, leaving tracked-but-modified ones (staged or
    /// not) alone. Both sets are filtered so a stale staged name cannot survive
    /// a clean; by construction staging only contains tracked tables, so the
    /// staged pass is normally a no-op.
    pub fn discard_untracked(&mut self, tracked: &HashSet<String>) {
        self.working.retain(|table| tracked.contains(table));
        self.staged.retain(|table| tracked.contains(table));
    }

    /// Rows for `dolt_status`. `states` carries caller-known `deleted` and
    /// `renamed` labels the name sets cannot infer; a staged-only table with
    /// no caller state defaults to `new table`, everything else to `modified`.
    pub fn status(&self, states: &HashMap<String, TableState>) -> Vec<StatusRow> {
        let mut tables: Vec<String> = self.working.union(&self.staged).cloned().collect();
        tables.sort();
        tables
            .into_iter()
            .map(|table| {
                let staged = self.staged.contains(&table);
                let status = states.get(&table).copied().unwrap_or_else(|| {
                    if staged && !self.working.contains(&table) {
                        TableState::NewTable
                    } else {
                        TableState::Modified
                    }
                });
                StatusRow {
                    table,
                    staged,
                    status,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn staging_stage_unstage_discard() {
        let mut s = StagingSet::new();
        s.stage("t1");
        assert!(s.has_staged());
        assert_eq!(s.staged_tables(), vec!["t1".to_string()]);
        s.unstage("t1");
        assert!(!s.has_staged());
        assert_eq!(s.working_tables(), vec!["t1".to_string()]);
        s.discard("t1");
        assert!(s.is_empty());
    }

    #[test]
    fn staging_status_defaults() {
        let mut s = StagingSet::new();
        s.stage("new_t");
        s.stage("mod_t");
        s.unstage("mod_t");
        let rows = s.status(&HashMap::new());
        assert_eq!(rows.len(), 2);
        let new_row = rows.iter().find(|r| r.table == "new_t").unwrap();
        assert!(new_row.staged);
        assert_eq!(new_row.status.to_string(), "new table");
        let mod_row = rows.iter().find(|r| r.table == "mod_t").unwrap();
        assert!(!mod_row.staged);
        assert_eq!(mod_row.status.to_string(), "modified");
    }

    #[test]
    fn staging_status_honors_caller_state() {
        let mut s = StagingSet::new();
        s.stage("gone_t");
        let mut states = HashMap::new();
        states.insert("gone_t".to_string(), TableState::Deleted);
        let rows = s.status(&states);
        assert_eq!(rows[0].status.to_string(), "deleted");
    }

    #[test]
    fn staging_untracked_discard_keeps_only_tracked() {
        let mut s = StagingSet::new();
        let mut tracked = HashSet::new();
        tracked.insert("staged_t".to_string());
        tracked.insert("tracked_working_t".to_string());
        s.stage("staged_t");
        s.stage("untracked_staged_t");
        s.unstage("tracked_working_t");
        s.unstage("untracked_t");
        s.discard_untracked(&tracked);
        assert_eq!(s.working_tables(), vec!["tracked_working_t".to_string()]);
        assert_eq!(s.staged_tables(), vec!["staged_t".to_string()]);
    }

    #[test]
    fn track_table_lands_in_working_set() {
        let mut s = VcStore::new("main");
        s.track_table("t1");
        let rows = s.status();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].table, "t1");
        assert!(!rows[0].staged);
    }

    #[test]
    fn add_all_stages_everything() {
        let mut s = VcStore::new("main");
        s.track_table("t1");
        s.track_table("t2");
        s.add_all().unwrap();
        let rows = s.status();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.staged));
    }

    #[test]
    fn clean_keeps_tracked_modified_table_after_reset_soft() {
        let mut s = configured_store();
        s.dolt_add(&["t1"]).unwrap();
        s.reset_soft().unwrap();
        s.clean().unwrap();
        assert!(s.status().iter().any(|r| r.table == "t1" && !r.staged));
    }

    #[test]
    fn clean_drops_untracked_working_tables() {
        let mut s = VcStore::new("main");
        s.staging.stage_working("ghost");
        s.track_table("t1");
        s.clean().unwrap();
        let rows = s.status();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].table, "t1");
    }

    #[test]
    fn detached_guards_mutating_ops() {
        let mut s = configured_store();
        s.open_detached(s.head_commit().unwrap());
        assert_eq!(
            s.dolt_add(&["t1"]).unwrap_err().to_string(),
            "cannot write in detached HEAD state"
        );
        assert_eq!(
            s.dolt_commit("x", None, false, false)
                .unwrap_err()
                .to_string(),
            "cannot write in detached HEAD state"
        );
        assert_eq!(
            s.create_branch("dev").unwrap_err().to_string(),
            "cannot write in detached HEAD state"
        );
        assert!(s.active_branch().is_none());
        // Checkout reattaches and unblocks writes.
        s.checkout("main").unwrap();
        assert!(s.active_branch().is_some());
        assert!(s.guard_write().is_ok());
    }

    #[test]
    fn detached_head_resolves_to_pinned_snapshot() {
        let mut s = configured_store();
        let tip = s.head_commit().unwrap();
        s.open_detached(tip);
        assert_eq!(s.resolve("HEAD").unwrap(), tip);
    }

    #[test]
    fn commit_force_bypasses_violations() {
        let mut s = configured_store();
        s.violations.push(Violation {
            table: "t1".to_string(),
            kind: crate::constraints::ViolationKind::Unique,
            row_pk: Vec::new(),
            detail: String::new(),
        });
        s.dolt_add(&["t1"]).unwrap();
        s.set_now(2);
        assert_eq!(
            s.dolt_commit("second", None, false, false)
                .unwrap_err()
                .to_string(),
            "cannot commit: constraint violations remain"
        );
        s.dolt_add(&["t1"]).unwrap();
        s.set_now(3);
        assert!(s.dolt_commit("second", None, false, true).is_ok());
    }

    #[test]
    fn commit_force_never_bypasses_conflicts() {
        let mut s = configured_store();
        s.conflicts.push(ConflictEntry {
            table: "t1".to_string(),
            pk: Vec::new(),
            base: None,
            ours: None,
            theirs: None,
            kind: crate::conflicts::ConflictKind::Rows,
            ours_schema: None,
            theirs_schema: None,
        });
        s.dolt_add(&["t1"]).unwrap();
        s.set_now(2);
        assert_eq!(
            s.dolt_commit("second", None, false, true)
                .unwrap_err()
                .to_string(),
            "cannot commit: unresolved merge conflicts"
        );
    }

    #[test]
    fn tag_lifecycle() {
        let mut s = configured_store();
        let tip = s.head_commit().unwrap();
        s.create_tag("v1", tip).unwrap();
        assert_eq!(s.resolve("v1").unwrap(), tip);
        s.delete_tag("v1").unwrap();
        assert_eq!(
            s.delete_tag("v1").unwrap_err().to_string(),
            "tag not found: v1"
        );
    }

    #[test]
    fn resolve_working_staged_is_unresolvable() {
        let s = configured_store();
        assert_eq!(
            s.resolve("WORKING").unwrap_err().to_string(),
            "invalid revision spec: 'WORKING'"
        );
        assert_eq!(
            s.resolve("STAGED").unwrap_err().to_string(),
            "invalid revision spec: 'STAGED'"
        );
    }

    #[test]
    fn resolve_working_staged_while_sets_live() {
        use crate::vtab_log::{staged_id, working_id};
        let mut s = VcStore::new("main");
        s.track_table("t1");
        assert_eq!(s.resolve("WORKING").unwrap(), working_id());
        assert_eq!(
            s.resolve("STAGED").unwrap_err().to_string(),
            "invalid revision spec: 'STAGED'"
        );
        s.dolt_add(&["t1"]).unwrap();
        assert_eq!(s.resolve("STAGED").unwrap(), staged_id());
        assert_eq!(
            s.resolve("WORKING").unwrap_err().to_string(),
            "invalid revision spec: 'WORKING'"
        );
    }

    #[test]
    fn pending_resolve_survives_writeback_failure() {
        use crate::conflicts::ConflictEntry;
        use crate::conflicts::ConflictKind;
        let mut s = configured_store();
        // Simulate a merge that recorded a conflict.
        s.conflicts.push(ConflictEntry {
            table: "t1".to_string(),
            pk: vec![crate::vtab_log::VcValue::Integer(1)],
            base: Some(crate::vtab_log::VcRow::new(vec![
                crate::vtab_log::VcValue::Integer(1),
                crate::vtab_log::VcValue::Text("b".into()),
            ])),
            ours: Some(crate::vtab_log::VcRow::new(vec![
                crate::vtab_log::VcValue::Integer(1),
                crate::vtab_log::VcValue::Text("o".into()),
            ])),
            theirs: Some(crate::vtab_log::VcRow::new(vec![
                crate::vtab_log::VcValue::Integer(1),
                crate::vtab_log::VcValue::Text("t".into()),
            ])),
            kind: ConflictKind::Rows,
            ours_schema: None,
            theirs_schema: None,
        });
        // Resolve the conflict so it lands in pending_resolve.
        s.resolve_conflict(
            "t1",
            &[crate::vtab_log::VcValue::Integer(1)],
            crate::conflicts::ResolveSide::Ours,
        )
        .unwrap();
        assert!(s.conflicts.is_empty());
        // Drain the pending resolve and dirty set (mimics take before write-back).
        let resolutions = s.take_pending_resolve();
        let changes = s.take_work_changes();
        assert!(!resolutions.is_empty());
        // Simulate a write-back failure: restore the drained state.
        let dirty_names: Vec<String> = changes.iter().map(|(t, _)| t.clone()).collect();
        s.restore_pending_resolve(resolutions);
        s.restore_dirty_work(dirty_names);
        // Both must be retryable: pending_resolve is repopulated and dirty_work
        // is set so the next take_work_changes call returns the same tables.
        assert!(!s.pending_resolve.is_empty());
        let retry = s.take_work_changes();
        assert_eq!(retry.len(), changes.len());
        // The restored pending resolve drains cleanly on retry.
        let retry_res = s.take_pending_resolve();
        assert_eq!(retry_res.len(), 1);
    }
}
