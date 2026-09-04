#[cfg(feature = "codspeed")]
use codspeed_criterion_compat::{
    black_box, criterion_group, criterion_main, Criterion, Throughput,
};
#[cfg(not(feature = "codspeed"))]
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use turso_versioning::chunk::{blake3_chunk_hash, ProllyNode, WeibullChunker, MAX_ITEMS};
use turso_versioning::model::NodeFlags;
use turso_versioning::orm::{OrmError, VersionIndex, VersionedDb, VersionedRow};
use turso_versioning::staging::VcStore;
use turso_versioning::vtab_log::{VcRead, VcRow, VcValue};

struct IndexedUser {
    id: i64,
    name: String,
}

impl VersionedRow for IndexedUser {
    const TABLE: &'static str = "indexed_users";
    const COLUMNS: &'static [&'static str] = &["id", "name"];
    const PK: &'static [&'static str] = &["id"];

    fn into_row(&self) -> VcRow {
        VcRow::new(vec![
            VcValue::Integer(self.id),
            VcValue::Text(self.name.clone()),
        ])
    }

    fn from_row(row: &VcRow) -> Result<Self, OrmError> {
        match row.values.as_slice() {
            [VcValue::Integer(id), VcValue::Text(name)] => Ok(Self {
                id: *id,
                name: name.clone(),
            }),
            _ => Err(OrmError::Decode("invalid indexed user".into())),
        }
    }
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_weibull_64mib(c: &mut Criterion) {
    let payload: Vec<u8> = (0..64 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
        .collect();
    let mut group = c.benchmark_group("weibull");
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function("64MiB", |b| {
        b.iter(|| {
            let mut chunker = WeibullChunker::new();
            chunker.push(black_box(&payload));
            chunker.finish()
        });
    });
    group.finish();
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_node_encode_4096(c: &mut Criterion) {
    let items: Vec<(Vec<u8>, Vec<u8>)> = (0..MAX_ITEMS)
        .map(|i| {
            let key = (i as u64).to_be_bytes().to_vec();
            let val = vec![i as u8; 64];
            (key, val)
        })
        .collect();
    let node = ProllyNode {
        flags: NodeFlags::INTKEY,
        counts: [0, 0],
        items,
    };
    c.bench_function("node_encode_4096", |b| {
        b.iter(|| black_box(&node).encode());
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_blake3_1mib(c: &mut Criterion) {
    let data = vec![0xABu8; 1024 * 1024];
    let mut group = c.benchmark_group("blake3");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("1MiB", |b| {
        b.iter(|| black_box(blake3_chunk_hash(black_box(&data))));
    });
    group.finish();
}

fn seeded_store(rows: usize) -> VcStore {
    let mut s = VcStore::new("main");
    s.config_set("user.name", "Ada");
    s.config_set("user.email", "ada@example.com");
    s.apply_work(
        "t",
        vec!["id".to_string(), "v".to_string()],
        vec!["id".to_string()],
        (0..rows)
            .map(|i| {
                VcRow::new(vec![
                    VcValue::Integer(i as i64),
                    VcValue::Text(format!("v{i}")),
                ])
            })
            .collect(),
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
    );
    s.track_table("t");
    s.dolt_add(&["t"]).unwrap();
    s.set_now(1);
    let tip = s.dolt_commit("seed", None, false, false).unwrap();
    s.record_snapshot(
        tip,
        "t",
        vec!["id".to_string(), "v".to_string()],
        vec!["id".to_string()],
        s.work_table("t").unwrap().rows.clone(),
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
    );
    s
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_commit_latency(c: &mut Criterion) {
    c.bench_function("vc_commit_1row", |b| {
        b.iter(|| {
            let mut s = seeded_store(10);
            s.apply_work(
                "t",
                vec!["id".to_string(), "v".to_string()],
                vec!["id".to_string()],
                vec![VcRow::new(vec![
                    VcValue::Integer(1),
                    VcValue::Text("x".into()),
                ])],
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
            );
            s.dolt_add(&["t"]).unwrap();
            s.set_now(2);
            black_box(s.dolt_commit("c", None, false, false).unwrap());
        });
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_branch_create(c: &mut Criterion) {
    c.bench_function("vc_branch_create", |b| {
        b.iter(|| {
            let mut s = seeded_store(10);
            s.create_branch("feature").unwrap();
            black_box(s.list_branches().len());
        });
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_diff_stat(c: &mut Criterion) {
    c.bench_function("vc_diff_1k", |b| {
        b.iter(|| {
            let mut s = seeded_store(1000);
            let tip = s.head_commit().unwrap();
            s.apply_work(
                "t",
                vec!["id".to_string(), "v".to_string()],
                vec!["id".to_string()],
                (0..1000i64)
                    .map(|i| {
                        VcRow::new(vec![
                            VcValue::Integer(i),
                            VcValue::Text(format!("v{}", i + 1)),
                        ])
                    })
                    .collect(),
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
            );
            s.dolt_add(&["t"]).unwrap();
            s.set_now(2);
            let tip2 = s.dolt_commit("c2", None, false, false).unwrap();
            s.record_snapshot(
                tip2,
                "t",
                vec!["id".to_string(), "v".to_string()],
                vec!["id".to_string()],
                s.work_table("t").unwrap().rows.clone(),
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
            );
            let rows1 = s.table_rows("t", &tip).unwrap();
            let rows2 = s.table_rows("t", &tip2).unwrap();
            black_box(rows1.len() + rows2.len());
        });
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_log_scan(c: &mut Criterion) {
    c.bench_function("vc_log_100", |b| {
        b.iter(|| {
            let mut s = VcStore::new("main");
            s.config_set("user.name", "Ada");
            s.config_set("user.email", "ada@example.com");
            for i in 0..100i64 {
                s.apply_work(
                    "t",
                    vec!["id".to_string()],
                    vec!["id".to_string()],
                    vec![VcRow::new(vec![VcValue::Integer(i)])],
                    String::new(),
                );
                s.track_table("t");
                s.dolt_add(&["t"]).unwrap();
                s.set_now(i);
                let tip = s.dolt_commit(&format!("c{i}"), None, false, false).unwrap();
                s.record_snapshot(
                    tip,
                    "t",
                    vec!["id".to_string()],
                    vec!["id".to_string()],
                    s.work_table("t").unwrap().rows.clone(),
                    String::new(),
                );
            }
            black_box(s.log_views().len());
        });
    });
}

fn indexed_history(depth: usize) -> (VersionIndex<IndexedUser, String>, Vec<String>) {
    let mut db = VersionedDb::new("main").unwrap();
    db.store_mut().config_set("user.name", "Ada");
    db.store_mut().config_set("user.email", "ada@example.com");
    for version in 0..depth {
        db.table::<IndexedUser>()
            .version(
                &IndexedUser {
                    id: 1,
                    name: format!("value-{version}"),
                },
                &format!("version {version}"),
            )
            .unwrap();
    }
    let index = db
        .table::<IndexedUser>()
        .version_index_by(|user| user.name.clone())
        .unwrap();
    let linear = index
        .iter()
        .flat_map(|(value, pointers)| std::iter::repeat_n(value.clone(), pointers.len()))
        .collect();
    (index, linear)
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_orm_history_value_lookup(c: &mut Criterion) {
    let (index, linear) = indexed_history(500);
    let needle = "value-250";
    let mut group = c.benchmark_group("orm_history_value_lookup_500");
    group.bench_function("version_index", |b| {
        b.iter(|| black_box(index.get(black_box(needle))).len());
    });
    group.bench_function("linear_scan", |b| {
        b.iter(|| {
            black_box(&linear)
                .iter()
                .filter(|value| value.as_str() == black_box(needle))
                .count()
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_weibull_64mib,
    bench_node_encode_4096,
    bench_blake3_1mib,
    bench_vc_commit_latency,
    bench_vc_branch_create,
    bench_vc_diff_stat,
    bench_vc_log_scan,
    bench_orm_history_value_lookup
);
criterion_main!(benches);
