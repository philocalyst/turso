# Remotes Protocol Skill

- `RemoteConfig { name, url }`, `TrackingRef`, `SourceId::Commit/Snapshot`, `SnapshotId([u8;20])` distinct from `ChunkHash`/`CommitId`.
- Registry: `VcStore { remotes: Vec<RemoteConfig>, tracking: BTreeMap<(String,String), CommitId> }`. Names trimmed, `/` rejected.
- URLs: `file://` and `mem://` only; `http` refused with `failed to open remote (URL must start with file:// or mem://)`.
- Push: closure = commit DAG + snapshots, `has_many` batch 256, `put_batch` batch 256, refs swap last.
- Fetch: `has_many` missing, `get_batch` 256, B4 verification before store.
- Pull: fetch then fast-forward or merge; refusals `cannot pull with uncommitted changes`, `cannot pull non-current branch without fast-forward`, `cannot merge a non-fast-forward pull in a lazy store; materialize the store first`.
- Clone: empty check `database is not empty — clone into a fresh database`, `--lazy` registers `OriginSource`.
- Transport trait `RemoteTransport { get_refs, has_many, get_batch, update_branch }`, `open_transport` dispatches.
- `file://` refs file: `[ver:1][default_branch_len:2][branch...][commit:20][blake3]` verified on read.
- Auth: `?auth=required` endpoints require active `MemCredStore` credential; `no credentials; run SELECT dolt_creds_new()`.
- `ChunkSource` trait for lazy: `get`, `get_many`, one `get_many` per commit prefetch, verification, atomic batch.
- Errors byte-exact per `RUBRIC_O5.md` §9.
