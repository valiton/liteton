use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

/// One model as every harness writer sees it. Costs are USD per 1M tokens.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelSpec {
    pub id: String,
    pub display_name: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub pricing: Pricing,
    /// Rates that replace `pricing` once the prompt passes a threshold, lowest threshold first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<PriceTier>,
    pub tool_calling: bool,
    pub vision: bool,
    pub reasoning: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Pricing {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PriceTier {
    pub above_tokens: u64,
    #[serde(flatten)]
    pub pricing: Pricing,
}

impl Pricing {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl ModelSpec {
    /// A model we only know the id of.
    pub fn bare(id: &str) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name(id),
            context_window: None,
            max_output_tokens: None,
            pricing: Pricing::default(),
            tiers: Vec::new(),
            tool_calling: true,
            vision: false,
            reasoning: false,
        }
    }

    /// The first tier the prompt can reach.
    pub fn long_context(&self) -> Option<&PriceTier> {
        self.tiers.first()
    }
}

/// `azure/gpt-5.4-nano` shows as `gpt-5.4-nano`.
pub fn display_name(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).to_string()
}

const CHAT_MODES: &[&str] = &["chat", "responses", "completion"];

pub fn parse_model_group_info(body: &Value) -> Vec<ModelSpec> {
    parse_entries(body, |group| Some((group["model_group"].as_str()?, group)))
}

pub fn parse_model_info(body: &Value) -> Vec<ModelSpec> {
    parse_entries(body, |model| {
        Some((model["model_name"].as_str()?, &model["model_info"]))
    })
}

fn parse_entries<'a>(
    body: &'a Value,
    entry: impl Fn(&'a Value) -> Option<(&'a str, &'a Value)>,
) -> Vec<ModelSpec> {
    let Some(items) = body["data"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(entry)
        .filter(|(_, info)| {
            info["mode"]
                .as_str()
                .is_none_or(|mode| CHAT_MODES.contains(&mode))
        })
        .map(|(id, info)| spec_from_info(id, info))
        .collect()
}

fn spec_from_info(id: &str, info: &Value) -> ModelSpec {
    let params: Vec<&str> = info["supported_openai_params"]
        .as_array()
        .map(|p| p.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let flag = |name: &str| info[name].as_bool().unwrap_or(false);
    ModelSpec {
        id: id.to_string(),
        display_name: display_name(id),
        context_window: as_u64(&info["max_input_tokens"]),
        max_output_tokens: as_u64(&info["max_output_tokens"]),
        pricing: Pricing {
            input: per_million(&info[INPUT]),
            output: per_million(&info[OUTPUT]),
            cache_read: per_million(&info[CACHE_READ]),
            cache_write: per_million(&info[CACHE_WRITE]),
        },
        tiers: price_tiers(info),
        tool_calling: flag("supports_function_calling") || params.contains(&"tools"),
        vision: flag("supports_vision"),
        reasoning: flag("supports_reasoning") || params.contains(&"reasoning_effort"),
    }
}

const INPUT: &str = "input_cost_per_token";
const OUTPUT: &str = "output_cost_per_token";
const CACHE_READ: &str = "cache_read_input_token_cost";
const CACHE_WRITE: &str = "cache_creation_input_token_cost";

/// Reads every `<price>_above_<N>k_tokens` key. Variants such as `..._tokens_priority` or
/// `..._above_1hr` are other billing modes and don't match.
fn price_tiers(info: &Value) -> Vec<PriceTier> {
    let Some(fields) = info.as_object() else {
        return Vec::new();
    };
    let mut tiers: BTreeMap<u64, Pricing> = BTreeMap::new();
    for (key, value) in fields {
        let Some((price, above_tokens)) = tier_key(key) else {
            continue;
        };
        let Some(cost) = per_million(value) else {
            continue;
        };
        let pricing = tiers.entry(above_tokens).or_default();
        match price {
            INPUT => pricing.input = Some(cost),
            OUTPUT => pricing.output = Some(cost),
            CACHE_READ => pricing.cache_read = Some(cost),
            _ => pricing.cache_write = Some(cost),
        }
    }
    tiers
        .into_iter()
        .filter(|(_, pricing)| !pricing.is_empty())
        .map(|(above_tokens, pricing)| PriceTier {
            above_tokens,
            pricing,
        })
        .collect()
}

/// `input_cost_per_token_above_272k_tokens` → `(INPUT, 272_000)`.
fn tier_key(key: &str) -> Option<(&'static str, u64)> {
    let (price, rest) = key.split_once("_above_")?;
    let price = [INPUT, OUTPUT, CACHE_READ, CACHE_WRITE]
        .into_iter()
        .find(|known| *known == price)?;
    let thousands: u64 = rest.strip_suffix("k_tokens")?.parse().ok()?;
    Some((price, thousands * 1000))
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_f64().map(|f| f as u64))
        .filter(|n| *n > 0)
}

/// Rounded to 6 decimals so float noise like 0.049999999 doesn't end up in configs.
fn per_million(v: &Value) -> Option<f64> {
    v.as_f64()
        .map(|per_token| (per_token * 1e6 * 1e6).round() / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pricing(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Pricing {
        Pricing {
            input: Some(input),
            output: Some(output),
            cache_read: Some(cache_read),
            cache_write: Some(cache_write),
        }
    }

    #[test]
    fn parses_model_groups() {
        let body = json!({"data": [
            {
                "model_group": "azure/gpt-5.4-nano",
                "mode": "chat",
                "max_input_tokens": 400000,
                "max_output_tokens": 128000,
                "input_cost_per_token": 5e-8,
                "output_cost_per_token": 4e-7,
                "cache_read_input_token_cost": 5e-9,
                "supports_function_calling": true,
                "supports_vision": true,
                "supported_openai_params": ["tools", "reasoning_effort"]
            },
            {"model_group": "text-embedding-3-small", "mode": "embedding"},
            {"model_group": "plain"}
        ]});
        let specs = parse_model_group_info(&body);
        assert_eq!(specs.len(), 2);
        let nano = &specs[0];
        assert_eq!(nano.display_name, "gpt-5.4-nano");
        assert_eq!(nano.context_window, Some(400000));
        assert_eq!(
            nano.pricing,
            Pricing {
                input: Some(0.05),
                output: Some(0.4),
                cache_read: Some(0.005),
                cache_write: None,
            }
        );
        assert!(nano.tool_calling && nano.vision && nano.reasoning);
        assert!(!specs[1].reasoning);
        assert!(nano.long_context().is_none());
    }

    #[test]
    fn parses_model_info_with_long_context_tier() {
        let body = json!({"data": [
            {
                "model_name": "gpt-6-luna",
                "model_info": {
                    "mode": "chat",
                    "max_input_tokens": 922000,
                    "max_output_tokens": 128000,
                    "input_cost_per_token": 1e-7,
                    "input_cost_per_token_priority": null,
                    "output_cost_per_token": 5e-7,
                    "cache_read_input_token_cost": 1e-8,
                    "cache_creation_input_token_cost": 1.25e-7,
                    "input_cost_per_token_above_128k_tokens": null,
                    "input_cost_per_token_above_200k_tokens": null,
                    "input_cost_per_token_above_272k_tokens": 2e-7,
                    "input_cost_per_token_above_272k_tokens_priority": null,
                    "input_cost_per_token_above_512k_tokens": null,
                    "output_cost_per_token_above_272k_tokens": 7.5e-7,
                    "output_cost_per_character_above_128k_tokens": null,
                    "cache_read_input_token_cost_above_272k_tokens": 2e-8,
                    "cache_creation_input_token_cost_above_272k_tokens": 2.5e-7,
                    "supports_function_calling": true,
                    "supports_vision": true,
                    "supports_reasoning": true
                }
            },
            {
                "model_name": "claude-fable-5-1",
                "model_info": {
                    "mode": "chat",
                    "max_input_tokens": 1000000,
                    "max_output_tokens": 128000,
                    "input_cost_per_token": 0.00001,
                    "output_cost_per_token": 0.00005,
                    "cache_read_input_token_cost": 2.5e-7,
                    "cache_creation_input_token_cost": 0.0000125,
                    "cache_creation_input_token_cost_above_1hr": 0.00002,
                    "cache_creation_input_token_cost_above_200k_tokens": null,
                    "input_cost_per_token_above_200k_tokens": null,
                    "supports_function_calling": true,
                    "supports_reasoning": true
                }
            },
            {"model_name": "text-embedding-3-small", "model_info": {"mode": "embedding"}}
        ]});
        let specs = parse_model_info(&body);
        assert_eq!(specs.len(), 2);

        let luna = &specs[0];
        assert_eq!(luna.pricing, pricing(0.1, 0.5, 0.01, 0.125));
        assert_eq!(
            luna.tiers,
            [PriceTier {
                above_tokens: 272_000,
                pricing: pricing(0.2, 0.75, 0.02, 0.25),
            }]
        );

        let fable = &specs[1];
        assert_eq!(fable.pricing, pricing(10.0, 50.0, 0.25, 12.5));
        assert!(fable.long_context().is_none());
    }

    #[test]
    fn tiers_are_sorted_by_threshold() {
        let info = json!({
            "input_cost_per_token_above_512k_tokens": 4e-6,
            "input_cost_per_token_above_200k_tokens": 2e-6,
            "output_cost_per_token_above_200k_tokens": 8e-6,
        });
        let tiers = price_tiers(&info);
        assert_eq!(
            tiers.iter().map(|t| t.above_tokens).collect::<Vec<_>>(),
            [200_000, 512_000]
        );
        assert_eq!(tiers[0].pricing.output, Some(8.0));
        assert_eq!(tiers[1].pricing.output, None);
    }
}
