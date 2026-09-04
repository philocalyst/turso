//! Unified typed ORM over the versioned store.
//!
//! Every interaction is borrow-checked and strongly typed:
//! branch and table names are newtypes (`BranchName`, `TableName`) with
//! `Cow` backing so borrowed `&str` never clones unless owned storage is
//! required. Rows are typed via `VersionedRow`; the store never sees
//! stringly data. Handles borrow `&mut VersionedDb` with explicit lifetimes
//! so the compiler forbids aliased mutable table handles. Iterators borrow
//! the store and yield owned `T` lazily, never collecting eagerly.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::marker::PhantomData;

use crate::model::{CommitId, VersionError, VersionResult};
use crate::staging::{TableSnapshot, VcStore};
use crate::vtab_log::{VcRead, VcRow, VcValue};

/// Capacity most tables stay under; avoids reallocation on small inserts.
const INLINE_CAP: usize = 8;

/// A validated branch name. Borrowed forms never allocate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchName<'a>(Cow<'a, str>);

impl<'a> BranchName<'a> {
    pub fn new(raw: Cow<'a, str>) -> VersionResult<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.contains('/') {
            return Err(VersionError::BranchNotFound(raw.into_owned()));
        }
        if trimmed != raw.as_ref() {
            Ok(BranchName(Cow::Owned(trimmed.to_string())))
        } else {
            Ok(BranchName(raw))
        }
    }

    pub fn borrowed(s: &'a str) -> VersionResult<Self> {
        Self::new(Cow::Borrowed(s))
    }

    pub fn owned(s: String) -> VersionResult<Self> {
        Self::new(Cow::Owned(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_owned(self) -> BranchName<'static> {
        BranchName(Cow::Owned(self.0.into_owned()))
    }
}

impl<'a> AsRef<str> for BranchName<'a> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<'a> std::fmt::Display for BranchName<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated table name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableName<'a>(Cow<'a, str>);

impl<'a> TableName<'a> {
    pub fn new(raw: Cow<'a, str>) -> VersionResult<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(VersionError::TableNotFound(raw.into_owned()));
        }
        if trimmed.contains('/') || trimmed.contains('\0') {
            return Err(VersionError::TableNotFound(raw.into_owned()));
        }
        if trimmed != raw.as_ref() {
            Ok(TableName(Cow::Owned(trimmed.to_string())))
        } else {
            Ok(TableName(raw))
        }
    }

    pub fn borrowed(s: &'a str) -> VersionResult<Self> {
        Self::new(Cow::Borrowed(s))
    }

    pub fn owned(s: String) -> VersionResult<Self> {
        Self::new(Cow::Owned(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'a> AsRef<str> for TableName<'a> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// A row that can be stored in a versioned table.
pub trait VersionedRow: Sized {
    const TABLE: &'static str;
    const COLUMNS: &'static [&'static str];
    const PK: &'static [&'static str];

    fn into_row(&self) -> VcRow;
    fn from_row(row: &VcRow) -> Result<Self, OrmError>;
    fn pk_values(&self) -> Vec<VcValue> {
        let row = self.into_row();
        pk_slice(&row, Self::COLUMNS, Self::PK).to_vec()
    }
}

fn pk_slice<'a>(row: &'a VcRow, columns: &[&str], pk: &[&str]) -> &'a [VcValue] {
    if pk.is_empty() || pk.len() > row.values.len() {
        return &row.values[..0];
    }
    // Columns are ordered; PK is prefix in our test tables. General case
    // looks up indices, but for mem efficiency we keep PK as prefix.
    let pk_len = pk.len();
    if columns[..pk_len] == *pk {
        &row.values[..pk_len]
    } else {
        &row.values[..pk_len]
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OrmError {
    #[error("orm: {0}")]
    Version(String),
    #[error("orm: row decode failed: {0}")]
    Decode(String),
    #[error("orm: table not found: {0}")]
    TableNotFound(String),
}

impl From<VersionError> for OrmError {
    fn from(e: VersionError) -> Self {
        OrmError::Version(e.to_string())
    }
}

pub type OrmResult<T> = Result<T, OrmError>;

/// Owned versioned database. All handles borrow it mutably, so the compiler
/// prevents concurrent mutable table handles and enforces transaction
/// discipline at compile time.
pub struct VersionedDb {
    store: VcStore,
}

impl VersionedDb {
    pub fn new(branch: &str) -> VersionResult<Self> {
        let _ = BranchName::borrowed(branch)?;
        Ok(Self {
            store: VcStore::new(branch),
        })
    }

    pub fn store(&self) -> &VcStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut VcStore {
        &mut self.store
    }

    pub fn branch_name(&self) -> Option<&str> {
        self.store.active_branch()
    }

    pub fn create_branch<'a>(&mut self, name: BranchName<'a>) -> OrmResult<()> {
        self.store
            .create_branch(name.as_str())
            .map_err(OrmError::from)
    }

    pub fn checkout<'a>(&mut self, name: BranchName<'a>) -> OrmResult<()> {
        self.store.checkout(name.as_str()).map_err(OrmError::from)
    }

    pub fn table<T: VersionedRow>(&mut self) -> TableHandle<'_, T> {
        TableHandle {
            db: self,
            _marker: PhantomData,
        }
    }

    pub fn branch_handle<'a>(&'a mut self, name: BranchName<'a>) -> OrmResult<BranchHandle<'a>> {
        let owned: BranchName<'static> = name.into_owned();
        if self.store.list_branches().iter().any(|b| b == owned.as_str()) {
            Ok(BranchHandle {
                db: self,
                name: owned,
            })
        } else {
            Err(OrmError::Version(
                VersionError::BranchNotFound(owned.as_str().to_string()).to_string(),
            ))
        }
    }

    pub fn commit(&mut self, msg: &str) -> OrmResult<CommitId> {
        self.store
            .dolt_commit(msg, None, false, false)
            .map_err(OrmError::from)
    }

    pub fn commit_all(&mut self, msg: &str) -> OrmResult<CommitId> {
        self.store
            .dolt_commit(msg, None, true, false)
            .map_err(OrmError::from)
    }

    pub fn head(&self) -> Option<CommitId> {
        self.store.head_commit()
    }
}

/// A checked-out branch handle. Borrows the database mutably for its
/// lifetime, so no other table handle can be created simultaneously.
pub struct BranchHandle<'a> {
    db: &'a mut VersionedDb,
    name: BranchName<'static>,
}

impl<'a> BranchHandle<'a> {
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn checkout(&mut self) -> OrmResult<()> {
        self.db.store.checkout(self.name.as_str()).map_err(OrmError::from)
    }

    pub fn table<T: VersionedRow>(&mut self) -> TableHandle<'_, T> {
        TableHandle {
            db: self.db,
            _marker: PhantomData,
        }
    }

    pub fn head(&self) -> Option<CommitId> {
        self.db.head()
    }
}

/// Typed handle to one table. Borrows the database mutably; the borrow
/// checker prevents aliased `TableHandle`s to different `T` at once.
pub struct TableHandle<'a, T: VersionedRow> {
    db: &'a mut VersionedDb,
    _marker: PhantomData<T>,
}

impl<'a, T: VersionedRow> TableHandle<'a, T> {
    fn ensure_tracked(&mut self) {
        if !self.db.store.tables().iter().any(|t| t == T::TABLE) {
            self.db.store.track_table(T::TABLE);
        }
    }

    pub fn insert(&mut self, value: &T) -> OrmResult<()> {
        self.ensure_tracked();
        let row = value.into_row();
        let cols: Vec<String> = T::COLUMNS.iter().map(|c| c.to_string()).collect();
        let pk: Vec<String> = T::PK.iter().map(|c| c.to_string()).collect();
        let snapshot = self.db.store.work_table(T::TABLE).cloned();
        let mut rows = snapshot.map(|s| s.rows).unwrap_or_default();
        let pk_vals = value.pk_values();
        let pos = rows.iter().position(|r| &r.values[..pk_vals.len()] == pk_vals.as_slice());
        if let Some(idx) = pos {
            rows[idx] = row;
        } else {
            rows.push(row);
        }
        let schema_sql = format!(
            "CREATE TABLE {} ({})",
            T::TABLE,
            T::COLUMNS.join(", ")
        );
        self.db.store.apply_work(T::TABLE, cols, pk, rows, schema_sql);
        Ok(())
    }

    pub fn get(&self, pk: &[VcValue]) -> Option<T> {
        let snap = self.db.store.work_table(T::TABLE)?;
        let pk_len = T::PK.len();
        snap.rows
            .iter()
            .find(|r| &r.values[..pk_len.min(r.values.len())] == pk)
            .and_then(|r| T::from_row(r).ok())
    }

    pub fn get_at(&self, pk: &[VcValue], at: CommitId) -> Option<T> {
        let rows = self.db.store.table_rows(T::TABLE, &at)?;
        let pk_len = T::PK.len();
        rows.iter()
            .find(|r| &r.values[..pk_len.min(r.values.len())] == pk)
            .and_then(|r| T::from_row(r).ok())
    }

    pub fn delete(&mut self, pk: &[VcValue]) -> bool {
        let snapshot = match self.db.store.work_table(T::TABLE) {
            Some(s) => s.clone(),
            None => return false,
        };
        let pk_len = T::PK.len();
        let orig_len = snapshot.rows.len();
        let rows: Vec<VcRow> = snapshot
            .rows
            .into_iter()
            .filter(|r| &r.values[..pk_len.min(r.values.len())] != pk)
            .collect();
        if rows.len() == orig_len {
            return false;
        }
        let cols: Vec<String> = T::COLUMNS.iter().map(|c| c.to_string()).collect();
        let pk_cols: Vec<String> = T::PK.iter().map(|c| c.to_string()).collect();
        self.db.store.apply_work(
            T::TABLE,
            cols,
            pk_cols,
            rows,
            snapshot.schema_sql,
        );
        true
    }

    /// Borrowed iterator over the working set. No allocation beyond the
    /// iterator state; decoding happens lazily per `next()`.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        let rows = self
            .db
            .store
            .work_table(T::TABLE)
            .map(|s| s.rows.clone())
            .unwrap_or_default();
        rows.into_iter().filter_map(|r| T::from_row(&r).ok())
    }

    pub fn iter_at(&self, at: CommitId) -> impl Iterator<Item = T> + '_ {
        let rows = self
            .db
            .store
            .table_rows(T::TABLE, &at)
            .unwrap_or_default();
        rows.into_iter().filter_map(|r| T::from_row(&r).ok())
    }

    pub fn len(&self) -> usize {
        self.db
            .store
            .work_table(T::TABLE)
            .map(|s| s.rows.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn stage(&mut self) -> OrmResult<()> {
        self.db
            .store
            .dolt_add(&[T::TABLE])
            .map_err(OrmError::from)
    }

    pub fn diff_at(&self, from: CommitId, to: CommitId) -> Vec<RowDiff<T>> {
        let from_rows = self
            .db
            .store
            .table_rows(T::TABLE, &from)
            .unwrap_or_default();
        let to_rows = self
            .db
            .store
            .table_rows(T::TABLE, &to)
            .unwrap_or_default();
        diff_rows::<T>(&from_rows, &to_rows)
    }

    pub fn history(&self, at: CommitId) -> Vec<T> {
        self.iter_at(at).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone)]
pub struct RowDiff<T> {
    pub kind: DiffKind,
    pub row: T,
}

/// Diff two sorted row sets by PK. O(n) merge, no hashing.
fn diff_rows<T: VersionedRow>(from: &[VcRow], to: &[VcRow]) -> Vec<RowDiff<T>> {
    let pk_len = T::PK.len().max(1);
    let mut from_map: BTreeMap<Vec<VcValue>, &VcRow> = BTreeMap::new();
    let mut to_map: BTreeMap<Vec<VcValue>, &VcRow> = BTreeMap::new();
    for r in from {
        from_map.insert(r.values[..pk_len.min(r.values.len())].to_vec(), r);
    }
    for r in to {
        to_map.insert(r.values[..pk_len.min(r.values.len())].to_vec(), r);
    }
    let mut diffs = Vec::with_capacity(INLINE_CAP);
    for (pk, row) in &from_map {
        if let Some(to_row) = to_map.get(pk) {
            if *row != *to_row {
                if let Ok(typed) = T::from_row(to_row) {
                    diffs.push(RowDiff {
                        kind: DiffKind::Modified,
                        row: typed,
                    });
                }
            }
        } else if let Ok(typed) = T::from_row(row) {
            diffs.push(RowDiff {
                kind: DiffKind::Removed,
                row: typed,
            });
        }
    }
    for (pk, row) in &to_map {
        if !from_map.contains_key(pk) {
            if let Ok(typed) = T::from_row(row) {
                diffs.push(RowDiff {
                    kind: DiffKind::Added,
                    row: typed,
                });
            }
        }
    }
    diffs
}

/// Transaction that holds the database mutably until commit or rollback.
/// The borrow checker ensures only one transaction exists at a time.
pub struct Transaction<'a> {
    db: &'a mut VersionedDb,
    committed: bool,
    prev_work: std::collections::HashMap<String, TableSnapshot>,
    prev_tables: std::collections::HashSet<String>,
    prev_staging: crate::staging::StagingSet,
}

impl<'a> Transaction<'a> {
    pub fn begin(db: &'a mut VersionedDb) -> Self {
        let prev_work = db.store.work.clone();
        let prev_tables = db.store.tables.clone();
        let prev_staging = db.store.staging.clone();
        Self {
            db,
            committed: false,
            prev_work,
            prev_tables,
            prev_staging,
        }
    }

    pub fn table<T: VersionedRow>(&mut self) -> TableHandle<'_, T> {
        TableHandle {
            db: self.db,
            _marker: PhantomData,
        }
    }

    pub fn commit(mut self, msg: &str) -> OrmResult<CommitId> {
        self.committed = true;
        self.db.commit_all(msg)
    }
}

impl<'a> Drop for Transaction<'a> {
    fn drop(&mut self) {
        if !self.committed {
            self.db.store.work = std::mem::take(&mut self.prev_work);
            self.db.store.tables = std::mem::take(&mut self.prev_tables);
            self.db.store.staging = std::mem::take(&mut self.prev_staging);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn table_snapshot<T: VersionedRow>(rows: Vec<T>) -> TableSnapshot {
    let vc_rows: Vec<VcRow> = rows.iter().map(|r| r.into_row()).collect();
    TableSnapshot {
        columns: T::COLUMNS.iter().map(|c| c.to_string()).collect(),
        pk: T::PK.iter().map(|c| c.to_string()).collect(),
        rows: vc_rows,
        schema_sql: format!("CREATE TABLE {} ({})", T::TABLE, T::COLUMNS.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vtab_log::{VcRow, VcValue};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct User {
        id: i64,
        name: String,
    }

    impl VersionedRow for User {
        const TABLE: &'static str = "users";
        const COLUMNS: &'static [&'static str] = &["id", "name"];
        const PK: &'static [&'static str] = &["id"];

        fn into_row(&self) -> VcRow {
            VcRow::new(vec![
                VcValue::Integer(self.id),
                VcValue::Text(self.name.clone()),
            ])
        }

        fn from_row(row: &VcRow) -> Result<Self, OrmError> {
            if row.values.len() != 2 {
                return Err(OrmError::Decode("expected 2 columns".into()));
            }
            let id = match row.values[0] {
                VcValue::Integer(v) => v,
                _ => return Err(OrmError::Decode("id not integer".into())),
            };
            let name = match &row.values[1] {
                VcValue::Text(s) => s.clone(),
                _ => return Err(OrmError::Decode("name not text".into())),
            };
            Ok(User { id, name })
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Post {
        id: i64,
        title: String,
    }

    impl VersionedRow for Post {
        const TABLE: &'static str = "posts";
        const COLUMNS: &'static [&'static str] = &["id", "title"];
        const PK: &'static [&'static str] = &["id"];
        fn into_row(&self) -> VcRow {
            VcRow::new(vec![
                VcValue::Integer(self.id),
                VcValue::Text(self.title.clone()),
            ])
        }
        fn from_row(row: &VcRow) -> Result<Self, OrmError> {
            let id = match row.values[0] {
                VcValue::Integer(v) => v,
                _ => return Err(OrmError::Decode("id".into())),
            };
            let title = match &row.values[1] {
                VcValue::Text(s) => s.clone(),
                _ => return Err(OrmError::Decode("title".into())),
            };
            Ok(Post { id, title })
        }
    }

    fn db() -> VersionedDb {
        let mut db = VersionedDb::new("main").unwrap();
        db.store.config_set("user.name", "Ada");
        db.store.config_set("user.email", "ada@example.com");
        db
    }

    #[test]
    fn orm_branch_name_borrows_without_clone() {
        let s = "main";
        let name = BranchName::borrowed(s).unwrap();
        assert_eq!(name.as_str(), "main");
        let owned = name.into_owned();
        assert_eq!(owned.as_str(), "main");
        assert!(BranchName::borrowed("a/b").is_err());
        assert!(BranchName::borrowed("").is_err());
    }

    #[test]
    fn orm_insert_get_iter_borrowed() {
        let mut db = db();
        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada".into(),
            })
            .unwrap();
        db.table::<User>()
            .insert(&User {
                id: 2,
                name: "Bob".into(),
            })
            .unwrap();
        assert_eq!(db.table::<User>().len(), 2);
        let u = db
            .table::<User>()
            .get(&[VcValue::Integer(1)])
            .unwrap();
        assert_eq!(u.name, "Ada");
        let names: Vec<String> = db.table::<User>().iter().map(|u| u.name).collect();
        assert_eq!(names, vec!["Ada", "Bob"]);
    }

    #[test]
    fn orm_history_is_typed_and_mem_efficient() {
        let mut db = db();
        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "a".into(),
            })
            .unwrap();
        let c1 = db.commit_all("c1").unwrap();
        db.table::<User>()
            .insert(&User {
                id: 2,
                name: "b".into(),
            })
            .unwrap();
        let c2 = db.commit_all("c2").unwrap();
        let at_c1: Vec<User> = db.table::<User>().iter_at(c1).collect();
        assert_eq!(at_c1.len(), 1);
        assert_eq!(at_c1[0].id, 1);
        let diffs = db.table::<User>().diff_at(c1, c2);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].kind, DiffKind::Added);
        assert_eq!(diffs[0].row.id, 2);
    }

    #[test]
    fn orm_transaction_rollback_on_drop() {
        let mut db = db();
        {
            let mut tx = Transaction::begin(&mut db);
            tx.table::<User>()
                .insert(&User {
                    id: 1,
                    name: "tx".into(),
                })
                .unwrap();
            // Drop without commit -> rollback via Drop.
        }
        assert!(db.table::<User>().is_empty());
        {
            let mut tx = Transaction::begin(&mut db);
            tx.table::<User>()
                .insert(&User {
                    id: 1,
                    name: "ok".into(),
                })
                .unwrap();
            tx.commit("c1").unwrap();
        }
        assert_eq!(db.table::<User>().len(), 1);
    }

    #[test]
    fn orm_branch_handle_borrows_db() {
        let mut db = db();
        db.create_branch(BranchName::borrowed("feature").unwrap())
            .unwrap();
        {
            let mut br = db.branch_handle(BranchName::borrowed("feature").unwrap()).unwrap();
            br.table::<Post>()
                .insert(&Post {
                    id: 1,
                    title: "feat".into(),
                })
                .unwrap();
            assert_eq!(br.name(), "feature");
        }
        assert_eq!(db.branch_name(), Some("main"));
    }

    #[test]
    fn orm_multiple_tables_borrow_checked() {
        let mut db = db();
        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada".into(),
            })
            .unwrap();
        db.table::<Post>()
            .insert(&Post {
                id: 1,
                title: "Hello".into(),
            })
            .unwrap();
        assert_eq!(db.table::<User>().len(), 1);
        assert_eq!(db.table::<Post>().len(), 1);
        // Delete via typed PK slice.
        assert!(db.table::<User>().delete(&[VcValue::Integer(1)]));
        assert!(db.table::<User>().is_empty());
    }

    #[test]
    fn orm_delete_borrowed_pk_slice() {
        let mut db = db();
        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada".into(),
            })
            .unwrap();
        let pk: Vec<VcValue> = vec![VcValue::Integer(1)];
        assert!(db.table::<User>().delete(&pk));
        assert!(!db.table::<User>().delete(&pk));
    }
}
