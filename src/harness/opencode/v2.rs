use jsonc_parser::cst::CstObject;
use serde_json::{Map, Value, json};

use super::format::{input_modalities, limit};
use crate::jsonc;
use crate::litellm::{ModelSpec, Pricing};

const PACKAGE: &str = "aisdk:@ai-sdk/openai-compatible";

pub fn set_connection(provider: &CstObject, base_url: &str) {
    jsonc::set_if_missing(provider, "package", &json!(PACKAGE));
    jsonc::merge_object(
        provider,
        json!({"settings": {"baseURL": base_url}})
            .as_object()
            .unwrap(),
    );
}

pub fn model_entry(model: &ModelSpec) -> Value {
    let mut entry = Map::new();
    entry.insert("name".into(), json!(model.display_name));
    entry.insert(
        "capabilities".into(),
        json!({"tools": model.tool_calling, "input": input_modalities(model), "output": ["text"]}),
    );
    if let Some(limit) = limit(model) {
        entry.insert("limit".into(), limit);
    }
    if let Some(cost) = tiered_cost(model) {
        entry.insert("cost".into(), cost);
    }
    Value::Object(entry)
}

/// Past `size` prompt tokens (input plus cache), the highest matching tier prices the whole
/// request, like LiteLLM's `above_<N>k_tokens`.
fn tiered_cost(model: &ModelSpec) -> Option<Value> {
    let base = cost(&model.pricing)?;
    if model.tiers.is_empty() {
        return Some(Value::Object(base));
    }
    let mut costs = vec![Value::Object(base)];
    for tier in &model.tiers {
        let mut cost_tier = Map::new();
        cost_tier.insert(
            "tier".into(),
            json!({"type": "context", "size": tier.above_tokens}),
        );
        cost_tier.extend(cost(&tier.pricing.or(&model.pricing))?);
        costs.push(Value::Object(cost_tier));
    }
    Some(Value::Array(costs))
}

fn cost(pricing: &Pricing) -> Option<Map<String, Value>> {
    let (Some(input), Some(output)) = (pricing.input, pricing.output) else {
        return None;
    };
    let mut cost = Map::new();
    cost.insert("input".into(), json!(input));
    cost.insert("output".into(), json!(output));
    let mut cache = Map::new();
    if let Some(read) = pricing.cache_read {
        cache.insert("read".into(), json!(read));
    }
    if let Some(write) = pricing.cache_write {
        cache.insert("write".into(), json!(write));
    }
    if !cache.is_empty() {
        cost.insert("cache".into(), Value::Object(cache));
    }
    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::tests::spec;
    use crate::litellm::PriceTier;

    #[test]
    fn writes_every_price_tier() {
        let mut haiku = spec("claude-haiku-5-5", false);
        haiku.pricing.cache_read = Some(0.01);
        haiku.tiers = vec![
            PriceTier {
                above_tokens: 100_000,
                pricing: Pricing {
                    input: Some(0.5),
                    output: Some(2.5),
                    cache_read: Some(0.05),
                    ..Default::default()
                },
            },
            PriceTier {
                above_tokens: 272_000,
                pricing: Pricing {
                    input: Some(1.0),
                    ..Default::default()
                },
            },
        ];
        let entry = model_entry(&haiku);
        assert_eq!(
            entry["cost"],
            json!([
                {"input": 0.05, "output": 0.4, "cache": {"read": 0.01}},
                {"tier": {"type": "context", "size": 100000}, "input": 0.5, "output": 2.5, "cache": {"read": 0.05}},
                {"tier": {"type": "context", "size": 272000}, "input": 1.0, "output": 0.4, "cache": {"read": 0.01}}
            ])
        );
        haiku.tiers.clear();
        assert_eq!(
            model_entry(&haiku)["cost"],
            json!({"input": 0.05, "output": 0.4, "cache": {"read": 0.01}})
        );
    }
}
