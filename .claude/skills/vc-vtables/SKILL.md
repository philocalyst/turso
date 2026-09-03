# VC vtables (O3 read path)

History and diff reads over versioned data. Pure logic in
`extensions/versioning/src/vtab_{log,history,diff}.rs`; thin engine glue in
`extensions/core/src/vc_vtabs.rs`. Per-table modules (`dolt_history_<t>`,
`dolt_at_<t>`, `dolt_blame_<t>`, `dolt_diff_<t>`) are O4 work (needs
column-aware `create`; see below). Fixed-name modules are live.

## Live engine vtables

`dolt_log` (bare + `dolt_log('rev'|'a..b'|'a...b')`), `dolt_diff` (bare),
`dolt_diff_stat(from,to[,tbl])`, `dolt_diff_summary(from,to[,tbl])`,
`dolt_schema_diff(from,to[,table])`, `dolt_patch(arg1[,arg2[,arg3]])`,
`dolt_schemas`. All `TableValuedFunction`; TVF args arrive as hidden-column
EQ constraints in hidden order.

## Rules for this area

- **Pure first.** Range resolution, diffing, ordering, planners live in
  `turso_versioning` with zero `turso_ext`/`core` imports (would be a
  dependency cycle: `turso_ext` depends on `turso_versioning`). Glue only
  maps constraints, renders `Value`s, surfaces errors.
- **State flows through the opening connection.** `core/vtab.rs::open`
  clones the core connection's `VcState` into the ext `Conn`; cursors read
  it via `VcState::with_store` (short, pure closures — never SQL inside).
  No global registry.
- **Engine scan protocol.** Core drives scans from `filter`/`next` return
  codes and never calls `eof()`: `filter` returns EOF on empty, `next`
  returns EOF past the last row. Errors report OK with an empty frame so
  the first `column` call surfaces the exact message (`column` must return
  `Err` on every index while undelivered; `next` returns `Error` after).
- **Empty vs error contract.** Bare scans on a commit-less branch return
  empty; identity probes (`commit_hash EQ`, TVF revisions) always resolve
  and error exactly (`branch/commit not found`, `invalid revision spec`).
- **`Value` is not `Clone`.** Cursors store an owned `Cell` enum and render
  on demand in `column`.
- **Only `Eq` plans.** `VcConstraint.op` (`Eq`/`Other`); anything else scans
  and lets the engine recheck. `omit=true` means the engine skips its
  recheck, so the glue must enforce the equality itself.
- **Never present unknown content as known.** Missing snapshots read as
  empty diffs, never fake deletes. `STAGED`/`WORKING` rows for staging-set
  members without snapshots read as data changes (membership implies
  CREATE, the only pre-O4 writer). Snapshots are rectangular (asserted in
  debug); ragged input splits to delete+add, never misaligned UPDATEs.
- **Nested ranges fail loudly** (`a..b..c` → `invalid revision spec`).
  `a...b` starts at the merge base. Walks terminate on revisit; only
  `generation`/`merge_base` report cycles as errors.

## O4 handoff

1. Fill `VcStore::record_snapshot` at commit (columns, pk, rows,
   schema SQL) — row content flows into history/diff/stat/patch untouched.
2. Add per-table engine modules. Blocker: TVF `create` gets no connection
   and empty args (`core/vtab.rs`), so column metadata is unavailable at
   schema-build time. Pure readers + schema strings are pinned and tested;
   wire them when a column-aware path exists.
3. `log_count` + `plan_log_count` are the tested COUNT contract; the engine
   has no aggregate visibility in `best_index` yet.

## Validate

`cargo test -p turso_versioning`, `cargo test -p turso_ext`,
`cargo clippy -p turso_versioning -p turso_ext --all-targets -- --deny=warnings`,
`cargo fmt --check`, sqltests `vc_log_basic` + `vc_diff_basic` +
`vc_history_basic` via `sqltest run --backend rust --snapshot-filter __never__`.
