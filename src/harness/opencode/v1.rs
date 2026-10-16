use std::rc::Rc;

use jsonc_parser::cst::CstObject;
use serde_json::{Map, Value, json};

use super::auth_file::AuthFile;
use super::{OpencodeCli, V2, Version, input_modalities, limit};
use crate::config::Config;
use crate::harness::{HarnessId, HarnessPaths, binary_on_path};
use crate::jsonc;
use crate::litellm::{ModelSpec, PriceTier, Pricing};

/// opencode 1: `provider`, the key in auth.json, one long-context price slot at 200k.
pub struct V1;

impl Version for V1 {
    const HARNESS: HarnessId = HarnessId::Opencode;
    const ROOT_KEY: &'static str = "provider";
    const CLI_MAJOR: u32 = 1;
    const HINT: &'static str = "opencode: run /models; the models are listed under \"LiteLLM\".";
    type Store = AuthFile;

    fn store(paths: &HarnessPaths, _cli: Rc<OpencodeCli>) -> AuthFile {
        AuthFile::new(paths.opencode_data_dir.join("auth.json"))
    }

    fn set_connection(provider: &CstObject, base_url: &str) {
        jsonc::merge_object(
            provider,
            json!({"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": base_url}})
                .as_object()
                .unwrap(),
        );
    }

    fn model_entry(model: &ModelSpec, config: &Config) -> Value {
        let mut entry = Map::new();
        entry.insert("name".into(), json!(model.display_name));
        entry.insert("tool_call".into(), json!(model.tool_calling));
        entry.insert("reasoning".into(), json!(model.reasoning));
        entry.insert("attachment".into(), json!(model.vision));
        entry.insert(
            "modalities".into(),
            json!({"input": input_modalities(model), "output": ["text"]}),
        );
        if let Some(limit) = limit(model) {
            entry.insert("limit".into(), limit);
        }
        if let Some(mut base) = cost(&model.pricing) {
            let approximate = config.opencode_approximate_long_context == Some(true);
            if let Some(long) =
                over_200k_tier(model, approximate).and_then(|tier| cost(&tier.pricing))
            {
                base.insert("context_over_200k".into(), Value::Object(long));
            }
            entry.insert("cost".into(), Value::Object(base));
        }
        Value::Object(entry)
    }

    fn effort_variant(level: &str) -> Value {
        json!({"reasoningEffort": level})
    }

    fn read_variants(variants: &Value) -> Vec<(String, Value)> {
        match variants {
            Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            _ => Vec::new(),
        }
    }

    fn write_variants(variants: Vec<(String, Value)>) -> Value {
        Value::Object(variants.into_iter().collect())
    }

    fn tidy_model(models: &CstObject, model: &ModelSpec, config: &Config) {
        if config.opencode_approximate_long_context != Some(true) {
            drop_approximation(models, model);
        }
    }

    /// Its folder or a binary is enough, unless the config was already migrated to opencode 2.
    fn detect_without_cli(config: Option<&Value>, config_dir_exists: bool) -> bool {
        (config_dir_exists || binary_on_path("opencode"))
            && !config.is_some_and(|v| v.get(V2::ROOT_KEY).is_some())
    }
}

/// opencode 1's config has one long-context slot, fixed at 200k.
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
        .find(|tier| tier.above_tokens > 200_000 && tier.pricing.is_priced())
}

/// After opting out, removes a `context_over_200k` that is exactly the approximation liteton
/// wrote before. A value the user typed in is kept.
fn drop_approximation(models: &CstObject, model: &ModelSpec) {
    let Some(written) = approximated_tier(model).and_then(|tier| cost(&tier.pricing)) else {
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

/// Flat `cache_read`/`cache_write`.
fn cost(pricing: &Pricing) -> Option<Map<String, Value>> {
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
    use super::super::tests::{Fixture, read};
    use super::*;
    use crate::harness::tests::spec;

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
        let entry = V1::model_entry(&model, &Config::default());
        assert_eq!(
            entry["cost"],
            json!({"input": 0.05, "output": 0.4, "cache_read": 0.005, "cache_write": 0.0625})
        );

        model.tiers = vec![tier(200_000, 1.0), tier(272_000, 2.0)];
        let entry = V1::model_entry(&model, &Config::default());
        assert_eq!(
            entry["cost"]["context_over_200k"],
            json!({"input": 1.0, "output": 4.0})
        );

        let approximate = Config {
            opencode_approximate_long_context: Some(true),
            ..Config::default()
        };
        let entry = V1::model_entry(&model, &approximate);
        assert_eq!(
            entry["cost"]["context_over_200k"],
            json!({"input": 1.0, "output": 4.0}),
            "an exact 200k tier still wins"
        );
        model.tiers = vec![tier(128_000, 0.1), tier(272_000, 2.0), tier(512_000, 3.0)];
        let entry = V1::model_entry(&model, &approximate);
        assert_eq!(
            entry["cost"]["context_over_200k"],
            json!({"input": 2.0, "output": 8.0}),
            "the lowest tier above 200k stands in"
        );
    }

    #[test]
    fn opting_out_removes_only_our_approximation() {
        let fx = Fixture::new();
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
        let harness = fx.harness::<V1>();
        let install = |config: &Config| {
            fx.apply(&fx.install(&harness, &models, config, None))
                .unwrap();
        };
        let config_path = fx.paths.opencode_config_dir.join("opencode.jsonc");
        let model = |value: &Value, id: &str| value["provider"]["litellm"]["models"][id].clone();

        install(&Config {
            opencode_approximate_long_context: Some(true),
            ..Config::default()
        });
        assert_eq!(
            model(&read(&config_path), "gpt-6-luna")["cost"]["context_over_200k"],
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
        let value = read(&config_path);
        assert!(
            model(&value, "gpt-6-luna")["cost"]
                .get("context_over_200k")
                .is_none()
        );
        assert_eq!(
            model(&value, "gpt-6-astra")["cost"]["context_over_200k"],
            json!({"input": 0.3, "output": 0.9}),
            "a hand-written value is kept"
        );
    }
}
