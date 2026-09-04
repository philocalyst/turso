use std::collections::{HashMap, HashSet};

use divan::{black_box, Bencher};
use turso::{Builder, Connection, Database};
use turso_versioning::gc::{mark, sweep, GcSnapshot};
use turso_versioning::model::ChunkHash;

fn main() {
    divan::main();
}

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
    let values = (0..rows)
        .map(|id| format!("({id}, 'value-{id}')"))
        .collect::<Vec<_>>()
        .join(", ");
    if !values.is_empty() {
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

fn setup_merge(rows: usize) -> Connection {
    let conn = setup_seed(rows);
    exec(&conn, "SELECT dolt_branch('feature')");
    exec(&conn, "SELECT dolt_checkout('feature')");
    exec(
        &conn,
        &format!("INSERT INTO t VALUES ({}, 'feature')", rows + 1),
    );
    exec(&conn, "SELECT dolt_add('-A')");
    exec(&conn, "SELECT dolt_commit('-m', 'feature')");
    exec(&conn, "SELECT dolt_checkout('main')");
    exec(
        &conn,
        &format!("INSERT INTO t VALUES ({}, 'main')", rows + 2),
    );
    exec(&conn, "SELECT dolt_add('-A')");
    exec(&conn, "SELECT dolt_commit('-m', 'main')");
    conn
}

#[turso_macros::divan_bench(args = [10, 100, 1000])]
fn divan_vc_commit_batch(bencher: Bencher, rows: usize) {
    bencher
        .with_inputs(|| setup_rows(rows))
        .bench_local_values(|conn| {
            black_box(exec(&conn, "SELECT dolt_add('-A')"));
            black_box(exec(&conn, "SELECT dolt_commit('-m', 'batch')"));
        });
}

#[turso_macros::divan_bench]
fn divan_vc_branch_create_delete(bencher: Bencher) {
    bencher
        .with_inputs(|| setup_seed(1))
        .bench_local_values(|conn| {
            black_box(exec(&conn, "SELECT dolt_branch('feature')"));
            black_box(exec(&conn, "SELECT dolt_branch('-d', 'feature')"));
        });
}

#[turso_macros::divan_bench(args = [10, 100, 1000])]
fn divan_vc_diff_stat(bencher: Bencher, rows: usize) {
    bencher
        .with_inputs(|| {
            let conn = setup_seed(rows);
            exec(&conn, "UPDATE t SET v = 'changed'");
            exec(&conn, "SELECT dolt_add('-A')");
            exec(&conn, "SELECT dolt_commit('-m', 'changed')");
            conn
        })
        .bench_local_values(|conn| {
            black_box(exec(
                &conn,
                "SELECT table_name, rows_modified FROM dolt_diff_stat('HEAD~1', 'HEAD')",
            ));
        });
}

#[turso_macros::divan_bench(args = [10, 100, 1000])]
fn divan_vc_merge(bencher: Bencher, rows: usize) {
    bencher
        .with_inputs(|| setup_merge(rows))
        .bench_local_values(|conn| {
            black_box(exec(&conn, "SELECT dolt_merge('feature')"));
        });
}

#[turso_macros::divan_bench(args = [5, 25, 100])]
fn divan_vc_log(bencher: Bencher, depth: usize) {
    bencher
        .with_inputs(|| setup_history(depth))
        .bench_local_values(|conn| {
            black_box(exec(&conn, "SELECT count(*) FROM dolt_log"));
        });
}

fn gc_snapshot(roots: usize) -> GcSnapshot {
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

#[turso_macros::divan_bench(args = [10, 50, 200])]
fn divan_vc_gc(bencher: Bencher, roots: usize) {
    bencher
        .with_inputs(|| gc_snapshot(roots))
        .bench_local_values(|snapshot| {
            let live = mark(black_box(&snapshot));
            black_box(sweep(black_box(&snapshot), black_box(&live)));
        });
}
