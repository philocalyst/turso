/// Commit model, V2 codec, ancestor walk, and merge-base LCA.
///
/// C1: V2 codec round-trips; canonical bytes → SHA-256 truncated to 20 bytes.
/// C2: merge_base via generation-weighted LCA.
/// C3: ancestors walk for ~N resolution and range filtering.

use sha2::{Digest, Sha256};

use crate::model::{ChunkHash, CommitId, RootHash, VersionError, VersionResult};
use crate::refs::{parse_revision, RefName, Revision};

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

/// V2 codec: encode a commit to canonical bytes.
///
/// Format: version_byte | parent_count | parent_ids(20B each) | root(20B) |
///         name_len(4B LE) | name | email_len(4B LE) | email |
///         msg_len(4B LE) | msg | timestamp(8B LE)
pub fn encode_v2(commit: &Commit) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    // version byte
    buf.push(2u8);
    // parent count
    buf.push(commit.parents.len() as u8);
    // parent ids
    for parent in &commit.parents {
        buf.extend_from_slice(&parent.0);
    }
    // root hash
    buf.extend_from_slice(&commit.root.0);
    // name
    let name_bytes = commit.meta.name.as_bytes();
    buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(name_bytes);
    // email
    let email_bytes = commit.meta.email.as_bytes();
    buf.extend_from_slice(&(email_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(email_bytes);
    // message
    let msg_bytes = commit.meta.message.as_bytes();
    buf.extend_from_slice(&(msg_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(msg_bytes);
    // timestamp
    buf.extend_from_slice(&commit.meta.timestamp.to_le_bytes());
    buf
}

/// V2 codec: decode canonical bytes back to a commit.
pub fn decode_v2(bytes: &[u8]) -> VersionResult<Commit> {
    if bytes.is_empty() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let mut pos = 0;

    // version byte
    let version = bytes[pos];
    pos += 1;
    if version != 2 {
        return Err(VersionError::InvalidCommitEncoding);
    }

    // parent count
    if pos >= bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let parent_count = bytes[pos] as usize;
    pos += 1;

    // parent ids
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

    // root hash
    if pos + 20 > bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let mut root_arr = [0u8; 20];
    root_arr.copy_from_slice(&bytes[pos..pos + 20]);
    pos += 20;

    // name
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

    // email
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

    // message
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

    // timestamp
    if pos + 8 > bytes.len() {
        return Err(VersionError::InvalidCommitEncoding);
    }
    let timestamp = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());

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

/// Compute the CommitId by hashing the canonical V2 bytes (SHA-256, truncated to 20).
pub fn hash_commit(commit: &Commit) -> CommitId {
    let canonical = encode_v2(commit);
    let hash = Sha256::digest(&canonical);
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&hash[..20]);
    CommitId(arr)
}

/// CommitStore trait: minimal interface for looking up and storing commits.
pub trait CommitStore {
    fn get_commit(&self, id: &CommitId) -> Option<Commit>;
    fn put_commit(&mut self, commit: Commit) -> CommitId;
}

/// In-memory commit store for testing.
pub struct MemCommitStore {
    commits: std::collections::HashMap<CommitId, Commit>,
}

impl MemCommitStore {
    pub fn new() -> Self {
        MemCommitStore {
            commits: std::collections::HashMap::new(),
        }
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

/// Walk first-parent ancestors of `id`.
///
/// C3: first-parent ~N resolution + range filtering.
/// Missing commit in chain → error.
pub fn ancestors(
    store: &dyn CommitStore,
    id: CommitId,
) -> VersionResult<Vec<CommitId>> {
    let mut visited = std::collections::HashSet::new();
    let mut result = Vec::new();
    let mut current = Some(id);

    while let Some(cid) = current {
        if !visited.insert(cid) {
            break;
        }
        let commit = store
            .get_commit(&cid)
            .ok_or_else(|| VersionError::MissingCommit(cid.to_hex()))?;
        result.push(cid);
        // first-parent walk: take only the first parent
        current = commit.parents.first().copied();
    }

    Ok(result)
}

/// Check if `a` is an ancestor of `b`.
pub fn is_ancestor(
    store: &dyn CommitStore,
    a: CommitId,
    b: CommitId,
) -> VersionResult<bool> {
    let chain = ancestors(store, b)?;
    Ok(chain.contains(&a))
}

/// Compute generation (distance from root) for a commit.
fn generation(store: &dyn CommitStore, id: CommitId) -> Option<u32> {
    let mut gen = 0u32;
    let mut current = Some(id);
    let mut visited = std::collections::HashSet::new();

    while let Some(cid) = current {
        if !visited.insert(cid) {
            return None; // cycle
        }
        let commit = store.get_commit(&cid)?;
        if commit.parents.is_empty() {
            return Some(gen);
        }
        gen += 1;
        current = commit.parents.first().copied();
    }
    None
}

/// Lowest common ancestor by generation.
///
/// C2: merge_base walks both ancestor chains and returns the commit with the
/// highest generation that appears in both. None means unrelated histories.
pub fn merge_base(
    store: &dyn CommitStore,
    a: CommitId,
    b: CommitId,
) -> VersionResult<Option<CommitId>> {
    let gen_a = generation(store, a)
        .ok_or_else(|| VersionError::MissingCommit(a.to_hex()))?;
    let gen_b = generation(store, b)
        .ok_or_else(|| VersionError::MissingCommit(b.to_hex()))?;

    let chain_a = ancestors(store, a)?;
    let chain_b_set: std::collections::HashSet<_> = ancestors(store, b)?.into_iter().collect();

    let mut best: Option<(u32, CommitId)> = None;

    for &cid in &chain_a {
        if chain_b_set.contains(&cid) {
            let g = generation(store, cid).unwrap_or(0);
            if best.map_or(true, |(bg, _)| g > bg) {
                best = Some((g, cid));
            }
        }
    }

    Ok(best.map(|(_, id)| id))
}

/// Resolve a Revision against a store, returning a CommitId.
pub fn resolve_revision(
    store: &dyn CommitStore,
    refs: &dyn crate::refs::RefStore,
    active_branch: Option<&RefName>,
    revision: &Revision,
    staged: Option<CommitId>,
    head: Option<CommitId>,
) -> VersionResult<CommitId> {
    match revision {
        Revision::Head => head.ok_or_else(|| {
            VersionError::InvalidRevisionSpec("HEAD".to_string())
        }),
        Revision::Working | Revision::Staged => staged.ok_or_else(|| {
            VersionError::InvalidRevisionSpec("WORKING".to_string())
        }),
        Revision::Branch(name) => {
            // Try branches first, then tags
            let branch_ref = RefName::branch(name);
            if let Some(id) = refs.get(&branch_ref) {
                return Ok(id);
            }
            let tag_ref = RefName::tag(name);
            if let Some(id) = refs.get(&tag_ref) {
                return Ok(id);
            }
            Err(VersionError::BranchNotFound(name.clone()))
        }
        Revision::Hash(hex_str) => {
            CommitId::from_hex(hex_str)
                .map_err(|_| VersionError::CommitNotFound(hex_str.clone()))
        }
        Revision::Ancestor(base, n) => {
            let base_id = resolve_revision(store, refs, active_branch, base, staged, head)?;
            let chain = ancestors(store, base_id)?;
            chain.get(*n as usize)
                .copied()
                .ok_or_else(|| VersionError::CommitNotFound(format!("{}~{}", base, n)))
        }
        Revision::Parent(base, _n) => {
            // ^N = Nth parent; for simplicity, treat ^1 as first-parent (same as ~1)
            let base_id = resolve_revision(store, refs, active_branch, base, staged, head)?;
            let commit = store
                .get_commit(&base_id)
                .ok_or_else(|| VersionError::MissingCommit(base_id.to_hex()))?;
            commit
                .parents
                .first()
                .copied()
                .ok_or_else(|| VersionError::CommitNotFound(format!("{}^1", base)))
        }
        Revision::Range(left, right) => {
            // Range: commits reachable from right but not from left
            // Resolution returns the right endpoint
            resolve_revision(store, refs, active_branch, right, staged, head)
        }
        Revision::SymmetricDifference(left, right) => {
            // Symmetric: merge-base of left..right, then to right
            let left_id = resolve_revision(store, refs, active_branch, left, staged, head)?;
            let right_id = resolve_revision(store, refs, active_branch, right, staged, head)?;
            merge_base(store, left_id, right_id)?
                .ok_or_else(|| VersionError::InvalidRevisionSpec(
                    format!("{}...{}", left, right),
                ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(decode_v2(&[1u8]).is_err()); // wrong version
        assert!(decode_v2(&[2u8, 0u8]).is_err()); // truncated
        assert!(decode_v2(&[]).is_err()); // empty
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
}
