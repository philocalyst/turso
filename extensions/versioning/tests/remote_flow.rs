use turso_versioning::vtab_log::{VcRow, VcValue};
use turso_versioning::{VcStore, VersionError};

#[test]
fn mem_remote_full_and_lazy_clone_reach_same_tip() {
    let mut source = seeded_store("seed");
    source
        .remote_add("origin", "mem://integration-clone-flow")
        .unwrap();
    source.push("origin", "main", false).unwrap();
    let tip = source.head_commit().unwrap();

    let mut full = VcStore::new("main");
    full.clone_remote("mem://integration-clone-flow", false)
        .unwrap();
    assert_eq!(full.head_commit(), Some(tip));
    assert!(full.get_commit(&tip).is_some());

    let mut lazy = VcStore::new("main");
    lazy.clone_remote("mem://integration-clone-flow", true)
        .unwrap();
    assert_eq!(lazy.head_commit(), Some(tip));
    assert!(lazy.get_commit(&tip).is_none());
    lazy.lazy_hydrate(tip).unwrap();
    assert!(lazy.get_commit(&tip).is_some());
}

#[test]
fn failed_pull_preserves_tracking_and_work() {
    let mut source = seeded_store("seed");
    source
        .remote_add("origin", "mem://integration-pull-atomic")
        .unwrap();
    source.push("origin", "main", false).unwrap();

    let mut receiver = VcStore::new("main");
    receiver
        .remote_add("origin", "mem://integration-pull-atomic")
        .unwrap();
    receiver.track_table("local");
    let before_tracking = receiver.tracking_refs();
    let before_tables = receiver.tables();
    assert_eq!(
        receiver.pull("origin", "main").unwrap_err(),
        VersionError::PullUncommittedChanges
    );
    assert_eq!(receiver.tracking_refs(), before_tracking);
    assert_eq!(receiver.tables(), before_tables);
}

#[test]
fn authenticated_remote_unlocks_after_issuing_a_credential() {
    let mut source = seeded_store("seed");
    source
        .remote_add("origin", "mem://integration-auth?auth=required")
        .unwrap();
    assert_eq!(
        source.push("origin", "main", false).unwrap_err(),
        VersionError::NoCredentials
    );
    let credential = turso_versioning::creds::issue_global();
    assert_eq!(credential.secret.len(), 32);
    source.push("origin", "main", false).unwrap();
}

#[test]
fn sql_lazy_clone_materializes_on_first_table_read() {
    let mut source = seeded_store("lazy SQL seed");
    source
        .remote_add("origin", "mem://integration-sql-lazy-clone")
        .unwrap();
    source.push("origin", "main", false).unwrap();

    let connection = connect();
    exec(
        &connection,
        "SELECT dolt_clone('--lazy', 'mem://integration-sql-lazy-clone')",
    )
    .unwrap();
    assert_eq!(
        exec(&connection, "SELECT id, value FROM items").unwrap(),
        vec![vec!["1".to_string(), "one".to_string()]]
    );
}

#[test]
fn sql_full_clone_and_pull_write_remote_rows_back() {
    let source = connect();
    for sql in [
        "SELECT dolt_config('user.name', 'Ada')",
        "SELECT dolt_config('user.email', 'ada@example.com')",
        "CREATE TABLE sql_items (id INTEGER PRIMARY KEY, value TEXT)",
        "INSERT INTO sql_items VALUES (1, 'one')",
        "SELECT dolt_add('sql_items')",
        "SELECT dolt_commit('seed')",
        "SELECT dolt_remote('add', 'origin', 'mem://integration-sql-pull')",
        "SELECT dolt_push('origin', 'main')",
    ] {
        exec(&source, sql).unwrap();
    }

    let receiver = connect();
    exec(&receiver, "SELECT dolt_clone('mem://integration-sql-pull')").unwrap();
    assert_eq!(
        exec(&receiver, "SELECT id, value FROM sql_items").unwrap(),
        vec![vec!["1".to_string(), "one".to_string()]]
    );

    for sql in [
        "INSERT INTO sql_items VALUES (2, 'two')",
        "SELECT dolt_add('sql_items')",
        "SELECT dolt_commit('second')",
        "SELECT dolt_push('origin', 'main')",
    ] {
        exec(&source, sql).unwrap();
    }
    exec(&receiver, "SELECT dolt_pull('origin', 'main')").unwrap();
    assert_eq!(
        exec(&receiver, "SELECT id, value FROM sql_items ORDER BY id").unwrap(),
        vec![
            vec!["1".to_string(), "one".to_string()],
            vec!["2".to_string(), "two".to_string()],
        ]
    );
}

fn seeded_store(message: &str) -> VcStore {
    let mut store = VcStore::new("main");
    store.config_set("user.name", "Ada");
    store.config_set("user.email", "ada@example.com");
    store.apply_work(
        "items",
        vec!["id".to_string(), "value".to_string()],
        vec!["id".to_string()],
        vec![VcRow::new(vec![
            VcValue::Integer(1),
            VcValue::Text("one".to_string()),
        ])],
        "CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT)".to_string(),
    );
    store.track_table("items");
    store.dolt_add(&["items"]).unwrap();
    store.dolt_commit(message, None, false, false).unwrap();
    store
}

fn connect() -> turso::Connection {
    futures::executor::block_on(async {
        let database = turso::Builder::new_local(":memory:")
            .build()
            .await
            .expect("open database");
        database.connect().expect("connect")
    })
}

fn exec(connection: &turso::Connection, sql: &str) -> Result<Vec<Vec<String>>, String> {
    futures::executor::block_on(async {
        let mut statement = connection
            .prepare(sql)
            .await
            .map_err(|error| error.to_string())?;
        let mut rows = statement
            .query(())
            .await
            .map_err(|error| error.to_string())?;
        let mut output = Vec::new();
        while let Some(row) = rows.next().await.map_err(|error| error.to_string())? {
            let mut values = Vec::new();
            for index in 0..row.column_count() {
                let value = row.get_value(index).map_err(|error| error.to_string())?;
                values.push(match value {
                    turso::Value::Null => String::new(),
                    turso::Value::Integer(value) => value.to_string(),
                    turso::Value::Real(value) => value.to_string(),
                    turso::Value::Text(value) => value,
                    turso::Value::Blob(value) => format!("{value:?}"),
                });
            }
            output.push(values);
        }
        Ok(output)
    })
}
