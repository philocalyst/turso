# RUBRIC O1-FIX — resolve ALL perf blockers, no compromises, no waivers

Base specs RUBRIC_O1.md + V2 + V3 + V4 still apply in full. Prior state:
GREEN except 2 perf MISSes documented in PERF_O1.md. This FIX rubric makes
both blockers gates. NOTHING is waived. "Still a blocker" is not an allowed
outcome.

## F-BLOCKER-1. Weibull chunker ≥ 350 MiB/s on 64 MiB (was 11.2 MiB/s)

Suspected cause: per-byte full xxHash32 recompute over the 32-byte window
(`split_at_level` in `src/chunk.rs`).

Required fix:
- Replace per-offset full `xxhash32(window, 0)` recompute with a true
  rolling hash update as the window slides (buzhash-style rolling window).
- Behavior must match doltlite `src/prolly_chunker.c` +
  `src/prolly_xxhash.c`: same window size (32 bytes), same seed, same
  `split_decision(hash32, level)` threshold mapping
  (`threshold = 1u32.checked_shl(32 - level).unwrap_or(0)`, level 0 = never
  split). Fetch the doltlite sources from
  https://github.com/dolthub/doltlite to confirm exact window/seed/threshold.
- Chunk-boundary golden tests must pass bit-for-bit vs doltlite
  `test/chunker_boundary_golden.sh` semantics: same input bytes → same
  boundary offsets. If the shell script cannot run locally, vendor its
  vectors as a Rust test (input description + expected boundary list) and
  cite the source commit in a why-comment.
- Keep contract: `MIN = 512`, `MAX = 16384`, `L = 4096` as assoc consts on
  `WeibullChunker`; `push()`-emitted chunks always in `[MIN, MAX]`;
  `finish()` may emit one final tail `< MIN`; `L` stays wired via
  `level_for_target` (`new()` = `with_level(level_for_target(L))`, expected
  chunk size `2^level`); `level_for_target` stays covered by
  `chunk_weibull_nominal_target_near_l`.
- Keep `split_decision` + `xxhash32` public shapes (tests + benches use them)
  unless the rolling hash provably returns identical values, in which case
  `xxhash32` may become the one-shot helper over the rolling state and
  `split_decision` is untouched.

New/affected tests (TDD, fail-before first):
- `chunk_weibull_rolling_matches_oneshot`: rolling hash value equals
  `xxhash32(window, 0)` at every offset on a mixed payload (≥ 4 KiB).
- `chunk_weibull_boundary_golden`: golden boundary offsets (vendored from
  doltlite golden script or generated against doltlite C + pinned in test)
  reproduced bit-for-bit at the specified level.
- `chunk_weibull_deterministic`, `chunk_weibull_nonzero_level_splits`,
  `chunk_weibull_split_push_matches_single_push`,
  `chunk_weibull_max_plus_10_tail`,
  `chunk_weibull_nominal_target_near_l` keep passing unmodified in intent
  (boundaries may ONLY change if doltlite goldens demand it, then update
  with justification + cite).
- Add/keep a criterion bench `bench_weibull_64mib` proving throughput.

Pass criterion: `cargo bench -p turso_versioning --bench versioning_bench`
Weibull 64 MiB throughput ≥ 350 MiB/s on the dev machine
(Apple M3 Pro arm64; if run elsewhere, record machine + numbers in
PERF_O1.md and still clear 350 MiB/s).

## F-BLOCKER-2. BLAKE3 1 MiB ≤ 900 µs p50 (was 1.095 ms, ~22% over)

Suspected cause: `blake3 = { features = ["pure"] }` disables SIMD
assembly/C build.

Required fix:
- Toolchain is available: Apple clang 17 arm64 present (`/usr/bin/clang`);
  `nix` (Lix 2.95.2) also present. Use `nix develop -c cargo ...` or
  `nix-shell -p clang llvm` ONLY if the Apple clang build fails. Do not
  require nix when Apple clang suffices.
- Drop the `pure` feature; enable arch-appropriate SIMD features of the
  `blake3` crate (`neon` on aarch64, `sse41`/`avx2` on x86_64 — check the
  blake3 1.5.5 Cargo feature list and use exactly what exists; default
  features already pull the right SIMD on most platforms, so plain removal
  of `pure` may suffice).
- Verify the build compiles with the C/NEON path on this box and bench it.
- Byte-identical output contract (from `src/prolly_hash.c` semantics):
  `blake3_chunk_hash(data)` = first 20 bytes of BLAKE3(data);
  streaming zero-tail behavior unchanged (single `blake3::hash` call is
  fine); BLAKE3("") truncated = `af1349b9...25c9` (existing
  `chunk_blake3_known_vector` KAT keeps passing). Empty input does NOT
  become all-zero hash — the "empty = all-zero" line in the task refers to
  doltlite's zero-tail padding convention, not the hash value; do not change
  the KAT.

New/affected tests:
- `chunk_blake3_known_vector` (existing KAT) keeps passing.
- Add `chunk_blake3_1mib_deterministic`: same 1 MiB input hashed twice →
  identical 20 bytes; and `chunk_blake3_prefix_differs`: inputs differing
  in the last byte hash differently (guards truncation to 20 B still
  sensitive).
- Criterion bench `bench_blake3_1mib` proves ≤ 900 µs p50.

Pass criterion: BLAKE3 1 MiB median ≤ 900 µs p50 in
`cargo bench -p turso_versioning --bench versioning_bench`.

## F-GATES (all green, no exceptions, no waivers)

1. `cargo test -p turso_versioning --lib` → 50+ tests passing (add the new
   boundary/hash tests above; count includes all existing V4 tests).
2. `cargo clippy -p turso_versioning --all-features --all-targets -- --deny=warnings` clean.
3. `cargo fmt --check -p turso_versioning` clean (`cargo fmt -p turso_versioning` applied).
4. `cargo bench -p turso_versioning --bench versioning_bench -- --sample-size 20 --warm-up-time 1 --measurement-time 3`
   subset run proves BOTH: Weibull ≥ 350 MiB/s AND BLAKE3 ≤ 900 µs.
   Paste before/after numbers + machine into PERF_O1.md.
5. `grep -rn "std::fs" extensions/versioning/src/` empty.
6. `git status --short -- core/ sqlite/` empty (greenfield-only, untouched).
7. Node encode 4096 items stays ≤ 150 µs (regression check).
8. Callers-before-callees in every touched file; why-only comments;
   plain language.
9. Exact `StoreError` Display strings unchanged (O1 §3 + `BadHash`).
10. Update `.claude/skills/prolly-chunk-store/SKILL.md` with fix notes
    (rolling hash design + SIMD feature choice).

## F-PROCESS (TDD, validation all the way down)

1. Write fail-before tests FIRST (rolling-vs-oneshot, boundary golden,
   blake3 determinism), show them failing.
2. Then fix chunker + Cargo.toml blake3 features.
3. Re-run: lib tests → clippy → fmt → bench subset → grep → git status.
4. Reviewer loop (muse-spark) until ALL GREEN including perf numbers.
5. PERF_O1.md updated with before/after table; no "left as non-gate" language.
