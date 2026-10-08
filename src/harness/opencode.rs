use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use jsonc_parser::cst::CstObject;
use serde_json::{Map, Value, json};

use super::{
    Change, Harness, HarnessId, HarnessPaths, InstallCtx, PROVIDER_ID, Plan, binary_on_path,
    deselected, next_record, read_optional,
};
use crate::config::{Config, HarnessRecord};
use crate::jsonc;
use crate::litellm::{ModelSpec, PriceTier, Pricing};

pub struct OpenCode {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl OpenCode {
    pub fn new(paths: &HarnessPaths) -> Self {
        Self {
            config_dir: paths.opencode_config_dir.clone(),
            data_dir: paths.opencode_data_dir.clone(),
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

    fn auth_path(&self) -> PathBuf {
        self.data_dir.join("auth.json")
    }
}

impl Harness for OpenCode {
    fn id(&self) -> HarnessId {
        HarnessId::Opencode
    }

    fn detect(&self) -> bool {
        self.config_dir.exists() || binary_on_path("opencode")
    }

    fn is_installed(&self) -> bool {
        read_optional(&self.config_path())
            .ok()
            .flatten()
            .and_then(|text| jsonc::read_value(&text).ok())
            .is_some_and(|v| v["provider"][PROVIDER_ID].is_object())
    }

    fn plan_install(&self, ctx: &InstallCtx, record: Option<&HarnessRecord>) -> Result<Plan> {
        let path = self.config_path();
        let before = read_optional(&path)?;
        let root = jsonc::parse(before.as_deref().unwrap_or(""))?;
        let obj = root.object_value_or_set();
        if before.is_none() {
            jsonc::set_if_missing(&obj, "$schema", &json!("https://opencode.ai/config.json"));
        }
        let providers = obj
            .object_value_or_create("provider")
            .ok_or_else(|| anyhow!("\"provider\" in {} is not an object", path.display()))?;
        let created_provider = providers.get(PROVIDER_ID).is_none();
        let provider = providers
            .object_value_or_create(PROVIDER_ID)
            .ok_or_else(|| {
                anyhow!(
                    "\"provider.{PROVIDER_ID}\" in {} is not an object",
                    path.display()
                )
            })?;
        jsonc::set_if_missing(&provider, "name", &json!("LiteLLM"));
        jsonc::merge_object(
            &provider,
            json!({"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": ctx.openai_base_url()}}).as_object().unwrap(),
        );
        let models = provider
            .object_value_or_create("models")
            .ok_or_else(|| anyhow!("\"provider.{PROVIDER_ID}.models\" is not an object"))?;

        let mut newly_added = Vec::new();
        for model in ctx.models {
            let mut value = model_entry(model, ctx.config);
            match models.object_value(&model.id) {
                None => newly_added.push(model.id.clone()),
                // A display name the user picked wins over ours.
                Some(existing) if existing.get("name").is_some() => {
                    value.as_object_mut().unwrap().remove("name");
                }
                Some(_) => {}
            }
            let mut entry = Map::new();
            entry.insert(model.id.clone(), value);
            jsonc::merge_object(&models, &entry);
            if ctx.config.opencode_approximate_long_context != Some(true) {
                drop_approximation(&models, model);
            }
        }
        let removed = deselected(record, ctx.models);
        for id in &removed {
            if let Some(prop) = models.get(id) {
                prop.remove();
            }
        }

        let after = jsonc::finish(&root, before.as_deref());
        let mut changes = vec![Change::File {
            path,
            before,
            after,
            private: false,
            summary: vec![],
        }];
        let (auth_change, created_secret) = self.auth_change(Some(&ctx.creds.api_key))?;
        changes.extend(auth_change);
        changes.retain(|c| !matches!(c, Change::File { before: Some(b), after, .. } if b == after));

        Ok(Plan {
            harness: self.id(),
            changes,
            record: Some(next_record(
                record,
                newly_added,
                &removed,
                created_provider,
                created_secret,
            )),
            notes: vec![],
        })
    }

    fn plan_uninstall(&self, record: &HarnessRecord) -> Result<Plan> {
        let mut changes = Vec::new();
        let path = self.config_path();
        if let Some(before) = read_optional(&path)? {
            let root = jsonc::parse(&before)?;
            if let Some(providers) = root.object_value().and_then(|o| o.object_value("provider"))
                && let Some(provider) = providers.object_value(PROVIDER_ID)
            {
                if let Some(models) = provider.object_value("models") {
                    for id in &record.added_models {
                        if let Some(prop) = models.get(id) {
                            prop.remove();
                        }
                    }
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
        if record.created_secret.is_some() {
            changes.extend(self.auth_change(None)?.0);
        }
        Ok(Plan {
            harness: self.id(),
            changes,
            record: None,
            notes: vec![],
        })
    }
}

impl OpenCode {
    /// Sets (or with `None`, removes) the provider's entry in auth.json, keeping all other entries.
    fn auth_change(&self, api_key: Option<&str>) -> Result<(Option<Change>, Option<String>)> {
        let path = self.auth_path();
        let before = read_optional(&path)?;
        let mut auth: Map<String, Value> = match &before {
            Some(text) if !text.trim().is_empty() => {
                serde_json::from_str(text).with_context(|| format!("parsing {}", path.display()))?
            }
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
            path,
            before,
            after,
            private: true,
            summary: vec![format!(
                "{action} the \"{PROVIDER_ID}\" API key entry (other entries are kept)"
            )],
        });
        Ok((change, (!existed).then(|| PROVIDER_ID.to_string())))
    }
}

fn model_entry(model: &ModelSpec, config: &Config) -> Value {
    let mut entry = Map::new();
    entry.insert("name".into(), json!(model.display_name));
    entry.insert("tool_call".into(), json!(model.tool_calling));
    entry.insert("reasoning".into(), json!(model.reasoning));
    entry.insert("attachment".into(), json!(model.vision));
    let input = if model.vision {
        json!(["text", "image"])
    } else {
        json!(["text"])
    };
    entry.insert(
        "modalities".into(),
        json!({"input": input, "output": ["text"]}),
    );
    if let (Some(context), Some(output)) = (model.context_window, model.max_output_tokens) {
        entry.insert(
            "limit".into(),
            json!({"context": context, "output": output}),
        );
    }
    if let Some(mut cost) = cost_object(&model.pricing) {
        let approximate = config.opencode_approximate_long_context == Some(true);
        if let Some(long) =
            over_200k_tier(model, approximate).and_then(|tier| cost_object(&tier.pricing))
        {
            cost.insert("context_over_200k".into(), Value::Object(long));
        }
        entry.insert("cost".into(), Value::Object(cost));
    }
    if model.reasoning {
        let variants: Map<String, Value> = config
            .reasoning_efforts_for(&model.id)
            .into_iter()
            .map(|level| (level.clone(), json!({"reasoningEffort": level})))
            .collect();
        entry.insert("variants".into(), Value::Object(variants));
    }
    Value::Object(entry)
}

/// opencode's config has one long-context slot, fixed at 200k.
fn over_200k_tier(model: &ModelSpec, approximate: bool) -> Option<&PriceTier> {
    let exact = model.tiers.iter().find(|tier| tier.above_tokens == 200_000);
    exact.or_else(|| approximated_tier(model).filter(|_| approximate))
}

/// The tier that would stand in for 200k: the lowest priced one above it, when none sits at 200k.
pub fn approximated_tier(model: &ModelSpec) -> Option<&PriceTier> {
    if model.tiers.iter().any(|tier| tier.above_tokens == 200_000) {
        return None;
    }
    model
        .tiers
        .iter()
        .find(|tier| tier.above_tokens > 200_000 && cost_object(&tier.pricing).is_some())
}

/// After opting out, removes a `context_over_200k` that is exactly the approximation liteton
/// wrote before. A value the user typed in is kept.
fn drop_approximation(models: &CstObject, model: &ModelSpec) {
    let Some(written) = approximated_tier(model).and_then(|tier| cost_object(&tier.pricing)) else {
        return;
    };
    if let Some(cost) = models
        .object_value(&model.id)
        .and_then(|entry| entry.object_value("cost"))
        && let Some(prop) = cost.get("context_over_200k")
        && prop.value().and_then(|v| jsonc::node_to_value(&v)) == Some(Value::Object(written))
    {
        prop.remove();
    }
}

fn cost_object(pricing: &Pricing) -> Option<Map<String, Value>> {
    let (Some(input), Some(output)) = (pricing.input, pricing.output) else {
        return None;
    };
    let mut cost = Map::new();
    cost.insert("input".into(), json!(input));
    cost.insert("output".into(), json!(output));
    if let Some(read) = pricing.cache_read {
        cost.insert("cache_read".into(), json!(read));
    }
    if let Some(write) = pricing.cache_write {
        cost.insert("cache_write".into(), json!(write));
    }
    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Credentials;
    use crate::harness::tests::{paths_in, spec};

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

    fn ctx<'a>(
        creds: &'a Credentials,
        models: &'a [ModelSpec],
        config: &'a Config,
    ) -> InstallCtx<'a> {
        InstallCtx {
            creds,
            models,
            config,
        }
    }

    #[test]
    fn merges_provider_without_touching_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        std::fs::create_dir_all(&paths.opencode_config_dir).unwrap();
        std::fs::write(
            paths.opencode_config_dir.join("opencode.jsonc"),
            USER_CONFIG,
        )
        .unwrap();
        std::fs::create_dir_all(&paths.opencode_data_dir).unwrap();
        std::fs::write(
            paths.opencode_data_dir.join("auth.json"),
            r#"{"openrouter": {"type": "api", "key": "or"}}"#,
        )
        .unwrap();

        let creds = Credentials {
            base_url: "https://llm.example.com".into(),
            api_key: "sk-1".into(),
        };
        let config = Config::default();
        let models = vec![spec("azure/gpt-5.4-nano", true)];
        let harness = OpenCode::new(&paths);
        let plan = harness
            .plan_install(&ctx(&creds, &models, &config), None)
            .unwrap();
        crate::harness::apply::apply(&plan, &dir.path().join("backup"), None).unwrap();

        let text =
            std::fs::read_to_string(paths.opencode_config_dir.join("opencode.jsonc")).unwrap();
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
            model["variants"]["xhigh"],
            json!({"reasoningEffort": "xhigh"})
        );

        let auth: Value = serde_json::from_str(
            &std::fs::read_to_string(paths.opencode_data_dir.join("auth.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(auth["openrouter"]["key"], json!("or"));
        assert_eq!(auth["litellm"], json!({"type": "api", "key": "sk-1"}));
        assert!(dir.path().join("backup/opencode/opencode.jsonc").exists());

        let record = plan.record.unwrap();
        assert_eq!(record.added_models, vec!["azure/gpt-5.4-nano".to_string()]);
        assert!(record.created_provider);

        // Re-running with the same input changes nothing.
        let again = harness
            .plan_install(&ctx(&creds, &models, &config), Some(&record))
            .unwrap();
        assert!(again.changes.is_empty(), "{:?}", again.changes);

        let uninstall = harness.plan_uninstall(&record).unwrap();
        crate::harness::apply::apply(&uninstall, &dir.path().join("backup2"), None).unwrap();
        let text =
            std::fs::read_to_string(paths.opencode_config_dir.join("opencode.jsonc")).unwrap();
        let value = jsonc::read_value(&text).unwrap();
        assert!(value["provider"].get("litellm").is_none());
        assert!(value["provider"]["sglang"].is_object());
        assert!(text.contains("// my models"));
        let auth: Value = serde_json::from_str(
            &std::fs::read_to_string(paths.opencode_data_dir.join("auth.json")).unwrap(),
        )
        .unwrap();
        assert!(auth.get("litellm").is_none());
        assert_eq!(auth["openrouter"]["key"], json!("or"));
    }

    #[test]
    fn keeps_hand_edits_inside_the_provider() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        std::fs::create_dir_all(&paths.opencode_config_dir).unwrap();
        std::fs::write(
            paths.opencode_config_dir.join("opencode.json"),
            r#"{"provider": {"litellm": {"name": "Work", "models": {"mine": {"name": "Mine"}, "azure/gpt-5.4-nano": {"name": "Custom", "options": {"x": 1}}}}}}"#,
        )
        .unwrap();
        let creds = Credentials {
            base_url: "https://llm.example.com".into(),
            api_key: "sk-1".into(),
        };
        let config = Config::default();
        let models = vec![spec("azure/gpt-5.4-nano", false)];
        let harness = OpenCode::new(&paths);
        let plan = harness
            .plan_install(&ctx(&creds, &models, &config), None)
            .unwrap();
        crate::harness::apply::apply(&plan, &dir.path().join("backup"), None).unwrap();

        let text =
            std::fs::read_to_string(paths.opencode_config_dir.join("opencode.json")).unwrap();
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

        let uninstall = harness.plan_uninstall(&record).unwrap();
        crate::harness::apply::apply(&uninstall, &dir.path().join("backup2"), None).unwrap();
        let value = jsonc::read_value(
            &std::fs::read_to_string(paths.opencode_config_dir.join("opencode.json")).unwrap(),
        )
        .unwrap();
        assert!(value["provider"]["litellm"]["models"]["mine"].is_object());
    }

    #[test]
    fn writes_only_the_200k_tier_as_context_over_200k() {
        let tier = |above_tokens, input| PriceTier {
            above_tokens,
            pricing: Pricing {
                input: Some(input),
                output: Some(input * 4.0),
                ..Default::default()
            },
        };
        let mut model = spec("gpt", false);
        model.pricing.cache_read = Some(0.005);
        model.pricing.cache_write = Some(0.0625);
        model.tiers = vec![tier(272_000, 0.2)];
        let entry = model_entry(&model, &Config::default());
        assert_eq!(
            entry["cost"],
            json!({"input": 0.05, "output": 0.4, "cache_read": 0.005, "cache_write": 0.0625})
        );

        model.tiers = vec![tier(200_000, 1.0), tier(272_000, 2.0)];
        let entry = model_entry(&model, &Config::default());
        assert_eq!(
            entry["cost"]["context_over_200k"],
            json!({"input": 1.0, "output": 4.0})
        );

        let approximate = Config {
            opencode_approximate_long_context: Some(true),
            ..Config::default()
        };
        let entry = model_entry(&model, &approximate);
        assert_eq!(
            entry["cost"]["context_over_200k"],
            json!({"input": 1.0, "output": 4.0}),
            "an exact 200k tier still wins"
        );
        model.tiers = vec![tier(128_000, 0.1), tier(272_000, 2.0), tier(512_000, 3.0)];
        let entry = model_entry(&model, &approximate);
        assert_eq!(
            entry["cost"]["context_over_200k"],
            json!({"input": 2.0, "output": 8.0}),
            "the lowest tier above 200k stands in"
        );
    }

    #[test]
    fn opting_out_removes_only_our_approximation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let creds = Credentials {
            base_url: "https://llm.example.com".into(),
            api_key: "sk-1".into(),
        };
        let mut luna = spec("gpt-6-luna", false);
        luna.tiers = vec![PriceTier {
            above_tokens: 272_000,
            pricing: Pricing {
                input: Some(0.2),
                output: Some(0.75),
                ..Default::default()
            },
        }];
        let mut astra = luna.clone();
        astra.id = "gpt-6-astra".into();
        let models = vec![luna, astra];
        let harness = OpenCode::new(&paths);
        let install = |config: &Config| {
            let plan = harness
                .plan_install(&ctx(&creds, &models, config), None)
                .unwrap();
            crate::harness::apply::apply(&plan, &dir.path().join("backup"), None).unwrap();
        };
        let config_path = paths.opencode_config_dir.join("opencode.jsonc");
        let read = || jsonc::read_value(&std::fs::read_to_string(&config_path).unwrap()).unwrap();

        install(&Config {
            opencode_approximate_long_context: Some(true),
            ..Config::default()
        });
        let models_path =
            |value: &Value, id: &str| value["provider"]["litellm"]["models"][id].clone();
        assert_eq!(
            models_path(&read(), "gpt-6-luna")["cost"]["context_over_200k"],
            json!({"input": 0.2, "output": 0.75})
        );

        let text = std::fs::read_to_string(&config_path).unwrap();
        let root = jsonc::parse(&text).unwrap();
        let astra_cost = root
            .object_value()
            .and_then(|o| o.object_value("provider"))
            .and_then(|o| o.object_value("litellm"))
            .and_then(|o| o.object_value("models"))
            .and_then(|o| o.object_value("gpt-6-astra"))
            .and_then(|o| o.object_value("cost"))
            .unwrap();
        astra_cost
            .get("context_over_200k")
            .unwrap()
            .set_value(jsonc::to_input(&json!({"input": 0.3, "output": 0.9})));
        std::fs::write(&config_path, jsonc::finish(&root, Some(&text))).unwrap();

        install(&Config::default());
        let value = read();
        assert!(
            models_path(&value, "gpt-6-luna")["cost"]
                .get("context_over_200k")
                .is_none()
        );
        assert_eq!(
            models_path(&value, "gpt-6-astra")["cost"]["context_over_200k"],
            json!({"input": 0.3, "output": 0.9}),
            "a hand-written value is kept"
        );
    }
}
