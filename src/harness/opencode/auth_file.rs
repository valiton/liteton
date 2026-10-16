use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use super::{CredentialPlan, CredentialStore};
use crate::config::HarnessRecord;
use crate::harness::{Change, PROVIDER_ID, read_optional};

/// opencode 1 reads API keys from auth.json in its data folder.
pub struct AuthFile {
    path: PathBuf,
}

impl AuthFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Sets (or with `None`, removes) the provider's entry, keeping all other entries.
    fn plan(&self, api_key: Option<&str>) -> Result<CredentialPlan> {
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
        Ok(CredentialPlan {
            change,
            created: (!existed).then(|| PROVIDER_ID.to_string()),
            ..CredentialPlan::default()
        })
    }
}

impl CredentialStore for AuthFile {
    fn plan_set(&self, api_key: &str, _record: Option<&HarnessRecord>) -> Result<CredentialPlan> {
        self.plan(Some(api_key))
    }

    fn plan_remove(&self) -> Result<CredentialPlan> {
        self.plan(None)
    }
}
