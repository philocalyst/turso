//! Per-connection branch session state.
//!
//! Integration note: this companion struct lives alongside
//! `core/connection.rs:387` and `core/database.rs:520`, owned by the
//! connection context — never inside the pager. The pager owns storage; this
//! struct owns which branch the connection is on and whether it is detached.
//! Each connection keeps its own active branch, and the uncommitted working
//! set belongs to that branch (R3). This crate has zero `core/` imports by
//! design, so the connection layer stores the struct and reads its fields
//! without the pager ever seeing it.

use crate::model::{CommitId, VersionError, VersionResult};
use crate::refs::RefName;

/// Branch session: either on a named branch or detached at a pinned commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBranch {
    current: Option<RefName>,
    detached: Option<CommitId>,
}

impl SessionBranch {
    pub fn new(branch: &str) -> Self {
        SessionBranch {
            current: Some(RefName::branch(branch)),
            detached: None,
        }
    }

    /// Attach to a named branch, clearing any detached pin.
    pub fn open_branch(&mut self, name: &str) -> RefName {
        self.current = Some(RefName::branch(name));
        self.detached = None;
        RefName::branch(name)
    }

    /// Open an immutable snapshot at `snapshot`. The id is pinned here, so a
    /// peer advancing or deleting the underlying ref does not move this
    /// connection's view (R4).
    pub fn open_detached(&mut self, snapshot: CommitId) {
        self.current = None;
        self.detached = Some(snapshot);
    }

    /// Reattach to a branch; this is what makes a detached connection
    /// writable again (R4).
    pub fn checkout(&mut self, name: &str) {
        self.current = Some(RefName::branch(name));
        self.detached = None;
    }

    /// `None` while detached, so `dolt_active_branch()` reads as NULL.
    pub fn active_branch(&self) -> Option<&RefName> {
        self.current.as_ref()
    }

    /// The pinned snapshot when detached.
    pub fn snapshot(&self) -> Option<CommitId> {
        self.detached
    }

    /// Writes are refused on a detached snapshot (R4).
    pub fn guard_write(&self) -> VersionResult<()> {
        if self.detached.is_some() {
            return Err(VersionError::DetachedHeadWrite);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_active_branch_tracks_checkout() {
        let mut s = SessionBranch::new("main");
        assert_eq!(s.active_branch().unwrap().name, "main");
        s.checkout("dev");
        assert_eq!(s.active_branch().unwrap().name, "dev");
        s.open_branch("feature");
        assert_eq!(s.active_branch().unwrap().name, "feature");
    }

    #[test]
    fn session_detached_hides_branch_and_blocks_writes() {
        let mut s = SessionBranch::new("main");
        let snapshot = CommitId([0x11; 20]);
        s.open_detached(snapshot);
        assert!(s.active_branch().is_none());
        assert_eq!(s.snapshot(), Some(snapshot));
        assert_eq!(
            s.guard_write().unwrap_err().to_string(),
            "cannot write in detached HEAD state"
        );
    }

    #[test]
    fn session_checkout_reattaches_and_unblocks_writes() {
        let mut s = SessionBranch::new("main");
        s.open_detached(CommitId([0x22; 20]));
        s.checkout("dev");
        assert_eq!(s.active_branch().unwrap().name, "dev");
        assert!(s.guard_write().is_ok());
    }
}
