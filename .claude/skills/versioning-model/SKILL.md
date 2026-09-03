# Versioning-Model Skill

## Overview

O2 version model + branching + staging for the `turso_versioning` crate in
`extensions/versioning/`, wired into SQL as `SELECT dolt_*()` scalar functions.
Builds on the O1 chunk store (`ChunkHash`, `VersionStore`); see
`.claude/skills/prolly-chunk-store/SKILL.md` for the storage layer.

Doltlite reference: https://github.com/dolthub/doltlite (per-session branching
`9df792a1`, commit `c7748b2ea`, LCA rewrite #847 `551e5598f`, savepoint matrix
#592-#616; C surface `doltlite_ref.c`, `chunk_refs.h` v7, `doltlite_commit.h#L13`).

## Key Patterns

### Strongly-Typed Refs and Commits

`CommitId` and `RootHash` are distinct `[u8; 20]` newtypes in
`extensions/versioning/src/model.rs` (sibling to O1's `ChunkHash`/`CommitHash`,
never interchangeable). Branch/tag names are `RefName { ns: RefNs, name }`
with `RefNs::{Heads, Tags}` — never bare strings. Revisions are the `Revision`
enum (`Head/Working/Staged/Branch/Tag/Hash/Ancestor/Parent/Range/
SymmetricDifference`); parse with `parse_revision`, open paths with
`parse_qualified_path` (`my.db@branch`, `my.db/branch`, plain path = `Head`).

### Error Strings Are Contract

`VersionError` Display strings match doltlite byte-for-byte and are pinned by
`version_error_displays_are_exact`. Examples: `branch 'NAME' already exists`,
`branch not found: NAME`, `commit not found: HASH`,
`invalid revision spec: 'SPEC'`, `cannot commit: unresolved merge conflicts`,
`cannot commit: constraint violations remain`, `nothing to commit`,
`invalid author: user.name and user.email must be set`,
`cannot write in detached HEAD state`, `table not found: NAME`,
`database is locked`, `incorrect number of arguments to dolt_X`,
`invalid commit encoding`. Return the enum, never a hand-built string.

### Ref Updates Use CAS, Busy Means Retry

`MemRefStore::compare_and_swap(name, expected: Option<CommitId>, next)` returns
`RefError::Busy` on a lost race; the SQL layer maps that (and only that) to
`SQLITE_BUSY` (errcode 5) with message `database is locked`, so clients retry.
All other errors surface as `CustomError` with the verbatim message. Never
clobber a stale tip: re-confirm HEAD under the graph lock.

### Commits: V2 Codec, Full-DAG LCA

`Commit { parents, root: RootHash, meta: CommitMeta }` encodes with `encode_v2`
(version byte 2, `u8` parent count with a hard `assert!`, LE lengths, LE `i64`
timestamp); `decode_v2` rejects truncation, wrong versions, and trailing bytes.
`CommitId` is SHA-256 of the canonical bytes truncated to 20 bytes (40-char
lowercase hex). `merge_base` walks **all** parents on both sides (BFS reachable
sets, longest-path generation); `ancestors` is first-parent-only for `~N` and
range filtering. Detached opens pin a `CommitId` by value: peer advance/delete
never moves the snapshot, and writes fail `cannot write in detached HEAD
state`. Detached state comes only from qualified-path opens — `dolt_checkout`
never detaches (doltlite behavior).

### Staging Gates and Savepoint Matrix

`VcStore` owns branch refs, the commit graph, `StagingSet` (working vs staged),
conflict/violation sets, and config. `dolt_commit` refuses in gate order:
conflicts, then violations (unless `force`), then empty (`nothing to commit`),
and requires `user.name`/`user.email` unless `--author` overrides. Staging
survives SQL `COMMIT`/`ROLLBACK` but is sealed inside savepoints
(`savepoint_sealed` / `preserve_staging_on`, matrix #592-#616 in
`staging.rs`). `dolt_status` values: `new table|modified|deleted|renamed`.

### SQL Surface Is SELECT-Only Scalars

No PRAGMA, no CALL: `dolt_add/commit/branch/checkout/tag/active_branch/
hashof/hashof_table/hashof_db/config/version/doltlite_engine` plus
`dolt_status/reset/clean`, registered as `ScalarFunc`s in
`extensions/core/src/versioning.rs` (`register_vc_functions`) with
per-connection `VcState` (`Mutex<VcStore>` + `Mutex<SessionBranch>` + graph
lock) owned by the connection companion field (`core/connection.rs`), never
the pager. `dispatch` special-cases `DatabaseLocked` to `ResultCode::Busy`;
`core/types.rs from_ffi_ref` maps `Busy` to `LimboError::Busy`. A CREATE TABLE
translate hook (`core/translate/schema.rs`) tracks new tables (conservative:
a later-failing statement can leave an inert tracked name).

## Where Things Live

- `extensions/versioning/src/refs.rs` — `RefName`, `RefNs`, `Revision`,
  `RefStore` + `MemRefStore`, CAS, qualified-path parsing.
- `extensions/versioning/src/commit.rs` — `Commit`, V2 codec, `CommitStore`,
  `ancestors`/`is_ancestor`/`merge_base`, `resolve_revision`.
- `extensions/versioning/src/staging.rs` — `VcStore`, `StagingSet`,
  commit gates, savepoint matrix.
- `extensions/versioning/src/session.rs` — `SessionBranch` (companion to
  `core/connection.rs` + `core/database.rs` state).
- `extensions/versioning/src/funcs.rs` — pure `dolt_*` specs + arity checks.
- `extensions/core/src/versioning.rs` — `ScalarFunc` registration, `VcState`,
  `dispatch` error mapping.
- Tests: `extensions/versioning/tests/{refs_cas,commit_lca,staging_gate}.rs`;
  `sqlite/conformance/turso-sqltests/vc_branch_basic.sqltest`,
  `vc_commit_gate.sqltest` (run: `make -C sqlite/conformance run-rust
  ARGS='--snapshot-filter __never__'`).

## Gotchas

- Toolchain: plain `cargo` fails on blake3-neon (missing clang); always use
  `nix develop -c cargo ...`.
- `parse_revision` tries `...` before `..`; `^N`/`~N` need numeric suffixes;
  empty range endpoints are rejected.
- `Revision::Range(a..b)` resolves to the right endpoint; `a...b` resolves to
  the merge base (documented simplification for later merge work).
- `dolt_status` is a scalar here; real dolt exposes a `dolt_status` table —
  reconcile at the next wire step. Merge itself is out of scope (conflict
  gate covered at Rust level only).
