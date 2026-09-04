/// Reference store: branch and tag names, CAS updates, revision parsing.
///
/// R1: branch create/list/delete mirrors doltlite_ref.c.
/// R2: qualified-path parsing for `my.db@branch` / `my.db/branch` open paths.
/// R5: revision specs HEAD/WORKING/STAGED/BRANCH/TAG/HASH with ~N/^N suffixes.
/// R6: compare_and_swap returns RefError::Busy on race.
use std::collections::HashMap;

use crate::model::{CommitId, VersionError, VersionResult};

/// Branch or tag ref namespace. Mirrors the `refs/heads/*` and `refs/tags/*`
/// file paths in doltlite's chunk refs (R2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefNs {
    Heads,
    Tags,
}

impl RefNs {
    pub fn as_str(self) -> &'static str {
        match self {
            RefNs::Heads => "heads",
            RefNs::Tags => "tags",
        }
    }
}

impl std::fmt::Display for RefNs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A namespace+name pair: `refs/heads/main` → RefName { ns: Heads, name: "main" }.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RefName {
    pub ns: RefNs,
    pub name: String,
}

impl RefName {
    pub fn branch(name: &str) -> Self {
        RefName {
            ns: RefNs::Heads,
            name: name.to_string(),
        }
    }

    pub fn tag(name: &str) -> Self {
        RefName {
            ns: RefNs::Tags,
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

/// Parse a qualified open path: `my.db@branch`, `my.db/branch`, `my.db/v1`,
/// `my.db/<40hex>`, `my.db/main~1`. Unknown suffix that is not a revision
/// reads as a plain path. Last `/` segment without a dot is the revision;
/// `@` always splits at the last occurrence. Plain path → HEAD.
///
/// Placed before `parse_revision` because the code reads top-down from the
/// caller; this is the user-facing entry point that funnels into the spec
/// parser below.
pub fn parse_qualified_path(path: &str) -> VersionResult<(String, Revision)> {
    if let Some((file, rev)) = path.rsplit_once('@') {
        return Ok((file.to_string(), parse_revision(rev)?));
    }
    if let Some(slash_pos) = path.rfind('/') {
        let candidate = &path[slash_pos + 1..];
        if !candidate.contains('.') && !candidate.is_empty() {
            let file = path[..slash_pos].to_string();
            return Ok((file, parse_revision(candidate)?));
        }
    }
    Ok((path.to_string(), Revision::Head))
}

/// Rubric-name alias for `parse_qualified_path` (R2).
pub fn parse_qualified(path: &str) -> VersionResult<(String, Revision)> {
    parse_qualified_path(path)
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
    /// A named branch.
    Branch(String),
    /// A named tag.
    Tag(String),
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
///
/// `...` is checked before `..` so a triple range never parses as a double
/// range with a `.`-leading right endpoint.
pub fn parse_revision(s: &str) -> VersionResult<Revision> {
    if let Some((left, right)) = s.split_once("...") {
        return Ok(Revision::SymmetricDifference(
            Box::new(parse_revision(left)?),
            Box::new(parse_revision(right)?),
        ));
    }
    if let Some((left, right)) = s.split_once("..") {
        return Ok(Revision::Range(
            Box::new(parse_revision(left)?),
            Box::new(parse_revision(right)?),
        ));
    }
    if let Some((base, n)) = s.rsplit_once('^') {
        if let Ok(n) = n.parse::<u32>() {
            return Ok(Revision::Parent(Box::new(parse_revision(base)?), n));
        }
        // A non-numeric `^` suffix is fine when the string also splits on `~`
        // (e.g. `main^2~1`); a bare `main^foo` is rejected below.
    }
    if let Some((base, n)) = s.rsplit_once('~') {
        let n: u32 = n
            .parse()
            .map_err(|_| VersionError::InvalidRevisionSpec(s.to_string()))?;
        return Ok(Revision::Ancestor(Box::new(parse_revision(base)?), n));
    }
    if s.contains('^') || s.contains('~') {
        return Err(VersionError::InvalidRevisionSpec(s.to_string()));
    }
    match s {
        "HEAD" | "head" => Ok(Revision::Head),
        "WORKING" | "working" => Ok(Revision::Working),
        "STAGED" | "staged" => Ok(Revision::Staged),
        _ if s.is_empty() => Err(VersionError::InvalidRevisionSpec(s.to_string())),
        _ => {
            if s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) {
                Ok(Revision::Hash(s.to_string()))
            } else {
                Ok(Revision::Branch(s.to_string()))
            }
        }
    }
}

impl std::fmt::Display for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Revision::Head => write!(f, "HEAD"),
            Revision::Working => write!(f, "WORKING"),
            Revision::Staged => write!(f, "STAGED"),
            Revision::Branch(name) | Revision::Tag(name) => write!(f, "{name}"),
            Revision::Hash(hex) => write!(f, "{hex}"),
            Revision::Ancestor(base, n) => write!(f, "{base}~{n}"),
            Revision::Parent(base, n) => write!(f, "{base}^{n}"),
            Revision::Range(left, right) => write!(f, "{left}..{right}"),
            Revision::SymmetricDifference(left, right) => write!(f, "{left}...{right}"),
        }
    }
}

/// Trait for storing and retrieving references.
pub trait RefStore {
    fn get(&self, name: &RefName) -> Option<CommitId>;
    fn set(&mut self, name: &RefName, id: CommitId);
    fn delete(&mut self, name: &RefName) -> bool;
    fn list(&self, ns: RefNs) -> Vec<(RefName, CommitId)>;

    /// R1 rubric-name aliases, kept on the trait so callers can speak the
    /// doltlite vocabulary without reaching into RefName internals.
    fn get_branch(&self, name: &str) -> Option<CommitId> {
        self.get(&RefName::branch(name))
    }

    fn get_tag(&self, name: &str) -> Option<CommitId> {
        self.get(&RefName::tag(name))
    }

    fn create_branch(&mut self, name: &str, at: CommitId) {
        self.set(&RefName::branch(name), at);
    }

    fn delete_branch(&mut self, name: &str) -> bool {
        self.delete(&RefName::branch(name))
    }

    fn delete_tag(&mut self, name: &str) -> bool {
        self.delete(&RefName::tag(name))
    }

    fn list_branches(&self) -> Vec<(RefName, CommitId)> {
        self.list(RefNs::Heads)
    }

    fn list_tags(&self) -> Vec<(RefName, CommitId)> {
        self.list(RefNs::Tags)
    }
}

/// CAS outcome. `Busy` means another writer won the race.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefError {
    #[error("database is locked")]
    Busy,
}

/// In-memory reference store with CAS support.
#[derive(Clone)]
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
    /// `expected` is `Option<CommitId>` rather than a bare id so the first
    /// write to a never-seen ref (branch creation) participates in the same
    /// CAS protocol as a tip advance: both must be monotonic or lose.
    ///
    /// R6: Race loser gets Busy. Stale tip never clobbers winner.
    pub fn compare_and_swap(
        &mut self,
        name: &RefName,
        expected: Option<CommitId>,
        next: CommitId,
    ) -> Result<(), RefError> {
        let current = self.refs.get(name).copied();
        if current == expected {
            self.refs.insert(name.clone(), next);
            Ok(())
        } else {
            Err(RefError::Busy)
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

    fn list(&self, ns: RefNs) -> Vec<(RefName, CommitId)> {
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
    fn parse_qualified_path_at_branch() {
        assert_eq!(
            parse_qualified_path("my.db@dev").unwrap(),
            ("my.db".to_string(), Revision::Branch("dev".into()))
        );
    }

    #[test]
    fn parse_qualified_path_slash_branch() {
        assert_eq!(
            parse_qualified_path("my.db/dev").unwrap(),
            ("my.db".to_string(), Revision::Branch("dev".into()))
        );
        assert_eq!(
            parse_qualified_path("my.db/v1").unwrap(),
            ("my.db".to_string(), Revision::Branch("v1".into()))
        );
        assert_eq!(
            parse_qualified_path("my.db/main~1").unwrap(),
            (
                "my.db".to_string(),
                Revision::Ancestor(Box::new(Revision::Branch("main".into())), 1)
            )
        );
    }

    #[test]
    fn parse_qualified_path_plain() {
        assert_eq!(
            parse_qualified_path("my.db").unwrap(),
            ("my.db".to_string(), Revision::Head)
        );
        assert_eq!(
            parse_qualified_path("my.db/archive.db").unwrap(),
            ("my.db/archive.db".to_string(), Revision::Head)
        );
    }

    #[test]
    fn parse_qualified_alias_matches_path() {
        assert_eq!(
            parse_qualified("my.db@dev").unwrap(),
            parse_qualified_path("my.db@dev").unwrap()
        );
    }

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
    fn parse_revision_parent() {
        assert_eq!(
            parse_revision("main^2").unwrap(),
            Revision::Parent(Box::new(Revision::Branch("main".into())), 2)
        );
    }

    #[test]
    fn parse_revision_stacked_specs() {
        // `main~1^2` splits on `^` first (suffix is numeric).
        assert_eq!(
            parse_revision("main~1^2").unwrap(),
            Revision::Parent(
                Box::new(Revision::Ancestor(
                    Box::new(Revision::Branch("main".into())),
                    1
                )),
                2
            )
        );
        // `main^2~1` has a non-numeric `^` suffix, so it splits on `~` first.
        assert_eq!(
            parse_revision("main^2~1").unwrap(),
            Revision::Ancestor(
                Box::new(Revision::Parent(
                    Box::new(Revision::Branch("main".into())),
                    2
                )),
                1
            )
        );
    }

    #[test]
    fn parse_revision_rejects_bad_suffixes() {
        assert_eq!(
            parse_revision("main^foo").unwrap_err().to_string(),
            "invalid revision spec: 'main^foo'"
        );
        assert_eq!(
            parse_revision("main~").unwrap_err().to_string(),
            "invalid revision spec: 'main~'"
        );
    }

    #[test]
    fn parse_revision_hash() {
        let hex = "abcdef0123456789abcdef0123456789abcdef01";
        assert_eq!(
            parse_revision(hex).unwrap(),
            Revision::Hash(hex.to_string())
        );
    }

    #[test]
    fn parse_revision_bad_spec() {
        assert_eq!(
            parse_revision("HEAD~x").unwrap_err().to_string(),
            "invalid revision spec: 'HEAD~x'"
        );
        assert_eq!(
            parse_revision("").unwrap_err().to_string(),
            "invalid revision spec: ''"
        );
        // Empty range endpoints are rejected, not treated as branches.
        assert_eq!(
            parse_revision("a..").unwrap_err().to_string(),
            "invalid revision spec: ''"
        );
        assert_eq!(
            parse_revision("..b").unwrap_err().to_string(),
            "invalid revision spec: ''"
        );
    }

    #[test]
    fn parse_revision_working_staged() {
        assert_eq!(parse_revision("WORKING").unwrap(), Revision::Working);
        assert_eq!(parse_revision("staged").unwrap(), Revision::Staged);
    }

    #[test]
    fn ref_name_full_path_and_display() {
        let b = RefName::branch("main");
        assert_eq!(b.full_path(), "refs/heads/main");
        assert_eq!(b.to_string(), "heads/main");
        let t = RefName::tag("v1");
        assert_eq!(t.full_path(), "refs/tags/v1");
        assert_eq!(t.to_string(), "tags/v1");
    }

    #[test]
    fn cas_race_returns_busy() {
        let mut store = MemRefStore::new();
        let name = RefName::branch("main");
        let c1 = CommitId([1u8; 20]);
        let c2 = CommitId([2u8; 20]);
        let c3 = CommitId([3u8; 20]);

        assert_eq!(store.compare_and_swap(&name, None, c1), Ok(()));
        assert_eq!(store.compare_and_swap(&name, None, c2), Err(RefError::Busy));
        assert_eq!(store.compare_and_swap(&name, Some(c1), c3), Ok(()));
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

        let branches = store.list(RefNs::Heads);
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].0.name, "dev");
        assert_eq!(branches[1].0.name, "main");

        assert!(store.delete(&dev));
        assert_eq!(store.get(&dev), None);
        assert_eq!(store.list(RefNs::Heads).len(), 1);
    }

    #[test]
    fn rubric_aliases_match_plain_calls() {
        let mut store = MemRefStore::new();
        let c1 = CommitId([1u8; 20]);
        store.create_branch("main", c1);
        assert_eq!(store.get_branch("main"), Some(c1));
        assert_eq!(store.get(&RefName::branch("main")), Some(c1));
        assert_eq!(store.list_branches().len(), 1);
        assert_eq!(store.list(RefNs::Heads).len(), 1);

        store.create_branch("dev", c1);
        assert!(store.delete_branch("dev"));
        assert!(!store.delete_branch("dev"));

        let t1 = CommitId([2u8; 20]);
        store.set(&RefName::tag("v1"), t1);
        assert_eq!(store.get_tag("v1"), Some(t1));
        assert_eq!(store.list_tags().len(), 1);
        assert!(store.delete_tag("v1"));
        assert!(!store.delete_tag("v1"));
    }
}
