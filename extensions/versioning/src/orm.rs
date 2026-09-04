//! Unified typed ORM over the versioned store.
//!
//! Every interaction is borrow-checked and strongly typed:
//! branch and table names are newtypes (`BranchName`, `TableName`) with
//! `Cow` backing so borrowed `&str` never clones unless owned storage is
//! required. Rows are typed via `VersionedRow`; the store never sees
//! stringly data. Handles borrow `&mut VersionedDb` with explicit lifetimes
//! so the compiler forbids aliased mutable table handles. Iterators clone a
//! stable row snapshot, then decode and yield owned `T` values lazily.

use std::borrow::{Borrow, Cow};
use std::collections::BTreeMap;
use std::marker::PhantomData;

use crate::model::{CommitId, VersionError, VersionResult};
use crate::staging::{TableSnapshot, VcStore};
use crate::vtab_log::{VcRead, VcRow, VcValue};

/// Small starting capacity for diff results.
const INLINE_CAP: usize = 8;
const VERSION_POINTER_FORMAT: i64 = 1;

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

    #[allow(clippy::wrong_self_convention)]
    fn into_row(&self) -> VcRow;
    fn from_row(row: &VcRow) -> Result<Self, OrmError>;

    /// Returns this value's identity in the declared primary-key order.
    fn pk_values(&self) -> OrmResult<Vec<VcValue>> {
        declared_row_key::<Self>(&self.into_row())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OrmError {
    #[error("orm: {0}")]
    Version(String),
    #[error("orm: row decode failed: {0}")]
    Decode(String),
    #[error("orm: table not found: {0}")]
    TableNotFound(String),
    #[error("orm: cannot checkout another branch with uncommitted changes")]
    CheckoutUncommittedChanges,
    #[error("orm: table {table} does not declare a primary key")]
    PrimaryKeyNotDeclared { table: &'static str },
    #[error("orm: primary key for {table} has {actual} values; expected {expected}")]
    PrimaryKeyArity {
        table: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("orm: invalid primary-key declaration for {table}: {reason}")]
    InvalidPrimaryKey { table: &'static str, reason: String },
    #[error("orm: row not found in {table} for primary key {key:?}")]
    RowNotFound {
        table: &'static str,
        key: Vec<VcValue>,
    },
    #[error("orm: update for {table} changed the selected primary key")]
    PrimaryKeyChanged { table: &'static str },
    #[error("orm: table {table} contains more than one row for primary key {key:?}")]
    DuplicatePrimaryKey {
        table: &'static str,
        key: Vec<VcValue>,
    },
    #[error("orm: invalid version pointer for {table}: {reason}")]
    InvalidVersionPointer { table: &'static str, reason: String },
}

impl From<VersionError> for OrmError {
    fn from(e: VersionError) -> Self {
        OrmError::Version(e.to_string())
    }
}

pub type OrmResult<T> = Result<T, OrmError>;

/// A primary key tied to one row type. A key for one model cannot be used to
/// select a row from another model.
pub struct RowKey<T: VersionedRow> {
    values: Vec<VcValue>,
    _marker: PhantomData<fn() -> T>,
}

impl<T: VersionedRow> std::fmt::Debug for RowKey<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RowKey").field(&self.values).finish()
    }
}

impl<T: VersionedRow> Clone for RowKey<T> {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T: VersionedRow> PartialEq for RowKey<T> {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}

impl<T: VersionedRow> Eq for RowKey<T> {}

impl<T: VersionedRow> RowKey<T> {
    pub fn new(values: impl IntoIterator<Item = VcValue>) -> OrmResult<Self> {
        let values: Vec<VcValue> = values.into_iter().collect();
        validate_declared_key::<T>(&values)?;
        Ok(Self {
            values,
            _marker: PhantomData,
        })
    }

    /// Builds a key from a typed value, avoiding hand-written cell types.
    pub fn from_value(value: &T) -> OrmResult<Self> {
        let row = encoded_row::<T>(value)?;
        Self::new(declared_row_key::<T>(&row)?)
    }

    pub fn values(&self) -> &[VcValue] {
        &self.values
    }
}

/// A typed pointer to one row at one immutable commit. Its cell encoding can
/// be stored in another versioned table to build persistent secondary indexes.
pub struct VersionPointer<T: VersionedRow> {
    revision: CommitId,
    key: RowKey<T>,
    row_ordinal: Option<usize>,
}

impl<T: VersionedRow> std::fmt::Debug for VersionPointer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionPointer")
            .field("revision", &self.revision)
            .field("key", &self.key)
            .field("row_ordinal", &self.row_ordinal)
            .finish()
    }
}

impl<T: VersionedRow> Clone for VersionPointer<T> {
    fn clone(&self) -> Self {
        Self {
            revision: self.revision,
            key: self.key.clone(),
            row_ordinal: self.row_ordinal,
        }
    }
}

impl<T: VersionedRow> PartialEq for VersionPointer<T> {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision && self.key == other.key
    }
}

impl<T: VersionedRow> Eq for VersionPointer<T> {}

impl<T: VersionedRow> VersionPointer<T> {
    pub fn new(revision: CommitId, key: RowKey<T>) -> Self {
        Self {
            revision,
            key,
            row_ordinal: None,
        }
    }

    pub fn from_value(revision: CommitId, value: &T) -> OrmResult<Self> {
        Ok(Self::new(revision, RowKey::from_value(value)?))
    }

    pub fn revision(&self) -> CommitId {
        self.revision
    }

    pub fn key(&self) -> &RowKey<T> {
        &self.key
    }

    pub fn row_ordinal(&self) -> Option<usize> {
        self.row_ordinal
    }

    /// Encodes a format version, the commit as a 20-byte BLOB, the optional row
    /// ordinal (`-1` when absent), then the primary-key cells.
    pub fn to_values(&self) -> Vec<VcValue> {
        let mut values = Vec::with_capacity(self.key.values.len() + 3);
        values.push(VcValue::Integer(VERSION_POINTER_FORMAT));
        values.push(VcValue::Blob(self.revision.0.to_vec()));
        let row_ordinal = self.row_ordinal.map_or(-1, |ordinal| {
            i64::try_from(ordinal).expect("row ordinal exceeds the largest supported table")
        });
        values.push(VcValue::Integer(row_ordinal));
        values.extend(self.key.values.iter().cloned());
        values
    }

    pub fn from_values(values: &[VcValue]) -> OrmResult<Self> {
        let [format, revision, row_ordinal, key @ ..] = values else {
            return Err(invalid_version_pointer::<T>(
                "missing the format, commit id, row ordinal, or primary key",
            ));
        };
        if format != &VcValue::Integer(VERSION_POINTER_FORMAT) {
            return Err(invalid_version_pointer::<T>(
                "unsupported pointer format version",
            ));
        }
        let VcValue::Blob(bytes) = revision else {
            return Err(invalid_version_pointer::<T>(
                "commit id must be a 20-byte BLOB",
            ));
        };
        let revision = bytes
            .as_slice()
            .try_into()
            .map(CommitId)
            .map_err(|_| invalid_version_pointer::<T>("commit id must be a 20-byte BLOB"))?;
        let row_ordinal = match row_ordinal {
            VcValue::Integer(-1) => None,
            VcValue::Integer(ordinal) if *ordinal >= 0 => Some(
                usize::try_from(*ordinal)
                    .map_err(|_| invalid_version_pointer::<T>("row ordinal is too large"))?,
            ),
            _ => {
                return Err(invalid_version_pointer::<T>(
                    "row ordinal must be a non-negative integer or -1",
                ));
            }
        };
        Ok(Self {
            revision,
            key: RowKey::new(key.iter().cloned())?,
            row_ordinal,
        })
    }

    pub fn load(&self, db: &VersionedDb) -> OrmResult<Option<T>> {
        if db.store.get_commit(&self.revision).is_none() {
            return Err(VersionError::CommitNotFound(self.revision.to_hex()).into());
        }
        let Some(snapshot) = db
            .store
            .snapshots
            .get(&self.revision)
            .and_then(|tables| tables.get(T::TABLE))
        else {
            return Ok(None);
        };
        validate_snapshot::<T>(snapshot)?;
        if let Some(row) = self
            .row_ordinal
            .and_then(|ordinal| snapshot.rows.get(ordinal))
        {
            if declared_row_key::<T>(row)? == self.key.values {
                return decode_row::<T>(row).map(Some);
            }
        }
        typed_selected_row::<T>(&snapshot.rows, self.key.values())
    }

    fn indexed(revision: CommitId, key: RowKey<T>, row_ordinal: usize) -> Self {
        Self {
            revision,
            key,
            row_ordinal: Some(row_ordinal),
        }
    }
}

/// An immutable secondary index over row versions reachable from one branch
/// head. Build it once, then each exact lookup is a B-tree lookup plus matches.
pub struct VersionIndex<T: VersionedRow, K: Ord> {
    head: Option<CommitId>,
    entries: BTreeMap<K, Vec<VersionPointer<T>>>,
    len: usize,
}

impl<T: VersionedRow, K: Ord> VersionIndex<T, K> {
    pub fn head(&self) -> Option<CommitId> {
        self.head
    }

    pub fn get<Q>(&self, value: &Q) -> &[VersionPointer<T>]
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.entries.get(value).map_or(&[], Vec::as_slice)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &[VersionPointer<T>])> {
        self.entries
            .iter()
            .map(|(value, pointers)| (value, pointers.as_slice()))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

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
        let switching = self.store.active_branch() != Some(name.as_str());
        if switching && self.store.has_uncommitted() {
            return Err(OrmError::CheckoutUncommittedChanges);
        }
        self.store.checkout(name.as_str()).map_err(OrmError::from)?;
        if switching {
            self.store.sync_work_to_head();
        }
        Ok(())
    }

    pub fn table<T: VersionedRow>(&mut self) -> TableHandle<'_, T> {
        TableHandle {
            db: self,
            _marker: PhantomData,
        }
    }

    pub fn branch_handle<'a>(&'a mut self, name: BranchName<'a>) -> OrmResult<BranchHandle<'a>> {
        let owned: BranchName<'static> = name.into_owned();
        if self
            .store
            .list_branches()
            .iter()
            .any(|b| b == owned.as_str())
        {
            self.checkout(BranchName::borrowed(owned.as_str())?)?;
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
        self.store
            .detached_snapshot()
            .or_else(|| self.store.head_commit())
    }
}

/// A handle for the active branch. Creating it checks out the named branch;
/// the mutable borrow prevents operations through another branch at the same
/// time.
pub struct BranchHandle<'a> {
    db: &'a mut VersionedDb,
    name: BranchName<'static>,
}

impl<'a> BranchHandle<'a> {
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn checkout(&mut self) -> OrmResult<()> {
        self.db.checkout(BranchName::borrowed(self.name.as_str())?)
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

    /// Selects a row by values in `T::PK` order. The returned handle carries
    /// `T`, so it cannot later write a value for a different table.
    pub fn row(self, pk: &[VcValue]) -> OrmResult<RowHandle<'a, T>> {
        Ok(self.select(RowKey::new(pk.iter().cloned())?))
    }

    /// Selects a row with a model-bound primary key.
    pub fn select(self, key: RowKey<T>) -> RowHandle<'a, T> {
        RowHandle { key, db: self.db }
    }

    /// Returns a reusable model-bound key for a typed value.
    pub fn key_for(&self, value: &T) -> OrmResult<RowKey<T>> {
        RowKey::from_value(value)
    }

    /// Inserts or updates one value and commits only that row. Any unrelated
    /// working or staged changes stay pending.
    pub fn version(&mut self, value: &T, message: &str) -> OrmResult<CommitId> {
        let before = self.db.store.clone();
        let result = (|| {
            self.insert(value)?;
            let key = RowKey::from_value(value)?;
            RowHandle { key, db: self.db }.commit(message)
        })();
        if result.is_err() {
            self.db.store = before;
        }
        result
    }

    /// Deletes and commits one selected row as a single atomic operation.
    pub fn version_delete(&mut self, pk: &[VcValue], message: &str) -> OrmResult<CommitId> {
        let before = self.db.store.clone();
        let result = (|| {
            let key: RowKey<T> = RowKey::new(pk.iter().cloned())?;
            let mut row = RowHandle { key, db: self.db };
            row.delete()?;
            row.commit(message)
        })();
        if result.is_err() {
            self.db.store = before;
        }
        result
    }

    pub fn insert(&mut self, value: &T) -> OrmResult<()> {
        self.db.store.guard_write()?;
        let row = encoded_row::<T>(value)?;
        let key = row_identity::<T>(&row)?;
        let cols: Vec<String> = T::COLUMNS.iter().map(|c| c.to_string()).collect();
        let pk: Vec<String> = T::PK.iter().map(|c| c.to_string()).collect();
        let snapshot = self.db.store.work_table(T::TABLE).cloned();
        if let Some(snapshot) = &snapshot {
            validate_snapshot::<T>(snapshot)?;
        }
        let schema_sql = snapshot
            .as_ref()
            .map(|snapshot| snapshot.schema_sql.clone())
            .unwrap_or_else(|| format!("CREATE TABLE {} ({})", T::TABLE, T::COLUMNS.join(", ")));
        let mut rows = snapshot.map(|snapshot| snapshot.rows).unwrap_or_default();
        if let Some(idx) = row_index_by_identity::<T>(&rows, &key)? {
            rows[idx] = row;
        } else {
            rows.push(row);
        }
        self.ensure_tracked();
        self.db
            .store
            .apply_work(T::TABLE, cols, pk, rows, schema_sql);
        Ok(())
    }

    /// Legacy lossy lookup. Use [`TableHandle::row`] when malformed rows and
    /// invalid keys must be reported to the caller.
    pub fn get(&self, pk: &[VcValue]) -> Option<T> {
        let snap = self.db.store.work_table(T::TABLE)?;
        snap.rows
            .iter()
            .find(|r| row_identity::<T>(r).is_ok_and(|row_key| row_key.as_slice() == pk))
            .and_then(|r| T::from_row(r).ok())
    }

    /// Legacy lossy historical lookup. Use [`RowHandle::at`] for checked
    /// row-level reads.
    pub fn get_at(&self, pk: &[VcValue], at: CommitId) -> Option<T> {
        let rows = self.db.store.table_rows(T::TABLE, &at)?;
        rows.iter()
            .find(|r| row_identity::<T>(r).is_ok_and(|row_key| row_key.as_slice() == pk))
            .and_then(|r| T::from_row(r).ok())
    }

    /// Legacy lossy delete. Use [`TableHandle::row`] and
    /// [`RowHandle::delete`] when a missing or malformed row must be visible.
    pub fn delete(&mut self, pk: &[VcValue]) -> bool {
        if self.db.store.guard_write().is_err() {
            return false;
        }
        let snapshot = match self.db.store.work_table(T::TABLE) {
            Some(s) => s.clone(),
            None => return false,
        };
        let mut deleted = false;
        let rows: Vec<VcRow> = snapshot
            .rows
            .into_iter()
            .filter(|row| match row_identity::<T>(row) {
                Ok(row_key) if row_key.as_slice() == pk => {
                    deleted = true;
                    false
                }
                _ => true,
            })
            .collect();
        if !deleted {
            return false;
        }
        let cols: Vec<String> = T::COLUMNS.iter().map(|c| c.to_string()).collect();
        let pk_cols: Vec<String> = T::PK.iter().map(|c| c.to_string()).collect();
        self.db
            .store
            .apply_work(T::TABLE, cols, pk_cols, rows, snapshot.schema_sql);
        true
    }

    /// Checked lookup that surfaces primary-key and row decoding errors.
    pub fn try_get(&self, pk: &[VcValue]) -> OrmResult<Option<T>> {
        let key: RowKey<T> = RowKey::new(pk.iter().cloned())?;
        let Some(snapshot) = self.db.store.work_table(T::TABLE) else {
            return Ok(None);
        };
        typed_selected_row::<T>(&snapshot.rows, key.values())
    }

    /// Checked historical lookup that surfaces an invalid revision, malformed
    /// row, or invalid primary key instead of turning it into `None`.
    pub fn try_get_at(&self, pk: &[VcValue], at: CommitId) -> OrmResult<Option<T>> {
        let key: RowKey<T> = RowKey::new(pk.iter().cloned())?;
        if self.db.store.get_commit(&at).is_none() {
            return Err(VersionError::CommitNotFound(at.to_hex()).into());
        }
        let rows = self.db.store.table_rows(T::TABLE, &at).unwrap_or_default();
        typed_selected_row::<T>(&rows, key.values())
    }

    /// Iterator over a snapshot of the working rows. Decoding is lazy after
    /// the row snapshot is cloned.
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
        let rows = self.db.store.table_rows(T::TABLE, &at).unwrap_or_default();
        rows.into_iter().filter_map(|r| T::from_row(&r).ok())
    }

    /// Checked working-set iterator. Each corrupt row is delivered as an
    /// error rather than being filtered out.
    pub fn try_iter(&self) -> impl Iterator<Item = OrmResult<T>> + '_ {
        let rows = self
            .db
            .store
            .work_table(T::TABLE)
            .map(|s| s.rows.clone())
            .unwrap_or_default();
        rows.into_iter().map(|row| decode_row::<T>(&row))
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
        self.db.store.dolt_add(&[T::TABLE]).map_err(OrmError::from)
    }

    pub fn diff_at(&self, from: CommitId, to: CommitId) -> Vec<RowDiff<T>> {
        self.try_diff_at(from, to).unwrap_or_default()
    }

    /// Checked diff that never drops malformed rows or invalid identities.
    pub fn try_diff_at(&self, from: CommitId, to: CommitId) -> OrmResult<Vec<RowDiff<T>>> {
        let from_rows = self
            .db
            .store
            .table_rows(T::TABLE, &from)
            .unwrap_or_default();
        let to_rows = self.db.store.table_rows(T::TABLE, &to).unwrap_or_default();
        diff_rows::<T>(&from_rows, &to_rows)
    }

    pub fn history(&self, at: CommitId) -> Vec<T> {
        self.iter_at(at).collect()
    }

    /// Builds a reusable secondary index over every changed row image in the
    /// active branch's first-parent history. Entries for each value are newest
    /// first and point to the exact row and commit where that value appeared.
    pub fn version_index_by<K: Ord>(
        &self,
        key_for: impl FnMut(&T) -> K,
    ) -> OrmResult<VersionIndex<T, K>> {
        build_version_index(self.db, key_for)
    }
}

/// A selected row. It owns the table handle's mutable database borrow, so a
/// selected row cannot be mixed with an aliased table operation.
pub struct RowHandle<'a, T: VersionedRow> {
    db: &'a mut VersionedDb,
    key: RowKey<T>,
}

impl<'a, T: VersionedRow> RowHandle<'a, T> {
    pub fn key(&self) -> &RowKey<T> {
        &self.key
    }

    /// Reads the selected row from the current working set. `None` is a real
    /// missing row; malformed stored rows are returned as errors.
    pub fn current(&self) -> OrmResult<Option<T>> {
        let Some(snapshot) = self.db.store.work_table(T::TABLE) else {
            return Ok(None);
        };
        typed_selected_row::<T>(&snapshot.rows, self.key.values())
    }

    /// Reads the selected row from one committed revision.
    pub fn at(&self, at: CommitId) -> OrmResult<Option<T>> {
        self.raw_at(at)?.as_ref().map(decode_row::<T>).transpose()
    }

    /// Lists changes to this row along the current branch's first-parent
    /// history, newest first. A `None` value records a deletion.
    pub fn history(&self) -> OrmResult<Vec<RowVersion<T>>> {
        let mut history = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut revision = self.db.head();

        while let Some(at) = revision {
            if !seen.insert(at) {
                return Err(VersionError::CommitGraphCycle(at.to_hex()).into());
            }
            let value = self.raw_at(at)?;
            let commit = self
                .db
                .store
                .get_commit(&at)
                .ok_or_else(|| OrmError::from(VersionError::CommitNotFound(at.to_hex())))?;
            let parent = commit.parents.first().copied();
            let parent_value = parent.map(|id| self.raw_at(id)).transpose()?.flatten();

            if value != parent_value {
                history.push(RowVersion {
                    revision: at,
                    value: value.as_ref().map(decode_row::<T>).transpose()?,
                });
            }
            revision = parent;
        }

        Ok(history)
    }

    /// Replaces the selected row in the working set. The typed value must keep
    /// the selected key; use a new selection for a primary-key move.
    pub fn update(&mut self, value: &T) -> OrmResult<()> {
        self.db.store.guard_write()?;
        let row = encoded_row::<T>(value)?;
        if declared_row_key::<T>(&row)? != self.key.values {
            return Err(OrmError::PrimaryKeyChanged { table: T::TABLE });
        }
        let snapshot = self
            .db
            .store
            .work_table(T::TABLE)
            .cloned()
            .ok_or_else(|| self.missing_row())?;
        validate_snapshot::<T>(&snapshot)?;
        let index = selected_row_index::<T>(&snapshot.rows, self.key.values())?
            .ok_or_else(|| self.missing_row())?;
        let mut rows = snapshot.rows;
        rows[index] = row;
        self.db.store.apply_work(
            T::TABLE,
            snapshot.columns,
            snapshot.pk,
            rows,
            snapshot.schema_sql,
        );
        Ok(())
    }

    /// Removes the selected row from the working set. Missing rows are an
    /// error, which keeps a mistyped key from becoming a silent no-op.
    pub fn delete(&mut self) -> OrmResult<()> {
        self.db.store.guard_write()?;
        let snapshot = self
            .db
            .store
            .work_table(T::TABLE)
            .cloned()
            .ok_or_else(|| self.missing_row())?;
        validate_snapshot::<T>(&snapshot)?;
        let index = selected_row_index::<T>(&snapshot.rows, self.key.values())?
            .ok_or_else(|| self.missing_row())?;
        let mut rows = snapshot.rows;
        rows.remove(index);
        self.db.store.apply_work(
            T::TABLE,
            snapshot.columns,
            snapshot.pk,
            rows,
            snapshot.schema_sql,
        );
        Ok(())
    }

    /// Commits only this row's difference from HEAD. Other rows and all other
    /// tables are restored to their working state after the commit succeeds.
    pub fn commit(&mut self, message: &str) -> OrmResult<CommitId> {
        self.db.store.guard_write()?;
        if self.db.store.is_merge_open() {
            return Err(VersionError::MergeInProgress.into());
        }
        if self.db.store.is_rebase_open() {
            return Err(VersionError::RebaseInProgress.into());
        }

        let before = self.db.store.clone();
        let original_work = before.work.clone();
        let original_staging = before.staging.clone();
        let head = self.db.head();
        let base = head.and_then(|id| {
            self.db
                .store
                .snapshots
                .get(&id)
                .and_then(|tables| tables.get(T::TABLE))
                .cloned()
        });
        let working = original_work.get(T::TABLE).cloned();
        let selected_snapshot =
            selected_commit_snapshot::<T>(base.as_ref(), working.as_ref(), self.key.values())?;

        self.db
            .store
            .work
            .insert(T::TABLE.to_string(), selected_snapshot);
        self.db.store.staging = crate::staging::StagingSet::new();
        self.db.store.staging.stage(T::TABLE);

        match self.db.store.dolt_commit(message, None, false, false) {
            Ok(commit) => {
                self.db.store.work = original_work;
                self.db.store.staging = original_staging;
                self.restore_non_selected_work(commit);
                Ok(commit)
            }
            Err(error) => {
                self.db.store = before;
                Err(error.into())
            }
        }
    }

    fn raw_at(&self, at: CommitId) -> OrmResult<Option<VcRow>> {
        if self.db.store.get_commit(&at).is_none() {
            return Err(VersionError::CommitNotFound(at.to_hex()).into());
        }
        let rows = self.db.store.table_rows(T::TABLE, &at).unwrap_or_default();
        selected_raw_row::<T>(&rows, self.key.values()).map(|row| row.cloned())
    }

    fn restore_non_selected_work(&mut self, commit: CommitId) {
        let committed = self
            .db
            .store
            .snapshots
            .get(&commit)
            .and_then(|tables| tables.get(T::TABLE));
        if self.db.store.work.get(T::TABLE) == committed {
            self.db.store.staging.discard(T::TABLE);
        }
    }

    fn missing_row(&self) -> OrmError {
        OrmError::RowNotFound {
            table: T::TABLE,
            key: self.key.values.clone(),
        }
    }
}

/// One committed image of a selected row. `value` is `None` for a deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowVersion<T> {
    pub revision: CommitId,
    pub value: Option<T>,
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

/// Diff two row sets by primary key in deterministic key order.
fn diff_rows<T: VersionedRow>(from: &[VcRow], to: &[VcRow]) -> OrmResult<Vec<RowDiff<T>>> {
    let from_map = rows_by_identity::<T>(from)?;
    let to_map = rows_by_identity::<T>(to)?;
    let mut diffs = Vec::with_capacity(INLINE_CAP);
    for (pk, (_, row)) in &from_map {
        if let Some((_, to_row)) = to_map.get(pk) {
            if *row != *to_row {
                diffs.push(RowDiff {
                    kind: DiffKind::Modified,
                    row: decode_row::<T>(to_row)?,
                });
            }
        } else {
            diffs.push(RowDiff {
                kind: DiffKind::Removed,
                row: decode_row::<T>(row)?,
            });
        }
    }
    for (pk, (_, row)) in &to_map {
        if !from_map.contains_key(pk) {
            diffs.push(RowDiff {
                kind: DiffKind::Added,
                row: decode_row::<T>(row)?,
            });
        }
    }
    Ok(diffs)
}

fn build_version_index<T, K>(
    db: &VersionedDb,
    mut key_for: impl FnMut(&T) -> K,
) -> OrmResult<VersionIndex<T, K>>
where
    T: VersionedRow,
    K: Ord,
{
    let head = db.head();
    let mut entries: BTreeMap<K, Vec<VersionPointer<T>>> = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut revision = head;
    let mut len = 0;

    while let Some(at) = revision {
        if !seen.insert(at) {
            return Err(VersionError::CommitGraphCycle(at.to_hex()).into());
        }
        let commit = db
            .store
            .get_commit(&at)
            .ok_or_else(|| OrmError::from(VersionError::CommitNotFound(at.to_hex())))?;
        let parent = commit.parents.first().copied();
        let current = db
            .store
            .snapshots
            .get(&at)
            .and_then(|tables| tables.get(T::TABLE));
        let previous = parent.and_then(|parent| {
            db.store
                .snapshots
                .get(&parent)
                .and_then(|tables| tables.get(T::TABLE))
        });
        index_changed_rows(current, previous, at, &mut key_for, &mut entries, &mut len)?;
        revision = parent;
    }

    Ok(VersionIndex { head, entries, len })
}

fn index_changed_rows<T, K>(
    current: Option<&TableSnapshot>,
    previous: Option<&TableSnapshot>,
    revision: CommitId,
    key_for: &mut impl FnMut(&T) -> K,
    entries: &mut BTreeMap<K, Vec<VersionPointer<T>>>,
    len: &mut usize,
) -> OrmResult<()>
where
    T: VersionedRow,
    K: Ord,
{
    if let Some(snapshot) = current {
        validate_snapshot::<T>(snapshot)?;
    }
    if let Some(snapshot) = previous {
        validate_snapshot::<T>(snapshot)?;
    }
    let current_rows = rows_by_identity::<T>(current.map_or(&[], |snapshot| &snapshot.rows))?;
    let previous_rows = rows_by_identity::<T>(previous.map_or(&[], |snapshot| &snapshot.rows))?;

    for (key, (row_ordinal, row)) in current_rows {
        if previous_rows
            .get(&key)
            .is_some_and(|(_, previous)| *previous == row)
        {
            continue;
        }
        let value = decode_row::<T>(row)?;
        entries
            .entry(key_for(&value))
            .or_default()
            .push(VersionPointer::indexed(
                revision,
                RowKey {
                    values: key,
                    _marker: PhantomData,
                },
                row_ordinal,
            ));
        *len += 1;
    }
    Ok(())
}

fn rows_by_identity<T: VersionedRow>(
    rows: &[VcRow],
) -> OrmResult<BTreeMap<Vec<VcValue>, (usize, &VcRow)>> {
    let mut indexed = BTreeMap::new();
    for (row_ordinal, row) in rows.iter().enumerate() {
        let key = row_identity::<T>(row)?;
        decode_row::<T>(row)?;
        if indexed.insert(key.clone(), (row_ordinal, row)).is_some() {
            return Err(OrmError::DuplicatePrimaryKey {
                table: T::TABLE,
                key,
            });
        }
    }
    Ok(indexed)
}

fn invalid_version_pointer<T: VersionedRow>(reason: &str) -> OrmError {
    OrmError::InvalidVersionPointer {
        table: T::TABLE,
        reason: reason.to_string(),
    }
}

fn encoded_row<T: VersionedRow>(value: &T) -> OrmResult<VcRow> {
    let row = value.into_row();
    decode_row::<T>(&row)?;
    Ok(row)
}

fn primary_key_indices<T: VersionedRow>() -> OrmResult<Vec<usize>> {
    if T::PK.is_empty() {
        return Err(OrmError::PrimaryKeyNotDeclared { table: T::TABLE });
    }

    let mut indices = Vec::with_capacity(T::PK.len());
    for primary_key in T::PK {
        let mut found = None;
        for (index, column) in T::COLUMNS.iter().enumerate() {
            if column == primary_key && found.replace(index).is_some() {
                return Err(OrmError::InvalidPrimaryKey {
                    table: T::TABLE,
                    reason: format!("column {primary_key} appears more than once"),
                });
            }
        }
        let Some(index) = found else {
            return Err(OrmError::InvalidPrimaryKey {
                table: T::TABLE,
                reason: format!("column {primary_key} is missing from COLUMNS"),
            });
        };
        if indices.contains(&index) {
            return Err(OrmError::InvalidPrimaryKey {
                table: T::TABLE,
                reason: format!("column {primary_key} appears more than once in PK"),
            });
        }
        indices.push(index);
    }
    Ok(indices)
}

fn validate_declared_key<T: VersionedRow>(key: &[VcValue]) -> OrmResult<()> {
    let expected = primary_key_indices::<T>()?.len();
    if key.len() != expected {
        return Err(OrmError::PrimaryKeyArity {
            table: T::TABLE,
            expected,
            actual: key.len(),
        });
    }
    Ok(())
}

fn row_identity<T: VersionedRow>(row: &VcRow) -> OrmResult<Vec<VcValue>> {
    validate_row_shape::<T>(row)?;
    if T::PK.is_empty() {
        return Ok(row.values.clone());
    }
    declared_row_key::<T>(row)
}

fn declared_row_key<T: VersionedRow>(row: &VcRow) -> OrmResult<Vec<VcValue>> {
    validate_row_shape::<T>(row)?;
    let indices = primary_key_indices::<T>()?;
    indices
        .into_iter()
        .map(|index| {
            row.values.get(index).cloned().ok_or_else(|| {
                OrmError::Decode(format!("{} row is missing column {index}", T::TABLE))
            })
        })
        .collect()
}

fn validate_row_shape<T: VersionedRow>(row: &VcRow) -> OrmResult<()> {
    if row.values.len() != T::COLUMNS.len() {
        return Err(OrmError::Decode(format!(
            "{} row has {} values for {} columns",
            T::TABLE,
            row.values.len(),
            T::COLUMNS.len()
        )));
    }
    Ok(())
}

fn decode_row<T: VersionedRow>(row: &VcRow) -> OrmResult<T> {
    validate_row_shape::<T>(row)?;
    T::from_row(row)
}

fn validate_snapshot<T: VersionedRow>(snapshot: &TableSnapshot) -> OrmResult<()> {
    let columns: Vec<String> = T::COLUMNS
        .iter()
        .map(|column| (*column).to_string())
        .collect();
    if snapshot.columns != columns {
        return Err(OrmError::Decode(format!(
            "{} snapshot columns do not match the model",
            T::TABLE
        )));
    }
    let primary_key: Vec<String> = T::PK.iter().map(|column| (*column).to_string()).collect();
    if snapshot.pk != primary_key {
        return Err(OrmError::InvalidPrimaryKey {
            table: T::TABLE,
            reason: "snapshot primary key does not match the model".into(),
        });
    }
    Ok(())
}

fn row_index_by_identity<T: VersionedRow>(
    rows: &[VcRow],
    key: &[VcValue],
) -> OrmResult<Option<usize>> {
    let mut found = None;
    for (index, row) in rows.iter().enumerate() {
        let row_key = row_identity::<T>(row)?;
        decode_row::<T>(row)?;
        if row_key == key && found.replace(index).is_some() {
            return Err(OrmError::DuplicatePrimaryKey {
                table: T::TABLE,
                key: key.to_vec(),
            });
        }
    }
    Ok(found)
}

fn selected_row_index<T: VersionedRow>(
    rows: &[VcRow],
    key: &[VcValue],
) -> OrmResult<Option<usize>> {
    validate_declared_key::<T>(key)?;
    let mut found = None;
    for (index, row) in rows.iter().enumerate() {
        let row_key = declared_row_key::<T>(row)?;
        decode_row::<T>(row)?;
        if row_key == key && found.replace(index).is_some() {
            return Err(OrmError::DuplicatePrimaryKey {
                table: T::TABLE,
                key: key.to_vec(),
            });
        }
    }
    Ok(found)
}

fn selected_raw_row<'a, T: VersionedRow>(
    rows: &'a [VcRow],
    key: &[VcValue],
) -> OrmResult<Option<&'a VcRow>> {
    let index = selected_row_index::<T>(rows, key)?;
    Ok(index.map(|index| &rows[index]))
}

fn typed_selected_row<T: VersionedRow>(rows: &[VcRow], key: &[VcValue]) -> OrmResult<Option<T>> {
    selected_raw_row::<T>(rows, key)?
        .map(decode_row::<T>)
        .transpose()
}

fn selected_commit_snapshot<T: VersionedRow>(
    base: Option<&TableSnapshot>,
    working: Option<&TableSnapshot>,
    key: &[VcValue],
) -> OrmResult<TableSnapshot> {
    let working = working.ok_or_else(|| OrmError::TableNotFound(T::TABLE.into()))?;
    validate_snapshot::<T>(working)?;
    if let Some(base) = base {
        validate_snapshot::<T>(base)?;
    }

    let base_row = base
        .map(|snapshot| selected_raw_row::<T>(&snapshot.rows, key))
        .transpose()?
        .flatten()
        .cloned();
    let working_row = selected_raw_row::<T>(&working.rows, key)?.cloned();
    if base_row == working_row {
        return Err(VersionError::NothingToCommit.into());
    }

    let mut selected = match base {
        Some(snapshot) => snapshot.clone(),
        None => TableSnapshot {
            columns: working.columns.clone(),
            pk: working.pk.clone(),
            rows: Vec::new(),
            schema_sql: working.schema_sql.clone(),
        },
    };
    let index = selected_row_index::<T>(&selected.rows, key)?;
    match (index, working_row) {
        (Some(index), Some(row)) => selected.rows[index] = row,
        (Some(index), None) => {
            selected.rows.remove(index);
        }
        (None, Some(row)) => selected.rows.push(row),
        (None, None) => return Err(VersionError::NothingToCommit.into()),
    }
    Ok(selected)
}

/// Transaction that holds the database mutably until commit or rollback.
/// The borrow checker ensures only one transaction exists at a time.
pub struct Transaction<'a> {
    db: &'a mut VersionedDb,
    previous: Option<VcStore>,
}

impl<'a> Transaction<'a> {
    pub fn begin(db: &'a mut VersionedDb) -> Self {
        let previous = db.store.clone();
        Self {
            db,
            previous: Some(previous),
        }
    }

    pub fn table<T: VersionedRow>(&mut self) -> TableHandle<'_, T> {
        TableHandle {
            db: self.db,
            _marker: PhantomData,
        }
    }

    pub fn commit(mut self, msg: &str) -> OrmResult<CommitId> {
        let commit = self.db.commit_all(msg)?;
        self.previous = None;
        Ok(commit)
    }
}

impl<'a> Drop for Transaction<'a> {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            self.db.store = previous;
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Membership {
        user_id: i64,
        role: String,
        org_id: i64,
    }

    impl VersionedRow for Membership {
        const TABLE: &'static str = "memberships";
        const COLUMNS: &'static [&'static str] = &["user_id", "role", "org_id"];
        const PK: &'static [&'static str] = &["org_id", "user_id"];

        fn into_row(&self) -> VcRow {
            VcRow::new(vec![
                VcValue::Integer(self.user_id),
                VcValue::Text(self.role.clone()),
                VcValue::Integer(self.org_id),
            ])
        }

        fn from_row(row: &VcRow) -> Result<Self, OrmError> {
            match row.values.as_slice() {
                [VcValue::Integer(user_id), VcValue::Text(role), VcValue::Integer(org_id)] => {
                    Ok(Self {
                        user_id: *user_id,
                        role: role.clone(),
                        org_id: *org_id,
                    })
                }
                _ => Err(OrmError::Decode("invalid membership row".into())),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct NoPrimaryKey {
        value: String,
    }

    impl VersionedRow for NoPrimaryKey {
        const TABLE: &'static str = "no_primary_key";
        const COLUMNS: &'static [&'static str] = &["value"];
        const PK: &'static [&'static str] = &[];

        fn into_row(&self) -> VcRow {
            VcRow::new(vec![VcValue::Text(self.value.clone())])
        }

        fn from_row(row: &VcRow) -> Result<Self, OrmError> {
            match row.values.as_slice() {
                [VcValue::Text(value)] => Ok(Self {
                    value: value.clone(),
                }),
                _ => Err(OrmError::Decode("invalid no-primary-key row".into())),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct NullableIdentity {
        id: Option<i64>,
        value: String,
    }

    impl VersionedRow for NullableIdentity {
        const TABLE: &'static str = "nullable_identity";
        const COLUMNS: &'static [&'static str] = &["id", "value"];
        const PK: &'static [&'static str] = &["id"];

        fn into_row(&self) -> VcRow {
            VcRow::new(vec![
                self.id.map_or(VcValue::Null, VcValue::Integer),
                VcValue::Text(self.value.clone()),
            ])
        }

        fn from_row(row: &VcRow) -> Result<Self, OrmError> {
            match row.values.as_slice() {
                [id, VcValue::Text(value)] => Ok(Self {
                    id: match id {
                        VcValue::Null => None,
                        VcValue::Integer(id) => Some(*id),
                        _ => return Err(OrmError::Decode("invalid nullable id".into())),
                    },
                    value: value.clone(),
                }),
                _ => Err(OrmError::Decode("invalid nullable row".into())),
            }
        }
    }

    std::thread_local! {
        static COUNTED_USER_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct CountedUser {
        id: i64,
        name: String,
    }

    impl VersionedRow for CountedUser {
        const TABLE: &'static str = "counted_users";
        const COLUMNS: &'static [&'static str] = &["id", "name"];
        const PK: &'static [&'static str] = &["id"];

        fn into_row(&self) -> VcRow {
            VcRow::new(vec![
                VcValue::Integer(self.id),
                VcValue::Text(self.name.clone()),
            ])
        }

        fn from_row(row: &VcRow) -> Result<Self, OrmError> {
            COUNTED_USER_DECODES.with(|count| count.set(count.get() + 1));
            match row.values.as_slice() {
                [VcValue::Integer(id), VcValue::Text(name)] => Ok(Self {
                    id: *id,
                    name: name.clone(),
                }),
                _ => Err(OrmError::Decode("invalid counted user row".into())),
            }
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
        let u = db.table::<User>().get(&[VcValue::Integer(1)]).unwrap();
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
    fn orm_branch_handle_checks_out_and_loads_the_selected_branch() {
        let mut db = db();
        db.table::<User>()
            .version(
                &User {
                    id: 1,
                    name: "Ada".into(),
                },
                "seed main",
            )
            .unwrap();
        db.create_branch(BranchName::borrowed("feature").unwrap())
            .unwrap();
        {
            let mut br = db
                .branch_handle(BranchName::borrowed("feature").unwrap())
                .unwrap();
            br.table::<Post>()
                .version(
                    &Post {
                        id: 1,
                        title: "feat".into(),
                    },
                    "add feature post",
                )
                .unwrap();
            assert_eq!(br.name(), "feature");
        }
        assert_eq!(db.branch_name(), Some("feature"));

        db.checkout(BranchName::borrowed("main").unwrap()).unwrap();
        assert_eq!(
            db.table::<Post>()
                .row(&[VcValue::Integer(1)])
                .unwrap()
                .current()
                .unwrap(),
            None
        );
        db.checkout(BranchName::borrowed("feature").unwrap())
            .unwrap();
        assert_eq!(
            db.table::<Post>()
                .row(&[VcValue::Integer(1)])
                .unwrap()
                .current()
                .unwrap(),
            Some(Post {
                id: 1,
                title: "feat".into(),
            })
        );
    }

    #[test]
    fn orm_checkout_refuses_to_discard_uncommitted_rows() {
        let mut db = db();
        db.create_branch(BranchName::borrowed("feature").unwrap())
            .unwrap();
        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada".into(),
            })
            .unwrap();

        assert_eq!(
            db.checkout(BranchName::borrowed("feature").unwrap()),
            Err(OrmError::CheckoutUncommittedChanges)
        );
        assert_eq!(db.branch_name(), Some("main"));
        assert_eq!(db.table::<User>().len(), 1);
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

    #[test]
    fn orm_uses_declared_composite_primary_key_order() {
        let mut db = db();
        db.table::<Membership>()
            .insert(&Membership {
                user_id: 7,
                role: "reader".into(),
                org_id: 42,
            })
            .unwrap();
        db.table::<Membership>()
            .insert(&Membership {
                user_id: 7,
                role: "owner".into(),
                org_id: 42,
            })
            .unwrap();

        let key = [VcValue::Integer(42), VcValue::Integer(7)];
        let membership = db.table::<Membership>().get(&key).unwrap();
        assert_eq!(membership.role, "owner");
        assert_eq!(db.table::<Membership>().len(), 1);
        assert!(db
            .table::<Membership>()
            .get(&[VcValue::Integer(42)])
            .is_none());
        assert!(db.table::<Membership>().delete(&key));
        assert!(db.table::<Membership>().is_empty());
    }

    #[test]
    fn orm_row_commit_versions_only_the_selected_row() {
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
        let initial = db.commit_all("initial").unwrap();

        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada Lovelace".into(),
            })
            .unwrap();
        db.table::<User>()
            .insert(&User {
                id: 2,
                name: "Robert".into(),
            })
            .unwrap();

        let committed = {
            let mut ada = db.table::<User>().row(&[VcValue::Integer(1)]).unwrap();
            assert_eq!(
                ada.current().unwrap(),
                Some(User {
                    id: 1,
                    name: "Ada Lovelace".into(),
                })
            );
            assert_eq!(
                ada.at(initial).unwrap(),
                Some(User {
                    id: 1,
                    name: "Ada".into(),
                })
            );
            ada.commit("version Ada only").unwrap()
        };

        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(1)])
                .unwrap()
                .at(committed)
                .unwrap(),
            Some(User {
                id: 1,
                name: "Ada Lovelace".into(),
            })
        );
        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(2)])
                .unwrap()
                .at(committed)
                .unwrap(),
            Some(User {
                id: 2,
                name: "Bob".into(),
            })
        );
        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(2)])
                .unwrap()
                .current()
                .unwrap(),
            Some(User {
                id: 2,
                name: "Robert".into(),
            })
        );
        assert_eq!(db.store().working_tables(), vec!["users"]);
    }

    #[test]
    fn orm_row_commit_keeps_other_staged_and_working_changes_after_failure() {
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
        db.table::<Post>()
            .insert(&Post {
                id: 1,
                title: "Before".into(),
            })
            .unwrap();
        let initial = db.commit_all("initial").unwrap();

        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada changed".into(),
            })
            .unwrap();
        db.table::<User>()
            .insert(&User {
                id: 2,
                name: "Bob changed".into(),
            })
            .unwrap();
        db.table::<Post>()
            .insert(&Post {
                id: 1,
                title: "Staged elsewhere".into(),
            })
            .unwrap();
        db.table::<Post>().stage().unwrap();
        db.store_mut().config.remove("user.name");

        {
            let mut ada = db.table::<User>().row(&[VcValue::Integer(1)]).unwrap();
            assert!(matches!(ada.commit("must fail"), Err(OrmError::Version(_))));
        }

        assert_eq!(db.head(), Some(initial));
        assert_eq!(db.store().staged_tables(), vec!["posts"]);
        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(1)])
                .unwrap()
                .current()
                .unwrap(),
            Some(User {
                id: 1,
                name: "Ada changed".into(),
            })
        );
        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(2)])
                .unwrap()
                .current()
                .unwrap(),
            Some(User {
                id: 2,
                name: "Bob changed".into(),
            })
        );
        assert_eq!(
            db.table::<Post>()
                .row(&[VcValue::Integer(1)])
                .unwrap()
                .current()
                .unwrap(),
            Some(Post {
                id: 1,
                title: "Staged elsewhere".into(),
            })
        );

        db.store_mut().config_set("user.name", "Ada");
        let committed = db
            .table::<User>()
            .row(&[VcValue::Integer(1)])
            .unwrap()
            .commit("Ada only")
            .unwrap();
        assert_eq!(
            db.table::<Post>()
                .row(&[VcValue::Integer(1)])
                .unwrap()
                .at(committed)
                .unwrap(),
            Some(Post {
                id: 1,
                title: "Before".into(),
            })
        );
        assert_eq!(db.store().staged_tables(), vec!["posts"]);
    }

    #[test]
    fn orm_row_delete_commits_only_the_selected_deletion() {
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
        db.commit_all("initial").unwrap();
        db.table::<User>()
            .insert(&User {
                id: 2,
                name: "Robert".into(),
            })
            .unwrap();

        let deleted = db
            .table::<User>()
            .version_delete(&[VcValue::Integer(1)], "remove Ada only")
            .unwrap();

        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(1)])
                .unwrap()
                .at(deleted)
                .unwrap(),
            None
        );
        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(2)])
                .unwrap()
                .at(deleted)
                .unwrap(),
            Some(User {
                id: 2,
                name: "Bob".into(),
            })
        );
        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(2)])
                .unwrap()
                .current()
                .unwrap(),
            Some(User {
                id: 2,
                name: "Robert".into(),
            })
        );
    }

    #[test]
    fn orm_row_handle_uses_declared_composite_key_and_reports_bad_keys() {
        let mut db = db();
        db.table::<Membership>()
            .insert(&Membership {
                user_id: 7,
                role: "reader".into(),
                org_id: 42,
            })
            .unwrap();
        db.commit_all("initial").unwrap();

        let committed = {
            let mut membership = db
                .table::<Membership>()
                .row(&[VcValue::Integer(42), VcValue::Integer(7)])
                .unwrap();
            membership
                .update(&Membership {
                    user_id: 7,
                    role: "owner".into(),
                    org_id: 42,
                })
                .unwrap();
            membership.commit("promote member").unwrap()
        };
        assert_eq!(
            db.table::<Membership>()
                .row(&[VcValue::Integer(42), VcValue::Integer(7)])
                .unwrap()
                .at(committed)
                .unwrap(),
            Some(Membership {
                user_id: 7,
                role: "owner".into(),
                org_id: 42,
            })
        );
        assert!(matches!(
            db.table::<Membership>().row(&[VcValue::Integer(42)]),
            Err(OrmError::PrimaryKeyArity { .. })
        ));
        assert_eq!(
            db.table::<Membership>()
                .row(&[VcValue::Integer(42), VcValue::Text("7".into())])
                .unwrap()
                .current()
                .unwrap(),
            None
        );
        assert!(matches!(
            db.table::<NoPrimaryKey>()
                .row(&[VcValue::Text("nope".into())]),
            Err(OrmError::PrimaryKeyNotDeclared { .. })
        ));
        assert!(matches!(
            NoPrimaryKey {
                value: "nope".into()
            }
            .pk_values(),
            Err(OrmError::PrimaryKeyNotDeclared { .. })
        ));
    }

    #[test]
    fn orm_row_handle_surfaces_malformed_rows_and_missing_deletions() {
        let mut db = db();
        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada".into(),
            })
            .unwrap();
        let snapshot = db.store().work_table(User::TABLE).unwrap().clone();
        db.store_mut().apply_work(
            User::TABLE,
            snapshot.columns,
            snapshot.pk,
            vec![VcRow::new(vec![
                VcValue::Text("not an integer id".into()),
                VcValue::Text("Ada".into()),
            ])],
            snapshot.schema_sql,
        );

        let mut selected = db.table::<User>().row(&[VcValue::Integer(1)]).unwrap();
        assert!(matches!(selected.current(), Err(OrmError::Decode(_))));
        assert!(matches!(selected.delete(), Err(OrmError::Decode(_))));
    }

    #[test]
    fn orm_row_history_records_updates_and_deletions() {
        let mut db = db();
        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada".into(),
            })
            .unwrap();
        let created = db.commit_all("create Ada").unwrap();

        let updated = {
            let mut ada = db.table::<User>().row(&[VcValue::Integer(1)]).unwrap();
            ada.update(&User {
                id: 1,
                name: "Ada Lovelace".into(),
            })
            .unwrap();
            ada.commit("rename Ada").unwrap()
        };
        let deleted = {
            let mut ada = db.table::<User>().row(&[VcValue::Integer(1)]).unwrap();
            ada.delete().unwrap();
            ada.commit("remove Ada").unwrap()
        };

        let history = db
            .table::<User>()
            .row(&[VcValue::Integer(1)])
            .unwrap()
            .history()
            .unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].revision, deleted);
        assert_eq!(history[0].value, None);
        assert_eq!(history[1].revision, updated);
        assert_eq!(
            history[1].value,
            Some(User {
                id: 1,
                name: "Ada Lovelace".into(),
            })
        );
        assert_eq!(history[2].revision, created);
    }

    #[test]
    fn orm_transaction_rolls_back_when_commit_fails() {
        let mut db = VersionedDb::new("main").unwrap();
        let result = {
            let mut tx = Transaction::begin(&mut db);
            tx.table::<User>()
                .insert(&User {
                    id: 1,
                    name: "Ada".into(),
                })
                .unwrap();
            tx.commit("no author")
        };

        assert!(matches!(result, Err(OrmError::Version(_))));
        assert!(db.table::<User>().is_empty());
        assert!(db.store().tables().is_empty());
        assert!(db.store().working_tables().is_empty());
        assert!(db.store().staged_tables().is_empty());
    }

    #[test]
    fn orm_row_writes_refuse_a_detached_snapshot() {
        let mut db = db();
        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada".into(),
            })
            .unwrap();
        let initial = db.commit_all("initial").unwrap();
        db.store_mut().open_detached(initial);

        assert!(matches!(
            db.table::<User>().insert(&User {
                id: 2,
                name: "Bob".into(),
            }),
            Err(OrmError::Version(_))
        ));
        {
            let mut ada = db.table::<User>().row(&[VcValue::Integer(1)]).unwrap();
            assert!(matches!(
                ada.update(&User {
                    id: 1,
                    name: "Ada changed".into(),
                }),
                Err(OrmError::Version(_))
            ));
            assert!(matches!(ada.delete(), Err(OrmError::Version(_))));
            assert!(matches!(ada.commit("must fail"), Err(OrmError::Version(_))));
        }
        assert_eq!(db.head(), Some(initial));
        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(1)])
                .unwrap()
                .current()
                .unwrap(),
            Some(User {
                id: 1,
                name: "Ada".into(),
            })
        );
    }

    #[test]
    fn orm_version_is_the_short_path_for_one_row() {
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
        db.commit_all("initial").unwrap();
        db.table::<User>()
            .insert(&User {
                id: 2,
                name: "Robert".into(),
            })
            .unwrap();

        let commit = db
            .table::<User>()
            .version(
                &User {
                    id: 1,
                    name: "Ada Lovelace".into(),
                },
                "version Ada",
            )
            .unwrap();

        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(1)])
                .unwrap()
                .at(commit)
                .unwrap()
                .unwrap()
                .name,
            "Ada Lovelace"
        );
        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(2)])
                .unwrap()
                .at(commit)
                .unwrap()
                .unwrap()
                .name,
            "Bob"
        );
        assert_eq!(
            db.table::<User>().get(&[VcValue::Integer(2)]).unwrap().name,
            "Robert"
        );
        assert_eq!(db.store().working_tables(), vec!["users"]);
    }

    #[test]
    fn orm_row_commit_preserves_same_table_staging() {
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
        db.commit_all("initial").unwrap();
        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada Lovelace".into(),
            })
            .unwrap();
        db.table::<User>()
            .insert(&User {
                id: 2,
                name: "Robert".into(),
            })
            .unwrap();
        db.table::<User>().stage().unwrap();

        db.table::<User>()
            .row(&[VcValue::Integer(1)])
            .unwrap()
            .commit("Ada only")
            .unwrap();

        assert_eq!(db.store().staged_tables(), vec!["users"]);
        let rest = db.commit("remaining staged row").unwrap();
        assert_eq!(
            db.table::<User>()
                .row(&[VcValue::Integer(2)])
                .unwrap()
                .at(rest)
                .unwrap()
                .unwrap()
                .name,
            "Robert"
        );
    }

    #[test]
    fn orm_row_commit_does_not_commit_an_unrelated_table_drop() {
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
                title: "Keep for now".into(),
            })
            .unwrap();
        db.commit_all("initial").unwrap();
        db.store_mut().drop_table(Post::TABLE).unwrap();

        let row_commit = db
            .table::<User>()
            .version(
                &User {
                    id: 1,
                    name: "Ada Lovelace".into(),
                },
                "Ada only",
            )
            .unwrap();
        assert!(db.store().table_rows(Post::TABLE, &row_commit).is_some());

        db.store_mut().dolt_add(&[Post::TABLE]).unwrap();
        let drop_commit = db.commit("drop post table").unwrap();
        assert!(db.store().table_rows(Post::TABLE, &drop_commit).is_none());
    }

    #[test]
    fn orm_row_selection_supports_null_primary_key_cells() {
        let mut db = db();
        let commit = db
            .table::<NullableIdentity>()
            .version(
                &NullableIdentity {
                    id: None,
                    value: "first".into(),
                },
                "nullable identity",
            )
            .unwrap();
        assert_eq!(
            db.table::<NullableIdentity>()
                .row(&[VcValue::Null])
                .unwrap()
                .at(commit)
                .unwrap(),
            Some(NullableIdentity {
                id: None,
                value: "first".into(),
            })
        );
    }

    #[test]
    fn orm_version_failure_is_atomic() {
        let mut db = VersionedDb::new("main").unwrap();
        let result = db.table::<User>().version(
            &User {
                id: 1,
                name: "Ada".into(),
            },
            "missing author",
        );
        assert!(matches!(result, Err(OrmError::Version(_))));
        assert!(db.store().tables().is_empty());
        assert!(db.store().work_snapshots().is_empty());
        assert_eq!(db.head(), None);
    }

    #[test]
    fn orm_insert_refuses_a_model_schema_mismatch_without_mutation() {
        let mut db = db();
        db.store_mut().apply_work(
            User::TABLE,
            vec!["id".into(), "alias".into()],
            vec!["id".into()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("Ada".into()),
            ])],
            "CREATE TABLE users (id INTEGER PRIMARY KEY, alias TEXT)".into(),
        );
        let before = db.store().work_table(User::TABLE).unwrap().clone();

        let result = db.table::<User>().insert(&User {
            id: 2,
            name: "Bob".into(),
        });

        assert!(matches!(result, Err(OrmError::Decode(_))));
        assert_eq!(db.store().work_table(User::TABLE), Some(&before));
        assert!(db.store().tables().is_empty());
    }

    #[test]
    fn orm_transaction_rolls_back_nested_row_commit() {
        let mut db = db();
        db.table::<User>()
            .insert(&User {
                id: 1,
                name: "Ada".into(),
            })
            .unwrap();
        let initial = db.commit_all("initial").unwrap();
        {
            let mut transaction = Transaction::begin(&mut db);
            transaction
                .table::<User>()
                .version(
                    &User {
                        id: 1,
                        name: "temporary".into(),
                    },
                    "temporary commit",
                )
                .unwrap();
        }

        assert_eq!(db.head(), Some(initial));
        assert_eq!(
            db.table::<User>().get(&[VcValue::Integer(1)]).unwrap().name,
            "Ada"
        );
    }

    #[test]
    fn orm_version_index_finds_exact_historical_values() {
        let mut db = db();
        let ada = db
            .table::<User>()
            .version(
                &User {
                    id: 1,
                    name: "Ada".into(),
                },
                "add Ada",
            )
            .unwrap();
        db.table::<User>()
            .version(
                &User {
                    id: 1,
                    name: "Augusta".into(),
                },
                "rename Ada",
            )
            .unwrap();
        let second_ada = db
            .table::<User>()
            .version(
                &User {
                    id: 2,
                    name: "Ada".into(),
                },
                "add another Ada",
            )
            .unwrap();

        let index = db
            .table::<User>()
            .version_index_by(|user| user.name.clone())
            .unwrap();
        let matches = index.get("Ada");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].revision(), second_ada);
        assert_eq!(matches[0].key().values(), &[VcValue::Integer(2)]);
        assert_eq!(matches[0].row_ordinal(), Some(1));
        assert_eq!(
            VersionPointer::<User>::from_values(&matches[0].to_values()).unwrap(),
            matches[0]
        );
        assert_eq!(matches[1].revision(), ada);
        assert_eq!(matches[1].key().values(), &[VcValue::Integer(1)]);
        assert!(index.get("missing").is_empty());

        assert_eq!(
            matches[1].load(&db).unwrap(),
            Some(User {
                id: 1,
                name: "Ada".into(),
            })
        );
    }

    #[test]
    fn orm_version_pointer_round_trips_through_index_table_values() {
        let key = RowKey::<User>::new([VcValue::Integer(7)]).unwrap();
        let pointer = VersionPointer::new(CommitId([0x42; 20]), key);

        let encoded = pointer.to_values();
        let decoded = VersionPointer::<User>::from_values(&encoded).unwrap();

        assert_eq!(decoded, pointer);
        assert!(matches!(
            VersionPointer::<User>::from_values(&[]),
            Err(OrmError::InvalidVersionPointer { .. })
        ));
        assert!(matches!(
            VersionPointer::<User>::from_values(
                &[VcValue::Blob(vec![0; 19]), VcValue::Integer(7),]
            ),
            Err(OrmError::InvalidVersionPointer { .. })
        ));
    }

    #[test]
    fn orm_version_index_skips_commits_that_did_not_change_the_row() {
        let mut db = db();
        db.table::<User>()
            .version(
                &User {
                    id: 1,
                    name: "Ada".into(),
                },
                "add Ada",
            )
            .unwrap();
        db.table::<Post>()
            .version(
                &Post {
                    id: 1,
                    title: "Unrelated".into(),
                },
                "add post",
            )
            .unwrap();
        let calls = std::cell::Cell::new(0);

        let index = db
            .table::<User>()
            .version_index_by(|user| {
                calls.set(calls.get() + 1);
                user.name.clone()
            })
            .unwrap();

        assert_eq!(calls.get(), 1);
        for _ in 0..100 {
            assert_eq!(index.get("Ada").len(), 1);
        }
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn orm_index_pointer_load_decodes_only_the_target_row() {
        let mut db = db();
        for id in 0..64 {
            db.table::<CountedUser>()
                .insert(&CountedUser {
                    id,
                    name: format!("user-{id}"),
                })
                .unwrap();
        }
        db.commit_all("seed counted users").unwrap();
        let index = db
            .table::<CountedUser>()
            .version_index_by(|user| user.name.clone())
            .unwrap();
        let pointer = &index.get("user-37")[0];
        COUNTED_USER_DECODES.with(|count| count.set(0));

        assert_eq!(pointer.load(&db).unwrap().unwrap().id, 37);
        assert_eq!(COUNTED_USER_DECODES.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn orm_index_pointer_verifies_its_ordinal_before_loading() {
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
        let revision = db.commit_all("seed users").unwrap();
        let index = db
            .table::<User>()
            .version_index_by(|user| user.name.clone())
            .unwrap();
        let pointer = index.get("Bob")[0].clone();
        db.store
            .snapshots
            .get_mut(&revision)
            .unwrap()
            .get_mut(User::TABLE)
            .unwrap()
            .rows
            .swap(0, 1);

        assert_eq!(
            pointer.load(&db).unwrap(),
            Some(User {
                id: 2,
                name: "Bob".into(),
            })
        );
    }

    #[test]
    fn orm_version_index_stops_at_a_detached_snapshot() {
        let mut db = db();
        let first = db
            .table::<User>()
            .version(
                &User {
                    id: 1,
                    name: "Ada".into(),
                },
                "add Ada",
            )
            .unwrap();
        db.table::<User>()
            .version(
                &User {
                    id: 1,
                    name: "Augusta".into(),
                },
                "rename Ada",
            )
            .unwrap();
        db.store_mut().open_detached(first);

        let index = db
            .table::<User>()
            .version_index_by(|user| user.name.clone())
            .unwrap();

        assert_eq!(index.head(), Some(first));
        assert_eq!(index.get("Ada").len(), 1);
        assert!(index.get("Augusta").is_empty());
    }
}
