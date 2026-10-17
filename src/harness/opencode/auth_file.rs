use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use super::KeyChange;
use crate::harness::{Change, PROVIDER_ID, read_optional};

pub struct AuthFile {
    path: PathBuf,
}

impl AuthFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `None` removes liteton's entry; other entries are kept.
    pub fn plan(&self, api_key: Option<&str>) -> Result<KeyChange> {
        let before = read_optional(&self.path)?;
        let mut auth: Map<String, Value> = match &before {
            Some(text) if !text.trim().is_empty() => serde_json::from_str(text)
                .with_context(|| format!("parsing {}", self.path.display()))?,
            _ => Map::new(),
        };
        let existed = auth.contains_key(PROVIDER_ID);
        match api_key {
            Some(key) => {
                auth.insert(PROVIDER_ID.to_string(), json!({"type": "api", "key": key}));
            }
            None => {
                auth.remove(PROVIDER_ID);
            }
        }
        let after = serde_json::to_string_pretty(&auth)? + "\n";
        let unchanged = before
            .as_deref()
            .and_then(|b| serde_json::from_str::<Map<String, Value>>(b).ok())
            .as_ref()
            == Some(&auth);
        let action = match (api_key, existed) {
            (Some(_), false) => "add",
            (Some(_), true) => "update",
            (None, _) => "remove",
        };
        let change = (!unchanged).then(|| Change::File {
            path: self.path.clone(),
            before,
            after,
            private: true,
            summary: vec![format!(
                "{action} the \"{PROVIDER_ID}\" API key entry (other entries are kept)"
            )],
        });
        Ok(KeyChange {
            change,
            created: (!existed).then(|| PROVIDER_ID.to_string()),
        })
    }
}
