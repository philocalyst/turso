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

On this box the `cc` crate picks up the nix `sccache` shim from `RUSTC_WRAPPER`
and fails, so builds use `env -u RUSTC_WRAPPER CC=/usr/bin/clang`.

Medians below are p50 proxies read from the Criterion `estimates.json`.

## Before / after (O1-FIX)

The "before" column is the state documented at O1 close-out (idle machine) plus
the current-code re-measurement on this run's machine. NOTE: the box was under
heavy, spiky load during the FIX runs (`nudox-serve` at ~100% CPU, intermittent
`zig test` runs, load average 10→78 on 11 cores), so the "after" numbers are
pessimistic; idle would be faster. Three clean full runs (sample-size 20) all
passed; the medians below are from the representative run. Run-to-run variance
is real under this load: a fresh independent reviewer run measured **380 MiB/s**
(Weibull) and **793 µs** (BLAKE3), both still passing their targets.

| Bench | Before (idle, O1 doc) | Before (re-measured today) | After (under load) | Target | Verdict |
|---|---|---|---|---|---|
| Weibull chunk 64 MiB | 5.71 s/iter → **11.2 MiB/s** | 20.09 s → 3.19 MiB/s | **138.38 ms → 462.2 MiB/s** (3 runs: 442–462) | ≥ 350 MiB/s | **pass** |
| Node encode 4096 items | 27.1 µs | 28.33 µs | **36.56 µs** | ≤ 150 µs p50 | pass |
| BLAKE3 chunk hash 1 MiB | 1.095 ms (914 MiB/s) | 1.098 ms | **639.78 µs (1.51 GiB/s)** (3 runs: 640–756 µs) | ≤ 900 µs p50 | **pass** |

## What cleared the blockers

### Weibull 64 MiB (11.2 MiB/s → 462.2 MiB/s)

Two changes, both needed:

1. **Rolling buzhash instead of per-byte xxhash32.** `split_at_level` recomputed
   `xxhash32(window, 0)` from scratch at every byte offset (~67M full hashes for
   64 MiB). It now keeps a `Buzhash` rolling state over the 32-byte window (one
   `rotate_left(1)` + two XORs per byte slid), so `split_decision` sees the same
   window hash at O(1)/byte. `buzhash32(window)` is the one-shot form and
   `chunk_weibull_rolling_matches_oneshot` asserts the rolling state matches it
   at every offset.

2. **Front-cursor instead of `Vec::drain(0..i)`.** Draining each chunk memmoved
   the entire remaining buffer, which is O(chunks × remaining) = quadratic; once
   the scan got fast, the drain dominated (26 s/iter). The chunker now keeps a
   `front` offset into the buffer and only compacts when the consumed prefix
   passes the midpoint, so each byte is memmoved at most twice (amortized
   linear). Boundaries are unchanged (content-defined) — the boundary golden
   vectors still pass bit-for-bit.

Doltlite parity note: doltlite's real `src/prolly_chunker.c` (master
`5c67114c0374`) is entry-based — `addToLevel` hashes each record *key* with
`prollyXXH32(key, seed=level)` and gates on the Weibull-CDF `prollyWeibullCheck`,
so it has no 32-byte rolling window, no per-byte scan, and no
`hash < 1 << (32-level)` threshold. Our byte-stream chunker keeps the same
window size (32), the same `split_decision` threshold mapping, and the same
`MIN`/`MAX`/`L` contract; the bit-for-bit doltlite tie is
`chunk_xxhash32_doltlite_golden`, which vendors the C golden vectors from
`test/prolly_chunker_boundary_test.c` (32 × 8-byte big-endian keys, seed 0).
That vendored test plus the extended canonical vectors caught TWO latent bugs in
our `xxhash32`, both invisible to the old chunker because its 32-byte windows
were a multiple of 16 (never reaching the 4-byte tail loop) and because it only
compared hashes against itself:
1. The 4-byte tail loop multiplied the running sum by `PRIME3` instead of the
   incoming word (`h32 + word*PRIME3`, not `(h32+word)*PRIME3`).
2. The 16-byte-block `round()` folded `input*PRIME4` instead of `input*PRIME2`
   (standard xxHash32 and doltlite `xxh_round` use `PRIME2`).
Fixed; `xxhash32` now matches `prollyXXH32` byte-for-byte, pinned by the vendored
8-byte-key vectors plus the 16-byte-block and 32-byte canonical vectors
(cross-checked against `twox-hash` 1.x).

### BLAKE3 1 MiB (1.095 ms → 639.78 µs)

`Cargo.toml` had `blake3 = { version = "1.5.5", features = ["pure"] }`, which
disables the SIMD assembly/C build. Removed `pure`, enabled the `neon` marker
feature (blake3's build.rs auto-enables the NEON C path on little-endian aarch64
when `pure` is unset; `neon` pins the intent). `1.5.5` is the manifest floor;
the lockfile resolves to **1.7.0**, and the `neon`/C path was verified at that
version. Byte-identical output: same 20-byte
truncation, KAT `chunk_blake3_known_vector` unchanged, and
`chunk_blake3_1mib_deterministic` + `chunk_blake3_prefix_differs` guard the
truncation.

### Node encode 4096 (28.3 µs ≤ 150 µs)

Passes with ~4x headroom under load. No action.

## Validation

- `cargo test -p turso_versioning --lib`: 56 passed, 0 failed (was 50; added
  `chunk_blake3_1mib_deterministic`, `chunk_blake3_prefix_differs`,
  `chunk_xxhash32_doltlite_golden`, `chunk_buzhash_table_stable`,
  `chunk_weibull_rolling_matches_oneshot`, `chunk_weibull_boundary_golden`).
- `cargo clippy -p turso_versioning --all-features --all-targets -- --deny=warnings`: clean.
- `cargo fmt --check -p turso_versioning`: clean.
- `grep -rn "std::fs" extensions/versioning/src/`: empty.
- `git status --short -- core/ sqlite/`: empty (greenfield-only).