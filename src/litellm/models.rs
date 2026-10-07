use serde::Serialize;
use serde_json::Value;

/// One model as every harness writer sees it. Costs are USD per 1M tokens.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelSpec {
    pub id: String,
    pub display_name: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub input_cost: Option<f64>,
    pub output_cost: Option<f64>,
    pub cache_read_cost: Option<f64>,
    pub cache_write_cost: Option<f64>,
    pub tool_calling: bool,
    pub vision: bool,
    pub reasoning: bool,
}

impl ModelSpec {
    /// A model we only know the id of.
    pub fn bare(id: &str) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name(id),
            context_window: None,
            max_output_tokens: None,
            input_cost: None,
            output_cost: None,
            cache_read_cost: None,
            cache_write_cost: None,
            tool_calling: true,
            vision: false,
            reasoning: false,
        }
    }
}

/// `azure/gpt-5.4-nano` shows as `gpt-5.4-nano`.
pub fn display_name(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).to_string()
}

const CHAT_MODES: &[&str] = &["chat", "responses", "completion"];

pub fn parse_model_group_info(body: &Value) -> Vec<ModelSpec> {
    let Some(groups) = body["data"].as_array() else {
        return Vec::new();
    };
    groups
        .iter()
        .filter(|g| {
            g["mode"]
                .as_str()
                .is_none_or(|mode| CHAT_MODES.contains(&mode))
        })
        .filter_map(|g| {
            let id = g["model_group"].as_str()?;
            let params: Vec<&str> = g["supported_openai_params"]
                .as_array()
                .map(|p| p.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let flag = |name: &str| g[name].as_bool().unwrap_or(false);
            Some(ModelSpec {
                id: id.to_string(),
                display_name: display_name(id),
                context_window: as_u64(&g["max_input_tokens"]),
                max_output_tokens: as_u64(&g["max_output_tokens"]),
                input_cost: per_million(&g["input_cost_per_token"]),
                output_cost: per_million(&g["output_cost_per_token"]),
                cache_read_cost: per_million(&g["cache_read_input_token_cost"]),
                cache_write_cost: per_million(&g["cache_creation_input_token_cost"]),
                tool_calling: flag("supports_function_calling") || params.contains(&"tools"),
                vision: flag("supports_vision"),
                reasoning: flag("supports_reasoning") || params.contains(&"reasoning_effort"),
            })
        })
        .collect()
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
        assert_eq!(nano.input_cost, Some(0.05));
        assert_eq!(nano.output_cost, Some(0.4));
        assert_eq!(nano.cache_read_cost, Some(0.005));
        assert_eq!(nano.cache_write_cost, None);
        assert!(nano.tool_calling && nano.vision && nano.reasoning);
        assert!(!specs[1].reasoning);
    }
}
