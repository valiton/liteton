//! opencode 1 and 2 read the same config file, each from its own top-level key that the other
//! ignores. The install flow lives here; `v1` and `v2` say how their schemas differ.

mod auth_file;
mod cli;
mod v1;
mod v2;
mod variants;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Result, anyhow};
use jsonc_parser::cst::CstObject;
use serde_json::{Map, Value, json};

pub use cli::OpencodeCli;
pub use v1::{V1, approximated_tier};
pub use v2::V2;

use super::{
    Change, Harness, HarnessId, HarnessPaths, InstallCtx, PROVIDER_ID, Plan, deselected,
    next_record, read_optional,
};
use crate::config::{Config, HarnessRecord, InstallState};
use crate::jsonc;
use crate::litellm::ModelSpec;

/// Everything that differs between opencode versions.
pub trait Version: 'static {
    const HARNESS: HarnessId;
    /// The top-level key holding the providers.
    const ROOT_KEY: &'static str;
    const CLI_MAJOR: u32;
    const HINT: &'static str;
    type Store: CredentialStore;

    fn store(paths: &HarnessPaths, cli: Rc<OpencodeCli>) -> Self::Store;
    /// Points the provider at LiteLLM.
    fn set_connection(provider: &CstObject, base_url: &str);
    fn model_entry(model: &ModelSpec, config: &Config) -> Value;
    /// The variant liteton writes for one effort level.
    fn effort_variant(level: &str) -> Value;
    fn read_variants(variants: &Value) -> Vec<(String, Value)>;
    fn write_variants(variants: Vec<(String, Value)>) -> Value;

    /// Runs after a model's entry is written.
    fn tidy_model(_models: &CstObject, _model: &ModelSpec, _config: &Config) {}

    /// Whether this version looks present when the CLI's version is unknown.
    fn detect_without_cli(_config: Option<&Value>, _config_dir_exists: bool) -> bool {
        false
    }

    fn inherited_record(_config: Option<&Value>, _state: &InstallState) -> Option<HarnessRecord> {
        None
    }
}

/// Where a version keeps the API key.
pub trait CredentialStore {
    fn plan_set(&self, api_key: &str, record: Option<&HarnessRecord>) -> Result<CredentialPlan>;
    fn plan_remove(&self) -> Result<CredentialPlan>;
}

#[derive(Debug, Default)]
pub struct CredentialPlan {
    pub change: Option<Change>,
    /// Saved as the record's `created_secret`.
    pub created: Option<String>,
    /// Saved as the record's `key_fingerprint`.
    pub fingerprint: Option<String>,
    /// Shown instead of failing when the store can't be changed now.
    pub note: Option<String>,
}

pub struct OpenCode<V: Version> {
    config_dir: PathBuf,
    cli: Rc<OpencodeCli>,
    store: V::Store,
}

impl<V: Version> OpenCode<V> {
    pub fn new(paths: &HarnessPaths, cli: Rc<OpencodeCli>) -> Self {
        Self {
            config_dir: paths.opencode_config_dir.clone(),
            store: V::store(paths, cli.clone()),
            cli,
        }
    }

    /// Prefers `opencode.jsonc`, then `opencode.json`, and creates `opencode.jsonc` if neither exists.
    fn config_path(&self) -> PathBuf {
        let jsonc = self.config_dir.join("opencode.jsonc");
        let json = self.config_dir.join("opencode.json");
        if jsonc.exists() || !json.exists() {
            jsonc
        } else {
            json
        }
    }

    fn config_value(&self) -> Option<Value> {
        read_optional(&self.config_path())
            .ok()
            .flatten()
            .and_then(|text| jsonc::read_value(&text).ok())
    }
}

impl<V: Version> Harness for OpenCode<V> {
    fn id(&self) -> HarnessId {
        V::HARNESS
    }

    fn detect(&self) -> bool {
        let config = self.config_value();
        if config
            .as_ref()
            .is_some_and(|v| v.get(V::ROOT_KEY).is_some())
        {
            return true;
        }
        match self.cli.major() {
            Some(major) => major == V::CLI_MAJOR,
            None => V::detect_without_cli(config.as_ref(), self.config_dir.exists()),
        }
    }

    fn is_installed(&self) -> bool {
        self.config_value()
            .is_some_and(|v| v[V::ROOT_KEY][PROVIDER_ID].is_object())
    }

    fn hint(&self) -> &'static str {
        V::HINT
    }

    fn inherited_record(&self, state: &InstallState) -> Option<HarnessRecord> {
        V::inherited_record(self.config_value().as_ref(), state)
    }

    fn plan_install(&self, ctx: &InstallCtx, record: Option<&HarnessRecord>) -> Result<Plan> {
        let path = self.config_path();
        let before = read_optional(&path)?;
        let root = jsonc::parse(before.as_deref().unwrap_or(""))?;
        let obj = root.object_value_or_set();
        if before.is_none() {
            jsonc::set_if_missing(&obj, "$schema", &json!("https://opencode.ai/config.json"));
        }
        let providers = child_object(&obj, V::ROOT_KEY, V::ROOT_KEY, &path)?;
        let created_provider = providers.get(PROVIDER_ID).is_none();
        let provider_path = format!("{}.{PROVIDER_ID}", V::ROOT_KEY);
        let provider = child_object(&providers, PROVIDER_ID, &provider_path, &path)?;
        jsonc::set_if_missing(&provider, "name", &json!("LiteLLM"));
        V::set_connection(&provider, &ctx.openai_base_url());
        let models = child_object(
            &provider,
            "models",
            &format!("{provider_path}.models"),
            &path,
        )?;

        let mut newly_added = Vec::new();
        for model in ctx.models {
            if write_model::<V>(&models, model, ctx.config) {
                newly_added.push(model.id.clone());
            }
        }
        let removed = deselected(record, ctx.models);
        remove_models(&models, removed.iter().copied());

        let after = jsonc::finish(&root, before.as_deref());
        let mut changes: Vec<Change> = Change::file(path, before, after).into_iter().collect();
        let credential = self.store.plan_set(&ctx.creds.api_key, record)?;
        changes.extend(credential.change);

        let mut next = next_record(
            record,
            newly_added,
            &removed,
            created_provider,
            credential.created,
        );
        if credential.fingerprint.is_some() {
            next.key_fingerprint = credential.fingerprint;
        }
        Ok(Plan {
            harness: V::HARNESS,
            changes,
            record: Some(next),
            notes: vec![],
        })
    }

    fn plan_uninstall(&self, record: &HarnessRecord) -> Result<Plan> {
        let mut changes = Vec::new();
        let mut notes = Vec::new();
        let path = self.config_path();
        if let Some(before) = read_optional(&path)? {
            let root = jsonc::parse(&before)?;
            if let Some(providers) = root
                .object_value()
                .and_then(|o| o.object_value(V::ROOT_KEY))
                && let Some(provider) = providers.object_value(PROVIDER_ID)
            {
                if let Some(models) = provider.object_value("models") {
                    remove_models(&models, &record.added_models);
                }
                let empty = provider
                    .object_value("models")
                    .is_none_or(|m| m.properties().is_empty());
                if record.created_provider
                    && empty
                    && let Some(prop) = providers.get(PROVIDER_ID)
                {
                    prop.remove();
                }
            }
            let after = jsonc::finish(&root, Some(&before));
            changes.extend(Change::file(path, Some(before), after));
        }
        if record.created_secret.is_some() {
            let credential = self.store.plan_remove()?;
            changes.extend(credential.change);
            notes.extend(credential.note);
        }
        Ok(Plan {
            harness: V::HARNESS,
            changes,
            record: None,
            notes,
        })
    }
}

/// `parent[key]`, created if missing. `shown` names it in the error.
fn child_object(parent: &CstObject, key: &str, shown: &str, path: &Path) -> Result<CstObject> {
    parent
        .object_value_or_create(key)
        .ok_or_else(|| anyhow!("\"{shown}\" in {} is not an object", path.display()))
}

/// Merges liteton's entry for one model and syncs its variants. Returns whether the entry is new.
fn write_model<V: Version>(models: &CstObject, model: &ModelSpec, config: &Config) -> bool {
    let mut value = V::model_entry(model, config);
    let existing = models.object_value(&model.id);
    // A display name the user picked wins over ours.
    if existing.as_ref().is_some_and(|e| e.get("name").is_some()) {
        value.as_object_mut().unwrap().remove("name");
    }
    let mut entry = Map::new();
    entry.insert(model.id.clone(), value);
    jsonc::merge_object(models, &entry);
    let efforts = if model.reasoning {
        config.reasoning_efforts_for(model)
    } else {
        Vec::new()
    };
    variants::sync::<V>(models, &model.id, &efforts);
    V::tidy_model(models, model, config);
    existing.is_none()
}

fn remove_models<'a>(models: &CstObject, ids: impl IntoIterator<Item = &'a String>) {
    for id in ids {
        if let Some(prop) = models.get(id) {
            prop.remove();
        }
    }
}

/// `limit`, shared by both versions.
fn limit(model: &ModelSpec) -> Option<Value> {
    let (context, output) = (model.context_window?, model.max_output_tokens?);
    Some(json!({"context": context, "output": output}))
}

fn input_modalities(model: &ModelSpec) -> Value {
    if model.vision {
        json!(["text", "image"])
    } else {
        json!(["text"])
    }
}

#[cfg(test)]
mod tests {
    use super::cli::CREDENTIAL_ID;
    use super::*;
    use crate::config::Credentials;
    use crate::harness::tests::{paths_in, spec};

    pub(super) struct Fixture {
        pub dir: tempfile::TempDir,
        pub paths: HarnessPaths,
        pub creds: Credentials,
    }

    impl Fixture {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let paths = paths_in(dir.path());
            Self {
                dir,
                paths,
                creds: Credentials {
                    base_url: "https://llm.example.com".into(),
                    api_key: "sk-1".into(),
                },
            }
        }

        pub fn write_config(&self, name: &str, text: &str) -> PathBuf {
            std::fs::create_dir_all(&self.paths.opencode_config_dir).unwrap();
            let path = self.paths.opencode_config_dir.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }

        pub fn harness<V: Version>(&self) -> OpenCode<V> {
            OpenCode::new(
                &self.paths,
                Rc::new(OpencodeCli::new(&self.paths.opencode_bin)),
            )
        }

        pub fn install(
            &self,
            harness: &dyn Harness,
            models: &[ModelSpec],
            config: &Config,
            record: Option<&HarnessRecord>,
        ) -> Plan {
            let ctx = InstallCtx {
                creds: &self.creds,
                models,
                config,
            };
            harness.plan_install(&ctx, record).unwrap()
        }

        pub fn apply(&self, plan: &Plan) -> Result<()> {
            crate::harness::apply::apply(plan, &self.dir.path().join("backup"), None)
        }

        pub fn backup(&self, name: &str) -> PathBuf {
            self.dir.path().join("backup").join(name)
        }
    }

    pub(super) fn read(path: &Path) -> Value {
        jsonc::read_value(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    const USER_CONFIG: &str = r#"{
  "mcp": {
    "semble": {"command": ["uvx", "semble"], "type": "local", "enabled": true}
  },
  "$schema": "https://opencode.ai/config.json",
  // my models
  "model": "github-copilot/gpt-5.6-luna",
  "provider": {
    "sglang": {
      "npm": "@ai-sdk/openai-compatible",
      "options": {"baseURL": "http://127.0.0.1:8000/v1/"}
    }
  }
}
"#;

    #[test]
    fn merges_provider_without_touching_the_rest() {
        let fx = Fixture::new();
        let config_path = fx.write_config("opencode.jsonc", USER_CONFIG);
        let auth_path = fx.paths.opencode_data_dir.join("auth.json");
        std::fs::create_dir_all(&fx.paths.opencode_data_dir).unwrap();
        std::fs::write(
            &auth_path,
            r#"{"openrouter": {"type": "api", "key": "or"}}"#,
        )
        .unwrap();

        let config = Config::default();
        let models = vec![spec("azure/gpt-5.4-nano", true)];
        let harness = fx.harness::<V1>();
        let plan = fx.install(&harness, &models, &config, None);
        fx.apply(&plan).unwrap();

        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(text.contains("// my models"));
        let value = jsonc::read_value(&text).unwrap();
        assert_eq!(value["mcp"]["semble"]["enabled"], json!(true));
        assert_eq!(
            value["provider"]["sglang"]["options"]["baseURL"],
            json!("http://127.0.0.1:8000/v1/")
        );
        let provider = &value["provider"]["litellm"];
        assert_eq!(
            provider["options"]["baseURL"],
            json!("https://llm.example.com/v1")
        );
        let model = &provider["models"]["azure/gpt-5.4-nano"];
        assert_eq!(model["name"], json!("gpt-5.4-nano"));
        assert_eq!(model["limit"], json!({"context": 400000, "output": 128000}));
        assert_eq!(model["cost"], json!({"input": 0.05, "output": 0.4}));
        assert_eq!(
            model["variants"]["high"],
            json!({"reasoningEffort": "high"})
        );

        let auth = read(&auth_path);
        assert_eq!(auth["openrouter"]["key"], json!("or"));
        assert_eq!(auth["litellm"], json!({"type": "api", "key": "sk-1"}));
        assert!(fx.backup("opencode/opencode.jsonc").exists());

        let record = plan.record.unwrap();
        assert_eq!(record.added_models, vec!["azure/gpt-5.4-nano".to_string()]);
        assert!(record.created_provider);

        // Re-running with the same input changes nothing.
        let again = fx.install(&harness, &models, &config, Some(&record));
        assert!(again.changes.is_empty(), "{:?}", again.changes);

        fx.apply(&harness.plan_uninstall(&record).unwrap()).unwrap();
        let text = std::fs::read_to_string(&config_path).unwrap();
        let value = jsonc::read_value(&text).unwrap();
        assert!(value["provider"].get("litellm").is_none());
        assert!(value["provider"]["sglang"].is_object());
        assert!(text.contains("// my models"));
        let auth = read(&auth_path);
        assert!(auth.get("litellm").is_none());
        assert_eq!(auth["openrouter"]["key"], json!("or"));
    }

    #[test]
    fn keeps_hand_edits_inside_the_provider() {
        let fx = Fixture::new();
        let config_path = fx.write_config(
            "opencode.json",
            r#"{"provider": {"litellm": {"name": "Work", "models": {"mine": {"name": "Mine"}, "azure/gpt-5.4-nano": {"name": "Custom", "options": {"x": 1}}}}}}"#,
        );
        let config = Config::default();
        let models = vec![spec("azure/gpt-5.4-nano", false)];
        let harness = fx.harness::<V1>();
        let plan = fx.install(&harness, &models, &config, None);
        fx.apply(&plan).unwrap();

        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(!text.ends_with('\n'), "the original file ending is kept");
        let value = jsonc::read_value(&text).unwrap();
        let provider = &value["provider"]["litellm"];
        assert_eq!(provider["name"], json!("Work"));
        assert_eq!(provider["models"]["mine"], json!({"name": "Mine"}));
        assert_eq!(
            provider["models"]["azure/gpt-5.4-nano"]["options"],
            json!({"x": 1})
        );
        assert_eq!(
            provider["models"]["azure/gpt-5.4-nano"]["name"],
            json!("Custom")
        );
        let record = plan.record.unwrap();
        assert!(record.added_models.is_empty());
        assert!(!record.created_provider);

        fx.apply(&harness.plan_uninstall(&record).unwrap()).unwrap();
        assert!(read(&config_path)["provider"]["litellm"]["models"]["mine"].is_object());
    }

    const V2_CONFIG: &str = r#"{
  "$schema": "https://opencode.ai/config.json",
  // mine
  "model": "github-copilot/gpt-6-luna",
  "providers": {
    "litellm": {
      "name": "LiteLLM",
      "package": "@opencode/ai/providers/openai-compatible",
      "settings": {"baseURL": "https://old.example.com/v1"},
      "models": {
        "gpt-6.1-sol": {
          "name": "gpt-6.1-sol",
          "variants": [
            {"id": "none", "settings": {"reasoningEffort": "none"}},
            {"id": "batch", "body": {"service_tier": "flex"}}
          ]
        }
      }
    }
  }
}
"#;

    const OTHER_CREDENTIAL: &str =
        r#"[{"id": "litellm", "connections": [{"type": "credential", "id": "cred_old"}]}]"#;

    #[test]
    fn v2_writes_providers_and_stores_the_key_through_the_cli() {
        let mut fx = Fixture::new();
        fx.creds.api_key = "sk-secret".into();
        let config_path = fx.write_config("opencode.jsonc", V2_CONFIG);
        cli::tests::fake_cli(&fx.paths, OTHER_CREDENTIAL);

        let config = Config::default();
        let mut sol = spec("gpt-6.1-sol", true);
        sol.pricing.cache_read = Some(0.01);
        sol.reasoning_efforts = Some(vec!["low".into(), "high".into(), "max".into()]);
        let models = vec![sol, spec("plain", false)];
        let harness = fx.harness::<V2>();
        assert!(harness.detect() && harness.is_installed());
        let plan = fx.install(&harness, &models, &config, None);

        let command = plan
            .changes
            .iter()
            .find(|c| matches!(c, Change::Command { .. }))
            .unwrap();
        let preview = crate::ui::strip_ansi(&crate::ui::change_preview(command));
        assert!(!preview.contains("sk-secret"), "{preview}");
        assert!(!format!("{command:?}").contains("sk-secret"));
        fx.apply(&plan).unwrap();

        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(text.contains("// mine"));
        let value = jsonc::read_value(&text).unwrap();
        assert!(value.get("provider").is_none());
        let provider = &value["providers"]["litellm"];
        assert_eq!(
            provider["package"],
            json!("@opencode/ai/providers/openai-compatible")
        );
        assert_eq!(
            provider["settings"]["baseURL"],
            json!("https://llm.example.com/v1")
        );
        let model = &provider["models"]["gpt-6.1-sol"];
        assert_eq!(
            model["capabilities"],
            json!({"tools": true, "input": ["text", "image"], "output": ["text"]})
        );
        assert_eq!(
            model["cost"],
            json!({"input": 0.05, "output": 0.4, "cache": {"read": 0.01}})
        );
        assert_eq!(
            model["variants"],
            json!([
                {"id": "low", "settings": {"reasoningEffort": "low"}},
                {"id": "high", "settings": {"reasoningEffort": "high"}},
                {"id": "max", "settings": {"reasoningEffort": "max"}},
                {"id": "batch", "body": {"service_tier": "flex"}}
            ])
        );
        assert!(provider["models"]["plain"].get("variants").is_none());

        assert_eq!(
            cli::tests::calls(&fx.paths)
                .lines()
                .filter(|l| l.starts_with("auth ") && *l != "auth list --format json")
                .collect::<Vec<_>>(),
            [
                "auth export litellm",
                "auth import",
                "auth switch litellm cred_liteton_litellm"
            ]
        );
        let imported: Value = serde_json::from_str(
            &std::fs::read_to_string(fx.paths.opencode_bin.with_file_name("stdin.log")).unwrap(),
        )
        .unwrap();
        assert_eq!(imported[0]["id"], json!(CREDENTIAL_ID));
        assert_eq!(imported[0]["value"]["key"], json!("sk-secret"));
        assert!(fx.backup("opencode2/opencode-credentials.json").exists());

        let record = plan.record.unwrap();
        assert_eq!(record.added_models, vec!["plain".to_string()]);
        assert!(!record.created_provider);
        assert_eq!(record.created_secret.as_deref(), Some(CREDENTIAL_ID));

        cli::tests::fake_cli(
            &fx.paths,
            r#"[{"id": "litellm", "connections": [{"id": "cred_old"}, {"id": "cred_liteton_litellm"}]}]"#,
        );
        let again = fx.install(&harness, &models, &config, Some(&record));
        assert!(again.changes.is_empty(), "{:?}", again.changes);

        fx.apply(&harness.plan_uninstall(&record).unwrap()).unwrap();
        assert!(
            cli::tests::calls(&fx.paths)
                .lines()
                .any(|l| l == "auth logout litellm cred_liteton_litellm")
        );
        let value = read(&config_path);
        let models = &value["providers"]["litellm"]["models"];
        assert!(models.get("plain").is_none());
        assert!(models["gpt-6.1-sol"].is_object());
    }

    #[test]
    fn v2_needs_the_v2_cli() {
        let fx = Fixture::new();
        let ctx = InstallCtx {
            creds: &fx.creds,
            models: &[spec("m", false)],
            config: &Config::default(),
        };
        let err = fx.harness::<V2>().plan_install(&ctx, None).unwrap_err();
        assert!(format!("{err:#}").contains("opencode 2 was not found"));
    }

    #[test]
    fn both_versions_share_the_file() {
        let fx = Fixture::new();
        let config_path = fx.write_config("opencode.jsonc", USER_CONFIG);
        cli::tests::fake_cli(&fx.paths, "[]");
        let config = Config::default();
        let models = vec![spec("m", true)];
        let v1 = fx.harness::<V1>();
        let v2 = fx.harness::<V2>();
        let plan_v1 = fx.install(&v1, &models, &config, None);
        let plan_v2 = fx.install(&v2, &models, &config, None);

        fx.apply(&plan_v1).unwrap();
        let stale = fx.apply(&plan_v2).unwrap_err();
        assert!(
            stale
                .downcast_ref::<crate::harness::apply::StalePlan>()
                .is_some()
        );
        fx.apply(&fx.install(&v2, &models, &config, None)).unwrap();

        let value = read(&config_path);
        assert_eq!(
            value["provider"]["litellm"]["models"]["m"]["variants"]["low"],
            json!({"reasoningEffort": "low"})
        );
        assert_eq!(
            value["providers"]["litellm"]["models"]["m"]["variants"][0],
            json!({"id": "low", "settings": {"reasoningEffort": "low"}})
        );
        assert!(value["provider"]["sglang"].is_object());
    }

    #[test]
    fn v2_inherits_the_v1_record_only_after_migration() {
        let fx = Fixture::new();
        let mut state = InstallState::default();
        let v1_record = HarnessRecord {
            added_models: vec!["m".into()],
            created_provider: true,
            created_secret: Some(PROVIDER_ID.into()),
            ..HarnessRecord::default()
        };
        let v2 = fx.harness::<V2>();
        assert_eq!(v2.inherited_record(&state), None, "no opencode 1 record");

        state
            .harnesses
            .insert(HarnessId::Opencode.key().into(), v1_record);
        fx.write_config(
            "opencode.jsonc",
            r#"{"provider": {"litellm": {"models": {"m": {}}}}, "providers": {"litellm": {}}}"#,
        );
        assert_eq!(v2.inherited_record(&state), None, "opencode 1 still has it");

        fx.write_config(
            "opencode.jsonc",
            r#"{"providers": {"litellm": {"models": {"m": {}}}}}"#,
        );
        let inherited = v2.inherited_record(&state).unwrap();
        assert_eq!(inherited.added_models, ["m"]);
        assert!(inherited.created_provider);
        assert_eq!(
            inherited.created_secret, None,
            "auth.json is not opencode 2's"
        );
        assert_eq!(fx.harness::<V1>().inherited_record(&state), None);
    }
}
