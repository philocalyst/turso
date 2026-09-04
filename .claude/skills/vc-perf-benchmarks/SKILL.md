# VC versioning performance skill

Use this skill for the O6 version-control performance matrix. Keep setup out
of the timed closure with Criterion `iter_batched` or Divan `with_inputs`.
Bench through the public SQL API where the operation is user-visible; the GC
bench may call only the existing read-only `gc::{mark, sweep}` API. Do not add
a `dolt_gc` SQL surface or touch O5 remote/gc implementation files.

Criterion functions in `extensions/versioning/benches/versioning_api_bench.rs`
must use `#[turso_macros::codspeed_criterion_benchmark]`. Divan functions in
`versioning_divan.rs` must use `#[turso_macros::divan_bench]`. Keep the
workspace benchmark names stable so CodSpeed can compare runs.

The required Criterion matrix covers one-row and 1,000-row commits, branch
create/switch, clean and modified diffs, fast-forward and three-way merges,
10- and 100-commit log walks, and empty/100-root mark+sweep. The Divan matrix
uses the row/depth/root sweeps declared in the source file.

Run compile checks in the pinned shell (debug profile; never add `--release`):

```bash
nix develop -c cargo bench -p turso_versioning --bench versioning_api_bench -- --test
nix develop -c cargo bench -p turso_versioning --bench versioning_divan -- --test
```

For comparisons, use `testing/doltlite-oracle` with the reference DoltLite
binary and at least 30 interleaved repetitions. Record the machine, toolchain,
reference commit, exact command, medians, and unsupported operations in
`extensions/versioning/PERF_O6.md`; never invent or silently omit a number.
