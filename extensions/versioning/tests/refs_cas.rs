//! Ref-store integration coverage: CAS races, branch lifecycle, revision
//! parsing (R1, R5, R6).

use turso_versioning::model::CommitId;
use turso_versioning::refs::{
    parse_qualified, parse_qualified_path, parse_revision, MemRefStore, RefError, RefName, RefNs,
    RefStore, Revision,
};

#[test]
fn cas_race_returns_busy() {
    let mut store = MemRefStore::new();
    let name = RefName::branch("main");
    let c1 = CommitId([1u8; 20]);
    let c2 = CommitId([2u8; 20]);
    let c3 = CommitId([3u8; 20]);

    assert_eq!(store.compare_and_swap(&name, None, c1), Ok(()));
    // Stale expected tip must not clobber the winner.
    assert_eq!(store.compare_and_swap(&name, None, c2), Err(RefError::Busy));
    assert_eq!(store.get(&name), Some(c1));
    assert_eq!(store.compare_and_swap(&name, Some(c1), c3), Ok(()));
    assert_eq!(store.get(&name), Some(c3));
}

#[test]
fn branch_lifecycle_create_list_delete() {
    let mut store = MemRefStore::new();
    let main = RefName::branch("main");
    let dev = RefName::branch("dev");
    store.set(&main, CommitId([1u8; 20]));
    store.set(&dev, CommitId([2u8; 20]));

    let branches = store.list(RefNs::Heads);
    assert_eq!(
        branches
            .iter()
            .map(|(n, _)| n.name.as_str())
            .collect::<Vec<_>>(),
        vec!["dev", "main"]
    );

    assert!(store.delete(&dev));
    assert_eq!(store.get(&dev), None);
    assert!(!store.delete(&dev));
}

#[test]
fn rubric_aliases_create_get_list_delete() {
    let mut store = MemRefStore::new();
    let c1 = CommitId([1u8; 20]);
    store.create_branch("main", c1);
    assert_eq!(store.get_branch("main"), Some(c1));
    assert_eq!(store.list_branches().len(), 1);
    assert!(store.delete_branch("main"));
    assert!(!store.delete_branch("main"));
}

#[test]
fn parse_revision_ancestor_and_symmetric() {
    assert_eq!(
        parse_revision("main~2").unwrap(),
        Revision::Ancestor(Box::new(Revision::Branch("main".into())), 2)
    );
    assert_eq!(
        parse_revision("a...b").unwrap(),
        Revision::SymmetricDifference(
            Box::new(Revision::Branch("a".into())),
            Box::new(Revision::Branch("b".into())),
        )
    );
    assert_eq!(
        parse_revision("a..b").unwrap(),
        Revision::Range(
            Box::new(Revision::Branch("a".into())),
            Box::new(Revision::Branch("b".into())),
        )
    );
}

#[test]
fn parse_revision_stacked_specs() {
    assert_eq!(
        parse_revision("main~1^2").unwrap(),
        Revision::Parent(
            Box::new(Revision::Ancestor(
                Box::new(Revision::Branch("main".into())),
                1
            )),
            2
        )
    );
    assert_eq!(
        parse_revision("main^2~1").unwrap(),
        Revision::Ancestor(
            Box::new(Revision::Parent(
                Box::new(Revision::Branch("main".into())),
                2
            )),
            1
        )
    );
}

#[test]
fn parse_revision_bad_spec_errors() {
    assert_eq!(
        parse_revision("HEAD~x").unwrap_err().to_string(),
        "invalid revision spec: 'HEAD~x'"
    );
    assert_eq!(
        parse_revision("").unwrap_err().to_string(),
        "invalid revision spec: ''"
    );
    assert_eq!(
        parse_revision("main^foo").unwrap_err().to_string(),
        "invalid revision spec: 'main^foo'"
    );
    // Empty range endpoints are rejected, not treated as branches.
    assert_eq!(
        parse_revision("a..").unwrap_err().to_string(),
        "invalid revision spec: ''"
    );
}

#[test]
fn parse_qualified_path_last_segment_is_revision() {
    assert_eq!(
        parse_qualified_path("my.db@dev").unwrap(),
        ("my.db".to_string(), Revision::Branch("dev".into()))
    );
    assert_eq!(
        parse_qualified_path("my.db/dev~1").unwrap(),
        (
            "my.db".to_string(),
            Revision::Ancestor(Box::new(Revision::Branch("dev".into())), 1)
        )
    );
    assert_eq!(
        parse_qualified_path("my.db/archive.db").unwrap(),
        ("my.db/archive.db".to_string(), Revision::Head)
    );
    assert_eq!(
        parse_qualified("my.db@dev").unwrap(),
        parse_qualified_path("my.db@dev").unwrap()
    );
}
