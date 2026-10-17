use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::{Change, CommandBackup, DbValue, Plan, command, read_optional};
use crate::vscdb::{SecretWriter, StateDb};

#[derive(Debug)]
pub struct StalePlan {
    pub path: PathBuf,
}

impl std::fmt::Display for StalePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} changed since the preview", self.path.display())
    }
}

impl std::error::Error for StalePlan {}

/// Backs up every touched file into `backup_dir`, then writes the plan.
/// `secrets` must be provided when the plan writes encrypted values.
/// Fails with [`StalePlan`] before writing anything if a file no longer matches the plan.
pub fn apply(plan: &Plan, backup_dir: &Path, secrets: Option<&SecretWriter>) -> Result<()> {
    for change in &plan.changes {
        if let Change::File { path, before, .. } = change
            && read_optional(path)? != *before
        {
            return Err(StalePlan { path: path.clone() }.into());
        }
    }
    let backup_dir = backup_dir.join(plan.harness.key());
    backup(plan, &backup_dir)?;

    let mut db_items: BTreeMap<PathBuf, Vec<(String, Option<String>)>> = BTreeMap::new();
    for change in &plan.changes {
        match change {
            Change::File {
                path,
                after,
                private,
                ..
            } => write_file(path, after, *private)?,
            Change::DbItem { db, key, value, .. } => {
                let value = match value {
                    DbValue::Text(text) => Some(text.clone()),
                    DbValue::Secret(plaintext) => Some(
                        secrets
                            .ok_or_else(|| anyhow!("secret storage is not unlocked"))?
                            .encode(plaintext),
                    ),
                    DbValue::Delete => None,
                };
                db_items
                    .entry(db.clone())
                    .or_default()
                    .push((key.clone(), value));
            }
            Change::Command { program, steps, .. } => {
                for step in steps {
                    command::run(program, step)?;
                }
            }
        }
    }
    for (db, items) in db_items {
        StateDb::open(&db)?
            .apply(&items)
            .with_context(|| format!("writing {}", db.display()))?;
    }
    Ok(())
}

fn backup(plan: &Plan, backup_dir: &Path) -> Result<()> {
    for change in &plan.changes {
        if let Change::Command {
            program,
            backup: Some(CommandBackup { step, file_name }),
            ..
        } = change
        {
            let saved = command::run(program, step)?;
            crate::config::ensure_private_dir(backup_dir)?;
            write_file(
                &backup_path(backup_dir, Path::new(file_name), false),
                &saved,
                true,
            )?;
        }
    }
    let paths: BTreeSet<&Path> = plan.changes.iter().filter_map(Change::path).collect();
    let mut done: Vec<&Path> = Vec::new();
    for change in &plan.changes {
        let Some(path) = change.path() else {
            continue;
        };
        if done.contains(&path) || !path.exists() {
            continue;
        }
        crate::config::ensure_private_dir(backup_dir)?;
        let shared_name = paths
            .iter()
            .filter(|p| p.file_name() == path.file_name())
            .count()
            > 1;
        let dest = backup_path(backup_dir, path, shared_name);
        match change {
            Change::File { .. } => {
                fs::copy(path, &dest).with_context(|| format!("backing up {}", path.display()))?;
            }
            Change::DbItem { .. } => StateDb::open_readonly(path)?.backup_to(&dest)?,
            Change::Command { .. } => {}
        }
        done.push(path);
    }
    Ok(())
}

/// Files that share a name (one `chatLanguageModels.json` per VSCode profile) get their parent
/// folder's name as a prefix.
fn backup_path(backup_dir: &Path, path: &Path, shared_name: bool) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let prefix = path
        .parent()
        .and_then(Path::file_name)
        .filter(|_| shared_name)
        .map(|p| format!("{}-", p.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| match n {
            1 => backup_dir.join(format!("{prefix}{name}")),
            n => backup_dir.join(format!("{prefix}{n}-{name}")),
        })
        .find(|candidate| !candidate.exists())
        .expect("an unused backup name")
}

/// Writes next to the target and renames, so a crash CAN'T leave a half-written config.
/// Symlinked configs (e.g. from a dotfiles repo) are written through to their target.
fn write_file(path: &Path, contents: &str, private: bool) -> Result<()> {
    let resolved;
    let path = if path.is_symlink() {
        resolved =
            fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
        resolved.as_path()
    } else {
        path
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("liteton-tmp");
    fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    let mode = if private {
        Some(0o600)
    } else {
        fs::metadata(path).ok().map(|m| m.permissions().mode())
    };
    if let Some(mode) = mode {
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}
