use std::path::Path;

use turso_versioning::model::{ChunkHash, Manifest, StoreError};
use turso_versioning::remote_wire::{decode_snapshot, encode_snapshot, snapshot_id};
use turso_versioning::vtab_log::{VcRow, VcValue};
use turso_versioning::TableSnapshot;

#[test]
fn compat_contract_evidence_does_not_drift() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let contract = include_str!("sqlite_compat_contract.tsv");
    for (line_number, line) in contract.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (claim, evidence) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("line {} is not TSV", line_number + 1));
        let (path, needle) = evidence
            .split_once("::")
            .unwrap_or_else(|| panic!("{claim}: evidence needs path::needle"));
        let contents = std::fs::read_to_string(manifest_dir.join(path))
            .unwrap_or_else(|error| panic!("{claim}: cannot read {path}: {error}"));
        assert!(
            contents.contains(needle),
            "{claim}: missing {needle:?} in {path}"
        );
    }
}

#[test]
fn compat_manifest_is_not_a_sqlite_database() {
    let manifest = Manifest {
        root: ChunkHash([1; 20]),
        meta: ChunkHash([2; 20]),
        commits: 0,
        working: 0,
    };
    assert_ne!(&manifest.encode()[..16], b"SQLite format 3\0");

    let mut foreign = [0; turso_versioning::model::MANIFEST_SIZE];
    foreign[..16].copy_from_slice(b"SQLite format 3\0");
    assert!(matches!(
        Manifest::decode(&foreign),
        Err(StoreError::BadManifest)
    ));
}

#[test]
fn compat_pk_identity_ignores_physical_row_order() {
    let a = snapshot(
        &["id", "value"],
        &["id"],
        vec![
            vec![VcValue::Integer(1), VcValue::Text("a".to_string())],
            vec![VcValue::Integer(2), VcValue::Text("b".to_string())],
        ],
    );
    let b = snapshot(
        &["id", "value"],
        &["id"],
        vec![
            vec![VcValue::Integer(2), VcValue::Text("b".to_string())],
            vec![VcValue::Integer(1), VcValue::Text("a".to_string())],
        ],
    );
    assert_eq!(snapshot_id(&a), snapshot_id(&b));
}

#[test]
fn compat_non_integer_pk_sorts_snapshot_rows() {
    let input = snapshot(
        &["name", "value"],
        &["name"],
        vec![
            vec![VcValue::Text("z".to_string()), VcValue::Integer(2)],
            vec![VcValue::Text("a".to_string()), VcValue::Integer(1)],
        ],
    );
    let decoded = decode_snapshot(&encode_snapshot(&input)).unwrap();
    assert_eq!(decoded.rows[0].values[0], VcValue::Text("a".to_string()));
    assert_eq!(decoded.rows[1].values[0], VcValue::Text("z".to_string()));
}

fn snapshot(columns: &[&str], pk: &[&str], rows: Vec<Vec<VcValue>>) -> TableSnapshot {
    TableSnapshot {
        columns: columns.iter().map(|value| (*value).to_string()).collect(),
        pk: pk.iter().map(|value| (*value).to_string()).collect(),
        rows: rows.into_iter().map(VcRow::new).collect(),
        schema_sql: String::new(),
    }
}
