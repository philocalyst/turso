//! Turso versioning extension: prolly-tree chunk store foundation.
//!
//! Greenfield storage in `extensions/versioning/`. Does not fork
//! `core/storage/btree.rs`, `pager.rs`, or `wal.rs`.
//!
//! Layout:
//! - `model` holds typed hashes (`ChunkHash`, `CommitHash`, `CommitId`,
//!   `RootHash`), file magic, manifest encode/decode, node flags, and the
//!   doltlite-compatible `VersionError` surface.
//! - `chunk` holds blake3 hashing, xxhash32 split decisions, Weibull
//!   chunking, prolly node codec, ordered builder, cursor, and working map.
//! - `store` holds the `VersionStore` trait (with `SqliteOverlayStore` overlay
//!   alias) plus the in-memory store, chunk index, staging area, and WAL state.
//! - `refs` holds `RefName`, `Revision` parsing, the `RefStore` trait, and
//!   compare-and-swap ref updates.
//! - `commit` holds the commit model, V2 codec, ancestor walk, and merge base.
//! - `staging` holds the working/staged sets and the `dolt_*` state machine.
//! - `session` holds the per-connection branch session state.
//! - `funcs` holds the pure string-level `SELECT dolt_*()` function specs.
//! - `gc` holds the mark-sweep stub (full collector lands in O5).

pub mod chunk;
pub mod commit;
pub mod conflicts;
pub mod constraints;
pub mod creds;
pub mod funcs;
pub mod gc;
pub mod merge;
pub mod merge_schema;
pub mod model;
pub mod refs;
pub mod remote;
pub mod remote_transport;
pub mod remote_wire;
pub mod replay;
pub mod session;
pub mod source;
pub mod staging;
pub mod store;
pub mod vtab_diff;
pub mod vtab_history;
pub mod vtab_log;
pub mod vtab_refs;

pub use commit::{Commit, CommitMeta, CommitStore, MemCommitStore};
pub use conflicts::{ConflictEntry, ConflictKind, ResolveSide};
pub use constraints::Violation;
pub use funcs::{FuncArg, FuncValue, VcOperations};
pub use merge::MergeOutcome;
pub use merge_schema::{SchemaDecision, SchemaIR};
pub use model::{CommitId, RootHash, VersionError, VersionResult};
pub use refs::{
    parse_qualified, parse_qualified_path, parse_revision, RefError, RefName, RefNs, RefStore,
    Revision,
};
pub use replay::{MergeResult, MergeStatusRow, RebaseState, RebaseStep};
pub use session::SessionBranch;
pub use staging::{StatusRow, TableSnapshot, TableState, VcStore};
