#[cfg(feature = "codspeed")]
use codspeed_criterion_compat::{
    black_box, criterion_group, criterion_main, Criterion, Throughput,
};
#[cfg(not(feature = "codspeed"))]
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use turso_versioning::chunk::{blake3_chunk_hash, ProllyNode, WeibullChunker, MAX_ITEMS};
use turso_versioning::model::NodeFlags;
use turso_versioning::staging::VcStore;
use turso_versioning::vtab_log::{VcRead, VcRow, VcValue};

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
            .map(|i| VcRow::new(vec![VcValue::Integer(i as i64), VcValue::Text(format!("v{i}"))]))
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
                vec![VcRow::new(vec![VcValue::Integer(1), VcValue::Text("x".into())])],
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
            black_box(s.create_branch(black_box("feature")).unwrap());
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
                (0..1000)
                    .map(|i| {
                        VcRow::new(vec![
                            VcValue::Integer(i as i64),
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
            for i in 0..100 {
                s.apply_work(
                    "t",
                    vec!["id".to_string()],
                    vec!["id".to_string()],
                    vec![VcRow::new(vec![VcValue::Integer(i)])],
                    String::new(),
                );
                s.track_table("t");
                s.dolt_add(&["t"]).unwrap();
                s.set_now(i as i64);
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

criterion_group!(
    benches,
    bench_weibull_64mib,
    bench_node_encode_4096,
    bench_blake3_1mib,
    bench_vc_commit_latency,
    bench_vc_branch_create,
    bench_vc_diff_stat,
    bench_vc_log_scan
);
criterion_main!(benches);
