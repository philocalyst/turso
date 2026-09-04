//! Canonical wire encoding for remote sync: snapshot records, content ids,
//! the remote refs blob, and the 256-id batch protocol.
//!
//! Every object that crosses a remote boundary is content-addressed and
//! byte-canonical, so both sides agree on ids without trusting each other
//! (B4: verify before persist).

use crate::commit::{decode_v2, hash_commit, CommitStore};
use crate::model::{CommitId, VersionError, VersionResult};
use crate::staging::TableSnapshot;
use crate::vtab_log::{VcRow, VcValue};

/// doltlite `SYNC_BATCH_SIZE`: one has-check or transfer round carries at
/// most this many ids.
pub const SYNC_BATCH_SIZE: usize = 256;

const SNAPSHOT_MAGIC: &[u8; 4] = b"SNAP";
const SNAPSHOT_VERSION: u8 = 1;
const REFS_VERSION: u8 = 1;

/// Content id of one snapshot record: the owner commit, table name, and
/// canonical snapshot bytes. Distinct newtype for the same discipline as
/// `CommitId`/`ChunkHash`: an id names what kind of thing it addresses.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SnapshotId(pub [u8; 20]);

impl SnapshotId {
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl std::fmt::Debug for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SnapshotId({})", self.to_hex())
    }
}

/// What a remote can be asked for: a commit object or a snapshot record.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub enum SourceId {
    Commit(CommitId),
    Snapshot(SnapshotId),
}

impl SourceId {
    pub fn to_hex(&self) -> String {
        match self {
            SourceId::Commit(id) => id.to_hex(),
            SourceId::Snapshot(id) => id.to_hex(),
        }
    }
}

/// One remote-side branch table: default branch name plus branch tips.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteRefs {
    pub default_branch: String,
    pub branches: Vec<(String, CommitId)>,
}

pub fn encode_snapshot_record(
    owner: &CommitId,
    table: &str,
    snap: &TableSnapshot,
) -> (SnapshotId, Vec<u8>) {
    let mut buf = Vec::with_capacity(96);
    buf.extend_from_slice(owner.as_bytes());
    let table_bytes = table.as_bytes();
    buf.extend_from_slice(&(table_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(table_bytes);
    encode_snapshot_body(&mut buf, snap);
    let id = SnapshotId(crate::chunk::blake3_chunk_hash(&buf).0);
    (id, buf)
}

pub fn decode_snapshot_record(bytes: &[u8]) -> VersionResult<(CommitId, String, TableSnapshot)> {
    let invalid = || VersionError::InvalidSnapshotEncoding;
    let mut reader = Reader::new(bytes);
    let Some(owner_bytes) = reader.take(20) else {
        return Err(invalid());
    };
    let mut owner = [0u8; 20];
    owner.copy_from_slice(owner_bytes);
    let Some(table) = reader.short_str() else {
        return Err(invalid());
    };
    let snap = decode_snapshot_body(reader)?;
    Ok((CommitId(owner), table, snap))
}

/// Bounds-checked cursor over a byte slice; every decode failure is the
/// same corruption error so callers never see a panic.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.pos + n > self.bytes.len() {
            return None;
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Some(slice)
    }

    fn short_str(&mut self) -> Option<String> {
        let len = u16::from_le_bytes(self.take(2)?.try_into().ok()?) as usize;
        let slice = self.take(len)?;
        String::from_utf8(slice.to_vec()).ok()
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

/// The snapshot body: canonical columns/pk/schema/rows. Rows with a primary
/// key are sorted by their encoded pk cells, so physical insert order and
/// rowid churn never change a snapshot's identity.
fn encode_snapshot_body(buf: &mut Vec<u8>, snap: &TableSnapshot) {
    buf.extend_from_slice(SNAPSHOT_MAGIC);
    buf.push(SNAPSHOT_VERSION);

    buf.extend_from_slice(&(snap.columns.len() as u16).to_le_bytes());
    for col in &snap.columns {
        let bytes = col.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(bytes);
    }

    buf.extend_from_slice(&(snap.pk.len() as u16).to_le_bytes());
    for col in &snap.pk {
        let bytes = col.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(bytes);
    }

    let schema = snap.schema_sql.as_bytes();
    buf.extend_from_slice(&(schema.len() as u32).to_le_bytes());
    buf.extend_from_slice(schema);

    let mut rows: Vec<&VcRow> = snap.rows.iter().collect();
    if !snap.pk.is_empty() {
        let positions: Vec<usize> = snap
            .pk
            .iter()
            .filter_map(|name| snap.columns.iter().position(|c| c == name))
            .collect();
        if positions.len() == snap.pk.len() {
            rows.sort_by_cached_key(|r| pk_key(&positions, r));
        }
    }

    buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for row in rows {
        buf.extend_from_slice(&(row.values.len() as u16).to_le_bytes());
        for value in &row.values {
            encode_cell(buf, value);
        }
    }
}

/// Sort key for one row: the encoded pk cells in pk order.
fn pk_key(positions: &[usize], row: &VcRow) -> Vec<Vec<u8>> {
    positions
        .iter()
        .map(|&i| {
            let mut cell = Vec::new();
            encode_cell(&mut cell, &row.values[i]);
            cell
        })
        .collect()
}

fn decode_snapshot_body(mut reader: Reader) -> VersionResult<TableSnapshot> {
    let invalid = || VersionError::InvalidSnapshotEncoding;
    if reader.take(4) != Some(SNAPSHOT_MAGIC) {
        return Err(invalid());
    }
    match reader.take(1) {
        Some(&[ver]) if ver == SNAPSHOT_VERSION => {}
        _ => return Err(invalid()),
    }

    let count = reader.u16().ok_or_else(invalid)?;
    let mut columns = Vec::with_capacity(count as usize);
    for _ in 0..count {
        columns.push(reader.short_str().ok_or_else(invalid)?);
    }
    let count = reader.u16().ok_or_else(invalid)?;
    let mut pk = Vec::with_capacity(count as usize);
    for _ in 0..count {
        pk.push(reader.short_str().ok_or_else(invalid)?);
    }
    let schema_len = reader.u32().ok_or_else(invalid)? as usize;
    let schema_slice = reader.take(schema_len).ok_or_else(invalid)?;
    let schema_sql = String::from_utf8(schema_slice.to_vec()).map_err(|_| invalid())?;

    let row_count = reader.u32().ok_or_else(invalid)? as usize;
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        let cell_count = reader.u16().ok_or_else(invalid)? as usize;
        let mut values = Vec::with_capacity(cell_count);
        for _ in 0..cell_count {
            values.push(decode_cell(&mut reader)?);
        }
        rows.push(VcRow::new(values));
    }
    if reader.pos != reader.bytes.len() {
        return Err(invalid());
    }

    Ok(TableSnapshot {
        columns,
        pk,
        rows,
        schema_sql,
    })
}

const CELL_NULL: u8 = 0;
const CELL_INT: u8 = 1;
const CELL_TEXT: u8 = 2;

fn encode_cell(buf: &mut Vec<u8>, value: &VcValue) {
    match value {
        VcValue::Null => buf.push(CELL_NULL),
        VcValue::Integer(i) => {
            buf.push(CELL_INT);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        VcValue::Text(s) => {
            buf.push(CELL_TEXT);
            let bytes = s.as_bytes();
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(bytes);
        }
    }
}

fn decode_cell(reader: &mut Reader) -> VersionResult<VcValue> {
    let invalid = || VersionError::InvalidSnapshotEncoding;
    let Some(tag) = reader.take(1) else {
        return Err(invalid());
    };
    match tag[0] {
        CELL_NULL => Ok(VcValue::Null),
        CELL_INT => {
            let slice = reader.take(8).ok_or_else(invalid)?;
            Ok(VcValue::Integer(i64::from_le_bytes(
                slice.try_into().unwrap(),
            )))
        }
        CELL_TEXT => {
            let len = reader.u32().ok_or_else(invalid)? as usize;
            let slice = reader.take(len).ok_or_else(invalid)?;
            String::from_utf8(slice.to_vec())
                .map(VcValue::Text)
                .map_err(|_| invalid())
        }
        _ => Err(invalid()),
    }
}

pub fn encode_refs(refs: &RemoteRefs) -> Vec<u8> {
    let mut sorted = refs.clone();
    sorted.branches.sort_by(|a, b| a.0.cmp(&b.0));
    sorted.branches.dedup_by(|a, b| a.0 == b.0);

    let mut buf = Vec::with_capacity(32);
    buf.push(REFS_VERSION);
    let def = sorted.default_branch.as_bytes();
    buf.extend_from_slice(&(def.len() as u16).to_le_bytes());
    buf.extend_from_slice(def);
    buf.extend_from_slice(&(sorted.branches.len() as u16).to_le_bytes());
    for (name, commit) in &sorted.branches {
        let bytes = name.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(bytes);
        buf.extend_from_slice(commit.as_bytes());
    }
    buf
}

pub fn decode_refs(bytes: &[u8]) -> VersionResult<RemoteRefs> {
    let invalid = || VersionError::FailedReadRemoteRefs;
    let mut reader = Reader::new(bytes);
    match reader.take(1) {
        Some(&[ver]) if ver == REFS_VERSION => {}
        _ => return Err(invalid()),
    }
    let default_branch = reader.short_str().ok_or_else(invalid)?;
    let count = reader.u16().ok_or_else(invalid)?;
    let mut branches = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = reader.short_str().ok_or_else(invalid)?;
        let slice = reader.take(20).ok_or_else(invalid)?;
        let mut id = [0u8; 20];
        id.copy_from_slice(slice);
        branches.push((name, CommitId(id)));
    }
    if reader.pos != reader.bytes.len() {
        return Err(invalid());
    }
    Ok(RemoteRefs {
        default_branch,
        branches,
    })
}

/// Check that `bytes` really is the object named by `id`. Refuses
/// non-canonical snapshot encodings so a remote cannot smuggle two byte
/// forms under one id (B4).
pub fn verify_object(id: &SourceId, bytes: &[u8]) -> VersionResult<()> {
    match id {
        SourceId::Commit(want) => {
            let commit = decode_v2(bytes)?;
            let got = hash_commit(&commit);
            if &got == want {
                Ok(())
            } else {
                Err(VersionError::ChunkVerificationFailed(want.to_hex()))
            }
        }
        SourceId::Snapshot(want) => {
            let got = SnapshotId(crate::chunk::blake3_chunk_hash(bytes).0);
            if &got != want {
                return Err(VersionError::ChunkVerificationFailed(want.to_hex()));
            }
            // The hash matched; also refuse bytes that are not the canonical
            // encoding so one id always means one byte form.
            let (owner, table, snap) = decode_snapshot_record(bytes)?;
            let (_, canonical) = encode_snapshot_record(&owner, &table, &snap);
            if canonical == bytes {
                Ok(())
            } else {
                Err(VersionError::ChunkVerificationFailed(want.to_hex()))
            }
        }
    }
}

/// Store one verified object into the right place: commits into the commit
/// map, snapshot records under their owner commit and table.
pub fn store_object(
    commits: &mut crate::commit::MemCommitStore,
    snapshots: &mut std::collections::HashMap<
        CommitId,
        std::collections::HashMap<String, TableSnapshot>,
    >,
    id: &SourceId,
    bytes: &[u8],
) -> VersionResult<()> {
    verify_object(id, bytes)?;
    match id {
        SourceId::Commit(_) => {
            let commit = decode_v2(bytes)?;
            commits.put_commit(commit);
            Ok(())
        }
        SourceId::Snapshot(_) => {
            let (owner, table, snap) = decode_snapshot_record(bytes)?;
            snapshots.entry(owner).or_default().insert(table, snap);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::{encode_v2, CommitStore};

    fn snap(columns: &[&str], pk: &[&str], rows: Vec<Vec<VcValue>>) -> TableSnapshot {
        TableSnapshot {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            pk: pk.iter().map(|s| s.to_string()).collect(),
            rows: rows.into_iter().map(|values| VcRow::new(values)).collect(),
            schema_sql: format!(
                "CREATE TABLE t ({})",
                columns
                    .iter()
                    .map(|c| format!("{c} TEXT"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn row(int: i64, text: &str) -> Vec<VcValue> {
        vec![VcValue::Integer(int), VcValue::Text(text.to_string())]
    }

    #[test]
    fn wire_snapshot_record_roundtrip() {
        let owner = CommitId([0x11; 20]);
        let s = snap(&["id", "v"], &["id"], vec![row(1, "a"), row(2, "b")]);
        let (id, bytes) = encode_snapshot_record(&owner, "t", &s);
        let (got_owner, got_table, got_snap) = decode_snapshot_record(&bytes).unwrap();
        assert_eq!(got_owner, owner);
        assert_eq!(got_table, "t");
        assert_eq!(got_snap, s);
        // The id is over the full record, so a different owner differs.
        let (other, _) = encode_snapshot_record(&CommitId([0x22; 20]), "t", &s);
        assert_ne!(id, other);
    }

    #[test]
    fn wire_snapshot_rows_sorted_by_pk() {
        let owner = CommitId([0x33; 20]);
        let mut a = snap(&["id", "v"], &["id"], vec![row(2, "b"), row(1, "a")]);
        let b = snap(&["id", "v"], &["id"], vec![row(1, "a"), row(2, "b")]);
        let (id_a, bytes_a) = encode_snapshot_record(&owner, "t", &a);
        let (id_b, bytes_b) = encode_snapshot_record(&owner, "t", &b);
        assert_eq!(id_a, id_b);
        assert_eq!(bytes_a, bytes_b);
        // Canonical order shows in the decoded rows.
        let (_, _, got) = decode_snapshot_record(&bytes_a).unwrap();
        assert_eq!(got.rows[0].values[0], VcValue::Integer(1));
        a.rows.clear();
        let (id_empty, _) = encode_snapshot_record(&owner, "t", &a);
        assert_ne!(id_empty, id_a);
    }

    #[test]
    fn wire_no_pk_rows_keep_capture_order() {
        let owner = CommitId([0x44; 20]);
        let a = snap(&["a", "b"], &[], vec![row(2, "x"), row(1, "y")]);
        let b = snap(&["a", "b"], &[], vec![row(1, "y"), row(2, "x")]);
        let (id_a, _) = encode_snapshot_record(&owner, "t", &a);
        let (id_b, _) = encode_snapshot_record(&owner, "t", &b);
        // Without a pk there is no canonical order: byte order is capture order.
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn wire_snapshot_record_rejects_truncation() {
        let owner = CommitId([0x55; 20]);
        let s = snap(&["id"], &["id"], vec![row(1, "a")]);
        let (_, bytes) = encode_snapshot_record(&owner, "t", &s);
        for cut in [0usize, 4, 10, bytes.len() - 1] {
            assert!(decode_snapshot_record(&bytes[..cut]).is_err());
        }
    }

    #[test]
    fn wire_refs_roundtrip_and_sorted() {
        let refs = RemoteRefs {
            default_branch: "main".to_string(),
            branches: vec![
                ("feature".to_string(), CommitId([0x02; 20])),
                ("main".to_string(), CommitId([0x01; 20])),
            ],
        };
        let bytes = encode_refs(&refs);
        let got = decode_refs(&bytes).unwrap();
        assert_eq!(got, refs);
        // Encoding sorts branches so identical ref sets encode identically.
        let mut shuffled = refs.clone();
        shuffled.branches.reverse();
        assert_eq!(encode_refs(&shuffled), bytes);
    }

    #[test]
    fn wire_refs_rejects_corruption() {
        let refs = RemoteRefs {
            default_branch: "main".to_string(),
            branches: vec![("main".to_string(), CommitId([0x01; 20]))],
        };
        let bytes = encode_refs(&refs);
        assert!(decode_refs(&[]).is_err());
        assert!(decode_refs(&bytes[..bytes.len() - 1]).is_err());
        // A corrupted length prefix pushes the cursor past the blob.
        let mut bad = bytes.clone();
        bad[1] = 0xFF;
        bad[2] = 0xFF;
        assert!(decode_refs(&bad).is_err());
        let mut ver = bytes.clone();
        ver[0] = 9;
        assert!(decode_refs(&ver).is_err());
    }

    #[test]
    fn wire_verify_object_accepts_commit_and_snapshot() {
        let commit = crate::commit::Commit {
            parents: vec![],
            root: crate::model::RootHash([0u8; 20]),
            meta: crate::commit::CommitMeta {
                name: "a".to_string(),
                email: "a@b".to_string(),
                message: "m".to_string(),
                timestamp: 7,
            },
        };
        let id = hash_commit(&commit);
        let bytes = encode_v2(&commit);
        verify_object(&SourceId::Commit(id), &bytes).unwrap();
        let mut tampered = bytes.clone();
        tampered[3] ^= 0xFF;
        assert_eq!(
            verify_object(&SourceId::Commit(id), &tampered).unwrap_err(),
            VersionError::ChunkVerificationFailed(id.to_hex())
        );

        let owner = CommitId([0x66; 20]);
        let s = snap(&["id"], &["id"], vec![row(1, "a")]);
        let (sid, sbytes) = encode_snapshot_record(&owner, "t", &s);
        verify_object(&SourceId::Snapshot(sid), &sbytes).unwrap();
        let mut bad = sbytes.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        assert_eq!(
            verify_object(&SourceId::Snapshot(sid), &bad).unwrap_err(),
            VersionError::ChunkVerificationFailed(sid.to_hex())
        );
    }

    #[test]
    fn wire_store_object_places_commits_and_snapshots() {
        let mut commits = crate::commit::MemCommitStore::new();
        let mut snapshots = std::collections::HashMap::new();
        let commit = crate::commit::Commit {
            parents: vec![],
            root: crate::model::RootHash([0u8; 20]),
            meta: crate::commit::CommitMeta {
                name: "a".to_string(),
                email: "a@b".to_string(),
                message: "m".to_string(),
                timestamp: 7,
            },
        };
        let cid = commits.put_commit(commit.clone());
        assert_eq!(
            store_object(
                &mut commits,
                &mut snapshots,
                &SourceId::Commit(cid),
                &encode_v2(&commit)
            )
            .unwrap(),
            ()
        );

        let owner = CommitId([0x77; 20]);
        let s = snap(&["id"], &["id"], vec![row(1, "a")]);
        let (sid, sbytes) = encode_snapshot_record(&owner, "t", &s);
        store_object(
            &mut commits,
            &mut snapshots,
            &SourceId::Snapshot(sid),
            &sbytes,
        )
        .unwrap();
        assert!(snapshots[&owner]["t"] == s);
    }

    #[test]
    fn wire_batch_size_matches_doltlite() {
        assert_eq!(SYNC_BATCH_SIZE, 256);
    }
}
