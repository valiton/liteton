//! In-place JSONC edits that keep comments, ordering and formatting of everything we don't touch.

use anyhow::{Result, anyhow};
use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode};
use jsonc_parser::{CollectOptions, ParseOptions, ast};
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

/// Puts accepted multi-line objects at `path` (`*` matches any key or index) on one line.
pub fn inline_objects(
    text: &str,
    path: &[&str],
    inline: impl Fn(&str, &Value) -> bool,
) -> Result<String> {
    let parsed =
        jsonc_parser::parse_to_ast(text, &CollectOptions::default(), &ParseOptions::default())
            .map_err(|e| anyhow!("{e}"))?;
    let mut edits = Vec::new();
    if let Some(root) = &parsed.value {
        collect_inline(text, root, path, "", &inline, &mut edits);
    }
    let mut out = text.to_string();
    for (range, compact) in edits.into_iter().rev() {
        out.replace_range(range, &compact);
    }
    Ok(out)
}

fn collect_inline(
    text: &str,
    node: &ast::Value,
    path: &[&str],
    key: &str,
    inline: &dyn Fn(&str, &Value) -> bool,
    edits: &mut Vec<(std::ops::Range<usize>, String)>,
) {
    let Some((segment, rest)) = path.split_first() else {
        if let ast::Value::Object(obj) = node {
            let range = obj.range.start..obj.range.end;
            let raw = &text[range.clone()];
            if raw.contains('\n')
                && !raw.contains("//")
                && !raw.contains("/*")
                && let Ok(value) = read_value(raw)
                && inline(key, &value)
            {
                edits.push((range, one_line(&value)));
            }
        }
        return;
    };
    let wanted = |name: &str| *segment == "*" || *segment == name;
    match node {
        ast::Value::Object(obj) => {
            for prop in &obj.properties {
                let name = prop.name.as_str();
                if wanted(name) {
                    collect_inline(text, &prop.value, rest, name, inline, edits);
                }
            }
        }
        ast::Value::Array(array) => {
            for (index, element) in array.elements.iter().enumerate() {
                let name = index.to_string();
                if wanted(&name) {
                    collect_inline(text, element, rest, &name, inline, edits);
                }
            }
        }
        _ => {}
    }
}

fn one_line(value: &Value) -> String {
    match value {
        Value::Object(map) if !map.is_empty() => {
            let props: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", Value::String(k.clone()), one_line(v)))
                .collect();
            format!("{{ {} }}", props.join(", "))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(one_line).collect();
            format!("[{}]", items.join(", "))
        }
        other => other.to_string(),
    }
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

    #[test]
    fn inlines_only_accepted_objects_at_the_path() {
        let text = r#"{
  // keep
  "v": [
    {
      "id": "low",
      "settings": {
        "reasoningEffort": "low"
      }
    },
    {
      "id": "mine",
      "body": {"a": 1}
    },
    {
      // a note
      "id": "high"
    },
    { "id": "medium" }
  ],
  "other": {
    "id": "low"
  }
}
"#;
        let out = inline_objects(text, &["v", "*"], |_, v| v["id"] != "mine").unwrap();
        assert!(out.contains(
            "    { \"id\": \"low\", \"settings\": { \"reasoningEffort\": \"low\" } },\n"
        ));
        assert!(
            out.contains("    {\n      \"id\": \"mine\""),
            "rejected: {out}"
        );
        assert!(out.contains("// a note"), "comments stay: {out}");
        assert!(out.contains("    { \"id\": \"medium\" }\n"));
        assert!(
            out.contains("\"other\": {\n    \"id\": \"low\"\n  }"),
            "outside the path: {out}"
        );
        assert!(out.contains("// keep"));
        assert_eq!(read_value(&out).unwrap(), read_value(text).unwrap());
        let all = inline_objects(&out, &["v", "*"], |_, _| true).unwrap();
        assert!(
            all.contains("{ \"id\": \"mine\", \"body\": { \"a\": 1 } }"),
            "{all}"
        );
    }
}
