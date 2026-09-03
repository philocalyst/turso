//! Three-way row merge: base/ours/theirs snapshots into merged rows.
//!
//! Mirrors doltlite's `merge_rows.c` pass over one table. Matching is
//! NAME-keyed at the caller (table name -> snapshot); here a row is keyed by
//! its primary-key cells. No-PK tables have no join key, so they merge by
//! full-row identity and every divergence is a conflict.

use std::collections::BTreeMap;

use crate::vtab_log::{VcRow, VcValue};

/// What happened to one row during the merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowOutcome {
    Unchanged,
    Ours,
    Theirs,
    Merged(VcRow),
    Conflict(ConflictRow),
}

/// The three sides of a conflicting row, plus the key that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRow {
    pub pk: Vec<VcValue>,
    pub base: Option<VcRow>,
    pub ours: Option<VcRow>,
    pub theirs: Option<VcRow>,
}

/// The merge result for one table: winning rows keyed by their pk, plus the
/// row-level conflicts. The conflict tuple's first field is the table name,
/// which this function does not know; the caller stamps it after the merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    pub rows: Vec<(Vec<VcValue>, VcRow)>,
    pub conflicts: Vec<(String, ConflictRow)>,
}

/// Merge one table's base/ours/theirs rows.
///
/// Fast paths run first: when one side matches base, the other wins whole,
/// with zero conflicts and no row rebuild. Otherwise every pk in the union of
/// the three sides is examined with the cell rule: a cell changed on one side
/// only is taken; changed identically on both is taken; changed differently on
/// both is a conflict. A row-level change on one side plus a delete on the
/// other is a conflict; a delete on both is a disappearance.
pub fn three_way_row_merge(
    base: &[VcRow],
    ours: &[VcRow],
    theirs: &[VcRow],
    pk: &[String],
    columns: &[String],
) -> MergeOutcome {
    if theirs == base {
        return MergeOutcome {
            rows: rows_keyed(ours, pk, columns),
            conflicts: Vec::new(),
        };
    }
    if ours == base {
        return MergeOutcome {
            rows: rows_keyed(theirs, pk, columns),
            conflicts: Vec::new(),
        };
    }
    if pk.is_empty() {
        return merge_no_pk(base, ours, theirs);
    }

    let base_map = key_rows(base, pk, columns);
    let ours_map = key_rows(ours, pk, columns);
    let theirs_map = key_rows(theirs, pk, columns);

    let mut keys: Vec<Vec<VcValue>> = base_map.keys().cloned().collect();
    for key in ours_map.keys().chain(theirs_map.keys()) {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    keys.sort();

    let mut rows = Vec::new();
    let mut conflicts = Vec::new();
    for key in keys {
        let b = base_map.get(&key).cloned();
        let o = ours_map.get(&key).cloned();
        let t = theirs_map.get(&key).cloned();
        match (b, o, t) {
            // Deleted on both sides: gone, and nobody disagrees.
            (Some(_), None, None) => {}
            // Added on one side only: keep it.
            (None, Some(o), None) => rows.push((key.clone(), o)),
            (None, None, Some(t)) => rows.push((key.clone(), t)),
            // Added on both sides: identical adds merge into one; different
            // adds under the same pk are a conflict.
            (None, Some(o), Some(t)) => {
                if o == t {
                    rows.push((key.clone(), o));
                } else {
                    conflicts.push((
                        String::new(),
                        ConflictRow {
                            pk: key.clone(),
                            base: None,
                            ours: Some(o),
                            theirs: Some(t),
                        },
                    ));
                }
            }
            // Deleted on ours while theirs modified it: conflict unless theirs
            // never moved off base.
            (Some(b), None, Some(t)) => {
                if t == b {
                    // Deleted on both.
                } else {
                    conflicts.push((
                        String::new(),
                        ConflictRow {
                            pk: key.clone(),
                            base: Some(b),
                            ours: None,
                            theirs: Some(t),
                        },
                    ));
                }
            }
            // Deleted on theirs while ours modified it: conflict unless ours
            // never moved off base.
            (Some(b), Some(o), None) => {
                if o == b {
                    // Deleted on both.
                } else {
                    conflicts.push((
                        String::new(),
                        ConflictRow {
                            pk: key.clone(),
                            base: Some(b),
                            ours: Some(o),
                            theirs: None,
                        },
                    ));
                }
            }
            // Present on all three: per-cell merge.
            (Some(b), Some(o), Some(t)) => match merge_row(&b, &o, &t, pk, columns) {
                RowOutcome::Merged(row) => rows.push((key.clone(), row)),
                RowOutcome::Unchanged => rows.push((key.clone(), o)),
                RowOutcome::Ours => rows.push((key.clone(), o)),
                RowOutcome::Theirs => rows.push((key.clone(), t)),
                RowOutcome::Conflict(conflict) => {
                    conflicts.push((String::new(), conflict));
                }
            },
            (None, None, None) => {}
        }
    }
    MergeOutcome { rows, conflicts }
}

/// Merge one row present in all three sides cell by cell.
fn merge_row(
    base: &VcRow,
    ours: &VcRow,
    theirs: &VcRow,
    pk: &[String],
    columns: &[String],
) -> RowOutcome {
    let mut values = Vec::with_capacity(columns.len());
    let mut changed = false;
    let mut conflicted = false;
    for i in 0..columns.len() {
        let b = base.values.get(i);
        let o = ours.values.get(i);
        let t = theirs.values.get(i);
        if o == t {
            values.push(o.cloned().unwrap_or(VcValue::Null));
            if o != b {
                changed = true;
            }
        } else if o == b {
            // Only theirs changed: take theirs.
            values.push(t.cloned().unwrap_or(VcValue::Null));
            changed = true;
        } else if t == b {
            // Only ours changed: take ours.
            values.push(o.cloned().unwrap_or(VcValue::Null));
            changed = true;
        } else {
            // Both sides changed to different values: conflict.
            conflicted = true;
            values.push(VcValue::Null);
        }
    }
    if conflicted {
        return RowOutcome::Conflict(ConflictRow {
            pk: extract_pk(ours, pk, columns),
            base: Some(base.clone()),
            ours: Some(ours.clone()),
            theirs: Some(theirs.clone()),
        });
    }
    if changed {
        RowOutcome::Merged(VcRow::new(values))
    } else {
        RowOutcome::Unchanged
    }
}

/// Merge tables without a primary key by full-row identity.
///
/// Identical row sets merge clean; any divergence is a conflict because there
/// is no key to join the two sides on. "Same set" is a multiset comparison, so
/// a row's order in the table does not matter.
fn merge_no_pk(base: &[VcRow], ours: &[VcRow], theirs: &[VcRow]) -> MergeOutcome {
    let mut ours_sorted = ours.to_vec();
    ours_sorted.sort();
    let mut theirs_sorted = theirs.to_vec();
    theirs_sorted.sort();
    if ours_sorted == theirs_sorted {
        return MergeOutcome {
            rows: ours
                .iter()
                .map(|row| (row.values.clone(), row.clone()))
                .collect(),
            conflicts: Vec::new(),
        };
    }
    let mut base_sorted = base.to_vec();
    base_sorted.sort();
    if base_sorted == ours_sorted {
        return MergeOutcome {
            rows: theirs
                .iter()
                .map(|row| (row.values.clone(), row.clone()))
                .collect(),
            conflicts: Vec::new(),
        };
    }
    if base_sorted == theirs_sorted {
        return MergeOutcome {
            rows: ours
                .iter()
                .map(|row| (row.values.clone(), row.clone()))
                .collect(),
            conflicts: Vec::new(),
        };
    }
    let mut rows = Vec::new();
    let mut conflicts = Vec::new();
    // Rows present in both sides are the clean intersection: nobody disputes
    // them, so they keep their shared image. A row added by exactly one side
    // is a conflict (there is no key to join the sides on, so divergence has
    // no safe winner).
    for row in ours_sorted.iter().chain(theirs_sorted.iter()) {
        if ours_sorted.contains(row) && theirs_sorted.contains(row) {
            rows.push((row.values.clone(), row.clone()));
            continue;
        }
        if base_sorted.contains(row) {
            // Present in base and still present here: this side did not add it.
            continue;
        }
        let pk = row.values.clone();
        conflicts.push((
            String::new(),
            ConflictRow {
                pk,
                base: None,
                ours: ours_sorted.contains(row).then(|| row.clone()),
                theirs: theirs_sorted.contains(row).then(|| row.clone()),
            },
        ));
    }
    rows.sort();
    rows.dedup();
    conflicts.sort_by(|a, b| a.1.pk.cmp(&b.1.pk));
    MergeOutcome { rows, conflicts }
}

/// Index rows by their pk cells. A duplicated key is corruption: the rows came
/// from a committed SQL table, which enforces pk uniqueness, so a collision
/// means the store is broken and silently dropping one row would hide it.
fn key_rows(rows: &[VcRow], pk: &[String], columns: &[String]) -> BTreeMap<Vec<VcValue>, VcRow> {
    let mut map = BTreeMap::new();
    for row in rows {
        let key = extract_pk(row, pk, columns);
        assert!(
            map.insert(key.clone(), row.clone()).is_none(),
            "duplicate primary key {key:?} in committed rows"
        );
    }
    map
}

/// Re-key rows in the same shape `key_rows` produced but without pk-based
/// dedup: the fast paths take a whole side wholesale. Tables without a
/// primary key key each row by its full identity, so no rows collapse.
fn rows_keyed(rows: &[VcRow], pk: &[String], columns: &[String]) -> Vec<(Vec<VcValue>, VcRow)> {
    if pk.is_empty() {
        return rows
            .iter()
            .map(|row| (row.values.clone(), row.clone()))
            .collect();
    }
    let mut out = Vec::with_capacity(rows.len());
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let key = extract_pk(row, pk, columns);
        assert!(
            seen.insert(key.clone()),
            "duplicate primary key {key:?} in committed rows"
        );
        out.push((key, row.clone()));
    }
    out
}

/// The pk cells of one row in column order.
fn extract_pk(row: &VcRow, pk: &[String], columns: &[String]) -> Vec<VcValue> {
    pk.iter()
        .filter_map(|name| columns.iter().position(|c| c == name))
        .filter_map(|idx| row.values.get(idx).cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(data: &[(&str, &str)]) -> Vec<VcRow> {
        data.iter()
            .map(|(id, v)| {
                VcRow::new(vec![
                    VcValue::Integer(id.parse().unwrap()),
                    VcValue::Text(v.to_string()),
                ])
            })
            .collect()
    }

    fn text_rows(data: &[&str]) -> Vec<VcRow> {
        data.iter()
            .map(|v| VcRow::new(vec![VcValue::Text(v.to_string())]))
            .collect()
    }

    fn pk() -> Vec<String> {
        vec!["id".to_string()]
    }

    fn cols() -> Vec<String> {
        vec!["id".to_string(), "v".to_string()]
    }

    #[test]
    fn fast_path_theirs_equals_base_keeps_ours() {
        let base = rows(&[("1", "a"), ("2", "b")]);
        let ours = rows(&[("1", "a"), ("2", "b"), ("3", "c")]);
        let theirs = rows(&[("1", "a"), ("2", "b")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert!(out.conflicts.is_empty());
        assert_eq!(out.rows, rows_keyed(&ours, &pk(), &cols()));
    }

    #[test]
    fn fast_path_ours_equals_base_takes_theirs() {
        let base = rows(&[("1", "a"), ("2", "b")]);
        let ours = rows(&[("1", "a"), ("2", "b")]);
        let theirs = rows(&[("1", "a"), ("2", "b"), ("3", "d")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert!(out.conflicts.is_empty());
        assert_eq!(out.rows, rows_keyed(&theirs, &pk(), &cols()));
    }

    #[test]
    fn cell_changed_on_one_side_is_taken() {
        let base = rows(&[("1", "a"), ("2", "b")]);
        let ours = rows(&[("1", "a"), ("2", "b")]);
        let theirs = rows(&[("1", "a"), ("2", "B")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert!(out.conflicts.is_empty());
        assert_eq!(
            out.rows,
            vec![
                (
                    vec![VcValue::Integer(1)],
                    VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())])
                ),
                (
                    vec![VcValue::Integer(2)],
                    VcRow::new(vec![VcValue::Integer(2), VcValue::Text("B".into())])
                ),
            ]
        );
    }

    #[test]
    fn cell_changed_identically_on_both_sides_merges_clean() {
        let base = rows(&[("1", "a")]);
        let ours = rows(&[("1", "z")]);
        let theirs = rows(&[("1", "z")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert!(out.conflicts.is_empty());
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].1.values[1], VcValue::Text("z".into()));
    }

    #[test]
    fn cell_changed_differently_on_both_sides_conflicts() {
        let base = rows(&[("1", "a")]);
        let ours = rows(&[("1", "ours")]);
        let theirs = rows(&[("1", "theirs")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert!(out.rows.is_empty());
        assert_eq!(out.conflicts.len(), 1);
        let (_, c) = &out.conflicts[0];
        assert_eq!(
            c.ours.as_ref().unwrap().values[1],
            VcValue::Text("ours".into())
        );
        assert_eq!(
            c.theirs.as_ref().unwrap().values[1],
            VcValue::Text("theirs".into())
        );
    }

    #[test]
    fn row_added_on_one_side_is_kept() {
        let base = rows(&[("1", "a")]);
        let ours = rows(&[("1", "a"), ("2", "b")]);
        let theirs = rows(&[("1", "a")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert!(out.conflicts.is_empty());
        assert!(out
            .rows
            .iter()
            .any(|(k, _)| k == &vec![VcValue::Integer(2)]));
    }

    #[test]
    fn row_added_identically_on_both_kept_once() {
        let base = rows(&[("1", "a")]);
        let ours = rows(&[("1", "a"), ("2", "b")]);
        let theirs = rows(&[("1", "a"), ("2", "b")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert!(out.conflicts.is_empty());
        assert_eq!(
            out.rows
                .iter()
                .filter(|(k, _)| k == &vec![VcValue::Integer(2)])
                .count(),
            1
        );
    }

    #[test]
    fn row_added_differently_under_same_pk_conflicts() {
        let base = rows(&[("1", "a")]);
        let ours = rows(&[("1", "a"), ("2", "ours")]);
        let theirs = rows(&[("1", "a"), ("2", "theirs")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert!(out
            .rows
            .iter()
            .all(|(k, _)| k != &vec![VcValue::Integer(2)]));
        assert_eq!(out.conflicts.len(), 1);
        assert!(out.conflicts[0].1.base.is_none());
    }

    #[test]
    fn delete_on_one_side_and_modify_on_other_conflicts() {
        let base = rows(&[("1", "a"), ("2", "b")]);
        let ours = rows(&[("1", "a")]);
        let theirs = rows(&[("1", "a"), ("2", "B")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert_eq!(out.conflicts.len(), 1);
        let (_, c) = &out.conflicts[0];
        assert_eq!(c.pk, vec![VcValue::Integer(2)]);
        assert!(c.ours.is_none());
        assert!(c.theirs.is_some());
    }

    #[test]
    fn delete_on_both_is_gone_without_conflict() {
        let base = rows(&[("1", "a"), ("2", "b")]);
        let ours = rows(&[("1", "a")]);
        let theirs = rows(&[("1", "a")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert!(out.conflicts.is_empty());
        assert!(out
            .rows
            .iter()
            .all(|(k, _)| k != &vec![VcValue::Integer(2)]));
    }

    #[test]
    fn unchanged_row_keeps_ours() {
        let base = rows(&[("1", "a")]);
        let ours = rows(&[("1", "a"), ("9", "x")]);
        let theirs = rows(&[("1", "a"), ("9", "x")]);
        let out = three_way_row_merge(&base, &ours, &theirs, &pk(), &cols());
        assert!(out.conflicts.is_empty());
        let r1 = out
            .rows
            .iter()
            .find(|(k, _)| k == &vec![VcValue::Integer(1)])
            .unwrap();
        assert_eq!(r1.1.values[1], VcValue::Text("a".into()));
    }

    #[test]
    fn no_pk_identical_sets_merge_clean() {
        let base = text_rows(&["a", "b"]);
        let ours = text_rows(&["a", "b", "c"]);
        let theirs = text_rows(&["a", "b", "c"]);
        let out = three_way_row_merge(&base, &ours, &theirs, &[], &[String::from("v")]);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.rows.len(), 3);
    }

    #[test]
    fn no_pk_divergence_is_conflict() {
        let base = text_rows(&["a", "b"]);
        let ours = text_rows(&["a", "b", "c"]);
        let theirs = text_rows(&["a", "b", "d"]);
        let out = three_way_row_merge(&base, &ours, &theirs, &[], &[String::from("v")]);
        assert_eq!(out.conflicts.len(), 2);
        // The clean intersection stays in the working rows: a and b appear in
        // both sides, so nobody disputes them even though the sides diverged.
        let shared: Vec<&str> = out
            .rows
            .iter()
            .map(|(_, r)| match &r.values[0] {
                VcValue::Text(s) => s.as_str(),
                _ => panic!("text row expected"),
            })
            .collect();
        assert_eq!(shared, vec!["a", "b"]);
    }

    #[test]
    fn no_pk_one_side_equals_base_takes_other_side() {
        let base = text_rows(&["a", "b"]);
        let ours = text_rows(&["a", "b"]);
        let theirs = text_rows(&["a", "b", "d"]);
        let out = three_way_row_merge(&base, &ours, &theirs, &[], &[String::from("v")]);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.rows.len(), 3);
    }

    #[test]
    fn multi_column_pk_keyed_by_both_columns() {
        let cols: Vec<String> = vec!["a".to_string(), "b".to_string(), "v".to_string()];
        let pk: Vec<String> = vec!["a".to_string(), "b".to_string()];
        let base = vec![VcRow::new(vec![
            VcValue::Integer(1),
            VcValue::Integer(1),
            VcValue::Text("x".into()),
        ])];
        let ours = base.clone();
        let theirs = vec![VcRow::new(vec![
            VcValue::Integer(1),
            VcValue::Integer(1),
            VcValue::Text("y".into()),
        ])];
        let out = three_way_row_merge(&base, &ours, &theirs, &pk, &cols);
        assert!(out.conflicts.is_empty());
        assert_eq!(
            out.rows[0].0,
            vec![VcValue::Integer(1), VcValue::Integer(1)]
        );
        assert_eq!(out.rows[0].1.values[2], VcValue::Text("y".into()));
    }
}
