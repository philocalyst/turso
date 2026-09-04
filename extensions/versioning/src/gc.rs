use std::collections::{HashMap, HashSet};

use crate::commit::Commit;
use crate::model::{ChunkHash, CommitId, VersionError, VersionResult};
use crate::refs::RefStore;
use crate::staging::VcStore;

/// What one gc run removed and kept, rendered as doltlite's
/// "N chunks removed, M chunks kept".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GcStats {
    pub removed: usize,
    pub kept: usize,
}

impl GcStats {
    pub fn summary(&self) -> String {
        format!("{} chunks removed, {} chunks kept", self.removed, self.kept)
    }
}

pub struct GcSnapshot {
    pub all_chunks: HashSet<ChunkHash>,
    pub refs: HashMap<ChunkHash, Vec<ChunkHash>>,
    /// Commit graph edges: commit hash → its referenced hashes (parents
    /// plus content chunks). Marks flow through these once the commit
    /// itself is reachable from a root.
    pub commit_refs: HashMap<ChunkHash, Vec<ChunkHash>>,
    pub working_sets: Vec<Vec<ChunkHash>>,
}

/// Walk references from working-set heads through chunk refs and commit
/// edges. A chunk reachable only through an unreachable commit is swept.
pub fn mark(snapshot: &GcSnapshot) -> HashSet<ChunkHash> {
    let mut live = HashSet::new();
    let mut stack: Vec<ChunkHash> = Vec::new();

    for ws in &snapshot.working_sets {
        for &h in ws {
            stack.push(h);
        }
    }

    while let Some(current) = stack.pop() {
        if !live.insert(current) {
            continue;
        }
        let mut push = |h: &ChunkHash| {
            if !live.contains(h) {
                stack.push(*h);
            }
        };
        if let Some(referenced) = snapshot.refs.get(&current) {
            for r in referenced {
                push(r);
            }
        }
        if let Some(referenced) = snapshot.commit_refs.get(&current) {
            for r in referenced {
                push(r);
            }
        }
    }

    live
}

/// Chunks not reachable from any root.
pub fn sweep(snapshot: &GcSnapshot, live: &HashSet<ChunkHash>) -> Vec<ChunkHash> {
    snapshot.all_chunks.difference(live).copied().collect()
}

/// The gc exclusivity gate: refuse unless the store is quiet (no open SQL
/// transaction, no uncommitted work, no open merge/rebase/conflicts).
pub fn gc_exclusive(store: &VcStore) -> VersionResult<()> {
    if store.gc_quiet() {
        Ok(())
    } else {
        Err(VersionError::GcRequiresExclusiveAccess)
    }
}

/// Full mark-sweep over the store: commits reachable from branches, tags,
/// tracking refs, and the detached pin survive with their snapshot
/// records; everything else is removed. Assumes the caller already passed
/// the exclusivity gate.
pub fn collect_garbage(store: &mut VcStore) -> GcStats {
    let live = live_commits(store);

    let mut removed = 0;
    let mut kept = 0;

    // Commits: rebuild the store with the live ones only.
    let entries: Vec<(CommitId, Commit)> = store
        .commit_entries()
        .into_iter()
        .filter(|(id, _)| live.contains(id))
        .collect();
    kept += entries.len();
    removed += store.commit_entries().len().saturating_sub(entries.len());
    store.rebuild_commit_store(entries);

    // Snapshots: drop the maps of dead commits, then dead records inside
    // surviving maps cannot exist (records are per-commit).
    let dead_snapshots: Vec<CommitId> = store
        .snapshots
        .keys()
        .filter(|id| !live.contains(id))
        .copied()
        .collect();
    removed += dead_snapshots.len();
    for id in &dead_snapshots {
        store.snapshots.remove(id);
    }
    kept += store.snapshots.len();

    // The id index follows the records it describes.
    store
        .snap_index
        .retain(|_, (owner, _)| store.snapshots.contains_key(owner));

    GcStats { removed, kept }
}

/// The compaction variant VACUUM runs: silently skips when the store is
/// not quiet (doltlite doltliteGcCompactWithPhase skip rules).
pub fn gc_compact(store: &mut VcStore) -> GcStats {
    if !store.gc_quiet() {
        return GcStats::default();
    }
    collect_garbage(store)
}

/// Commit ids reachable from every root the store names.
fn live_commits(store: &VcStore) -> HashSet<CommitId> {
    let mut live = HashSet::new();
    let mut stack: Vec<CommitId> = Vec::new();

    for (_, id) in store.refs.list(crate::refs::RefNs::Heads) {
        stack.push(id);
    }
    for (_, id) in store.refs.list(crate::refs::RefNs::Tags) {
        stack.push(id);
    }
    for (_, id) in store.tracking_refs() {
        stack.push(id);
    }
    if let Some(detached) = store.detached_snapshot() {
        stack.push(detached);
    }

    while let Some(current) = stack.pop() {
        if !live.insert(current) {
            continue;
        }
        if let Some(commit) = store.get_commit(&current) {
            for parent in commit.parents {
                if !live.contains(&parent) {
                    stack.push(parent);
                }
            }
        }
    }

    live
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::{CommitMeta, CommitStore};
    use crate::model::RootHash;
    use crate::vtab_log::{VcRow, VcValue};

    #[test]
    fn gc_stub_marks_live_set() {
        let a = ChunkHash([0x01; 20]);
        let b = ChunkHash([0x02; 20]);
        let c = ChunkHash([0x03; 20]);
        let d = ChunkHash([0x04; 20]);

        let mut refs = HashMap::new();
        refs.insert(a, vec![b, c]);
        refs.insert(b, vec![d]);

        let mut all = HashSet::new();
        all.insert(a);
        all.insert(b);
        all.insert(c);
        all.insert(d);

        let snapshot = GcSnapshot {
            all_chunks: all,
            refs,
            commit_refs: HashMap::new(),
            working_sets: vec![vec![a]],
        };

        let live = mark(&snapshot);
        assert!(live.contains(&a));
        assert!(live.contains(&b));
        assert!(live.contains(&c));
        assert!(live.contains(&d));

        let removal = sweep(&snapshot, &live);
        assert!(removal.is_empty());
    }

    #[test]
    fn gc_stub_sweep_removes_unreachable() {
        let a = ChunkHash([0x01; 20]);
        let b = ChunkHash([0x02; 20]);
        let orphan = ChunkHash([0x03; 20]);

        let mut refs = HashMap::new();
        refs.insert(a, vec![b]);

        let mut all = HashSet::new();
        all.insert(a);
        all.insert(b);
        all.insert(orphan);

        let snapshot = GcSnapshot {
            all_chunks: all,
            refs,
            commit_refs: HashMap::new(),
            working_sets: vec![vec![a]],
        };

        let live = mark(&snapshot);
        let removal = sweep(&snapshot, &live);
        assert_eq!(removal.len(), 1);
        assert_eq!(removal[0], orphan);
    }

    #[test]
    fn gc_cycle_does_not_loop() {
        let a = ChunkHash([0x01; 20]);
        let b = ChunkHash([0x02; 20]);

        let mut refs = HashMap::new();
        refs.insert(a, vec![b]);
        refs.insert(b, vec![a]);

        let mut all = HashSet::new();
        all.insert(a);
        all.insert(b);

        let snapshot = GcSnapshot {
            all_chunks: all,
            refs,
            commit_refs: HashMap::new(),
            working_sets: vec![vec![a]],
        };

        let live = mark(&snapshot);
        assert_eq!(live.len(), 2);
    }

    #[test]
    fn gc_self_loop() {
        let a = ChunkHash([0x01; 20]);

        let mut refs = HashMap::new();
        refs.insert(a, vec![a]);

        let mut all = HashSet::new();
        all.insert(a);

        let snapshot = GcSnapshot {
            all_chunks: all,
            refs,
            commit_refs: HashMap::new(),
            working_sets: vec![vec![a]],
        };

        let live = mark(&snapshot);
        assert_eq!(live.len(), 1);
    }

    #[test]
    fn gc_commit_traversal_keeps_reachable_chunks() {
        // Rewritten from the O1 stub contract: commit edges now participate
        // in marking, so a chunk referenced by a REACHED commit survives,
        // and one hanging off an unreached commit is swept.
        let root_commit = ChunkHash([0x11; 20]);
        let kept_chunk = ChunkHash([0x21; 20]);
        let dead_commit = ChunkHash([0x31; 20]);
        let dead_chunk = ChunkHash([0x41; 20]);

        let mut commit_refs = HashMap::new();
        commit_refs.insert(root_commit, vec![kept_chunk]);
        commit_refs.insert(dead_commit, vec![dead_chunk]);

        let mut all = HashSet::new();
        all.insert(root_commit);
        all.insert(kept_chunk);
        all.insert(dead_commit);
        all.insert(dead_chunk);

        let snapshot = GcSnapshot {
            all_chunks: all,
            refs: HashMap::new(),
            commit_refs,
            // The working set roots at the reachable commit.
            working_sets: vec![vec![root_commit]],
        };

        let live = mark(&snapshot);
        assert!(live.contains(&root_commit));
        assert!(live.contains(&kept_chunk));
        assert!(!live.contains(&dead_commit));
        assert!(!live.contains(&dead_chunk));
    }

    fn store_with_history() -> (VcStore, CommitId, CommitId) {
        let mut store = VcStore::new("main");
        store.config_set("user.name", "A");
        store.config_set("user.email", "a@b");
        store.set_now(10);
        let rows = |n: i64| {
            vec![VcRow::new(vec![
                VcValue::Integer(n),
                VcValue::Text("x".into()),
            ])]
        };
        for i in 0..2 {
            store.apply_work(
                "t",
                vec!["id".to_string(), "v".to_string()],
                vec!["id".to_string()],
                rows(i),
                String::new(),
            );
            store.track_table("t");
            store.dolt_add(&["t"]).unwrap();
            let tip = store.dolt_commit("m", None, false, false).unwrap();
            store.record_snapshot(
                tip,
                "t",
                vec!["id".to_string(), "v".to_string()],
                vec!["id".to_string()],
                store.work_table("t").unwrap().rows.clone(),
                String::new(),
            );
        }
        let tips: Vec<CommitId> = store
            .refs
            .list(crate::refs::RefNs::Heads)
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        let head = tips[0];
        // Build an orphan commit directly in the commit store.
        let orphan = store.commit_store_mut().put_commit(Commit {
            parents: vec![],
            root: RootHash([0; 20]),
            meta: CommitMeta {
                name: "ghost".into(),
                email: "g@b".into(),
                message: "unreachable".into(),
                timestamp: 99,
            },
        });
        (store, head, orphan)
    }

    #[test]
    fn gc_collect_removes_unreachable_commit() {
        let (mut store, head, orphan) = store_with_history();
        let stats = collect_garbage(&mut store);
        assert!(stats.removed >= 1);
        assert!(store.get_commit(&head).is_some());
        assert!(store.get_commit(&orphan).is_none());
        // The summary matches doltlite's format.
        assert_eq!(
            stats.summary(),
            format!(
                "{} chunks removed, {} chunks kept",
                stats.removed, stats.kept
            )
        );
    }

    #[test]
    fn gc_keeps_tag_tracking_and_detached_roots() {
        let (mut store, head, orphan) = store_with_history();
        store.create_tag("v1", orphan).unwrap();
        store
            .tracking
            .insert(("origin".to_string(), "side".to_string()), orphan);
        collect_garbage(&mut store);
        // Tagged and tracked: still live.
        assert!(store.get_commit(&orphan).is_some());
        store.delete_tag("v1").unwrap();
        store.tracking.clear();
        collect_garbage(&mut store);
        assert!(store.get_commit(&orphan).is_none());
        let _ = head;
    }

    #[test]
    fn gc_removes_snapshots_of_dead_commits() {
        let (mut store, head, orphan) = store_with_history();
        let mut tables = HashMap::new();
        tables.insert(
            "ghost".to_string(),
            crate::staging::TableSnapshot {
                columns: vec!["id".to_string()],
                pk: vec!["id".to_string()],
                rows: vec![],
                schema_sql: String::new(),
            },
        );
        store.record_snapshot(
            orphan,
            "ghost",
            vec!["id".into()],
            vec!["id".into()],
            vec![],
            String::new(),
        );
        let _ = tables;
        collect_garbage(&mut store);
        assert!(store.snapshots.get(&head).is_some());
        assert!(store.snapshots.get(&orphan).is_none());
    }

    #[test]
    fn gc_exclusive_gate_refuses_busy_store() {
        let mut store = VcStore::new("main");
        assert!(gc_exclusive(&store).is_ok());
        store.note_txn_event("BEGIN");
        assert_eq!(
            gc_exclusive(&store).unwrap_err(),
            VersionError::GcRequiresExclusiveAccess
        );
        store.note_txn_event("COMMIT");
        assert!(gc_exclusive(&store).is_ok());
        store.track_table("t");
        assert_eq!(
            gc_exclusive(&store).unwrap_err(),
            VersionError::GcRequiresExclusiveAccess
        );
    }

    #[test]
    fn gc_compact_skips_when_not_quiet() {
        let (mut store, _head, orphan) = store_with_history();
        store.note_txn_event("BEGIN");
        let stats = gc_compact(&mut store);
        assert_eq!(stats.removed, 0);
        assert!(store.get_commit(&orphan).is_some());
        store.note_txn_event("COMMIT");
        let stats = gc_compact(&mut store);
        assert!(stats.removed >= 1);
        assert!(store.get_commit(&orphan).is_none());
    }
}
