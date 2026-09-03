# Merge Semantics Skill

Three-way row merge, schema predetect/rebuild, transient conflict management,
constraint detectors, cherry-pick/revert/rebase drivers, merge_status, and
per-table column-aware modules. All in `extensions/versioning/`.

## Key Files

| File | Purpose |
|------|---------|
| `merge.rs` | Three-way row merge: fast path, cell rules, no-PK, multi-col PK |
| `merge_schema.rs` | CREATE TABLE parser, classify/same_schema/union_columns, rebuild |
| `conflicts.rs` | Transient conflicts: ours/theirs resolution, schema conflict entries |
| `constraints.rs` | FK, unique, check, not-null, strict detectors, eval_check |
| `replay.rs` | merge_branch, cherry_pick, revert, rebase_onto/plan/continue/abort |
| `staging.rs` | Working/staged sets, commit gates, snapshots, dirty_work tracking |

## Schema Merge Matrix (merge_schema.rs)

`merge_schema(base, ours, theirs) → SchemaDecision`:
- Both present: classify each side against base (Unchanged/Compatible/Breaking),
  apply the matrix. PK-signature rule: both sides changed PK differently → conflict.
- One side deleted (base existed): always a conflict. Distinguish whether the
  surviving side modified the table from base:
  - `"table deleted on ours, unchanged on theirs"` / `"table deleted on ours, modified on theirs"`
  - `"table deleted on theirs, unchanged on ours"` / `"table deleted on theirs, modified on ours"`
  If the surviving side is absent too: `"table deleted on ours, absent on theirs"`.
  The user resolves by --ours/--theirs.
- Neither present (both deleted): conflict.

CHECK constraints: compatible additions (CHECK added) are merged via union_columns.
Differing CHECK additions conflict. Constraint removal is breaking.

## Row Merge Rules (merge.rs)

- Fast path: theirs == base → keep ours; ours == base → take theirs.
- Cell rule: changed on one side → take it; same change both sides → take it;
  different changes → conflict.
- Row-level: added on one side only → keep; add/add same PK → conflict;
  delete/modify → conflict; delete/delete → gone.

## Transient Conflicts (conflicts.rs)

Conflicts live in `VcStore` memory only. Never written into commits. COMMIT
refuses while any exist. Resolved by `--ours`/`--theirs` per-row.

## Write-Back Atomicity (versioning.rs glue)

`sync_work_to_sql` wraps all table writes in an outer SQL SAVEPOINT. If any
table write fails, the SAVEPOINT rolls back every table. On store-side failure
the pending-resolve entries and dirty flags are restored so a retry can
re-attempt the full write-back (the SQL tables were rolled back to match the
store's working set).

## Testing

- Unit: `merge_schema::tests`, `merge::tests`, `replay::tests`, `conflicts::tests`
- Integration: `tests/merge_conflicts.rs`, `tests/replay.rs`, `tests/writeback_atomicity.rs`
- Sqltests: `vc_merge_basic`, `vc_conflicts_basic`, `vc_replay_basic`, `vc_vtabs_basic`

Run: `cargo test -p turso_versioning` and
`make -C sqlite/conformance run-rust ARGS='--snapshot-filter __never__'`
