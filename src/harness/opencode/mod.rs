//! opencode 1, opencode 2 and Kilo Code (an opencode 1 fork) share one install flow.

mod auth_file;
mod cli;
mod format;
mod kilo;
mod v1;
mod v2;
mod variants;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Result, anyhow};
use jsonc_parser::cst::CstObject;
use serde_json::{Map, Value, json};

pub use cli::OpencodeCli;
pub use v1::approximated_tier;

use super::{
    Change, Harness, HarnessId, HarnessPaths, InstallCtx, PROVIDER_ID, Plan, binary_on_path,
    deselected, next_record, read_optional,
};
use crate::config::{Config, HarnessRecord, InstallState, key_fingerprint};
use crate::jsonc;
use crate::litellm::ModelSpec;
use auth_file::AuthFile;
use cli::CliCredentials;
use format::Format;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum App {
    OpenCode1,
    OpenCode2,
    Kilo,
}

impl App {
    fn id(self) -> HarnessId {
        match self {
            Self::OpenCode1 => HarnessId::Opencode,
            Self::OpenCode2 => HarnessId::Opencode2,
            Self::Kilo => HarnessId::Kilo,
        }
    }

    fn format(self) -> Format {
        match self {
            Self::OpenCode1 | Self::Kilo => Format::V1,
            Self::OpenCode2 => Format::V2,
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::OpenCode1 => "opencode: run /models; the models are listed under \"LiteLLM\".",
            Self::OpenCode2 => "opencode 2: run /models; the models are listed under \"LiteLLM\".",
            Self::Kilo => "Kilo Code: the models are listed under \"LiteLLM\" in the model picker.",
        }
    }

    fn schema_url(self) -> &'static str {
        match self {
            Self::OpenCode1 | Self::OpenCode2 => "https://opencode.ai/config.json",
            Self::Kilo => "https://app.kilo.ai/config.json",
        }
    }

    fn config_files(self) -> &'static [&'static str] {
        match self {
            Self::OpenCode1 | Self::OpenCode2 => &["opencode.jsonc", "opencode.json"],
            // Kilo reads all of these and saves its settings to the first one that exists.
            Self::Kilo => &[
                "kilo.jsonc",
                "kilo.json",
                "opencode.jsonc",
                "opencode.json",
                "config.json",
            ],
        }
    }
}

enum Keys {
    File(AuthFile),
    Cli(CliCredentials),
}

#[derive(Debug, Default)]
struct KeyChange {
    change: Option<Change>,
    created: Option<String>,
}

impl Keys {
    fn plan_set(&self, api_key: &str, record: Option<&HarnessRecord>) -> Result<KeyChange> {
        match self {
            Self::File(file) => file.plan(Some(api_key)),
            Self::Cli(cli) => cli.plan_set(api_key, record),
        }
    }

    fn plan_remove(&self, notes: &mut Vec<String>) -> Result<Option<Change>> {
        match self {
            Self::File(file) => Ok(file.plan(None)?.change),
            Self::Cli(cli) => Ok(cli.plan_remove(notes)),
        }
    }
}

pub struct OpenCode {
    app: App,
    paths: HarnessPaths,
    config_dir: PathBuf,
    cli: Rc<OpencodeCli>,
    keys: Keys,
}

impl OpenCode {
    pub fn new(app: App, paths: &HarnessPaths, cli: Rc<OpencodeCli>) -> Self {
        let (config_dir, keys) = match app {
            App::OpenCode1 => (
                paths.opencode_config_dir.clone(),
                Keys::File(AuthFile::new(paths.opencode_data_dir.join("auth.json"))),
            ),
            App::OpenCode2 => (
                paths.opencode_config_dir.clone(),
                Keys::Cli(CliCredentials::new(cli.clone())),
            ),
            App::Kilo => (
                paths.kilo_config_dir.clone(),
                Keys::File(AuthFile::new(paths.kilo_data_dir.join("auth.json"))),
            ),
        };
        Self {
            app,
            paths: paths.clone(),
            config_dir,
            cli,
            keys,
        }
    }

    fn config_path(&self) -> PathBuf {
        let files = self.app.config_files();
        files
            .iter()
            .map(|name| self.config_dir.join(name))
            .find(|path| path.exists())
            .unwrap_or_else(|| self.config_dir.join(files[0]))
    }

    fn config_value(&self) -> Option<Value> {
        read_optional(&self.config_path())
            .ok()
            .flatten()
            .and_then(|text| jsonc::read_value(&text).ok())
    }
}

impl Harness for OpenCode {
    fn id(&self) -> HarnessId {
        self.app.id()
    }

    fn detect(&self) -> bool {
        let config = self.config_value();
        let has = |key: &str| config.as_ref().is_some_and(|v| v.get(key).is_some());
        if has(self.app.format().root_key()) {
            return true;
        }
        match self.app {
            App::OpenCode1 => match self.cli.major() {
                Some(major) => major == 1,
                None => {
                    (self.config_dir.exists() || binary_on_path("opencode"))
                        && !has(Format::V2.root_key())
                }
            },
            App::OpenCode2 => self.cli.major() == Some(2),
            App::Kilo => self.config_dir.exists() || binary_on_path("kilo"),
        }
    }

    fn is_installed(&self) -> bool {
        self.config_value()
            .is_some_and(|v| v[self.app.format().root_key()][PROVIDER_ID].is_object())
    }

    fn hint(&self) -> &'static str {
        self.app.hint()
    }

    /// After a migration to opencode 2, the opencode 1 record still lists what liteton added.
    fn inherited_record(&self, state: &InstallState) -> Option<HarnessRecord> {
        let migrated = self
            .config_value()
            .is_none_or(|v| !v[Format::V1.root_key()][PROVIDER_ID].is_object());
        if self.app != App::OpenCode2 || !migrated {
            return None;
        }
        let v1 = state.harnesses.get(HarnessId::Opencode.key())?;
        Some(HarnessRecord {
            added_models: v1.added_models.clone(),
            created_provider: v1.created_provider,
            ..HarnessRecord::default()
        })
    }

    fn plan_install(&self, ctx: &InstallCtx, record: Option<&HarnessRecord>) -> Result<Plan> {
        let format = self.app.format();
        let path = self.config_path();
        let before = read_optional(&path)?;
        let root = jsonc::parse(before.as_deref().unwrap_or(""))?;
        let obj = root.object_value_or_set();
        if before.is_none() {
            jsonc::set_if_missing(&obj, "$schema", &json!(self.app.schema_url()));
        }
        let providers = child_object(&obj, format.root_key(), &path)?;
        let created_provider = providers.get(PROVIDER_ID).is_none();
        let provider = child_object(&providers, PROVIDER_ID, &path)?;
        jsonc::set_if_missing(&provider, "name", &json!("LiteLLM"));
        format.set_connection(&provider, &ctx.openai_base_url());
        let models = child_object(&provider, "models", &path)?;

        let mut newly_added = Vec::new();
        for model in ctx.models {
            if models.get(&model.id).is_none() {
                newly_added.push(model.id.clone());
            }
            write_model(&models, model, ctx.config, format);
        }
        let removed = deselected(record, ctx.models);
        remove_models(&models, removed.iter().copied());
        let mut previous = record.map(|r| r.previous.clone()).unwrap_or_default();
        if self.app == App::Kilo {
            let settings = kilo::config_settings(ctx.config, ctx.models);
            kilo::apply(&obj, &settings, kilo::CONFIG, &mut previous);
        }

        let after = inline_effort_variants(&jsonc::finish(&root, before.as_deref()), format)?;
        let mut changes: Vec<Change> = Change::file(path, before, after).into_iter().collect();
        let keys = self.keys.plan_set(&ctx.creds.api_key, record)?;
        changes.extend(keys.change);
        if self.app == App::Kilo {
            changes.extend(kilo::plan_vscode_install(
                &self.paths,
                ctx.config,
                ctx.models,
                &mut previous,
            )?);
        }

        let mut next = next_record(
            record,
            newly_added,
            &removed,
            created_provider,
            keys.created,
        );
        next.key_fingerprint = Some(key_fingerprint(&ctx.creds.api_key));
        next.previous = previous;
        Ok(Plan {
            harness: self.app.id(),
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
            let root_key = self.app.format().root_key();
            if let Some(obj) = root.object_value()
                && let Some(providers) = obj.object_value(root_key)
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
                    if providers.properties().is_empty()
                        && let Some(prop) = obj.get(root_key)
                    {
                        prop.remove();
                    }
                }
            }
            if self.app == App::Kilo
                && let Some(obj) = root.object_value()
            {
                kilo::restore(&obj, kilo::CONFIG, &record.previous);
            }
            let after = jsonc::finish(&root, Some(&before));
            changes.extend(Change::file(path, Some(before), after));
        }
        if record.created_secret.is_some() {
            changes.extend(self.keys.plan_remove(&mut notes)?);
        }
        if self.app == App::Kilo {
            changes.extend(kilo::plan_vscode_uninstall(&self.paths, &record.previous)?);
        }
        Ok(Plan {
            harness: self.app.id(),
            changes,
            record: None,
            notes,
        })
    }
}

fn child_object(parent: &CstObject, key: &str, path: &Path) -> Result<CstObject> {
    parent
        .object_value_or_create(key)
        .ok_or_else(|| anyhow!("\"{key}\" in {} is not an object", path.display()))
}

fn write_model(models: &CstObject, model: &ModelSpec, config: &Config, format: Format) {
    let mut value = format.model_entry(model, config);
    // A display name the user picked wins over ours.
    if models
        .object_value(&model.id)
        .is_some_and(|e| e.get("name").is_some())
    {
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
    variants::sync(models, &model.id, &efforts, format);
    format.tidy_model(models, model, config);
}

fn inline_effort_variants(text: &str, format: Format) -> Result<String> {
    let path = [
        format.root_key(),
        PROVIDER_ID,
        "models",
        "*",
        "variants",
        "*",
    ];
    jsonc::inline_objects(text, &path, |key, variant| {
        *variant == format.effort_variant(format.variant_id(key, variant))
    })
}

fn remove_models<'a>(models: &CstObject, ids: impl IntoIterator<Item = &'a String>) {
    for id in ids {
        if let Some(prop) = models.get(id) {
            prop.remove();
        }
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
            self.write_in(&self.paths.opencode_config_dir, name, text)
        }

        pub fn write_in(&self, dir: &Path, name: &str, text: &str) -> PathBuf {
            std::fs::create_dir_all(dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }

        pub fn harness(&self, app: App) -> OpenCode {
            OpenCode::new(
                app,
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
        let harness = fx.harness(App::OpenCode1);
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
        let harness = fx.harness(App::OpenCode1);
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
        let harness = fx.harness(App::OpenCode2);
        assert!(harness.detect() && harness.is_installed());
        let plan = fx.install(&harness, &models, &config, None);

        let command = plan
            .changes
            .iter()
            .find(|c| matches!(c, Change::Command { .. }))
            .unwrap();
        let preview = crate::ui::strip_ansi(&crate::ui::change_preview(command));
        assert!(!preview.contains("sk-secret"), "{preview}");
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
        let err = fx
            .harness(App::OpenCode2)
            .plan_install(&ctx, None)
            .unwrap_err();
        assert!(format!("{err:#}").contains("opencode 2 was not found"));
    }

    #[test]
    fn both_versions_share_the_file() {
        let fx = Fixture::new();
        let config_path = fx.write_config("opencode.jsonc", USER_CONFIG);
        cli::tests::fake_cli(&fx.paths, "[]");
        let config = Config::default();
        let models = vec![spec("m", true)];
        let v1 = fx.harness(App::OpenCode1);
        let v2 = fx.harness(App::OpenCode2);
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
        let v2 = fx.harness(App::OpenCode2);
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
        assert_eq!(fx.harness(App::OpenCode1).inherited_record(&state), None);
    }

    #[test]
    fn effort_variants_go_on_one_line_each() {
        let fx = Fixture::new();
        let config_path = fx.write_config(
            "opencode.jsonc",
            r#"{
  "providers": {
    "litellm": {
      "models": {
        "m": {
          "variants": [
            {
              "id": "low",
              "settings": {
                "reasoningEffort": "low"
              }
            },
            {
              "id": "batch",
              "body": {"service_tier": "flex"}
            }
          ]
        }
      }
    }
  }
}
"#,
        );
        cli::tests::fake_cli(&fx.paths, "[]");
        let config = Config::default();
        let models = vec![spec("m", true)];
        let v2 = fx.harness(App::OpenCode2);
        fx.apply(&fx.install(&v2, &models, &config, None)).unwrap();
        let text = std::fs::read_to_string(&config_path).unwrap();
        for level in ["low", "medium", "high"] {
            let line = format!(
                "{{ \"id\": \"{level}\", \"settings\": {{ \"reasoningEffort\": \"{level}\" }} }}"
            );
            assert!(text.contains(&line), "{text}");
        }
        assert!(
            text.contains("{\n              \"id\": \"batch\","),
            "a hand-written variant keeps its layout: {text}"
        );
        let again = fx.install(&v2, &models, &config, None);
        assert!(
            !again
                .changes
                .iter()
                .any(|c| matches!(c, Change::File { .. })),
            "{:?}",
            again.changes
        );

        let v1 = fx.harness(App::OpenCode1);
        fx.apply(&fx.install(&v1, &models, &config, None)).unwrap();
        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            text.contains("\"low\": { \"reasoningEffort\": \"low\" }"),
            "{text}"
        );
    }

    #[test]
    fn writes_kilo_files_and_leaves_opencode_alone() {
        let fx = Fixture::new();
        let kilo = fx.harness(App::Kilo);
        let seeded = fx.write_in(
            &fx.paths.kilo_config_dir,
            "kilo.jsonc",
            "{\n  \"$schema\": \"https://app.kilo.ai/config.json\"\n}",
        );
        assert!(kilo.detect() && !kilo.is_installed());

        let models = vec![spec("gpt-6-luna", true)];
        let plan = fx.install(&kilo, &models, &Config::default(), None);
        fx.apply(&plan).unwrap();
        let value = read(&seeded);
        assert_eq!(value["$schema"], json!("https://app.kilo.ai/config.json"));
        let provider = &value["provider"]["litellm"];
        assert_eq!(
            provider["options"]["baseURL"],
            json!("https://llm.example.com/v1")
        );
        assert_eq!(
            provider["models"]["gpt-6-luna"]["variants"]["low"],
            json!({"reasoningEffort": "low"})
        );
        let auth_path = fx.paths.kilo_data_dir.join("auth.json");
        assert_eq!(
            read(&auth_path)["litellm"],
            json!({"type": "api", "key": "sk-1"})
        );
        assert!(!fx.paths.opencode_config_dir.exists());
        assert!(!fx.paths.opencode_data_dir.exists());
        assert!(fx.backup("kilo/kilo.jsonc").exists());

        let record = plan.record.unwrap();
        let again = fx.install(&kilo, &models, &Config::default(), Some(&record));
        assert!(again.changes.is_empty(), "{:?}", again.changes);

        fx.apply(&kilo.plan_uninstall(&record).unwrap()).unwrap();
        assert_eq!(
            read(&seeded),
            json!({"$schema": "https://app.kilo.ai/config.json"})
        );
        assert!(read(&auth_path).get("litellm").is_none());
        assert!(!fx.paths.vscode_user_dir.join("settings.json").exists());
    }

    #[test]
    fn writes_the_config_kilo_saves_to() {
        let fx = Fixture::new();
        let kilo = fx.harness(App::Kilo);
        let config = fx.write_in(&fx.paths.kilo_config_dir, "opencode.json", "{}");
        fx.write_in(&fx.paths.kilo_config_dir, "config.json", "{}");
        let plan = fx.install(&kilo, &[spec("m", false)], &Config::default(), None);
        fx.apply(&plan).unwrap();
        assert!(read(&config)["provider"]["litellm"].is_object());
        assert!(!fx.paths.kilo_config_dir.join("kilo.jsonc").exists());

        let fresh = Fixture::new();
        let plan = fresh.install(
            &fresh.harness(App::Kilo),
            &[spec("m", false)],
            &Config::default(),
            None,
        );
        fresh.apply(&plan).unwrap();
        let created = read(&fresh.paths.kilo_config_dir.join("kilo.jsonc"));
        assert_eq!(created["$schema"], json!("https://app.kilo.ai/config.json"));
    }

    #[test]
    fn kilo_uses_only_litellm_and_restores_on_uninstall() {
        let fx = Fixture::new();
        let kilo_config = fx.write_in(
            &fx.paths.kilo_config_dir,
            "kilo.jsonc",
            "{\n  // keep\n  \"share\": \"manual\",\n  \"experimental\": {\"batch_tool\": true}\n}\n",
        );
        std::fs::create_dir_all(
            fx.paths
                .vscode_extensions_dir
                .join("kilocode.kilo-code-7.8.14-darwin-arm64"),
        )
        .unwrap();
        let vscode_settings = fx.write_in(
            &fx.paths.vscode_user_dir,
            "settings.json",
            "{\n  // mine\n  \"editor.fontSize\": 14\n}\n",
        );
        let config = Config {
            kilo_model: Some("big".into()),
            kilo_small_model: Some("small".into()),
            ..Config::default()
        };
        let models = vec![spec("big", true), spec("small", false)];
        let kilo = fx.harness(App::Kilo);
        let plan = fx.install(&kilo, &models, &config, None);
        fx.apply(&plan).unwrap();

        let text = std::fs::read_to_string(&kilo_config).unwrap();
        assert!(text.contains("// keep"));
        let value = jsonc::read_value(&text).unwrap();
        assert_eq!(value["enabled_providers"], json!(["litellm"]));
        assert_eq!(value["share"], json!("disabled"));
        assert_eq!(value["remote_control"], json!(false));
        assert_eq!(
            value["experimental"],
            json!({"batch_tool": true, "openTelemetry": false})
        );
        assert_eq!(value["model"], json!("litellm/big"));
        assert_eq!(value["small_model"], json!("litellm/small"));

        let text = std::fs::read_to_string(&vscode_settings).unwrap();
        assert!(text.contains("// mine"));
        assert_eq!(
            jsonc::read_value(&text).unwrap(),
            json!({
                "editor.fontSize": 14,
                "kilo-code.new.autocomplete.enableAutoTrigger": false,
                "kilo-code.new.model.providerID": "litellm",
                "kilo-code.new.model.modelID": "big"
            })
        );

        let record = plan.record.unwrap();
        assert_eq!(record.previous["kilo:share"], json!("manual"));
        assert_eq!(
            record.previous["kilo:experimental/openTelemetry"],
            Value::Null
        );
        let again = fx.install(&kilo, &models, &config, Some(&record));
        assert!(again.changes.is_empty(), "{:?}", again.changes);
        assert_eq!(again.record.unwrap().previous, record.previous);

        fx.apply(&kilo.plan_uninstall(&record).unwrap()).unwrap();
        let text = std::fs::read_to_string(&kilo_config).unwrap();
        assert!(text.contains("// keep"));
        assert_eq!(
            jsonc::read_value(&text).unwrap(),
            json!({"share": "manual", "experimental": {"batch_tool": true}})
        );
        let text = std::fs::read_to_string(&vscode_settings).unwrap();
        assert!(text.contains("// mine"));
        assert_eq!(
            jsonc::read_value(&text).unwrap(),
            json!({"editor.fontSize": 14})
        );
    }

    #[test]
    fn kilo_models_are_set_only_when_installed() {
        let fx = Fixture::new();
        let config = Config {
            kilo_model: Some("gone".into()),
            kilo_small_model: Some("m".into()),
            ..Config::default()
        };
        let plan = fx.install(&fx.harness(App::Kilo), &[spec("m", false)], &config, None);
        fx.apply(&plan).unwrap();
        let value = read(&fx.paths.kilo_config_dir.join("kilo.jsonc"));
        assert!(value.get("model").is_none());
        assert_eq!(value["small_model"], json!("litellm/m"));
    }

    #[test]
    fn kilo_writes_each_vscode_profile_with_its_own_settings() {
        let fx = Fixture::new();
        let user = &fx.paths.vscode_user_dir;
        std::fs::create_dir_all(
            fx.paths
                .vscode_extensions_dir
                .join("kilocode.kilo-code-7.8.14"),
        )
        .unwrap();
        fx.write_in(
            &user.join("globalStorage"),
            "storage.json",
            &json!({"userDataProfiles": [
                {"location": "-24b23f00", "name": "DSA"},
                {"location": "builtin/agents", "name": "Agents",
                 "useDefaultFlags": {"settings": true}}
            ]})
            .to_string(),
        );
        let dsa = fx.write_in(
            &user.join("profiles/-24b23f00"),
            "settings.json",
            r#"{"kilo-code.new.autocomplete.enableAutoTrigger": true}"#,
        );
        let config = Config {
            kilo_model: Some("m".into()),
            ..Config::default()
        };
        let kilo = fx.harness(App::Kilo);
        let plan = fx.install(&kilo, &[spec("m", false)], &config, None);
        fx.apply(&plan).unwrap();

        let expected = json!({
            "kilo-code.new.autocomplete.enableAutoTrigger": false,
            "kilo-code.new.model.providerID": "litellm",
            "kilo-code.new.model.modelID": "m"
        });
        assert_eq!(read(&user.join("settings.json")), expected);
        assert_eq!(read(&dsa), expected);
        assert!(!user.join("profiles/builtin/agents/settings.json").exists());

        let record = plan.record.unwrap();
        assert_eq!(
            record.previous["vscode/profiles/-24b23f00:kilo-code.new.autocomplete.enableAutoTrigger"],
            json!(true)
        );
        fx.apply(&kilo.plan_uninstall(&record).unwrap()).unwrap();
        assert_eq!(read(&user.join("settings.json")), json!({}));
        assert_eq!(
            read(&dsa),
            json!({"kilo-code.new.autocomplete.enableAutoTrigger": true})
        );
    }
}
