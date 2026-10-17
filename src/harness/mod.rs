pub mod apply;
pub mod command;
pub mod cursor;
pub mod opencode;
pub mod vscode;

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use clap::ValueEnum;

pub use command::{CommandBackup, CommandStep};

use crate::config::{
    Config, Credentials, HarnessRecord, InstallState, home_dir, xdg_config_home, xdg_data_home,
};
use crate::litellm::ModelSpec;
use crate::vscdb::ElectronApp;
use opencode::{App, OpenCode, OpencodeCli};

/// Name of the provider entry liteton owns in every harness.
pub const PROVIDER_ID: &str = "litellm";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum)]
pub enum HarnessId {
    Vscode,
    Opencode,
    Opencode2,
    Kilo,
    Cursor,
}

impl HarnessId {
    pub fn key(self) -> &'static str {
        match self {
            Self::Vscode => "vscode",
            Self::Opencode => "opencode",
            Self::Opencode2 => "opencode2",
            Self::Kilo => "kilo",
            Self::Cursor => "cursor",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Vscode => "VSCode",
            Self::Opencode => "opencode",
            Self::Opencode2 => "opencode 2",
            Self::Kilo => "Kilo Code",
            Self::Cursor => "Cursor",
        }
    }
}

pub struct InstallCtx<'a> {
    pub creds: &'a Credentials,
    pub models: &'a [ModelSpec],
    pub config: &'a Config,
}

impl InstallCtx<'_> {
    pub fn openai_base_url(&self) -> String {
        format!("{}/v1", self.creds.base_url)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DbValue {
    Text(String),
    /// Plaintext that gets encrypted with the app's safeStorage key at apply time.
    Secret(String),
    Delete,
}

#[derive(Debug, Clone)]
pub enum Change {
    File {
        path: PathBuf,
        before: Option<String>,
        after: String,
        /// Holds credentials: written with mode 0600, and previews show `summary` instead of contents.
        private: bool,
        summary: Vec<String>,
    },
    DbItem {
        app: ElectronApp,
        db: PathBuf,
        key: String,
        label: String,
        summary: Vec<String>,
        value: DbValue,
    },
    /// Runs a CLI that owns its own store, e.g. `opencode auth`.
    Command {
        label: String,
        summary: Vec<String>,
        program: PathBuf,
        steps: Vec<CommandStep>,
        backup: Option<CommandBackup>,
    },
}

impl Change {
    /// `None` when the text stays the same.
    pub fn file(path: PathBuf, before: Option<String>, after: String) -> Option<Self> {
        (before.as_deref() != Some(after.as_str())).then_some(Self::File {
            path,
            before,
            after,
            private: false,
            summary: vec![],
        })
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::File { path, .. } => Some(path),
            Self::DbItem { db, .. } => Some(db),
            Self::Command { .. } => None,
        }
    }
}

#[derive(Debug)]
pub struct Plan {
    pub harness: HarnessId,
    pub changes: Vec<Change>,
    /// Record to save after applying; `None` removes it.
    pub record: Option<HarnessRecord>,
    pub notes: Vec<String>,
}

impl Plan {
    pub fn electron_app(&self) -> Option<ElectronApp> {
        self.changes.iter().find_map(|c| match c {
            Change::DbItem { app, .. } => Some(*app),
            Change::File { .. } | Change::Command { .. } => None,
        })
    }

    pub fn writes_secrets(&self) -> bool {
        self.changes.iter().any(|c| {
            matches!(
                c,
                Change::DbItem {
                    value: DbValue::Secret(_),
                    ..
                }
            )
        })
    }
}

pub trait Harness {
    fn id(&self) -> HarnessId;
    fn detect(&self) -> bool;
    fn is_installed(&self) -> bool;
    fn hint(&self) -> &'static str;
    fn plan_install(&self, ctx: &InstallCtx, record: Option<&HarnessRecord>) -> Result<Plan>;
    fn plan_uninstall(&self, record: &HarnessRecord) -> Result<Plan>;
    fn inherited_record(&self, _state: &InstallState) -> Option<HarnessRecord> {
        None
    }
}

/// Every location a harness reads or writes, so tests can point them at temp dirs.
#[derive(Debug, Clone)]
pub struct HarnessPaths {
    pub opencode_config_dir: PathBuf,
    pub opencode_data_dir: PathBuf,
    pub opencode_bin: PathBuf,
    pub kilo_config_dir: PathBuf,
    pub kilo_data_dir: PathBuf,
    pub vscode_user_dir: PathBuf,
    pub vscode_extensions_dir: PathBuf,
    pub cursor_user_dir: PathBuf,
}

impl Default for HarnessPaths {
    fn default() -> Self {
        Self {
            opencode_config_dir: xdg_config_home().join("opencode"),
            opencode_data_dir: xdg_data_home().join("opencode"),
            opencode_bin: "opencode".into(),
            kilo_config_dir: xdg_config_home().join("kilo"),
            kilo_data_dir: xdg_data_home().join("kilo"),
            vscode_user_dir: ElectronApp::VSCode.user_dir(),
            vscode_extensions_dir: home_dir().join(".vscode/extensions"),
            cursor_user_dir: ElectronApp::Cursor.user_dir(),
        }
    }
}

pub fn all(paths: &HarnessPaths) -> Vec<Box<dyn Harness>> {
    let cli = Rc::new(OpencodeCli::new(&paths.opencode_bin));
    HarnessId::value_variants()
        .iter()
        .map(|&id| build(paths, id, &cli))
        .collect()
}

pub fn get(paths: &HarnessPaths, id: HarnessId) -> Box<dyn Harness> {
    build(paths, id, &Rc::new(OpencodeCli::new(&paths.opencode_bin)))
}

fn build(paths: &HarnessPaths, id: HarnessId, cli: &Rc<OpencodeCli>) -> Box<dyn Harness> {
    let opencode = |app| Box::new(OpenCode::new(app, paths, cli.clone()));
    match id {
        HarnessId::Vscode => Box::new(vscode::VsCode::new(paths)),
        HarnessId::Opencode => opencode(App::OpenCode1),
        HarnessId::Opencode2 => opencode(App::OpenCode2),
        HarnessId::Kilo => opencode(App::Kilo),
        HarnessId::Cursor => Box::new(cursor::Cursor::new(paths)),
    }
}

pub fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// Ids liteton created earlier but that are no longer selected.
pub fn deselected<'a>(record: Option<&'a HarnessRecord>, models: &[ModelSpec]) -> Vec<&'a String> {
    record
        .map(|r| deselected_ids(&r.added_models, models))
        .unwrap_or_default()
}

pub fn deselected_ids<'a>(added: &'a [String], models: &[ModelSpec]) -> Vec<&'a String> {
    added
        .iter()
        .filter(|id| !models.iter().any(|m| &m.id == *id))
        .collect()
}

/// Combines the previous record with this run: keeps earlier additions, adds new ones, drops deselected.
pub fn next_record(
    record: Option<&HarnessRecord>,
    newly_added: Vec<String>,
    removed: &[&String],
    created_provider: bool,
    created_secret: Option<String>,
) -> HarnessRecord {
    let mut next = record.cloned().unwrap_or_default();
    next.added_models.retain(|id| !removed.contains(&id));
    for id in newly_added {
        if !next.added_models.contains(&id) {
            next.added_models.push(id);
        }
    }
    next.created_provider |= created_provider;
    if next.created_secret.is_none() {
        next.created_secret = created_secret;
    }
    next
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn paths_in(dir: &Path) -> HarnessPaths {
        HarnessPaths {
            opencode_config_dir: dir.join("config/opencode"),
            opencode_data_dir: dir.join("data/opencode"),
            opencode_bin: dir.join("bin/opencode"),
            kilo_config_dir: dir.join("config/kilo"),
            kilo_data_dir: dir.join("data/kilo"),
            vscode_user_dir: dir.join("Code/User"),
            vscode_extensions_dir: dir.join("vscode-extensions"),
            cursor_user_dir: dir.join("Cursor/User"),
        }
    }

    pub fn spec(id: &str, reasoning: bool) -> ModelSpec {
        ModelSpec {
            context_window: Some(400000),
            max_output_tokens: Some(128000),
            pricing: crate::litellm::Pricing {
                input: Some(0.05),
                output: Some(0.4),
                ..Default::default()
            },
            vision: true,
            reasoning,
            ..ModelSpec::bare(id)
        }
    }

    /// A `state.vscdb` with the same schema VSCode and Cursor use.
    pub fn create_state_db(path: &Path, rows: &[(&str, &str)]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
        )
        .unwrap();
        for (key, value) in rows {
            conn.execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, value],
            )
            .unwrap();
        }
    }
}
