use std::collections::{HashMap, HashSet};

use crate::model::{ChunkHash, CommitHash};

pub struct GcSnapshot {
    pub all_chunks: HashSet<ChunkHash>,
    pub refs: HashMap<ChunkHash, Vec<ChunkHash>>,
    /// Commit traversal resolves in O5 once the commit object exists. Until
    /// then `mark()` never reads this: callers must mirror commit-reachable
    /// roots into `working_sets` or a commit-rooted chunk gets swept.
    pub commits: Vec<CommitHash>,
    pub working_sets: Vec<Vec<ChunkHash>>,
}

/// Walks references from working-set heads only. Commit traversal is O5; today
/// commit-reachable roots must already be present in `working_sets` or they
/// are not marked (see `GcSnapshot::commits`).
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
        if let Some(referenced) = snapshot.refs.get(&current) {
            for &r in referenced {
                if !live.contains(&r) {
                    stack.push(r);
                }
            }
        }
    }

    live
}

/// Chunks not reachable from any root.
pub fn sweep(snapshot: &GcSnapshot, live: &HashSet<ChunkHash>) -> Vec<ChunkHash> {
    snapshot.all_chunks.difference(live).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            all_chunks: all.clone(),
            refs,
            commits: vec![],
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
            all_chunks: all.clone(),
            refs,
            commits: vec![],
            working_sets: vec![vec![a]],
        };

        let live = mark(&snapshot);
        let removal = sweep(&snapshot, &live);
        assert_eq!(removal.len(), 1);
        assert_eq!(removal[0], orphan);
    }

    #[test]
    fn gc_all_chunks_superset_of_reachable() {
        let a = ChunkHash([0x01; 20]);
        let b = ChunkHash([0x02; 20]);
        let unreachable = ChunkHash([0x03; 20]);

        let mut refs = HashMap::new();
        refs.insert(a, vec![b]);

        let mut all = HashSet::new();
        all.insert(a);
        all.insert(b);
        all.insert(unreachable);

        let snapshot = GcSnapshot {
            all_chunks: all,
            refs,
            commits: vec![],
            working_sets: vec![vec![a]],
        };

        let live = mark(&snapshot);
        for h in &live {
            assert!(snapshot.all_chunks.contains(h));
        }
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
            commits: vec![],
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
            commits: vec![],
            working_sets: vec![vec![a]],
        };

        let live = mark(&snapshot);
        assert_eq!(live.len(), 1);
    }

    #[test]
    fn gc_commits_field_documented() {
        // Pins the current contract: commits is non-empty and unread, so only
        // working_set roots get marked. A chunk reachable only through a commit
        // is swept today — fixing that is O5, and this test fails if someone
        // changes mark() silently.
        let a = ChunkHash([0x01; 20]);
        let commit_only = ChunkHash([0x09; 20]);

        let mut all = HashSet::new();
        all.insert(a);
        all.insert(commit_only);

        let snapshot = GcSnapshot {
            all_chunks: all,
            refs: HashMap::new(),
            commits: vec![CommitHash([0x99; 20])],
            working_sets: vec![vec![a]],
        };

        let live = mark(&snapshot);
        assert!(live.contains(&a));
        assert!(!live.contains(&commit_only));
    }
}
