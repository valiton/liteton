use jsonc_parser::cst::{CstObject, CstObjectProp};

use super::format::Format;
use crate::jsonc;

pub fn sync(models: &CstObject, model_id: &str, efforts: &[String], format: Format) {
    let Some(entry) = models.object_value(model_id) else {
        return;
    };
    let prop = entry.get("variants");
    let existing = prop
        .as_ref()
        .and_then(CstObjectProp::value)
        .and_then(|v| jsonc::node_to_value(&v));
    let mut variants: Vec<_> = efforts
        .iter()
        .map(|level| (level.clone(), format.effort_variant(level)))
        .collect();
    for (id, value) in existing
        .as_ref()
        .map(|v| format.read_variants(v))
        .unwrap_or_default()
    {
        let ours = value == format.effort_variant(&id);
        if !ours && !efforts.contains(&id) {
            variants.push((id, value));
        }
    }
    let desired = (!variants.is_empty()).then(|| format.write_variants(variants));
    match (prop, desired) {
        (Some(prop), Some(desired)) if existing.as_ref() != Some(&desired) => {
            prop.set_value(jsonc::to_input(&desired));
        }
        (Some(prop), None) => prop.remove(),
        (None, Some(desired)) => {
            entry.append("variants", jsonc::to_input(&desired));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn synced(format: Format, variants: &Value, efforts: &[&str]) -> Value {
        let text = json!({"m": {"variants": variants}}).to_string();
        let root = jsonc::parse(&text).unwrap();
        let models = root.object_value().unwrap();
        let efforts: Vec<String> = efforts.iter().map(ToString::to_string).collect();
        sync(&models, "m", &efforts, format);
        jsonc::read_value(&jsonc::finish(&root, Some(&text))).unwrap()["m"]
            .get("variants")
            .cloned()
            .unwrap_or(Value::Null)
    }

    #[test]
    fn v1_drops_effort_variants_the_model_lost() {
        let variants = synced(
            Format::V1,
            &json!({
                "none": {"reasoningEffort": "none"},
                "fast": {"reasoningEffort": "low", "textVerbosity": "low"}
            }),
            &["high", "xhigh"],
        );
        assert_eq!(
            variants,
            json!({
                "high": {"reasoningEffort": "high"},
                "xhigh": {"reasoningEffort": "xhigh"},
                "fast": {"reasoningEffort": "low", "textVerbosity": "low"}
            })
        );
    }

    #[test]
    fn v2_keeps_user_variants_after_ours() {
        let variants = synced(
            Format::V2,
            &json!([
                {"id": "none", "settings": {"reasoningEffort": "none"}},
                {"id": "batch", "body": {"service_tier": "flex"}}
            ]),
            &["low"],
        );
        assert_eq!(
            variants,
            json!([
                {"id": "low", "settings": {"reasoningEffort": "low"}},
                {"id": "batch", "body": {"service_tier": "flex"}}
            ])
        );
    }

    #[test]
    fn removes_variants_when_nothing_is_left() {
        let variants = synced(
            Format::V2,
            &json!([{"id": "low", "settings": {"reasoningEffort": "low"}}]),
            &[],
        );
        assert_eq!(variants, Value::Null);
    }
}
