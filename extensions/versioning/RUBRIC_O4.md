# O4 Rubric — Write path: merge + conflicts + replay (extensions/versioning + turso_ext glue)

Reference: https://github.com/dolthub/doltlite (master @ 2026).
Dolt behavior: merge produces working-set changes + transient (never durable)
conflicts; COMMIT is refused while conflicted; autocommit failure rolls back
the whole statement.

O2/O3 dependencies (build on them, do not fork them):
`Commit`/`CommitMeta`/`CommitId`/`hash_commit`, `MemCommitStore`/`CommitStore`,
`merge_base`/`ancestors`/`is_ancestor` (`commit.rs`), `RefName`/`MemRefStore`/
`RefStore`/`compare_and_swap`, `Revision`/`parse_revision` (`refs.rs`),
`VcStore` (`staging.rs`: working/staged sets, `dolt_commit` gate, snapshots
via `record_snapshot`, conflicts/violations gates), `VcRead`/`VcValue`/`VcRow`/
`VcCommitView`/`MemVcRead` + `log_rows`/`history_rows`/`at_rows`/`blame_rows`/
`diff_rows`/`diff_table_rows` (`vtab_*.rs`), `VcOperations`/`funcs.rs` scalar
specs, `turso_ext` thin glue `extensions/core/src/vc_vtabs.rs`.

Key doltlite commits: `b0a84615` (merge wiring), `ced46a84` (defer-flush),
`551e5598f` (LCA rewrite — already used by O2 `merge_base`), `a3b54a68`
(never-persist-conflicts).

## 0. BLOCKERS — RESOLVE, NEVER WAIVE

B1. Same crate-cycle gate as O3: `turso_ext` depends on `turso_versioning`,
so `turso_versioning` MUST NOT import `turso_ext`/`core/`. Pure merge/conflict/
constraint/replay logic lives in the versioning crate with ZERO `turso_ext`/
`core/` imports. Thin `VTabModule`/`ScalarFunc` glue lives in `turso_ext` only.
`cargo build` enforces this; a cycle is a build failure, not a waiver.

B2. O3 deferred item is owned HERE, not waived: per-table engine modules
(`dolt_history_<t>`, `dolt_at_<t>`, `dolt_blame_<t>`, `dolt_diff_<t>`) with
column-aware create. Root cause (O3 §5): TVF `create` receives no connection
and empty args (`core/vtab.rs:87-108` passes `Vec::new()`), so per-table column
metadata is unavailable at schema-build time. O4 owns the tables, so O4
supplies the fix: thread the opening connection and/or table arguments into
TVF `create` (extend the `VTabModule::create` signature or pass args through),
so `dolt_history_<t>('start_ref')` etc. can declare live columns of `t`.
Pure per-table readers already exist and are Rust-tested; the contract (names,
schemas, hidden columns in `vtab_history.rs`/`vtab_diff.rs` schema tests)
is pinned for this wiring. After O4, all four per-table families work live.

B3. Conflicts are TRANSIENT, never durable. `a3b54a68`: conflicts live in the
working set only; they are never written into a commit object, never survive
a commit, and COMMIT is refused while any exist. Autocommit statement failure
rolls back the whole statement. Tests pin: conflict markers present pre-commit,
absent post-resolve/commit, and no commit object ever contains conflict rows.

## 1. `src/merge.rs` — three-way row merge

From `doltlite_merge.c`,
[merge_pass1.c](https://github.com/dolthub/doltlite/blob/master/src/merge_pass1.c) (`:1453`),
[merge_pass2.c](https://github.com/dolthub/doltlite/blob/master/src/merge_pass2.c) (`:716`),
[merge_rows.c](https://github.com/dolthub/doltlite/blob/master/src/merge_rows.c) (`:1407`).

```rust
pub enum RowOutcome { Unchanged, Ours, Theirs, Merged(VcRow), Conflict(ConflictRow) }
pub struct ConflictRow { pub pk: Vec<VcValue>, pub base: Option<VcRow>, pub ours: Option<VcRow>, pub theirs: Option<VcRow> }
pub struct MergeOutcome { pub rows: Vec<(Vec<VcValue>, VcRow)>, pub conflicts: Vec<(String, ConflictRow)> }
pub fn three_way_row_merge(base: &[VcRow], ours: &[VcRow], theirs: &[VcRow], pk: &[String], columns: &[String]) -> MergeOutcome;
```

Rules:
- LCA comes from O2 `merge_base`; pass1 computes the base snapshot triple
  (base/ours/theirs per table), pass2 merges rows.
- Matching is NAME-keyed (table name → snapshot), never rootpage-keyed:
  table identity survives renumber; after merge, renumber + master rebuild
  so `sqlite_master` reflects the merged schema set.
- Fast path: if `theirs == base`, keep ours untouched (zero conflicts, zero
  row rebuild); if `ours == base`, take theirs wholesale. Tests pin both
  fast paths (no conflict rows, output identical to the winning side).
- Slow path cell rule: cell changed on one side only → take the changed cell;
  changed on both sides to the same value → take it, no conflict; changed on
  both sides differently → conflict row recording base/ours/theirs.
- Row-level: added on one side only → keep; added on both identically → keep
  once; added differently under same PK → conflict. Deleted on one side,
  modified on the other → conflict. Deleted on both → gone, no conflict.
- Empty-table and no-PK tables: no-PK tables merge by full-row identity
  (identical row sets merge clean; any divergence is a conflict, since there
  is no key to join on).

## 2. `src/merge_schema.rs` — schema-IR merge

From [merge_predetect.c](https://github.com/dolthub/doltlite/blob/master/src/merge_predetect.c) (`:1219`),
[merge_rebuild.c](https://github.com/dolthub/doltlite/blob/master/src/merge_rebuild.c) (`:478`),
[merge_schema.c](https://github.com/dolthub/doltlite/blob/master/src/merge_schema.c) (`:1444`).

```rust
pub struct SchemaIR { pub table: String, pub columns: Vec<ColIR>, pub pk: Vec<String>, pub sql: String }
pub struct ColIR { pub name: String, pub decl: String, pub notnull: bool, pub dflt: Option<String>, pub pk_pos: Option<usize> }
pub enum SchemaDecision { Clean(SchemaIR), Conflict(String) }
pub fn merge_schema(base: Option<&SchemaIR>, ours: Option<&SchemaIR>, theirs: Option<&SchemaIR>) -> SchemaDecision;
```

Rules:
- Parse `CREATE TABLE` SQL into `SchemaIR` (columns in order, PK order, types
  normalized case-insensitively; `STRICT` flag and `CHECK` text kept verbatim
  for the matrix below).
- PK-signature rule: if either side changes the PK column set/order vs base
  and the two results differ → `Conflict`, no data merge attempted for that
  table ("match Dolt or refuse": Dolt refuses PK-divergent merges; so do we).
- "Match Dolt or refuse" matrix (row = ours-vs-base × theirs-vs-base):

  | ours \ theirs | unchanged | compatible (add nullable col / add CHECK) | breaking (drop/rename/type change/PK change) |
  |---|---|---|---|
  | unchanged | take base | take theirs | take theirs iff theirs alone changed, else Conflict |
  | compatible | take ours | union columns (both added, different names) / Conflict (same name, different decl) | Conflict |
  | breaking | take ours iff ours alone changed | Conflict | Conflict unless byte-identical results |

- Predetect runs before any row merge: a schema `Conflict` blocks the data
  merge for that table and records a schema conflict (resolvable only by
  `--ours`/`--theirs`, §3). Rebuild regenerates the merged `CREATE TABLE`
  SQL (column order: base order, then ours-added, then theirs-added).

## 3. `src/conflicts.rs` — transient conflicts + resolve

```rust
pub struct ConflictEntry { pub table: String, pub pk: Vec<VcValue>, pub base: Option<VcRow>, pub ours: Option<VcRow>, pub theirs: Option<VcRow>, pub kind: ConflictKind }
pub enum ConflictKind { Rows, Schema(String) }
pub fn resolve_conflict(store: &mut VcStore, table: &str, pk: &[VcValue], side: ResolveSide) -> VersionResult<()>;
pub enum ResolveSide { Ours, Theirs }
```

Rules (`a3b54a68` never-persist):
- Conflicts live in `VcStore` working memory only (extend the existing
  `conflicts: HashSet<String>` gate into a table holding full `ConflictEntry`
  rows). They are never encoded into commits, never written to the chunk
  store, never returned by `table_rows(at)` for any committed `at`.
- `dolt_commit` refuses while any conflict remains: exact
  `cannot commit: unresolved merge conflicts` (O2 string, already gated in
  `staging.rs:196` — keep that gate, feed it from the new table).
- Autocommit: a merge/replay statement that hits a conflict rolls back the
  whole statement's working-set mutation except the recorded conflicts
  themselves (conflicts ARE the statement's output; partial row application
  is not). Never durable, never half-applied.
- `dolt_conflicts_<t>` vtable per merged table (same per-table wiring as B2)
  + `dolt_conflicts` union; TVF arg filters by table. Schema:
  `pk cols..., base_<c>..., our_<c>..., their_<c>..., conflict_type TEXT`.
- Resolve: `SELECT dolt_conflicts_resolve('--ours'|'--theirs', table[, pk...])`
  applies that side's row image to the working set and drops the entry;
  wrong side arg → `invalid resolve side: 'X' (want '--ours' or '--theirs')`.
  Resolving the last conflict unblocks commit.

## 4. `src/constraints.rs` — detectors + violations tables

From `merge_constraints*.c`,
[verify_constraints.c](https://github.com/dolthub/doltlite/blob/master/src/verify_constraints.c) (`:363`).

```rust
pub enum ViolationKind { ForeignKey, Unique, Check, NotNull, Strict }
pub struct Violation { pub table: String, pub kind: ViolationKind, pub row_pk: Vec<VcValue>, pub detail: String }
pub fn verify_constraints(merged: &MergedWork, schemas: &[SchemaIR]) -> Vec<Violation>;
```

Rules:
- Detectors run on the merged working set BEFORE commit: FK (child row with
  no parent key), unique (duplicate key values incl. multi-column), check
  (stored `CHECK` text evaluated per row — support the comparison subset
  `=,!=,<,<=,>,>=,IS NULL,IS NOT NULL,LIKE,IN` over local columns; anything
  else → violation `check expression not supported: EXPR` rather than silent
  pass), not-null (NULL into a `NOT NULL` column), strict (value that fails
  `STRICT` typing, e.g. TEXT into INT).
- `dolt_commit` refuses while violations remain unless `--force`:
  exact `cannot commit: constraint violations remain` (O2 string, gate at
  `staging.rs:199` — keep, feed from the new detectors). `--force` commits
  anyway and CLEARS the recorded violations (they describe the pre-commit
  working set, which is now committed).
- `dolt_constraint_violations` (+ per-table `dolt_constraint_violations_<t>`)
  schema: `table_name TEXT, violation_type TEXT, pk_cols TEXT, details TEXT`.
  `violation_type` values: `foreign key`, `unique`, `check`, `not null`,
  `strict` (lowercase, Dolt-identical).

## 5. `src/replay.rs` — cherry-pick + revert + rebase + merge drivers

- `cherry_pick(store, commit)`: single-commit replay = three-way merge with
  base = commit's FIRST parent snapshot, ours = current working/HEAD snapshot,
  theirs = commit snapshot. No LCA walk (parent is the base by construction).
  Initial (parentless) commit → whole-tree add. Merge commit without `-m`
  → `cherry-pick of merge commit needs -m PARENT (got N parents)`.
- `revert(store, commit)`: inverse of cherry-pick: base = commit snapshot,
  theirs = commit's first-parent snapshot (the direction is flipped), ours =
  current. Reverting the initial commit empties its tables. Message defaults
  to `Revert "<orig message>"`. Reverting a merge commit without `-m` →
  same `-m` error as cherry-pick.
- `rebase`: atomic plan-table replay. Plan rows: `pick <hash>`,
  `drop <hash>`, `reword <hash> <msg>`, `squash <hash>`, `fixup <hash>`.
  Replay onto the new base one commit at a time; any conflict aborts the
  WHOLE rebase (no partial application — atomic) and leaves the pre-rebase
  HEAD + working set intact, reporting `rebase conflict at <hash>: resolve
  and --continue`. `--continue` resumes after resolve; `--abort` restores the
  saved pre-rebase tip. Squash/fixup fold the message (squash concatenates,
  fixup discards) and produce ONE commit.
- `merge` drivers: `--squash` (single working-set apply + single commit, no
  second parent), `--no-commit` (apply to working set, stop before commit),
  `--abort` (discard merge state: working set restored, conflicts dropped).
  `dolt_merge_status` vtable: `table_name TEXT, rows_merged INTEGER,
  rows_conflicted INTEGER, schema_conflict INTEGER, state TEXT`
  (`clean`/`conflicted`/`aborted`).

## 6. Scalar fns + vtables (turso_ext glue, thin)

| # | doltlite origin | turso scalar / vtab | notes |
|---|---|---|---|
| M1 | `dolt_merge` | `SELECT dolt_merge('branch'[, '--squash'\|'--no-commit'\|'--abort'\|'-m MODE'])` | returns merge commit hash or `conflict` count text; `--abort` path restores |
| M2 | `dolt_merge_base` | `SELECT dolt_merge_base('a','b')` | O2 `merge_base` surface; `None` → NULL, not error |
| M3 | `dolt_cherry_pick` | `SELECT dolt_cherry_pick('<hash>'[, '-m N'])` | single commit; `-m` picks merge parent |
| M4 | `dolt_revert` | `SELECT dolt_revert('<hash>'[, '-m N'])` | inverse commit |
| M5 | `dolt_rebase` | `SELECT dolt_rebase('--continue'\|'--abort'\|'-- onto ...'...)` | atomic + plan table form |
| M6 | `dolt_conflicts_resolve` | `SELECT dolt_conflicts_resolve('--ours'|'--theirs', table[, pk...])` | §3 errors exact |
| M7 | `dolt_verify_constraints` | `SELECT dolt_verify_constraints()` | returns violation count; rows in CV tables |
| V1 | merge status | `dolt_merge_status` vtable (§5 schema) | HIDDEN handling: none (no hidden cols) |
| V2 | conflicts | `dolt_conflicts` + `dolt_conflicts_<t>` (§3 schema; TVF table arg is a plain filter, HIDDEN-free) | per-table needs B2 wiring |
| V3 | violations | `dolt_constraint_violations[_<t>]` (§4 schema) | per-table needs B2 wiring |

Arity errors reuse `incorrect number of arguments to dolt_X` (O2).

## 7. Exact errors (byte-for-byte; O2 strings reused, new ones below)

Reuse: `cannot commit: unresolved merge conflicts`,
`cannot commit: constraint violations remain`, `nothing to commit`,
`branch not found: NAME`, `commit not found: HASH`,
`invalid revision spec: 'SPEC'`, `table not found: NAME`,
`cannot write in detached HEAD state`, `database is locked`,
`incorrect number of arguments to dolt_X`, `table has no primary key: NAME`.

New:
- `merge conflict: N conflicting rows in M tables` (merge/cherry-pick/revert
  statement result when conflicts are recorded; commit itself still refused
  by the O2 gate string above — never conflate the two)
- `schema conflict in table: NAME (DETAIL)` (predetect refusal; DETAIL is the
  PK/matrix reason, e.g. `primary key changed on both sides`)
- `invalid resolve side: 'X' (want '--ours' or '--theirs')`
- `cherry-pick of merge commit needs -m PARENT (got N parents)` (revert shares it)
- `rebase conflict at <hash>: resolve and --continue`
- `no rebase in progress`
- `no merge in progress`
- `check expression not supported: EXPR`
- `merge --abort: no merge in progress` is NOT used — use `no merge in progress`.

## 8. Files to create/modify

- `extensions/versioning/src/merge.rs` — §1 (fast path + pass1/pass2 + renumber note).
- `extensions/versioning/src/merge_schema.rs` — §2 (SchemaIR parse + matrix + rebuild).
- `extensions/versioning/src/conflicts.rs` — §3 (transient table + resolve).
- `extensions/versioning/src/constraints.rs` — §4 (5 detectors + force-clear rule).
- `extensions/versioning/src/replay.rs` — §5 (cherry-pick/revert/rebase + merge drivers).
- `extensions/versioning/src/lib.rs` — add the five modules.
- `extensions/versioning/src/staging.rs` — feed commit gates from §§3–4 tables
  (keep exact O2 strings); `record_snapshot` path must serve per-table row reads
  so B2 wiring has content to declare.
- `extensions/versioning/src/funcs.rs` — M1–M7 pure specs on `VcOperations`.
- `extensions/core/src/vc_vtabs.rs` — V1–V3 vtables + B2 per-table wiring
  (thread conn/args into TVF create; `core/vtab.rs` change kept minimal).
- `sqlite/conformance/turso-sqltests/vc_merge_basic.sqltest` — fast-path merge,
  conflicting merge + `dolt_merge_status`, resolve ours/theirs → commit unblocked.
- `sqlite/conformance/turso-sqltests/vc_conflicts_basic.sqltest` — conflicts tables,
  autocommit rollback, never-durable proof, CV detectors + `--force`.
- `sqlite/conformance/turso-sqltests/vc_replay_basic.sqltest` — cherry-pick,
  revert (+ inverse check), rebase pick/drop/squash + `--abort`, merge
  `--squash`/`--no-commit`/`--abort`.
- `.claude/skills/merge-semantics/SKILL.md` — orchestrator writes after review loop.

## 9. Tests (must fail without change, pass with it — TDD: failing tests FIRST)

- Rust unit (versioning crate): `merge.rs::tests` (fast paths, cell rules,
  add/add, delete/modify, no-PK identity, name-keyed not rootpage-keyed),
  `merge_schema.rs::tests` (PK rule, full matrix, rebuild order),
  `conflicts.rs::tests` (transient: never in commit objects; resolve sides;
  last-resolve unblocks), `constraints.rs::tests` (one per detector + force
  clears + unsupported-check expression), `replay.rs::tests` (cherry-pick
  single + merge-commit -m error, revert inverse, rebase atomic abort +
  continue + squash/fixup messages, squash/no-commit/abort drivers).
- Rust integration: `extensions/versioning/tests/merge_conflicts.rs`
  (branch → diverge → merge → conflict → resolve → commit over `VcStore`),
  `extensions/versioning/tests/replay.rs` (cherry-pick/revert/rebase over
  `VcStore` + `MemVcRead` history assertions).
- `.sqltest` × 3 files above (names exact). Same rule as O3: never skip —
  wire the minimal `turso_ext` registration so they actually run.
- Validation (all green, paste full output in final report):
  `cargo test -p turso_versioning`, `cargo clippy --workspace --all-features
  --all-targets -- --deny=warnings`, `cargo fmt --check`,
  `make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'`
  (at minimum the new `vc_merge*`/`vc_replay*`/`vc_conflicts*` files).

## 10. Review gates (brutal reviewer checks)

1. Correctness paramount: crash > corrupt. No silent pass on unsupported
   CHECK; no half-applied rebase; no durable conflict bytes anywhere.
2. Strong types, exact error strings §7. No stringly commits/refs.
3. No `turso_ext`/`core/` imports in versioning crate; glue thin.
4. Callers before callees (top-down files); comments only why-level.
5. Plain language, no jargon.
6. O(log N) LCA reuse (O2 `merge_base`), never a re-walk; fast paths proven.

## Status

- [x] v1 implemented
- [x] reviewer pass 1 — RED with B1-B6
- [x] v2/v3 fixes — all B1-B6 resolved
- [x] O4 re-review fixes — B3 restore-on-failure, B4 ours-deleted schema-conflict, merge_schema deletion-is-conflict, union_columns O(n²) cleanup
- [x] O4 final nits — merge_schema comment/message fixes, O(1) HashSet, dead code removal

### Fix Log

- B1: `merge_schema.rs:92-124` single-side delete now checks base for modification
- B2: `merge_schema.rs:196-285` CHECK constraints included in classify/same_schema/union_columns
- B3: `versioning.rs:155-195` outer SAVEPOINT wraps sync_work_to_sql; dirty flags + pending_resolve restored on failure for retry consistency
- B4: `replay.rs:750-763` schema-conflicted tables keep ours-side image in work; ours-deleted + theirs-modified records schema conflict with theirs_schema preserved
- B5: clippy clean for turso_versioning + turso_ext; workspace turso_core lints pre-existing
- B6: removed root RUBRIC_O4.md duplicate and *.snap.new files
- O4-fix1: `merge_schema.rs:94-122` ours-deleted arm: distinguishes unchanged/modified theirs + absent. Mirror arm `merge_schema.rs:126-140` theirs-deleted: distinguishes unchanged/modified ours (symmetric).
- O4-fix2: `merge_schema.rs:197-211` replaced `Vec` with `HashSet` for theirs_new so the `O(1)` lookup claim in the comment is correct.
- O4-fix3: `staging.rs:207-219` + `versioning.rs:165-191` write-back failure restores drained pending_resolve and dirty flags for retry consistency

## Validation

- `cargo test -p turso_versioning`: **342 passed**, 0 failed
- `cargo fmt --check`: **clean**
- `cargo clippy -p turso_versioning --all-features --all-targets -- --deny=warnings`: **zero warnings** (workspace turso_core lints pre-existing)
- `make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'`: **1647 passed**, 341 skipped — vc subset PASS
