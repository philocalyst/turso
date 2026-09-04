# O6 Rubric — Oracle + Perf: differential harness, corpus gaps, benchmarks, perf-vs-doltlite

## 0. BLOCKERS — RESOLVE, NEVER WAIVE

B1. O5 owns `extensions/versioning/src/gc.rs`, `extensions/versioning/src/remote*`
(if present), `remotesrv`, compat TSVs, and `RUBRIC_O5.md`. O6 MUST NOT modify
them. O6's GC exposure is benchmark-read-only: it may call the existing public
`gc::{GcSnapshot, mark, sweep}` API but never edit `gc.rs`, and MUST NOT add a
`dolt_gc` SQL surface (that is O5's). No differential scenario or bench may
invoke remotes logic.

B2. TDD: every behavioral or harness change is demonstrated RED first (run the
new test, record the failing output), then landed GREEN. Commits land green
(repo convention: bodies carry `Tests:` evidence; RED evidence goes in the body
when the change fixes behavior). Pure coverage additions to already-implemented
surfaces are test-only commits that must pass.

B3. Zero waived blockers. Gates in §7 are mandatory per commit. If a gate is
red, fix it before committing.

B4. Borrow-first: extend existing files/harnesses before creating new ones.
New files are justified only where no existing harness can express the
coverage (documented per file below).

## 1. Ground Truth (verified 2026-09-04)

- Baseline: `cargo test -p turso_versioning` → 342 passed (O4 GREEN).
- Existing corpus: 9 `vc_*.sqltest` files in
  `sqlite/conformance/turso-sqltests/` (branch, commit_gate, conflicts, diff,
  history, log, merge, replay, vtabs).
- Doltlite is a **C fork of SQLite** (not Go): out-of-tree build
  `mkdir build && cd build && ../configure && make doltlite`, prolly engine on
  by default. Research clone: `/private/tmp/doltlite-research` @ `5c67114`
  (merge PR #2596). Oracle tests: `test/vc_oracle_*_test.sh` (70 scripts).
  Bucket manifests: `test/oracle-buckets/*.txt`. Perf baseline:
  `performance-report.md` (DoltLite vs SQLite, nightly PASS).
- `divan` exists as workspace dep (`codspeed-divan-compat 4.2.1`);
  `#[turso_macros::divan_bench]` pattern lives in `core/benches/*.rs`.
- `#[turso_macros::codspeed_criterion_benchmark]` pattern lives in
  `extensions/versioning/benches/versioning_bench.rs`.
- SQL-level driving pattern for benches/tests:
  `extensions/versioning/tests/writeback_atomicity.rs` (`turso::Builder`
  in-memory + `exec` helper).
- CLI subprocess pattern: `testing/sqltest/src/backends/cli.rs`
  (tursodb `<db> -q -m list`, SQL on stdin, `parse_list_output`).
- doltlite shell: `doltlite <db>` with `.headers off` / `.mode list` dot
  commands on stdin, pipe-separated output (see any `vc_oracle_*_test.sh`).
- Normalization rules exist in doltlite's `test/lib/vc_oracle_common.sh` and
  per-test `normalize()` (fold 40-hex/32-base32 hashes to `<HASH>`, fold
  true/false to 1/0, sort comma lists, strip `\r` and quotes).

## 2. vc_*.sqltest Corpus Gaps (evidence-based)

Verified zero-coverage APIs across the 9 existing files: `dolt_tag`,
`dolt_merge_base`, `dolt_clean`. Verified scenario gaps: multi-table merge
(all merges are single-table), no-PK table merge, `dolt_merge_status`
lifecycle depth (only mid-merge state is asserted, not after-resolve and
after-commit), `dolt_rebase` plan-table form depth.

Verified vtab gaps blocking differential scenarios (doltlite exposes these;
turso does not — probed 2026-09-04):
- `dolt_branches` table: columns `name, hash, latest_commit_message, remote,
  branch, dirty` (see doltlite `vc_oracle_branches_test.sh:32`)
- `dolt_tags` table: columns `tag_name, tag_hash, message` (see doltlite
  `vc_oracle_tags_test.sh:32`)

Known dialect deltas the harness must absorb (scenario prelude whose output
is discarded; doltlite `dolt_config(k,v)` returns 0 while turso echoes the
value; both accept `dolt_commit('-m','msg')` flag form — scenarios MUST use
the flag form, never positional):

Borrow-first resolution (extend existing files; one new file only for no-PK
merge semantics which fit no existing file):

| Gap | Action |
|-----|--------|
| `dolt_branches` vtab | implement in `extensions/versioning` (borrow `vtab_log.rs` module pattern; register beside existing vtabs) + cover |
| `dolt_tags` vtab | same + cover |
| `dolt_tag` create/list/delete/checkout-at | extend `vc_branch_basic.sqltest` |
| `dolt_clean` | extend `vc_commit_gate.sqltest` |
| `dolt_merge_base` (common ancestor, NULL on unrelated) | extend `vc_merge_basic.sqltest` |
| `dolt_merge_status` after resolve and after commit | extend `vc_merge_basic.sqltest` |
| multi-table merge (3+ tables, schema + data) | extend `vc_merge_basic.sqltest` |
| `dolt_rebase` plan-table form (`--onto b --plan`, plan edit, continue) | extend `vc_replay_basic.sqltest` |
| no-PK table merge (full-row identity) | NEW `vc_merge_no_pk.sqltest` |

Every new test block must first be run RED-verified (assert current behavior;
if an assertion fails, that is a bug: fix in `extensions/versioning/` minus
O5-owned files) and pass the runner afterwards. `vc_gc.sqltest` is explicitly
OUT of O6 scope (no SQL surface; `gc.rs` is O5-owned).

Corpus gate: `make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'`
passes with the extended corpus.

## 3. Differential Oracle Harness — `testing/doltlite-oracle/`

New crate `doltlite-oracle` (justified: no existing harness runs the same
scenario against an external binary and diffs normalized output; sql_gen
generates random SQL, the fuzzer compares in-process). Add as a workspace
member (single-line append to root `Cargo.toml` members — shared file with
O5, append-only).

### 3.1 Layout

```
testing/doltlite-oracle/
├── Cargo.toml            # bin + lib, no async runtime needed (std::process)
├── src/lib.rs            # scenario model, normalization, structured diff
├── src/runner.rs         # spawn tursodb + doltlite, feed SQL, capture output
├── src/main.rs           # CLI: run --filter <name> | --batch | --time
├── src/check_buckets.rs  # guard: manifests valid, buckets non-empty, ownership
└── buckets/
    ├── refs-workspace/       # branch/tag/ref scenarios (.sql files + manifest)
    ├── diff-history-data/    # diff/history/blame scenarios
    ├── merge-replay-schema/  # merge/rebase/cherry-pick/schema-diff scenarios
    ├── feature-interaction/  # cross-feature combos
    └── remotes-recovery/     # O5-OWNED: manifest may exist; O6 never writes scenarios here
```

### 3.2 Scenario format

A scenario is a plain `.sql` file: setup DDL/DML + `dolt_*` calls + final
projection queries. Both engines run the identical script. Differential
pass = normalized outputs agree (no golden files). Scenarios must `ORDER BY`
in projections where row order is engine-dependent, and must not select raw
timestamps or unsorted set-valued columns.

### 3.3 Runner contract

- Spawn `tursodb :memory: -q -m list` (path default `target/debug/tursodb`,
  override `--tursodb` / `$TURSODB_BIN`) and
  `doltlite :memory:` (path override `--doltlite` / `$DOLTLITE_BIN`) with
  `.headers off` + `.mode list` prepended, script on stdin, capture stdout.
- Engine errors are compared too: normalize error text (strip `near line N`,
  trailing result codes, miette `×`/`│` decoration — borrow
  `normalize_cli_error` logic from `testing/sqltest/src/backends/cli.rs`).
- Exit non-zero on divergence; `--filter <name>` runs one scenario,
  `--batch` runs every bucket; `--time` times both engines (median of N reps)
  for the PERF_O6 report.

### 3.4 Normalization (borrowed from doltlite's oracle normalizers)

1. Strip `\r` and trailing whitespace.
2. Fold 40-hex and 32-base32 hash tokens to `<HASH>`.
3. Fold `true`/`false` cells to `1`/`0`.
4. Sort comma-separated lists inside a cell (e.g. `unmerged_tables`).
5. Collapse runs of spaces to one.

### 3.5 Structured diff output

```
scenario: <name>
turso_lines: <N>
doltlite_lines: <M>
diffs:
  line <L>: turso=<X> doltlite=<Y>
```

### 3.6 `check_buckets` guard

Rust test in `src/check_buckets.rs`, always runs in `cargo test`:
1. Every bucket dir has a manifest; every manifest entry exists on disk.
2. Every O6-owned bucket has ≥1 scenario.
3. `remotes-recovery/` contains ZERO O6-authored scenario files (any file
   there must be O5's; guard asserts O6's manifests never list it).
4. Full differential pass runs only when `$DOLTLITE_BIN` is set (integration
   test, `#[ignore]` without the env) and asserts zero divergences, printing
   `bucket | scenarios | pass | fail | skip`.

## 4. Benchmarks

### 4.1 Criterion API benches — NEW `extensions/versioning/benches/versioning_api_bench.rs`

Drives SQL through the `turso` in-memory driver (borrow the
`writeback_atomicity.rs` connect/exec pattern). All functions annotated
`#[turso_macros::codspeed_criterion_benchmark]`:

| Bench | Operation |
|-------|-----------|
| `bench_vc_commit_single` | 1-row `dolt_add`+`dolt_commit` |
| `bench_vc_commit_batch_1k` | 1000-row commit (setup outside timed loop) |
| `bench_vc_branch_create` | `dolt_branch` on fresh store |
| `bench_vc_branch_switch` | `dolt_checkout` ping-pong |
| `bench_vc_diff_no_change` | `dolt_diff` HEAD vs working, clean |
| `bench_vc_diff_100_rows` | `dolt_diff` over 100 modified rows |
| `bench_vc_merge_fast_forward` | fast-forward merge |
| `bench_vc_merge_3way` | diverged 3-way merge |
| `bench_vc_log_10_commits` / `bench_vc_log_100_commits` | log walk |
| `bench_vc_gc_empty` | `gc::mark`+`sweep` on empty snapshot (read-only use of O5 API) |
| `bench_vc_gc_100_commits` | synthetic 100-root snapshot mark+sweep |

### 4.2 Divan benches — NEW `extensions/versioning/benches/versioning_divan.rs`

Wiring: `divan = { workspace = true }` dev-dep + `[[bench]] harness = false`,
dual `main()` codspeed/local pattern from `core/benches/alloc_collections.rs`.
All functions annotated `#[turso_macros::divan_bench]` with `args` sweeps:

| Bench | Sweep |
|-------|-------|
| `divan_vc_commit_batch` | rows: 10, 100, 1000 |
| `divan_vc_branch_create_delete` | single pair measurement |
| `divan_vc_diff_stat` | rows: 10, 100, 1000 |
| `divan_vc_merge` | rows: 10, 100, 1000 |
| `divan_vc_log` | depth: 5, 25, 100 |
| `divan_vc_gc` | roots: 10, 50, 200 (read-only `gc` API) |

### 4.3 Recording

Run both bench targets locally (debug `cargo bench` forbidden to run with
`--release`; use default `cargo bench -p turso_versioning`), record numbers in
`extensions/versioning/PERF_O6.md`.

## 5. PERF_O6.md — Performance vs Doltlite

Location: `extensions/versioning/PERF_O6.md`. Sections:

1. Machine & toolchain (match `PERF_O1.md` format).
2. Methodology: scenarios from §4, `--time` median of ≥30 reps per engine,
   interleaved engine order, same in-memory mode.
3. Results: one row per §4.1 operation — Turso p50, Doltlite p50, ratio,
   winner. Doltlite driven by the same scenario SQL through its shell in
   `:memory:` mode.
4. Honest analysis: wins, noise-level ties, losses with root cause.
5. Reproducibility: doltlite commit `5c67114`, build recipe, exact commands.
6. If doltlite cannot complete a scenario (missing API), mark the row
   `unsupported-in-doltlite` with evidence — never silently drop it.

## 6. Validation Gates (every commit)

```bash
cargo test -p turso_versioning
cargo test -p doltlite_oracle
cargo clippy -p turso_versioning -p doltlite_oracle --all-features --all-targets -- --deny=warnings
cargo fmt --check
make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'   # when corpus touched
cargo bench -p turso_versioning --bench versioning_api_bench -- --test   # compile check only
cargo bench -p turso_versioning --bench versioning_divan -- --test       # compile check only (if supported)
```

## 7. Commit Discipline

- Atomic, subject ≤10 words, scope prefix: `oracle:`, `sqltests:`, `benches:`, `versioning:`.
- Never push. Never commit secrets.
- Suggested sequence:
  1. `oracle: add doltlite differential harness` (skeleton + normalization unit tests RED-verified in body)
  2. `oracle: add same-scenario runner and buckets`
  3. `oracle: wire check_buckets guard`
  4. `sqltests: cover tag clean merge_base` (corpus RED-verified bodies)
  5. `sqltests: cover merge status lifecycle depth`
  6. `sqltests: cover multi table and no-pk merge`
  7. `sqltests: cover rebase plan form`
  8. `benches: add criterion versioning api matrix`
  9. `benches: add divan versioning sweeps`
  10. `oracle: time both engines for perf report`
  11. `versioning: record perf-vs-doltlite report`
  12. `docs: add vc-perf-benchmarks skill`

## 8. Deliverables Checklist

- [x] `testing/doltlite-oracle` crate: runner, normalization, structured diff, `--filter/--batch/--time`, and deterministic `--seeds START:END`
- [x] Buckets populated: refs-workspace, diff-history-data, merge-replay-schema, feature-interaction (≥2 scenarios each); remotes-recovery untouched by O6
- [x] `check_buckets` guard green in `cargo test`
- [x] Differential run green with `$DOLTLITE_BIN` set (18 scenarios and the 1:10000 seed sweep)
- [x] Corpus gaps closed (§2), runner green
- [x] 12 Criterion API benches + 6 Divan benches compile and run
- [x] `PERF_O6.md` with honest per-operation comparison
- [x] `.claude/skills/vc-perf-benchmarks/SKILL.md`
- [x] All gates green; no O5-owned path modified; zero waived blockers

## 9. Status / fix log — 2026-09-04

- `nix develop -c cargo test -p turso_versioning`: 357 passed.
- `nix develop -c cargo test -p doltlite_oracle`: 10 passed, 1 ignored (the
  ignored test requires `$DOLTLITE_BIN` and is covered by the explicit batch
  run).
- Pinned differential run: 18/18 checked-in scenarios passed against
  DoltLite commit `5c67114`; `run --seeds 1:10000` passed 10,000/10,000.
- Criterion API target: all 12 `-- --test` benchmark checks succeeded.
- Divan target: all 6 benches and declared argument sweeps compiled and were
  discovered by `-- --test`.
- Final integrated gates passed: `cargo fmt --all -- --check`, strict combined
  clippy for `turso_versioning`/`turso_ext`/`turso_core`, and the full pinned
  conformance run (SQLite 12,787 passed; Turso 1,675 passed; zero failures or
  errors). The final `turso_versioning` package run passed 452 tests. The
  versioning Criterion target also covers indexed versus linear ORM history
  lookup across 500 historical values.
