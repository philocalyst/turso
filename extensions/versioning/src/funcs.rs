//! Pure string-level scalar function specs for `SELECT dolt_*()`.
//!
//! F1–F3. These operate on a `VcOperations` seam so they run without
//! `core/`; the CORE-WIRE block at the bottom maps each function onto
//! `extensions/core/src/functions.rs::ScalarFunc`.

use sha2::{Digest, Sha256};

use crate::conflicts::ResolveSide;
use crate::model::{CommitId, VersionError, VersionResult};
use crate::replay::{MergeResult, MergeStatusRow, RebaseStep};
use crate::staging::StatusRow;
use crate::vtab_log::VcValue;

/// A scalar argument in its raw SQL form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuncArg<'a> {
    Text(&'a str),
    Integer(i64),
    Real(u64),
    Blob(&'a [u8]),
    Null,
}

impl<'a> FuncArg<'a> {
    fn as_text(&self) -> Option<&'a str> {
        match self {
            FuncArg::Text(s) => Some(s),
            _ => None,
        }
    }
}

/// A scalar return value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FuncValue {
    Text(String),
    Integer(i64),
    Null,
}

/// Operations the `dolt_*` functions need from the store. `staging::VcStore`
/// implements this; tests use the real store so funcs stay honest.
pub trait VcOperations {
    fn add(&mut self, tables: &[&str]) -> VersionResult<usize>;
    fn add_all(&mut self) -> VersionResult<usize>;
    fn commit(
        &mut self,
        msg: &str,
        author: Option<(&str, &str)>,
        all: bool,
        force: bool,
    ) -> VersionResult<CommitId>;
    fn create_branch(&mut self, name: &str) -> VersionResult<()>;
    fn delete_branch(&mut self, name: &str) -> VersionResult<()>;
    fn list_branches(&self) -> Vec<String>;
    fn checkout(&mut self, name: &str) -> VersionResult<()>;
    fn create_tag(&mut self, name: &str, at: CommitId) -> VersionResult<()>;
    fn delete_tag(&mut self, name: &str) -> VersionResult<()>;
    fn active_branch(&self) -> Option<&str>;
    fn resolve(&self, spec: &str) -> VersionResult<CommitId>;
    fn config_get(&self, key: &str) -> Option<String>;
    fn config_set(&mut self, key: &str, value: &str);
    fn tables(&self) -> Vec<String>;
    fn table_hash(&self, table: &str, revision: Option<&str>) -> VersionResult<String>;
    fn db_hash(&self, revision: Option<&str>) -> VersionResult<String>;
    fn status(&self) -> Vec<StatusRow>;
    fn reset_soft(&mut self) -> VersionResult<()>;
    fn reset_hard(&mut self) -> VersionResult<()>;
    fn clean(&mut self) -> VersionResult<Vec<String>>;
    fn merge_branch(
        &mut self,
        branch: &str,
        squash: bool,
        no_commit: bool,
        parent: Option<usize>,
    ) -> VersionResult<MergeResult>;
    fn merge_abort(&mut self) -> VersionResult<()>;
    fn merge_base(&self, a: &str, b: &str) -> VersionResult<Option<CommitId>>;
    fn cherry_pick(&mut self, commit: &str, parent: Option<usize>) -> VersionResult<MergeResult>;
    fn revert(&mut self, commit: &str, parent: Option<usize>) -> VersionResult<MergeResult>;
    fn rebase_onto(&mut self, onto: &str) -> VersionResult<CommitId>;
    fn rebase_plan(&mut self, onto: &str) -> VersionResult<usize>;
    fn rebase_edit_plan(&mut self, steps: Vec<RebaseStep>) -> VersionResult<()>;
    fn rebase_continue(&mut self) -> VersionResult<CommitId>;
    fn rebase_abort(&mut self) -> VersionResult<()>;
    fn resolve_conflict(
        &mut self,
        table: &str,
        pk: &[VcValue],
        side: ResolveSide,
    ) -> VersionResult<()>;
    fn verify_constraints(&mut self) -> VersionResult<usize>;
    fn merge_status_rows(&self) -> Vec<MergeStatusRow>;
    fn remote_add(&mut self, name: &str, url: &str) -> VersionResult<()>;
    fn remote_remove(&mut self, name: &str) -> VersionResult<()>;
    fn push(&mut self, remote: &str, branch: &str, force: bool) -> VersionResult<()>;
    fn fetch(&mut self, remote: &str, branch: Option<&str>) -> VersionResult<()>;
    fn pull(&mut self, remote: &str, branch: &str) -> VersionResult<()>;
    fn clone_remote(&mut self, url: &str, lazy: bool) -> VersionResult<()>;
    fn gc(&mut self) -> VersionResult<String>;
}

impl VcOperations for crate::staging::VcStore {
    fn add(&mut self, tables: &[&str]) -> VersionResult<usize> {
        self.dolt_add(tables)?;
        Ok(tables.len())
    }

    fn add_all(&mut self) -> VersionResult<usize> {
        let changed = crate::staging::VcStore::changed_tables(self);
        crate::staging::VcStore::add_all(self)?;
        Ok(changed.len())
    }

    fn commit(
        &mut self,
        msg: &str,
        author: Option<(&str, &str)>,
        all: bool,
        force: bool,
    ) -> VersionResult<CommitId> {
        self.dolt_commit(msg, author, all, force)
    }

    fn create_branch(&mut self, name: &str) -> VersionResult<()> {
        crate::staging::VcStore::create_branch(self, name)
    }

    fn delete_branch(&mut self, name: &str) -> VersionResult<()> {
        crate::staging::VcStore::delete_branch(self, name)
    }

    fn list_branches(&self) -> Vec<String> {
        crate::staging::VcStore::list_branches(self)
    }

    fn checkout(&mut self, name: &str) -> VersionResult<()> {
        crate::staging::VcStore::checkout(self, name)
    }

    fn create_tag(&mut self, name: &str, at: CommitId) -> VersionResult<()> {
        crate::staging::VcStore::create_tag(self, name, at)
    }

    fn delete_tag(&mut self, name: &str) -> VersionResult<()> {
        crate::staging::VcStore::delete_tag(self, name)
    }

    fn active_branch(&self) -> Option<&str> {
        crate::staging::VcStore::active_branch(self)
    }

    fn resolve(&self, spec: &str) -> VersionResult<CommitId> {
        crate::staging::VcStore::resolve(self, spec)
    }

    fn config_get(&self, key: &str) -> Option<String> {
        crate::staging::VcStore::config_get(self, key)
    }

    fn config_set(&mut self, key: &str, value: &str) {
        crate::staging::VcStore::config_set(self, key, value);
    }

    fn tables(&self) -> Vec<String> {
        let mut names = crate::staging::VcStore::tables(self);
        names.sort();
        names
    }

    fn table_hash(&self, table: &str, revision: Option<&str>) -> VersionResult<String> {
        let snapshot = match revision {
            Some(revision) => {
                let commit = self.resolve(revision)?;
                self.snapshots
                    .get(&commit)
                    .and_then(|tables| tables.get(table))
            }
            None => self.work.get(table).or_else(|| {
                self.head_commit()
                    .and_then(|commit| self.snapshots.get(&commit))
                    .and_then(|tables| tables.get(table))
            }),
        }
        .ok_or_else(|| VersionError::TableNotFound(table.to_string()))?;
        Ok(crate::remote_wire::snapshot_id(snapshot).to_hex())
    }

    fn db_hash(&self, revision: Option<&str>) -> VersionResult<String> {
        let mut snapshots: Vec<(&str, &crate::staging::TableSnapshot)> = match revision {
            Some(revision) => {
                let commit = self.resolve(revision)?;
                self.snapshots
                    .get(&commit)
                    .into_iter()
                    .flat_map(|tables| tables.iter())
                    .map(|(name, snapshot)| (name.as_str(), snapshot))
                    .collect()
            }
            None => self
                .work
                .iter()
                .map(|(name, snapshot)| (name.as_str(), snapshot))
                .collect(),
        };
        snapshots.sort_by_key(|(name, _)| *name);
        let mut hasher = Sha256::new();
        for (name, snapshot) in snapshots {
            hasher.update(name.as_bytes());
            hasher.update([0]);
            hasher.update(crate::remote_wire::encode_snapshot(snapshot));
        }
        Ok(hash_bytes(&hasher.finalize()))
    }

    fn status(&self) -> Vec<StatusRow> {
        crate::staging::VcStore::status(self)
    }

    fn reset_soft(&mut self) -> VersionResult<()> {
        crate::staging::VcStore::reset_soft(self)
    }

    fn reset_hard(&mut self) -> VersionResult<()> {
        crate::staging::VcStore::reset_hard(self)
    }

    fn clean(&mut self) -> VersionResult<Vec<String>> {
        crate::staging::VcStore::clean(self)
    }

    fn merge_branch(
        &mut self,
        branch: &str,
        squash: bool,
        no_commit: bool,
        parent: Option<usize>,
    ) -> VersionResult<MergeResult> {
        crate::staging::VcStore::merge_branch(self, branch, squash, no_commit, parent)
    }

    fn merge_abort(&mut self) -> VersionResult<()> {
        crate::staging::VcStore::merge_abort(self)
    }

    fn merge_base(&self, a: &str, b: &str) -> VersionResult<Option<CommitId>> {
        let a_id = self.resolve(a)?;
        let b_id = self.resolve(b)?;
        crate::staging::VcStore::merge_base_of(self, a_id, b_id)
    }

    fn cherry_pick(&mut self, commit: &str, parent: Option<usize>) -> VersionResult<MergeResult> {
        crate::staging::VcStore::cherry_pick(self, commit, parent)
    }

    fn revert(&mut self, commit: &str, parent: Option<usize>) -> VersionResult<MergeResult> {
        crate::staging::VcStore::revert(self, commit, parent)
    }

    fn rebase_onto(&mut self, onto: &str) -> VersionResult<CommitId> {
        crate::staging::VcStore::rebase_onto(self, onto)
    }

    fn rebase_plan(&mut self, onto: &str) -> VersionResult<usize> {
        crate::staging::VcStore::rebase_plan(self, onto)
    }

    fn rebase_edit_plan(&mut self, steps: Vec<RebaseStep>) -> VersionResult<()> {
        crate::staging::VcStore::rebase_edit_plan(self, steps)
    }

    fn rebase_continue(&mut self) -> VersionResult<CommitId> {
        crate::staging::VcStore::rebase_continue(self)
    }

    fn rebase_abort(&mut self) -> VersionResult<()> {
        crate::staging::VcStore::rebase_abort(self)
    }

    fn resolve_conflict(
        &mut self,
        table: &str,
        pk: &[VcValue],
        side: ResolveSide,
    ) -> VersionResult<()> {
        crate::staging::VcStore::resolve_conflict(self, table, pk, side)
    }

    fn verify_constraints(&mut self) -> VersionResult<usize> {
        crate::staging::VcStore::verify_constraints(self)
    }

    fn merge_status_rows(&self) -> Vec<MergeStatusRow> {
        crate::staging::VcStore::merge_status_rows(self)
    }

    fn remote_add(&mut self, name: &str, url: &str) -> VersionResult<()> {
        crate::staging::VcStore::remote_add(self, name, url)
    }

    fn remote_remove(&mut self, name: &str) -> VersionResult<()> {
        crate::staging::VcStore::remote_remove(self, name)
    }

    fn push(&mut self, remote: &str, branch: &str, force: bool) -> VersionResult<()> {
        crate::staging::VcStore::push(self, remote, branch, force)
    }

    fn fetch(&mut self, remote: &str, branch: Option<&str>) -> VersionResult<()> {
        let branch = match branch {
            Some(branch) => branch.to_string(),
            None => crate::staging::VcStore::remote_default_branch(self, remote)?,
        };
        crate::staging::VcStore::fetch(self, remote, &branch)
    }

    fn pull(&mut self, remote: &str, branch: &str) -> VersionResult<()> {
        crate::staging::VcStore::pull(self, remote, branch).map(|_| ())
    }

    fn clone_remote(&mut self, url: &str, lazy: bool) -> VersionResult<()> {
        crate::staging::VcStore::clone_remote(self, url, lazy)
    }

    fn gc(&mut self) -> VersionResult<String> {
        crate::gc::gc_exclusive(self)?;
        Ok(crate::gc::collect_garbage(self).summary())
    }
}

pub fn dolt_add(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    if args.len() == 1 && text("dolt_add", &args[0])? == "-A" {
        return Ok(FuncValue::Integer(vc.add_all()? as i64));
    }
    if args.is_empty() {
        return Err(VersionError::IncorrectArity("dolt_add".to_string()));
    }
    let mut tables = Vec::with_capacity(args.len());
    for arg in args {
        tables.push(text("dolt_add", arg)?);
    }
    Ok(FuncValue::Integer(vc.add(&tables)? as i64))
}

pub fn dolt_commit(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    let (msg, all, author, force) = parse_commit_args(args)?;
    let author = author
        .as_ref()
        .map(|(name, email)| (name.as_str(), email.as_str()));
    let id = vc.commit(&msg, author, all, force)?;
    Ok(FuncValue::Text(id.to_hex()))
}

pub fn dolt_branch(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    match args {
        [] => Ok(FuncValue::Text(vc.list_branches().join("\n"))),
        [name] => {
            let name = text("dolt_branch", name)?;
            vc.create_branch(name)?;
            Ok(FuncValue::Text(name.to_string()))
        }
        [flag, name] if matches!(text("dolt_branch", flag)?, "-d" | "-D") => {
            let name = text("dolt_branch", name)?;
            vc.delete_branch(name)?;
            Ok(FuncValue::Text(name.to_string()))
        }
        _ => Err(VersionError::IncorrectArity("dolt_branch".to_string())),
    }
}

pub fn dolt_checkout(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    match args {
        [name] => {
            let name = text("dolt_checkout", name)?;
            vc.checkout(name)?;
            Ok(FuncValue::Text(name.to_string()))
        }
        _ => Err(VersionError::IncorrectArity("dolt_checkout".to_string())),
    }
}

pub fn dolt_tag(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    match args {
        [name] => {
            let name = text("dolt_tag", name)?;
            let at = vc.resolve("HEAD")?;
            vc.create_tag(name, at)?;
            Ok(FuncValue::Text(name.to_string()))
        }
        [flag, name] if text("dolt_tag", flag)? == "-d" => {
            let name = text("dolt_tag", name)?;
            vc.delete_tag(name)?;
            Ok(FuncValue::Text(name.to_string()))
        }
        [name, rev] => {
            let name = text("dolt_tag", name)?;
            let at = vc.resolve(text("dolt_tag", rev)?)?;
            vc.create_tag(name, at)?;
            Ok(FuncValue::Text(name.to_string()))
        }
        _ => Err(VersionError::IncorrectArity("dolt_tag".to_string())),
    }
}

pub fn dolt_active_branch(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    arity_exact("dolt_active_branch", args, 0)?;
    match vc.active_branch() {
        Some(name) => Ok(FuncValue::Text(name.to_string())),
        None => Ok(FuncValue::Null),
    }
}

pub fn dolt_hashof(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    arity_exact("dolt_hashof", args, 1)?;
    let id = vc.resolve(text("dolt_hashof", &args[0])?)?;
    Ok(FuncValue::Text(id.to_hex()))
}

pub fn dolt_hashof_table(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    let (name, revision) = match args {
        [name] => (text("dolt_hashof_table", name)?, None),
        [name, revision] => (
            text("dolt_hashof_table", name)?,
            Some(text("dolt_hashof_table", revision)?),
        ),
        _ => {
            return Err(VersionError::IncorrectArity(
                "dolt_hashof_table".to_string(),
            ))
        }
    };
    Ok(FuncValue::Text(vc.table_hash(name, revision)?))
}

pub fn dolt_hashof_db(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    let revision = match args {
        [] => None,
        [revision] => Some(text("dolt_hashof_db", revision)?),
        _ => return Err(VersionError::IncorrectArity("dolt_hashof_db".to_string())),
    };
    Ok(FuncValue::Text(vc.db_hash(revision)?))
}

pub fn dolt_config(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    match args {
        [key] => match vc.config_get(text("dolt_config", key)?) {
            Some(value) => Ok(FuncValue::Text(value)),
            None => Ok(FuncValue::Null),
        },
        [key, value] => {
            let value = text("dolt_config", value)?;
            vc.config_set(text("dolt_config", key)?, value);
            Ok(FuncValue::Text(value.to_string()))
        }
        _ => Err(VersionError::IncorrectArity("dolt_config".to_string())),
    }
}

pub fn dolt_status(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    arity_exact("dolt_status", args, 0)?;
    let text = vc
        .status()
        .iter()
        .map(|row| format!("{}|{}|{}", row.table, row.staged as i64, row.status.label()))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(FuncValue::Text(text))
}

pub fn dolt_reset(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    match args {
        [] => {
            vc.reset_soft()?;
            Ok(FuncValue::Integer(0))
        }
        [flag] => match text("dolt_reset", flag)? {
            "--soft" => {
                vc.reset_soft()?;
                Ok(FuncValue::Integer(0))
            }
            "--hard" => {
                vc.reset_hard()?;
                Ok(FuncValue::Integer(0))
            }
            _ => Err(VersionError::IncorrectArity("dolt_reset".to_string())),
        },
        _ => Err(VersionError::IncorrectArity("dolt_reset".to_string())),
    }
}

pub fn dolt_clean(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    arity_exact("dolt_clean", args, 0)?;
    let _ = vc.clean()?;
    Ok(FuncValue::Integer(0))
}

/// M1: `dolt_merge('branch'[, '--squash'|'--no-commit'|'--abort'|'-m' N])`.
/// `-m N` picks parent N of a merge commit as the merge base.
pub fn dolt_merge(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    match args {
        [flag] if text("dolt_merge", flag)? == "--abort" => {
            vc.merge_abort()?;
            Ok(FuncValue::Integer(0))
        }
        [branch] => Ok(merge_result(vc.merge_branch(
            text("dolt_merge", branch)?,
            false,
            false,
            None,
        )?)),
        [branch, flag] => {
            let branch = text("dolt_merge", branch)?;
            match text("dolt_merge", flag)? {
                "--squash" => Ok(merge_result(vc.merge_branch(branch, true, false, None)?)),
                "--no-commit" => Ok(merge_result(vc.merge_branch(branch, false, true, None)?)),
                _ => Err(VersionError::IncorrectArity("dolt_merge".to_string())),
            }
        }
        [branch, flag, n] if text("dolt_merge", flag)? == "-m" => {
            let branch = text("dolt_merge", branch)?;
            let n = text("dolt_merge", n)?
                .parse::<usize>()
                .map_err(|_| VersionError::IncorrectArity("dolt_merge".to_string()))?;
            if n < 1 {
                return Err(VersionError::IncorrectArity("dolt_merge".to_string()));
            }
            Ok(merge_result(vc.merge_branch(
                branch,
                false,
                false,
                Some(n - 1),
            )?))
        }
        _ => Err(VersionError::IncorrectArity("dolt_merge".to_string())),
    }
}

/// M2: `dolt_merge_base('a', 'b')`; `None` reads as NULL.
pub fn dolt_merge_base(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    arity_exact("dolt_merge_base", args, 2)?;
    let a = text("dolt_merge_base", &args[0])?;
    let b = text("dolt_merge_base", &args[1])?;
    match vc.merge_base(a, b)? {
        Some(id) => Ok(FuncValue::Text(id.to_hex())),
        None => Ok(FuncValue::Null),
    }
}

/// M3: `dolt_cherry_pick('<hash>'[, '-m N'])`.
pub fn dolt_cherry_pick(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    let (spec, parent) = parse_replay_args("dolt_cherry_pick", args)?;
    Ok(merge_result(vc.cherry_pick(spec, parent)?))
}

/// M4: `dolt_revert('<hash>'[, '-m N'])`.
pub fn dolt_revert(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    let (spec, parent) = parse_replay_args("dolt_revert", args)?;
    Ok(merge_result(vc.revert(spec, parent)?))
}

/// M5: `dolt_rebase('--continue'|'--abort'|'--onto' 'base')`, plus the
/// plan-table form: `dolt_rebase('--onto', 'base', '--plan')` pauses with the
/// plan, `dolt_rebase('--plan', 'drop <spec>', 'pick <spec>', ...)` edits it,
/// and `--continue` replays the edited plan.
pub fn dolt_rebase(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    match args {
        [flag] if text("dolt_rebase", flag)? == "--continue" => {
            Ok(FuncValue::Text(vc.rebase_continue()?.to_hex()))
        }
        [flag] if text("dolt_rebase", flag)? == "--abort" => {
            vc.rebase_abort()?;
            Ok(FuncValue::Integer(0))
        }
        [flag, onto] if text("dolt_rebase", flag)? == "--onto" => Ok(FuncValue::Text(
            vc.rebase_onto(text("dolt_rebase", onto)?)?.to_hex(),
        )),
        [flag, onto, mode]
            if text("dolt_rebase", flag)? == "--onto" && text("dolt_rebase", mode)? == "--plan" =>
        {
            let count = vc.rebase_plan(text("dolt_rebase", onto)?)?;
            Ok(FuncValue::Integer(count as i64))
        }
        [flag, steps @ ..] if text("dolt_rebase", flag)? == "--plan" => {
            let plan = parse_rebase_steps(vc, "dolt_rebase", steps)?;
            vc.rebase_edit_plan(plan)?;
            Ok(FuncValue::Integer(0))
        }
        _ => Err(VersionError::IncorrectArity("dolt_rebase".to_string())),
    }
}

/// Parse plan rows (`pick <spec>`, `drop <spec>`, `reword <spec> <msg>`,
/// `squash <spec>`, `fixup <spec>`) into steps, resolving each spec against
/// the still-paused HEAD.
fn parse_rebase_steps(
    vc: &mut dyn VcOperations,
    name: &str,
    args: &[FuncArg],
) -> VersionResult<Vec<RebaseStep>> {
    let mut steps = Vec::new();
    for arg in args {
        let row = text(name, arg)?;
        let mut parts = row.split_whitespace();
        let action = parts.next().unwrap_or("");
        let spec = parts
            .next()
            .ok_or_else(|| VersionError::IncorrectArity(name.to_string()))?;
        let id = vc.resolve(spec)?;
        let step = match action {
            "pick" => RebaseStep::Pick(id),
            "drop" => RebaseStep::Drop(id),
            "squash" => RebaseStep::Squash(id),
            "fixup" => RebaseStep::Fixup(id),
            "reword" => {
                let msg = parts.collect::<Vec<_>>().join(" ");
                if msg.is_empty() {
                    return Err(VersionError::IncorrectArity(name.to_string()));
                }
                RebaseStep::Reword(id, msg)
            }
            _ => return Err(VersionError::IncorrectArity(name.to_string())),
        };
        steps.push(step);
    }
    if steps.is_empty() {
        return Err(VersionError::IncorrectArity(name.to_string()));
    }
    Ok(steps)
}

/// M6: `dolt_conflicts_resolve('--ours'|'--theirs', table[, pk...])`.
pub fn dolt_conflicts_resolve(
    vc: &mut dyn VcOperations,
    args: &[FuncArg],
) -> VersionResult<FuncValue> {
    let [side, table, rest @ ..] = args else {
        return Err(VersionError::IncorrectArity(
            "dolt_conflicts_resolve".to_string(),
        ));
    };
    let side = match text("dolt_conflicts_resolve", side)? {
        "--ours" => ResolveSide::Ours,
        "--theirs" => ResolveSide::Theirs,
        other => return Err(VersionError::InvalidResolveSide(other.to_string())),
    };
    let table = text("dolt_conflicts_resolve", table)?;
    let pk: Vec<VcValue> = rest.iter().map(arg_to_value).collect();
    vc.resolve_conflict(table, &pk, side)?;
    Ok(FuncValue::Integer(0))
}

/// M7: `dolt_verify_constraints()` — records violations, returns the count.
pub fn dolt_verify_constraints(
    vc: &mut dyn VcOperations,
    args: &[FuncArg],
) -> VersionResult<FuncValue> {
    arity_exact("dolt_verify_constraints", args, 0)?;
    let count = vc.verify_constraints()?;
    Ok(FuncValue::Integer(count as i64))
}

pub fn dolt_remote(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    match args {
        [] => Err(VersionError::UsageDoltRemote),
        [_] => Err(VersionError::ActionAndNameRequired),
        [action, name] => match text("dolt_remote", action)? {
            "remove" => {
                vc.remote_remove(text("dolt_remote", name)?)?;
                Ok(FuncValue::Integer(0))
            }
            "add" => Err(VersionError::UrlRequiredForAdd),
            _ => Err(VersionError::UnknownRemoteAction),
        },
        [action, name, url] => match text("dolt_remote", action)? {
            "add" => {
                vc.remote_add(text("dolt_remote", name)?, text("dolt_remote", url)?)?;
                Ok(FuncValue::Integer(0))
            }
            "remove" => Err(VersionError::TooManyArguments),
            _ => Err(VersionError::UnknownRemoteAction),
        },
        _ => Err(VersionError::TooManyArguments),
    }
}

pub fn dolt_push(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    let (remote, branch, force) = match args {
        [remote, branch] => (
            text("dolt_push", remote)?,
            text("dolt_push", branch)?,
            false,
        ),
        [remote, branch, flag] => {
            let flag = text("dolt_push", flag)?;
            if flag != "--force" {
                return Err(VersionError::UnknownOption(flag.to_string()));
            }
            (text("dolt_push", remote)?, text("dolt_push", branch)?, true)
        }
        _ => return Err(VersionError::RemoteAndBranchRequired),
    };
    if remote.is_empty() || branch.is_empty() {
        return Err(VersionError::RemoteAndBranchRequired);
    }
    vc.push(remote, branch, force)?;
    Ok(FuncValue::Integer(0))
}

pub fn dolt_fetch(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    let (remote, branch) = match args {
        [] => return Err(VersionError::RemoteNameRequired),
        [remote] => (text("dolt_fetch", remote)?, None),
        [remote, branch] => (
            text("dolt_fetch", remote)?,
            Some(text("dolt_fetch", branch)?),
        ),
        _ => return Err(VersionError::UsageDoltFetch),
    };
    if remote.is_empty() {
        return Err(VersionError::RemoteNameRequired);
    }
    if branch == Some("") {
        return Err(VersionError::BranchNameRequired);
    }
    vc.fetch(remote, branch)?;
    Ok(FuncValue::Integer(0))
}

pub fn dolt_pull(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    let [remote, branch] = args else {
        return Err(VersionError::UsageDoltPull);
    };
    let remote = text("dolt_pull", remote)?;
    let branch = text("dolt_pull", branch)?;
    if remote.is_empty() || branch.is_empty() {
        return Err(VersionError::UsageDoltPull);
    }
    vc.pull(remote, branch)?;
    Ok(FuncValue::Integer(0))
}

pub fn dolt_clone(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    let (url, lazy) = match args {
        [] => return Err(VersionError::UrlRequired),
        [url] => (text("dolt_clone", url)?, false),
        [flag, url] if text("dolt_clone", flag)? == "--lazy" => (text("dolt_clone", url)?, true),
        _ => return Err(VersionError::UsageDoltClone),
    };
    if url.is_empty() {
        return Err(VersionError::UrlRequired);
    }
    vc.clone_remote(url, lazy)?;
    Ok(FuncValue::Integer(0))
}

pub fn dolt_gc(vc: &mut dyn VcOperations, args: &[FuncArg]) -> VersionResult<FuncValue> {
    arity_exact("dolt_gc", args, 0)?;
    Ok(FuncValue::Text(vc.gc()?))
}

pub fn dolt_creds_new(args: &[FuncArg]) -> VersionResult<FuncValue> {
    arity_exact("dolt_creds_new", args, 0)?;
    Ok(FuncValue::Text(crate::creds::issue_global().kid))
}

pub fn dolt_creds(args: &[FuncArg]) -> VersionResult<FuncValue> {
    use crate::creds::CredStore;

    match args {
        [] => Ok(FuncValue::Text(
            crate::creds::with_global(|store| store.list()).join("\n"),
        )),
        [action, kid] => {
            let action = text("dolt_creds", action)?;
            let kid = text("dolt_creds", kid)?;
            crate::creds::with_global(|store| match action {
                "use" => store.set_active(kid),
                "rm" => store.remove(kid),
                _ => Err(VersionError::UsageDoltCreds),
            })?;
            Ok(FuncValue::Text(kid.to_string()))
        }
        _ => Err(VersionError::UsageDoltCreds),
    }
}

/// Render a merge/replay driver result as its scalar value: the new commit
/// hash, the conflict report, or 0 when no commit was made.
fn merge_result(result: MergeResult) -> FuncValue {
    match result {
        MergeResult::Committed(id) => FuncValue::Text(id.to_hex()),
        MergeResult::Conflicts { rows, tables } => FuncValue::Text(format!(
            "merge conflict: {rows} conflicting rows in {tables} tables"
        )),
        MergeResult::SchemaConflict { table, detail } => {
            FuncValue::Text(format!("schema conflict in table: {table} ({detail})"))
        }
        MergeResult::Applied => FuncValue::Integer(0),
    }
}

/// Parse `dolt_cherry_pick`/`dolt_revert` args: a commit spec plus an
/// optional `-m N` parent pick. `-m N` selects the N-th parent (1-based).
fn parse_replay_args<'a>(
    name: &str,
    args: &[FuncArg<'a>],
) -> VersionResult<(&'a str, Option<usize>)> {
    match args {
        [spec] => Ok((text(name, spec)?, None)),
        [spec, flag, n] if text(name, flag)? == "-m" => {
            let n = text(name, n)?
                .parse::<usize>()
                .map_err(|_| VersionError::IncorrectArity(name.to_string()))?;
            if n < 1 {
                return Err(VersionError::IncorrectArity(name.to_string()));
            }
            Ok((text(name, spec)?, Some(n - 1)))
        }
        _ => Err(VersionError::IncorrectArity(name.to_string())),
    }
}

/// A resolve primary-key argument as a versioned value. A NULL cell is
/// preserved, never dropped: a NULL primary-key cell is a real key the store
/// must look up, and silently dropping the arg would resolve the wrong row.
fn arg_to_value(arg: &FuncArg) -> VcValue {
    match arg {
        FuncArg::Text(s) => VcValue::Text(s.to_string()),
        FuncArg::Integer(i) => VcValue::Integer(*i),
        FuncArg::Real(bits) => VcValue::Real(*bits),
        FuncArg::Blob(bytes) => VcValue::Blob(bytes.to_vec()),
        FuncArg::Null => VcValue::Null,
    }
}

pub fn dolt_version(args: &[FuncArg]) -> VersionResult<FuncValue> {
    arity_exact("dolt_version", args, 0)?;
    Ok(FuncValue::Text(format!("v{}", env!("CARGO_PKG_VERSION"))))
}

pub fn doltlite_engine(args: &[FuncArg]) -> VersionResult<FuncValue> {
    arity_exact("doltlite_engine", args, 0)?;
    Ok(FuncValue::Text("prolly".to_string()))
}

// ============================================================================
// Helpers. Kept below every dolt_* caller so the file reads top-down.
// ============================================================================

fn arity_exact(name: &str, args: &[FuncArg], n: usize) -> VersionResult<()> {
    if args.len() == n {
        Ok(())
    } else {
        Err(VersionError::IncorrectArity(name.to_string()))
    }
}

/// A text argument. Non-text SQL values surface as the arity error: the
/// doltlite error set has no type-error message, and core wiring converts SQL
/// types before dispatch.
fn text<'a>(name: &str, arg: &FuncArg<'a>) -> VersionResult<&'a str> {
    arg.as_text()
        .ok_or_else(|| VersionError::IncorrectArity(name.to_string()))
}

/// SHA-256 truncated to 20 bytes, lowercase hex. Table/db hashes reuse this
/// so equal key-sets hash equal regardless of commit history (F2).
fn hash_bytes(digest: &[u8]) -> String {
    hex::encode(&digest[..20])
}

/// Parsed `dolt_commit` arguments: message, stage-all flag, optional
/// (name, email) author override, and force flag.
type CommitArgs = (String, bool, Option<(String, String)>, bool);

/// Parse `dolt_commit` arguments into `CommitArgs`.
///
/// Flags may appear in any order; the remaining non-flag arguments join to
/// form the message. `--author` consumes the following argument, in dolt's
/// `--author "Name <email>"` form.
fn parse_commit_args(args: &[FuncArg]) -> VersionResult<CommitArgs> {
    let mut all = false;
    let mut force = false;
    let mut author = None;
    let mut msg_parts = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match text("dolt_commit", &args[i])? {
            "-A" | "--all" | "-am" => all = true,
            "-m" | "--message" => {}
            "--author" => {
                let raw = text(
                    "dolt_commit",
                    args.get(i + 1)
                        .ok_or_else(|| VersionError::IncorrectArity("dolt_commit".to_string()))?,
                )?;
                author = Some(parse_author(raw)?);
                i += 1;
            }
            "--force" => force = true,
            flag if flag.starts_with('-') => {
                return Err(VersionError::IncorrectArity("dolt_commit".to_string()))
            }
            word => msg_parts.push(word.to_string()),
        }
        i += 1;
    }
    if msg_parts.is_empty() {
        return Err(VersionError::IncorrectArity("dolt_commit".to_string()));
    }
    Ok((msg_parts.join(" "), all, author, force))
}

/// Parse dolt's `--author "Name <email>"` into a name/email pair.
fn parse_author(raw: &str) -> VersionResult<(String, String)> {
    let (name, rest) = raw.split_once('<').ok_or(VersionError::InvalidAuthor)?;
    let email = rest
        .split_once('>')
        .map(|(email, _)| email.trim().to_string())
        .unwrap_or_default();
    let name = name.trim().to_string();
    if name.is_empty() || email.is_empty() {
        return Err(VersionError::InvalidAuthor);
    }
    Ok((name, email))
}

// CORE-WIRE: each function is registered as a ScalarFunc by
// `extensions/core/src/versioning.rs`, which owns the per-connection
// `VcStore` (plus branch session) and converts `Value`s to `FuncArg`s before
// forwarding to the fn above:
//
//   "dolt_add"            -> funcs::dolt_add            (&mut VcStore, args)
//   "dolt_commit"         -> funcs::dolt_commit         (&mut VcStore, args)
//   "dolt_branch"         -> funcs::dolt_branch         (&mut VcStore, args)
//   "dolt_checkout"       -> funcs::dolt_checkout       (&mut VcStore, args)
//   "dolt_tag"            -> funcs::dolt_tag            (&mut VcStore, args)
//   "dolt_active_branch"  -> funcs::dolt_active_branch  (&VcStore, args)
//   "dolt_hashof"         -> funcs::dolt_hashof         (&VcStore, args)
//   "dolt_hashof_table"   -> funcs::dolt_hashof_table   (&VcStore, args)
//   "dolt_hashof_db"      -> funcs::dolt_hashof_db      (&VcStore, args)
//   "dolt_config"         -> funcs::dolt_config         (&mut VcStore, args)
//   "dolt_status"         -> funcs::dolt_status         (&VcStore, args)
//   "dolt_reset"          -> funcs::dolt_reset          (&mut VcStore, args)
//   "dolt_clean"          -> funcs::dolt_clean          (&mut VcStore, args)
//   "dolt_merge"          -> funcs::dolt_merge          (&mut VcStore, args)
//   "dolt_merge_base"     -> funcs::dolt_merge_base     (&mut VcStore, args)
//   "dolt_cherry_pick"    -> funcs::dolt_cherry_pick    (&mut VcStore, args)
//   "dolt_revert"         -> funcs::dolt_revert         (&mut VcStore, args)
//   "dolt_rebase"         -> funcs::dolt_rebase         (&mut VcStore, args)
//   "dolt_conflicts_resolve" -> funcs::dolt_conflicts_resolve (&mut VcStore, args)
//   "dolt_verify_constraints" -> funcs::dolt_verify_constraints (&mut VcStore, args)
//   "dolt_version"        -> funcs::dolt_version        (args)
//   "doltlite_engine"     -> funcs::doltlite_engine     (args)
//
// Unknown function names never reach this module: the SQL layer reports the
// normal "no such function" error before a ScalarFunc is constructed.
// Detached-HEAD handling is `staging::VcStore::guard_write`, which every
// mutating store method calls before changing state.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::staging::VcStore;
    use crate::vtab_log::{VcRead, VcRow, VcValue};

    fn store() -> VcStore {
        VcStore::new("main")
    }

    fn commit_store(s: &mut VcStore) {
        s.config_set("user.name", "Ada");
        s.config_set("user.email", "ada@example.com");
        s.track_table("t1");
        s.dolt_add(&["t1"]).unwrap();
        s.set_now(1);
        s.dolt_commit("first", None, false, false).unwrap();
    }

    #[test]
    fn version_and_engine() {
        assert_eq!(
            dolt_version(&[]).unwrap(),
            FuncValue::Text(format!("v{}", env!("CARGO_PKG_VERSION")))
        );
        assert_eq!(
            doltlite_engine(&[]).unwrap(),
            FuncValue::Text("prolly".into())
        );
        assert_eq!(
            dolt_version(&[FuncArg::Text("x")]).unwrap_err().to_string(),
            "incorrect number of arguments to dolt_version"
        );
    }

    #[test]
    fn remote_scalars_parse_and_run() {
        let mut s = store();
        commit_store(&mut s);
        assert_eq!(
            dolt_remote(
                &mut s,
                &[
                    FuncArg::Text("add"),
                    FuncArg::Text("origin"),
                    FuncArg::Text("mem://funcs-remote-scalars"),
                ],
            )
            .unwrap(),
            FuncValue::Integer(0)
        );
        assert_eq!(
            dolt_push(&mut s, &[FuncArg::Text("origin"), FuncArg::Text("main")],).unwrap(),
            FuncValue::Integer(0)
        );

        let mut clone = store();
        assert_eq!(
            dolt_clone(&mut clone, &[FuncArg::Text("mem://funcs-remote-scalars")],).unwrap(),
            FuncValue::Integer(0)
        );
        assert_eq!(clone.head_commit(), s.head_commit());
    }

    #[test]
    fn remote_scalar_errors_are_exact() {
        let mut s = store();
        assert_eq!(
            dolt_remote(&mut s, &[]).unwrap_err(),
            VersionError::UsageDoltRemote
        );
        assert_eq!(
            dolt_push(&mut s, &[FuncArg::Text("origin")]).unwrap_err(),
            VersionError::RemoteAndBranchRequired
        );
        assert_eq!(
            dolt_push(
                &mut s,
                &[
                    FuncArg::Text("origin"),
                    FuncArg::Text("main"),
                    FuncArg::Text("--nope"),
                ],
            )
            .unwrap_err(),
            VersionError::UnknownOption("--nope".to_string())
        );
        assert_eq!(
            dolt_fetch(&mut s, &[]).unwrap_err(),
            VersionError::RemoteNameRequired
        );
        assert_eq!(
            dolt_clone(&mut s, &[]).unwrap_err(),
            VersionError::UrlRequired
        );
    }

    #[test]
    fn gc_and_credential_scalars_run() {
        let mut s = store();
        assert_eq!(
            dolt_gc(&mut s, &[]).unwrap(),
            FuncValue::Text("0 chunks removed, 0 chunks kept".to_string())
        );
        let FuncValue::Text(kid) = dolt_creds_new(&[]).unwrap() else {
            panic!("credential id")
        };
        assert_eq!(kid.len(), 64);
        let FuncValue::Text(list) = dolt_creds(&[]).unwrap() else {
            panic!("credential list")
        };
        assert_eq!(list.lines().next(), Some(kid.as_str()));
    }

    #[test]
    fn add_commit_hashof_roundtrip() {
        let mut s = store();
        commit_store(&mut s);
        let id = s.head_commit().unwrap();
        assert_eq!(
            dolt_hashof(&mut s, &[FuncArg::Text("HEAD")]).unwrap(),
            FuncValue::Text(id.to_hex())
        );
    }

    #[test]
    fn add_arity_enforced() {
        let mut s = store();
        assert_eq!(
            dolt_add(&mut s, &[]).unwrap_err().to_string(),
            "incorrect number of arguments to dolt_add"
        );
    }

    #[test]
    fn hashof_working_reads_sentinel_while_set_lives() {
        // No content exists behind WORKING yet (O4), so its hash is the
        // documented placeholder id, not content. Pinned so O4's content hashing
        // visibly replaces this behavior.
        let mut s = store();
        assert_eq!(
            dolt_hashof(&mut s, &[FuncArg::Text("WORKING")])
                .unwrap_err()
                .to_string(),
            "invalid revision spec: 'WORKING'"
        );
        s.track_table("t1");
        assert_eq!(
            dolt_hashof(&mut s, &[FuncArg::Text("WORKING")]).unwrap(),
            FuncValue::Text(crate::vtab_log::working_id().to_hex())
        );
    }

    #[test]
    fn add_returns_count_and_minus_a_stages_all() {
        let mut s = store();
        s.track_table("t1");
        s.track_table("t2");
        assert_eq!(
            dolt_add(&mut s, &[FuncArg::Text("-A")]).unwrap(),
            FuncValue::Integer(2)
        );
        assert!(s.status().iter().all(|r| r.staged));
    }

    #[test]
    fn commit_gate_nothing_to_commit() {
        let mut s = store();
        assert_eq!(
            dolt_commit(&mut s, &[FuncArg::Text("msg")])
                .unwrap_err()
                .to_string(),
            "nothing to commit"
        );
    }

    #[test]
    fn commit_flag_forms() {
        let mut s = store();
        commit_store(&mut s);
        for (i, args) in [
            vec![FuncArg::Text("-m"), FuncArg::Text("via -m")],
            vec![FuncArg::Text("-A"), FuncArg::Text("via -A")],
            vec![FuncArg::Text("-am"), FuncArg::Text("via -am")],
            vec![
                FuncArg::Text("--author"),
                FuncArg::Text("Grace <grace@example.com>"),
                FuncArg::Text("-m"),
                FuncArg::Text("via --author"),
            ],
        ]
        .into_iter()
        .enumerate()
        {
            s.dolt_add(&["t1"]).unwrap();
            s.set_now(i as i64 + 2);
            let id = dolt_commit(&mut s, &args).unwrap();
            assert_eq!(id, FuncValue::Text(s.head_commit().unwrap().to_hex()));
        }
    }

    #[test]
    fn commit_force_flag_bypasses_violations() {
        let mut s = store();
        commit_store(&mut s);
        s.violations.push(crate::constraints::Violation {
            table: "t1".to_string(),
            kind: crate::constraints::ViolationKind::Unique,
            row_pk: Vec::new(),
            detail: String::new(),
        });
        s.dolt_add(&["t1"]).unwrap();
        s.set_now(2);
        assert_eq!(
            dolt_commit(&mut s, &[FuncArg::Text("msg")])
                .unwrap_err()
                .to_string(),
            "cannot commit: constraint violations remain"
        );
        assert!(dolt_commit(&mut s, &[FuncArg::Text("--force"), FuncArg::Text("msg")]).is_ok());
    }

    #[test]
    fn branch_list_create_checkout() {
        let mut s = store();
        commit_store(&mut s);
        dolt_branch(&mut s, &[FuncArg::Text("dev")]).unwrap();
        let list = dolt_branch(&mut s, &[]).unwrap();
        assert_eq!(list, FuncValue::Text("dev\nmain".into()));
        dolt_checkout(&mut s, &[FuncArg::Text("dev")]).unwrap();
        assert_eq!(
            dolt_active_branch(&mut s, &[]).unwrap(),
            FuncValue::Text("dev".into())
        );
        assert_eq!(
            dolt_branch(&mut s, &[FuncArg::Text("dev")])
                .unwrap_err()
                .to_string(),
            "branch 'dev' already exists"
        );
    }

    #[test]
    fn tag_create_resolve_and_delete() {
        let mut s = store();
        commit_store(&mut s);
        dolt_tag(&mut s, &[FuncArg::Text("v1")]).unwrap();
        assert_eq!(dolt_hashof(&mut s, &[FuncArg::Text("v1")]).unwrap(), {
            let id = s.head_commit().unwrap();
            FuncValue::Text(id.to_hex())
        });
        assert_eq!(
            dolt_tag(&mut s, &[FuncArg::Text("-d"), FuncArg::Text("v1")]).unwrap(),
            FuncValue::Text("v1".into())
        );
        // A deleted tag name is no longer resolvable through the spec parser,
        // which maps bare names to branches first.
        assert_eq!(
            dolt_hashof(&mut s, &[FuncArg::Text("v1")])
                .unwrap_err()
                .to_string(),
            "branch not found: v1"
        );
    }

    #[test]
    fn config_get_set() {
        let mut s = store();
        assert_eq!(
            dolt_config(&mut s, &[FuncArg::Text("user.name")]).unwrap(),
            FuncValue::Null
        );
        dolt_config(
            &mut s,
            &[FuncArg::Text("user.name"), FuncArg::Text("Grace")],
        )
        .unwrap();
        assert_eq!(
            dolt_config(&mut s, &[FuncArg::Text("user.name")]).unwrap(),
            FuncValue::Text("Grace".into())
        );
    }

    #[test]
    fn status_reset_clean_roundtrip() {
        let mut s = store();
        s.config_set("user.name", "Ada");
        s.config_set("user.email", "ada@example.com");
        s.track_table("t1");
        assert_eq!(
            dolt_add(&mut s, &[FuncArg::Text("t1")]).unwrap(),
            FuncValue::Integer(1)
        );
        assert_eq!(
            dolt_status(&mut s, &[]).unwrap(),
            FuncValue::Text("t1|1|new table".into())
        );
        assert_eq!(dolt_reset(&mut s, &[]).unwrap(), FuncValue::Integer(0));
        assert_eq!(
            dolt_status(&mut s, &[]).unwrap(),
            FuncValue::Text("t1|0|modified".into())
        );
        assert_eq!(
            dolt_reset(&mut s, &[FuncArg::Text("--hard")]).unwrap(),
            FuncValue::Integer(0)
        );
        assert_eq!(
            dolt_status(&mut s, &[]).unwrap(),
            FuncValue::Text("".into())
        );
        assert_eq!(dolt_clean(&mut s, &[]).unwrap(), FuncValue::Integer(0));
    }

    #[test]
    fn hashof_db_history_independent() {
        let mut a = store();
        let mut b = store();
        for (store, names) in [(&mut a, ["t1", "t2"]), (&mut b, ["t2", "t1"])] {
            for name in names {
                store.track_table(name);
                store.apply_work(name, Vec::new(), Vec::new(), Vec::new(), String::new());
            }
        }
        assert_eq!(
            dolt_hashof_db(&mut a, &[]).unwrap(),
            dolt_hashof_db(&mut b, &[]).unwrap()
        );
        a.track_table("t3");
        a.apply_work("t3", Vec::new(), Vec::new(), Vec::new(), String::new());
        assert_ne!(
            dolt_hashof_db(&mut a, &[]).unwrap(),
            dolt_hashof_db(&mut b, &[]).unwrap()
        );
    }

    fn apply_and_commit(s: &mut VcStore, msg: &str) {
        let tables: Vec<String> = s.work_tables_content();
        for t in &tables {
            if !s.tables().contains(t) {
                s.track_table(t);
            }
        }
        let names: Vec<&str> = tables.iter().map(|t| t.as_str()).collect();
        let args: Vec<FuncArg> = names.iter().map(|n| FuncArg::Text(n)).collect();
        dolt_add(s, &args).unwrap();
        dolt_commit(s, &[FuncArg::Text(msg)]).unwrap();
    }

    fn seed_clean_feature() -> (VcStore, String) {
        let mut s = store();
        s.config_set("user.name", "Ada");
        s.config_set("user.email", "ada@example.com");
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("b".into())]),
            ],
            String::new(),
        );
        apply_and_commit(&mut s, "seed");
        dolt_branch(&mut s, &[FuncArg::Text("feature")]).unwrap();
        dolt_checkout(&mut s, &[FuncArg::Text("feature")]).unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("b".into())]),
                VcRow::new(vec![VcValue::Integer(3), VcValue::Text("feat".into())]),
            ],
            String::new(),
        );
        apply_and_commit(&mut s, "feature work");
        let feature = s.head_commit().unwrap().to_hex();
        dolt_checkout(&mut s, &[FuncArg::Text("main")]).unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("b".into())]),
                VcRow::new(vec![VcValue::Integer(4), VcValue::Text("main".into())]),
            ],
            String::new(),
        );
        apply_and_commit(&mut s, "main work");
        (s, feature)
    }

    fn seed_divergent() -> (VcStore, String) {
        let mut s = store();
        s.config_set("user.name", "Ada");
        s.config_set("user.email", "ada@example.com");
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("base".into()),
            ])],
            String::new(),
        );
        apply_and_commit(&mut s, "seed");
        dolt_branch(&mut s, &[FuncArg::Text("feature")]).unwrap();
        dolt_checkout(&mut s, &[FuncArg::Text("feature")]).unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("theirs".into()),
            ])],
            String::new(),
        );
        apply_and_commit(&mut s, "feature work");
        let feature = s.head_commit().unwrap().to_hex();
        dolt_checkout(&mut s, &[FuncArg::Text("main")]).unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("ours".into()),
            ])],
            String::new(),
        );
        apply_and_commit(&mut s, "main work");
        (s, feature)
    }

    #[test]
    fn merge_clean_returns_commit_hash() {
        let (mut s, feature) = seed_clean_feature();
        let result = dolt_merge(&mut s, &[FuncArg::Text(&feature)]).unwrap();
        match result {
            FuncValue::Text(hash) => assert_eq!(hash, s.head_commit().unwrap().to_hex()),
            other => panic!("expected hash, got {other:?}"),
        }
    }

    #[test]
    fn merge_conflict_returns_report_and_resolve_unblocks() {
        let (mut s, feature) = seed_divergent();
        let result = dolt_merge(&mut s, &[FuncArg::Text(&feature)]).unwrap();
        assert_eq!(
            result,
            FuncValue::Text("merge conflict: 1 conflicting rows in 1 tables".into())
        );
        // Commit is refused while conflicted.
        assert_eq!(
            dolt_commit(&mut s, &[FuncArg::Text("x")])
                .unwrap_err()
                .to_string(),
            "cannot commit: unresolved merge conflicts"
        );
        // Resolve --theirs then commit.
        assert_eq!(
            dolt_conflicts_resolve(
                &mut s,
                &[
                    FuncArg::Text("--theirs"),
                    FuncArg::Text("t"),
                    FuncArg::Integer(1)
                ],
            )
            .unwrap(),
            FuncValue::Integer(0)
        );
        assert!(dolt_commit(&mut s, &[FuncArg::Text("merged")]).is_ok());
    }

    #[test]
    fn merge_abort_flag_errors_without_merge() {
        let mut s = store();
        assert_eq!(
            dolt_merge(&mut s, &[FuncArg::Text("--abort")])
                .unwrap_err()
                .to_string(),
            "no merge in progress"
        );
    }

    #[test]
    fn merge_schema_conflict_reports_error_and_status_flag() {
        let mut s = store();
        s.config_set("user.name", "Ada");
        s.config_set("user.email", "ada@example.com");
        s.apply_work(
            "t",
            vec!["a".to_string(), "b".to_string()],
            vec!["a".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("x".into()),
            ])],
            String::new(),
        );
        apply_and_commit(&mut s, "seed");
        dolt_branch(&mut s, &[FuncArg::Text("feature")]).unwrap();
        dolt_checkout(&mut s, &[FuncArg::Text("feature")]).unwrap();
        s.apply_work(
            "t",
            vec!["a".to_string(), "b".to_string()],
            vec!["b".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("x".into()),
            ])],
            String::new(),
        );
        apply_and_commit(&mut s, "feature work");
        dolt_checkout(&mut s, &[FuncArg::Text("main")]).unwrap();
        s.apply_work(
            "t",
            vec!["a".to_string(), "b".to_string()],
            vec!["a".to_string(), "b".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("x".into()),
            ])],
            String::new(),
        );
        apply_and_commit(&mut s, "main work");
        let result = dolt_merge(&mut s, &[FuncArg::Text("feature")]).unwrap();
        let FuncValue::Text(message) = result else {
            panic!("expected schema conflict message, got {result:?}");
        };
        assert!(
            message.starts_with("schema conflict in table: t ("),
            "{message}"
        );
        // The status vtable marks the schema conflict.
        let rows = s.merge_status_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].schema_conflict, 1);
        assert_eq!(rows[0].state, "conflicted");
    }

    #[test]
    fn merge_m_parent_selector_through_scalar() {
        let (mut s, feature) = seed_clean_feature();
        // seed_clean_feature's feature tip is a single-parent commit, so -m 1
        // resolves to parent 0 and the merge still lands.
        let result = dolt_merge(
            &mut s,
            &[
                FuncArg::Text(&feature),
                FuncArg::Text("-m"),
                FuncArg::Text("1"),
            ],
        )
        .unwrap();
        match result {
            FuncValue::Text(hash) => assert_eq!(hash, s.head_commit().unwrap().to_hex()),
            other => panic!("expected hash, got {other:?}"),
        }
    }

    #[test]
    fn merge_refused_while_conflicted_through_scalar() {
        let (mut s, feature) = seed_divergent();
        let result = dolt_merge(&mut s, &[FuncArg::Text(&feature)]).unwrap();
        assert!(matches!(result, FuncValue::Text(_)));
        let err = dolt_merge(&mut s, &[FuncArg::Text(&feature)]).unwrap_err();
        assert_eq!(err.to_string(), "merge already in progress");
    }

    #[test]
    fn rebase_plan_with_drop_produces_single_commit() {
        let mut s = store();
        s.config_set("user.name", "Ada");
        s.config_set("user.email", "ada@example.com");
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![VcRow::new(vec![
                VcValue::Integer(1),
                VcValue::Text("a".into()),
            ])],
            String::new(),
        );
        apply_and_commit(&mut s, "seed");
        dolt_branch(&mut s, &[FuncArg::Text("feature")]).unwrap();
        dolt_checkout(&mut s, &[FuncArg::Text("feature")]).unwrap();
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("first".into())]),
            ],
            String::new(),
        );
        apply_and_commit(&mut s, "first");
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(2), VcValue::Text("first".into())]),
                VcRow::new(vec![VcValue::Integer(3), VcValue::Text("second".into())]),
            ],
            String::new(),
        );
        apply_and_commit(&mut s, "second");
        dolt_checkout(&mut s, &[FuncArg::Text("main")]).unwrap();
        dolt_checkout(&mut s, &[FuncArg::Text("feature")]).unwrap();
        // Pause with the plan, drop the oldest commit (HEAD~2), continue.
        assert_eq!(
            dolt_rebase(
                &mut s,
                &[
                    FuncArg::Text("--onto"),
                    FuncArg::Text("main"),
                    FuncArg::Text("--plan")
                ],
            )
            .unwrap(),
            FuncValue::Integer(2)
        );
        assert_eq!(
            dolt_rebase(
                &mut s,
                &[
                    FuncArg::Text("--plan"),
                    FuncArg::Text("drop HEAD~1"),
                    FuncArg::Text("pick HEAD")
                ],
            )
            .unwrap(),
            FuncValue::Integer(0)
        );
        let result = dolt_rebase(&mut s, &[FuncArg::Text("--continue")]).unwrap();
        match result {
            FuncValue::Text(hash) => assert_eq!(hash, s.head_commit().unwrap().to_hex()),
            other => panic!("expected hash, got {other:?}"),
        }
        // One replay commit carrying only the second commit's change.
        let tip = s.head_commit().unwrap();
        let rows = s.table_rows("t", &tip).unwrap();
        assert!(rows.iter().any(|r| r.values.contains(&VcValue::Integer(3))));
        assert!(rows
            .iter()
            .all(|r| !r.values.contains(&VcValue::Integer(2))));
    }

    #[test]
    fn merge_no_commit_and_squash_flags() {
        let (mut s, feature) = seed_clean_feature();
        assert_eq!(
            dolt_merge(
                &mut s,
                &[FuncArg::Text(&feature), FuncArg::Text("--no-commit")]
            )
            .unwrap(),
            FuncValue::Integer(0)
        );
        // The merge state is open; a commit now creates two parents.
        let head = s.head_commit().unwrap();
        s.set_now(50);
        dolt_commit(&mut s, &[FuncArg::Text("finish")]).unwrap();
        assert_ne!(s.head_commit().unwrap(), head);
    }

    #[test]
    fn merge_base_returns_lca_or_null() {
        let (mut s, _feature) = seed_clean_feature();
        let base =
            dolt_merge_base(&mut s, &[FuncArg::Text("main"), FuncArg::Text("feature")]).unwrap();
        match base {
            FuncValue::Text(hash) => assert!(hash.len() == 40),
            other => panic!("expected hash, got {other:?}"),
        }
        // Unrelated store has no LCA.
        let mut empty = store();
        empty.track_table("x");
        assert_eq!(
            dolt_merge_base(&mut empty, &[FuncArg::Text("a"), FuncArg::Text("b")])
                .unwrap_err()
                .to_string(),
            "branch not found: a"
        );
    }

    #[test]
    fn cherry_pick_returns_new_commit_hash() {
        let (mut s, feature) = seed_clean_feature();
        let result = dolt_cherry_pick(&mut s, &[FuncArg::Text(&feature)]).unwrap();
        match result {
            FuncValue::Text(hash) => assert_eq!(hash, s.head_commit().unwrap().to_hex()),
            other => panic!("expected hash, got {other:?}"),
        }
    }

    #[test]
    fn cherry_pick_arity_and_m_flag() {
        let (mut s, feature) = seed_clean_feature();
        assert_eq!(
            dolt_cherry_pick(&mut s, &[]).unwrap_err().to_string(),
            "incorrect number of arguments to dolt_cherry_pick"
        );
        assert_eq!(
            dolt_cherry_pick(
                &mut s,
                &[
                    FuncArg::Text(&feature),
                    FuncArg::Text("-m"),
                    FuncArg::Text("1")
                ],
            )
            .unwrap(),
            FuncValue::Text(s.head_commit().unwrap().to_hex())
        );
    }

    #[test]
    fn revert_returns_inverse_commit() {
        let (mut s, _feature) = seed_clean_feature();
        let main = s.head_commit().unwrap().to_hex();
        let result = dolt_revert(&mut s, &[FuncArg::Text(&main)]).unwrap();
        match result {
            FuncValue::Text(hash) => assert_eq!(hash, s.head_commit().unwrap().to_hex()),
            other => panic!("expected hash, got {other:?}"),
        }
    }

    #[test]
    fn rebase_onto_returns_new_head() {
        let (mut s, _feature) = seed_clean_feature();
        dolt_checkout(&mut s, &[FuncArg::Text("feature")]).unwrap();
        let head_before = s.head_commit().unwrap();
        let result =
            dolt_rebase(&mut s, &[FuncArg::Text("--onto"), FuncArg::Text("main")]).unwrap();
        match result {
            FuncValue::Text(hash) => assert_eq!(hash, s.head_commit().unwrap().to_hex()),
            other => panic!("expected hash, got {other:?}"),
        }
        assert_ne!(s.head_commit().unwrap(), head_before);
    }

    #[test]
    fn rebase_conflict_reports_and_continue_resumes() {
        let (mut s, feature) = seed_divergent();
        dolt_checkout(&mut s, &[FuncArg::Text("feature")]).unwrap();
        let err =
            dolt_rebase(&mut s, &[FuncArg::Text("--onto"), FuncArg::Text("main")]).unwrap_err();
        assert!(err.to_string().starts_with("rebase conflict at "), "{err}");
        assert!(
            err.to_string().ends_with(": resolve and --continue"),
            "{err}"
        );
        // Atomic: feature's tip never moved.
        let head = s.head_commit().unwrap();
        dolt_conflicts_resolve(
            &mut s,
            &[
                FuncArg::Text("--ours"),
                FuncArg::Text("t"),
                FuncArg::Integer(1),
            ],
        )
        .unwrap();
        let result = dolt_rebase(&mut s, &[FuncArg::Text("--continue")]).unwrap();
        match result {
            FuncValue::Text(hash) => assert_eq!(hash, s.head_commit().unwrap().to_hex()),
            other => panic!("expected hash, got {other:?}"),
        }
        assert_ne!(s.head_commit().unwrap(), head);
        let _ = feature;
    }

    #[test]
    fn rebase_no_progress_errors() {
        let mut s = store();
        assert_eq!(
            dolt_rebase(&mut s, &[FuncArg::Text("--continue")])
                .unwrap_err()
                .to_string(),
            "no rebase in progress"
        );
        assert_eq!(
            dolt_rebase(&mut s, &[FuncArg::Text("--abort")])
                .unwrap_err()
                .to_string(),
            "no rebase in progress"
        );
    }

    #[test]
    fn conflicts_resolve_invalid_side_errors_exact() {
        let (mut s, feature) = seed_divergent();
        dolt_merge(&mut s, &[FuncArg::Text(&feature)]).unwrap();
        let err = dolt_conflicts_resolve(
            &mut s,
            &[
                FuncArg::Text("--mine"),
                FuncArg::Text("t"),
                FuncArg::Integer(1),
            ],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid resolve side: '--mine' (want '--ours' or '--theirs')"
        );
    }

    #[test]
    fn arg_to_value_preserves_null_pk_cells() {
        assert_eq!(arg_to_value(&FuncArg::Null), VcValue::Null);
        assert_eq!(arg_to_value(&FuncArg::Integer(7)), VcValue::Integer(7));
        assert_eq!(
            arg_to_value(&FuncArg::Real(1.5f64.to_bits())),
            VcValue::real(1.5)
        );
        assert_eq!(
            arg_to_value(&FuncArg::Blob(&[0x00, 0xff])),
            VcValue::Blob(vec![0x00, 0xff])
        );
        assert_eq!(
            arg_to_value(&FuncArg::Text("k")),
            VcValue::Text("k".to_string())
        );
    }

    #[test]
    fn conflicts_resolve_with_null_pk_reaches_store() {
        // A NULL primary-key cell is a real key: the resolve must not silently
        // drop the argument, or the store would look up an empty key.
        let (mut s, feature) = seed_divergent();
        dolt_merge(&mut s, &[FuncArg::Text(&feature)]).unwrap();
        s.conflicts[0].pk = vec![VcValue::Null];
        // The store still resolves by the NULL key; a wrong side is what
        // errors, proving the NULL cell was passed through untouched.
        assert!(dolt_conflicts_resolve(
            &mut s,
            &[FuncArg::Text("--ours"), FuncArg::Text("t"), FuncArg::Null],
        )
        .is_ok());
        assert!(s.conflicts.is_empty());
    }

    #[test]
    fn verify_constraints_counts_and_gates_commit() {
        let (mut s, _feature) = seed_clean_feature();
        // Introduce a duplicate primary-key value in the working set.
        s.apply_work(
            "t",
            vec!["id".to_string(), "v".to_string()],
            vec!["id".to_string()],
            vec![
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
                VcRow::new(vec![VcValue::Integer(1), VcValue::Text("a".into())]),
            ],
            String::new(),
        );
        let count = dolt_verify_constraints(&mut s, &[]).unwrap();
        assert_eq!(count, FuncValue::Integer(1));
        // Commit is refused; --force bypasses.
        dolt_add(&mut s, &[FuncArg::Text("t")]).unwrap();
        assert_eq!(
            dolt_commit(&mut s, &[FuncArg::Text("x")])
                .unwrap_err()
                .to_string(),
            "cannot commit: constraint violations remain"
        );
        assert!(dolt_commit(&mut s, &[FuncArg::Text("--force"), FuncArg::Text("x")]).is_ok());
    }
}
