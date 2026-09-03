# RUBRIC O1 v2 — fix pass (reviewer verdict NOT GREEN, 6 BLOCKERs + 10 SHOULD-FIX)

Base spec RUBRIC_O1.md still applies. This v2 lists only deltas, in fix order.
All item names/shapes exact. Re-verify: fmt, clippy `--deny=warnings`, 21+ tests.

## B1. Real B-tree overlay (was: HashMap wearing SqliteOverlayStore name)
`store.rs`: `SqliteOverlayStore` must persist via Turso B-tree, not `HashMap`.
Create `__version_chunks(hash BLOB PK, data BLOB)` + `__version_root` tables
through `turso_core` public API (see `bindings/rust` for open/query pattern),
all I/O via Turso VFS. No `std::fs`. If `turso_core` dep is genuinely
unwirable from this crate, rename to `InMemoryStore` AND implement a
`ChunkTable` storage trait marking the exact VFS seam O-follow-up will bind —
but real overlay is strongly preferred. Delete stale "in-memory acceptable" docs.

## B2. Chunker actually chunks (L unused, [MIN,MAX] violated on tails)
`chunk.rs`: wire level logic so `L` is read; scan each `push()` once
`len >= MIN`; emit only chunks in `[MIN, MAX]`; carry `< MIN` tail in buffer
across pushes (document merge-not-emit choice in a why-comment). Expose exact
assoc consts `WeibullChunker::{MIN=512, MAX=16384, L=4096}` (free consts may go).
New tests: empty input → 0 chunks; 10-byte input → 0 emitted, 10 buffered;
`MAX+10` hostile-no-hash-split input → every emitted chunk in range;
split-push vs single-push determinism (`push(a);push(b)` == `push(ab)`).

## B3. Exact names (4 mismatches)
- `model::FileMagic` item with `DLTC: u32 = 0x444C5443` (replace bare `FILE_MAGIC`
  or keep as assoc const on `FileMagic`).
- `model::NodeFlags(pub u16)` with `INTKEY/BLOBKEY/COUNTS` assoc consts +
  `contains()`; `ProllyNode.flags: NodeFlags`.
- `ChunkStaging::drop()` (rename `drop_staged`; no alias).
- `GcSnapshot::{commits: Vec<CommitHash>, working_sets: Vec<Vec<ChunkHash>>}`
  that `mark()` walks.

## B4. Node codec hostile-bytes validation
`decode()`: reject `count > MAX_ITEMS` BEFORE allocating
(`Vec::with_capacity(count.min(MAX_ITEMS))`); reject trailing bytes
(`pos != buf.len()` → `BadNodeMagic`). `Builder::push`/`finish`: reject
`len > MAX_ITEMS` (no `as u16` silent truncation). New tests: count
`MAX_ITEMS+1` rejected; trailing byte rejected; oversized builder rejected.

## B5. Atomic WAL replay
`WalState::replay`: decode ALL entries to `Vec<Op>` first (no store mutation);
only then apply. Add test: corrupt WAL → `Err(CorruptWal)` AND store untouched.

## B6. Perf numbers (was: unmeasured)
Add benches (repo-mandated `turso_macros` codspeed/divan macros per AGENTS.md):
Weibull 64 MiB, node-encode 4096 items, BLAKE3 1 MiB. Paste numbers + machine
into `extensions/versioning/PERF_O1.md`. Targets: ≥350 MiB/s, ≤150 µs p50,
≤900 µs p50. Until measured, no perf claims.

## S7. from_hex error variant
New `StoreError::BadHash` (`"bad hash string"`) for decode/length failures in
both `from_hex`; `ChunkNotFound` only for well-formed-but-absent.

## S8. Staging/index offsets
`ChunkStaging::commit` assigns real offsets into `ChunkIndex`, or drops the
index param and indexes on `put_chunk`. No two hashes at offset 0.

## S9. split_decision level guard
`assert!(level <= 32)` top of fn. Test levels 0/1/31/32.

## S10. Cursor contract
Doc: "`seek` lands at first item ≥ key; `advance` returns item after current."
Fix `advance()` to match; test seek + 2 advances over 2-node cursor. SKILL.md:
"linear scan" (or implement binary search for real).

## S11. Distinctness test teeth
Generic-bound helper refusing `CommitHash` where `ChunkHash` required
(compile-fail proof) + assert same-bytes equality of `as_bytes()`; rename if
still smoke-only.

## S12. GcSnapshot::all_chunks
`sweep(snapshot, live)` reads `snapshot.all_chunks`, or delete field. Test:
`all ⊃ reachable` + unreachable + cycle + self-loop.

## S13. Stronger chunker tests
Covered by B2 new tests.

## S14. Callers-before-callees
`chunk.rs` order: `blake3_chunk_hash` → `split_decision` → `WeibullChunker` →
`ProllyNode` → `Builder/Cursor/MutMap` → `xxhash32`+`round` bottom. Fix
`model.rs:154` "calles" typo.

## S15. Comment cull
Delete all what-narration comments (reviewer list: chunk.rs:170,179,257,273,212-213;
store.rs:72,80,137; gc.rs:39; model.rs:119). Keep only why (PNOD value, BLAKE3-20
per doltlite a29ad438, zero-reserved per single-file layout). Fix stale docs
(store.rs:8-9, SKILL.md:45 shift formula). SKILL.md: no "binary search" claim.

## S16. WalEntry sealed
Private fields + `WalEntry::chunk(hash, data)` / `::root(hash)` ctors enforcing
`data[0] == tag`; `replay` validates else `CorruptWal`.

## N17/N18. Nits
Use `PRIME1/PRIME4` consts in `round()`; reword "std::fs" mentions so
`grep -rn std::fs` is literally empty.
