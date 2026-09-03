/// Commit model, V2 codec, ancestor walk, and merge-base LCA.
///
/// C1: V2 codec round-trips; canonical bytes → SHA-256 truncated to 20 bytes.
/// C2: merge_base via generation-weighted LCA over the full commit DAG.
/// C3: ancestors walk for ~N resolution and range filtering.
use sha2::{Digest, Sha256};

use crate::model::{CommitId, RootHash, VersionError, VersionResult};
use crate::refs::{RefName, Revision};

use std::collections::{HashMap, HashSet};

/// A commit in the version graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub parents: Vec<CommitId>,
    pub root: RootHash,
    pub meta: CommitMeta,
}

/// Author and timestamp metadata attached to every commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    pub name: String,
    pub email: String,
    pub message: String,
    pub timestamp: i64,
}

/// Compute the CommitId by hashing the canonical V2 bytes (SHA-256, truncated to 20).
pub fn hash_commit(commit: &Commit) -> CommitId {
    let canonical = encode_v2(commit);
    let hash = Sha256::digest(&canonical);
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&hash[..20]);
    CommitId(arr)
}

/// V2 codec: encode a commit to canonical bytes.
///
/// Format: version_byte | parent_count | parent_ids(20B each) | root(20B) |
///         name_len(4B LE) | name | email_len(4B LE) | email |
///         msg_len(4B LE) | msg | timestamp(8B LE)
pub fn encode_v2(commit: &Commit) -> Vec<u8> {
    assert!(
        commit.parents.len() <= u8::MAX as usize,
        "parent count {} does not fit in the V2 parent_count byte",
        commit.parents.len()
    );
    let mut buf = Vec::with_capacity(128);
    buf.push(2u8);
    buf.push(commit.parents.len() as u8);
    for parent in &commit.parents {
        buf.extend_from_slice(&parent.0);
    }
    buf.extend_from_slice(&commit.root.0);
    let name_bytes = commit.meta.name.as_bytes();
    buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(name_bytes);
    let email_bytes = commit.meta.email.as_bytes();
    buf.extend_from_slice(&(email_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(email_bytes);
    let msg_bytes = commit.meta.message.as_bytes();
    buf.extend_from_slice(&(msg_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(msg_bytes);
    buf.extend_from_slice(&commit.meta.timestamp.to_le_bytes());
    buf
}

/// V2 codec: decode canonical bytes back to a commit.
pub fn decode_v2(bytes: &[u8]) -> VersionResult<Commit> {
    if bytes.is_empty() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let mut pos = 0;

    let version = bytes[pos];
    pos += 1;
    if version != 2 {
        return Err(VersionError::InvalidCommitEncoding);
    }

    if pos >= bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let parent_count = bytes[pos] as usize;
    pos += 1;

    let mut parents = Vec::with_capacity(parent_count);
    for _ in 0..parent_count {
        if pos + 20 > bytes.len() {
            return Err(VersionError::InvalidCommitEncoding);
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes[pos..pos + 20]);
        parents.push(CommitId(arr));
        pos += 20;
    }

    if pos + 20 > bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let mut root_arr = [0u8; 20];
    root_arr.copy_from_slice(&bytes[pos..pos + 20]);
    pos += 20;

    if pos + 4 > bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let name_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    if pos + name_len > bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let name = String::from_utf8(bytes[pos..pos + name_len].to_vec())
        .map_err(|_| VersionError::InvalidCommitEncoding)?;
    pos += name_len;

    if pos + 4 > bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let email_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    if pos + email_len > bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let email = String::from_utf8(bytes[pos..pos + email_len].to_vec())
        .map_err(|_| VersionError::InvalidCommitEncoding)?;
    pos += email_len;

    if pos + 4 > bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let msg_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4;
    if pos + msg_len > bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let message = String::from_utf8(bytes[pos..pos + msg_len].to_vec())
        .map_err(|_| VersionError::InvalidCommitEncoding)?;
    pos += msg_len;

    if pos + 8 > bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let timestamp = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
    pos += 8;
    if pos != bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }

    Ok(Commit {
        parents,
        root: RootHash(root_arr),
        meta: CommitMeta {
            name,
            email,
            message,
            timestamp,
        },
    })
}

/// CommitStore trait: minimal interface for looking up and storing commits.
pub trait CommitStore {
    fn get_commit(&self, id: &CommitId) -> Option<Commit>;
    fn put_commit(&mut self, commit: Commit) -> CommitId;
}

/// In-memory commit store for testing.
pub struct MemCommitStore {
    commits: HashMap<CommitId, Commit>,
}

impl MemCommitStore {
    pub fn new() -> Self {
        MemCommitStore {
            commits: HashMap::new(),
        }
    }

    /// Every stored commit. Callers sort; the store keeps no order.
    pub fn entries(&self) -> Vec<(CommitId, Commit)> {
        self.commits
            .iter()
            .map(|(id, commit)| (*id, commit.clone()))
            .collect()
    }
}

impl Default for MemCommitStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitStore for MemCommitStore {
    fn get_commit(&self, id: &CommitId) -> Option<Commit> {
        self.commits.get(id).cloned()
    }

    fn put_commit(&mut self, commit: Commit) -> CommitId {
        let id = hash_commit(&commit);
        self.commits.insert(id, commit);
        id
    }
}

/// Resolve a parsed `Revision` to a concrete `CommitId`.
///
/// `head` is the active branch tip, `working`/`staged` the uncommitted
/// snapshots; the caller owns all three because they live in the session, not
/// the store. `None` for a snapshot means that revision spec is unresolvable
/// on this store.
pub fn resolve_revision(
    store: &dyn CommitStore,
    refs: &dyn crate::refs::RefStore,
    revision: &Revision,
    head: Option<CommitId>,
    working: Option<CommitId>,
    staged: Option<CommitId>,
) -> VersionResult<CommitId> {
    match revision {
        Revision::Head => head.ok_or_else(|| VersionError::InvalidRevisionSpec("HEAD".to_string())),
        Revision::Working => {
            working.ok_or_else(|| VersionError::InvalidRevisionSpec("WORKING".to_string()))
        }
        Revision::Staged => {
            staged.ok_or_else(|| VersionError::InvalidRevisionSpec("STAGED".to_string()))
        }
        Revision::Branch(name) => {
            if let Some(id) = refs.get(&RefName::branch(name)) {
                return Ok(id);
            }
            if let Some(id) = refs.get(&RefName::tag(name)) {
                return Ok(id);
            }
            Err(VersionError::BranchNotFound(name.clone()))
        }
        Revision::Tag(name) => refs
            .get(&RefName::tag(name))
            .ok_or_else(|| VersionError::TagNotFound(name.clone())),
        Revision::Hash(hex_str) => {
            let id = CommitId::from_hex(hex_str)
                .map_err(|_| VersionError::CommitNotFound(hex_str.clone()))?;
            if store.get_commit(&id).is_some() {
                Ok(id)
            } else {
                Err(VersionError::CommitNotFound(hex_str.clone()))
            }
        }
        Revision::Ancestor(base, n) | Revision::Parent(base, n) => {
            // Both ~N and ^N resolve to the Nth first-parent ancestor.
            let base_id = resolve_revision(store, refs, base, head, working, staged)?;
            let chain = ancestors(store, base_id)?;
            chain
                .get(*n as usize)
                .copied()
                .ok_or_else(|| VersionError::CommitNotFound(base_id.to_hex()))
        }
        // `a..b` simplifies to the right endpoint's commit id here; the range
        // filtering itself happens in dolt_log over the ancestor walk, which
        // O2 defers along with the log surface.
        Revision::Range(_left, right) => {
            resolve_revision(store, refs, right, head, working, staged)
        }
        Revision::SymmetricDifference(left, right) => {
            let left_id = resolve_revision(store, refs, left, head, working, staged)?;
            let right_id = resolve_revision(store, refs, right, head, working, staged)?;
            merge_base(store, left_id, right_id)?
                .ok_or_else(|| VersionError::InvalidRevisionSpec(format!("{left}...{right}")))
        }
    }
}

/// Lowest common ancestor of `a` and `b`, deepest in the DAG first.
///
/// C2: walks the full ancestor DAG of both sides (all parents, not just the
/// first-parent chain), intersects the two reachable sets, and returns the
/// common commit with the greatest longest-path depth. None means unrelated
/// histories.
pub fn merge_base(
    store: &dyn CommitStore,
    a: CommitId,
    b: CommitId,
) -> VersionResult<Option<CommitId>> {
    let set_a = reachable_ancestors(store, a)?;
    let set_b = reachable_ancestors(store, b)?;

    let mut best: Option<(u32, CommitId)> = None;
    for cid in set_a.intersection(&set_b) {
        let depth = generation(store, *cid)?;
        if best.is_none_or(|(bd, _)| depth > bd) {
            best = Some((depth, *cid));
        }
    }
    Ok(best.map(|(_, id)| id))
}

/// Check if `a` is an ancestor of `b`.
pub fn is_ancestor(store: &dyn CommitStore, a: CommitId, b: CommitId) -> VersionResult<bool> {
    let chain = ancestors(store, b)?;
    Ok(chain.contains(&a))
}

/// Every commit reachable from `id` through all parents, inclusive.
///
/// A commit missing from the store is an error, not a truncated walk.
fn reachable_ancestors(store: &dyn CommitStore, id: CommitId) -> VersionResult<HashSet<CommitId>> {
    let mut visited = HashSet::new();
    let mut queue = vec![id];
    while let Some(cid) = queue.pop() {
        if !visited.insert(cid) {
            continue;
        }
        let commit = store
            .get_commit(&cid)
            .ok_or_else(|| VersionError::CommitNotFound(cid.to_hex()))?;
        queue.extend(commit.parents.iter().copied());
    }
    Ok(visited)
}

/// Longest path from the root commit over all parents (depth in the DAG).
///
/// Memoized so a shared ancestor is charged once per call. A cycle in the
/// commit graph is corruption, not a shape we silently resolve: it surfaces
/// as an error instead of pretending the graph is acyclic.
fn generation(store: &dyn CommitStore, id: CommitId) -> VersionResult<u32> {
    fn walk(
        store: &dyn CommitStore,
        id: CommitId,
        memo: &mut HashMap<CommitId, u32>,
        path: &mut HashSet<CommitId>,
    ) -> VersionResult<u32> {
        if let Some(&depth) = memo.get(&id) {
            return Ok(depth);
        }
        let commit = store
            .get_commit(&id)
            .ok_or_else(|| VersionError::CommitNotFound(id.to_hex()))?;
        if commit.parents.is_empty() {
            memo.insert(id, 0);
            return Ok(0);
        }
        if !path.insert(id) {
            return Err(VersionError::CommitGraphCycle(id.to_hex()));
        }
        let mut max = 0;
        for parent in &commit.parents {
            max = max.max(walk(store, *parent, memo, path)?);
        }
        path.remove(&id);
        memo.insert(id, max + 1);
        Ok(max + 1)
    }

    let mut memo = HashMap::new();
    let mut path = HashSet::new();
    walk(store, id, &mut memo, &mut path)
}

/// Walk first-parent ancestors of `id`, inclusive.
///
/// C3: first-parent ~N resolution + range filtering. A commit missing from
/// the store is an error, not an empty chain — callers must never silently
/// resolve a short history as "no ancestors". A revisited commit ends the
/// walk (see `vtab_log::commit_chain` for the contract split with the
/// cycle-reporting `generation`).
pub fn ancestors(store: &dyn CommitStore, id: CommitId) -> VersionResult<Vec<CommitId>> {
    let mut visited = HashSet::new();
    let mut result = Vec::new();
    let mut current = Some(id);

    while let Some(cid) = current {
        if !visited.insert(cid) {
            break;
        }
        let commit = store
            .get_commit(&cid)
            .ok_or_else(|| VersionError::CommitNotFound(cid.to_hex()))?;
        result.push(cid);
        current = commit.parents.first().copied();
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::RefStore;

    fn make_commit(parents: Vec<CommitId>, msg: &str) -> Commit {
        Commit {
            parents,
            root: RootHash([0u8; 20]),
            meta: CommitMeta {
                name: "test".into(),
                email: "test@test.com".into(),
                message: msg.into(),
                timestamp: 1000,
            },
        }
    }

    #[test]
    fn v2_codec_roundtrip() {
        let commit = make_commit(vec![], "initial");
        let encoded = encode_v2(&commit);
        let decoded = decode_v2(&encoded).unwrap();
        assert_eq!(commit, decoded);
    }

    #[test]
    fn v2_codec_corrupt() {
        assert!(decode_v2(&[1u8]).is_err());
        assert!(decode_v2(&[2u8, 0u8]).is_err());
        assert!(decode_v2(&[]).is_err());
        // A valid header but a dangling trailing byte is not round-trippable.
        let commit = make_commit(vec![], "x");
        let mut buf = encode_v2(&commit);
        buf.push(0x00);
        assert_eq!(
            decode_v2(&buf).unwrap_err(),
            VersionError::InvalidCommitEncoding
        );
    }

    #[test]
    fn v2_encode_rejects_overflowing_parent_count() {
        let parents = (0..=u8::MAX as usize)
            .map(|i| CommitId([i as u8; 20]))
            .collect();
        let commit = Commit {
            parents,
            root: RootHash([0u8; 20]),
            meta: CommitMeta {
                name: "t".into(),
                email: "t@t.com".into(),
                message: "too many parents".into(),
                timestamp: 0,
            },
        };
        let result = std::panic::catch_unwind(|| encode_v2(&commit));
        assert!(result.is_err());
    }

    #[test]
    fn hash_deterministic() {
        let commit = make_commit(vec![], "test");
        let h1 = hash_commit(&commit);
        let h2 = hash_commit(&commit);
        assert_eq!(h1, h2);
    }

    #[test]
    fn ancestor_chain() {
        let mut store = MemCommitStore::new();

        let c0 = make_commit(vec![], "c0");
        let id0 = store.put_commit(c0);

        let c1 = make_commit(vec![id0], "c1");
        let id1 = store.put_commit(c1);

        let c2 = make_commit(vec![id1], "c2");
        let id2 = store.put_commit(c2);

        let chain = ancestors(&store, id2).unwrap();
        assert_eq!(chain, vec![id2, id1, id0]);
    }

    #[test]
    fn is_ancestor_check() {
        let mut store = MemCommitStore::new();

        let c0 = make_commit(vec![], "c0");
        let id0 = store.put_commit(c0);

        let c1 = make_commit(vec![id0], "c1");
        let id1 = store.put_commit(c1);

        assert!(is_ancestor(&store, id0, id1).unwrap());
        assert!(!is_ancestor(&store, id1, id0).unwrap());
    }

    #[test]
    fn merge_base_diamond() {
        //     c0
        //    / \
        //   c1  c2
        //    \ /
        //     c3
        let mut store = MemCommitStore::new();

        let c0 = make_commit(vec![], "c0");
        let id0 = store.put_commit(c0);

        let c1 = make_commit(vec![id0], "c1");
        let id1 = store.put_commit(c1);

        let c2 = make_commit(vec![id0], "c2");
        let id2 = store.put_commit(c2);

        let c3 = make_commit(vec![id1, id2], "c3");
        let id3 = store.put_commit(c3);

        let base = merge_base(&store, id1, id2).unwrap();
        assert_eq!(base, Some(id0));

        let base = merge_base(&store, id1, id3).unwrap();
        assert_eq!(base, Some(id1));

        // id2 is a direct parent of id3, so it is its own LCA.
        let base = merge_base(&store, id2, id3).unwrap();
        assert_eq!(base, Some(id2));
    }

    #[test]
    fn merge_base_criss_cross() {
        //     c0
        //    /  \
        //   c1  c2
        //   |\  /|
        //   | \/ |
        //   | /\ |
        //   c3  c4   (both merge c1 and c2)
        //    \  /
        //     c5
        let mut store = MemCommitStore::new();

        let id0 = store.put_commit(make_commit(vec![], "c0"));
        let id1 = store.put_commit(make_commit(vec![id0], "c1"));
        let id2 = store.put_commit(make_commit(vec![id0], "c2"));
        let id3 = store.put_commit(make_commit(vec![id1, id2], "c3"));
        let id4 = store.put_commit(make_commit(vec![id1, id2], "c4"));
        let id5 = store.put_commit(make_commit(vec![id3, id4], "c5"));

        // c3 and c4 share c0, c1, and c2; the deepest common ancestors are
        // c1 and c2 (both depth 1). Either is a valid LCA.
        let base = merge_base(&store, id3, id4).unwrap().unwrap();
        assert!(base == id1 || base == id2, "unexpected LCA {base:?}");
        assert_eq!(merge_base(&store, id1, id4).unwrap(), Some(id1));
        assert_eq!(merge_base(&store, id3, id5).unwrap(), Some(id3));
    }

    #[test]
    fn merge_base_unrelated() {
        let mut store = MemCommitStore::new();

        let c0 = make_commit(vec![], "c0");
        let id0 = store.put_commit(c0);

        let c1 = make_commit(vec![], "c1");
        let id1 = store.put_commit(c1);

        let base = merge_base(&store, id0, id1).unwrap();
        assert_eq!(base, None);
    }

    #[test]
    fn merge_base_uses_second_parent() {
        // A merge whose second parent carries commits the first does not:
        // the LCA of c2 and c3 must be found through c2's second parent.
        let mut store = MemCommitStore::new();

        let id0 = store.put_commit(make_commit(vec![], "c0"));
        let id1 = store.put_commit(make_commit(vec![id0], "c1"));
        let id2 = store.put_commit(make_commit(vec![id0, id1], "c2"));
        let id3 = store.put_commit(make_commit(vec![id1], "c3"));

        assert_eq!(merge_base(&store, id2, id3).unwrap(), Some(id1));
    }

    #[test]
    fn ancestors_missing_commit_errors() {
        let store = MemCommitStore::new();
        let missing = CommitId([0xDE; 20]);
        let err = ancestors(&store, missing).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("commit not found: {}", missing.to_hex())
        );
    }

    #[test]
    fn generation_surfaces_cycle() {
        // A 2-cycle (A's parent is B, B's parent is A) is corruption, not a
        // resolvable shape; generation must surface it instead of looping.
        let mut store = MemCommitStore::new();
        let id_a = CommitId([0xAA; 20]);
        let id_b = CommitId([0xBB; 20]);
        store.commits.insert(id_a, make_commit(vec![id_b], "a"));
        store.commits.insert(id_b, make_commit(vec![id_a], "b"));
        assert_eq!(
            generation(&store, id_a).unwrap_err(),
            VersionError::CommitGraphCycle(id_a.to_hex())
        );
    }

    #[test]
    fn resolve_revision_head_and_branch() {
        let mut store = MemCommitStore::new();
        let mut refs = crate::refs::MemRefStore::new();

        let c0 = make_commit(vec![], "c0");
        let id0 = store.put_commit(c0);
        refs.set(&RefName::branch("main"), id0);

        let head = resolve_revision(&store, &refs, &Revision::Head, Some(id0), None, None).unwrap();
        assert_eq!(head, id0);

        let br = resolve_revision(
            &store,
            &refs,
            &Revision::Branch("main".into()),
            Some(id0),
            None,
            None,
        )
        .unwrap();
        assert_eq!(br, id0);

        let missing = resolve_revision(
            &store,
            &refs,
            &Revision::Branch("nope".into()),
            Some(id0),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(missing.to_string(), "branch not found: nope");
    }

    #[test]
    fn resolve_revision_hash_and_ancestor() {
        let mut store = MemCommitStore::new();

        let c0 = make_commit(vec![], "c0");
        let id0 = store.put_commit(c0);
        let c1 = make_commit(vec![id0], "c1");
        let id1 = store.put_commit(c1);

        let by_hash = resolve_revision(
            &store,
            &crate::refs::MemRefStore::new(),
            &Revision::Hash(id1.to_hex()),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(by_hash, id1);

        let by_ancestor = resolve_revision(
            &store,
            &crate::refs::MemRefStore::new(),
            &Revision::Ancestor(Box::new(Revision::Hash(id1.to_hex())), 1),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(by_ancestor, id0);

        let missing = resolve_revision(
            &store,
            &crate::refs::MemRefStore::new(),
            &Revision::Hash("0000000000000000000000000000000000000000".into()),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(
            missing.to_string(),
            "commit not found: 0000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn resolve_revision_working_staged_and_tag() {
        let mut store = MemCommitStore::new();
        let mut refs = crate::refs::MemRefStore::new();

        let id0 = store.put_commit(make_commit(vec![], "c0"));
        refs.set(&RefName::tag("v1"), id0);

        assert_eq!(
            resolve_revision(&store, &refs, &Revision::Working, None, Some(id0), None).unwrap(),
            id0
        );
        assert_eq!(
            resolve_revision(&store, &refs, &Revision::Staged, None, None, Some(id0)).unwrap(),
            id0
        );
        assert_eq!(
            resolve_revision(&store, &refs, &Revision::Tag("v1".into()), None, None, None).unwrap(),
            id0
        );
        assert_eq!(
            resolve_revision(
                &store,
                &refs,
                &Revision::Tag("nope".into()),
                None,
                None,
                None
            )
            .unwrap_err()
            .to_string(),
            "tag not found: nope"
        );
        assert_eq!(
            resolve_revision(&store, &refs, &Revision::Working, None, None, None)
                .unwrap_err()
                .to_string(),
            "invalid revision spec: 'WORKING'"
        );
    }
}
