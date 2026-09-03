use std::collections::HashMap;

use crate::model::{ChunkHash, StoreError};

/// Storage seam: chunk persistence via VFS-backed tables.
///
/// B-tree overlay O2 implements this with `__version_chunks` and
/// `__version_root` tables through the Turso VFS layer. This trait
/// exists so the versioning crate never touches the local file system directly.
pub trait VersionStore {
    fn get_chunk(&self, hash: &ChunkHash) -> Result<Option<Vec<u8>>, StoreError>;
    fn put_chunk(&mut self, hash: ChunkHash, data: &[u8]) -> Result<(), StoreError>;
    fn root_get(&self) -> Result<Option<ChunkHash>, StoreError>;
    fn root_set(&mut self, root: ChunkHash) -> Result<(), StoreError>;
    fn flush(&mut self) -> Result<(), StoreError>;
}

/// Compat alias; O1 documents the trait as `VersionStore`.
pub use VersionStore as ChunkTable;

/// Hash → location lookup. Nothing writes it in the real path yet: `put_chunk`
/// indexes on write in O2, so this stays a table structure until then.
pub struct ChunkIndex {
    entries: HashMap<ChunkHash, usize>,
}

impl Default for ChunkIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkIndex {
    pub fn new() -> Self {
        ChunkIndex {
            entries: HashMap::new(),
        }
    }

    pub fn insert(&mut self, hash: ChunkHash, offset: usize) {
        self.entries.insert(hash, offset);
    }

    pub fn get(&self, hash: &ChunkHash) -> Option<usize> {
        self.entries.get(hash).copied()
    }

    pub fn delete(&mut self, hash: &ChunkHash) -> bool {
        self.entries.remove(hash).is_some()
    }
}

pub struct ChunkStaging {
    staged: HashMap<ChunkHash, Vec<u8>>,
}

impl Default for ChunkStaging {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkStaging {
    pub fn new() -> Self {
        ChunkStaging {
            staged: HashMap::new(),
        }
    }

    pub fn stage(&mut self, hash: ChunkHash, data: Vec<u8>) {
        self.staged.insert(hash, data);
    }

    /// Returns staged chunks without mutating any index.
    pub fn commit(&mut self) -> HashMap<ChunkHash, Vec<u8>> {
        std::mem::take(&mut self.staged)
    }

    #[allow(clippy::should_implement_trait)]
    pub fn drop(&mut self) {
        self.staged.clear();
    }
}

pub const TAG_CHUNK: u8 = 0x01;
pub const TAG_ROOT: u8 = 0x02;

pub struct WalEntry {
    tag: u8,
    data: Vec<u8>,
}

impl WalEntry {
    fn chunk(hash: ChunkHash, data: Vec<u8>) -> Self {
        let mut record = Vec::with_capacity(1 + 20 + data.len());
        record.push(TAG_CHUNK);
        record.extend_from_slice(hash.as_bytes());
        record.extend_from_slice(&data);
        WalEntry {
            tag: TAG_CHUNK,
            data: record,
        }
    }

    fn root(hash: ChunkHash) -> Self {
        let mut record = Vec::with_capacity(1 + 20);
        record.push(TAG_ROOT);
        record.extend_from_slice(hash.as_bytes());
        WalEntry {
            tag: TAG_ROOT,
            data: record,
        }
    }
}

pub struct WalState {
    entries: Vec<WalEntry>,
}

impl Default for WalState {
    fn default() -> Self {
        Self::new()
    }
}

impl WalState {
    pub fn new() -> Self {
        WalState {
            entries: Vec::new(),
        }
    }

    pub fn append_chunk(&mut self, hash: ChunkHash, data: Vec<u8>) {
        self.entries.push(WalEntry::chunk(hash, data));
    }

    pub fn append_root(&mut self, root: ChunkHash) {
        self.entries.push(WalEntry::root(root));
    }

    /// Decode all entries first, then apply — no partial store mutation on corrupt WAL.
    pub fn replay<T: VersionStore>(&self, store: &mut T) -> Result<(), StoreError> {
        let mut ops: Vec<Op> = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            if entry.data.first() != Some(&entry.tag) {
                return Err(StoreError::CorruptWal);
            }
            match entry.tag {
                TAG_CHUNK => {
                    if entry.data.len() < 21 {
                        return Err(StoreError::CorruptWal);
                    }
                    let mut hash = [0u8; 20];
                    hash.copy_from_slice(&entry.data[1..21]);
                    let chunk_data = entry.data[21..].to_vec();
                    ops.push(Op::PutChunk {
                        hash: ChunkHash(hash),
                        data: chunk_data,
                    });
                }
                TAG_ROOT => {
                    if entry.data.len() < 21 {
                        return Err(StoreError::CorruptWal);
                    }
                    let mut hash = [0u8; 20];
                    hash.copy_from_slice(&entry.data[1..21]);
                    ops.push(Op::SetRoot(ChunkHash(hash)));
                }
                _ => return Err(StoreError::CorruptWal),
            }
        }
        for op in ops {
            match op {
                Op::PutChunk { hash, data } => {
                    store.put_chunk(hash, &data)?;
                }
                Op::SetRoot(root) => {
                    store.root_set(root)?;
                }
            }
        }
        Ok(())
    }
}

enum Op {
    PutChunk { hash: ChunkHash, data: Vec<u8> },
    SetRoot(ChunkHash),
}

/// In-memory `VersionStore` for testing.
pub struct InMemoryStore {
    chunks: HashMap<ChunkHash, Vec<u8>>,
    root: Option<ChunkHash>,
}

/// Production overlay name. The real B-tree overlay binds in O2 via the Turso
/// VFS; the in-memory store stands in so O1 stays greenfield and never touches
/// `core/storage`.
pub type SqliteOverlayStore = InMemoryStore;

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryStore {
    pub fn new() -> Self {
        InMemoryStore {
            chunks: HashMap::new(),
            root: None,
        }
    }
}

impl VersionStore for InMemoryStore {
    fn get_chunk(&self, hash: &ChunkHash) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.chunks.get(hash).cloned())
    }

    fn put_chunk(&mut self, hash: ChunkHash, data: &[u8]) -> Result<(), StoreError> {
        self.chunks.insert(hash, data.to_vec());
        Ok(())
    }

    fn root_get(&self) -> Result<Option<ChunkHash>, StoreError> {
        Ok(self.root)
    }

    fn root_set(&mut self, root: ChunkHash) -> Result<(), StoreError> {
        self.root = Some(root);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_version_store_names_exist() {
        fn exercise<T: VersionStore>(store: &mut T) -> Result<(), StoreError> {
            let data = b"alias check".to_vec();
            let hash = crate::chunk::blake3_chunk_hash(&data);
            store.put_chunk(hash, &data)?;
            store.root_set(hash)?;
            store.flush()?;
            assert_eq!(store.get_chunk(&hash)?, Some(data));
            Ok(())
        }

        let mut overlay: SqliteOverlayStore = InMemoryStore::new();
        exercise(&mut overlay).unwrap();
        let mut mem: InMemoryStore = InMemoryStore::new();
        exercise(&mut mem).unwrap();
    }

    #[test]
    fn store_overlay_put_get_roundtrip() {
        let mut store = InMemoryStore::new();
        let data = b"hello versioning".to_vec();
        let hash = crate::chunk::blake3_chunk_hash(&data);
        store.put_chunk(hash, &data).unwrap();
        let got = store.get_chunk(&hash).unwrap().unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn store_wal_replay_restores_root() {
        let mut wal = WalState::new();
        let data = b"wal test data".to_vec();
        let hash = crate::chunk::blake3_chunk_hash(&data);
        wal.append_chunk(hash, data.clone());
        wal.append_root(ChunkHash([0x42; 20]));

        let mut store = InMemoryStore::new();
        wal.replay(&mut store).unwrap();

        let got = store.get_chunk(&hash).unwrap().unwrap();
        assert_eq!(got, data);
        assert_eq!(store.root_get().unwrap(), Some(ChunkHash([0x42; 20])));
    }

    #[test]
    fn store_wal_rejects_truncated_chunk() {
        let mut wal = WalState::new();
        wal.entries.push(WalEntry {
            tag: TAG_CHUNK,
            data: vec![TAG_CHUNK, 0x01],
        });
        let mut store = InMemoryStore::new();
        assert!(wal.replay(&mut store).is_err());
    }

    #[test]
    fn store_wal_rejects_truncated_root() {
        let mut wal = WalState::new();
        wal.entries.push(WalEntry {
            tag: TAG_ROOT,
            data: vec![TAG_ROOT],
        });
        let mut store = InMemoryStore::new();
        assert!(wal.replay(&mut store).is_err());
    }

    #[test]
    fn store_wal_rejects_unknown_tag() {
        let mut wal = WalState::new();
        wal.entries.push(WalEntry {
            tag: 0xFF,
            data: vec![0xFF; 30],
        });
        let mut store = InMemoryStore::new();
        assert!(wal.replay(&mut store).is_err());
    }

    #[test]
    fn store_wal_rejects_tag_data_mismatch() {
        let mut wal = WalState::new();
        wal.entries.push(WalEntry {
            tag: TAG_CHUNK,
            data: vec![TAG_ROOT; 30],
        });
        let mut store = InMemoryStore::new();
        assert!(matches!(
            wal.replay(&mut store),
            Err(StoreError::CorruptWal)
        ));
        assert!(store.root_get().unwrap().is_none());

        let mut wal = WalState::new();
        wal.entries.push(WalEntry {
            tag: TAG_ROOT,
            data: vec![TAG_CHUNK; 30],
        });
        let mut store = InMemoryStore::new();
        assert!(matches!(
            wal.replay(&mut store),
            Err(StoreError::CorruptWal)
        ));
        assert!(store.root_get().unwrap().is_none());
    }

    #[test]
    fn store_wal_corrupt_leaves_store_untouched() {
        let mut wal = WalState::new();
        let data = b"good data".to_vec();
        let hash = crate::chunk::blake3_chunk_hash(&data);
        wal.append_chunk(hash, data);
        wal.entries.push(WalEntry {
            tag: 0xFF,
            data: vec![0xFF; 30],
        });

        let mut store = InMemoryStore::new();
        assert!(wal.replay(&mut store).is_err());
        // store should not have the good chunk — replay was atomic
        assert!(store.get_chunk(&hash).unwrap().is_none());
    }

    #[test]
    fn store_chunk_index_roundtrip() {
        let mut idx = ChunkIndex::new();
        let h = ChunkHash([0x01; 20]);
        idx.insert(h, 42);
        assert_eq!(idx.get(&h), Some(42));
        assert!(idx.delete(&h));
        assert_eq!(idx.get(&h), None);
    }

    #[test]
    fn store_chunk_staging_commit() {
        let mut staging = ChunkStaging::new();
        let h = ChunkHash([0x03; 20]);
        staging.stage(h, vec![1, 2, 3]);
        let committed = staging.commit();
        assert!(committed.contains_key(&h));
        assert!(staging.staged.is_empty());
    }

    #[test]
    fn store_chunk_staging_drop() {
        let mut staging = ChunkStaging::new();
        let h = ChunkHash([0x04; 20]);
        staging.stage(h, vec![1, 2, 3]);
        staging.drop();
        assert!(staging.staged.is_empty());
    }
}
