//! Transient merge conflicts and `--ours`/`--theirs` resolution.
//!
//! Conflicts live only in the `VcStore` working memory: they are never encoded
//! into commits, never written to the chunk store, and `dolt_commit` refuses
//! while any remain (`cannot commit: unresolved merge conflicts`). Resolution
//! drops an entry and hands the chosen side's row image to the caller so the
//! SQL glue can apply it to the working tables.

use crate::model::{VersionError, VersionResult};
use crate::staging::{TableSnapshot, VcStore};
use crate::vtab_log::{VcRow, VcValue};

/// Why a row or table is in conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    Rows,
    Schema(String),
}

/// One conflict row, exactly as `merge_rows.c` records base/ours/theirs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictEntry {
    pub table: String,
    pub pk: Vec<VcValue>,
    pub base: Option<VcRow>,
    pub ours: Option<VcRow>,
    pub theirs: Option<VcRow>,
    pub kind: ConflictKind,
    /// Full table image from each side of a schema conflict, so resolving
    /// `--ours`/`--theirs` can drop that side's whole table into the working
    /// set. `None` for row conflicts.
    pub ours_schema: Option<TableSnapshot>,
    pub theirs_schema: Option<TableSnapshot>,
}

/// Which side of a conflict wins a resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveSide {
    Ours,
    Theirs,
}

/// Resolve one conflict by taking a side. The chosen row image is recorded in
/// the store's pending-resolve map (keyed by table and pk) so the caller can
/// apply it to the working SQL tables, and the entry is dropped. Resolving the
/// last conflict unblocks commit.
pub fn resolve_conflict(
    store: &mut VcStore,
    table: &str,
    pk: &[VcValue],
    side: ResolveSide,
) -> VersionResult<()> {
    store.resolve_conflict(table, pk, side)
}

impl VcStore {
    /// Resolve a conflict, recording the applied row image for the glue.
    ///
    /// A missing entry is an error, not a silent no-op: silently ignoring a
    /// resolution would leave a phantom conflict that blocks commit forever.
    pub fn resolve_conflict(
        &mut self,
        table: &str,
        pk: &[VcValue],
        side: ResolveSide,
    ) -> VersionResult<()> {
        let pos = self
            .conflicts
            .iter()
            .position(|c| c.table == table && c.pk == pk)
            .ok_or_else(|| VersionError::NoConflict(table.to_string()))?;
        let entry = self.conflicts.remove(pos);
        let image = match side {
            ResolveSide::Ours => entry.ours.clone(),
            ResolveSide::Theirs => entry.theirs.clone(),
        };
        // The chosen side becomes the working content, so the SQL glue can
        // write it back and a follow-up commit records the resolution.
        self.apply_resolution(table, pk, &entry, side);
        self.dirty_work.insert(table.to_string());
        self.pending_resolve
            .entry(table.to_string())
            .or_default()
            .insert(pk.to_vec(), image);
        Ok(())
    }

    /// Apply a resolution to the working content: the chosen row replaces the
    /// conflict key, or the chosen side's whole table replaces a schema
    /// conflict. A paused rebase carries its own rebase working set, so the
    /// resolution lands there too.
    fn apply_resolution(
        &mut self,
        table: &str,
        pk: &[VcValue],
        entry: &ConflictEntry,
        side: ResolveSide,
    ) {
        match &entry.kind {
            ConflictKind::Rows => {
                let image = match side {
                    ResolveSide::Ours => entry.ours.clone(),
                    ResolveSide::Theirs => entry.theirs.clone(),
                };
                apply_row_to_work(&mut self.work, table, pk, image.clone());
                if let Some(st) = &mut self.rebase_state {
                    apply_row_to_work(&mut st.rebase_work, table, pk, image);
                }
            }
            ConflictKind::Schema(_) => {
                let snapshot = match side {
                    ResolveSide::Ours => entry.ours_schema.clone(),
                    ResolveSide::Theirs => entry.theirs_schema.clone(),
                };
                if let Some(snap) = snapshot {
                    self.work.insert(table.to_string(), snap.clone());
                    if let Some(st) = &mut self.rebase_state {
                        st.rebase_work.insert(table.to_string(), snap);
                    }
                }
            }
        }
    }

    /// The row images chosen by `resolve_conflict`, drained by the glue after
    /// it applies them to SQL.
    pub fn take_pending_resolve(&mut self) -> Vec<(String, Vec<VcValue>, Option<VcRow>)> {
        let mut out = Vec::new();
        let entries = std::mem::take(&mut self.pending_resolve);
        for (table, rows) in entries {
            for (pk, image) in rows {
                out.push((table.clone(), pk, image));
            }
        }
        out
    }

    /// Every recorded conflict, for the conflicts vtables.
    pub fn conflict_entries(&self) -> Vec<ConflictEntry> {
        self.conflicts.clone()
    }
}

/// Replace, insert, or delete one row of a table's working content by pk.
/// A `Some` image replaces the key; `None` deletes it.
fn apply_row_to_work(
    work: &mut std::collections::HashMap<String, TableSnapshot>,
    table: &str,
    pk: &[VcValue],
    image: Option<VcRow>,
) {
    let Some(snapshot) = work.get_mut(table) else {
        return;
    };
    let positions: Vec<usize> = snapshot
        .pk
        .iter()
        .filter_map(|name| snapshot.columns.iter().position(|c| c == name))
        .collect();
    let key_of = |row: &VcRow| {
        positions
            .iter()
            .filter_map(|i| row.values.get(*i).cloned())
            .collect::<Vec<_>>()
    };
    if let Some(image) = image {
        if let Some(slot) = snapshot.rows.iter_mut().find(|row| key_of(row) == pk) {
            *slot = image;
        } else {
            snapshot.rows.push(image);
        }
    } else {
        snapshot.rows.retain(|row| key_of(row) != pk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_store() -> VcStore {
        let mut s = VcStore::new("main");
        s.config_set("user.name", "Ada");
        s.config_set("user.email", "ada@example.com");
        s.track_table("t");
        s.dolt_add(&["t"]).unwrap();
        s.set_now(1);
        s.dolt_commit("seed", None, false, false).unwrap();
        s
    }

    fn entry(table: &str, pk: i64) -> ConflictEntry {
        ConflictEntry {
            table: table.to_string(),
            pk: vec![VcValue::Integer(pk)],
            base: Some(VcRow::new(vec![
                VcValue::Integer(pk),
                VcValue::Text("b".into()),
            ])),
            ours: Some(VcRow::new(vec![
                VcValue::Integer(pk),
                VcValue::Text("o".into()),
            ])),
            theirs: Some(VcRow::new(vec![
                VcValue::Integer(pk),
                VcValue::Text("t".into()),
            ])),
            kind: ConflictKind::Rows,
            ours_schema: None,
            theirs_schema: None,
        }
    }

    #[test]
    fn conflicts_are_transient_and_never_in_commits() {
        let mut s = configured_store();
        s.conflicts.push(entry("t", 1));
        // The conflict lives in working memory; committed snapshots never see it.
        let committed = s.committed_snapshot_tables();
        assert!(!committed.contains(&"t".to_string()));
        s.dolt_add(&["t"]).unwrap();
        s.set_now(2);
        assert_eq!(
            s.dolt_commit("x", None, false, false)
                .unwrap_err()
                .to_string(),
            "cannot commit: unresolved merge conflicts"
        );
    }

    #[test]
    fn resolve_ours_applies_ours_image_and_drops_entry() {
        let mut s = configured_store();
        s.conflicts.push(entry("t", 1));
        let side = ResolveSide::Ours;
        resolve_conflict(&mut s, "t", &[VcValue::Integer(1)], side).unwrap();
        assert!(s.conflicts.is_empty());
        let images = s.take_pending_resolve();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].0, "t");
        assert_eq!(images[0].1, vec![VcValue::Integer(1)]);
        assert_eq!(
            images[0].2.as_ref().unwrap().values[1],
            VcValue::Text("o".into())
        );
    }

    #[test]
    fn resolve_theirs_applies_theirs_image() {
        let mut s = configured_store();
        s.conflicts.push(entry("t", 1));
        resolve_conflict(&mut s, "t", &[VcValue::Integer(1)], ResolveSide::Theirs).unwrap();
        let images = s.take_pending_resolve();
        assert_eq!(
            images[0].2.as_ref().unwrap().values[1],
            VcValue::Text("t".into())
        );
    }

    #[test]
    fn resolve_deleted_side_records_a_delete() {
        let mut s = configured_store();
        let mut e = entry("t", 1);
        e.ours = None;
        s.conflicts.push(e);
        resolve_conflict(&mut s, "t", &[VcValue::Integer(1)], ResolveSide::Ours).unwrap();
        let images = s.take_pending_resolve();
        assert!(images[0].2.is_none());
    }

    #[test]
    fn resolving_last_conflict_unblocks_commit() {
        let mut s = configured_store();
        s.conflicts.push(entry("t", 1));
        s.conflicts.push(entry("t", 2));
        s.dolt_add(&["t"]).unwrap();
        s.set_now(2);
        assert_eq!(
            s.dolt_commit("x", None, false, false)
                .unwrap_err()
                .to_string(),
            "cannot commit: unresolved merge conflicts"
        );
        resolve_conflict(&mut s, "t", &[VcValue::Integer(1)], ResolveSide::Ours).unwrap();
        s.dolt_add(&["t"]).unwrap();
        s.set_now(3);
        assert_eq!(
            s.dolt_commit("x", None, false, false)
                .unwrap_err()
                .to_string(),
            "cannot commit: unresolved merge conflicts"
        );
        resolve_conflict(&mut s, "t", &[VcValue::Integer(2)], ResolveSide::Theirs).unwrap();
        s.dolt_add(&["t"]).unwrap();
        s.set_now(4);
        assert!(s.dolt_commit("x", None, false, false).is_ok());
    }

    #[test]
    fn resolve_missing_conflict_is_an_error() {
        let mut s = configured_store();
        let err =
            resolve_conflict(&mut s, "t", &[VcValue::Integer(1)], ResolveSide::Ours).unwrap_err();
        assert_eq!(err.to_string(), "no conflict found for table 't'");
    }

    #[test]
    fn schema_conflicts_are_recorded_with_detail() {
        let mut s = configured_store();
        s.conflicts.push(ConflictEntry {
            table: "t".to_string(),
            pk: Vec::new(),
            base: None,
            ours: None,
            theirs: None,
            kind: ConflictKind::Schema("primary key changed on both sides".to_string()),
            ours_schema: None,
            theirs_schema: None,
        });
        assert_eq!(s.conflict_entries().len(), 1);
        let entries = s.conflict_entries();
        assert_eq!(
            entries[0].kind,
            ConflictKind::Schema("primary key changed on both sides".to_string())
        );
    }
}
