use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use jsonc_parser::cst::{CstArray, CstObject};
use serde_json::{Value, json};

use super::{
    Change, DbValue, Harness, HarnessId, HarnessPaths, InstallCtx, PROVIDER_ID, Plan,
    binary_on_path, deselected, next_record, read_optional,
};
use crate::config::{Config, HarnessRecord, key_fingerprint};
use crate::jsonc;
use crate::litellm::ModelSpec;
use crate::vscdb::{ElectronApp, StateDb, secret_key};

const VENDOR: &str = "customendpoint";
const DEFAULT_SECRET: &str = "chat.lm.secret.liteton";

pub struct VsCode {
    user_dir: PathBuf,
}

impl VsCode {
    pub fn new(paths: &HarnessPaths) -> Self {
        Self {
            user_dir: paths.vscode_user_dir.clone(),
        }
    }

    fn models_path(&self) -> PathBuf {
        self.user_dir.join("chatLanguageModels.json")
    }

    fn state_db(&self) -> PathBuf {
        self.user_dir.join("globalStorage/state.vscdb")
    }
}

impl Harness for VsCode {
    fn id(&self) -> HarnessId {
        HarnessId::Vscode
    }

    fn detect(&self) -> bool {
        self.user_dir.exists() || binary_on_path("code")
    }

    fn is_installed(&self) -> bool {
        read_optional(&self.models_path())
            .ok()
            .flatten()
            .and_then(|text| jsonc::read_value(&text).ok())
            .and_then(|v| v.as_array().cloned())
            .is_some_and(|entries| entries.iter().any(|e| e["name"] == PROVIDER_ID))
    }

    fn plan_install(&self, ctx: &InstallCtx, record: Option<&HarnessRecord>) -> Result<Plan> {
        let db_path = self.state_db();
        if !db_path.exists() {
            bail!(
                "{} not found; open VSCode once before installing",
                db_path.display()
            );
        }
        let path = self.models_path();
        let before = read_optional(&path)?;
        let root = jsonc::parse(before.as_deref().unwrap_or(""))?;
        let entries = root.array_value_or_set();

        let (entry, created_provider) = match find_entry(&entries) {
            Some(entry) => (entry, false),
            None => {
                let node = entries.append(jsonc::to_input(&json!({
                    "name": PROVIDER_ID,
                    "vendor": VENDOR,
                    "apiKey": format!("${{input:{DEFAULT_SECRET}}}"),
                    "apiType": "chat-completions",
                    "models": [],
                })));
                (
                    node.as_object()
                        .ok_or_else(|| anyhow!("could not create the {PROVIDER_ID} entry"))?,
                    true,
                )
            }
        };
        let secret = match jsonc::string_prop(&entry, "apiKey")
            .as_deref()
            .and_then(secret_ref)
        {
            Some(existing) => existing.to_string(),
            None => {
                jsonc::merge_object(
                    &entry,
                    json!({"apiKey": format!("${{input:{DEFAULT_SECRET}}}")})
                        .as_object()
                        .unwrap(),
                );
                DEFAULT_SECRET.to_string()
            }
        };
        jsonc::set_if_missing(&entry, "apiType", &json!("chat-completions"));

        let models = entry
            .array_value_or_create("models")
            .ok_or_else(|| anyhow!("\"models\" of the {PROVIDER_ID} entry is not an array"))?;
        let mut newly_added = Vec::new();
        for model in ctx.models {
            let value = model_entry(model, &ctx.creds.base_url, ctx.config);
            match find_model(&models, &model.id) {
                Some(existing) => {
                    let mut value = value;
                    if existing.get("name").is_some() {
                        value.as_object_mut().unwrap().remove("name");
                    }
                    jsonc::merge_object(&existing, value.as_object().unwrap());
                }
                None => {
                    models.append(jsonc::to_input(&value));
                    newly_added.push(model.id.clone());
                }
            }
        }
        let removed = deselected(record, ctx.models);
        for id in &removed {
            if let Some(existing) = find_model(&models, id) {
                existing.remove();
            }
        }

        let secret_existed = StateDb::open_readonly(&db_path)?
            .get(&secret_key(&secret))?
            .is_some();
        let mut changes = Vec::new();
        let after = jsonc::finish(&root, before.as_deref());
        if before.as_deref() != Some(after.as_str()) {
            changes.push(Change::File {
                path,
                before,
                after,
                private: false,
                summary: vec![],
            });
        }
        let fingerprint = key_fingerprint(&ctx.creds.api_key);
        if !secret_existed || record.and_then(|r| r.key_fingerprint.as_ref()) != Some(&fingerprint)
        {
            changes.push(Change::DbItem {
                app: ElectronApp::VSCode,
                db: db_path,
                key: secret_key(&secret),
                label: format!("VSCode secret storage: {secret}"),
                summary: vec![format!(
                    "{} the API key (encrypted with \"Code Safe Storage\")",
                    if secret_existed { "replace" } else { "store" }
                )],
                value: DbValue::Secret(ctx.creds.api_key.clone()),
            });
        }

        let mut next = next_record(
            record,
            newly_added,
            &removed,
            created_provider,
            (!secret_existed).then_some(secret),
        );
        next.key_fingerprint = Some(fingerprint);
        Ok(Plan {
            harness: self.id(),
            changes,
            record: Some(next),
            notes: vec![],
        })
    }

    fn plan_uninstall(&self, record: &HarnessRecord) -> Result<Plan> {
        let mut changes = Vec::new();
        let path = self.models_path();
        if let Some(before) = read_optional(&path)? {
            let root = jsonc::parse(&before)?;
            if let Some(entries) = root.array_value()
                && let Some(entry) = find_entry(&entries)
            {
                if let Some(models) = entry.array_value("models") {
                    for id in &record.added_models {
                        if let Some(model) = find_model(&models, id) {
                            model.remove();
                        }
                    }
                }
                let empty = entry
                    .array_value("models")
                    .is_none_or(|m| m.elements().is_empty());
                if record.created_provider && empty {
                    entry.remove();
                }
            }
            let after = jsonc::finish(&root, Some(&before));
            if after != before {
                changes.push(Change::File {
                    path,
                    before: Some(before),
                    after,
                    private: false,
                    summary: vec![],
                });
            }
        }
        if let Some(secret) = &record.created_secret {
            changes.push(Change::DbItem {
                app: ElectronApp::VSCode,
                db: self.state_db(),
                key: secret_key(secret),
                label: format!("VSCode secret storage: {secret}"),
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

fn find_entry(entries: &CstArray) -> Option<CstObject> {
    entries
        .elements()
        .into_iter()
        .filter_map(|n| n.as_object())
        .find(|o| jsonc::string_prop(o, "name").as_deref() == Some(PROVIDER_ID))
}

fn find_model(models: &CstArray, id: &str) -> Option<CstObject> {
    models
        .elements()
        .into_iter()
        .filter_map(|n| n.as_object())
        .find(|o| jsonc::string_prop(o, "id").as_deref() == Some(id))
}

fn secret_ref(api_key: &str) -> Option<&str> {
    api_key.strip_prefix("${input:")?.strip_suffix('}')
}

fn model_entry(model: &ModelSpec, base_url: &str, config: &Config) -> Value {
    let mut entry = json!({
        "id": model.id,
        "name": model.display_name,
        "url": base_url,
        "toolCalling": model.tool_calling,
        "vision": model.vision,
        "thinking": model.reasoning,
    });
    let obj = entry.as_object_mut().unwrap();
    if model.reasoning {
        obj.insert(
            "supportsReasoningEffort".into(),
            json!(config.reasoning_efforts_for(&model.id)),
        );
    }
    if let Some(context) = model.context_window {
        obj.insert("contextWindow".into(), json!(context));
    }
    if let Some(output) = model.max_output_tokens {
        obj.insert("maxOutputTokens".into(), json!(output));
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Credentials;
    use crate::harness::tests::{create_state_db, paths_in, spec};

    const USER_MODELS: &str = r#"[
	{
		"name": "self-hosted",
		"vendor": "customendpoint",
		"apiType": "chat-completions",
		"models": [{"id": "Qwen/Qwen3.8-27B", "name": "Qwen3.8 27B", "url": "http://localhost:8000/v1/chat/completions"}]
	},
	{
		"name": "litellm",
		"vendor": "customendpoint",
		"apiKey": "${input:chat.lm.secret.1c807067}",
		"apiType": "chat-completions",
		"models": [{"id": "azure/gpt-5-mini", "name": "gpt-5-mini", "url": "https://old", "custom": 1}],
		"settings": {"azure/gpt-5.4-nano": {"reasoningEffort": "none"}}
	}
]
"#;

    #[test]
    fn merges_into_existing_entry_and_reuses_secret() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        std::fs::create_dir_all(&paths.vscode_user_dir).unwrap();
        std::fs::write(
            paths.vscode_user_dir.join("chatLanguageModels.json"),
            USER_MODELS,
        )
        .unwrap();
        create_state_db(
            &paths.vscode_user_dir.join("globalStorage/state.vscdb"),
            &[("secret://chat.lm.secret.1c807067", "x")],
        );

        let creds = Credentials {
            base_url: "https://llm.example.com".into(),
            api_key: "sk-1".into(),
        };
        let config = Config::default();
        let models = vec![
            spec("azure/gpt-5-mini", true),
            spec("azure/gpt-5.4-nano", false),
        ];
        let harness = VsCode::new(&paths);
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

        let Change::File { after, .. } = &plan.changes[0] else {
            panic!("expected file change")
        };
        assert!(
            after.contains("\t\t\"name\": \"self-hosted\""),
            "tabs are kept:\n{after}"
        );
        let value = jsonc::read_value(after).unwrap();
        assert_eq!(
            value[0]["models"][0]["url"],
            json!("http://localhost:8000/v1/chat/completions")
        );
        let entry = &value[1];
        assert_eq!(
            entry["settings"],
            json!({"azure/gpt-5.4-nano": {"reasoningEffort": "none"}})
        );
        assert_eq!(entry["models"][0]["custom"], json!(1));
        assert_eq!(entry["models"][0]["url"], json!("https://llm.example.com"));
        assert_eq!(
            entry["models"][0]["supportsReasoningEffort"],
            json!(["none", "low", "medium", "high", "xhigh"])
        );
        assert_eq!(entry["models"][1]["id"], json!("azure/gpt-5.4-nano"));
        assert_eq!(entry["models"][1]["contextWindow"], json!(400000));
        assert!(entry["models"][1].get("supportsReasoningEffort").is_none());

        let Change::DbItem { key, value, .. } = &plan.changes[1] else {
            panic!("expected secret change")
        };
        assert_eq!(key, "secret://chat.lm.secret.1c807067");
        assert_eq!(value, &DbValue::Secret("sk-1".into()));

        let record = plan.record.unwrap();
        assert_eq!(record.added_models, vec!["azure/gpt-5.4-nano".to_string()]);
        assert!(!record.created_provider);
        assert_eq!(record.created_secret, None);

        // Same key on the next run: the secret is not rewritten, so VSCode needn't be closed.
        let again = harness
            .plan_install(
                &InstallCtx {
                    creds: &creds,
                    models: &models,
                    config: &config,
                },
                Some(&record),
            )
            .unwrap();
        assert!(
            again
                .changes
                .iter()
                .all(|c| matches!(c, Change::File { .. }))
        );
        let rotated = Credentials {
            api_key: "sk-2".into(),
            ..creds.clone()
        };
        let rotated = harness
            .plan_install(
                &InstallCtx {
                    creds: &rotated,
                    models: &models,
                    config: &config,
                },
                Some(&record),
            )
            .unwrap();
        assert!(rotated.writes_secrets());
    }

    #[test]
    fn creates_entry_and_uninstalls_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        create_state_db(
            &paths.vscode_user_dir.join("globalStorage/state.vscdb"),
            &[],
        );

        let creds = Credentials {
            base_url: "https://llm.example.com".into(),
            api_key: "sk-1".into(),
        };
        let config = Config::default();
        let models = vec![spec("azure/gpt-5.4-nano", true)];
        let harness = VsCode::new(&paths);
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
        assert!(record.created_provider);
        assert_eq!(record.created_secret.as_deref(), Some(DEFAULT_SECRET));

        // Apply only the file part; the secret needs the Keychain.
        let file_only = Plan {
            changes: plan
                .changes
                .into_iter()
                .filter(|c| matches!(c, Change::File { .. }))
                .collect(),
            ..plan
        };
        crate::harness::apply::apply(&file_only, &dir.path().join("backup"), None).unwrap();
        assert!(harness.is_installed());

        let uninstall = harness.plan_uninstall(&record).unwrap();
        assert!(matches!(
            uninstall.changes.last(),
            Some(Change::DbItem {
                value: DbValue::Delete,
                ..
            })
        ));
        crate::harness::apply::apply(&uninstall, &dir.path().join("backup2"), None).unwrap();
        assert!(!harness.is_installed());
        let db = StateDb::open_readonly(&paths.vscode_user_dir.join("globalStorage/state.vscdb"))
            .unwrap();
        assert!(db.get("secret://chat.lm.secret.liteton").unwrap().is_none());
    }
}
