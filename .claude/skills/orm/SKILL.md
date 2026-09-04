# ORM Skill — typed, borrow-checked, mem-efficient

Use `extensions/versioning/src/orm.rs` for all typed interactions. Never use stringly
`VcStore` calls directly in new code.

## Types

- `BranchName<'a>(Cow<'a, str>)` — validated, borrowed `&str` never clones unless owned.
- `TableName<'a>` — same.
- `VersionedRow` trait — `TABLE`, `COLUMNS`, `PK` consts, `into_row(&self) -> VcRow`, `from_row(&VcRow)`, and checked `pk_values()`.
- `VersionedDb { store: VcStore }` — owns store, all handles borrow `&mut Self` so the compiler forbids aliased mut table handles.
- `BranchHandle<'a>` — checks out its named branch, loads that branch's committed
  rows, and borrows `&'a mut VersionedDb` so another branch cannot be used at the
  same time. Switching branches with pending changes is rejected.
- `TableHandle<'a, T>` — `PhantomData<T>`, checked `row(pk)`, one-call `version(value, message)` / `version_delete(pk, message)`, plus the table-wide insert/read/stage/diff helpers.
- `RowKey<T>` — validated key values in declared `T::PK` order. The model marker prevents using one table's key with another table.
- `RowHandle<'a, T>` — checked `current`, `at`, `history`, `update`, `delete`, and selective `commit`. A selective commit starts from HEAD and overlays only this key, leaving every other working or staged change pending.
- `VersionIndex<T, K>` — an owned B-tree index over changed row images in the
  active branch's first-parent history. Build it with `version_index_by`; exact
  queries are `O(log distinct_values + matches)` and return newest-first typed
  pointers.
- `VersionPointer<T>` — a `(CommitId, RowKey<T>)` pointer with a verified row
  ordinal for normal `O(1)` loading. `to_values`/`from_values` encode it as a
  20-byte commit BLOB, ordinal, and PK cells so another versioned table can
  persist it as a secondary-index entry. The cells start with a format version
  so future pointer formats can be rejected safely.
- `Transaction<'a>` — snapshots the full `VcStore` at `begin` and restores it on `Drop` or failed `commit()`. Only one exists at a time.

## Data structures

- `BTreeMap<Vec<VcValue>, &VcRow>` for diff — O(n) merge, deterministic order, no hashing.
- `Vec::with_capacity(INLINE_CAP)` where `INLINE_CAP=8` for small tables.
- Iterators clone a stable `Vec<VcRow>` snapshot and decode its rows lazily.

## Borrow rules

- Every handle holds `&'a mut VersionedDb`; creating a second handle requires the first to be dropped.
- `Cow<'a, str>` for names: `BranchName::borrowed(&str)` is zero-copy; `into_owned()` moves to `'static` only when stored.
- `insert(&mut self, &T)` takes `&T`, clones only the needed `VcValue`s into `VcRow`; `get(&self, &[VcValue])` borrows PK slice.

## Perf

- `iter()` and `iter_at()` clone `Vec<VcRow>` once per call, then decode lazily.
- `diff_rows` uses `BTreeMap` not `HashMap` to avoid randomization and keep `O(n log n)` with sorted PK.
- `version_index_by` scans and decodes history once. It omits commits where a
  row did not change; reuse the returned index for all lookups. Pointer loads
  use the stored row ordinal, verify the PK, and fall back to a checked scan if
  transport changed snapshot row order.
- Selective row commits and `Transaction` use a full `VcStore` clone for failure atomicity. This API favors correctness; use the SQL transaction path for large production working sets until the store has copy-on-write snapshots.

## Example

```rust
struct User { id: i64, name: String }
impl VersionedRow for User { ... }

let mut db = VersionedDb::new("main").unwrap();
db.store_mut().config_set("user.name", "Ada");
db.store_mut().config_set("user.email", "ada@example.com");
let commit = db.table::<User>()
    .version(&User { id: 1, name: "Ada".into() }, "add Ada")?;
let u = db.table::<User>().get(&[VcValue::Integer(1)]).unwrap();
let versions = db.table::<User>()
    .row(&[VcValue::Integer(1)])?
    .history()?;
let names = db.table::<User>()
    .version_index_by(|user| user.name.clone())?;
for pointer in names.get("Ada") {
    let exact_historical_user = pointer.load(&db)?;
    let index_table_cells = pointer.to_values();
}
```
