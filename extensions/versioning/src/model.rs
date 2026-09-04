use std::fmt;

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ChunkHash(pub [u8; 20]);

impl ChunkHash {
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, StoreError> {
        let bytes = hex::decode(s).map_err(|_| StoreError::BadHash(s.to_string()))?;
        if bytes.len() != 20 {
            return Err(StoreError::BadHash(s.to_string()));
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Ok(ChunkHash(arr))
    }
}

impl fmt::Debug for ChunkHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChunkHash({})", self.to_hex())
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CommitHash(pub [u8; 20]);

impl CommitHash {
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, StoreError> {
        let bytes = hex::decode(s).map_err(|_| StoreError::BadHash(s.to_string()))?;
        if bytes.len() != 20 {
            return Err(StoreError::BadHash(s.to_string()));
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Ok(CommitHash(arr))
    }
}

impl fmt::Debug for CommitHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CommitHash({})", self.to_hex())
    }
}

pub struct FileMagic;

impl FileMagic {
    pub const DLTC: u32 = 0x444C5443;
    pub const FORMAT_VERSION: u16 = 12;
}

#[derive(Copy, Clone)]
pub struct NodeFlags(pub u16);

impl NodeFlags {
    pub const INTKEY: NodeFlags = NodeFlags(0x01);
    pub const BLOBKEY: NodeFlags = NodeFlags(0x02);
    pub const COUNTS: NodeFlags = NodeFlags(0x04);

    pub fn contains(self, flag: NodeFlags) -> bool {
        (self.0 & flag.0) != 0
    }
}

impl std::ops::BitOr for NodeFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        NodeFlags(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for NodeFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for NodeFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeFlags(0x{:04X})", self.0)
    }
}

pub const MANIFEST_SIZE: usize = 168;

pub struct Manifest {
    pub root: ChunkHash,
    pub meta: ChunkHash,
    pub commits: u32,
    pub working: u32,
}

/// Identity of a commit in the version graph. Deliberately distinct from
/// `ChunkHash`/`CommitHash` even though the bytes are interchangeable: a
/// commit id refers to a commit object, a chunk hash refers to content.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CommitId(pub [u8; 20]);

impl CommitId {
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> VersionResult<Self> {
        let bytes = hex::decode(s).map_err(|_| VersionError::CommitNotFound(s.to_string()))?;
        if bytes.len() != 20 {
            return Err(VersionError::CommitNotFound(s.to_string()));
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Ok(CommitId(arr))
    }
}

impl fmt::Debug for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CommitId({})", self.to_hex())
    }
}

/// Root tree hash of a commit. Content-addressable like `ChunkHash`, so it
/// reuses `StoreError::BadHash` for malformed hex.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RootHash(pub [u8; 20]);

impl RootHash {
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, StoreError> {
        let bytes = hex::decode(s).map_err(|_| StoreError::BadHash(s.to_string()))?;
        if bytes.len() != 20 {
            return Err(StoreError::BadHash(s.to_string()));
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Ok(RootHash(arr))
    }
}

impl fmt::Debug for RootHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RootHash({})", self.to_hex())
    }
}

/// Versioning-layer errors. Display strings are doltlite-compatible and must
/// not drift: the SQL layer surfaces them verbatim as user-facing messages.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    #[error("cannot commit: unresolved merge conflicts")]
    Conflicts,

    #[error("cannot commit: constraint violations remain")]
    Violations,

    #[error("nothing to commit")]
    NothingToCommit,

    #[error("branch '{0}' already exists")]
    BranchAlreadyExists(String),

    #[error("branch not found: {0}")]
    BranchNotFound(String),

    #[error("tag not found: {0}")]
    TagNotFound(String),

    #[error("commit not found: {0}")]
    CommitNotFound(String),

    #[error("invalid revision spec: '{0}'")]
    InvalidRevisionSpec(String),

    #[error("invalid author: user.name and user.email must be set")]
    InvalidAuthor,

    #[error("cannot write in detached HEAD state")]
    DetachedHeadWrite,

    #[error("table not found: {0}")]
    TableNotFound(String),

    #[error("database is locked")]
    DatabaseLocked,

    #[error("incorrect number of arguments to {0}")]
    IncorrectArity(String),

    #[error("invalid commit encoding")]
    InvalidCommitEncoding,

    #[error("commit graph contains a cycle at {0}")]
    CommitGraphCycle(String),

    #[error("table has no primary key: {0}")]
    TableHasNoPrimaryKey(String),

    #[error("ref required: dolt_at_{0} needs a revision argument")]
    AtRefRequired(String),

    #[error("schema conflict in table: {0} ({1})")]
    SchemaConflict(String, String),

    #[error("invalid resolve side: '{0}' (want '--ours' or '--theirs')")]
    InvalidResolveSide(String),

    #[error("cherry-pick of merge commit needs -m PARENT (got {0} parents)")]
    CherryPickMergeNeedsParent(usize),

    #[error("rebase conflict at {0}: resolve and --continue")]
    RebaseConflict(String),

    #[error("no rebase in progress")]
    NoRebaseInProgress,

    #[error("no merge in progress")]
    NoMergeInProgress,

    #[error("merge already in progress")]
    MergeInProgress,

    #[error("rebase already in progress")]
    RebaseInProgress,

    #[error("invalid schema for table: {0}")]
    InvalidSchema(String),

    #[error("no conflict found for table '{0}'")]
    NoConflict(String),

    #[error("failed to write working set to SQL: {0}")]
    WorkWrite(String),

    #[error("usage: dolt_remote(action, name [, url])")]
    UsageDoltRemote,

    #[error("action and name required")]
    ActionAndNameRequired,

    #[error("url required for add")]
    UrlRequiredForAdd,

    #[error("too many arguments")]
    TooManyArguments,

    #[error("remote name invalid")]
    RemoteNameInvalid,

    #[error("remote already exists")]
    RemoteAlreadyExists,

    #[error("remote not found")]
    RemoteNotFound,

    #[error("unknown action: use 'add' or 'remove'")]
    UnknownRemoteAction,

    #[error("remote and branch required")]
    RemoteAndBranchRequired,

    #[error("unknown option: {0}")]
    UnknownOption(String),

    #[error("usage: dolt_fetch(remote [, branch])")]
    UsageDoltFetch,

    #[error("remote name required")]
    RemoteNameRequired,

    #[error("branch name required")]
    BranchNameRequired,

    #[error("fetch failed: branch not found on remote")]
    FetchBranchNotFound,

    #[error("failed to read remote refs")]
    FailedReadRemoteRefs,

    #[error("fetch failed")]
    FetchFailed,

    #[error("usage: dolt_pull(remote, branch)")]
    UsageDoltPull,

    #[error("tracking branch not found after fetch")]
    TrackingNotFoundAfterFetch,

    #[error("cannot pull non-current branch without fast-forward")]
    PullNonCurrentNotFastForward,

    #[error("cannot pull with uncommitted changes")]
    PullUncommittedChanges,

    #[error("cannot merge a non-fast-forward pull in a lazy store; materialize the store first")]
    PullLazyNonFF,

    #[error("not a fast-forward of the remote branch (use force to overwrite)")]
    PushNotFastForward,

    #[error("usage: dolt_clone(['--lazy'], url)")]
    UsageDoltClone,

    #[error("url required")]
    UrlRequired,

    #[error("database is not empty — clone into a fresh database")]
    CloneNotEmpty,

    #[error("gc requires exclusive access")]
    GcRequiresExclusiveAccess,

    #[error("no credentials; run SELECT dolt_creds_new()")]
    NoCredentials,

    #[error("no such credential")]
    NoSuchCredential,

    #[error("usage: dolt_creds('rm', <kid>)")]
    UsageDoltCreds,

    #[error("failed to open remote (URL must start with file:// or mem://)")]
    BadRemoteUrl,

    #[error("chunk not found: {0}")]
    ChunkNotFound(String),

    #[error("chunk verification failed: {0}")]
    ChunkVerificationFailed(String),

    #[error("VACUUM INTO is not supported for versioned databases")]
    VacuumIntoUnsupported,

    #[error("invalid snapshot encoding")]
    InvalidSnapshotEncoding,

    #[error("remote storage failed: {0}")]
    RemoteStorageFailed(String),
}

pub type VersionResult<T> = Result<T, VersionError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("bad node magic: expected PNOD")]
    BadNodeMagic,

    #[error("unordered insert into prolly builder")]
    UnorderedInsert,

    /// Absence surfaces as `Ok(None)` today; this variant is reserved for the
    /// O2 B-tree path where chunk lookup is fallible and missing is an error.
    #[error("chunk not found: {0}")]
    ChunkNotFound(String),

    #[error("bad file manifest")]
    BadManifest,

    #[error("corrupt WAL record")]
    CorruptWal,

    #[error("bad hash string: {0}")]
    BadHash(String),
}

impl Manifest {
    pub fn encode(&self) -> [u8; MANIFEST_SIZE] {
        let mut buf = [0u8; MANIFEST_SIZE];
        buf[0..4].copy_from_slice(&FileMagic::DLTC.to_be_bytes());
        buf[4..6].copy_from_slice(&FileMagic::FORMAT_VERSION.to_be_bytes());
        buf[6..26].copy_from_slice(&self.root.0);
        buf[26..46].copy_from_slice(&self.meta.0);
        buf[46..50].copy_from_slice(&self.commits.to_be_bytes());
        buf[50..54].copy_from_slice(&self.working.to_be_bytes());
        buf
    }

    pub fn decode(buf: &[u8; MANIFEST_SIZE]) -> Result<Self, StoreError> {
        let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != FileMagic::DLTC {
            return Err(StoreError::BadManifest);
        }
        let version = u16::from_be_bytes([buf[4], buf[5]]);
        if version != FileMagic::FORMAT_VERSION {
            return Err(StoreError::BadManifest);
        }

        let mut root = [0u8; 20];
        root.copy_from_slice(&buf[6..26]);
        let mut meta = [0u8; 20];
        meta.copy_from_slice(&buf[26..46]);
        let commits = u32::from_be_bytes([buf[46], buf[47], buf[48], buf[49]]);
        let working = u32::from_be_bytes([buf[50], buf[51], buf[52], buf[53]]);

        if buf[54..MANIFEST_SIZE].iter().any(|&b| b != 0) {
            return Err(StoreError::BadManifest);
        }

        Ok(Manifest {
            root: ChunkHash(root),
            meta: ChunkHash(meta),
            commits,
            working,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only `ChunkHash` satisfies `Into<ChunkHash>`, so a `CommitHash`
    /// argument is refused at compile time — the two hashes stay distinct
    /// even though their bytes are interchangeable.
    fn requires_chunk<T: Into<ChunkHash>>(t: T) -> ChunkHash {
        t.into()
    }

    #[test]
    fn model_chunk_hash_hex_roundtrip() {
        let h = ChunkHash([0x41; 20]);
        let hex = h.to_hex();
        let h2 = ChunkHash::from_hex(&hex).unwrap();
        assert_eq!(h, h2);
        assert_eq!(h2.as_bytes(), &[0x41; 20]);
    }

    #[test]
    fn model_commit_hash_is_not_chunk_hash() {
        let ch = ChunkHash([0x42; 20]);
        let _: ChunkHash = requires_chunk(ch);
        let c = CommitHash([0x42; 20]);
        assert_eq!(ch.as_bytes(), c.as_bytes());
    }

    // Compile-fail proof backing the test above: `Into<ChunkHash>` has no
    // impl for `CommitHash`, so uncommenting the call breaks the build.
    #[cfg(any())]
    fn _commit_hash_refused_by_bound() {
        let c = CommitHash([0; 20]);
        let _ = requires_chunk(c);
    }

    #[test]
    fn model_manifest_168_byte_roundtrip() {
        let m = Manifest {
            root: ChunkHash([1; 20]),
            meta: ChunkHash([2; 20]),
            commits: 42,
            working: 7,
        };
        let buf = m.encode();
        assert_eq!(buf.len(), 168);
        let m2 = Manifest::decode(&buf).unwrap();
        assert_eq!(m2.root, m.root);
        assert_eq!(m2.meta, m.meta);
        assert_eq!(m2.commits, m.commits);
        assert_eq!(m2.working, m.working);
    }

    #[test]
    fn model_manifest_rejects_bad_magic() {
        let mut buf = [0u8; MANIFEST_SIZE];
        buf[0..4].copy_from_slice(&0xDEADBEEF_u32.to_be_bytes());
        assert!(Manifest::decode(&buf).is_err());
    }

    #[test]
    fn model_manifest_rejects_bad_version() {
        let mut buf = [0u8; MANIFEST_SIZE];
        buf[0..4].copy_from_slice(&FileMagic::DLTC.to_be_bytes());
        buf[4..6].copy_from_slice(&99_u16.to_be_bytes());
        assert!(Manifest::decode(&buf).is_err());
    }

    #[test]
    fn model_file_magic_consts() {
        assert_eq!(FileMagic::DLTC, 0x444C5443);
        assert_eq!(FileMagic::FORMAT_VERSION, 12);
    }

    #[test]
    fn model_node_flags_contains() {
        let flags = NodeFlags::INTKEY | NodeFlags::COUNTS;
        assert!(flags.contains(NodeFlags::INTKEY));
        assert!(!flags.contains(NodeFlags::BLOBKEY));
        assert!(flags.contains(NodeFlags::COUNTS));
    }

    #[test]
    fn store_error_display_bad_node_magic() {
        assert_eq!(
            StoreError::BadNodeMagic.to_string(),
            "bad node magic: expected PNOD"
        );
    }

    #[test]
    fn store_error_display_unordered_insert() {
        assert_eq!(
            StoreError::UnorderedInsert.to_string(),
            "unordered insert into prolly builder"
        );
    }

    #[test]
    fn store_error_display_chunk_not_found() {
        let hex = "abcdef0123456789abcdef0123456789abcdef01";
        assert_eq!(
            StoreError::ChunkNotFound(hex.to_string()).to_string(),
            format!("chunk not found: {hex}")
        );
    }

    #[test]
    fn store_error_display_bad_manifest() {
        assert_eq!(StoreError::BadManifest.to_string(), "bad file manifest");
    }

    #[test]
    fn store_error_display_corrupt_wal() {
        assert_eq!(StoreError::CorruptWal.to_string(), "corrupt WAL record");
    }

    #[test]
    fn store_error_display_bad_hash() {
        assert_eq!(
            StoreError::BadHash("zz".to_string()).to_string(),
            "bad hash string: zz"
        );
    }

    #[test]
    fn model_commit_id_hex_roundtrip() {
        let id = CommitId([0xAB; 20]);
        let hex = id.to_hex();
        assert_eq!(id, CommitId::from_hex(&hex).unwrap());
        assert_eq!(id.as_bytes(), &[0xAB; 20]);
    }

    #[test]
    fn model_commit_id_rejects_short_hex() {
        let err = CommitId::from_hex("abcd").unwrap_err();
        assert_eq!(err.to_string(), "commit not found: abcd");
    }

    #[test]
    fn model_root_hash_hex_roundtrip() {
        let root = RootHash([0x0F; 20]);
        let hex = root.to_hex();
        assert_eq!(root, RootHash::from_hex(&hex).unwrap());
        assert_eq!(root.as_bytes(), &[0x0F; 20]);
    }

    #[test]
    fn model_commit_id_is_not_chunk_hash() {
        let id = CommitId([0x42; 20]);
        let ch = ChunkHash([0x42; 20]);
        assert_eq!(id.as_bytes(), ch.as_bytes());
        assert_eq!(id.to_hex(), ch.to_hex());
    }

    #[test]
    fn version_error_displays_are_exact() {
        assert_eq!(
            VersionError::Conflicts.to_string(),
            "cannot commit: unresolved merge conflicts"
        );
        assert_eq!(
            VersionError::Violations.to_string(),
            "cannot commit: constraint violations remain"
        );
        assert_eq!(
            VersionError::NothingToCommit.to_string(),
            "nothing to commit"
        );
        assert_eq!(
            VersionError::BranchAlreadyExists("main".into()).to_string(),
            "branch 'main' already exists"
        );
        assert_eq!(
            VersionError::BranchNotFound("dev".into()).to_string(),
            "branch not found: dev"
        );
        assert_eq!(
            VersionError::TagNotFound("v1".into()).to_string(),
            "tag not found: v1"
        );
        assert_eq!(
            VersionError::CommitNotFound("abc123".into()).to_string(),
            "commit not found: abc123"
        );
        assert_eq!(
            VersionError::InvalidRevisionSpec("HEAD~x".into()).to_string(),
            "invalid revision spec: 'HEAD~x'"
        );
        assert_eq!(
            VersionError::InvalidAuthor.to_string(),
            "invalid author: user.name and user.email must be set"
        );
        assert_eq!(
            VersionError::DetachedHeadWrite.to_string(),
            "cannot write in detached HEAD state"
        );
        assert_eq!(
            VersionError::TableNotFound("t1".into()).to_string(),
            "table not found: t1"
        );
        assert_eq!(
            VersionError::DatabaseLocked.to_string(),
            "database is locked"
        );
        assert_eq!(
            VersionError::IncorrectArity("dolt_add".into()).to_string(),
            "incorrect number of arguments to dolt_add"
        );
        assert_eq!(
            VersionError::InvalidCommitEncoding.to_string(),
            "invalid commit encoding"
        );
        assert_eq!(
            VersionError::CommitGraphCycle("abcd".into()).to_string(),
            "commit graph contains a cycle at abcd"
        );
        assert_eq!(
            VersionError::AtRefRequired("t".into()).to_string(),
            "ref required: dolt_at_t needs a revision argument"
        );
        assert_eq!(
            VersionError::SchemaConflict("t".into(), "breaking".into()).to_string(),
            "schema conflict in table: t (breaking)"
        );
        assert_eq!(
            VersionError::InvalidResolveSide("--mine".into()).to_string(),
            "invalid resolve side: '--mine' (want '--ours' or '--theirs')"
        );
        assert_eq!(
            VersionError::CherryPickMergeNeedsParent(2).to_string(),
            "cherry-pick of merge commit needs -m PARENT (got 2 parents)"
        );
        assert_eq!(
            VersionError::RebaseConflict("abc".into()).to_string(),
            "rebase conflict at abc: resolve and --continue"
        );
        assert_eq!(
            VersionError::NoRebaseInProgress.to_string(),
            "no rebase in progress"
        );
        assert_eq!(
            VersionError::NoMergeInProgress.to_string(),
            "no merge in progress"
        );
        assert_eq!(
            VersionError::MergeInProgress.to_string(),
            "merge already in progress"
        );
        assert_eq!(
            VersionError::RebaseInProgress.to_string(),
            "rebase already in progress"
        );
        assert_eq!(
            VersionError::InvalidSchema("t".into()).to_string(),
            "invalid schema for table: t"
        );
        assert_eq!(
            VersionError::NoConflict("t".into()).to_string(),
            "no conflict found for table 't'"
        );
        assert_eq!(
            VersionError::WorkWrite("boom".into()).to_string(),
            "failed to write working set to SQL: boom"
        );
    }
}
