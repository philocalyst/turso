//! Turso versioning extension: prolly-tree chunk store foundation.
//!
//! Greenfield storage in `extensions/versioning/`. Does not fork
//! `core/storage/btree.rs`, `pager.rs`, or `wal.rs`.
//!
//! Layout:
//! - `model` holds typed hashes (`ChunkHash`, `CommitHash`), file magic,
//!   manifest encode/decode, and node flags.
//! - `chunk` holds blake3 hashing, xxhash32 split decisions, Weibull
//!   chunking, prolly node codec, ordered builder, cursor, and working map.
//! - `store` holds the `VersionStore` trait (with `SqliteOverlayStore` overlay
//!   alias) plus the in-memory store, chunk index, staging area, and WAL state.
//! - `gc` holds the mark-sweep stub (full collector lands in O5).

pub mod chunk;
pub mod gc;
pub mod model;
pub mod store;
