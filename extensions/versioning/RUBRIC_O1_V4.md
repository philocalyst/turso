# RUBRIC O1 v4 — reviewer fix-list (reviewer verdict NOT GREEN, 6 BLOCKERs + 8 SHOULD-FIX)

Base specs RUBRIC_O1.md + V2 + V3 still apply. Current: 36 tests pass,
fmt/clippy clean, grep `std::fs` empty, `core/`+`sqlite/` untouched.
Fix in order below. All item names/shapes exact.

## B1. `WeibullChunker::L` is dead — read it or delete it
`chunk.rs`: `pub const L: usize = 4_096` is never read. Decide its meaning
and wire it in: use `L` as the nominal target size driving the level
threshold choice for splitting (document the mapping in a why-comment), so
`Self::L` is read in non-test code. If no honest mapping exists, delete the
const and record the removal + reason in PERF_O1.md. Add a test asserting
whatever contract you choose (e.g. average chunk size near `L` on a
splitting payload, or absence of the const).

## B2. Specify `finish()`-tail contract + literal `MAX+10` test
`chunk.rs` `finish()` emits whatever remains (e.g. 10 bytes), violating
`[MIN, MAX]` on a `MAX+10` zeros payload at level 0. Specified contract:
`push()`-emitted chunks are always in `[MIN, MAX]`; `finish()` may emit one
final tail chunk smaller than `MIN` (it flushes the merge-not-emit buffer).
Document this on `finish()` in one why-comment. Add tests:
- `chunk_weibull_max_plus_10_tail`: `MAX+10` zeros → every `push()`-emitted
  chunk in `[MIN, MAX]`; `finish()` tail is exactly 10 bytes.
- Keep the 100 000-zeros test; note in a comment why its remainder is
  in-range (arithmetic luck: 100000 − 6×16384 = 1696).

## B3. Test the hash-split path at a nonzero level
`new()` hardcodes level 0 (never splits; fixed-16384 splitter + tail).
Add `chunk_weibull_nonzero_level_splits`: chunk ≥64 KiB mixed payload with
`WeibullChunker::with_level` at a level that actually splits (pick level so
boundaries occur, e.g. level ≥ 8); assert every `push()`-emitted chunk in
`[MIN, MAX]` and boundaries identical across two runs. Upgrade
`chunk_weibull_split_push_matches_single_push` to use a payload that reaches
a split (≥64 KiB at that level), not 1000+2000 bytes.

## B4. Cursor + MutMap tests (V2 S10)
Add `chunk_cursor_seek_plus_two_advances`: 2-node cursor, `seek` to first
item ≥ key, then 2 `advance()` calls returning the next two items in order.
Add `mutmap_put_get_delete_snapshot`: put/get/delete/snapshot round-trip.

## B5. `mark()` ignores `commits`
`gc.rs` `mark()` seeds only from `working_sets`; `snapshot.commits` unread,
so a commit-rooted live chunk with empty `working_sets` sweeps live data.
Resolution: `CommitHash` currently has no chunk mapping, so document on
`GcSnapshot::commits` + `mark()` in a why-comment that commit traversal
resolves in O-follow-up once the commit object exists, and that TODAY callers
must place commit-reachable roots into `working_sets` (state the invariant).
Add test `gc_commits_field_documented`: snapshot with `commits` non-empty
and `working_sets` empty marks only working-set reachables (pins current
contract, fails if someone silently changes it). Full commit-graph walk is O5.

## B6. `replay` validates `data[0] == tag`
`store.rs` `replay`: after decoding each entry, check the embedded
`entry.data[0]` equals `entry.tag`, else `Err(CorruptWal)`. Add test
`store_wal_rejects_tag_data_mismatch`: entry with `tag = TAG_CHUNK` but
`data[0] = TAG_ROOT` (and vice versa) → `Err(CorruptWal)` and store
untouched (atomicity).

## S1. Exact Display-string assertions (O1 §3)
One test per `StoreError` variant asserting `to_string()` exactly:
- `BadNodeMagic` → `"bad node magic: expected PNOD"`
- `UnorderedInsert` → `"unordered insert into prolly builder"`
- `ChunkNotFound("ab…")` → `"chunk not found: ab…"` (use a real hex)
- `BadManifest` → `"bad file manifest"`
- `CorruptWal` → `"corrupt WAL record"`
- `BadHash("zz")` → `"bad hash string: zz"`

## S2. Orphan docs: `ChunkNotFound` + `ChunkIndex` producer
`get_chunk` returns `Ok(None)`; nothing constructs `ChunkNotFound`; nothing
writes `ChunkIndex` in the real path. One why-comment on each: absence-is-`None`
today, variant reserved for the O2 B-tree path; `ChunkIndex` gets its producer
in O2 when `put_chunk` indexes on write. No behavior change.

## S3. `Builder` full-vs-unordered variant reuse
`Builder::push` returns `UnorderedInsert` when full (`len >= MAX_ITEMS`).
Document on `push` in one line that capacity rejection reuses the variant
(no silent `as u16` truncation — guarded). No new variant in O1.

## S4. `encode()` key/value `u16` length guard
`ProllyNode::encode` casts lengths `as u16` unchecked; >65535-byte key
silently corrupts. Add guard: `encode()` panics with a plain-language message
(`assert!(key.len() <= u16::MAX as usize, "prolly key too long")`, same for
value) — crash>corrupt. Add test `chunk_node_encode_rejects_huge_key`
(`#[should_panic]` or assert-based). Document the ≤64 KiB invariant.

## S5. Distinctness test teeth (V2 S11)
Strengthen `model_commit_hash_is_not_chunk_hash`: add a generic-bound helper
`fn requires_chunk<T: Into<ChunkHash>>(t: T) -> ChunkHash` (or `Borrow`)
where only `ChunkHash` implements the bound, proving `CommitHash` is refused
at compile time (comment explains the compile-fail property), plus keep the
same-bytes `as_bytes()` equality assertion.

## S6. SKILL.md nits
- Shift formula: document the level-0 special case (impl threshold 0 = never
  split via `checked_shl(32).unwrap_or(0)`; naive `1 << (32-level)` reads 2³²).
- Record tag mapping: rubric `CHUNK`/`ROOT` ↔ code `TAG_CHUNK`/`TAG_ROOT`.

## S7. Split the `macros/` fix into its own commit
`macros/src/lib.rs` collapsible_match fix is safe + necessary (do NOT revert
— reverting re-breaks workspace clippy) but must live in its own commit,
outside the O1 versioning change, so the close-out stays greenfield-pure.
When committing, use two commits. Do NOT push unless asked.

## Gates (all green, no exceptions)
- `cargo fmt -p turso_versioning` clean.
- `cargo clippy -p turso_versioning --all-features --all-targets -- --deny=warnings` clean.
- `cargo test -p turso_versioning` all pass (44+ tests expected).
- `grep -rn "std::fs" extensions/versioning/src/` empty.
- `git status --short -- core/ sqlite/` empty (still untouched).
