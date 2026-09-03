# RUBRIC O1 — Greenfield storage foundation in `extensions/versioning/`

Crate: `turso_versioning`. Greenfield only. Do NOT fork or modify
`core/storage/btree.rs`, `pager.rs`, `wal.rs`.

Doltlite reference: https://github.com/dolthub/doltlite (C fork, prolly tree
below `btree.h`). Key commits: `32cc53504` (prolly birth), `d8961c171`
(single-file WAL), `e62ec0d68` (Weibull), `a29ad438` (BLAKE3), `4a9930c53`
(GC stop-world), `16ab88447` (VFS).

## 1. API table (must all exist, exact names)

| Module | Item | Shape | Doltlite ref |
|---|---|---|---|
| `model` | `ChunkHash([u8; 20])` | tuple newtype, `Copy`, `Ord`, `Hash`, `as_bytes()`, `to_hex()`, `from_hex()` | `src/prolly_hash.h#L7` |
| `model` | `CommitHash` | distinct tuple newtype (NOT a type alias of `ChunkHash`), same traits | commit V2 object id |
| `model` | `FileMagic` | `DLTC` = `0x444C5443`, `FORMAT_VERSION: u16 = 12` | magic/format header |
| `model` | `Manifest` | bytes `[0,168)` file layout header; `encode()`/`decode()` round-trip | single-file layout |
| `model` | `NodeFlags` | bitflags: `INTKEY`, `BLOBKEY`, `COUNTS` | node header flags |
| `chunk` | `blake3_chunk_hash(data: &[u8]) -> ChunkHash` | BLAKE3 XOF/output truncated to 20 bytes, via `blake3` crate | commit `a29ad438` |
| `chunk` | `split_decision(hash32: u32, level: u32) -> bool` | xxHash32-based boundary test vs level threshold | `src/chunk_store.h#L14` |
| `chunk` | `WeibullChunker` | `MIN: usize = 512`, `MAX: usize = 16384`, `L: usize = 4096`; `push()`/`finish()` emits `Vec<Chunk>` with sizes in `[MIN, MAX]` | commit `e62ec0d68` |
| `chunk` | `ProllyNode::{encode,decode}` | magic `PNOD` = `0x504E4F44`, `MAX_ITEMS = 4096`, flags, subtree counts round-trip byte-identical | node codec |
| `chunk` | `Builder` | ordered insert → flushed `ProllyNode`s, rejects out-of-order keys | prolly builder |
| `chunk` | `Cursor` | ordered seek/next over node levels | prolly cursor |
| `chunk` | `MutMap` | in-memory ordered map with snapshot/get/put/delete | working map |
| `store` | `VersionStore` trait | `get_chunk`, `put_chunk`, `root_get`, `root_set`, `flush`, all-IO-via-VFS (no `std::fs`, no raw fd) | commit `16ab88447` |
| `store` | `SqliteOverlayStore` | `VersionStore` backed by Turso B-tree tables (overlay, not S4 fork) | overlay design |
| `store` | `ChunkIndex` | hash → location lookup, insert/delete/get | chunk index |
| `store` | `ChunkStaging` | unreferenced write staging, `stage()`/`commit()`/`drop()` | staging area |
| `store` | `WalState` | tags `CHUNK = 0x01`, `ROOT = 0x02`; append/replay | commit `d8961c171` |
| `gc` | `mark()` / `sweep()` | stub: walk refs+commits+working-sets, return live `ChunkHash` set + removal list; full collector is O5 | commit `4a9930c53` |
| every error | `StoreError` (`thiserror`) | exact strings below; no `anyhow`, no stringly errors | — |

## 2. Tests (fail-before on placeholder, pass-after; all in-crate `#[cfg(test)]`)

- `model_chunk_hash_hex_roundtrip`
- `model_commit_hash_is_not_chunk_hash` (compile-time distinctness + runtime check)
- `model_manifest_168_byte_roundtrip`
- `chunk_blake3_known_vector` (20-byte truncation of BLAKE3("") = first 20 bytes of `af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262`)
- `chunk_weibull_sizes_within_min_max` (mixed payload, assert every chunk in `[512, 16384]`)
- `chunk_weibull_deterministic` (same input → identical boundaries twice)
- `chunk_node_pnod_roundtrip` (flags INTKEY/BLOBKEY/COUNTS + subtree counts survive)
- `chunk_node_rejects_bad_magic` (expects `StoreError::BadNodeMagic`)
- `chunk_builder_rejects_unordered` (expects `StoreError::UnorderedInsert`)
- `store_overlay_put_get_roundtrip`
- `store_wal_replay_restores_root`
- `gc_stub_marks_live_set`

## 3. Exact error strings (`StoreError` Display)

- `BadNodeMagic` → `"bad node magic: expected PNOD"`
- `UnorderedInsert` → `"unordered insert into prolly builder"`
- `ChunkNotFound` → `"chunk not found: {hex}"`
- `BadManifest` → `"bad file manifest"`
- `CorruptWal` → `"corrupt WAL record"`

## 4. Perf targets (beat doltlite single-thread on same box; `cargo test --release` harness timing ok for O1)

- Weibull chunk 64 MiB mixed payload: ≥ 350 MiB/s split throughput.
- Node encode 4096 items: ≤ 150 µs p50.
- BLAKE3 chunk hash 1 MiB: ≤ 900 µs p50.

## 5. Gates (all green, no exceptions)

- `cargo fmt -p turso_versioning` clean.
- `cargo clippy -p turso_versioning --all-features --all-targets -- --deny=warnings` clean.
- `cargo test -p turso_versioning` all pass.
- No `void*`/raw hex strings in public API; no `std::fs` anywhere in crate (`grep -rn std::fs` empty).
- Callers before callees in every file; comments only for why (invariants, compat quirks), never what.
- Plain language in names/docs (no "bootstrap-safe", no "executable domain").

## 6. Also write

- `.claude/skills/prolly-chunk-store/SKILL.md` documenting the patterns used.
