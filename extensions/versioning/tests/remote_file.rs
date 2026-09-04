use std::cell::RefCell;

use turso_versioning::remote_transport::{FileTransport, RemoteTransport};
use turso_versioning::remote_wire::{RemoteRefs, SnapshotId, SourceId};
use turso_versioning::vtab_log::{VcRow, VcValue};
use turso_versioning::{CommitId, VcStore, VersionError, VersionResult};

#[test]
fn file_remote_push_fetch_clone_and_corruption_are_safe() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("file://{}", directory.path().display());
    let mut source = seeded_store();
    source.remote_add("origin", &url).unwrap();
    source.push("origin", "main", false).unwrap();
    let tip = source.head_commit().unwrap();

    let transport = FileTransport::open(directory.path().to_path_buf());
    assert_eq!(transport.find_branch("main").unwrap(), Some(tip));
    for id in transport.walk(tip).unwrap() {
        assert!(directory.path().join("objects").join(id.to_hex()).is_file());
    }

    let mut clone = VcStore::new("main");
    clone.clone_remote(&url, false).unwrap();
    assert_eq!(clone.head_commit(), Some(tip));

    let object_path = directory
        .path()
        .join("objects")
        .join(SourceId::Commit(tip).to_hex());
    let mut bytes = std::fs::read(&object_path).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&object_path, bytes).unwrap();

    let mut receiver = VcStore::new("main");
    receiver.remote_add("origin", &url).unwrap();
    let error = receiver.fetch("origin", "main").unwrap_err();
    assert_eq!(error, VersionError::ChunkVerificationFailed(tip.to_hex()));
    assert!(receiver.commit_entries().is_empty());
    assert!(receiver.tracking_refs().is_empty());
}

#[test]
fn push_writes_objects_before_moving_the_ref() {
    let source = seeded_store();
    let transport = OrderingTransport::default();
    source.push_to_transport(&transport, "main", false).unwrap();
    let events = transport.events.borrow();
    assert_eq!(events.last().map(String::as_str), Some("ref"));
    assert!(events[..events.len() - 1]
        .iter()
        .all(|event| event == "objects"));
}

#[derive(Default)]
struct OrderingTransport {
    events: RefCell<Vec<String>>,
}

impl RemoteTransport for OrderingTransport {
    fn get_refs(&self) -> VersionResult<Vec<u8>> {
        Ok(Vec::new())
    }

    fn default_branch(&self) -> VersionResult<String> {
        Ok("main".to_string())
    }

    fn find_branch(&self, _branch: &str) -> VersionResult<Option<CommitId>> {
        Ok(None)
    }

    fn walk(&self, _tip: CommitId) -> VersionResult<Vec<SourceId>> {
        Ok(Vec::new())
    }

    fn has_many(&self, ids: &[SourceId]) -> VersionResult<Vec<bool>> {
        Ok(vec![false; ids.len()])
    }

    fn get_batch(&self, _ids: &[SourceId]) -> VersionResult<Vec<(SourceId, Vec<u8>)>> {
        Ok(Vec::new())
    }

    fn put_batch(
        &self,
        _objects: &[(SourceId, Vec<u8>)],
        _snap_lists: &[(CommitId, Vec<SnapshotId>)],
    ) -> VersionResult<()> {
        self.events.borrow_mut().push("objects".to_string());
        Ok(())
    }

    fn update_branch(&self, _branch: &str, _commit: CommitId, _force: bool) -> VersionResult<()> {
        self.events.borrow_mut().push("ref".to_string());
        Ok(())
    }

    fn replace_refs(&self, _refs: &RemoteRefs) -> VersionResult<()> {
        Ok(())
    }
}

fn seeded_store() -> VcStore {
    let mut store = VcStore::new("main");
    store.config_set("user.name", "Ada");
    store.config_set("user.email", "ada@example.com");
    store.apply_work(
        "items",
        vec!["id".to_string(), "value".to_string()],
        vec!["id".to_string()],
        vec![VcRow::new(vec![
            VcValue::Integer(1),
            VcValue::Text("one".to_string()),
        ])],
        "CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT)".to_string(),
    );
    store.track_table("items");
    store.dolt_add(&["items"]).unwrap();
    let tip = store.dolt_commit("seed", None, false, false).unwrap();
    store.record_snapshot(
        tip,
        "items",
        vec!["id".to_string(), "value".to_string()],
        vec!["id".to_string()],
        store.work_table("items").unwrap().rows.clone(),
        "CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT)".to_string(),
    );
    assert_eq!(store.branch_tip("main"), Some(tip));
    store
}
