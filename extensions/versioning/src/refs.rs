/// Reference store: branch and tag names, CAS updates, revision parsing.
///
/// R1: branch create/list/delete mirrors doltlite_ref.c.
/// R2: qualified-path parsing for `my.db@branch` / `my.db/branch` open paths.
/// R5: revision specs HEAD/WORKING/STAGED/BRANCH/TAG/HASH with ~N/^N suffixes.
/// R6: compare_and_swap returns RefError::Busy on race.

use std::collections::HashMap;

use crate::model::{CommitId, VersionError, VersionResult};

/// A namespace+name pair: `refs/heads/main` → RefName { ns: "heads", name: "main" }.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RefName {
    pub ns: String,
    pub name: String,
}

impl RefName {
    pub fn branch(name: &str) -> Self {
        RefName {
            ns: "heads".to_string(),
            name: name.to_string(),
        }
    }

    pub fn tag(name: &str) -> Self {
        RefName {
            ns: "tags".to_string(),
            name: name.to_string(),
        }
    }

    pub fn full_path(&self) -> String {
        format!("refs/{}/{}", self.ns, self.name)
    }
}

impl std::fmt::Display for RefName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.ns, self.name)
    }
}

/// A parsed revision specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revision {
    /// The current HEAD of the active branch.
    Head,
    /// The working set (uncommitted changes).
    Working,
    /// The staging area.
    Staged,
    /// A named branch or tag.
    Branch(String),
    /// An explicit 40-char hex commit hash.
    Hash(String),
    /// Nth ancestor via ~N (first-parent walk).
    Ancestor(Box<Revision>, u32),
    /// Nth parent via ^N.
    Parent(Box<Revision>, u32),
    /// Range `a..b` — commits reachable from b but not from a.
    Range(Box<Revision>, Box<Revision>),
    /// Triple range `a...b` — merge-base of a and b to right.
    SymmetricDifference(Box<Revision>, Box<Revision>),
}

/// Parse a revision string like `HEAD~3`, `main`, `a..b`, `a...b`.
pub fn parse_revision(s: &str) -> VersionResult<Revision> {
    // Try triple range first: a...b
    if let Some((left, right)) = s.split_once("...") {
        return Ok(Revision::SymmetricDifference(
            Box::new(parse_revision(left)?),
            Box::new(parse_revision(right)?,
        ));
    }
    // Try double range: a..b
    if let Some((left, right)) = s.split_once("..") {
        return Ok(Revision::Range(
            Box::new(parse_revision(left)?),
            Box::new(parse_revision(right)?,
        ));
    }
    // Try ^N suffix
    if let Some((base, n)) = s.rsplit_once('^') {
        let n: u32 = n.parse().map_err(|_| {
            VersionError::InvalidRevisionSpec(s.to_string())
        })?;
        return Ok(Revision::Parent(Box::new(parse_revision(base)?), n));
    }
    // Try ~N suffix
    if let Some((base, n)) = s.rsplit_once('~') {
        let n: u32 = n.parse().map_err(|_| {
            VersionError::InvalidRevisionSpec(s.to_string())
        })?;
        return Ok(Revision::Ancestor(Box::new(parse_revision(base)?), n));
    }
    match s {
        "HEAD" | "head" => Ok(Revision::Head),
        "WORKING" | "working" => Ok(Revision::Working),
        "STAGED" | "staged" => Ok(Revision::Staged),
        _ => {
            // 40-char hex → hash, else → branch name
            if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) {
                Ok(Revision::Hash(s.to_string()))
            } else {
                Ok(Revision::Branch(s.to_string()))
            }
        }
    }
}

/// Parse a qualified open path: `my.db@branch`, `my.db/branch`, etc.
/// Returns (file_path, Revision).
pub fn parse_qualified_path(path: &str) -> VersionResult<(String, Revision)> {
    // Try @branch syntax
    if let Some((file, rev)) = path.rsplit_once('@') {
        return Ok((file.to_string(), parse_revision(rev)?));
    }
    // Try /branch syntax (only if it looks like a branch, not a file path)
    // For simplicity, treat the last path segment after the first / as a revision
    // if it doesn't contain a dot (suggesting a file extension).
    if let Some(slash_pos) = path.rfind('/') {
        let candidate = &path[slash_pos + 1..];
        if !candidate.contains('.') && !candidate.is_empty() {
            let file = path[..slash_pos].to_string();
            return Ok((file, parse_revision(candidate)?));
        }
    }
    // Plain file path → HEAD
    Ok((path.to_string(), Revision::Head))
}

/// Trait for storing and retrieving references.
pub trait RefStore {
    fn get(&self, name: &RefName) -> Option<CommitId>;
    fn set(&mut self, name: &RefName, id: CommitId);
    fn delete(&mut self, name: &RefName) -> bool;
    fn list(&self, ns: &str) -> Vec<(RefName, CommitId)>;
}

/// CAS result: Busy means another writer won the race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasResult {
    Ok,
    Busy,
}

/// In-memory reference store with CAS support.
pub struct MemRefStore {
    refs: HashMap<RefName, CommitId>,
}

impl MemRefStore {
    pub fn new() -> Self {
        MemRefStore {
            refs: HashMap::new(),
        }
    }

    /// Compare-and-swap: set `name` to `next` only if current == `expected`.
    ///
    /// R6: Race loser gets Busy. Stale tip never clobbers winner.
    pub fn compare_and_swap(
        &mut self,
        name: &RefName,
        expected: Option<CommitId>,
        next: CommitId,
    ) -> CasResult {
        let current = self.refs.get(name).copied();
        if current == expected {
            self.refs.insert(name.clone(), next);
            CasResult::Ok
        } else {
            CasResult::Busy
        }
    }
}

impl Default for MemRefStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RefStore for MemRefStore {
    fn get(&self, name: &RefName) -> Option<CommitId> {
        self.refs.get(name).copied()
    }

    fn set(&mut self, name: &RefName, id: CommitId) {
        self.refs.insert(name.clone(), id);
    }

    fn delete(&mut self, name: &RefName) -> bool {
        self.refs.remove(name).is_some()
    }

    fn list(&self, ns: &str) -> Vec<(RefName, CommitId)> {
        let mut entries: Vec<_> = self
            .refs
            .iter()
            .filter(|(k, _)| k.ns == ns)
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        entries.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_revision_head() {
        assert_eq!(parse_revision("HEAD").unwrap(), Revision::Head);
    }

    #[test]
    fn parse_revision_ancestor() {
        assert_eq!(
            parse_revision("HEAD~3").unwrap(),
            Revision::Ancestor(Box::new(Revision::Head), 3)
        );
    }

    #[test]
    fn parse_revision_range() {
        assert_eq!(
            parse_revision("a..b").unwrap(),
            Revision::Range(
                Box::new(Revision::Branch("a".into())),
                Box::new(Revision::Branch("b".into())),
            )
        );
    }

    #[test]
    fn parse_revision_symmetric() {
        assert_eq!(
            parse_revision("a...b").unwrap(),
            Revision::SymmetricDifference(
                Box::new(Revision::Branch("a".into())),
                Box::new(Revision::Branch("b".into())),
            )
        );
    }

    #[test]
    fn cas_race_returns_busy() {
        let mut store = MemRefStore::new();
        let name = RefName::branch("main");
        let c1 = CommitId([1u8; 20]);
        let c2 = CommitId([2u8; 20]);
        let c3 = CommitId([3u8; 20]);

        // First CAS succeeds (expected = None, current = None)
        assert_eq!(
            store.compare_and_swap(&name, None, c1),
            CasResult::Ok
        );
        // Second CAS fails (expected = None, but current = c1)
        assert_eq!(
            store.compare_and_swap(&name, None, c2),
            CasResult::Busy
        );
        // Third CAS succeeds (expected = c1, current = c1)
        assert_eq!(
            store.compare_and_swap(&name, Some(c1), c3),
            CasResult::Ok
        );
        assert_eq!(store.get(&name), Some(c3));
    }

    #[test]
    fn branch_lifecycle() {
        let mut store = MemRefStore::new();
        let main = RefName::branch("main");
        let dev = RefName::branch("dev");
        let c1 = CommitId([1u8; 20]);
        let c2 = CommitId([2u8; 20]);

        store.set(&main, c1);
        store.set(&dev, c2);

        assert_eq!(store.get(&main), Some(c1));
        assert_eq!(store.get(&dev), Some(c2));

        let branches = store.list("heads");
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].0.name, "dev");
        assert_eq!(branches[1].0.name, "main");

        assert!(store.delete(&dev));
        assert_eq!(store.get(&dev), None);
        assert_eq!(store.list("heads").len(), 1);
    }
}
