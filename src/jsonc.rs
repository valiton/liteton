//! In-place JSONC edits that keep comments, ordering and formatting of everything we don't touch.

use anyhow::{Result, anyhow};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode};
use serde_json::{Map, Value};

pub fn parse(text: &str) -> Result<CstRootNode> {
    CstRootNode::parse(text, &ParseOptions::default()).map_err(|e| anyhow!("{e}"))
}

pub fn read_value(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    jsonc_parser::parse_to_serde_value(text, &ParseOptions::default()).map_err(|e| anyhow!("{e}"))
}

pub fn to_input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(to_input).collect()),
        Value::Object(map) => {
            CstInputValue::Object(map.iter().map(|(k, v)| (k.clone(), to_input(v))).collect())
        }
    }
}

/// Recursively writes `value` into `obj`: nested objects are merged key by key, everything
/// else is replaced, and keys only present in `obj` are left alone.
pub fn merge_object(obj: &CstObject, value: &Map<String, Value>) {
    for (key, new) in value {
        match obj.get(key) {
            Some(prop) => {
                let existing = prop.value().and_then(|v| v.as_object());
                match (existing, new) {
                    (Some(existing), Value::Object(new_map)) => merge_object(&existing, new_map),
                    _ => {
                        if prop.value().and_then(|v| node_to_value(&v)).as_ref() != Some(new) {
                            prop.set_value(to_input(new));
                        }
                    }
                }
            }
            None => {
                obj.append(key, to_input(new));
            }
        }
    }
}

pub fn set_if_missing(obj: &CstObject, key: &str, value: &Value) {
    if obj.get(key).is_none() {
        obj.append(key, to_input(value));
    }
}

pub fn node_to_value(node: &CstNode) -> Option<Value> {
    node.to_serde_value()
}

pub fn string_prop(obj: &CstObject, key: &str) -> Option<String> {
    obj.get(key)?.value()?.as_string_lit()?.decoded_value().ok()
}

/// Serializes the tree, ending new files with a newline and keeping the original file ending otherwise.
pub fn finish(root: &CstRootNode, before: Option<&str>) -> String {
    let mut text = root.to_string();
    if before.is_none_or(|b| b.ends_with('\n')) && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_keeps_comments_and_foreign_keys() {
        let text =
            "{\n  // keep me\n  \"a\": 1,\n  \"nested\": { \"user\": true, \"ours\": 1 }\n}\n";
        let root = parse(text).unwrap();
        let obj = root.object_value().unwrap();
        merge_object(
            &obj,
            json!({"nested": {"ours": 2, "added": "x"}, "b": [1, 2]})
                .as_object()
                .unwrap(),
        );
        let out = finish(&root, Some(text));
        assert!(out.contains("// keep me"));
        assert!(out.contains("\"user\": true"));
        let value = read_value(&out).unwrap();
        assert_eq!(
            value,
            json!({"a": 1, "nested": {"user": true, "ours": 2, "added": "x"}, "b": [1, 2]})
        );
    }

    #[test]
    fn unchanged_merge_is_a_no_op() {
        let text = "{\n  \"a\": {\"b\": [1,2], \"c\": 0.05}\n}\n";
        let root = parse(text).unwrap();
        merge_object(
            &root.object_value().unwrap(),
            json!({"a": {"b": [1, 2], "c": 0.05}}).as_object().unwrap(),
        );
        assert_eq!(finish(&root, Some(text)), text);
    }
}
