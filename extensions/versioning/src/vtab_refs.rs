//! Ref listings for the `dolt_branches` and `dolt_tags` vtabs: one row per
//! local branch and per tag, matching doltlite's column order. Turso has no
//! remotes and no annotated tags, so `remote` and tag `message` render empty.

use crate::staging::VcStore;

pub const DOLT_BRANCHES_SCHEMA: &str = "CREATE TABLE dolt_branches (name TEXT, hash TEXT, latest_commit_message TEXT, remote TEXT, branch INTEGER, dirty INTEGER)";
pub const DOLT_TAGS_SCHEMA: &str =
    "CREATE TABLE dolt_tags (tag_name TEXT, tag_hash TEXT, message TEXT)";
pub const DOLT_REMOTES_SCHEMA: &str =
    "CREATE TABLE dolt_remotes (name TEXT, url TEXT, fetch_specs TEXT, params TEXT)";
pub const DOLT_REMOTE_BRANCHES_SCHEMA: &str = "CREATE TABLE dolt_remote_branches (name TEXT, hash TEXT, latest_commit_message TEXT, name_prefix TEXT HIDDEN)";

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

pub struct RemoteRow {
    pub name: String,
    pub url: String,
    pub fetch_specs: String,
    pub params: String,
}

pub struct RemoteBranchRow {
    pub name: String,
    pub hash: String,
    pub latest_commit_message: String,
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

pub fn remotes_rows(store: &VcStore) -> Vec<RemoteRow> {
    let mut rows: Vec<RemoteRow> = store
        .remote_configs()
        .iter()
        .map(|remote| RemoteRow {
            name: remote.name.clone(),
            url: remote.url.clone(),
            fetch_specs: format!("[\"refs/heads/*:refs/remotes/{}/*\"]", remote.name),
            params: "{}".to_string(),
        })
        .collect();
    rows.sort_by(|left, right| left.name.cmp(&right.name));
    rows
}

pub fn remote_branches_rows(store: &VcStore, prefix: Option<&str>) -> Vec<RemoteBranchRow> {
    let mut rows: Vec<RemoteBranchRow> = store
        .tracking_refs()
        .into_iter()
        .filter_map(|((remote, branch), commit)| {
            let name = format!("remotes/{remote}/{branch}");
            if prefix.is_some_and(|prefix| !name.starts_with(prefix)) {
                return None;
            }
            Some(RemoteBranchRow {
                name,
                hash: commit.to_hex(),
                latest_commit_message: store
                    .get_commit(&commit)
                    .map(|commit| commit.meta.message)
                    .unwrap_or_default(),
            })
        })
        .collect();
    rows.sort_by(|left, right| left.name.cmp(&right.name));
    rows
}

/// The checked-out branch is dirty when any tracked table has uncommitted
/// changes — the same source `dolt_add('-A')` stages. The engine's write hook
/// marks freshly written tables; committed-then-modified content is caught by
/// the head-snapshot comparison.
fn branch_dirty(store: &VcStore) -> bool {
    !store.changed_tables().is_empty()
}
