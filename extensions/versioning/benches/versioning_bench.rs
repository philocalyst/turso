#[cfg(feature = "codspeed")]
use codspeed_criterion_compat::{
    black_box, criterion_group, criterion_main, Criterion, Throughput,
};
#[cfg(not(feature = "codspeed"))]
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use turso_versioning::chunk::{blake3_chunk_hash, ProllyNode, WeibullChunker, MAX_ITEMS};
use turso_versioning::model::NodeFlags;

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

criterion_group!(
    benches,
    bench_weibull_64mib,
    bench_node_encode_4096,
    bench_blake3_1mib
);
criterion_main!(benches);
