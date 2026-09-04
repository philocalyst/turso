//! Commit-history read path: `dolt_log` rows, range splitting, pushdown plan.
//!
//! L1. doltlite's `doltlite_log.c` walks newest-first from the session HEAD
//! (merges enqueue all parents) and filters `a..b` / `a...b` TVF slices.
//! Everything here is pure: rows come from a `VcRead` provider, so the engine
//! glue stays thin and tests run without `core/`.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::commit::{Commit, CommitStore};
use crate::model::{CommitId, RootHash, VersionError, VersionResult};
use crate::refs::RefStore;

/// Revision ids that never name a real commit. `resolve()` returns these for
/// the uncommitted snapshots; row readers serve the matching snapshot instead
/// of hitting the commit store. Documented here so O4 replaces them with
/// content hashes without changing callers.
pub const WORKING_SENTINEL: [u8; 20] = [0xAA; 20];
pub const STAGED_SENTINEL: [u8; 20] = [0xAB; 20];

pub fn working_id() -> CommitId {
    CommitId(WORKING_SENTINEL)
}

pub fn staged_id() -> CommitId {
    CommitId(STAGED_SENTINEL)
}

/// A single cell value in a versioned row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VcValue {
    Null,
    Integer(i64),
    /// IEEE-754 bits preserve the exact SQL REAL value while keeping rows
    /// hashable and totally ordered for deterministic snapshots.
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl VcValue {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            VcValue::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn real(value: f64) -> Self {
        VcValue::Real(value.to_bits())
    }

    pub fn as_real(&self) -> Option<f64> {
        match self {
            VcValue::Real(bits) => Some(f64::from_bits(*bits)),
            _ => None,
        }
    }
}

/// One versioned table row: values line up with the table's column order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VcRow {
    pub values: Vec<VcValue>,
}

impl VcRow {
    pub fn new(values: Vec<VcValue>) -> Self {
        VcRow { values }
    }
}

/// Commit metadata as the vtables render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcCommitView {
    pub id: CommitId,
    pub name: String,
    pub email: String,
    pub message: String,
    pub timestamp: i64,
}

/// What `dolt_log` renders per row, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub hash: String,
    pub committer: String,
    pub email: String,
    pub date: String,
    pub message: String,
    pub revision: String,
}

/// Source of version history and snapshots. The engine implements this over
/// the live `VcStore`; tests use `MemVcRead` below. Row snapshots arrive with
/// O4; until then `table_rows` returns `None` and callers emit empty diffs.
pub trait VcRead {
    fn parents(&self, id: &CommitId) -> VersionResult<Vec<CommitId>>;
    fn commit_view(&self, id: &CommitId) -> VersionResult<VcCommitView>;
    fn head(&self) -> VersionResult<CommitId>;
    fn head_label(&self) -> String;
    fn resolve(&self, spec: &str) -> VersionResult<CommitId>;
    fn table_columns(&self, table: &str) -> Option<Vec<String>>;
    fn table_pk(&self, table: &str) -> Option<Vec<String>> {
        let _ = table;
        None
    }
    fn table_rows(&self, table: &str, at: &CommitId) -> Option<Vec<VcRow>>;
    fn table_schema_sql(&self, table: &str, at: &CommitId) -> Option<String>;
    fn tables(&self) -> Vec<String>;
    /// Commits reachable for counting without building rows.
    fn commit_total(&self) -> usize;
    /// Tables with uncommitted staged changes (names only; O4 adds content).
    fn staged_tables(&self) -> Vec<String> {
        Vec::new()
    }
    /// Tables with uncommitted working changes (names only; O4 adds content).
    fn working_tables(&self) -> Vec<String> {
        Vec::new()
    }
    /// Whether an uncommitted snapshot (`WORKING`/`STAGED` sentinel) exists.
    fn has_snapshot(&self, id: &CommitId) -> bool {
        let _ = id;
        false
    }
}

/// Rows for `SELECT * FROM dolt_log` (bare form, `range = None`) or
/// `SELECT * FROM dolt_log('a..b')` / `('a...b')` / `('rev')`.
///
/// Newest-first breadth-first walk from the resolved tip; merges enqueue all
/// parents. `a..b` keeps commits reachable from `b` but not from `a`;
/// `a...b` keeps commits reachable from `b` but not from the merge base.
/// The uncommitted snapshots have no history, so `WORKING`/`STAGED` resolve
/// to zero rows rather than an error.
pub fn log_rows(provider: &dyn VcRead, range: Option<&str>) -> VersionResult<Vec<LogRow>> {
    match range {
        None => {
            let tip = provider.head()?;
            let label = provider.head_label();
            walk_newest_first(provider, tip, &label)
        }
        Some(spec) => {
            let revision = crate::refs::parse_revision(spec)?;
            log_rows_for_revision(provider, &revision, spec)
        }
    }
}

/// Declared schema, column order 1:1 with doltlite's `doltliteLogSchema`.
pub const DOLT_LOG_SCHEMA: &str =
    "CREATE TABLE dolt_log (commit_hash TEXT, committer TEXT, email TEXT, date TEXT, message TEXT, revision TEXT HIDDEN)";

/// Pushdown plan for `dolt_log`. Column 0 is `commit_hash`, column 5 the
/// hidden `revision` TVF argument. The log is newest-first, so only
/// `ORDER BY date DESC` is consumed.
pub fn plan_log(constraints: &[VcConstraint], order_by: &[VcOrderBy], row_estimate: u32) -> VcPlan {
    let mut omit = vec![false; constraints.len()];
    let mut argv = Vec::new();
    for (i, c) in constraints.iter().enumerate() {
        if !c.usable || c.op != VcOp::Eq {
            continue;
        }
        if c.column == 0 && !argv.contains(&0) {
            argv.push(0);
            omit[i] = true;
            return VcPlan::probe(1, "hash", omit, argv, 10.0, 1);
        }
        if c.column == 5 && !argv.contains(&5) {
            argv.push(5);
            omit[i] = true;
            return VcPlan::probe(2, "rev", omit, argv, 100.0, row_estimate);
        }
    }
    let order_consumed = order_by.len() == 1 && order_by[0].column == 3 && order_by[0].desc;
    VcPlan {
        idx_num: 0,
        idx_str: None,
        omit,
        argv,
        cost: 1000.0 + row_estimate as f64,
        rows: row_estimate,
        order_consumed,
    }
}

/// One commit by hash: the `commit_hash EQ` probe path. A hash lookup never
/// walks the graph, which is what makes the probe O(1) instead of O(N).
pub fn log_probe(provider: &dyn VcRead, hash: &str) -> VersionResult<Option<LogRow>> {
    let id =
        CommitId::from_hex(hash).map_err(|_| VersionError::CommitNotFound(hash.to_string()))?;
    let view = provider.commit_view(&id)?;
    Ok(Some(LogRow {
        hash: view.id.to_hex(),
        committer: view.name,
        email: view.email,
        date: view.timestamp.to_string(),
        message: view.message,
        revision: provider.head_label(),
    }))
}

/// Commit count without building rows. The `COUNT(*)` fast path reads this;
/// the engine cannot see aggregates in `best_index`, so the glue cannot call
/// it yet — the planner and counter stay as the tested contract for it.
pub fn log_count(provider: &dyn VcRead) -> usize {
    provider.commit_total()
}

/// Constant-time plan for `SELECT COUNT(*) FROM dolt_log` with no filter.
/// Pairs with `log_count`: the count needs only the total, never the walk.
pub fn plan_log_count(_commit_total: u32) -> VcPlan {
    VcPlan {
        idx_num: 3,
        idx_str: Some("count".to_string()),
        omit: Vec::new(),
        argv: Vec::new(),
        cost: 1.0,
        rows: 1,
        order_consumed: false,
    }
}

/// One usable constraint on a vtable column. Only equality is plannable;
/// anything else scans and lets the engine recheck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcConstraint {
    pub column: u32,
    pub op: VcOp,
    pub usable: bool,
}

/// Constraint operators the planners understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcOp {
    Eq,
    Other,
}

/// One `ORDER BY` term on a vtable column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcOrderBy {
    pub column: u32,
    pub desc: bool,
}

/// The planner's answer: which constraints the scan consumes (`omit`),
/// which filter arguments they map to (`argv`), and the cost estimates.
#[derive(Debug, Clone, PartialEq)]
pub struct VcPlan {
    pub idx_num: i32,
    pub idx_str: Option<String>,
    pub omit: Vec<bool>,
    pub argv: Vec<u32>,
    pub cost: f64,
    pub rows: u32,
    pub order_consumed: bool,
}

impl VcPlan {
    fn probe(
        idx_num: i32,
        name: &str,
        omit: Vec<bool>,
        argv: Vec<u32>,
        cost: f64,
        rows: u32,
    ) -> Self {
        VcPlan {
            idx_num,
            idx_str: Some(name.to_string()),
            omit,
            argv,
            cost,
            rows,
            order_consumed: false,
        }
    }
}

/// In-memory `VcRead` for tests: real commit and ref stores plus per-commit
/// table snapshots. Tests push the oldest commit first; reads are newest first.
#[derive(Default)]
pub struct MemVcRead {
    commits: crate::commit::MemCommitStore,
    refs: crate::refs::MemRefStore,
    order: Vec<CommitId>,
    head_branch: String,
    tables: HashMap<String, MemTable>,
    working: Option<HashMap<String, MemSnapshot>>,
    staged: Option<HashMap<String, MemSnapshot>>,
}

#[derive(Debug, Clone, Default)]
struct MemTable {
    snapshots: HashMap<CommitId, MemSnapshot>,
}

#[derive(Debug, Clone, Default)]
struct MemSnapshot {
    columns: Vec<String>,
    pk: Vec<String>,
    rows: Vec<VcRow>,
    schema_sql: String,
}

impl MemVcRead {
    pub fn new(head_branch: &str) -> Self {
        MemVcRead {
            head_branch: head_branch.to_string(),
            ..Default::default()
        }
    }

    /// Append a commit on top of `parents`; the branch tip follows.
    pub fn push_commit(
        &mut self,
        parents: Vec<CommitId>,
        name: &str,
        email: &str,
        message: &str,
        timestamp: i64,
    ) -> CommitId {
        let commit = Commit {
            parents,
            root: RootHash([0u8; 20]),
            meta: crate::commit::CommitMeta {
                name: name.to_string(),
                email: email.to_string(),
                message: message.to_string(),
                timestamp,
            },
        };
        let id = self.commits.put_commit(commit);
        self.order.push(id);
        self.refs
            .set(&crate::refs::RefName::branch(&self.head_branch), id);
        id
    }

    pub fn set_branch_tip(&mut self, branch: &str, at: CommitId) {
        self.refs.set(&crate::refs::RefName::branch(branch), at);
    }

    pub fn set_head_branch(&mut self, branch: &str) {
        self.head_branch = branch.to_string();
    }

    /// Store one table snapshot at one commit.
    pub fn put_snapshot(
        &mut self,
        table: &str,
        columns: &[&str],
        pk: &[&str],
        at: CommitId,
        rows: Vec<Vec<VcValue>>,
        schema_sql: &str,
    ) {
        let snap = MemSnapshot {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            pk: pk.iter().map(|s| s.to_string()).collect(),
            rows: rows.into_iter().map(VcRow::new).collect(),
            schema_sql: schema_sql.to_string(),
        };
        self.tables
            .entry(table.to_string())
            .or_default()
            .snapshots
            .insert(at, snap);
    }

    pub fn set_working(&mut self, tables: HashMap<String, MemSnapshotBuilder>) {
        self.working = Some(tables.into_iter().map(|(k, v)| (k, v.build())).collect());
    }

    pub fn set_staged(&mut self, tables: HashMap<String, MemSnapshotBuilder>) {
        self.staged = Some(tables.into_iter().map(|(k, v)| (k, v.build())).collect());
    }

    fn snapshot_at(&self, table: &str, at: &CommitId) -> Option<&MemSnapshot> {
        if *at == working_id() {
            return self.working.as_ref()?.get(table);
        }
        if *at == staged_id() {
            return self.staged.as_ref()?.get(table);
        }
        self.tables.get(table)?.snapshots.get(at)
    }
}

/// Test helper: builds one table snapshot without exposing `MemSnapshot`.
#[derive(Debug, Clone, Default)]
pub struct MemSnapshotBuilder {
    columns: Vec<String>,
    pk: Vec<String>,
    rows: Vec<VcRow>,
    schema_sql: String,
}

impl MemSnapshotBuilder {
    pub fn new(columns: &[&str], pk: &[&str], schema_sql: &str) -> Self {
        MemSnapshotBuilder {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            pk: pk.iter().map(|s| s.to_string()).collect(),
            rows: Vec::new(),
            schema_sql: schema_sql.to_string(),
        }
    }

    pub fn row(mut self, values: Vec<VcValue>) -> Self {
        self.rows.push(VcRow::new(values));
        self
    }

    fn build(self) -> MemSnapshot {
        MemSnapshot {
            columns: self.columns,
            pk: self.pk,
            rows: self.rows,
            schema_sql: self.schema_sql,
        }
    }
}

impl VcRead for MemVcRead {
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
        self.refs
            .get(&crate::refs::RefName::branch(&self.head_branch))
            .ok_or_else(|| VersionError::BranchNotFound(self.head_branch.clone()))
    }

    fn head_label(&self) -> String {
        self.head_branch.clone()
    }

    fn resolve(&self, spec: &str) -> VersionResult<CommitId> {
        if spec == "WORKING" || spec == "working" {
            return match self.working {
                Some(_) => Ok(working_id()),
                None => Err(VersionError::InvalidRevisionSpec(spec.to_string())),
            };
        }
        if spec == "STAGED" || spec == "staged" {
            return match self.staged {
                Some(_) => Ok(staged_id()),
                None => Err(VersionError::InvalidRevisionSpec(spec.to_string())),
            };
        }
        let revision = crate::refs::parse_revision(spec)?;
        let head = self.head().ok();
        crate::commit::resolve_revision(&self.commits, &self.refs, &revision, head, None, None)
    }

    fn table_columns(&self, table: &str) -> Option<Vec<String>> {
        self.tables
            .get(table)?
            .snapshots
            .values()
            .next()
            .map(|s| s.columns.clone())
    }

    fn table_pk(&self, table: &str) -> Option<Vec<String>> {
        self.tables
            .get(table)?
            .snapshots
            .values()
            .next()
            .map(|s| s.pk.clone())
    }

    fn table_rows(&self, table: &str, at: &CommitId) -> Option<Vec<VcRow>> {
        self.snapshot_at(table, at).map(|s| s.rows.clone())
    }

    fn table_schema_sql(&self, table: &str, at: &CommitId) -> Option<String> {
        self.snapshot_at(table, at).map(|s| s.schema_sql.clone())
    }

    fn tables(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tables.keys().cloned().collect();
        names.sort();
        names
    }

    fn commit_total(&self) -> usize {
        self.order.len()
    }

    fn has_snapshot(&self, id: &CommitId) -> bool {
        if *id == working_id() {
            self.working.is_some()
        } else if *id == staged_id() {
            self.staged.is_some()
        } else {
            false
        }
    }
}

/// Dispatch a parsed TVF revision to the matching walk.
fn log_rows_for_revision(
    provider: &dyn VcRead,
    revision: &crate::refs::Revision,
    spec: &str,
) -> VersionResult<Vec<LogRow>> {
    use crate::refs::Revision;
    match revision {
        Revision::Range(left, right) => {
            reject_nested(left, spec)?;
            reject_nested(right, spec)?;
            let right_id = resolve_single(provider, right)?;
            if is_snapshot_id(&right_id) {
                return Ok(Vec::new());
            }
            let left_id = resolve_single(provider, left)?;
            let excluded = reachable_set(provider, left_id)?;
            walk_newest_first_excluding(provider, right_id, &excluded, spec)
        }
        Revision::SymmetricDifference(left, right) => {
            reject_nested(left, spec)?;
            reject_nested(right, spec)?;
            let left_id = resolve_single(provider, left)?;
            let right_id = resolve_single(provider, right)?;
            if is_snapshot_id(&right_id) {
                return Ok(Vec::new());
            }
            let excluded = if is_snapshot_id(&left_id) {
                HashSet::new()
            } else {
                let adapter = ReadAdapter(provider);
                let base = crate::commit::merge_base(&adapter, left_id, right_id)?;
                match base {
                    Some(b) => reachable_set(provider, b)?,
                    None => HashSet::new(),
                }
            };
            walk_newest_first_excluding(provider, right_id, &excluded, spec)
        }
        single => {
            let id = resolve_single(provider, single)?;
            if is_snapshot_id(&id) {
                return Ok(Vec::new());
            }
            walk_newest_first(provider, id, spec)
        }
    }
}

/// Nested ranges (`a..b..c`) collapse silently if resolved piecemeal, so
/// they fail loudly instead. Tags display as bare names and re-parse as
/// branches; the resolve fallback (branch, then tag) still finds them.
pub(crate) fn reject_nested(revision: &crate::refs::Revision, spec: &str) -> VersionResult<()> {
    use crate::refs::Revision;
    match revision {
        Revision::Range(_, _) | Revision::SymmetricDifference(_, _) => {
            Err(VersionError::InvalidRevisionSpec(spec.to_string()))
        }
        Revision::Ancestor(base, _) | Revision::Parent(base, _) => reject_nested(base, spec),
        _ => Ok(()),
    }
}

/// Resolve one non-range revision endpoint against the provider.
fn resolve_single(
    provider: &dyn VcRead,
    revision: &crate::refs::Revision,
) -> VersionResult<CommitId> {
    provider.resolve(&revision.to_string())
}

/// The uncommitted snapshots carry no history.
fn is_snapshot_id(id: &CommitId) -> bool {
    *id == working_id() || *id == staged_id()
}

/// Every commit reachable from `tip` through all parents, inclusive.
/// A missing commit is an error, never a silent truncation.
fn reachable_set(provider: &dyn VcRead, tip: CommitId) -> VersionResult<HashSet<CommitId>> {
    let mut seen = HashSet::new();
    let mut stack = vec![tip];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        stack.extend(provider.parents(&id)?);
    }
    Ok(seen)
}

/// Breadth-first walk from `tip`, newest commit first. Children queue behind
/// their parent ordered by timestamp (newest first, hex tiebreak), so linear
/// history reads exactly newest-to-oldest and merges stay deterministic.
fn walk_newest_first(
    provider: &dyn VcRead,
    tip: CommitId,
    label: &str,
) -> VersionResult<Vec<LogRow>> {
    walk_newest_first_excluding(provider, tip, &HashSet::new(), label)
}

fn walk_newest_first_excluding(
    provider: &dyn VcRead,
    tip: CommitId,
    excluded: &HashSet<CommitId>,
    label: &str,
) -> VersionResult<Vec<LogRow>> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([tip]);
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        let view = provider.commit_view(&id)?;
        if !excluded.contains(&id) {
            rows.push(LogRow {
                hash: view.id.to_hex(),
                committer: view.name,
                email: view.email,
                date: view.timestamp.to_string(),
                message: view.message,
                revision: label.to_string(),
            });
        }
        let mut parents = provider.parents(&id)?;
        parents.sort_by_cached_key(|p| {
            let ts = provider
                .commit_view(p)
                .map(|v| v.timestamp)
                .unwrap_or(i64::MIN);
            (std::cmp::Reverse(ts), p.to_hex())
        });
        queue.extend(parents);
    }
    Ok(rows)
}

/// First-parent commit chain from `tip` back to the root, newest first.
/// A missing commit is an error; a revisited commit ends the walk instead of
/// looping, matching `commit::ancestors`. Walks terminate by contract while
/// `generation`/`merge_base` report cycles as errors: listing must end,
/// math must not pretend a cyclic graph is acyclic.
pub fn commit_chain(provider: &dyn VcRead, tip: CommitId) -> VersionResult<Vec<CommitId>> {
    if is_snapshot_id(&tip) {
        return Ok(vec![tip]);
    }
    let mut seen = HashSet::new();
    let mut chain = Vec::new();
    let mut current = Some(tip);
    while let Some(id) = current {
        if !seen.insert(id) {
            break;
        }
        let parents = provider.parents(&id)?;
        chain.push(id);
        current = parents.into_iter().next();
    }
    Ok(chain)
}

/// Lowest common ancestor of two commits through a provider.
pub fn merge_base_of(
    provider: &dyn VcRead,
    left: CommitId,
    right: CommitId,
) -> VersionResult<Option<CommitId>> {
    let adapter = ReadAdapter(provider);
    crate::commit::merge_base(&adapter, left, right)
}

/// Adapts a `VcRead` provider to the commit-store interface so range code
/// reuses `merge_base` instead of forking it. Writes are never used; `put`
/// hashes without storing.
struct ReadAdapter<'a>(&'a dyn VcRead);

impl<'a> CommitStore for ReadAdapter<'a> {
    fn get_commit(&self, id: &CommitId) -> Option<Commit> {
        let view = self.0.commit_view(id).ok()?;
        let parents = self.0.parents(id).ok()?;
        Some(Commit {
            parents,
            root: RootHash([0u8; 20]),
            meta: crate::commit::CommitMeta {
                name: view.name,
                email: view.email,
                message: view.message,
                timestamp: view.timestamp,
            },
        })
    }

    fn put_commit(&mut self, commit: Commit) -> CommitId {
        crate::commit::hash_commit(&commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain() -> (MemVcRead, CommitId, CommitId, CommitId) {
        let mut p = MemVcRead::new("main");
        let c0 = p.push_commit(vec![], "Ada", "ada@x.com", "first", 100);
        let c1 = p.push_commit(vec![c0], "Ada", "ada@x.com", "second", 200);
        let c2 = p.push_commit(vec![c1], "Bea", "bea@x.com", "third", 300);
        (p, c0, c1, c2)
    }

    #[test]
    fn log_schema_column_order_matches_doltlite() {
        assert_eq!(
            DOLT_LOG_SCHEMA,
            "CREATE TABLE dolt_log (commit_hash TEXT, committer TEXT, email TEXT, date TEXT, message TEXT, revision TEXT HIDDEN)"
        );
    }

    #[test]
    fn log_bare_walks_newest_first() {
        let (p, c0, c1, c2) = chain();
        let rows = log_rows(&p, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].hash, c2.to_hex());
        assert_eq!(rows[1].hash, c1.to_hex());
        assert_eq!(rows[2].hash, c0.to_hex());
        assert_eq!(rows[0].committer, "Bea");
        assert_eq!(rows[0].email, "bea@x.com");
        assert_eq!(rows[0].date, "300");
        assert_eq!(rows[0].message, "third");
        assert_eq!(rows[0].revision, "main");
    }

    #[test]
    fn log_two_dot_excludes_left_reachable() {
        let (mut p, c0, _c1, c2) = chain();
        p.set_branch_tip("dev", c2);
        p.set_head_branch("main");
        let rows = log_rows(&p, Some(&format!("{}..dev", c0.to_hex()))).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].hash, c2.to_hex());
    }

    #[test]
    fn log_three_dot_excludes_merge_base() {
        let mut p = MemVcRead::new("main");
        let c0 = p.push_commit(vec![], "Ada", "a@x", "base", 100);
        p.set_branch_tip("main", c0);
        let c1 = p.push_commit(vec![c0], "Ada", "a@x", "left", 200);
        p.set_branch_tip("left", c1);
        p.set_branch_tip("main", c0);
        let c2 = p.push_commit(vec![c0], "Bea", "b@x", "right", 300);
        p.set_branch_tip("right", c2);
        p.set_branch_tip("main", c0);
        p.set_head_branch("right");
        let rows = log_rows(&p, Some("left...right")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hash, c2.to_hex());
    }

    #[test]
    fn log_single_rev_walks_from_tip() {
        let (p, c0, c1, _) = chain();
        let rows = log_rows(&p, Some(&format!("{}~1", c1.to_hex()))).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hash, c0.to_hex());
    }

    #[test]
    fn log_working_has_no_history() {
        let (mut p, _, _, _) = chain();
        p.set_working(HashMap::new());
        let rows = log_rows(&p, Some("WORKING")).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn log_missing_head_errors_branch_not_found() {
        let p = MemVcRead::new("ghost");
        assert_eq!(
            log_rows(&p, None).unwrap_err().to_string(),
            "branch not found: ghost"
        );
    }

    #[test]
    fn log_bad_spec_errors_invalid_revision() {
        let (p, _, _, _) = chain();
        assert_eq!(
            log_rows(&p, Some("HEAD~x")).unwrap_err().to_string(),
            "invalid revision spec: 'HEAD~x'"
        );
    }

    #[test]
    fn log_missing_commit_errors_commit_not_found() {
        let (mut p, _, _, c2) = chain();
        p.commits = crate::commit::MemCommitStore::new();
        let err = log_rows(&p, None).unwrap_err().to_string();
        assert!(err.starts_with("commit not found: "), "{err}");
        let _ = c2;
    }

    #[test]
    fn plan_hash_probe_is_ologn() {
        let cs = vec![VcConstraint {
            column: 0,
            op: VcOp::Eq,
            usable: true,
        }];
        let plan = plan_log(&cs, &[], 100);
        assert_eq!(plan.idx_num, 1);
        assert_eq!(plan.idx_str.as_deref(), Some("hash"));
        assert_eq!(plan.omit, vec![true]);
        assert_eq!(plan.argv, vec![0]);
        assert_eq!(plan.rows, 1);
        assert!(plan.cost < 100.0);
    }

    #[test]
    fn plan_revision_probe() {
        let cs = vec![VcConstraint {
            column: 5,
            op: VcOp::Eq,
            usable: true,
        }];
        let plan = plan_log(&cs, &[], 100);
        assert_eq!(plan.idx_num, 2);
        assert_eq!(plan.omit, vec![true]);
    }

    #[test]
    fn plan_full_scan_consumes_date_desc_only() {
        let plan = plan_log(
            &[],
            &[VcOrderBy {
                column: 3,
                desc: true,
            }],
            50,
        );
        assert_eq!(plan.idx_num, 0);
        assert!(plan.order_consumed);
        assert_eq!(plan.rows, 50);
        let asc = plan_log(
            &[],
            &[VcOrderBy {
                column: 3,
                desc: false,
            }],
            50,
        );
        assert!(!asc.order_consumed);
    }

    #[test]
    fn plan_unusable_constraint_falls_back_to_scan() {
        let cs = vec![VcConstraint {
            column: 0,
            op: VcOp::Eq,
            usable: false,
        }];
        let plan = plan_log(&cs, &[], 7);
        assert_eq!(plan.idx_num, 0);
        assert_eq!(plan.omit, vec![false]);
        assert_eq!(plan.rows, 7);
    }

    #[test]
    fn plan_count_is_constant_time() {
        let plan = plan_log_count(100);
        assert_eq!(plan.cost, 1.0);
        assert_eq!(plan.rows, 1);
        assert_eq!(plan.idx_str.as_deref(), Some("count"));
    }

    #[test]
    fn log_count_needs_no_row_build() {
        let (p, _, _, _) = chain();
        assert_eq!(log_count(&p), 3);
        let mut big = MemVcRead::new("main");
        let mut parent = None;
        for i in 0..100 {
            let parents = parent.into_iter().collect();
            parent = Some(big.push_commit(parents, "A", "a@x", &format!("c{i}"), i));
        }
        assert_eq!(log_count(&big), 100);
    }

    #[test]
    fn plan_rejects_non_eq_constraints() {
        let cs = vec![VcConstraint {
            column: 0,
            op: VcOp::Other,
            usable: true,
        }];
        let plan = plan_log(&cs, &[], 9);
        assert_eq!(plan.idx_num, 0);
        assert_eq!(plan.omit, vec![false]);
    }

    #[test]
    fn log_nested_range_fails_loudly() {
        let (p, _, _, _) = chain();
        assert_eq!(
            log_rows(&p, Some("a..b..c")).unwrap_err().to_string(),
            "invalid revision spec: 'a..b..c'"
        );
    }

    #[test]
    fn log_tag_resolves_through_fallback() {
        let mut p = MemVcRead::new("main");
        let c0 = p.push_commit(vec![], "Ada", "a@x", "first", 100);
        p.refs.set(&crate::refs::RefName::tag("v1"), c0);
        let rows = log_rows(&p, Some("v1")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message, "first");
    }

    #[test]
    fn lowercase_snapshot_specs_resolve() {
        let (mut p, _, _, _) = chain();
        p.set_working(HashMap::new());
        assert_eq!(p.resolve("working").unwrap(), working_id());
        assert_eq!(
            p.resolve("staged").unwrap_err().to_string(),
            "invalid revision spec: 'staged'"
        );
    }

    #[test]
    fn probe_never_walks_the_graph() {
        struct NoWalk<'a> {
            inner: &'a MemVcRead,
        }
        impl VcRead for NoWalk<'_> {
            fn parents(&self, _id: &CommitId) -> VersionResult<Vec<CommitId>> {
                panic!("probe must not walk parents");
            }
            fn commit_view(&self, id: &CommitId) -> VersionResult<VcCommitView> {
                self.inner.commit_view(id)
            }
            fn head(&self) -> VersionResult<CommitId> {
                panic!("probe must not read head");
            }
            fn head_label(&self) -> String {
                "main".to_string()
            }
            fn resolve(&self, spec: &str) -> VersionResult<CommitId> {
                self.inner.resolve(spec)
            }
            fn table_columns(&self, _table: &str) -> Option<Vec<String>> {
                None
            }
            fn table_rows(&self, _table: &str, _at: &CommitId) -> Option<Vec<VcRow>> {
                None
            }
            fn table_schema_sql(&self, _table: &str, _at: &CommitId) -> Option<String> {
                None
            }
            fn tables(&self) -> Vec<String> {
                Vec::new()
            }
            fn commit_total(&self) -> usize {
                1
            }
        }
        let (p, _, _, c2) = chain();
        let guard = NoWalk { inner: &p };
        let row = log_probe(&guard, &c2.to_hex())
            .unwrap()
            .expect("tip resolves");
        assert_eq!(row.hash, c2.to_hex());
    }

    #[test]
    fn probe_returns_single_row_on_large_history() {
        let mut p = MemVcRead::new("main");
        let mut parent = None;
        for i in 0..100 {
            let parents = parent.into_iter().collect();
            parent = Some(p.push_commit(parents, "A", "a@x", &format!("c{i}"), i));
        }
        let tip = parent.unwrap();
        let cs = vec![VcConstraint {
            column: 0,
            op: VcOp::Eq,
            usable: true,
        }];
        let plan = plan_log(&cs, &[], 100);
        assert_eq!(plan.idx_num, 1);
        let row = log_probe(&p, &tip.to_hex()).unwrap().expect("tip resolves");
        assert_eq!(row.hash, tip.to_hex());
        assert_eq!(row.message, "c99");
        assert!(log_probe(&p, "0000000000000000000000000000000000000000")
            .unwrap_err()
            .to_string()
            .starts_with("commit not found: "));
    }
}
