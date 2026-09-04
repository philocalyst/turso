//! Ref listings for the `dolt_branches` and `dolt_tags` vtabs: one row per
//! local branch and per tag, matching doltlite's column order. Turso has no
//! remotes and no annotated tags, so `remote` and tag `message` render empty.

use crate::staging::VcStore;

pub const DOLT_BRANCHES_SCHEMA: &str = "CREATE TABLE dolt_branches (name TEXT, hash TEXT, latest_commit_message TEXT, remote TEXT, branch INTEGER, dirty INTEGER)";
pub const DOLT_TAGS_SCHEMA: &str =
    "CREATE TABLE dolt_tags (tag_name TEXT, tag_hash TEXT, message TEXT)";

pub struct BranchRow {
    pub name: String,
    pub hash: String,
    pub latest_commit_message: String,
    pub remote: String,
    pub branch: bool,
    pub dirty: bool,
}

pub struct TagRow {
    pub tag_name: String,
    pub tag_hash: String,
    pub message: String,
}

pub fn branches_rows(store: &VcStore) -> Vec<BranchRow> {
    let active = store.active_branch();
    let dirty = branch_dirty(store);
    store
        .list_branches()
        .into_iter()
        .map(|name| {
            let tip = store.branch_tip(&name);
            BranchRow {
                latest_commit_message: tip
                    .and_then(|t| store.get_commit(&t))
                    .map(|c| c.meta.message)
                    .unwrap_or_default(),
                hash: tip.map(|t| t.to_hex()).unwrap_or_default(),
                remote: String::new(),
                branch: active == Some(name.as_str()),
                dirty: active == Some(name.as_str()) && dirty,
                name,
            }
        })
        .collect()
}

pub fn tags_rows(store: &VcStore) -> Vec<TagRow> {
    store
        .list_tags()
        .into_iter()
        .map(|(name, at)| TagRow {
            tag_hash: at.to_hex(),
            message: String::new(),
            tag_name: name,
        })
        .collect()
}

/// The checked-out branch is dirty when any tracked table has uncommitted
/// changes — the same source `dolt_add('-A')` stages. The engine's write hook
/// marks freshly written tables; committed-then-modified content is caught by
/// the head-snapshot comparison.
fn branch_dirty(store: &VcStore) -> bool {
    !store.changed_tables().is_empty()
}
