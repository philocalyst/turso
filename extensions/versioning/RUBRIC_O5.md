# O5 Rubric — Remotes + lazy sources + GC + compat contract (extensions/versioning + glue)

Reference: https://github.com/dolthub/doltlite (master @ 2026).
Key doltlite refs: #144 `72cb1f0e` (filesystem remotes: push/fetch/pull/clone),
#2551 (host-pluggable chunk source callbacks), #2552 (origin-backed lazy
clones), #1546 `4a9930c53` (dolt_gc fresh exclusive view + VACUUM hook),
#1898 (machine-readable SQLite compat contract TSV).
Local reference copies: `/var/folders/vf/qpw72bpn65g0y01bnbwf90n80000gn/T/opencode/doltlite-ref/`.

O1–O4 dependencies (build on them, do not fork them): `VersionStore`/
`InMemoryStore` (`store.rs`), `blake3_chunk_hash` (`chunk.rs`), `Commit`/
`hash_commit`/`encode_v2`/`decode_v2`/`MemCommitStore` (`commit.rs`),
`RefName`/`RefNs`/`MemRefStore`/`compare_and_swap` (`refs.rs`), `VcStore`
(`staging.rs`: commits + refs + per-commit `snapshots` + staging + gates),
`SessionBranch` (`session.rs`), `VcOperations` specs (`funcs.rs`), glue
`extensions/core/src/versioning.rs` + `vc_vtabs.rs`, `VcState` stored on the
core connection (`core/database.rs:2368`+, `conn.versioning`).

Out of scope (O6 sibling owns; never touch): `testing/doltlite-oracle/`,
`benches/`, `RUBRIC_O6.md`.

## 0. BLOCKERS — RESOLVE, NEVER WAIVE

B1. Crate-cycle gate (same as O3/O4): `turso_versioning` MUST NOT import
`turso_ext`/`core/`. Pure remote/creds/gc/compat logic lives in the
versioning crate with ZERO `turso_ext`/`core/` imports and ZERO new crate
dependencies (std + existing blake3/sha2/thiserror/hex only). Thin
`ScalarFunc`/`VTabModule` glue lives in `turso_ext`; the VACUUM hook in
`core/` is the one core change and stays minimal.

B2. O1 stub debt is owned HERE, not waived: `gc.rs` `GcSnapshot::commits`
is documented "traversal resolves in O5". O5 supplies commit traversal:
mark walks refs → commits → parents + per-commit snapshot objects. The O1
test `gc_commits_field_documented` pins the OLD sweep-commit-only-chunks
behavior and explicitly says "fixing that is O5" — rewrite it to pin the
NEW behavior (commit-reachable objects survive); the rewrite is part of
this change, not a silent deletion.

B3. Remote operations are atomic per command: a failed push/fetch/pull/
clone leaves local refs, tracking refs, and (for pull/clone) the working
set exactly as before. No half-applied remote state, ever. Objects are
transferred BEFORE any ref moves (crash leaves unreachable objects, never
a dangling ref) — same ordering doltlite uses.

B4. Verification before persistence: every object fetched from a remote or
a lazy source is hash-verified against its requested id BEFORE it enters
the store. Corrupt bytes are rejected (`chunk verification failed: <hex>`),
never stored, and the active statement unwinds without poisoning the
connection (next statement works; doltlite #2551 contract).

B5. No network: `http://`/`https://` URLs are refused with
`failed to open remote (URL must start with file:// or mem://)`. The
transport trait is the seam where a real HTTP client registers later.
Do not add a network dependency to any crate in this change.

## 1. `src/remote.rs` — remote model, registry, commands

From [doltlite_remote.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_remote.c),
[doltlite_remote_sql.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_remote_sql.c).

```rust
pub struct RemoteConfig { pub name: String, pub url: String }
pub fn normalize_remote_name(raw: &str) -> VersionResult<String>; // trim; reject '/' and empty
pub struct TrackingRef { pub remote: String, pub branch: String, pub commit: CommitId }
```

Rules:
- `VcStore` gains `remotes: Vec<RemoteConfig>` and
  `tracking: BTreeMap<(String, String), CommitId>` (key = remote, branch).
  Adding a duplicate name → `remote already exists`; removing a missing one
  → `remote not found`; removing a remote also drops its tracking entries
  (doltlite `chunkStoreDeleteRemote`).
- Names are trimmed on add/remove and stored trimmed (`'  spaced  '` →
  `spaced`); a name containing `/` (or empty after trim) →
  `remote name invalid`.
- URLs: `file://<path>` (directory remote) and `mem://<name>` (in-process
  remote; unique per test, e.g. `mem://vc-remotes-r1`). `mem://<name>?auth=required`
  marks the endpoint as requiring an active credential (§7). Anything else
  (including `http://`, `https://`, bare paths) → B5 error.
- Commands are free functions over `(&mut VcStore, &dyn RemoteTransport, …)`
  in `remote.rs`; pure specs on `VcOperations` (funcs.rs) delegate to them.

### Push (doltlite `doltPushParsedFunc`, remote.c sync-out)
- Resolve local branch tip; missing → `branch not found: NAME`.
- Compute the closure: commit DAG from the tip (parents) + every reachable
  commit's snapshot objects. Ask the remote `has_many` in batches of 256
  (`SYNC_BATCH_SIZE`), upload the missing objects in batches of 256, then
  update the remote branch ref. Remote branch may be created freely; an
  existing remote tip must be an ancestor of the new tip unless `--force`,
  else → `not a fast-forward of the remote branch (use force to overwrite)`.
- Returns `0` (integer) on success, including the already-up-to-date no-op
  (no object re-upload — Rust test proves the second push transfers zero
  objects).

### Fetch (doltlite `doltFetchFunc`, remote.c sync-in)
- Read the remote refs blob (missing/unreadable → `failed to read remote
  refs`); find the branch; missing → `fetch failed: branch not found on
  remote`. Compute local closure needs from the remote tip, download
  missing objects in 256-batches with B4 verification, update the tracking
  ref `(remote, branch)`. Local branches are NOT touched.
- Returns `0`; up-to-date fetch transfers zero objects.

### Pull (doltlite `doltPullFunc`)
- Fetch first; then: uncommitted working/staged changes → `cannot pull with
  uncommitted changes`. Branch ≠ current session branch and not a
  fast-forward → `cannot pull non-current branch without fast-forward`.
  Non-fast-forward on the CURRENT branch of a lazy store → `cannot merge a
  non-fast-forward pull in a lazy store; materialize the store first`
  (byte-for-byte, both lines). Fast-forward: move the branch ref, replay
  snapshots into the working set (write-back via the existing
  `sync_work_to_sql` path), update tracking. Returns `0`.
- Tracking ref missing after a successful fetch → `tracking branch not found
  after fetch` (defense in depth; fetch must guarantee it exists).

### Clone (doltlite `doltCloneFunc`)
- Target store must be empty (no commits, no remotes, no tracked tables)
  else → `database is not empty — clone into a fresh database` (em dash).
- Full clone: copy ALL remote branches' closures + the remote's default
  branch name; create local branch tips, tracking refs for every remote
  branch, register remote `origin` at the cloned URL, check out the default
  branch, materialize tables via write-back. Returns `0`.
- `--lazy`: fetch the refs blob ONLY — create branch tips + tracking refs +
  `origin`, mark the store lazy, register the origin-backed source (§3).
  Tables materialize on first read through the source. Returns `0`.
- Clone into a store that already cloned → the not-empty error (test:
  `dolt_clone` twice).

## 2. `src/remote_wire.rs` — canonical encoding + batch protocol

The durable objects this engine versions are commits (V2 codec, sha256 id)
and per-commit table snapshots. Snapshots get a canonical codec now:

```rust
pub fn encode_snapshot(snap: &TableSnapshot) -> Vec<u8>;   // deterministic: sorted pk, typed values
pub fn decode_snapshot(bytes: &[u8]) -> VersionResult<TableSnapshot>;
pub fn snapshot_id(snap: &TableSnapshot) -> SnapshotId;    // blake3, 20 bytes, of canonical bytes
pub struct SnapshotId(pub [u8; 20]);
```

- `SnapshotId` is distinct from `ChunkHash`/`CommitId` (same typed-hash
  discipline as model.rs). `VcStore` keeps `snapshots` keyed by CommitId;
  O5 adds id-based lookup so closure/has-checks and lazy misses work by
  content address.
- Refs blob (remote-side state), version byte 1:
  `[ver:1][default_branch_len:2][default_branch][num_branches:2]
  per branch: [name_len:2][name][commit:20]`. Encode/decode + round-trip +
  truncation/corruption tests (borrow the shape of doltlite refs v4,
  minus the remote/tracking sections a remote never stores).
- Batch protocol (both directions), reused by transports, remotesrv, and
  the lazy source: `has_many(ids) -> Vec<bool>` and
  `get_batch(ids) -> Vec<(id, bytes)>` at most 256 ids per call.
- Tamper tests: flip one byte of a snapshot encoding → decode fails or id
  mismatch; wrong-id bytes → `chunk verification failed: <hex>`.

## 3. `src/source.rs` — lazy chunk-source traits (doltlite #2551)

```rust
pub trait ChunkSource: Send + Sync {
    fn get(&self, id: SourceId) -> VersionResult<Vec<u8>>;          // names the hash on failure
    fn get_many(&self, ids: &[SourceId]) -> VersionResult<Vec<Option<Vec<u8>>>>;
}
pub enum SourceId { Commit(CommitId), Snapshot(SnapshotId) }
```

Rules (all pinned by tests):
- Reads through `VcStore` on a lazy store resolve misses by calling the
  registered source (`Arc<dyn ChunkSource>`), verifying (B4), inserting,
  then serving. The source is consulted at most once per id per successful
  fetch; a second read of the same id makes no source call (counting fake
  in tests).
- `OriginSource` wraps a `RemoteTransport` + remote name: a miss asks the
  remote for the object by id. `clone('--lazy', url)` registers it (§1).
- Prefetch: when a commit's table is read on a lazy store, the snapshot
  ids for that commit are fetched with ONE `get_many` batch (doltlite
  `xGetMany` interior-node prefetch analog). A counting test pins the
  batch count.
- Source failure modes surface as errors that NAME the id: missing →
  `chunk not found: <hex>`; corrupt → `chunk verification failed: <hex>`
  (B4). A failed multi-object fetch leaves the earlier objects of that
  batch unstored (one batch = one atomic insert set; doltlite #2551
  "failed multi-chunk fetch did not leave the earlier chunk").
- No source registered + local miss → the store's existing absent-result
  behavior (`commit not found: <hex>` for commits; empty/`table not found`
  for snapshots) — read path unchanged when not lazy (doltlite
  "Preserve" contract).
- Writes (`dolt_commit`) on a lazy store force materialization of the
  branch tip first (fetch-on-write), so a new commit's parent chain is
  local before the ref moves.

## 4. `src/remote_transport.rs` — transport trait, mem hub (remotesrv), file remote, auth

```rust
pub trait RemoteTransport {
    fn get_refs(&self) -> VersionResult<Vec<u8>>;                    // refs blob (§2)
    fn has_many(&self, ids: &[SourceId]) -> VersionResult<Vec<bool>>;
    fn get_batch(&self, ids: &[SourceId]) -> VersionResult<Vec<(SourceId, Vec<u8>)>>;
    fn update_branch(&self, branch: &str, commit: CommitId, force: bool) -> VersionResult<()>;
    fn default_branch(&self) -> VersionResult<String>;
}
pub fn open_transport(url: &str) -> VersionResult<Box<dyn RemoteTransport>>; // URL dispatch, B5 errors
```

- `mem://` hub: process-wide registry (global `Mutex<HashMap<…>>`) of
  in-memory endpoints. THIS IS THE REMOTESRV: each endpoint is a served
  remote; `RemoteServer::serve(endpoint, Request) -> Response` exposes the
  same typed request/response the batch protocol defines (`GetRefs`,
  `HasMany`, `GetBatch`, `UpdateBranch`, wrapped with an auth check), so
  both transports and any future TCP/HTTP front end go through one code
  path. No sockets (B5). Integration test drives `serve` directly and then
  proves the mem transport produces identical outcomes.
- `file://` remote: a directory holding `refs` (refs blob + trailing
  blake3 of the blob, verify on read like doltlite `xGetRefs` hash check)
  and `objects/<hex>` files. Object files are written+fsynced BEFORE the
  refs swap; refs swap is write-temp-then-rename (atomic). A truncated or
  hash-mismatched refs file → `failed to read remote refs`.
- Auth: `?auth=required` endpoints refuse every request without an active
  credential → `no credentials; run SELECT dolt_creds_new()`. With an
  active credential (any credential the local cred store issued —
  in-process trust model, documented in creds.rs) requests proceed.
  `file://` ignores credentials.

## 5. `src/gc.rs` — full mark-sweep + exclusive gate + VACUUM hook (#1546)

- Replace the stub's unused `commits: Vec<CommitHash>` with real traversal
  input: `commit_refs: HashMap<CommitId, (Vec<CommitId>, Vec<SnapshotId>)>`
  (parents, snapshot objects). `mark()` now walks working sets AND
  commits; B2's rewritten test pins that commit-reachable objects survive.
- Store-level entry point:
  `pub fn collect_garbage(store: &mut VcStore) -> GcStats` marks from:
  all branch tips, tags, tracking refs, the detached pin, working/staged
  snapshots; walks parents + snapshots; sweeps unreachable commits and
  snapshots from the store; returns counts.
- `SELECT dolt_gc()` (funcs spec + glue): runs `collect_garbage`; result
  text `"%d chunks removed, %d chunks kept"` (doltlite format; our objects
  are the chunks' analog). Refuses with `gc requires exclusive access`
  (Busy-coded, like `DatabaseLocked`) when: the SQL session is inside a
  BEGIN txn, OR uncommitted working/staged changes exist, OR a
  merge/rebase/conflict is open. Track SQL txn state via the existing
  core→store event hook (`savepoint_sealed`/`preserve_staging_on`
  callers in core): extend the event set with BEGIN (enter) and
  COMMIT/ROLLBACK (exit).
- VACUUM hook (mirror of doltlite vacuum.c + #1546):
  - `VACUUM INTO` on a store with versioned content (any commit, tracked
    table, or remote) → parse/execute error `VACUUM INTO is not supported
    for versioned databases` (engine-neutral wording; doltlite says
    "doltlite databases" — substitution is deliberate and documented
    here). Gate where `core/translate/vacuum.rs:28` matches `into`, using
    connection-held versioning state.
  - In-place `VACUUM` on a versioned store first runs the compaction
    variant `gc_compact(store) -> GcStats` — non-exclusive, silently
    skipped when the exclusive preconditions fail (open txn, uncommitted
    changes; doltlite `doltliteGcCompactWithPhase` skip rules), errors
    otherwise propagate with phase context. Wire from `core/vdbe/vacuum.rs`
    through the connection's `versioning` field (`core/database.rs`).

## 6. Compat contract TSV + NOTADB + clustered-PK rowid alias (#1898)

- `extensions/versioning/tests/sqlite_compat_contract.tsv`: tab-separated
  `claim \t evidence` rows, one claim per line, `#` comments allowed.
  Minimum claim set (adapted honestly from #1898; every row's evidence
  must be a needle that exists — file path + substring, or test name):
  1. `versioning_manifest_magic_is_not_sqlite` → `model.rs FileMagic::DLTC`
     + compat test asserting manifest bytes differ from
     `SQLite format 3\0` in the first 16 bytes.
  2. `notadb_on_mismatch` → `model_manifest_rejects_bad_magic` + compat
     test: a foreign-magic manifest decodes to `bad file manifest`, the
     same class of refusal SQLite reports as NOTADB.
  3. `integer_pk_is_rowid_alias` → sqltest vc_compat (rowid == pk value,
     INSERT via rowid lands in the pk column).
  4. `versioned_identity_is_pk_never_rowid` → staging/compat tests:
     snapshots key rows by declared PK; rowid is never captured; deleting
     + reinserting identical PK rows (rowids change) leaves
     `dolt_hashof_table` unchanged.
  5. `non_integer_pk_tables_are_clustered_by_pk` → vc diff/merge tests
     (O3/O4) + compat test asserting snapshot ordering follows PK order,
     not rowid order. SQL layer keeps standard SQLite rowid semantics —
     the divergence from doltlite's rowid-less storage is stated in the
     TSV as a comment row.
- `tests/compat_contract.rs`: parses the TSV, fails when any evidence
  needle is missing from the named file (drift gate), and runs the
  behavior assertions above (manifest-vs-SQLite header, decode refusal,
  hashof-unchanged-under-rowid-churn, PK-order pinning).

## 7. `src/creds.rs` — credentials

```rust
pub struct Credential { pub kid: String, pub secret: [u8; 32] }
pub trait CredStore { fn issue(&mut self) -> Credential; fn list(&self) -> Vec<String>;
                      fn active(&self) -> Option<String>; fn set_active(&mut self, kid: &str) -> VersionResult<()>;
                      fn remove(&mut self, kid: &str) -> VersionResult<()>; }
```

- Process-wide `MemCredStore` (same lifetime model as the mem hub).
  `kid` = blake3-derived hex of the secret; secret derives from
  blake3(counter ‖ steady-time ‖ kid-round) — NO crypto RNG dependency;
  the module doc states plainly this is the seam where a real keypair
  lands and is not a cryptographic identity.
- SQL surface: `dolt_creds_new()` → kid text; `dolt_creds()` → one kid per
  row (active first); `dolt_creds('use', kid)` → kid (sets active);
  `dolt_creds('rm', kid)` → removed kid. Errors: missing kid →
  `no such credential`; wrong arity/shape →
  `usage: dolt_creds('rm', <kid>)` (doltlite string); auth paths per §4.

## 8. SQL surface (turso_ext glue, thin)

| # | doltlite origin | turso surface | notes |
|---|---|---|---|
| M1 | `dolt_remote` | `SELECT dolt_remote('add'\|'remove', name[, url])` | §1 errors byte-exact |
| M2 | `dolt_push` | `SELECT dolt_push(remote, branch[, '--force'])` | 0 on success; `remote and branch required`; unknown flag → `unknown option: FLAG` (house style) |
| M3 | `dolt_fetch` | `SELECT dolt_fetch(remote[, branch])` | §1 errors |
| M4 | `dolt_pull` | `SELECT dolt_pull(remote, branch)` | §1 errors |
| M5 | `dolt_clone` | `SELECT dolt_clone(['--lazy',] url)` | §1 errors |
| M6 | `dolt_gc` | `SELECT dolt_gc()` | §5 result + Busy-coded refusal |
| M7 | `dolt_creds_new`/`dolt_creds` | §7 | |
| V1 | `dolt_remotes` | vtab `name TEXT, url TEXT, fetch_specs TEXT, params TEXT` | fetch_specs literal `["refs/heads/*:refs/remotes/<name>/*"]`, params `{}` (doltlite remConnect schema) |
| V2 | `dolt_remote_branches` | vtab `name TEXT, hash TEXT, latest_commit_message TEXT` | rows `remotes/<remote>/<branch>` from tracking refs; optional TVF arg filters by name prefix |

Arity errors reuse `incorrect number of arguments to dolt_X` (house style)
where doltlite instead spells a usage message; both surfaces keep their
own strings per §9 — do not mix.

Push/pull/clone shims wrap the op in the O4 write-back discipline
(`vc_sync_shim` shape): pull and full clone apply the new working set via
`sync_work_to_sql`; a write-back failure restores prior refs/tracking
(B3).

## 9. Exact errors (byte-for-byte)

Reused (do not reword): all O1–O4 strings incl. `branch not found: NAME`,
`commit not found: HASH`, `database is locked`, `cannot write in detached
HEAD state`, `incorrect number of arguments to dolt_X`.

New (doltlite verbatim):
- `usage: dolt_remote(action, name [, url])`
- `action and name required`
- `url required for add`
- `too many arguments`
- `remote name invalid`
- `remote already exists`
- `remote not found`
- `unknown action: use 'add' or 'remove'`
- `remote and branch required`
- `usage: dolt_fetch(remote [, branch])`
- `remote name required`
- `branch name required`
- `fetch failed: branch not found on remote`
- `failed to read remote refs`
- `fetch failed`
- `usage: dolt_pull(remote, branch)`
- `tracking branch not found after fetch`
- `cannot pull non-current branch without fast-forward`
- `cannot pull with uncommitted changes`
- `cannot merge a non-fast-forward pull in a lazy store; materialize the store first`
- `not a fast-forward of the remote branch (use force to overwrite)`
- `usage: dolt_clone(['--lazy'], url)`
- `url required`
- `database is not empty — clone into a fresh database`
- `gc requires exclusive access`
- `no credentials; run SELECT dolt_creds_new()`
- `no such credential`
- `usage: dolt_creds('rm', <kid>)`

New (ours, documented divergences):
- `failed to open remote (URL must start with file:// or mem://)` — doltlite
  allows http; we refuse (B5) and the accepted schemes are ours.
- `chunk not found: <hex>` / `chunk verification failed: <hex>` — StoreError
  shape extended to carry the id.
- `VACUUM INTO is not supported for versioned databases` — engine-neutral
  wording of doltlite's "…for doltlite databases".
- `authentication required` is NOT used — the no-credential case uses the
  doltlite `no credentials; …` string above.

## 10. Files to create/modify

- `extensions/versioning/src/remote.rs` — §1 (model, registry, push/fetch/
  pull/clone commands).
- `extensions/versioning/src/remote_wire.rs` — §2 (snapshot codec, ids,
  refs blob, batch types).
- `extensions/versioning/src/source.rs` — §3 (ChunkSource, OriginSource,
  prefetch, lazy miss path).
- `extensions/versioning/src/remote_transport.rs` — §4 (trait, mem hub +
  RemoteServer, file remote, auth, `open_transport`).
- `extensions/versioning/src/creds.rs` — §7.
- `extensions/versioning/src/gc.rs` — §5 (B2 rewrite + collect_garbage +
  gc_compact).
- `extensions/versioning/src/model.rs` — extend StoreError/VersionError
  with §9 variants only.
- `extensions/versioning/src/staging.rs` — remotes/tracking fields, lazy
  flag + registered source, snapshot-id index, BEGIN/COMMIT txn tracking.
- `extensions/versioning/src/funcs.rs` — M1–M7 pure specs.
- `extensions/versioning/src/lib.rs` — new modules + re-exports.
- `extensions/core/src/versioning.rs` + `vc_vtabs.rs` — M1–M7 shims, V1/V2
  vtabs, gc shim (Busy-coded), write-back discipline for pull/clone.
- `core/translate/vacuum.rs` / `core/vdbe/vacuum.rs` / `core/database.rs`
  — minimal VACUUM-INTO gate + compaction hook via `conn.versioning`.
- `extensions/versioning/tests/remote_flow.rs` — mem-hub end-to-end
  (clone full/lazy, pull matrix, auth, gc interplay, prefetch counting,
  tamper, failure atomicity).
- `extensions/versioning/tests/remote_file.rs` — file:// transport flow +
  crash-ordering + corrupt refs.
- `extensions/versioning/tests/compat_contract.rs` +
  `tests/sqlite_compat_contract.tsv` — §6.
- `sqlite/conformance/turso-sqltests/vc_remotes.sqltest` — §8 surface over
  `mem://vc-remotes-*` URLs only (unique names; process-shared hub).
- `sqlite/conformance/turso-sqltests/vc_gc.sqltest` — gc result/refusals,
  VACUUM + VACUUM INTO.
- `sqlite/conformance/turso-sqltests/vc_compat.sqltest` — rowid alias +
  hashof-under-rowid-churn.

## 11. Tests (TDD: failing tests FIRST — show RED, then implement to GREEN)

- Rust unit per module: name rules, URL dispatch (B5), snapshot codec
  round-trip/truncate/tamper, refs blob round-trip/corrupt, batch cap 256,
  push closure + no-op second push, FF/force push matrix, fetch tracking,
  pull matrix (all three refusals + FF apply), clone full/lazy/twice/
  non-empty, source miss/verify/count/prefetch/atomic-batch, creds
  CRUD + auth required, gc traversal (B2 rewrite, tracking-anchored
  survival, sweep counts, exclusive refusals), VACUUM gates, compat TSV
  drift gate.
- Rust integration: the three files above.
- sqltests × 3 (names exact). Never skipped — wire the glue so they run.
- Validation (all green, paste full output): `cargo test -p
  turso_versioning`; `cargo clippy -p turso_versioning -p turso_ext
  -p turso_core --all-features --all-targets -- --deny=warnings`
  (pre-existing core warnings: none NEW from this change);
  `cargo fmt --check`; `make -C sqlite/conformance run-rust
  ARGS='--snapshot-filter __never__'` (at minimum vc_remotes/vc_gc/
  vc_compat pass, zero regressions in the vc_* and broader suites).

## 12. Review gates (brutal reviewer checks)

1. Correctness paramount: B3/B4 invariants proven by tests, not asserted.
   Crash > corrupt: refs move last, always.
2. Strong types: SnapshotId/SourceId never stringly; exact §9 strings.
3. No `turso_ext`/`core/` imports in versioning crate; no new deps; glue
   thin; core diff minimal and localized to VACUUM + existing seams.
4. Callers before callees; comments only why-level; plain language.
5. O2 LCA reuse for ancestry checks (never re-walk per parent).
6. No waived blockers; every fix logged in the rubric Status/Fix Log.
7. TDD evidence: RED output before GREEN for each deliverable group.

## Status

- [x] rubric written (this file)
- [x] v1 implemented
- [x] reviewer pass completed — findings logged below
- [x] fix loop to GREEN
- [x] validation output recorded
- [x] `.claude/skills/remotes-protocol/SKILL.md` written

## Status / Fix Log — 2026-09-04

- Remote commands now verify downloaded objects before persistence, transfer
  objects before refs, and restore local refs, tracking refs, lazy state, and
  working state after failures.
- The file transport uses unique temporary files, fsyncs objects before the
  ref swap, and rejects corrupt refs and objects. The memory transport follows
  the same typed server protocol and authorization rules.
- Lazy clones fetch verified objects in atomic batches, cache successful
  reads, materialize before writes, and write materialized state back to SQL.
- GC walks the full ref/commit/snapshot closure, honors its exclusive-access
  gate, and is wired to in-place `VACUUM`. `VACUUM INTO` keeps SQLite's
  in-transaction error precedence and otherwise refuses versioned databases.
- Direct SQL inserts, updates, deletes, table drops, failed/rolled-back table
  creation, and hard resets now keep the versioned working set exact. REAL and
  BLOB values preserve their types through capture, history, diff, and replay.
- A partial commit now removes only table drops explicitly included in its
  staged set. This closes the cross-table deletion leak found during the ORM
  row-commit review.
- ORM history lookups can build a reusable typed B-tree index by any row value.
  Hits carry persistable `(commit, primary key, verified row ordinal)` pointers
  that normally load the exact historical row in constant time.
- `nix develop -c cargo test -p turso_versioning`: 452 passed, 0 failed across
  all unit and integration test targets (380 library tests plus 72 integration
  tests).
- `nix develop -c cargo clippy -p turso_versioning -p turso_ext -p turso_core
  --all-features --all-targets -- --deny=warnings`: passed.
- `nix develop -c cargo fmt --all -- --check` and `git diff --check`: passed.
- Full pinned conformance run: SQLite 12,787 passed / 0 failed / 0 errors / 7
  skipped; Turso 1,675 passed / 0 failed / 0 errors / 341 skipped.
