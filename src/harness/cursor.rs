//! Cursor is genuinely ass, kinda experimental

use std::net::{IpAddr, ToSocketAddrs};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::{
    Change, DbValue, Harness, HarnessId, HarnessPaths, InstallCtx, Plan, binary_on_path,
    deselected, next_record,
};
use crate::config::{HarnessRecord, key_fingerprint};
use crate::vscdb::{ElectronApp, StateDb, secret_key};

const APPLICATION_USER_KEY: &str = "src.vs.platform.reactivestorage.browser.reactiveStorageServiceImpl.persistentStorage.applicationUser";
const API_KEY_SECRET: &str = "cursorAuth/openAIKey";
const BASE_URL: &str = "openAIBaseUrl";
const USE_KEY: &str = "useOpenAIKey";

pub const WARNINGS: &[&str] = &[
    "Cursor sends custom-key requests from its own servers, not from your machine. Your LiteLLM URL must be reachable from the public internet; endpoints limited to company IPs or a VPN will not work.",
    "The OpenAI base URL override is global: while it is on, Cursor's built-in OpenAI models are also sent to your LiteLLM proxy. Turn off \"OpenAI API Key\" in Cursor Settings > Models to use them normally again.",
];

pub struct Cursor {
    user_dir: PathBuf,
}

impl Cursor {
    pub fn new(paths: &HarnessPaths) -> Self {
        Self {
            user_dir: paths.cursor_user_dir.clone(),
        }
    }

    fn state_db(&self) -> PathBuf {
        self.user_dir.join("globalStorage/state.vscdb")
    }

    fn read_settings(&self) -> Result<(StateDb, Value)> {
        let path = self.state_db();
        if !path.exists() {
            bail!(
                "{} not found; open Cursor once before installing",
                path.display()
            );
        }
        let db = StateDb::open_readonly(&path)?;
        let text = db.get(APPLICATION_USER_KEY)?.ok_or_else(|| {
            anyhow!("Cursor has no saved settings yet; open Cursor once and sign in")
        })?;
        let value: Value =
            serde_json::from_str(&text).context("parsing Cursor's applicationUser settings")?;
        if !value.is_object() {
            bail!("Cursor's applicationUser settings are not a JSON object");
        }
        Ok((db, value))
    }

    fn settings_change(&self, label: &str, summary: Vec<String>, value: &Value) -> Change {
        Change::DbItem {
            app: ElectronApp::Cursor,
            db: self.state_db(),
            key: APPLICATION_USER_KEY.into(),
            label: label.into(),
            summary,
            value: DbValue::Text(value.to_string()),
        }
    }
}

impl Harness for Cursor {
    fn id(&self) -> HarnessId {
        HarnessId::Cursor
    }

    fn detect(&self) -> bool {
        self.user_dir.exists() || binary_on_path("cursor")
    }

    fn is_installed(&self) -> bool {
        self.read_settings()
            .is_ok_and(|(_, v)| v[USE_KEY] == json!(true) && v[BASE_URL].is_string())
    }

    fn plan_install(&self, ctx: &InstallCtx, record: Option<&HarnessRecord>) -> Result<Plan> {
        ensure_public_url(&ctx.creds.base_url)?;
        let (db, mut settings) = self.read_settings()?;
        let mut summary = Vec::new();

        let mut previous = record.map(|r| r.previous.clone()).unwrap_or_default();
        for key in [BASE_URL, USE_KEY] {
            previous
                .entry(key.to_string())
                .or_insert_with(|| settings.get(key).cloned().unwrap_or(Value::Null));
        }

        let base_url = ctx.openai_base_url();
        set_top(&mut settings, BASE_URL, json!(base_url), &mut summary);
        set_top(&mut settings, USE_KEY, json!(true), &mut summary);

        let enabled = string_list(&settings["aiSettings"]["modelOverrideEnabled"]);
        let mut newly_added = Vec::new();
        let removed = deselected(record, ctx.models);
        let mut next_enabled: Vec<String> = enabled
            .iter()
            .filter(|id| !removed.contains(id))
            .cloned()
            .collect();
        for model in ctx.models {
            if !next_enabled.contains(&model.id) {
                next_enabled.push(model.id.clone());
                newly_added.push(model.id.clone());
            }
        }
        let disabled = string_list(&settings["aiSettings"]["modelOverrideDisabled"]);
        let next_disabled: Vec<String> = disabled
            .iter()
            .filter(|id| !ctx.models.iter().any(|m| &&m.id == id))
            .cloned()
            .collect();
        for id in &newly_added {
            summary.push(format!("enable model {id}"));
        }
        for id in &removed {
            summary.push(format!("remove model {id}"));
        }
        set_ai_list(
            &mut settings,
            "modelOverrideEnabled",
            &enabled,
            next_enabled,
        )?;
        set_ai_list(
            &mut settings,
            "modelOverrideDisabled",
            &disabled,
            next_disabled,
        )?;

        let mut changes = Vec::new();
        if !summary.is_empty() {
            changes.push(self.settings_change("Cursor settings (state.vscdb)", summary, &settings));
        }
        let secret_existed = db.get(&secret_key(API_KEY_SECRET))?.is_some();
        let fingerprint = key_fingerprint(&ctx.creds.api_key);
        if !secret_existed || record.and_then(|r| r.key_fingerprint.as_ref()) != Some(&fingerprint)
        {
            changes.push(Change::DbItem {
                app: ElectronApp::Cursor,
                db: self.state_db(),
                key: secret_key(API_KEY_SECRET),
                label: "Cursor secret storage: OpenAI API key".into(),
                summary: vec![format!(
                    "{} the API key (encrypted with \"Cursor Safe Storage\")",
                    if secret_existed { "replace" } else { "store" }
                )],
                value: DbValue::Secret(ctx.creds.api_key.clone()),
            });
        }

        let mut next = next_record(
            record,
            newly_added,
            &removed,
            false,
            (!secret_existed).then(|| API_KEY_SECRET.to_string()),
        );
        next.previous = previous;
        next.key_fingerprint = Some(fingerprint);
        Ok(Plan {
            harness: self.id(),
            changes,
            record: Some(next),
            notes: vec!["Cursor may not accept model ids containing \"/\"; if a model is rejected, add an alias for it in LiteLLM.".into()],
        })
    }

    fn plan_uninstall(&self, record: &HarnessRecord) -> Result<Plan> {
        let (_, mut settings) = self.read_settings()?;
        let mut summary = Vec::new();
        for key in [BASE_URL, USE_KEY] {
            match record.previous.get(key) {
                Some(Value::Null) | None if key == USE_KEY => {
                    set_top(&mut settings, key, json!(false), &mut summary)
                }
                Some(Value::Null) | None => {
                    if settings.as_object_mut().unwrap().remove(key).is_some() {
                        summary.push(format!("remove {key}"));
                    }
                }
                Some(previous) => set_top(&mut settings, key, previous.clone(), &mut summary),
            }
        }
        let enabled = string_list(&settings["aiSettings"]["modelOverrideEnabled"]);
        let next: Vec<String> = enabled
            .iter()
            .filter(|id| !record.added_models.contains(id))
            .cloned()
            .collect();
        for id in enabled.iter().filter(|id| record.added_models.contains(id)) {
            summary.push(format!("remove model {id}"));
        }
        set_ai_list(&mut settings, "modelOverrideEnabled", &enabled, next)?;

        let mut changes = Vec::new();
        if !summary.is_empty() {
            changes.push(self.settings_change("Cursor settings (state.vscdb)", summary, &settings));
        }
        if record.created_secret.is_some() {
            changes.push(Change::DbItem {
                app: ElectronApp::Cursor,
                db: self.state_db(),
                key: secret_key(API_KEY_SECRET),
                label: "Cursor secret storage: OpenAI API key".into(),
                summary: vec!["delete the API key".into()],
                value: DbValue::Delete,
            });
        }
        Ok(Plan {
            harness: self.id(),
            changes,
            record: None,
            notes: vec![],
        })
    }
}

fn set_top(settings: &mut Value, key: &str, value: Value, summary: &mut Vec<String>) {
    let current = settings.get(key).cloned().unwrap_or(Value::Null);
    if current != value {
        summary.push(format!("{key}: {current} → {value}"));
        settings[key] = value;
    }
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn set_ai_list(
    settings: &mut Value,
    key: &str,
    current: &[String],
    next: Vec<String>,
) -> Result<()> {
    if current == next.as_slice() {
        return Ok(());
    }
    let ai = settings
        .as_object_mut()
        .unwrap()
        .entry("aiSettings")
        .or_insert_with(|| json!({}));
    let ai = ai
        .as_object_mut()
        .ok_or_else(|| anyhow!("Cursor's aiSettings is not an object"))?;
    ai.insert(key.into(), json!(next));
    Ok(())
}

/// Cursor's servers reject loopback and fkn private addresses
pub fn ensure_public_url(base_url: &str) -> Result<()> {
    let url =
        reqwest::Url::parse(base_url).with_context(|| format!("invalid base URL {base_url}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("base URL {base_url} has no host"))?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        bail!(
            "Cursor cannot reach {host}: its servers make the requests, so the URL must be public"
        );
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<IpAddr> = (host, port)
        .to_socket_addrs()
        .with_context(|| {
            format!("could not resolve {host}; Cursor's servers will not be able to either")
        })?
        .map(|a| a.ip())
        .collect();
    if let Some(ip) = addrs.iter().find(|ip| !is_public(ip)) {
        bail!("{host} resolves to the non-public address {ip}; Cursor's servers cannot reach it");
    }
    Ok(())
}

fn is_public(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Credentials};
    use crate::harness::tests::{create_state_db, paths_in, spec};

    #[test]
    fn rejects_private_urls() {
        assert!(ensure_public_url("http://localhost:4000").is_err());
        assert!(ensure_public_url("http://127.0.0.1:4000").is_err());
        assert!(ensure_public_url("https://10.1.2.3").is_err());
        assert!(ensure_public_url("https://192.168.1.10/v1").is_err());
        assert!(ensure_public_url("https://100.100.1.1").is_err());
        assert!(ensure_public_url("https://8.8.8.8").is_ok());
    }

    #[test]
    fn install_and_uninstall_restore_settings() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let settings = json!({
            "openAIBaseUrl": "https://old.example.com",
            "useOpenAIKey": false,
            "aiSettings": {"modelOverrideEnabled": ["gpt-5.5"], "modelOverrideDisabled": ["azure/gpt-5.4-nano", "grok"], "other": 1},
            "unrelated": {"a": [1, 2]}
        });
        let db_path = paths.cursor_user_dir.join("globalStorage/state.vscdb");
        create_state_db(&db_path, &[(APPLICATION_USER_KEY, &settings.to_string())]);

        let harness = Cursor::new(&paths);
        let config = Config::default();
        let models = vec![spec("azure/gpt-5.4-nano", true)];
        let creds = Credentials {
            base_url: "https://8.8.8.8".into(),
            api_key: "sk-1".into(),
        };
        let plan = harness
            .plan_install(
                &InstallCtx {
                    creds: &creds,
                    models: &models,
                    config: &config,
                },
                None,
            )
            .unwrap();
        let record = plan.record.clone().unwrap();
        assert_eq!(record.previous[BASE_URL], json!("https://old.example.com"));
        assert_eq!(record.created_secret.as_deref(), Some(API_KEY_SECRET));

        let file_only = Plan {
            changes: plan
                .changes
                .into_iter()
                .filter(|c| {
                    matches!(
                        c,
                        Change::DbItem {
                            value: DbValue::Text(_),
                            ..
                        }
                    )
                })
                .collect(),
            ..plan
        };
        crate::harness::apply::apply(&file_only, &dir.path().join("backup"), None).unwrap();
        assert!(dir.path().join("backup/cursor/state.vscdb").exists());

        let (_, after) = harness.read_settings().unwrap();
        assert_eq!(after[BASE_URL], json!("https://8.8.8.8/v1"));
        assert_eq!(after[USE_KEY], json!(true));
        assert_eq!(
            after["aiSettings"]["modelOverrideEnabled"],
            json!(["gpt-5.5", "azure/gpt-5.4-nano"])
        );
        assert_eq!(
            after["aiSettings"]["modelOverrideDisabled"],
            json!(["grok"])
        );
        assert_eq!(after["aiSettings"]["other"], json!(1));
        assert_eq!(after["unrelated"], json!({"a": [1, 2]}));
        assert!(harness.is_installed());

        let uninstall = harness.plan_uninstall(&record).unwrap();
        crate::harness::apply::apply(&uninstall, &dir.path().join("backup2"), None).unwrap();
        let (_, restored) = harness.read_settings().unwrap();
        assert_eq!(restored[BASE_URL], json!("https://old.example.com"));
        assert_eq!(restored[USE_KEY], json!(false));
        assert_eq!(
            restored["aiSettings"]["modelOverrideEnabled"],
            json!(["gpt-5.5"])
        );
    }
}
