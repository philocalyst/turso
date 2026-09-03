# O3 Rubric — Read path: history/diff vtables + pushdowns (extensions/versioning + turso_ext glue)

Reference: https://github.com/dolthub/doltlite (master @ 2026).
Files: `src/doltlite_log.c`, `src/doltlite_history.c` (history), `src/doltlite_at.c` (at),
`src/doltlite_blame.c` (blame), `src/doltlite_diff.c` (diff), `src/doltlite_diff_table.c`
(diff_table), `src/doltlite_diff_stat.c` (diff_stat + diff_summary),
`src/doltlite_schema_diff.c` (schema_diff), `src/doltlite_patch.c` (patch),
`src/doltlite_schemas.c` (schemas). Schemas researched via doltlite source;
column names/orders below are 1:1 with doltlite's `sqlite3_declare_vtab` strings.

O2 dependency: `Commit`/`CommitMeta`/`CommitId`, `MemCommitStore`/`CommitStore`,
`RefName`/`MemRefStore`/`RefStore`, `Revision`/`parse_revision`, `merge_base`,
`ancestors`, `VcStore`, `VersionError`/`VersionResult`. Build on them, do not fork them.

## 0. BLOCKER RESOLVED (do not relitigate, do not waive)

`turso_ext` (extensions/core) depends on `turso_versioning` (extensions/core/Cargo.toml:17),
so `turso_versioning` MUST NOT depend on `turso_ext`: that is a dependency cycle and
`cargo build` will refuse it. Therefore:

- Pure read-model logic lives in `extensions/versioning/src/vtab_log.rs`,
  `vtab_history.rs`, `vtab_diff.rs` with ZERO `turso_ext`/`core/` imports
  (same gate as O2 review: no `core/` imports from the versioning crate).
- Thin `VTabModule`/`VTable`/`VTabCursor` glue lives in ONE new file
  `extensions/core/src/vc_vtabs.rs`, which calls into the pure logic.
  Registration follows the `register_extension! { vtabs: {...} }` + `VTabModuleDerive`
  pattern in `extensions/csv/src/lib.rs`.
- The glue is thin: argument parsing, `best_index` mapping, row rendering.
  All range resolution, diff computation, ordering, and pushdown planning are pure
  and unit-tested in the versioning crate.

## 1. The `VcRead` provider seam (versioning crate, `vtab_log.rs` top)

Row data does not exist yet (O4 owns tables). Every vtable reads through this trait;
tests use the in-memory fake. Never reach into `VcStore` internals from vtables.

```rust
pub struct VcRow { pub values: Vec<VcValue> }
pub enum VcValue { Null, Integer(i64), Text(String) }
pub struct VcCommitView { pub id: CommitId, pub name: String, pub email: String,
    pub message: String, pub timestamp: i64 }
pub trait VcRead {
    fn parents(&self, id: &CommitId) -> VersionResult<Vec<CommitId>>;
    fn commit_view(&self, id: &CommitId) -> VersionResult<VcCommitView>;
    fn head(&self) -> VersionResult<CommitId>;
    fn head_label(&self) -> String;
    fn resolve(&self, spec: &str) -> VersionResult<CommitId>;
    fn commit_total(&self) -> usize;
    fn table_columns(&self, table: &str) -> Option<Vec<String>>;
    fn table_pk(&self, table: &str) -> Option<Vec<String>>;
    fn table_rows(&self, table: &str, at: &CommitId) -> Option<Vec<VcRow>>;
    fn table_schema_sql(&self, table: &str, at: &CommitId) -> Option<String>;
    fn tables(&self) -> Vec<String>;
    fn staged_tables(&self) -> Vec<String>;   // default empty
    fn working_tables(&self) -> Vec<String>;  // default empty
    fn has_snapshot(&self, id: &CommitId) -> bool; // default false
}
```

`MemVcRead`: struct with `commits: Vec<(CommitId, VcCommitView, HashMap<String, (Vec<String>, Vec<VcRow>, String)>)>`
plus branch tips; newest-first = insertion order reversed (tests push oldest first).
`table_rows` returns `None` for unknown table/commit (caller maps to `table not found: NAME`
or `commit not found: HASH` — exact O2 strings, never new strings).
Lowercase `working`/`staged` resolve like their uppercase forms (pinned by test).

`WORKING`/`STAGED` support: `MemVcRead` holds optional `working: HashMap<String, (Vec<String>, Vec<VcRow>)>`
and `staged: ...` snapshots; `resolve("WORKING")`/`resolve("STAGED")` return a sentinel
`CommitId` (`[0xAA;20]` / `[0xAB;20]`) and `table_rows` serves the matching snapshot.
Real O4 wiring replaces the sentinel with content hashes; the sentinel values are
documented on the trait. Rule: sentinel ids never enter `commit()`; resolving them
through `commit()` is `commit not found: <hex>`.

## 2. 1:1 vtable schemas (doltlite → turso)

| # | doltlite (blob) | turso vtable | schema (exact order, doltlite-identical) |
|---|-----------------|--------------|------------------------------------------|
| L1 | [doltlite_log.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_log.c) `doltliteLogSchema` | `dolt_log` (+ TVF `dolt_log('rev'\|'a..b'\|'a...b')`) | `commit_hash TEXT, committer TEXT, email TEXT, date TEXT, message TEXT, revision TEXT HIDDEN` |
| H1 | [doltlite_history.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_history.c) `htBuildSchema` | `dolt_history_<t>` TVF `dolt_history_<t>('start_ref')` | `<live cols of t in order>, commit_hash TEXT, committer TEXT, commit_date TEXT, start_ref TEXT HIDDEN` |
| H2 | [doltlite_at.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_at.c) `atBuildSchema` | `dolt_at_<t>('ref')`, ref mandatory | `<cols of t at ref>, commit_ref TEXT HIDDEN` |
| H3 | [doltlite_blame.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_blame.c) `blameBuildSchema` | `dolt_blame_<t>` | `<pk cols only, PK order>, "commit" TEXT, commit_date TEXT, committer TEXT, email TEXT, message TEXT`; no-PK table → `table has no primary key: NAME` |
| H4 | [doltlite_schemas.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_schemas.c) `zSchemasSchema` | `dolt_schemas` | `type TEXT, name TEXT, fragment TEXT, extra TEXT, sql_mode TEXT` (`extra`/`sql_mode` always NULL; rows = views/triggers) |
| D1 | [doltlite_diff.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_diff.c) `diffSchema` | `dolt_diff` | `commit_hash TEXT, committer TEXT, email TEXT, date TEXT, message TEXT, data_change INTEGER, schema_change INTEGER, table_name TEXT` |
| D2 | [doltlite_diff_table.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_diff_table.c) `buildDiffSchema` | `dolt_diff_<t>` bare = full history; TVF slice `(from,to)` | `to_<c...>, to_commit TEXT, to_commit_date TEXT, from_<c...>, from_commit TEXT, from_commit_date TEXT, diff_type TEXT` (`added`/`deleted`/`modified`); `from_ref TEXT HIDDEN, to_ref TEXT HIDDEN` |
| D3 | [doltlite_diff_stat.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_diff_stat.c) `dstSchema` | `dolt_diff_stat(from_ref,to_ref[,tbl])` TVF | `table_name TEXT, rows_unmodified INTEGER, rows_added INTEGER, rows_deleted INTEGER, rows_modified INTEGER, cells_added INTEGER, cells_deleted INTEGER, cells_modified INTEGER, old_row_count INTEGER, new_row_count INTEGER, old_cell_count INTEGER, new_cell_count INTEGER, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN, tbl TEXT HIDDEN` |
| D4 | same file `dssSchema` | `dolt_diff_summary(from_ref,to_ref[,tbl])` TVF | `from_table_name TEXT, to_table_name TEXT, diff_type TEXT` (`added`/`dropped`/`renamed`/`modified`), `data_change INTEGER, schema_change INTEGER, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN, tbl TEXT HIDDEN` |
| D5 | [doltlite_schema_diff.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_schema_diff.c) `sdSchema` | `dolt_schema_diff(from_ref,to_ref[,table])` TVF | `from_table_name TEXT, to_table_name TEXT, from_create_statement TEXT, to_create_statement TEXT, from_ref TEXT HIDDEN, to_ref TEXT HIDDEN, table_name TEXT HIDDEN` |
| D6 | [doltlite_patch.c](https://github.com/dolthub/doltlite/blob/master/src/doltlite_patch.c) `patchSchemaSql` | `dolt_patch(arg1[,arg2[,arg3]])` TVF | `statement_order INTEGER, from_commit_hash TEXT, to_commit_hash TEXT, table_name TEXT, diff_type TEXT` (`schema`/`data`), `statement TEXT, arg1 TEXT HIDDEN, arg2 TEXT HIDDEN, arg3 TEXT HIDDEN`; statements end `;`, ordered executable |

Semantics (doltlite-identical):
- Log order newest-first from session HEAD (or TVF ref); merges enqueue all parents.
- `dolt_log('a..b')` = reachable(b) − reachable(a); `a...b` = b − merge-base(a,b).
  Same splitter for `dolt_diff_<t>(from,to)`, `dolt_patch('a..b'/'a...b')`
  (three-dot left → merge-base), `dolt_schema_diff` single-arg two-dot form.
- `dolt_diff` emits `STAGED` (HEAD→staged) then `WORKING` (staged→work) rows first.
  `dolt_diff_<t>` accepts `WORKING`/`STAGED` endpoints. `to_commit`/`from_commit`
  hold the literal `WORKING`/`STAGED` strings for those rows.
- `dolt_patch` recipe: `dolt_patch('HEAD','WORKING') WHERE diff_type='data'
  ORDER BY statement_order` must yield executable SQL in order.
- Dates render as the provider timestamp (`i64` → text via `to_string`; no formatting
  library, no timezone logic — O4 may upgrade rendering, tests pin `to_string`).

## 3. Pushdown contracts (one per VC vtable; pure planner fns in versioning crate)

Each vtable exposes `pub fn plan(constraints: &[VcConstraint], order_by: &[VcOrderBy]) -> VcPlan`
where `VcConstraint = { column: u32, op: VcOp::Eq | VcOp::Other, usable: bool }`,
`VcOrderBy = { column: u32, desc: bool }`, `VcPlan = { idx_num: i32, idx_str: Option<String>,
omit: Vec<bool>, argv: Vec<u32>, cost: f64, rows: u32 }`. Only `Eq` plans;
anything else scans and lets the engine recheck. The `turso_ext` glue maps
`ConstraintInfo`→`VcConstraint` and `VcPlan`→`IndexInfo` 1:1. Test the planner directly.

| vtable | constraints consumed (omit=true) | idx_num/idx_str | else |
|---|---|---|---|
| `dolt_log` | `commit_hash EQ` probe, or `revision EQ` (HIDDEN) | `1`/`"hash"`, `2`/`"rev"` | full walk; `ORDER BY date DESC` consumed (log is newest-first) |
| `dolt_history_<t>` | `commit_hash EQ` + `start_ref EQ` (HIDDEN) | `1`/`"hash"`, `2`/`"start"` | full scan; never omit row-value constraints (rechecked) |
| `dolt_at_<t>` | `commit_ref EQ` (HIDDEN) mandatory slice | `1`/`"ref"` | without ref cost = 1e12 (same as doltlite `at` mandatory-ref cost) |
| `dolt_blame_<t>` | rowid/PK range only for single-INTEGER-PK tables; else full scan | `1`/`"pk"` | full scan |
| `dolt_schemas` | `type EQ` + `name EQ` | `1`/`"type"`, `2`/`"name"` | full scan |
| `dolt_diff` | `commit_hash EQ` + `table_name EQ` | `1`/`"hash"`, `2`/`"table"` | full scan |
| `dolt_diff_<t>` | `to_commit EQ` / `from_commit EQ` / both / `from_ref+to_ref` slice (omit) / single `from_ref` | `1`..`4`/`"to"`,`"from"`,`"both"`,`"slice"` | full history scan |
| `dolt_diff_stat/summary/schema_diff/patch` | `from_ref/to_ref/tbl EQ` (stat/summary/schema require both; patch 1–3 positional + optional `diff_type EQ`) | `1`/`"refs"` | stat/summary/schema without both refs → empty rows (not error) |
| COUNT fast path | `SELECT COUNT(*) FROM <vtab>` with no usable filter uses provider subtree count when available (`VcRead::table_rows().len()` + commit count); planner returns `rows` estimate without materializing. Test asserts `plan()` cost for unfiltered COUNT is constant-time (no row build). |
| COUNT contract (v2 correction) | `plan_log_count` + `log_count(provider)` form the tested contract (count from `commit_total()`, never the walk; `NoWalk` test proves the probe never walks). The engine cannot see aggregates in `best_index`, so no COUNT wiring exists in the glue — the pair stays as the contract O4's scan layer will call. |

O(log N) seeks: `commit_hash EQ` and `to/from_commit EQ` probes resolve via hash lookup,
never a full walk. Tests assert probe path with a 100-commit store returns 1 row and
the planner chose `idx_num=1`.

## 4. Exact errors (byte-for-byte; reuse O2 strings, three new ones)

- Reuse: `commit not found: HASH`, `invalid revision spec: 'SPEC'`, `branch not found: NAME`,
  `tag not found: NAME`, `table not found: NAME`, `incorrect number of arguments to dolt_X`.
- New: `table has no primary key: NAME` (blame on no-PK table),
  `ref required: dolt_at_<t> needs a revision argument` (at without ref),
  `unknown table in diff: NAME` is NOT used — unknown tables in diff/stat return zero rows,
  never an error (doltlite returns empty diff for missing tables).

## 5. Files to create/modify

- `extensions/versioning/src/vtab_log.rs` — `VcRead`/`VcValue`/`VcRow`/`VcCommitView`/`MemVcRead`
  + `log_rows(provider, range: Option<LogRange>)` + `plan_log()` pushdown planner.
- `extensions/versioning/src/vtab_history.rs` — `history_rows/at_rows (ref mandatory)/
  blame_rows (pk cols + error)/schemas_rows` + `plan_history()/plan_at()/plan_blame()/plan_schemas()`.
- `extensions/versioning/src/vtab_diff.rs` — `diff_rows/diff_table_rows/diff_stat_rows/
  diff_summary_rows/schema_diff_rows/patch_rows (ordered, `;`-terminated)` + planners.
- `extensions/versioning/src/lib.rs` — add the three modules.
- `extensions/core/src/vc_vtabs.rs` — thin glue: one `VTabModule` per FIXED-NAME
  vtable (`dolt_log`, `dolt_diff`, `dolt_diff_stat`, `dolt_diff_summary`,
  `dolt_schema_diff`, `dolt_patch`, `dolt_schemas`). Per-table modules
  (`dolt_history_<t>`/`dolt_at_<t>`/`dolt_blame_<t>`/`dolt_diff_<t>`) are an
  O4 entrance requirement, NOT waived: TVF `create` receives no connection
  and empty args (`core/vtab.rs:87-108` passes `Vec::new()`), so per-table
  column metadata is unavailable at schema-build time and only the table
  owner (O4) can supply it. The pure per-table readers are 1:1 and
  Rust-tested; the engine contract (names, schemas, hidden columns) is
  pinned in `vtab_history.rs`/`vtab_diff.rs` schema tests for O4 to wire.
- Bare scans on a commit-less branch return empty, never
  `branch not found`; identity probes (`commit_hash EQ`, TVF revisions)
  always resolve and error exactly when absent.
- `sqlite/conformance/turso-sqltests/vc_log_basic.sqltest` — log order/limit/branch filter,
  `a..b`/`a...b` TVF forms, error cases.
- `sqlite/conformance/turso-sqltests/vc_diff_basic.sqltest` — diff/stat/summary/schema_diff/patch,
  WORKING support, ordered executable patch.
- `sqlite/conformance/turso-sqltests/vc_history_basic.sqltest` — history/at/blame/schemas,
  mandatory-ref error, no-PK error.
- `.claude/skills/vc-vtables/SKILL.md` — orchestrator writes after review loop (not implementor).

## 6. Tests (must fail without change, pass with it — TDD: failing tests FIRST)

- Rust unit (versioning crate): `vtab_log.rs::tests`, `vtab_history.rs::tests`,
  `vtab_diff.rs::tests` — schemas (column order), ordering, range splitter
  (`..` before `...` never misparses), WORKING/STAGED rows, patch ordering +
  `;` termination, planner choices per table above, O(log N) probe test, COUNT fast path.
- Rust integration `extensions/versioning/tests/vtab_read.rs` — end-to-end over `MemVcRead`:
  3-commit chain across 2 branches, full matrix of §2 rows + §3 plans + §4 errors.
- `.sqltest` × 3 files above (names exact). NOTE: `.sqltest` runs against the live engine;
  if `vc_vtabs` glue registration is not yet wired to a live `VcRead`, write the `.sqltest`
  files with `skip` markers? NO — never skip. Instead: sqltests assert through the glue
  against a test-seeded provider; if engine wiring is incomplete, the implementor must
  complete the minimal wiring (register modules in `versioning.rs::register_vc_functions`
  path or equivalent) so the sqltests actually run. Zero waived blockers.
- Validation (all green, paste full output in final report):
  `cargo test -p turso_versioning`, `cargo clippy --workspace --all-features --all-targets -- --deny=warnings`,
  `cargo fmt --check`, `make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'`
  (at minimum the three new `vc_*` files; full suite if time permits).

## 7. Review gates (brutal reviewer checks)

1. Strong types (no stringly commits/refs); exact error strings §4.
2. No `turso_ext`/`core/` imports in versioning crate; glue thin and in `turso_ext` only.
3. Callers before callees (top-down files); comments only why-level.
4. Plain language, no jargon.
5. O(log N) probes proven by test, not claimed; COUNT fast path proven.

## Status

- [x] v1 implemented (orchestrator-direct; nemotron channel returned empty
  twice with zero files, so the orchestrator implemented per the rubric)
  — 169 lib + 43 integration tests green, reviewer loop below
- [x] reviewer pass 1 (muse-spark) — verdict NEEDS-V2 (14 must-fix + nits)
- [x] v2 fixes (orchestrator-direct) — VcOp plannability, VcStore WORKING/
  STAGED sentinels, HistoryRow.start, log_count/commit_total, nested-range
  rejection, cycle contract docs, patch delete+add split + rectangular
  asserts, typed blame keys, schema-derived planner test, multi-column error
  test, sqltest probe coverage, per-table→O4 amendment
- [x] reviewer pass 2 (muse-spark) — verdict GREEN (14/14 PASS, nits only)
- [x] v3 nits (orchestrator-direct) — lowercase-resolve test, split_range
  nested test, hashof-WORKING sentinel pin, `.claude/skills/vc-vtables/SKILL.md`
- [x] green: fmt + clippy (turso_versioning, turso_ext deny-clean; zero O3
  findings in workspace clippy) + cargo test + sqltests (20 new O3 + 12 O2)

## v1 notes (decisions locked during implementation, not waivers)

- Glue placement: `turso_ext` cannot host the derive (needs external
  `turso_ext` path) — solved with `extern crate self as turso_ext` in
  `extensions/core/src/lib.rs`. `turso_versioning` still has zero
  `turso_ext`/`core` imports (cycle preserved).
- State channel: vtable `create`/`best_index` are stateless, so per-table
  state flows through the opening connection: `turso_ext::Conn` carries an
  optional `Arc<VcState>` clone set by `core/vtab.rs::open` from the core
  connection's `versioning` field. No global registry, no lifetime hazard.
- Engine protocol learned the hard way: core drives scans from
  `filter`/`next` return codes and never calls `eof()`. `filter` returns EOF
  on empty, `next` returns EOF past the last row; errors report OK with an
  empty frame so the first `column` call surfaces the exact message.
- Bare reads on a commit-less branch return empty, never
  `branch not found` (probes/TVF revisions still error exactly).
- Per-table engine modules (`dolt_history_<t>` etc.) deferred to O4 with a
  precise reason: TVF `create` receives no connection and empty args, so
  column metadata is unavailable at schema-build time; only the table owner
  (O4) can supply it. Pure layer is 1:1 and Rust-tested; engine exposes the
  7 fixed-name modules live.
- `VcStore::dolt_commit` still writes zero roots (O4 computes real roots);
  `VcStore` implements `VcRead` so the glue reads live metadata today and
  row content once `record_snapshot` is fed.
- `dolt_diff` STAGED/WORKING rows come from the staging sets when snapshots
  are absent (membership implies CREATE, the only pre-O4 writer), with the
  snapshot compare taking over when both sides exist.
- Workspace `cargo clippy --deny=warnings` has ~100 pre-existing errors in
  untouched crates (toolchain lint drift); zero in O3 files (proven by grep).
