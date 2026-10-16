use jsonc_parser::cst::CstObject;

use super::Version;
use crate::jsonc;

/// Writes one variant per effort level and drops effort variants for levels the model no
/// longer has. Variants the user wrote themselves are kept.
pub fn sync<V: Version>(models: &CstObject, model_id: &str, efforts: &[String]) {
    let Some(entry) = models.object_value(model_id) else {
        return;
    };
    let prop = entry.get("variants");
    let existing = prop
        .as_ref()
        .and_then(|p| p.value())
        .and_then(|v| jsonc::node_to_value(&v));
    let mut variants: Vec<_> = efforts
        .iter()
        .map(|level| (level.clone(), V::effort_variant(level)))
        .collect();
    for (id, value) in existing.as_ref().map(V::read_variants).unwrap_or_default() {
        let ours = value == V::effort_variant(&id);
        if !ours && !efforts.contains(&id) {
            variants.push((id, value));
        }
    }
    let desired = (!variants.is_empty()).then(|| V::write_variants(variants));
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

    use super::super::{V1, V2};
    use super::*;

    fn synced<V: Version>(variants: Value, efforts: &[&str]) -> Value {
        let text = json!({"m": {"variants": variants}}).to_string();
        let root = jsonc::parse(&text).unwrap();
        let models = root.object_value().unwrap();
        let efforts: Vec<String> = efforts.iter().map(|l| l.to_string()).collect();
        sync::<V>(&models, "m", &efforts);
        jsonc::read_value(&jsonc::finish(&root, Some(&text))).unwrap()["m"]
            .get("variants")
            .cloned()
            .unwrap_or(Value::Null)
    }

    #[test]
    fn v1_drops_effort_variants_the_model_lost() {
        let variants = synced::<V1>(
            json!({
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
        let variants = synced::<V2>(
            json!([
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
        let variants = synced::<V2>(
            json!([{"id": "low", "settings": {"reasoningEffort": "low"}}]),
            &[],
        );
        assert_eq!(variants, Value::Null);
    }
}
