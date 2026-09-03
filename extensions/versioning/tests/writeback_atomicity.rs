//! Real-connection write-back atomicity: a table rewrite that fails an insert
//! must roll back the whole rewrite, leaving the SQL table at its pre-merge
//! count. The unit layer cannot exercise this (it has no SQL connection), so
//! it runs against the bundled engine through the public `turso` driver.

use turso::{Builder, Database};

fn connect() -> turso::Connection {
    futures::executor::block_on(async {
        let db: Database = Builder::new_local(":memory:")
            .build()
            .await
            .expect("open db");
        db.connect().expect("connect")
    })
}

fn exec(conn: &turso::Connection, sql: &str) -> Result<Vec<Vec<String>>, String> {
    futures::executor::block_on(async {
        let mut stmt = conn.prepare(sql).await.map_err(|e| e.to_string())?;
        let mut rows_result = stmt.query(()).await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(row) = rows_result.next().await.map_err(|e| e.to_string())? {
            let mut values = Vec::new();
            for i in 0..row.column_count() {
                let v = row.get_value(i).map_err(|e| e.to_string())?;
                values.push(match v {
                    turso::Value::Null => String::new(),
                    turso::Value::Integer(i) => i.to_string(),
                    turso::Value::Real(f) => f.to_string(),
                    turso::Value::Text(t) => t,
                    turso::Value::Blob(b) => String::from_utf8_lossy(&b).to_string(),
                });
            }
            out.push(values);
        }
        Ok(out)
    })
}

fn count(conn: &turso::Connection, sql: &str) -> i64 {
    let rows = exec(conn, sql).expect("count query");
    rows[0][0].parse().expect("count")
}

/// A conflicted merge whose merged work contains two rows with the same v value
/// (the unique index the work ignores). The second insert violates the index;
/// the whole rewrite must roll back, so the table keeps its pre-merge rows.
#[test]
fn writeback_failure_rolls_back_whole_table() {
    let conn = connect();
    for stmt in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
        "INSERT INTO t VALUES (1, 'base'), (2, 'keep')",
    ] {
        exec(&conn, stmt).expect("setup");
    }
    // seed
    exec(&conn, "SELECT dolt_config('user.name', 'Ada')").unwrap();
    exec(&conn, "SELECT dolt_config('user.email', 'ada@example.com')").unwrap();
    exec(&conn, "SELECT dolt_add('t')").unwrap();
    exec(&conn, "SELECT dolt_commit('seed')").unwrap();
    // feature changes row 1 to 'x' and adds a row with v='y'.
    exec(&conn, "SELECT dolt_branch('feature')").unwrap();
    exec(&conn, "SELECT dolt_checkout('feature')").unwrap();
    exec(&conn, "UPDATE t SET v = 'x' WHERE id = 1").unwrap();
    exec(&conn, "INSERT INTO t VALUES (3, 'y')").unwrap();
    exec(&conn, "SELECT dolt_add('t')").unwrap();
    exec(&conn, "SELECT dolt_commit('feature work')").unwrap();
    // main changes row 1 to 'y' and adds a row with v='x'.
    exec(&conn, "SELECT dolt_checkout('main')").unwrap();
    exec(&conn, "UPDATE t SET v = 'y' WHERE id = 1").unwrap();
    exec(&conn, "INSERT INTO t VALUES (4, 'x')").unwrap();
    exec(&conn, "SELECT dolt_add('t')").unwrap();
    exec(&conn, "SELECT dolt_commit('main work')").unwrap();
    let before = count(&conn, "SELECT count(*) FROM t");
    assert_eq!(before, 3);
    // The merge conflicts on row 1; its work keeps main's image ('y'), which
    // collides with the added row ('y') under the unique index.
    let err = exec(&conn, "SELECT dolt_merge('feature')").expect_err("merge write-back fails");
    assert!(
        err.contains("failed to write working set to SQL"),
        "unexpected error: {err}"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM t"), before);
}

/// Two-table write-back: a merge that touches two tables where the second
/// table's write-back fails.  The outer SAVEPOINT must roll back both tables,
/// not just the failing one — the first table's SQL must revert too.
#[test]
fn two_table_writeback_failure_rolls_back_both() {
    let conn = connect();
    // Table a has no unique constraint on v; table b does.
    for stmt in [
        "CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
        "INSERT INTO a VALUES (1, 'base')",
        "INSERT INTO b VALUES (1, 'base')",
    ] {
        exec(&conn, stmt).expect("setup");
    }
    exec(&conn, "SELECT dolt_config('user.name', 'Ada')").unwrap();
    exec(&conn, "SELECT dolt_config('user.email', 'ada@example.com')").unwrap();
    exec(&conn, "SELECT dolt_add('a', 'b')").unwrap();
    exec(&conn, "SELECT dolt_commit('seed')").unwrap();
    // feature adds rows to both tables.
    exec(&conn, "SELECT dolt_branch('feature')").unwrap();
    exec(&conn, "SELECT dolt_checkout('feature')").unwrap();
    exec(&conn, "INSERT INTO a VALUES (2, 'feat_a')").unwrap();
    exec(&conn, "INSERT INTO b VALUES (2, 'feat_b')").unwrap();
    exec(&conn, "SELECT dolt_add('a', 'b')").unwrap();
    exec(&conn, "SELECT dolt_commit('feature work')").unwrap();
    // main adds rows to both tables with the same v value on table b as
    // feature added, creating a UNIQUE violation in the merged working set.
    exec(&conn, "SELECT dolt_checkout('main')").unwrap();
    exec(&conn, "INSERT INTO a VALUES (3, 'main_a')").unwrap();
    exec(&conn, "INSERT INTO b VALUES (3, 'feat_b')").unwrap();
    exec(&conn, "SELECT dolt_add('a', 'b')").unwrap();
    exec(&conn, "SELECT dolt_commit('main work')").unwrap();
    let a_before = count(&conn, "SELECT count(*) FROM a");
    let b_before = count(&conn, "SELECT count(*) FROM b");
    // The merge is clean (no row conflicts), but the merged working set for
    // table b contains two rows with v='feat_b' (id=2 from feature, id=3
    // from main), violating the UNIQUE index during write-back.
    let err = exec(&conn, "SELECT dolt_merge('feature')").expect_err("merge must fail");
    assert!(
        err.contains("failed to write working set to SQL"),
        "unexpected error: {err}"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM a"), a_before);
    assert_eq!(count(&conn, "SELECT count(*) FROM b"), b_before);
}
