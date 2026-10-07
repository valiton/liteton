use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::{Change, DbValue, Plan};
use crate::vscdb::{SecretWriter, StateDb};

/// Backs up every touched file into `backup_dir`, then writes the plan.
/// `secrets` must be provided when the plan writes encrypted values.
pub fn apply(plan: &Plan, backup_dir: &Path, secrets: Option<&SecretWriter>) -> Result<()> {
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
    let mut done: Vec<&Path> = Vec::new();
    for change in &plan.changes {
        let path = change.path();
        if done.contains(&path) || !path.exists() {
            continue;
        }
        crate::config::ensure_private_dir(backup_dir)?;
        let dest = backup_dir.join(path.file_name().unwrap_or_default());
        match change {
            Change::File { .. } => {
                fs::copy(path, &dest).with_context(|| format!("backing up {}", path.display()))?;
            }
            Change::DbItem { .. } => StateDb::open_readonly(path)?.backup_to(&dest)?,
        }
        done.push(path);
    }
    Ok(())
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
