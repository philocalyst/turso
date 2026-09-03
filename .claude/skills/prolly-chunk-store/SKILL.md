# Prolly-Chunk-Store Skill

## Overview

This skill covers the `turso_versioning` crate in `extensions/versioning/`, implementing a doltlite-compatible prolly-tree chunk store foundation.

## Key Patterns

### Strongly-Typed Hashes

`ChunkHash` and `CommitHash` are distinct newtypes over `[u8; 20]`. They are never raw hex strings or `void*` in the public API. Use `as_bytes()`, `to_hex()`, and `from_hex()` for conversions.

### File Manifest

`Manifest` is a 168-byte header at offset 0 of the single-file store. Layout (big-endian):
- `[0,4)` magic `FileMagic::DLTC` = `0x444C5443`
- `[4,6)` format version `FileMagic::FORMAT_VERSION` = `12`
- `[6,26)` root `ChunkHash`
- `[26,46)` meta `ChunkHash`
- `[46,50)` commit count `u32`
- `[50,54)` working set count `u32`
- `[54,168)` reserved (zeroes)

`encode()`/`decode()` are byte-identical round-trip.

### NodeFlags

Tuple struct `NodeFlags(u16)` with associated constants `INTKEY`, `BLOBKEY`, `COUNTS`
(typed as `NodeFlags`, not bare `u16`) and a `contains(NodeFlags)` method.
`BitOr`/`BitOrAssign` combine flags without wrapping in the tuple form.

### Prolly Node Codec

Nodes use magic `PNOD` = `0x504E4F44`. Layout:
- `[0,4)` magic u32
- `[4,6)` flags u16 (INTKEY=0x01, BLOBKEY=0x02, COUNTS=0x04)
- `[6,8)` item count u16
- `[8,16)` subtree counts [u32; 2]
- `[16..)` items: (key_len:u16, key, val_len:u16, value)*

MAX_ITEMS = 4096. Decode rejects count > MAX_ITEMS and trailing bytes. Encode/decode round-trip byte-identical.
Key and value lengths are stored as u16 (≤ 64 KiB each), and `encode()` asserts that bound rather
than silently truncating — crash over corrupt.

### Weibull Chunker

Associated constants: `MIN=512`, `MAX=16384`, `L=4096`. Uses xxhash32 of a 32-byte sliding window
compared against a level threshold for boundary decisions. `L` is the nominal target size:
the expected chunk size is `2^level`, so `new()` picks `level = floor(log2(L))` (= 12 → expected
chunk ~L) and `with_level()` overrides it. Level 0 never hash-splits (see shift formula below).
`push()` only ever emits chunks in `[MIN, MAX]`; `finish()` may emit one final tail smaller than
`MIN` (undersized remainder is merged across pushes, never emitted early). Deterministic: same
input → identical boundaries. `push_emitted_len()` counts chunks emitted by `push` (never the tail).

### xxHash32 Split Decision

`split_decision(hash32, level)` returns `hash32 < threshold` where
`threshold = 1u32.checked_shl(32 - level).unwrap_or(0)`. Higher level → more splits → smaller
chunks. Level must be ≤ 32. Level 0 is a special case: the naive `1 << (32 - 0)` reads `2^32`,
which overflows `u32`, so the impl clamps via `checked_shl` and the threshold is `0` — never split.
xxHash32 is implemented inline (no new dependency).

### VersionStore Trait

```rust
pub trait VersionStore {
    fn get_chunk(&self, hash: &ChunkHash) -> Result<Option<Vec<u8>>, StoreError>;
    fn put_chunk(&mut self, hash: ChunkHash, data: &[u8]) -> Result<(), StoreError>;
    fn root_get(&self) -> Result<Option<ChunkHash>, StoreError>;
    fn root_set(&mut self, root: ChunkHash) -> Result<(), StoreError>;
    fn flush(&mut self) -> Result<(), StoreError>;
}
```

`ChunkTable` is a single-line re-export alias (`pub use VersionStore as ChunkTable;`),
not a second copy of the method list. `SqliteOverlayStore` aliases the in-memory
store: the real B-tree overlay binds in O2 via the Turso VFS, and the in-memory
stand-in keeps O1 greenfield (no `core/storage` dependency). All chunk I/O goes
through this abstraction; no direct file-system calls anywhere in the crate.

### WAL State

Tags: CHUNK=0x01, ROOT=0x02. Rubric names CHUNK/ROOT map to the code constants
`TAG_CHUNK`/`TAG_ROOT`. Private fields; constructed via `WalEntry::chunk()` / `WalEntry::root()`
which enforce `data[0] == tag`. `replay()` validates each entry's embedded `data[0]` equals its
`tag` (else `CorruptWal`), decodes all entries to `Vec<Op>` first, then applies — no partial store
mutation on corrupt WAL.

### GC Stub

`GcSnapshot` holds `all_chunks`, `refs`, `commits` (Vec<CommitHash>),
and `working_sets` (Vec<Vec<ChunkHash>>).
`mark()` walks the reference graph from working-set heads only — `commits` is never read today
(commit traversal lands in O5 once the commit object exists), so callers must mirror
commit-reachable roots into `working_sets` or they get swept.
`sweep()` reads `snapshot.all_chunks` and returns chunks not in the live set. Full collector is O5.

### Builder

Ordered insert into prolly nodes. Rejects out-of-order keys and count > MAX_ITEMS with
`StoreError::UnorderedInsert`. Capacity rejection reuses that same variant (the count is guarded,
never a silent `as u16` truncation).

### Cursor

Seek lands at first item ≥ key; advance returns the item after the current position.
`seek` is a linear scan across items (not binary search).

### MutMap

In-memory ordered map with snapshot/get/put/delete. Lookups and insert points are
binary search over the sorted snapshot vector.

## Error Types

All errors use `thiserror`. Exact Display strings:
- `BadNodeMagic` → "bad node magic: expected PNOD"
- `UnorderedInsert` → "unordered insert into prolly builder"
- `ChunkNotFound` → "chunk not found: {hex}"
- `BadManifest` → "bad file manifest"
- `CorruptWal` → "corrupt WAL record"
- `BadHash` → "bad hash string: {hex}"

## Code Conventions

- Callers before callees in every file
- Comments only for why (invariants, compat quirks), never what
- Plain language names
- No `void*` or raw hex in public API
- No direct file-system calls in the crate
