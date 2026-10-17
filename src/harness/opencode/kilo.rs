use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Result;
use jsonc_parser::cst::CstObject;
use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::harness::vscode::VsCode;
use crate::harness::{Change, HarnessPaths, PROVIDER_ID, read_optional};
use crate::jsonc;
use crate::litellm::ModelSpec;

pub const CONFIG: &str = "kilo";
const VSCODE: &str = "vscode";

type Setting = (&'static [&'static str], Value);

pub fn config_settings(config: &Config, models: &[ModelSpec]) -> Vec<Setting> {
    let mut settings: Vec<Setting> = vec![
        (&["enabled_providers"], json!([PROVIDER_ID])),
        (&["share"], json!("disabled")),
        (&["remote_control"], json!(false)),
        (&["experimental", "openTelemetry"], json!(false)),
    ];
    if let Some(id) = chosen(config.kilo_model.as_deref(), models) {
        settings.push((&["model"], json!(format!("{PROVIDER_ID}/{id}"))));
    }
    if let Some(id) = chosen(config.kilo_small_model.as_deref(), models) {
        settings.push((&["small_model"], json!(format!("{PROVIDER_ID}/{id}"))));
    }
    settings
}

/// Kilo's autocomplete only talks to the Kilo Gateway, Mistral or Inception, never to LiteLLM.
fn vscode_settings(config: &Config, models: &[ModelSpec]) -> Vec<Setting> {
    let mut settings: Vec<Setting> = vec![(
        &["kilo-code.new.autocomplete.enableAutoTrigger"],
        json!(false),
    )];
    if let Some(id) = chosen(config.kilo_model.as_deref(), models) {
        settings.push((&["kilo-code.new.model.providerID"], json!(PROVIDER_ID)));
        settings.push((&["kilo-code.new.model.modelID"], json!(id)));
    }
    settings
}

fn chosen<'a>(id: Option<&'a str>, models: &[ModelSpec]) -> Option<&'a str> {
    id.filter(|id| models.iter().any(|m| m.id == *id))
}

/// Writes each setting and remembers, once, the value it replaced (`null` when there was none).
pub fn apply(
    obj: &CstObject,
    settings: &[Setting],
    file: &str,
    previous: &mut BTreeMap<String, Value>,
) {
    for (path, value) in settings {
        previous
            .entry(format!("{file}:{}", path.join("/")))
            .or_insert_with(|| get(obj, path).unwrap_or(Value::Null));
        set(obj, path, value);
    }
}

pub fn restore(obj: &CstObject, file: &str, previous: &BTreeMap<String, Value>) {
    let prefix = format!("{file}:");
    for (key, value) in previous {
        let Some(path) = key.strip_prefix(&prefix) else {
            continue;
        };
        let path: Vec<&str> = path.split('/').collect();
        if value.is_null() {
            remove(obj, &path);
        } else {
            set(obj, &path, value);
        }
    }
}

fn set(obj: &CstObject, path: &[&str], value: &Value) {
    let nested = path.iter().rev().fold(value.clone(), |inner, key| {
        Value::Object(Map::from_iter([(key.to_string(), inner)]))
    });
    jsonc::merge_object(obj, nested.as_object().unwrap());
}

fn get(obj: &CstObject, path: &[&str]) -> Option<Value> {
    let (last, parents) = path.split_last()?;
    let mut current = obj.clone();
    for key in parents {
        current = current.object_value(key)?;
    }
    current
        .get(last)?
        .value()
        .and_then(|v| jsonc::node_to_value(&v))
}

/// Also removes parent objects the removal leaves empty.
fn remove(obj: &CstObject, path: &[&str]) {
    match path {
        [] => {}
        [key] => {
            if let Some(prop) = obj.get(key) {
                prop.remove();
            }
        }
        [parent, rest @ ..] => {
            if let Some(child) = obj.object_value(parent) {
                remove(&child, rest);
                if child.properties().is_empty()
                    && let Some(prop) = obj.get(parent)
                {
                    prop.remove();
                }
            }
        }
    }
}

fn vscode_extension_installed(extensions_dir: &Path) -> bool {
    fs::read_dir(extensions_dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("kilocode.kilo-code-")
        })
}

/// Every VS Code profile with its own settings; profiles sharing the Default's are covered by it.
pub fn plan_vscode_install(
    paths: &HarnessPaths,
    config: &Config,
    models: &[ModelSpec],
    previous: &mut BTreeMap<String, Value>,
) -> Result<Vec<Change>> {
    if !vscode_extension_installed(&paths.vscode_extensions_dir) {
        return Ok(Vec::new());
    }
    let settings = vscode_settings(config, models);
    let mut changes = Vec::new();
    for (profile, path) in VsCode::new(paths).settings_files() {
        let before = read_optional(&path)?;
        let root = jsonc::parse(before.as_deref().unwrap_or(""))?;
        let label = vscode_label(&profile);
        apply(&root.object_value_or_set(), &settings, &label, previous);
        let after = jsonc::finish(&root, before.as_deref());
        changes.extend(Change::file(path, before, after));
    }
    Ok(changes)
}

pub fn plan_vscode_uninstall(
    paths: &HarnessPaths,
    previous: &BTreeMap<String, Value>,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for (profile, path) in VsCode::new(paths).settings_files() {
        let Some(before) = read_optional(&path)? else {
            continue;
        };
        let root = jsonc::parse(&before)?;
        if let Some(obj) = root.object_value() {
            restore(&obj, &vscode_label(&profile), previous);
        }
        let after = jsonc::finish(&root, Some(&before));
        changes.extend(Change::file(path, Some(before), after));
    }
    Ok(changes)
}

fn vscode_label(profile: &str) -> String {
    if profile.is_empty() {
        VSCODE.to_string()
    } else {
        format!("{VSCODE}/{profile}")
    }
}
