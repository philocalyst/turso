# RUBRIC O1 v3 — final close-out (baseline: 34 pass / 1 fail)

Base specs RUBRIC_O1.md + RUBRIC_O1_V2.md still apply. This v3 lists only
remaining deltas, in fix order. Baseline `cargo test -p turso_versioning`:
34 passed, 1 failed (`chunk::tests::split_decision_levels`).

## F1. Fix `split_decision_levels` test (the 1 red test — impl is right, test is wrong)
`chunk.rs`: `split_decision` uses `threshold = 1 << (32 - level)`, i.e.
split iff `hash < 2^(32-level)`. So level 1 → threshold 2^31 (half of all
hashes split), NOT "only hash 0 splits". Rewrite the test:
- level 0: threshold 0 → `!split(0,0)`, `!split(MAX,0)`.
- level 1: threshold 2^31 → `split(0,1)`, `split(2^31 - 1, 1)`, `!split(2^31, 1)`, `!split(MAX,1)`.
- level 31: threshold 2 → `split(0,31)`, `split(1,31)`, `!split(2,31)`.
- level 32: threshold 1 → `split(0,32)`, `!split(1,32)`, `!split(MAX,32)`.

## F2. Exact rubric names: `VersionStore` trait + `SqliteOverlayStore` type
O1 API table requires `store::VersionStore` (trait with `get_chunk`,
`put_chunk`, `root_get`, `root_set`, `flush`) and `store::SqliteOverlayStore`.
V2 B1 allowed the `ChunkTable` + `InMemoryStore` fallback seam. Keep that
seam, but also expose the exact rubric names with zero duplication:
- Rename the trait `ChunkTable` → `VersionStore`; keep
  `pub type ChunkTable = VersionStore;` as a compat alias (or vice versa —
  one must be the real trait, the other a single-line alias, no duplicated
  method lists).
- Add `pub type SqliteOverlayStore = InMemoryStore;` with a why-comment:
  real B-tree overlay binds in O2 via the Turso VFS; in-memory stands in so
  O1 stays greenfield and never touches `core/storage`.
- Update `WalState::replay<T: ChunkTable>` bound to `T: VersionStore`
  (keep alias working).
- New test `store_version_store_names_exist`: generic fn bounded on
  `VersionStore` accepts `SqliteOverlayStore` + `InMemoryStore`.

## F3. `NodeFlags` assoc consts typed as `NodeFlags`
V2 B3: `model::NodeFlags(pub u16)` with `INTKEY`/`BLOBKEY`/`COUNTS` assoc
consts + `contains()`. Current consts are bare `u16`, forcing call sites to
wrap (`NodeFlags(NodeFlags::INTKEY | ...)`). Change to:
`pub const INTKEY: NodeFlags = NodeFlags(0x01);` (same for BLOBKEY 0x02,
COUNTS 0x04). Update `contains(self, flag: NodeFlags)`, `BitOr`/`BitOrAssign`
stays, and fix all call sites (`chunk.rs` node test, builder finish,
`model.rs` test). Keep `pub u16` inner field.

## F4. `grep -rn std::fs` must be literally empty (V2 N17/N18)
`store.rs:9` comment mentions `std::fs`, so grep hits. Reword to avoid the
literal string, e.g. "never touches the local file system directly" (no
`std` + `fs` adjacent with `::`). Verify with
`grep -rn "std::fs" extensions/versioning/src/` → empty.

## F5. Callers-before-callees + comment cull (V2 S14/S15)
- `chunk.rs`: order must read `blake3_chunk_hash` → `split_decision` →
  `WeibullChunker` → `ProllyNode` → `Builder`/`Cursor`/`MutMap` →
  `xxhash32` + `round` at bottom. Currently `xxhash32`/`round` sit at top;
  move them to the bottom.
- Delete what-narration comments; keep only why (PNOD value, BLAKE3-20 per
  doltlite a29ad438, zero-reserved per single-file layout, tail merge-not-emit
  choice). Fix SKILL.md "binary search" claim if cursor is linear scan
  (cursor `seek` is linear scan; `MutMap` uses binary search — state each
  plainly in SKILL.md).

## F6. Perf numbers (V2 B6)
`benches/versioning_bench.rs` exists — check it uses repo-mandated
`turso_macros` codspeed/divan macros per AGENTS.md. Run benches, write
`extensions/versioning/PERF_O1.md` with machine + numbers vs targets
(Weibull 64 MiB ≥ 350 MiB/s; node-encode 4096 items ≤ 150 µs p50;
BLAKE3 1 MiB ≤ 900 µs p50). No perf claims until measured.

## Gates (all green, no exceptions)
- `cargo fmt -p turso_versioning` clean.
- `cargo clippy -p turso_versioning --all-features --all-targets -- --deny=warnings` clean.
- `cargo test -p turso_versioning` all pass (35+ tests incl. new
  `store_version_store_names_exist` + fixed `split_decision_levels`).
- `grep -rn "std::fs" extensions/versioning/src/` empty.
- `.claude/skills/prolly-chunk-store/SKILL.md` exists, accurate (linear-scan
  cursor, binary-search MutMap, correct shift formula).
