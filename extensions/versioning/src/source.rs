//! Lazy chunk sources: fetch-on-miss for objects a lazy clone did not copy
//! (doltlite #2551/#2552).
//!
//! A source is consulted only for ids the store lacks; every returned
//! object is verified against its id before it is stored (B4), one
//! `get_many` batch is one atomic insert set, and a source failure names
//! the requested hash and unwinds without poisoning the store.

use crate::model::{CommitId, VersionError, VersionResult};
use crate::refs::RefStore;
use crate::remote_wire::{SourceId, SYNC_BATCH_SIZE};
use crate::staging::VcStore;

/// What a lazy store can ask a source for: one object or a batch. Sources
/// are the seam where hosts plug their own backing store. No `Send`/`Sync`
/// bound: a source lives inside `VcStore`, which is only reachable behind
/// the owning connection's lock.
pub trait ChunkSource {
    /// Fetch one object by id. Errors name the id.
    fn get(&self, id: SourceId) -> VersionResult<Vec<u8>>;
    /// Fetch a batch; `None` entries mean "not present" and surface as
    /// `chunk not found` errors naming the id.
    fn get_many(&self, ids: &[SourceId]) -> VersionResult<Vec<Option<Vec<u8>>>>;
}

/// A source backed by the origin remote a lazy clone recorded.
pub struct OriginSource {
    url: String,
}

impl OriginSource {
    pub fn new(url: &str) -> Self {
        OriginSource {
            url: url.to_string(),
        }
    }
}

impl ChunkSource for OriginSource {
    fn get(&self, id: SourceId) -> VersionResult<Vec<u8>> {
        let transport = crate::remote_transport::open_transport(&self.url)?;
        let mut objects = transport.get_batch(&[id])?;
        Ok(objects.remove(0).1)
    }

    fn get_many(&self, ids: &[SourceId]) -> VersionResult<Vec<Option<Vec<u8>>>> {
        let transport = crate::remote_transport::open_transport(&self.url)?;
        let objects = transport.get_batch(ids)?;
        Ok(objects.into_iter().map(|(_, bytes)| Some(bytes)).collect())
    }
}

impl VcStore {
    /// Bring one tip's catalog closure into the store through `source`,
    /// verifying every object first. The second hydrate of the same tip
    /// makes no source calls.
    pub fn hydrate_with(&mut self, source: &dyn ChunkSource, tip: CommitId) -> VersionResult<()> {
        let missing = self.missing_for(tip)?;
        for batch in missing.chunks(SYNC_BATCH_SIZE) {
            let fetched = source.get_many(batch)?;
            // Verify and decode the whole batch before storing any of it:
            // one batch is one atomic insert set.
            let mut staged = Vec::with_capacity(batch.len());
            for (id, bytes) in batch.iter().zip(fetched) {
                let Some(bytes) = bytes else {
                    return Err(VersionError::ChunkNotFound(id.to_hex()));
                };
                crate::remote_wire::verify_object(id, &bytes)?;
                staged.push((*id, bytes));
            }
            for (id, bytes) in staged {
                ingest(self, &id, &bytes)?;
            }
        }
        Ok(())
    }

    /// Hydrate through the recorded origin, if this store is lazy.
    pub fn lazy_hydrate(&mut self, tip: CommitId) -> VersionResult<()> {
        let Some(url) = self.lazy_origin.clone() else {
            return Ok(());
        };
        let source = OriginSource::new(&url);
        self.hydrate_with(&source, tip)
    }

    /// Hydrate every ref and tracking tip, then stop being lazy: the store
    /// holds everything it can name locally.
    pub fn materialize(&mut self) -> VersionResult<()> {
        if self.lazy_origin.is_none() {
            return Ok(());
        }
        let tips: Vec<CommitId> = self
            .refs
            .list(crate::refs::RefNs::Heads)
            .into_iter()
            .map(|(_, id)| id)
            .chain(self.tracking.values().copied())
            .collect();
        for tip in tips {
            self.lazy_hydrate(tip)?;
        }
        self.lazy_origin = None;
        Ok(())
    }

    /// The ids `hydrate_with` would need for `tip`: the catalog closure's
    /// objects this store still lacks.
    fn missing_for(&self, tip: CommitId) -> VersionResult<Vec<SourceId>> {
        let needed: Vec<SourceId> = match self.lazy_catalog.get(&tip) {
            Some(ids) => ids
                .iter()
                .copied()
                .filter(|id| !self.has_object_public(id))
                .collect(),
            None => vec![SourceId::Commit(tip)],
        };
        Ok(needed)
    }

    /// Record the object closure behind a lazy tip (the ids a remote walk
    /// returned; bytes stay remote until a read needs them).
    pub(crate) fn note_lazy_catalog(&mut self, tip: CommitId, ids: Vec<SourceId>) {
        self.lazy_catalog.insert(tip, ids);
    }

    /// Test/store-internal presence check by object id.
    pub(crate) fn has_object_public(&self, id: &SourceId) -> bool {
        match id {
            SourceId::Commit(commit) => self.get_commit(commit).is_some(),
            SourceId::Snapshot(snap) => self.snap_index.contains_key(snap),
        }
    }

    /// Store a verified object (commits land in the commit map, snapshot
    /// records under their owner commit and table).
    pub(crate) fn store_remote_object_public(
        &mut self,
        id: &SourceId,
        bytes: &[u8],
    ) -> VersionResult<()> {
        crate::remote::store_remote_object_on(self, id, bytes)
    }
}

/// Verify then store one source object. Private to keep the verify-before-
/// persist rule in one place.
fn ingest(store: &mut VcStore, id: &SourceId, bytes: &[u8]) -> VersionResult<()> {
    crate::remote_wire::verify_object(id, bytes)?;
    store.store_remote_object_public(id, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A counting fake source: serves programmed bytes, counts calls.
    struct Fake {
        objects: RefCell<std::collections::HashMap<SourceId, Vec<u8>>>,
        gets: std::cell::Cell<usize>,
        batches: std::cell::Cell<usize>,
        fail_batches: std::cell::Cell<bool>,
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                objects: RefCell::new(std::collections::HashMap::new()),
                gets: std::cell::Cell::new(0),
                batches: std::cell::Cell::new(0),
                fail_batches: std::cell::Cell::new(false),
            }
        }
    }

    impl ChunkSource for Fake {
        fn get(&self, id: SourceId) -> VersionResult<Vec<u8>> {
            self.gets.set(self.gets.get() + 1);
            self.objects
                .borrow()
                .get(&id)
                .cloned()
                .ok_or_else(|| VersionError::ChunkNotFound(id.to_hex()))
        }

        fn get_many(&self, ids: &[SourceId]) -> VersionResult<Vec<Option<Vec<u8>>>> {
            self.batches.set(self.batches.get() + 1);
            if self.fail_batches.get() {
                return Err(VersionError::FetchFailed);
            }
            let objects = self.objects.borrow();
            Ok(ids.iter().map(|id| objects.get(id).cloned()).collect())
        }
    }

    fn commit_with_snapshot(tag: u8, tables: usize) -> (VcStore, CommitId) {
        let mut store = VcStore::new("main");
        store.config_set("user.name", "A");
        store.config_set("user.email", "a@b");
        store.set_now(100 + tag as i64);
        let mut names = Vec::new();
        for t in 0..tables {
            let name = format!("t{t}");
            store.apply_work(
                &name,
                vec!["id".to_string()],
                vec!["id".to_string()],
                vec![crate::vtab_log::VcRow::new(vec![
                    crate::vtab_log::VcValue::Integer(t as i64),
                ])],
                String::new(),
            );
            store.track_table(&name);
            names.push(name);
        }
        for name in &names {
            store.dolt_add(&[name]).unwrap();
        }
        let tip = store.dolt_commit("m", None, false, false).unwrap();
        for name in &names {
            store.record_snapshot(
                tip,
                name,
                vec!["id".to_string()],
                vec!["id".to_string()],
                store.work_table(name).unwrap().rows.clone(),
                String::new(),
            );
        }
        (store, tip)
    }

    /// Program a source with the store's objects and record the id
    /// catalog, so a lazy hydrate knows what to ask for.
    fn seed_source(store: &VcStore, tip: CommitId, source: &Fake) -> Vec<SourceId> {
        let mut objects = source.objects.borrow_mut();
        let mut catalog = vec![SourceId::Commit(tip)];
        if let Some(commit) = store.get_commit(&tip) {
            objects.insert(SourceId::Commit(tip), crate::commit::encode_v2(&commit));
        }
        if let Some(tables) = store.snapshots.get(&tip) {
            for (table, snap) in tables {
                let (id, bytes) = crate::remote_wire::encode_snapshot_record(&tip, table, snap);
                objects.insert(SourceId::Snapshot(id), bytes);
                catalog.push(SourceId::Snapshot(id));
            }
        }
        catalog
    }

    #[test]
    fn source_hydrate_fetches_missing_and_verifies() {
        let (owner, tip) = commit_with_snapshot(1, 2);
        let fake = Fake::new();
        let catalog = seed_source(&owner, tip, &fake);

        let mut lazy = VcStore::new("main");
        lazy.refs.set(&crate::refs::RefName::branch("main"), tip);
        lazy.branches.insert("main".to_string());
        lazy.note_lazy_catalog(tip, catalog);
        lazy.hydrate_with(&fake, tip).unwrap();
        assert!(lazy.get_commit(&tip).is_some());
        assert_eq!(lazy.snapshots.get(&tip).map(|t| t.len()), Some(2));
    }

    #[test]
    fn source_second_hydrate_makes_no_calls() {
        let (owner, tip) = commit_with_snapshot(2, 1);
        let fake = Fake::new();
        let catalog = seed_source(&owner, tip, &fake);
        let mut lazy = VcStore::new("main");
        lazy.note_lazy_catalog(tip, catalog);
        lazy.hydrate_with(&fake, tip).unwrap();
        fake.batches.set(0);
        fake.gets.set(0);
        lazy.hydrate_with(&fake, tip).unwrap();
        assert_eq!(fake.batches.get(), 0);
        assert_eq!(fake.gets.get(), 0);
    }

    #[test]
    fn source_one_commit_uses_one_batch() {
        let (owner, tip) = commit_with_snapshot(3, 3);
        let fake = Fake::new();
        let catalog = seed_source(&owner, tip, &fake);
        let mut lazy = VcStore::new("main");
        lazy.note_lazy_catalog(tip, catalog);
        lazy.hydrate_with(&fake, tip).unwrap();
        assert_eq!(fake.batches.get(), 1);
    }

    #[test]
    fn source_missing_object_names_hash() {
        let fake = Fake::new();
        let tip = CommitId([0x5A; 20]);
        let mut store = VcStore::new("main");
        // Program only a harmless snapshot so the walk reaches the missing
        // commit and reports it by hash.
        let err = store.hydrate_with(&fake, tip).unwrap_err();
        assert_eq!(err, VersionError::ChunkNotFound(tip.to_hex()));
    }

    #[test]
    fn source_corrupt_bytes_rejected_and_not_stored() {
        let (owner, tip) = commit_with_snapshot(4, 1);
        let fake = Fake::new();
        let catalog = seed_source(&owner, tip, &fake);
        // Corrupt the commit bytes under the same id.
        {
            let mut objects = fake.objects.borrow_mut();
            let entry = objects.get_mut(&SourceId::Commit(tip)).unwrap();
            let last = entry.len() - 1;
            entry[last] ^= 0xFF;
        }
        let mut lazy = VcStore::new("main");
        lazy.note_lazy_catalog(tip, catalog);
        let err = lazy.hydrate_with(&fake, tip).unwrap_err();
        assert_eq!(err, VersionError::ChunkVerificationFailed(tip.to_hex()));
        assert!(lazy.get_commit(&tip).is_none());
    }

    #[test]
    fn source_failed_batch_stores_nothing() {
        let (owner, tip) = commit_with_snapshot(5, 2);
        let fake = Fake::new();
        let catalog = seed_source(&owner, tip, &fake);
        fake.fail_batches.set(true);
        let mut lazy = VcStore::new("main");
        lazy.note_lazy_catalog(tip, catalog);
        assert!(lazy.hydrate_with(&fake, tip).is_err());
        assert!(lazy.get_commit(&tip).is_none());
    }

    #[test]
    fn source_materialize_clears_lazy_origin() {
        let (owner, tip) = commit_with_snapshot(6, 1);
        // Serve through a mem endpoint so OriginSource can reach it.
        let transport = crate::remote_transport::MemTransport::open("src-mat-1", false).unwrap();
        let mut pusher = owner;
        pusher.remote_add("origin", "mem://src-mat-1").unwrap();
        pusher.push("origin", "main", false).unwrap();

        let mut lazy = VcStore::new("main");
        lazy.clone_remote("mem://src-mat-1", true).unwrap();
        assert_eq!(lazy.lazy_origin(), Some("mem://src-mat-1"));
        assert!(lazy.get_commit(&tip).is_none());
        lazy.materialize().unwrap();
        assert!(lazy.get_commit(&tip).is_some());
        assert_eq!(lazy.lazy_origin(), None);
    }

    #[test]
    fn source_lazy_hydrate_uses_origin() {
        let (owner, tip) = commit_with_snapshot(7, 1);
        let mut pusher = owner;
        pusher.remote_add("origin", "mem://src-lazy-1").unwrap();
        pusher.push("origin", "main", false).unwrap();
        let mut lazy = VcStore::new("main");
        lazy.clone_remote("mem://src-lazy-1", true).unwrap();
        lazy.lazy_hydrate(tip).unwrap();
        assert!(lazy.get_commit(&tip).is_some());
    }

    #[test]
    fn source_batch_cap_respected() {
        assert_eq!(SYNC_BATCH_SIZE, 256);
    }
}
