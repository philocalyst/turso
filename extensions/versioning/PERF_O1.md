# PERF_O1 — measured numbers vs targets

Machine: Apple M3 Pro, 11 cores, 36 GB RAM, arm64, macOS 15.7.4.
Toolchain: rustc 1.97.1, `cargo bench` (optimized + debuginfo).

Bench file: `benches/versioning_bench.rs`. All three functions use
`#[turso_macros::codspeed_criterion_benchmark]` (Criterion via `turso_macros`,
per AGENTS.md). Ran with:

```sh
cargo bench -p turso_versioning --bench versioning_bench \
  -- --sample-size 20 --warm-up-time 1 --measurement-time 3
```

Medians below are p50 proxies read from the Criterion `estimates.json`.

| Bench | Median | Target | Verdict |
|---|---|---|---|
| Weibull chunk 64 MiB | 5.71 s/iter → **11.2 MiB/s** | ≥ 350 MiB/s | **MISS** (~31x short) |
| Node encode 4096 items | **27.1 µs** | ≤ 150 µs p50 | pass |
| BLAKE3 chunk hash 1 MiB | **1.095 ms** (914 MiB/s) | ≤ 900 µs p50 | MISS (~22% over) |

## Why the misses, and what clears them

### Weibull 64 MiB (11.2 MiB/s vs ≥ 350 MiB/s)

`split_at_level` computes a fresh `xxhash32` over the 32-byte window at every
byte offset from MIN to the split point (seed 0). 64 MiB ⇒ roughly 67M window
hashes; each is a full xxHash32 init + two 16-byte rounds + tail + avalanche.
That per-window cost is the bottleneck, not BLAKE3 or the drain/copy.

Hitting ≥ 350 MiB/s needs a rolling hash that is updated as the window slides
(doltlite's chunker does this), instead of recomputing from scratch per offset.
That is an algorithmic change, not a micro-opt, and is left to a follow-up so
O1 close-out stays correctness-only.

### BLAKE3 1 MiB (1.095 ms vs ≤ 900 µs)

`Cargo.toml` pins `blake3 = { version = "1.5.5", features = ["pure"] }`. The
`pure` feature disables the SIMD assembly/C build, which is deliberate: the
default blake3 build compiles `c/blake3_neon.c`, and this machine has no
`clang` in its toolchain (nix sccache), so the default build fails here. Pure
Rust blake3 caps 1 MiB hashing at ~914 MiB/s. Native NEON blake3 would clear
the 900 µs target comfortably; revisit when a C toolchain is available or via
an arch-gated SIMD-capable hash.

### Node encode 4096 (27.1 µs ≤ 150 µs)

Passes with ~5x headroom. No action.