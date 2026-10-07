//! `state.vscdb`, the SQLite key/value store VSCode-based apps keep in `User/globalStorage`.

pub mod safe_storage;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::config::home_dir;
use safe_storage::{SafeStorage, decode_buffer, encode_buffer};

pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self { conn })
    }

    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { conn })
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        let value: Option<rusqlite::types::Value> = self
            .conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match value {
            Some(rusqlite::types::Value::Text(text)) => Some(text),
            Some(rusqlite::types::Value::Blob(bytes)) => {
                Some(String::from_utf8(bytes).context("non-UTF-8 value")?)
            }
            Some(rusqlite::types::Value::Null) | None => None,
            Some(other) => bail!("unexpected value type for {key}: {other:?}"),
        })
    }

    /// Keys starting with `prefix`; used to find an existing secret for the round-trip check.
    pub fn keys_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key FROM ItemTable WHERE substr(key, 1, length(?1)) = ?1")?;
        let rows = stmt.query_map(params![prefix], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Writes all changes in one transaction. `None` deletes the key.
    pub fn apply(&mut self, items: &[(String, Option<String>)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (key, value) in items {
            match value {
                Some(value) => tx.execute(
                    "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
                    params![key, value],
                )?,
                None => tx.execute("DELETE FROM ItemTable WHERE key = ?1", params![key])?,
            };
        }
        tx.commit()?;
        Ok(())
    }

    /// Consistent copy even while the database is in WAL mode.
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        self.conn
            .execute("VACUUM INTO ?1", params![dest.to_string_lossy()])
            .with_context(|| format!("backing up to {}", dest.display()))?;
        Ok(())
    }
}

/// A VSCode-based desktop app whose `state.vscdb` liteton edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElectronApp {
    VSCode,
    Cursor,
}

impl ElectronApp {
    pub fn name(self) -> &'static str {
        match self {
            Self::VSCode => "Visual Studio Code",
            Self::Cursor => "Cursor",
        }
    }

    pub fn user_dir(self) -> PathBuf {
        let folder = match self {
            Self::VSCode => "Code",
            Self::Cursor => "Cursor",
        };
        home_dir()
            .join("Library/Application Support")
            .join(folder)
            .join("User")
    }

    /// Keychain (service, account) holding the app's safeStorage password.
    pub fn keychain_item(self) -> (&'static str, &'static str) {
        match self {
            Self::VSCode => ("Code Safe Storage", "Code"),
            Self::Cursor => ("Cursor Safe Storage", "Cursor"),
        }
    }

    fn bundle_ids(self) -> &'static [&'static str] {
        match self {
            Self::VSCode => &["com.microsoft.VSCode", "com.microsoft.VSCodeInsiders"],
            Self::Cursor => &["com.todesktop.230313mzl4w4u92"],
        }
    }

    fn process_pattern(self) -> &'static str {
        match self {
            Self::VSCode => "/Visual Studio Code.app/Contents/MacOS/",
            Self::Cursor => "/Cursor.app/Contents/MacOS/",
        }
    }

    pub fn is_running(self) -> bool {
        Command::new("pgrep")
            .args(["-f", self.process_pattern()])
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    /// Quitting the app would also kill the terminal liteton runs in.
    pub fn is_parent_terminal(self) -> bool {
        std::env::var("__CFBundleIdentifier")
            .is_ok_and(|id| self.bundle_ids().contains(&id.as_str()))
    }

    pub fn quit(self) -> Result<()> {
        let status = Command::new("osascript")
            .args(["-e", &format!("quit app \"{}\"", self.name())])
            .status()?;
        if !status.success() {
            bail!("osascript could not quit {}", self.name());
        }
        for _ in 0..60 {
            if !self.is_running() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        bail!("{} is still running", self.name())
    }

    /// Unlocks safeStorage and proves the derived key decrypts a secret the app wrote itself.
    pub fn unlock_secrets(self, db: &StateDb) -> Result<SecretWriter> {
        let (service, account) = self.keychain_item();
        let storage = SafeStorage::from_keychain(service, account)?;
        let existing = db.keys_with_prefix("secret://")?;
        let mut verified = false;
        for key in existing {
            let Some(value) = db.get(&key)? else { continue };
            let Ok(bytes) = decode_buffer(&value) else {
                continue;
            };
            storage.decrypt(&bytes).with_context(|| {
                format!(
                    "round-trip check failed on {key}; not writing any secrets to {}",
                    self.name()
                )
            })?;
            verified = true;
            break;
        }
        Ok(SecretWriter { storage, verified })
    }
}

pub struct SecretWriter {
    storage: SafeStorage,
    /// False when the app had no secrets yet, so the key derivation could not be checked.
    pub verified: bool,
}

impl SecretWriter {
    pub fn encode(&self, plaintext: &str) -> String {
        encode_buffer(&self.storage.encrypt(plaintext))
    }
}

pub fn secret_key(name: &str) -> String {
    format!("secret://{name}")
}
