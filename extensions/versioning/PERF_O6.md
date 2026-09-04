# PERF_O6 — versioning API scenarios vs DoltLite

## Machine and toolchain

Machine: Apple Mac15,6 (Apple Silicon arm64), 11 CPUs, 36 GB RAM, Darwin
24.6.0.

Toolchain: `rustc 1.88.0`, `cargo 1.88.0`, pinned by `nix develop`.

Turso binary: `target/debug/tursodb` from this working tree.

DoltLite binary: `/private/tmp/doltlite-research/build/doltlite`, research clone
commit `5c67114c037485ef4d0e4653abf260dc64125298`.

## Methodology

The oracle runs the same SQL script against both shells in a fresh `:memory:`
database. It prepends `.headers off` and `.mode list`, normalizes hashes,
booleans, list-valued cells, and shell error decoration, and fails on any
remaining difference. `--time` alternates engine order and reports the median
of 30 repetitions. These are process wall-time measurements: each scenario's
setup and shell startup are included, so they are comparison proxies rather
than claims about an isolated function's CPU time. The Criterion benches keep
setup outside their timed closures and are the operation-level measurements.

Measured with:

```sh
nix develop -c sh -c \
  'DOLTLITE_BIN=/private/tmp/doltlite-research/build/doltlite \
   TURSODB_BIN=$PWD/target/debug/tursodb \
   cargo run -p doltlite_oracle -- run --time --filter perf_ --reps 30'
```

The 18 checked-in scenarios passed the same differential run. The generated
deterministic sweep also passed all 10,000 seeds with:

```sh
nix develop -c sh -c \
  'DOLTLITE_BIN=/private/tmp/doltlite-research/build/doltlite \
   TURSODB_BIN=$PWD/target/debug/tursodb \
   cargo run -p doltlite_oracle -- run --seeds 1:10000'
```

## Results

Values are milliseconds, p50 over 30 interleaved process runs. Ratio is
`Turso / DoltLite`; a lower value wins. The scenario column identifies the
exact checked-in script used for each row.

| Criterion operation | Scenario | Turso p50 | DoltLite p50 | Ratio | Winner |
|---|---|---:|---:|---:|---|
| `bench_vc_commit_single` | `perf_commit_single` | 16.251 | 5.020 | 3.237 | DoltLite |
| `bench_vc_commit_batch_1k` | `perf_commit_batch_1k` | 158.683 | 13.793 | 11.504 | DoltLite |
| `bench_vc_branch_create` | `perf_branch_create` | 15.722 | 4.826 | 3.258 | DoltLite |
| `bench_vc_branch_switch` | `perf_branch_switch` | 16.747 | 5.081 | 3.296 | DoltLite |
| `bench_vc_diff_no_change` | `perf_diff_no_change` | 15.962 | 4.958 | 3.220 | DoltLite |
| `bench_vc_diff_100_rows` | `perf_diff_100_rows` | 28.332 | 5.070 | 5.588 | DoltLite |
| `bench_vc_merge_fast_forward` | `perf_merge_fast_forward` | 18.370 | 5.438 | 3.378 | DoltLite |
| `bench_vc_merge_3way` | `perf_merge_3way` | 19.425 | 5.569 | 3.488 | DoltLite |
| `bench_vc_log_10_commits` | `perf_log_10` | 23.716 | 5.404 | 4.388 | DoltLite |
| `bench_vc_log_100_commits` | `perf_log_100` | 128.978 | 12.817 | 10.063 | DoltLite |
| `bench_vc_gc_empty` | — | unsupported-in-doltlite | unsupported-in-doltlite | — | — |
| `bench_vc_gc_100_commits` | — | unsupported-in-doltlite | unsupported-in-doltlite | — | — |

The GC rows are intentionally not silently dropped. O6 may use the existing
read-only `gc::{GcSnapshot, mark, sweep}` API for the Turso benchmark, but the
O5 contract forbids adding or invoking a `dolt_gc` SQL surface or remotes
logic in the differential harness. There is therefore no honest DoltLite
side measurement for those two rows.

## Analysis

DoltLite is faster in every comparable process-level scenario on this run,
with ratios from 3.220x to 11.504x. The largest gap is the 1,000-row commit;
the next largest is the 100-commit log walk. Those cases do more versioning
work, but the result also includes Turso shell startup and SQL setup, so it is
not evidence that the underlying Rust operation is intrinsically that many
times slower. There were no noise-level ties in these 30-repetition medians.

The operation-level Criterion and Divan targets are kept separate from this
shell comparison. Their setup is outside the timed closure, which makes them
the right place to investigate an implementation regression after this
baseline is established.

## Reproducibility

Build DoltLite from the pinned research checkout, then build Turso in the
pinned shell:

```sh
cd /private/tmp/doltlite-research
mkdir -p build
cd build
../configure
make doltlite

cd /Users/mileswirht/Downloads/turso
nix develop -c cargo build -p turso_cli --bin tursodb
```

Run `check-buckets`, `run --batch`, `run --time --filter perf_ --reps 30`, and
`run --seeds 1:10000` as shown above. Do not replace a missing reference
operation with an invented number; keep it marked unsupported until both
engines have a comparable, in-scope operation.
