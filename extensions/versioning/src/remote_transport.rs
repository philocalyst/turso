//! Remote transports: the URL-facing seam every remote command talks through.
//!
//! Two transports ship: `mem://` (an in-process served endpoint — the
//! remotesrv) and `file://` (a directory holding the refs blob and object
//! files). Network schemes are refused, not silently degraded (B5); a real
//! HTTP client registers behind `RemoteTransport` later.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::commit::decode_v2;
use crate::model::{CommitId, VersionError, VersionResult};
use crate::remote_wire::{decode_refs, encode_refs, RemoteRefs, SnapshotId, SourceId};

/// Everything a remote command can ask of storage.
pub trait RemoteTransport {
    /// The raw refs blob bytes (see `remote_wire::decode_refs`).
    fn get_refs(&self) -> VersionResult<Vec<u8>>;
    /// The remote's default branch name.
    fn default_branch(&self) -> VersionResult<String>;
    /// A branch tip from the refs blob, `None` when the remote lacks it.
    fn find_branch(&self, branch: &str) -> VersionResult<Option<CommitId>>;
    /// The object closure reachable from `tip`: commits by parent walk plus
    /// each commit's snapshot records.
    fn walk(&self, tip: CommitId) -> VersionResult<Vec<SourceId>>;
    /// Which of `ids` the remote already stores.
    fn has_many(&self, ids: &[SourceId]) -> VersionResult<Vec<bool>>;
    /// Fetch objects by id; every returned object must be verified by the
    /// caller before it is stored anywhere (B4).
    fn get_batch(&self, ids: &[SourceId]) -> VersionResult<Vec<(SourceId, Vec<u8>)>>;
    /// Store objects, then the per-commit snapshot lists. Objects land
    /// before any ref move so a crash leaves unreachable bytes, never a
    /// dangling ref (B3).
    fn put_batch(
        &self,
        objects: &[(SourceId, Vec<u8>)],
        snap_lists: &[(CommitId, Vec<SnapshotId>)],
    ) -> VersionResult<()>;
    /// Move a branch ref. An existing tip must be an ancestor of `commit`
    /// unless `force`, else the push is refused.
    fn update_branch(&self, branch: &str, commit: CommitId, force: bool) -> VersionResult<()>;
    /// Replace the whole refs blob (clone bootstrap for a fresh remote).
    fn replace_refs(&self, refs: &RemoteRefs) -> VersionResult<()>;
}

/// Typed remotesrv protocol: one request, one response. Both the mem
/// transport and any future network front end go through `serve`, so the
/// serving logic exists exactly once.
#[derive(Debug)]
pub enum Request {
    GetRefs,
    HasMany(Vec<SourceId>),
    GetBatch(Vec<SourceId>),
    PutBatch(Vec<(SourceId, Vec<u8>)>, Vec<(CommitId, Vec<SnapshotId>)>),
    UpdateBranch {
        branch: String,
        commit: CommitId,
        force: bool,
    },
    ReplaceRefs(RemoteRefs),
}

#[derive(Debug)]
pub enum Response {
    Refs(Vec<u8>),
    Presence(Vec<bool>),
    Objects(Vec<(SourceId, Vec<u8>)>),
    Ok,
}

/// One in-process served remote: refs, objects, snapshot lists, and whether
/// it demands credentials.
pub struct MemEndpoint {
    pub refs: RemoteRefs,
    pub objects: HashMap<SourceId, Vec<u8>>,
    pub snap_lists: HashMap<CommitId, Vec<SnapshotId>>,
    pub auth_required: bool,
}

impl MemEndpoint {
    pub fn new(default_branch: &str) -> Self {
        MemEndpoint {
            refs: RemoteRefs {
                default_branch: default_branch.to_string(),
                branches: Vec::new(),
            },
            objects: HashMap::new(),
            snap_lists: HashMap::new(),
            auth_required: false,
        }
    }
}

fn hub() -> &'static Mutex<HashMap<String, MemEndpoint>> {
    static HUB: OnceLock<Mutex<HashMap<String, MemEndpoint>>> = OnceLock::new();
    HUB.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The `mem://` transport handle: every call routes through the hub and
/// `serve`, exactly like a network front end would.
pub struct MemTransport {
    name: String,
}

/// Open a transport by URL. `file://<path>` and `mem://<name>[?auth=required]`
/// are accepted; everything else is refused (B5).
pub fn open_transport(url: &str) -> VersionResult<Box<dyn RemoteTransport>> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(VersionError::BadRemoteUrl);
    };
    match scheme {
        "file" => Ok(Box::new(FileTransport::open(PathBuf::from(rest)))),
        "mem" => {
            let (name, query) = match rest.split_once('?') {
                Some((name, query)) => (name, Some(query)),
                None => (rest, None),
            };
            if name.is_empty() {
                return Err(VersionError::BadRemoteUrl);
            }
            let auth_required = match query {
                Some("auth=required") => true,
                Some(_) => return Err(VersionError::BadRemoteUrl),
                None => false,
            };
            Ok(Box::new(MemTransport::open(name, auth_required)?))
        }
        _ => Err(VersionError::BadRemoteUrl),
    }
}

impl MemTransport {
    /// Register (or find) the endpoint behind a mem URL. The first open
    /// that asks for auth fixes the endpoint's posture; later plain opens
    /// keep it.
    pub fn open(name: &str, auth_required: bool) -> VersionResult<Self> {
        {
            let mut hub = hub().lock().unwrap();
            let endpoint = hub.entry(name.to_string()).or_insert_with(|| {
                let mut ep = MemEndpoint::new("main");
                ep.auth_required = auth_required;
                ep
            });
            if auth_required {
                endpoint.auth_required = true;
            }
        }
        Ok(MemTransport {
            name: name.to_string(),
        })
    }

    fn endpoint<R>(
        &self,
        f: impl FnOnce(&mut MemEndpoint) -> VersionResult<R>,
    ) -> VersionResult<R> {
        let mut hub = hub().lock().unwrap();
        let endpoint = hub
            .get_mut(&self.name)
            .ok_or(VersionError::FailedReadRemoteRefs)?;
        f(endpoint)
    }
}

impl RemoteTransport for MemTransport {
    fn get_refs(&self) -> VersionResult<Vec<u8>> {
        self.endpoint(
            |ep| match serve(ep, Request::GetRefs, auth_kid().as_deref())? {
                Response::Refs(blob) => Ok(blob),
                _ => Ok(Vec::new()),
            },
        )
    }

    fn default_branch(&self) -> VersionResult<String> {
        let blob = self.get_refs()?;
        Ok(decode_refs(&blob)?.default_branch)
    }

    fn find_branch(&self, branch: &str) -> VersionResult<Option<CommitId>> {
        let refs = decode_refs(&self.get_refs()?)?;
        Ok(refs
            .branches
            .into_iter()
            .find(|(name, _)| name == branch)
            .map(|(_, id)| id))
    }

    fn walk(&self, tip: CommitId) -> VersionResult<Vec<SourceId>> {
        self.endpoint(|ep| {
            if ep.auth_required {
                require_auth(auth_kid())?;
            }
            walk_objects(&ep.objects, &ep.snap_lists, tip)
        })
    }

    fn has_many(&self, ids: &[SourceId]) -> VersionResult<Vec<bool>> {
        self.endpoint(|ep| {
            let Response::Presence(present) =
                serve(ep, Request::HasMany(ids.to_vec()), auth_kid().as_deref())?
            else {
                return Ok(Vec::new());
            };
            Ok(present)
        })
    }

    fn get_batch(&self, ids: &[SourceId]) -> VersionResult<Vec<(SourceId, Vec<u8>)>> {
        self.endpoint(|ep| {
            let Response::Objects(objects) =
                serve(ep, Request::GetBatch(ids.to_vec()), auth_kid().as_deref())?
            else {
                return Ok(Vec::new());
            };
            Ok(objects)
        })
    }

    fn put_batch(
        &self,
        objects: &[(SourceId, Vec<u8>)],
        snap_lists: &[(CommitId, Vec<SnapshotId>)],
    ) -> VersionResult<()> {
        self.endpoint(|ep| {
            serve(
                ep,
                Request::PutBatch(objects.to_vec(), snap_lists.to_vec()),
                auth_kid().as_deref(),
            )?;
            Ok(())
        })
    }

    fn update_branch(&self, branch: &str, commit: CommitId, force: bool) -> VersionResult<()> {
        self.endpoint(|ep| {
            serve(
                ep,
                Request::UpdateBranch {
                    branch: branch.to_string(),
                    commit,
                    force,
                },
                auth_kid().as_deref(),
            )?;
            Ok(())
        })
    }

    fn replace_refs(&self, refs: &RemoteRefs) -> VersionResult<()> {
        self.endpoint(|ep| {
            serve(
                ep,
                Request::ReplaceRefs(refs.clone()),
                auth_kid().as_deref(),
            )?;
            Ok(())
        })
    }
}

/// The active kid for auth checks, routed through the process-wide cred
/// store the way a network client would route its credentials.
fn auth_kid() -> Option<String> {
    crate::creds::active_kid()
}

fn require_auth(active: Option<String>) -> VersionResult<()> {
    if active.is_none() {
        return Err(VersionError::NoCredentials);
    }
    Ok(())
}

/// Serve one request against an endpoint. `active_kid` is the caller's
/// active credential id, checked when the endpoint requires auth.
pub fn serve(
    endpoint: &mut MemEndpoint,
    req: Request,
    active_kid: Option<&str>,
) -> VersionResult<Response> {
    if endpoint.auth_required {
        require_auth(active_kid.map(|k| k.to_string()))?;
    }
    match req {
        Request::GetRefs => Ok(Response::Refs(encode_refs(&endpoint.refs))),
        Request::HasMany(ids) => Ok(Response::Presence(
            ids.iter()
                .map(|id| endpoint.objects.contains_key(id))
                .collect(),
        )),
        Request::GetBatch(ids) => {
            let mut objects = Vec::with_capacity(ids.len());
            for id in ids {
                let bytes = endpoint
                    .objects
                    .get(&id)
                    .ok_or_else(|| VersionError::ChunkNotFound(id.to_hex()))?;
                objects.push((id, bytes.clone()));
            }
            Ok(Response::Objects(objects))
        }
        Request::PutBatch(objects, snap_lists) => {
            for (id, bytes) in objects {
                endpoint.objects.insert(id, bytes);
            }
            for (commit, snaps) in snap_lists {
                endpoint.snap_lists.insert(commit, snaps);
            }
            Ok(Response::Ok)
        }
        Request::UpdateBranch {
            branch,
            commit,
            force,
        } => {
            match endpoint
                .refs
                .branches
                .iter_mut()
                .find(|(name, _)| *name == branch)
            {
                Some((_name, tip)) => {
                    if !force && *tip != commit && !is_ancestor(&endpoint.objects, *tip, commit) {
                        return Err(VersionError::PushNotFastForward);
                    }
                    *tip = commit;
                }
                None => endpoint.refs.branches.push((branch.clone(), commit)),
            }
            Ok(Response::Ok)
        }
        Request::ReplaceRefs(refs) => {
            endpoint.refs = refs;
            Ok(Response::Ok)
        }
    }
}

/// Closure walk over an object map: commits by parent links, snapshot
/// records through the per-commit lists.
fn walk_objects(
    objects: &HashMap<SourceId, Vec<u8>>,
    snap_lists: &HashMap<CommitId, Vec<SnapshotId>>,
    tip: CommitId,
) -> VersionResult<Vec<SourceId>> {
    let mut seen: std::collections::HashSet<SourceId> = std::collections::HashSet::new();
    let mut stack = vec![tip];
    while let Some(commit_id) = stack.pop() {
        let id = SourceId::Commit(commit_id);
        if !seen.insert(id) {
            continue;
        }
        let Some(bytes) = objects.get(&id) else {
            return Err(VersionError::ChunkNotFound(id.to_hex()));
        };
        let commit = decode_v2(bytes)?;
        stack.extend(commit.parents);
        if let Some(snaps) = snap_lists.get(&commit_id) {
            for snap in snaps {
                seen.insert(SourceId::Snapshot(*snap));
            }
        }
    }
    let mut closure: Vec<SourceId> = seen.into_iter().collect();
    closure.sort();
    Ok(closure)
}

/// Is `ancestor` reachable from `tip` through parent links in `objects`?
fn is_ancestor(objects: &HashMap<SourceId, Vec<u8>>, ancestor: CommitId, tip: CommitId) -> bool {
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![tip];
    while let Some(commit_id) = stack.pop() {
        if commit_id == ancestor {
            return true;
        }
        if !seen.insert(commit_id) {
            continue;
        }
        if let Some(bytes) = objects.get(&SourceId::Commit(commit_id)) {
            if let Ok(commit) = decode_v2(bytes) {
                stack.extend(commit.parents);
            }
        }
    }
    false
}

/// A `file://` remote: a directory of `refs`, `objects/<hex>`, and
/// `snaps/<commithex>` files.
pub struct FileTransport {
    root: PathBuf,
}

impl FileTransport {
    pub fn open(root: PathBuf) -> Self {
        FileTransport { root }
    }

    fn object_path(&self, id: &SourceId) -> PathBuf {
        self.root.join("objects").join(id.to_hex())
    }

    fn snaps_path(&self, commit: &CommitId) -> PathBuf {
        self.root.join("snaps").join(commit.to_hex())
    }

    fn refs_path(&self) -> PathBuf {
        self.root.join("refs")
    }

    fn read_object(&self, id: &SourceId) -> VersionResult<Vec<u8>> {
        std::fs::read(self.object_path(id)).map_err(|_| VersionError::ChunkNotFound(id.to_hex()))
    }

    fn load_refs(&self) -> VersionResult<RemoteRefs> {
        let blob = self.get_refs()?;
        decode_refs(&blob)
    }

    fn write_refs(&self, refs: &RemoteRefs) -> VersionResult<()> {
        let blob = encode_refs(refs);
        let mut raw = blob.clone();
        raw.extend_from_slice(&crate::chunk::blake3_chunk_hash(&blob).0);
        write_atomically(&self.refs_path(), &raw)
    }

    /// Ancestor check driven by the object files on disk.
    fn file_is_ancestor(&self, ancestor: CommitId, tip: CommitId) -> VersionResult<bool> {
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![tip];
        while let Some(commit_id) = stack.pop() {
            if commit_id == ancestor {
                return Ok(true);
            }
            if !seen.insert(commit_id) {
                continue;
            }
            let bytes = self.read_object(&SourceId::Commit(commit_id))?;
            let commit = decode_v2(&bytes)?;
            stack.extend(commit.parents);
        }
        Ok(false)
    }
}

/// Write bytes to a temp file next to `path`, then rename over it. The
/// rename is the visibility point: readers see the old or the new bytes,
/// never a half-written file (B3).
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> VersionResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| VersionError::RemoteStorageFailed(format!("{path:?}: {e}")))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)
        .map_err(|e| VersionError::RemoteStorageFailed(format!("{path:?}: {e}")))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| VersionError::RemoteStorageFailed(format!("{path:?}: {e}")))
}

impl RemoteTransport for FileTransport {
    fn get_refs(&self) -> VersionResult<Vec<u8>> {
        let raw =
            std::fs::read(self.refs_path()).map_err(|_| VersionError::FailedReadRemoteRefs)?;
        if raw.len() < 20 {
            return Err(VersionError::FailedReadRemoteRefs);
        }
        let (blob, hash) = raw.split_at(raw.len() - 20);
        let expected: [u8; 20] = hash.try_into().unwrap();
        if crate::chunk::blake3_chunk_hash(blob).0 != expected {
            return Err(VersionError::FailedReadRemoteRefs);
        }
        Ok(blob.to_vec())
    }

    fn default_branch(&self) -> VersionResult<String> {
        Ok(self.load_refs()?.default_branch)
    }

    fn find_branch(&self, branch: &str) -> VersionResult<Option<CommitId>> {
        let refs = self.load_refs()?;
        Ok(refs
            .branches
            .into_iter()
            .find(|(name, _)| name == branch)
            .map(|(_, id)| id))
    }

    fn walk(&self, tip: CommitId) -> VersionResult<Vec<SourceId>> {
        let mut objects: HashMap<SourceId, Vec<u8>> = HashMap::new();
        let mut snap_lists: HashMap<CommitId, Vec<SnapshotId>> = HashMap::new();
        let mut stack = vec![tip];
        let mut visited = std::collections::HashSet::new();
        while let Some(commit_id) = stack.pop() {
            if !visited.insert(commit_id) {
                continue;
            }
            let bytes = self.read_object(&SourceId::Commit(commit_id))?;
            let commit = decode_v2(&bytes)?;
            stack.extend(commit.parents);
            if let Ok(list) = std::fs::read_to_string(self.snaps_path(&commit_id)) {
                let snaps: Vec<SnapshotId> = list
                    .lines()
                    .filter(|line| !line.is_empty())
                    .filter_map(|line| {
                        let bytes = hex::decode(line).ok()?;
                        bytes.try_into().ok()
                    })
                    .map(SnapshotId)
                    .collect();
                snap_lists.insert(commit_id, snaps);
            }
            objects.insert(SourceId::Commit(commit_id), bytes);
        }
        // Snapshot bytes are not needed for the id closure, only presence.
        for snaps in snap_lists.values() {
            for snap in snaps {
                objects.insert(SourceId::Snapshot(*snap), Vec::new());
            }
        }
        walk_objects(&objects, &snap_lists, tip)
    }

    fn has_many(&self, ids: &[SourceId]) -> VersionResult<Vec<bool>> {
        Ok(ids.iter().map(|id| self.object_path(id).exists()).collect())
    }

    fn get_batch(&self, ids: &[SourceId]) -> VersionResult<Vec<(SourceId, Vec<u8>)>> {
        let mut objects = Vec::with_capacity(ids.len());
        for id in ids {
            objects.push((*id, self.read_object(id)?));
        }
        Ok(objects)
    }

    fn put_batch(
        &self,
        objects: &[(SourceId, Vec<u8>)],
        snap_lists: &[(CommitId, Vec<SnapshotId>)],
    ) -> VersionResult<()> {
        for (id, bytes) in objects {
            write_atomically(&self.object_path(id), bytes)?;
        }
        for (commit, snaps) in snap_lists {
            let body = snaps
                .iter()
                .map(|s| s.to_hex())
                .collect::<Vec<_>>()
                .join("\n");
            write_atomically(&self.snaps_path(commit), body.as_bytes())?;
        }
        Ok(())
    }

    fn update_branch(&self, branch: &str, commit: CommitId, force: bool) -> VersionResult<()> {
        let mut refs = self.load_refs()?;
        let existing_tip = refs
            .branches
            .iter()
            .find(|(name, _)| name == branch)
            .map(|(_, id)| *id);
        if let Some(tip) = existing_tip {
            if !force && tip != commit && !self.file_is_ancestor(tip, commit)? {
                return Err(VersionError::PushNotFastForward);
            }
            for (name, id) in refs.branches.iter_mut() {
                if name == branch {
                    *id = commit;
                    break;
                }
            }
        } else {
            refs.branches.push((branch.to_string(), commit));
        }
        self.write_refs(&refs)
    }

    fn replace_refs(&self, refs: &RemoteRefs) -> VersionResult<()> {
        self.write_refs(refs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_wire::SYNC_BATCH_SIZE;

    fn commit_obj(ts: i64, parents: Vec<CommitId>) -> (CommitId, Vec<u8>) {
        let commit = crate::commit::Commit {
            parents,
            root: crate::model::RootHash([0u8; 20]),
            meta: crate::commit::CommitMeta {
                name: "a".to_string(),
                email: "a@b".to_string(),
                message: "m".to_string(),
                timestamp: ts,
            },
        };
        let id = crate::commit::hash_commit(&commit);
        (id, crate::commit::encode_v2(&commit))
    }

    fn endpoint_with_main() -> MemEndpoint {
        let mut ep = MemEndpoint::new("main");
        let (id, bytes) = commit_obj(1, vec![]);
        ep.refs.branches.push(("main".to_string(), id));
        ep.objects.insert(SourceId::Commit(id), bytes);
        ep
    }

    #[test]
    fn transport_open_dispatches_schemes() {
        assert!(open_transport("file:///tmp/somewhere").is_ok());
        assert!(open_transport("mem://t-open-1").is_ok());
        assert!(matches!(
            open_transport("http://example.com/db"),
            Err(VersionError::BadRemoteUrl)
        ));
        assert!(matches!(
            open_transport("https://example.com/db"),
            Err(VersionError::BadRemoteUrl)
        ));
        assert!(matches!(
            open_transport("/plain/path"),
            Err(VersionError::BadRemoteUrl)
        ));
    }

    #[test]
    fn transport_serve_roundtrip_refs_and_objects() {
        let mut ep = endpoint_with_main();
        let Response::Refs(blob) = serve(&mut ep, Request::GetRefs, None).unwrap() else {
            panic!("refs response");
        };
        let refs = decode_refs(&blob).unwrap();
        assert_eq!(refs.default_branch, "main");
        assert_eq!(refs.branches.len(), 1);

        let (id, _) = commit_obj(1, vec![]);
        let Response::Presence(present) = serve(
            &mut ep,
            Request::HasMany(vec![
                SourceId::Commit(id),
                SourceId::Commit(CommitId([9; 20])),
            ]),
            None,
        )
        .unwrap() else {
            panic!("presence response");
        };
        assert_eq!(present, vec![true, false]);
    }

    #[test]
    fn transport_serve_update_branch_enforces_fast_forward() {
        let mut ep = endpoint_with_main();
        let (base, base_bytes) = commit_obj(1, vec![]);
        ep.objects.insert(SourceId::Commit(base), base_bytes);
        let (tip, tip_bytes) = commit_obj(2, vec![base]);
        ep.objects.insert(SourceId::Commit(tip), tip_bytes);
        let (side, side_bytes) = commit_obj(3, vec![]);
        ep.objects.insert(SourceId::Commit(side), side_bytes);

        serve(
            &mut ep,
            Request::UpdateBranch {
                branch: "main".to_string(),
                commit: tip,
                force: false,
            },
            None,
        )
        .unwrap();
        assert_eq!(ep.refs.branches[0].1, tip);

        assert_eq!(
            serve(
                &mut ep,
                Request::UpdateBranch {
                    branch: "main".to_string(),
                    commit: side,
                    force: false,
                },
                None,
            )
            .unwrap_err(),
            VersionError::PushNotFastForward
        );
        serve(
            &mut ep,
            Request::UpdateBranch {
                branch: "main".to_string(),
                commit: side,
                force: true,
            },
            None,
        )
        .unwrap();
        assert_eq!(ep.refs.branches[0].1, side);
    }

    #[test]
    fn transport_serve_auth_required_without_credentials() {
        let mut ep = endpoint_with_main();
        ep.auth_required = true;
        assert_eq!(
            serve(&mut ep, Request::GetRefs, None).unwrap_err(),
            VersionError::NoCredentials
        );
        // Serving with an active kid present succeeds; the transport layer
        // supplies it from the cred store.
        let Response::Refs(_) = serve(&mut ep, Request::GetRefs, Some("kid123")).unwrap() else {
            panic!("refs response");
        };
    }

    #[test]
    fn transport_mem_hub_shared_between_handles() {
        let a = MemTransport::open("t-shared-1", false).unwrap();
        let (id, bytes) = commit_obj(1, vec![]);
        a.put_batch(&[(SourceId::Commit(id), bytes)], &[]).unwrap();
        let b = MemTransport::open("t-shared-1", false).unwrap();
        assert_eq!(b.has_many(&[SourceId::Commit(id)]).unwrap(), vec![true]);
        let c = MemTransport::open("t-shared-2", false).unwrap();
        assert_eq!(c.has_many(&[SourceId::Commit(id)]).unwrap(), vec![false]);
    }

    #[test]
    fn transport_mem_walk_follows_parents_and_snap_lists() {
        let t = MemTransport::open("t-walk-1", false).unwrap();
        let (a, a_bytes) = commit_obj(1, vec![]);
        let (b, b_bytes) = commit_obj(2, vec![a]);
        t.put_batch(
            &[
                (SourceId::Commit(a), a_bytes),
                (SourceId::Commit(b), b_bytes),
            ],
            &[],
        )
        .unwrap();
        t.replace_refs(&RemoteRefs {
            default_branch: "main".to_string(),
            branches: vec![("main".to_string(), b)],
        })
        .unwrap();
        let closure = t.walk(b).unwrap();
        assert!(closure.contains(&SourceId::Commit(a)));
        assert!(closure.contains(&SourceId::Commit(b)));
        let missing = CommitId([0xAB; 20]);
        assert_eq!(
            t.walk(missing).unwrap_err(),
            VersionError::ChunkNotFound(missing.to_hex())
        );
    }

    #[test]
    fn transport_mem_get_batch_missing_object_names_hash() {
        let t = MemTransport::open("t-missing-1", false).unwrap();
        let missing = SourceId::Snapshot(SnapshotId([0xCD; 20]));
        assert_eq!(
            t.get_batch(&[missing]).unwrap_err(),
            VersionError::ChunkNotFound(missing.to_hex())
        );
    }

    #[test]
    fn transport_file_roundtrip_and_corrupt_refs() {
        let dir = tempfile::tempdir().unwrap();
        let t = FileTransport::open(dir.path().to_path_buf());
        assert_eq!(
            t.get_refs().unwrap_err(),
            VersionError::FailedReadRemoteRefs
        );
        let (id, bytes) = commit_obj(1, vec![]);
        t.put_batch(&[(SourceId::Commit(id), bytes.clone())], &[])
            .unwrap();
        t.replace_refs(&RemoteRefs {
            default_branch: "main".to_string(),
            branches: vec![("main".to_string(), id)],
        })
        .unwrap();
        assert_eq!(t.find_branch("main").unwrap(), Some(id));
        assert_eq!(t.default_branch().unwrap(), "main");
        assert_eq!(t.get_batch(&[SourceId::Commit(id)]).unwrap()[0].1, bytes);
        assert!(t.walk(id).unwrap().contains(&SourceId::Commit(id)));

        // Corrupt the refs file: the trailing hash no longer matches.
        let refs_path = dir.path().join("refs");
        let raw = std::fs::read(&refs_path).unwrap();
        let mut bad = raw.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        std::fs::write(&refs_path, bad).unwrap();
        assert_eq!(
            FileTransport::open(dir.path().to_path_buf())
                .get_refs()
                .unwrap_err(),
            VersionError::FailedReadRemoteRefs
        );
    }

    #[test]
    fn transport_file_update_branch_enforces_fast_forward() {
        let dir = tempfile::tempdir().unwrap();
        let t = FileTransport::open(dir.path().to_path_buf());
        let (base, base_bytes) = commit_obj(1, vec![]);
        let (tip, tip_bytes) = commit_obj(2, vec![base]);
        let (side, side_bytes) = commit_obj(3, vec![]);
        t.put_batch(
            &[
                (SourceId::Commit(base), base_bytes),
                (SourceId::Commit(tip), tip_bytes),
                (SourceId::Commit(side), side_bytes),
            ],
            &[],
        )
        .unwrap();
        t.replace_refs(&RemoteRefs {
            default_branch: "main".to_string(),
            branches: vec![("main".to_string(), base)],
        })
        .unwrap();
        t.update_branch("main", tip, false).unwrap();
        assert_eq!(t.find_branch("main").unwrap(), Some(tip));
        assert_eq!(
            t.update_branch("main", side, false).unwrap_err(),
            VersionError::PushNotFastForward
        );
        t.update_branch("main", side, true).unwrap();
        assert_eq!(t.find_branch("main").unwrap(), Some(side));
    }

    #[test]
    fn transport_file_snap_lists_survive_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let t = FileTransport::open(dir.path().to_path_buf());
        let (id, bytes) = commit_obj(1, vec![]);
        let snap = SnapshotId([0xEE; 20]);
        t.put_batch(
            &[
                (SourceId::Commit(id), bytes),
                (SourceId::Snapshot(snap), vec![7; 33]),
            ],
            &[(id, vec![snap])],
        )
        .unwrap();
        let closure = t.walk(id).unwrap();
        assert!(closure.contains(&SourceId::Commit(id)));
        assert!(closure.contains(&SourceId::Snapshot(snap)));
    }

    #[test]
    fn transport_batch_size_constant() {
        assert_eq!(SYNC_BATCH_SIZE, 256);
    }
}
