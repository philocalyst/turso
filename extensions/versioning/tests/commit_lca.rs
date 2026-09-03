//! Commit-graph integration coverage: V2 codec, LCA, ancestor walks (C1–C3).

use turso_versioning::commit::{
    ancestors, decode_v2, encode_v2, hash_commit, is_ancestor, merge_base, resolve_revision,
    Commit, CommitMeta, CommitStore, MemCommitStore,
};
use turso_versioning::model::{CommitId, RootHash, VersionError};
use turso_versioning::refs::{MemRefStore, RefName, RefStore, Revision};

fn make_commit(parents: Vec<CommitId>, msg: &str, ts: i64) -> Commit {
    Commit {
        parents,
        root: RootHash([0u8; 20]),
        meta: CommitMeta {
            name: "test".into(),
            email: "test@test.com".into(),
            message: msg.into(),
            timestamp: ts,
        },
    }
}

#[test]
fn v2_roundtrip_preserves_all_fields() {
    let mut store = MemCommitStore::new();
    let c0 = make_commit(vec![], "initial", 1);
    let id0 = store.put_commit(c0);
    let c1 = make_commit(vec![id0], "second", 2);
    let encoded = encode_v2(&c1);
    let id1 = store.put_commit(c1.clone());

    let decoded = decode_v2(&encoded).unwrap();
    assert_eq!(decoded, c1);
    assert_eq!(decoded.parents, vec![id0]);
    assert_eq!(decoded.meta.timestamp, 2);
    assert_eq!(hash_commit(&decoded), id1);
}

#[test]
fn corrupt_bytes_yield_invalid_commit_encoding() {
    assert_eq!(
        decode_v2(&[]).unwrap_err(),
        VersionError::InvalidCommitEncoding
    );
    // Wrong version byte.
    assert_eq!(
        decode_v2(&[1, 0]).unwrap_err(),
        VersionError::InvalidCommitEncoding
    );
    // Truncated parent id (version + count + 4 bytes of a 20-byte id).
    assert_eq!(
        decode_v2(&[2, 1, 0xAB, 0xCD, 0xAB, 0xCD]).unwrap_err(),
        VersionError::InvalidCommitEncoding
    );
    // Valid payload with a dangling trailing byte.
    let commit = make_commit(vec![], "x", 1);
    let mut buf = encode_v2(&commit);
    buf.push(0x00);
    assert_eq!(
        decode_v2(&buf).unwrap_err(),
        VersionError::InvalidCommitEncoding
    );
}

#[test]
fn invalid_utf8_in_metadata_is_invalid_encoding() {
    let commit = make_commit(vec![], "x", 1);
    let mut buf = encode_v2(&commit);
    // First name byte sits after version(1) + count(1) + root(20) + len(4).
    buf[1 + 1 + 20 + 4] = 0xFF;
    assert_eq!(
        decode_v2(&buf).unwrap_err(),
        VersionError::InvalidCommitEncoding
    );
}

#[test]
fn diamond_lca() {
    //     c0
    //    /  \
    //   c1  c2
    //    \  /
    //     c3
    let mut store = MemCommitStore::new();
    let id0 = store.put_commit(make_commit(vec![], "c0", 0));
    let id1 = store.put_commit(make_commit(vec![id0], "c1", 1));
    let id2 = store.put_commit(make_commit(vec![id0], "c2", 2));
    let id3 = store.put_commit(make_commit(vec![id1, id2], "c3", 3));

    assert_eq!(merge_base(&store, id1, id2).unwrap(), Some(id0));
    assert_eq!(merge_base(&store, id1, id3).unwrap(), Some(id1));
    // id2 is a direct parent of id3, so it is its own LCA.
    assert_eq!(merge_base(&store, id2, id3).unwrap(), Some(id2));
}

#[test]
fn criss_cross_lca() {
    //     c0
    //    /  \
    //   c1  c2
    //   |\  /|
    //   | \/ |
    //   | /\ |
    //   c3  c4   (both merge c1 and c2)
    //    \  /
    //     c5
    let mut store = MemCommitStore::new();
    let id0 = store.put_commit(make_commit(vec![], "c0", 0));
    let id1 = store.put_commit(make_commit(vec![id0], "c1", 1));
    let id2 = store.put_commit(make_commit(vec![id0], "c2", 2));
    let id3 = store.put_commit(make_commit(vec![id1, id2], "c3", 3));
    let id4 = store.put_commit(make_commit(vec![id1, id2], "c4", 4));
    let id5 = store.put_commit(make_commit(vec![id3, id4], "c5", 5));

    // c3 and c4 share c0, c1, and c2; the deepest common ancestors are c1
    // and c2 (both depth 1). Either is a valid LCA.
    let base = merge_base(&store, id3, id4).unwrap().unwrap();
    assert!(base == id1 || base == id2, "unexpected LCA {base:?}");
    assert_eq!(merge_base(&store, id1, id4).unwrap(), Some(id1));
    assert_eq!(merge_base(&store, id3, id5).unwrap(), Some(id3));
    assert_eq!(merge_base(&store, id4, id5).unwrap(), Some(id4));
}

#[test]
fn unrelated_histories_have_no_merge_base() {
    let mut store = MemCommitStore::new();
    let id0 = store.put_commit(make_commit(vec![], "c0", 0));
    let id1 = store.put_commit(make_commit(vec![], "c1", 1));
    assert_eq!(merge_base(&store, id0, id1).unwrap(), None);
}

#[test]
fn ancestor_walk_is_first_parent_chain() {
    let mut store = MemCommitStore::new();
    let id0 = store.put_commit(make_commit(vec![], "c0", 0));
    let id1 = store.put_commit(make_commit(vec![id0], "c1", 1));
    let id2 = store.put_commit(make_commit(vec![id1], "c2", 2));

    assert_eq!(ancestors(&store, id2).unwrap(), vec![id2, id1, id0]);
    assert!(is_ancestor(&store, id0, id2).unwrap());
    assert!(!is_ancestor(&store, id2, id0).unwrap());
}

#[test]
fn missing_commit_surfaces_commit_not_found() {
    let store = MemCommitStore::new();
    let missing = CommitId([0xDE; 20]);
    let err = ancestors(&store, missing).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("commit not found: {}", missing.to_hex())
    );
    assert_eq!(merge_base(&store, missing, missing).unwrap_err(), err);
}

#[test]
fn resolve_tilde_and_symmetric_difference() {
    let mut store = MemCommitStore::new();
    let mut refs = MemRefStore::new();
    let id0 = store.put_commit(make_commit(vec![], "c0", 0));
    let id1 = store.put_commit(make_commit(vec![id0], "c1", 1));
    refs.set(&RefName::branch("main"), id1);

    let tilde1 = resolve_revision(
        &store,
        &refs,
        &Revision::Ancestor(Box::new(Revision::Branch("main".into())), 1),
        Some(id1),
        None,
        None,
    )
    .unwrap();
    assert_eq!(tilde1, id0);

    let sym = resolve_revision(
        &store,
        &refs,
        &Revision::SymmetricDifference(
            Box::new(Revision::Branch("main".into())),
            Box::new(Revision::Hash(id1.to_hex())),
        ),
        Some(id1),
        None,
        None,
    )
    .unwrap();
    assert_eq!(sym, id1);
}
