use jsonc_parser::cst::CstObject;
use serde_json::{Value, json};

use super::{v1, v2};
use crate::config::Config;
use crate::litellm::ModelSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    V1,
    V2,
}

impl Format {
    pub fn root_key(self) -> &'static str {
        match self {
            Self::V1 => "provider",
            Self::V2 => "providers",
        }
    }

    pub fn set_connection(self, provider: &CstObject, base_url: &str) {
        match self {
            Self::V1 => v1::set_connection(provider, base_url),
            Self::V2 => v2::set_connection(provider, base_url),
        }
    }

    pub fn model_entry(self, model: &ModelSpec, config: &Config) -> Value {
        match self {
            Self::V1 => v1::model_entry(model, config),
            Self::V2 => v2::model_entry(model),
        }
    }

    pub fn effort_variant(self, level: &str) -> Value {
        match self {
            Self::V1 => json!({"reasoningEffort": level}),
            Self::V2 => json!({"id": level, "settings": {"reasoningEffort": level}}),
        }
    }

    pub fn variant_id<'a>(self, key: &'a str, variant: &'a Value) -> &'a str {
        match self {
            Self::V1 => key,
            Self::V2 => variant["id"].as_str().unwrap_or_default(),
        }
    }

    pub fn read_variants(self, variants: &Value) -> Vec<(String, Value)> {
        match (self, variants) {
            (Self::V1, Value::Object(map)) => {
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            }
            (Self::V2, Value::Array(list)) => list
                .iter()
                .map(|v| (self.variant_id("", v).to_string(), v.clone()))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn write_variants(self, variants: Vec<(String, Value)>) -> Value {
        match self {
            Self::V1 => Value::Object(variants.into_iter().collect()),
            Self::V2 => Value::Array(variants.into_iter().map(|(_, v)| v).collect()),
        }
    }

    pub fn tidy_model(self, models: &CstObject, model: &ModelSpec, config: &Config) {
        if self == Self::V1 && config.opencode_approximate_long_context != Some(true) {
            v1::drop_approximation(models, model);
        }
    }
}

pub fn limit(model: &ModelSpec) -> Option<Value> {
    let (context, output) = (model.context_window?, model.max_output_tokens?);
    Some(json!({"context": context, "output": output}))
}

pub fn input_modalities(model: &ModelSpec) -> Value {
    if model.vision {
        json!(["text", "image"])
    } else {
        json!(["text"])
    }
}
