#[cfg(feature = "codspeed")]
use codspeed_criterion_compat::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
#[cfg(not(feature = "codspeed"))]
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};

use std::collections::{HashMap, HashSet};

use turso::{Builder, Connection, Database};
use turso_versioning::gc::{mark, sweep, GcSnapshot};
use turso_versioning::model::ChunkHash;

fn connect() -> Connection {
    futures::executor::block_on(async {
        let db: Database = Builder::new_local(":memory:")
            .build()
            .await
            .expect("open db");
        db.connect().expect("connect")
    })
}

fn exec(conn: &Connection, sql: &str) -> Vec<Vec<String>> {
    futures::executor::block_on(async {
        let mut statement = conn.prepare(sql).await.expect("prepare SQL");
        let mut rows = statement.query(()).await.expect("execute SQL");
        let mut output = Vec::new();
        while let Some(row) = rows.next().await.expect("read row") {
            let mut values = Vec::with_capacity(row.column_count());
            for index in 0..row.column_count() {
                let value = row.get_value(index).expect("read value");
                values.push(match value {
                    turso::Value::Null => String::new(),
                    turso::Value::Integer(value) => value.to_string(),
                    turso::Value::Real(value) => value.to_string(),
                    turso::Value::Text(value) => value,
                    turso::Value::Blob(value) => String::from_utf8_lossy(&value).into_owned(),
                });
            }
            output.push(values);
        }
        output
    })
}

fn setup_rows(rows: usize) -> Connection {
    let conn = connect();
    for statement in [
        "SELECT dolt_config('user.name', 'Ada')",
        "SELECT dolt_config('user.email', 'ada@example.com')",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
    ] {
        exec(&conn, statement);
    }
    if rows != 0 {
        let values = (0..rows)
            .map(|id| format!("({id}, 'value-{id}')"))
            .collect::<Vec<_>>()
            .join(", ");
        exec(&conn, &format!("INSERT INTO t VALUES {values}"));
    }
    conn
}

fn setup_seed(rows: usize) -> Connection {
    let conn = setup_rows(rows);
    exec(&conn, "SELECT dolt_add('-A')");
    exec(&conn, "SELECT dolt_commit('-m', 'seed')");
    conn
}

fn setup_branch() -> Connection {
    let conn = setup_seed(1);
    exec(&conn, "SELECT dolt_branch('feature')");
    conn
}

fn setup_branch_with_commit() -> Connection {
    let conn = setup_branch();
    exec(&conn, "SELECT dolt_checkout('feature')");
    exec(
        &conn,
        "INSERT INTO t VALUES (1 + (SELECT max(id) FROM t), 'feature')",
    );
    exec(&conn, "SELECT dolt_add('-A')");
    exec(&conn, "SELECT dolt_commit('-m', 'feature')");
    exec(&conn, "SELECT dolt_checkout('main')");
    conn
}

fn setup_merge_3way() -> Connection {
    let conn = setup_seed(1);
    exec(&conn, "SELECT dolt_branch('feature')");
    exec(&conn, "SELECT dolt_checkout('feature')");
    exec(&conn, "UPDATE t SET v = 'feature' WHERE id = 0");
    exec(&conn, "SELECT dolt_add('-A')");
    exec(&conn, "SELECT dolt_commit('-m', 'feature')");
    exec(&conn, "SELECT dolt_checkout('main')");
    exec(&conn, "INSERT INTO t VALUES (1, 'main')");
    exec(&conn, "SELECT dolt_add('-A')");
    exec(&conn, "SELECT dolt_commit('-m', 'main')");
    conn
}

fn setup_history(depth: usize) -> Connection {
    let conn = setup_seed(1);
    for index in 0..depth {
        exec(
            &conn,
            &format!("INSERT INTO t VALUES ({}, 'v{}')", index + 1, index),
        );
        exec(&conn, "SELECT dolt_add('-A')");
        exec(
            &conn,
            &format!("SELECT dolt_commit('-m', 'commit-{index}')"),
        );
    }
    conn
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_commit_single(c: &mut Criterion) {
    c.bench_function("bench_vc_commit_single", |b| {
        b.iter_batched(
            || setup_rows(1),
            |conn| {
                black_box(exec(&conn, "SELECT dolt_add('-A')"));
                black_box(exec(&conn, "SELECT dolt_commit('-m', 'single-row')"));
            },
            BatchSize::SmallInput,
        );
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_commit_batch_1k(c: &mut Criterion) {
    c.bench_function("bench_vc_commit_batch_1k", |b| {
        b.iter_batched(
            || setup_rows(1_000),
            |conn| {
                black_box(exec(&conn, "SELECT dolt_add('-A')"));
                black_box(exec(&conn, "SELECT dolt_commit('-m', 'batch-1k')"));
            },
            BatchSize::SmallInput,
        );
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_branch_create(c: &mut Criterion) {
    c.bench_function("bench_vc_branch_create", |b| {
        b.iter_batched(
            || setup_seed(1),
            |conn| black_box(exec(&conn, "SELECT dolt_branch('feature')")),
            BatchSize::SmallInput,
        );
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_branch_switch(c: &mut Criterion) {
    c.bench_function("bench_vc_branch_switch", |b| {
        b.iter_batched(
            setup_branch,
            |conn| {
                black_box(exec(&conn, "SELECT dolt_checkout('feature')"));
                black_box(exec(&conn, "SELECT dolt_checkout('main')"));
            },
            BatchSize::SmallInput,
        );
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_diff_no_change(c: &mut Criterion) {
    c.bench_function("bench_vc_diff_no_change", |b| {
        b.iter_batched(
            || setup_seed(100),
            |conn| black_box(exec(&conn, "SELECT count(*) FROM dolt_diff")),
            BatchSize::SmallInput,
        );
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_diff_100_rows(c: &mut Criterion) {
    c.bench_function("bench_vc_diff_100_rows", |b| {
        b.iter_batched(
            || {
                let conn = setup_seed(100);
                exec(&conn, "UPDATE t SET v = 'changed'");
                conn
            },
            |conn| black_box(exec(&conn, "SELECT count(*) FROM dolt_diff")),
            BatchSize::SmallInput,
        );
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_merge_fast_forward(c: &mut Criterion) {
    c.bench_function("bench_vc_merge_fast_forward", |b| {
        b.iter_batched(
            setup_branch_with_commit,
            |conn| black_box(exec(&conn, "SELECT dolt_merge('feature')")),
            BatchSize::SmallInput,
        );
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_merge_3way(c: &mut Criterion) {
    c.bench_function("bench_vc_merge_3way", |b| {
        b.iter_batched(
            setup_merge_3way,
            |conn| black_box(exec(&conn, "SELECT dolt_merge('feature')")),
            BatchSize::SmallInput,
        );
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_log_10_commits(c: &mut Criterion) {
    c.bench_function("bench_vc_log_10_commits", |b| {
        b.iter_batched(
            || setup_history(10),
            |conn| black_box(exec(&conn, "SELECT count(*) FROM dolt_log")),
            BatchSize::SmallInput,
        );
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_log_100_commits(c: &mut Criterion) {
    c.bench_function("bench_vc_log_100_commits", |b| {
        b.iter_batched(
            || setup_history(100),
            |conn| black_box(exec(&conn, "SELECT count(*) FROM dolt_log")),
            BatchSize::SmallInput,
        );
    });
}

fn empty_gc_snapshot() -> GcSnapshot {
    GcSnapshot {
        all_chunks: HashSet::new(),
        refs: HashMap::new(),
        commit_refs: HashMap::new(),
        working_sets: Vec::new(),
    }
}

fn gc_snapshot_with_roots(roots: usize) -> GcSnapshot {
    let mut all_chunks = HashSet::new();
    let mut working_sets = Vec::new();
    for index in 0..roots {
        let mut bytes = [0; 20];
        bytes[..8].copy_from_slice(&(index as u64).to_be_bytes());
        let root = ChunkHash(bytes);
        all_chunks.insert(root);
        working_sets.push(vec![root]);
    }
    GcSnapshot {
        all_chunks,
        refs: HashMap::new(),
        commit_refs: HashMap::new(),
        working_sets,
    }
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_gc_empty(c: &mut Criterion) {
    let snapshot = empty_gc_snapshot();
    c.bench_function("bench_vc_gc_empty", |b| {
        b.iter(|| {
            let live = mark(black_box(&snapshot));
            black_box(sweep(black_box(&snapshot), black_box(&live)));
        });
    });
}

#[turso_macros::codspeed_criterion_benchmark]
fn bench_vc_gc_100_commits(c: &mut Criterion) {
    let snapshot = gc_snapshot_with_roots(100);
    c.bench_function("bench_vc_gc_100_commits", |b| {
        b.iter(|| {
            let live = mark(black_box(&snapshot));
            black_box(sweep(black_box(&snapshot), black_box(&live)));
        });
    });
}

criterion_group!(
    benches,
    bench_vc_commit_single,
    bench_vc_commit_batch_1k,
    bench_vc_branch_create,
    bench_vc_branch_switch,
    bench_vc_diff_no_change,
    bench_vc_diff_100_rows,
    bench_vc_merge_fast_forward,
    bench_vc_merge_3way,
    bench_vc_log_10_commits,
    bench_vc_log_100_commits,
    bench_vc_gc_empty,
    bench_vc_gc_100_commits
);
criterion_main!(benches);
