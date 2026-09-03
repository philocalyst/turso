# O2 Rubric — Version model + branching + staging (extensions/versioning)

Reference: https://github.com/dolthub/doltlite (master @ 2026; per-session branching 9df792a1,
commit c7748b2ea, LCA rewrite #847 551e5598f, savepoint matrix #592–#616).
C-API surface: `doltlite_ref.c`, `chunk_refs.h` v7, `doltlite_commit.h#L13`.
Dolt behavior docs: README "Dolt Features", "Per-Session Branching Architecture", "Concurrency".

O1 dependency: `ChunkHash`, `VersionStore` traits. Merge point:
`extensions/versioning/src/model.rs` + `store.rs`.
If O1 traits are absent, define minimal local traits in `model.rs`
(`trait O1ChunkStore { fn get(&self,h:&ChunkHash)->Option<Vec<u8>>; fn put(&self,bytes:&[u8])->ChunkHash; }`,
`trait O1VersionStore` over refs/commits) and mark `// O1-MERGE:` adapters. Never block on O1.

## 1:1 API table (doltlite → turso)

| # | doltlite (blob link) | turso symbol | behavior (identical) |
|---|----------------------|--------------|----------------------|
| R1 | `src/doltlite_ref.c` → branch create/list/delete | `refs.rs: RefName{ns,name}`, `RefStore::create_branch/get_branch/list_branches/delete_branch` | `SELECT dolt_branch('f')` creates at HEAD; `-d` deletes; list = `dolt_branches` order by name. Errors: `branch 'f' already exists`, `branch not found: f`. |
| R2 | `src/chunk_refs.h` v7 refs/heads/*, refs/tags/* | `RefName::branch(name)`, `::tag(name)`, `parse_qualified(path)->(file, Revision)` | `my.db@branch`, `my.db/branch`, `my.db/v1`, `my.db/<40hex>`, `my.db/main~1` open paths. Unknown ref → `branch not found: X` / `invalid revision spec: X`. |
| R3 | per-session branching 9df792a1 | `session.rs: SessionBranch { current: Option<RefName>, detached: Option<CommitId> }` alongside `core/connection.rs:387` + `database.rs:520`, never inside pager | Each connection independent active branch; uncommitted working set belongs to branch. `active_branch()` NULL when detached. Checkout from detached reattaches + writable. |
| R4 | detached revisions (README) | `Revision::Hash/Tag/Ancestor`, `SessionBranch::open_detached` | Tag/hash/~N opens immutable snapshot, read-only; writes fail `cannot write in detached HEAD state`. Peer advance/delete does not move open snapshot. |
| R5 | revision specs HEAD/WORKING/STAGED/BRANCH/TAG/HASH, `~N`/`^N` suffixed, `a..b`/`a...b` | `refs.rs: Revision` enum + `parse_revision(s)` | `HEAD~N`/`HEAD^N` = Nth ancestor (first-parent); `a..b` endpoints; `a...b` = merge-base→right. Bad spec → `invalid revision spec: 'X'`. Kept as strings first, resolved via commit walk. |
| R6 | CAS ref update (`chunk_refs.h` v7) | `RefStore::compare_and_swap(name, expected: CommitId, next: CommitId) -> Result<(), RefError::Busy>` | Race loser gets `SQLITE_BUSY` (message `database is locked` at SQL layer). Stale tip must never clobber winner. VC ops re-confirm HEAD under lock. |
| C1 | `doltlite_commit.h#L13` commit struct, c7748b2ea | `commit.rs: Commit{parents:Vec<CommitId>, root:RootHash, meta:CommitMeta{name,email,message,timestamp}}`, `CommitId`, V2 codec `encode_v2/decode_v2` | 40-char lowercase hex ids = hash of canonical V2 bytes. Codec round-trips; corrupt bytes → `invalid commit encoding`. |
| C2 | LCA rewrite #847 551e5598f | `commit.rs: merge_base(store, a, b) -> Option<CommitId>` | Lowest common ancestor by generation; none → None (unrelated histories). Used by merge/cherry-pick/`...` ranges. |
| C3 | ancestor walk | `ancestors(store, id)`, `is_ancestor(store, a, b)` | First-parent `~N` resolution + `dolt_log('a..b')` filtering. Missing commit → `commit not found: <hash>`. |
| S1 | `dolt_add` | `staging.rs: dolt_add(tables: &[&str])`, `add_all()` for `-A` | Stage working→staged per table; `-A` stages all incl. new. Unknown table → `table not found: X`. Honors ignore patterns (O4 owns table, call `is_ignored(name)` hook). |
| S2 | `dolt_commit` + gate | `dolt_commit(msg, author_override) -> CommitId` | Requires staged or `-A`; refuses with live conflicts: `cannot commit: unresolved merge conflicts`; refuses with violations unless force: `cannot commit: constraint violations remain`. Empty commit → `nothing to commit`. |
| S3 | `dolt_status` / `dolt_reset` / `dolt_clean` / `dolt_config` | `status() -> Vec<StatusRow{table,staged:bool,status}>`, `reset(--soft/--hard)`, `clean()`, `config_get/set` | Status values: `new table|modified|deleted|renamed`. `--soft` unstage keep working; `--hard` discard uncommitted. Config per-connection, not persisted; `user.name`/`user.email` required by commit else `invalid author: user.name and user.email must be set` (unless `--author` override). |
| S4 | savepoint seal/preserve matrix #592–#616 | `staging.rs: savepoint_sealed(sql_state) -> bool`, `preserve_staging_on(event)` | Staging (branch-scoped working/staged sets) survives COMMIT/ROLLBACK of SQL txn; sealed (invisible) inside savepoints per matrix. Document matrix in code comment. |
| F1 | scalar fns, `SELECT dolt_*() only`, no PRAGMA/CALL | `funcs.rs` via `extensions/core/src/functions.rs ScalarFunc` + `lib.rs ExtensionApi` | `dolt_add/commit/branch/checkout/tag/active_branch/hashof/hashof_table/hashof_db/config/version/doltlite_engine`. Wrong arity → `incorrect number of arguments to dolt_X`. Unknown fn → normal SQLite no-such-function. |
| F2 | `dolt_hashof*` | `hashof(rev)`, `hashof_table(name[, rev])`, `hashof_db([rev])` | 40-hex lowercase; table/db hashes history-independent (same key-set → same hash). Bad rev → `invalid revision spec`. |
| F3 | `dolt_version`, `doltlite_engine` | `version() -> "vX.Y.Z"`, `engine() -> "prolly"` | version from crate version; engine always `prolly` in versioning crate. |

## Exact error strings (must match byte-for-byte)

- `cannot commit: unresolved merge conflicts`
- `cannot commit: constraint violations remain`
- `nothing to commit`
- `branch 'NAME' already exists`
- `branch not found: NAME`
- `commit not found: HASH`
- `invalid revision spec: 'SPEC'`
- `invalid author: user.name and user.email must be set`
- `cannot write in detached HEAD state`
- `table not found: NAME`
- `database is locked` (BUSY surface)
- `incorrect number of arguments to dolt_X`

## Busy / snapshot semantics

- One durable writer at a time; concurrent write-begin → `SQLITE_BUSY`.
- Snapshot upgrade after peer advance → `SQLITE_BUSY_SNAPSHOT`.
- `compare_and_swap` returns `RefError::Busy` on race → maps to SQLITE_BUSY, never lost update.
- Detached opens pin commit snapshot; readers stay live across GC/writer.

## Files to create (crate extensions/versioning)

- `extensions/versioning/Cargo.toml` (name `turso_versioning`, lib crate, deps: sha2/hex/thiserror or std-only equivalents + workspace deps allowed; add to workspace members)
- `extensions/versioning/src/lib.rs` (re-exports, no logic)
- `extensions/versioning/src/model.rs` (ChunkHash/RootHash newtypes + O1-merge traits)
- `extensions/versioning/src/refs.rs` (RefName, Revision, RefStore trait + MemRefStore, CAS)
- `extensions/versioning/src/commit.rs` (Commit/Meta/Id, V2 codec, ancestors, merge_base)
- `extensions/versioning/src/staging.rs` (StagingSet, add/commit/status/reset/clean/config, gate, savepoint matrix)
- `extensions/versioning/src/session.rs` (SessionBranch, qualified-path parsing, detached RO guard)
- `extensions/versioning/src/funcs.rs` (ScalarFunc impls, pure logic + injectable Store hooks, no core/ imports)
- `sqlite/conformance/turso-sqltests/vc_branch_basic.sqltest`
- `sqlite/conformance/turso-sqltests/vc_commit_gate.sqltest`
- `.claude/skills/versioning-model/SKILL.md` (orchestrator writes after review loop)

## Tests (must fail without change, pass with it)

- `.sqltest`: `vc_branch_basic.sqltest` (branch create/checkout/active_branch/list/delete, `@branch` path note, detached RO), `vc_commit_gate.sqltest` (add/commit/status, conflict gate error, empty-commit error, hashof determinism).
- Rust integration `extensions/versioning/tests/{refs_cas.rs, commit_lca.rs, staging_gate.rs}`: CAS race → Busy; LCA diamond; commit refused with conflicts; `~N`/`a...b` resolution.
- Validation: `cargo fmt`, `cargo clippy --workspace --all-features --all-targets -- --deny=warnings`, `cargo test -p turso_versioning`, `make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'`.

## Review gates (muse-spark reviewer, brutal)

1. Strong types everywhere (no stringly branch/commit); errors exact strings.
2. No `core/` imports from versioning crate; session state design keeps pager clean (companion struct + integration notes, not edits inside pager).
3. No new PRAGMA/CALL; SELECT-only scalar fns.
4. Code flows top-down (callers first), comments only why-level (SQLite quirk / invariant / bug ref).
5. Plain language (no "bootstrap-safe" jargon).

## Status

- [ ] v1 implemented (nemotron)
- [ ] reviewer pass 1 (muse-spark)
- [ ] v2/v3 fixes
- [ ] green: fmt + clippy + cargo test + sqltests
