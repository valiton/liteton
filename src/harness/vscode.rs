use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use jsonc_parser::cst::{CstArray, CstObject, CstRootNode};
use serde_json::{Value, json};

use super::{
    Change, DbValue, Harness, HarnessId, HarnessPaths, InstallCtx, PROVIDER_ID, Plan,
    binary_on_path, deselected_ids, read_optional,
};
use crate::config::{Config, HarnessRecord, ProfileRecord, key_fingerprint};
use crate::jsonc;
use crate::litellm::ModelSpec;
use crate::vscdb::{ElectronApp, StateDb, secret_key};

const VENDOR: &str = "customendpoint";
const DEFAULT_SECRET: &str = "chat.lm.secret.liteton";
const MODELS_FILE: &str = "chatLanguageModels.json";

pub struct VsCode {
    user_dir: PathBuf,
}

/// A VSCode profile that reads its own `chatLanguageModels.json`.
#[derive(Debug, Clone, PartialEq)]
struct Profile {
    /// Folder relative to the User folder ("" = Default); keys the install record.
    key: String,
    name: String,
    dir: PathBuf,
}

impl Profile {
    fn models_path(&self) -> PathBuf {
        self.dir.join(MODELS_FILE)
    }
}

#[derive(Debug, Default)]
struct Profiles {
    /// Default first.
    own: Vec<Profile>,
    /// Names of profiles that use the Default profile's models.
    shared: Vec<String>,
}

impl VsCode {
    pub fn new(paths: &HarnessPaths) -> Self {
        Self {
            user_dir: paths.vscode_user_dir.clone(),
        }
    }

    fn state_db(&self) -> PathBuf {
        self.user_dir.join("globalStorage/state.vscdb")
    }

    /// VSCode reads `chatLanguageModels.json` from each profile's folder, except for profiles
    /// with `useDefaultFlags.languageModels`, which read the Default profile's. The profile list
    /// lives in `globalStorage/storage.json`; if it can't be read, folders with the file are used.
    fn profiles(&self) -> Profiles {
        let mut profiles = Profiles {
            own: vec![Profile {
                key: String::new(),
                name: "Default".into(),
                dir: self.user_dir.clone(),
            }],
            shared: Vec::new(),
        };
        let mut shared_dirs = Vec::new();
        let stored = read_optional(&self.user_dir.join("globalStorage/storage.json"))
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v["userDataProfiles"].as_array().cloned());
        let mut complete = stored.is_some();
        for profile in stored.iter().flatten() {
            let Some(dir) = self.profile_dir(&profile["location"]) else {
                complete = false;
                continue;
            };
            let name = profile["name"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| self.key_for(&dir));
            if profile["useDefaultFlags"]["languageModels"] == json!(true) {
                profiles.shared.push(name);
                shared_dirs.push(dir);
            } else if !profiles.own.iter().any(|p| p.dir == dir) {
                profiles.own.push(Profile {
                    key: self.key_for(&dir),
                    name,
                    dir,
                });
            }
        }
        if complete {
            return profiles;
        }
        for dir in self.profile_dirs_with_models() {
            if !profiles.own.iter().any(|p| p.dir == dir) && !shared_dirs.contains(&dir) {
                profiles.own.push(Profile {
                    key: self.key_for(&dir),
                    name: dir
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    dir,
                });
            }
        }
        profiles
    }

    /// `location` is relative to `User/profiles`; older VSCode versions stored a URI.
    fn profile_dir(&self, location: &Value) -> Option<PathBuf> {
        let path = match location {
            Value::String(s) => match s.strip_prefix("file://") {
                Some(uri_path) => PathBuf::from(percent_decode(uri_path)),
                None => PathBuf::from(s),
            },
            Value::Object(uri) => PathBuf::from(uri.get("path")?.as_str()?),
            _ => return None,
        };
        Some(if path.is_absolute() {
            path
        } else {
            self.user_dir.join("profiles").join(path)
        })
    }

    fn key_for(&self, dir: &Path) -> String {
        dir.strip_prefix(&self.user_dir)
            .unwrap_or(dir)
            .to_string_lossy()
            .into_owned()
    }

    fn dir_for(&self, key: &str) -> PathBuf {
        if key.is_empty() {
            self.user_dir.clone()
        } else {
            self.user_dir.join(key)
        }
    }

    /// Profile folders (`profiles/<id>` and built-in `profiles/builtin/<name>`) that already
    /// have a `chatLanguageModels.json`.
    fn profile_dirs_with_models(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![(self.user_dir.join("profiles"), 0)];
        while let Some((dir, depth)) = pending.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for path in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
                if path.join(MODELS_FILE).is_file() {
                    found.push(path.clone());
                }
                if depth == 0 {
                    pending.push((path, 1));
                }
            }
        }
        found.sort();
        found
    }
}

impl Harness for VsCode {
    fn id(&self) -> HarnessId {
        HarnessId::Vscode
    }

    fn detect(&self) -> bool {
        self.user_dir.exists() || binary_on_path("code")
    }

    fn is_installed(&self) -> bool {
        self.profiles()
            .own
            .iter()
            .any(|p| has_entry(&p.models_path()))
    }

    fn plan_install(&self, ctx: &InstallCtx, record: Option<&HarnessRecord>) -> Result<Plan> {
        let db_path = self.state_db();
        if !db_path.exists() {
            bail!(
                "{} not found; open VSCode once before installing",
                db_path.display()
            );
        }
        let profiles = self.profiles();
        let previous = record
            .map(HarnessRecord::profile_records)
            .unwrap_or_default();

        let mut notes: Vec<String> = profiles_note(&profiles).into_iter().collect();
        let mut files = Vec::new();
        let mut first_error = None;
        for profile in &profiles.own {
            let path = profile.models_path();
            let read = read_optional(&path).and_then(|before| {
                let root = jsonc::parse(before.as_deref().unwrap_or(""))
                    .with_context(|| format!("parsing {}", path.display()))?;
                Ok((before, root))
            });
            match read {
                Ok((before, root)) => files.push((profile, path, before, root)),
                Err(e) => {
                    notes.push(format!("Skipped the {} profile: {e:#}", profile.name));
                    first_error.get_or_insert(e);
                }
            }
        }
        if files.is_empty()
            && let Some(e) = first_error
        {
            return Err(e);
        }
        // New entries reuse an existing secret (Default's first), so all profiles share one key.
        let new_secret = files
            .iter()
            .find_map(|(.., root)| entry_secret(root))
            .unwrap_or_else(|| DEFAULT_SECRET.to_string());

        let mut changes = Vec::new();
        let mut secrets = BTreeSet::new();
        let mut next_profiles = previous.clone();
        next_profiles.retain(|key, _| self.dir_for(key).exists());
        for (profile, path, before, root) in files {
            let last = previous.get(&profile.key).cloned().unwrap_or_default();
            let (secret, next) = merge_profile(&root, ctx, &new_secret, &last)?;
            secrets.insert(secret);
            next_profiles.insert(profile.key.clone(), next);
            let after = jsonc::finish(&root, before.as_deref());
            if before.as_deref() != Some(after.as_str()) {
                changes.push(Change::File {
                    path,
                    before,
                    after,
                    private: false,
                    summary: vec![],
                });
            }
        }

        let db = StateDb::open_readonly(&db_path)?;
        let fingerprint = key_fingerprint(&ctx.creds.api_key);
        let key_changed = record.and_then(|r| r.key_fingerprint.as_ref()) != Some(&fingerprint);
        let mut created_secrets: Vec<String> = record
            .map(|r| r.all_created_secrets().cloned().collect())
            .unwrap_or_default();
        for secret in &secrets {
            let existed = db.get(&secret_key(secret))?.is_some();
            if !existed && !created_secrets.contains(secret) {
                created_secrets.push(secret.clone());
            }
            if !existed || key_changed {
                changes.push(Change::DbItem {
                    app: ElectronApp::VSCode,
                    db: db_path.clone(),
                    key: secret_key(secret),
                    label: format!("VSCode secret storage: {secret}"),
                    summary: vec![format!(
                        "{} the API key (encrypted with \"Code Safe Storage\")",
                        if existed { "replace" } else { "store" }
                    )],
                    value: DbValue::Secret(ctx.creds.api_key.clone()),
                });
            }
        }

        let mut next = record.cloned().unwrap_or_default();
        next.added_models.clear();
        next.created_provider = false;
        next.created_secret = None;
        next.created_secrets = created_secrets;
        next.profiles = next_profiles;
        next.key_fingerprint = Some(fingerprint);
        Ok(Plan {
            harness: self.id(),
            changes,
            record: Some(next),
            notes,
        })
    }

    fn plan_uninstall(&self, record: &HarnessRecord) -> Result<Plan> {
        let mut changes = Vec::new();
        for (key, profile) in record.profile_records() {
            let path = self.dir_for(&key).join(MODELS_FILE);
            if let Some(change) = remove_from(&path, &profile)? {
                changes.push(change);
            }
        }
        for secret in record.all_created_secrets() {
            changes.push(Change::DbItem {
                app: ElectronApp::VSCode,
                db: self.state_db(),
                key: secret_key(secret),
                label: format!("VSCode secret storage: {secret}"),
                summary: vec!["delete the API key".into()],
                value: DbValue::Delete,
            });
        }
        Ok(Plan {
            harness: self.id(),
            changes,
            record: None,
            notes: vec![],
        })
    }
}

/// Adds or updates the litellm entry in one profile's file. Returns the secret id the entry
/// points to and the profile's next record.
fn merge_profile(
    root: &CstRootNode,
    ctx: &InstallCtx,
    new_secret: &str,
    last: &ProfileRecord,
) -> Result<(String, ProfileRecord)> {
    let entries = root.array_value_or_set();
    let (entry, created_provider) = match find_entry(&entries) {
        Some(entry) => (entry, false),
        None => {
            let node = entries.append(jsonc::to_input(&json!({
                "name": PROVIDER_ID,
                "vendor": VENDOR,
                "apiKey": format!("${{input:{new_secret}}}"),
                "apiType": "chat-completions",
                "models": [],
            })));
            (
                node.as_object()
                    .ok_or_else(|| anyhow!("could not create the {PROVIDER_ID} entry"))?,
                true,
            )
        }
    };
    let secret = match jsonc::string_prop(&entry, "apiKey")
        .as_deref()
        .and_then(secret_ref)
    {
        Some(existing) => existing.to_string(),
        None => {
            jsonc::merge_object(
                &entry,
                json!({"apiKey": format!("${{input:{new_secret}}}")})
                    .as_object()
                    .unwrap(),
            );
            new_secret.to_string()
        }
    };
    jsonc::set_if_missing(&entry, "apiType", &json!("chat-completions"));

    let models = entry
        .array_value_or_create("models")
        .ok_or_else(|| anyhow!("\"models\" of the {PROVIDER_ID} entry is not an array"))?;
    let mut newly_added = Vec::new();
    for model in ctx.models {
        let value = model_entry(model, &ctx.creds.base_url, ctx.config);
        match find_model(&models, &model.id) {
            Some(existing) => {
                let mut value = value;
                if existing.get("name").is_some() {
                    value.as_object_mut().unwrap().remove("name");
                }
                jsonc::merge_object(&existing, value.as_object().unwrap());
            }
            None => {
                models.append(jsonc::to_input(&value));
                newly_added.push(model.id.clone());
            }
        }
    }
    let removed = deselected_ids(&last.added_models, ctx.models);
    for id in &removed {
        if let Some(existing) = find_model(&models, id) {
            existing.remove();
        }
    }

    let mut next = last.clone();
    next.added_models.retain(|id| !removed.contains(&id));
    for id in newly_added {
        if !next.added_models.contains(&id) {
            next.added_models.push(id);
        }
    }
    next.created_provider |= created_provider;
    Ok((secret, next))
}

/// Removes the models liteton added, and the entry itself when liteton created it and it's empty.
fn remove_from(path: &Path, profile: &ProfileRecord) -> Result<Option<Change>> {
    let Some(before) = read_optional(path)? else {
        return Ok(None);
    };
    let root = jsonc::parse(&before)?;
    if let Some(entries) = root.array_value()
        && let Some(entry) = find_entry(&entries)
    {
        if let Some(models) = entry.array_value("models") {
            for id in &profile.added_models {
                if let Some(model) = find_model(&models, id) {
                    model.remove();
                }
            }
        }
        let empty = entry
            .array_value("models")
            .is_none_or(|m| m.elements().is_empty());
        if profile.created_provider && empty {
            entry.remove();
        }
    }
    let after = jsonc::finish(&root, Some(&before));
    Ok((after != before).then(|| Change::File {
        path: path.to_path_buf(),
        before: Some(before),
        after,
        private: false,
        summary: vec![],
    }))
}

/// "Profiles: Default, DSA. Agents uses the Default profile's models." Only when there are
/// profiles besides Default.
fn profiles_note(profiles: &Profiles) -> Option<String> {
    if profiles.own.len() < 2 && profiles.shared.is_empty() {
        return None;
    }
    let names: Vec<&str> = profiles.own.iter().map(|p| p.name.as_str()).collect();
    let mut note = format!("Profiles: {}.", names.join(", "));
    match profiles.shared.as_slice() {
        [] => {}
        [one] => note.push_str(&format!(" {one} uses the Default profile's models.")),
        many => note.push_str(&format!(
            " {} use the Default profile's models.",
            many.join(", ")
        )),
    }
    Some(note)
}

fn has_entry(path: &Path) -> bool {
    read_optional(path)
        .ok()
        .flatten()
        .and_then(|text| jsonc::read_value(&text).ok())
        .and_then(|v| v.as_array().cloned())
        .is_some_and(|entries| entries.iter().any(|e| e["name"] == PROVIDER_ID))
}

fn entry_secret(root: &CstRootNode) -> Option<String> {
    let entry = find_entry(&root.array_value()?)?;
    Some(secret_ref(&jsonc::string_prop(&entry, "apiKey")?)?.to_string())
}

fn find_entry(entries: &CstArray) -> Option<CstObject> {
    entries
        .elements()
        .into_iter()
        .filter_map(|n| n.as_object())
        .find(|o| jsonc::string_prop(o, "name").as_deref() == Some(PROVIDER_ID))
}

fn find_model(models: &CstArray, id: &str) -> Option<CstObject> {
    models
        .elements()
        .into_iter()
        .filter_map(|n| n.as_object())
        .find(|o| jsonc::string_prop(o, "id").as_deref() == Some(id))
}

fn secret_ref(api_key: &str) -> Option<&str> {
    api_key.strip_prefix("${input:")?.strip_suffix('}')
}

/// `%20` and friends in `file://` profile locations.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn model_entry(model: &ModelSpec, base_url: &str, config: &Config) -> Value {
    let mut entry = json!({
        "id": model.id,
        "name": model.display_name,
        "url": base_url,
        "toolCalling": model.tool_calling,
        "vision": model.vision,
        "thinking": model.reasoning,
    });
    let obj = entry.as_object_mut().unwrap();
    if model.reasoning {
        obj.insert(
            "supportsReasoningEffort".into(),
            json!(config.reasoning_efforts_for(&model.id)),
        );
    }
    if let Some(context) = model.context_window {
        obj.insert("contextWindow".into(), json!(context));
    }
    if let Some(output) = model.max_output_tokens {
        obj.insert("maxOutputTokens".into(), json!(output));
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Credentials;
    use crate::harness::tests::{create_state_db, paths_in, spec};

    const USER_MODELS: &str = r#"[
	{
		"name": "self-hosted",
		"vendor": "customendpoint",
		"apiType": "chat-completions",
		"models": [{"id": "Qwen/Qwen3.8-27B", "name": "Qwen3.8 27B", "url": "http://localhost:8000/v1/chat/completions"}]
	},
	{
		"name": "litellm",
		"vendor": "customendpoint",
		"apiKey": "${input:chat.lm.secret.1c807067}",
		"apiType": "chat-completions",
		"models": [{"id": "azure/gpt-5-mini", "name": "gpt-5-mini", "url": "https://old", "custom": 1}],
		"settings": {"azure/gpt-5.4-nano": {"reasoningEffort": "none"}}
	}
]
"#;

    fn creds() -> Credentials {
        Credentials {
            base_url: "https://llm.example.com".into(),
            api_key: "sk-1".into(),
        }
    }

    fn install(
        harness: &VsCode,
        creds: &Credentials,
        models: &[ModelSpec],
        record: Option<&HarnessRecord>,
    ) -> Plan {
        harness
            .plan_install(
                &InstallCtx {
                    creds,
                    models,
                    config: &Config::default(),
                },
                record,
            )
            .unwrap()
    }

    fn files_only(plan: Plan) -> Plan {
        Plan {
            changes: plan
                .changes
                .into_iter()
                .filter(|c| matches!(c, Change::File { .. }))
                .collect(),
            ..plan
        }
    }

    fn read(path: &Path) -> Value {
        jsonc::read_value(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn written(plan: &Plan) -> Vec<PathBuf> {
        plan.changes
            .iter()
            .filter(|c| matches!(c, Change::File { .. }))
            .map(|c| c.path().to_path_buf())
            .collect()
    }

    fn deleted_secrets(plan: &Plan) -> Vec<String> {
        plan.changes
            .iter()
            .filter_map(|c| match c {
                Change::DbItem {
                    key,
                    value: DbValue::Delete,
                    ..
                } => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    fn write_profiles(user: &Path, profiles: Value) {
        std::fs::write(
            user.join("globalStorage/storage.json"),
            json!({"userDataProfiles": profiles}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn merges_into_existing_entry_and_reuses_secret() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        std::fs::create_dir_all(&paths.vscode_user_dir).unwrap();
        std::fs::write(paths.vscode_user_dir.join(MODELS_FILE), USER_MODELS).unwrap();
        create_state_db(
            &paths.vscode_user_dir.join("globalStorage/state.vscdb"),
            &[("secret://chat.lm.secret.1c807067", "x")],
        );

        let creds = creds();
        let models = vec![
            spec("azure/gpt-5-mini", true),
            spec("azure/gpt-5.4-nano", false),
        ];
        let harness = VsCode::new(&paths);
        let plan = install(&harness, &creds, &models, None);
        assert!(plan.notes.is_empty(), "no profile note with only Default");

        let Change::File { after, .. } = &plan.changes[0] else {
            panic!("expected file change")
        };
        assert!(
            after.contains("\t\t\"name\": \"self-hosted\""),
            "tabs are kept:\n{after}"
        );
        let value = jsonc::read_value(after).unwrap();
        assert_eq!(
            value[0]["models"][0]["url"],
            json!("http://localhost:8000/v1/chat/completions")
        );
        let entry = &value[1];
        assert_eq!(
            entry["settings"],
            json!({"azure/gpt-5.4-nano": {"reasoningEffort": "none"}})
        );
        assert_eq!(entry["models"][0]["custom"], json!(1));
        assert_eq!(entry["models"][0]["url"], json!("https://llm.example.com"));
        assert_eq!(
            entry["models"][0]["supportsReasoningEffort"],
            json!(["none", "low", "medium", "high", "xhigh"])
        );
        assert_eq!(entry["models"][1]["id"], json!("azure/gpt-5.4-nano"));
        assert_eq!(entry["models"][1]["contextWindow"], json!(400000));
        assert!(entry["models"][1].get("supportsReasoningEffort").is_none());

        let Change::DbItem { key, value, .. } = &plan.changes[1] else {
            panic!("expected secret change")
        };
        assert_eq!(key, "secret://chat.lm.secret.1c807067");
        assert_eq!(value, &DbValue::Secret("sk-1".into()));

        let record = plan.record.unwrap();
        let default = &record.profiles[""];
        assert_eq!(default.added_models, vec!["azure/gpt-5.4-nano".to_string()]);
        assert!(!default.created_provider);
        assert!(record.created_secrets.is_empty());

        // Same key on the next run: the secret is not rewritten, so VSCode needn't be closed.
        let again = install(&harness, &creds, &models, Some(&record));
        assert!(
            again
                .changes
                .iter()
                .all(|c| matches!(c, Change::File { .. }))
        );
        let rotated = Credentials {
            api_key: "sk-2".into(),
            ..creds.clone()
        };
        assert!(install(&harness, &rotated, &models, Some(&record)).writes_secrets());
    }

    #[test]
    fn creates_entry_and_uninstalls_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        create_state_db(
            &paths.vscode_user_dir.join("globalStorage/state.vscdb"),
            &[],
        );

        let models = vec![spec("azure/gpt-5.4-nano", true)];
        let harness = VsCode::new(&paths);
        let plan = install(&harness, &creds(), &models, None);
        let record = plan.record.clone().unwrap();
        assert!(record.profiles[""].created_provider);
        assert_eq!(record.created_secrets, [DEFAULT_SECRET]);

        // Apply only the file part; the secret needs the Keychain.
        crate::harness::apply::apply(&files_only(plan), &dir.path().join("backup"), None).unwrap();
        assert!(harness.is_installed());

        let uninstall = harness.plan_uninstall(&record).unwrap();
        assert!(matches!(
            uninstall.changes.last(),
            Some(Change::DbItem {
                value: DbValue::Delete,
                ..
            })
        ));
        crate::harness::apply::apply(&uninstall, &dir.path().join("backup2"), None).unwrap();
        assert!(!harness.is_installed());
        let db = StateDb::open_readonly(&paths.vscode_user_dir.join("globalStorage/state.vscdb"))
            .unwrap();
        assert!(db.get("secret://chat.lm.secret.liteton").unwrap().is_none());
    }

    #[test]
    fn writes_every_profile_with_its_own_models() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let user = &paths.vscode_user_dir;
        create_state_db(&user.join("globalStorage/state.vscdb"), &[]);
        std::fs::write(
            user.join("globalStorage/storage.json"),
            json!({"userDataProfiles": [
                {"location": "-24b23f00", "name": "DSA"},
                {"location": "502346", "name": "Work"},
                {"location": "builtin/agents", "name": "Agents",
                 "useDefaultFlags": {"languageModels": true, "settings": true}}
            ]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(user.join(MODELS_FILE), "[]").unwrap();
        let work = user.join("profiles/502346");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join(MODELS_FILE), USER_MODELS).unwrap();
        let agents = user.join("profiles/builtin/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join(MODELS_FILE), "[]").unwrap();

        let models = vec![spec("azure/gpt-5.4-nano", false)];
        let harness = VsCode::new(&paths);
        let plan = install(&harness, &creds(), &models, None);
        assert_eq!(
            plan.notes,
            ["Profiles: Default, DSA, Work. Agents uses the Default profile's models."]
        );
        assert_eq!(
            written(&plan),
            [
                user.join(MODELS_FILE),
                user.join("profiles/-24b23f00").join(MODELS_FILE),
                work.join(MODELS_FILE),
            ]
        );
        let secrets: Vec<&String> = plan
            .changes
            .iter()
            .filter_map(|c| match c {
                Change::DbItem { key, .. } => Some(key),
                Change::File { .. } => None,
            })
            .collect();
        assert_eq!(
            secrets,
            ["secret://chat.lm.secret.1c807067"],
            "new entries reuse the Work profile's secret"
        );

        let record = plan.record.clone().unwrap();
        crate::harness::apply::apply(&files_only(plan), &dir.path().join("backup"), None).unwrap();
        let backups: Vec<String> = std::fs::read_dir(dir.path().join("backup/vscode"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        for name in [
            "User-chatLanguageModels.json",
            "502346-chatLanguageModels.json",
        ] {
            assert!(backups.contains(&name.to_string()), "{backups:?}");
        }

        let dsa = read(&user.join("profiles/-24b23f00").join(MODELS_FILE));
        assert_eq!(dsa[0]["apiKey"], json!("${input:chat.lm.secret.1c807067}"));
        assert_eq!(dsa[0]["models"][0]["id"], json!("azure/gpt-5.4-nano"));
        let work_value = read(&work.join(MODELS_FILE));
        assert_eq!(work_value[0]["name"], json!("self-hosted"));
        assert_eq!(work_value[1]["models"][0]["custom"], json!(1));
        assert_eq!(
            work_value[1]["models"][1]["id"],
            json!("azure/gpt-5.4-nano")
        );
        assert_eq!(read(&agents.join(MODELS_FILE)), json!([]));

        let uninstall = harness.plan_uninstall(&record).unwrap();
        crate::harness::apply::apply(&uninstall, &dir.path().join("backup2"), None).unwrap();
        assert_eq!(read(&user.join(MODELS_FILE)), json!([]));
        assert_eq!(
            read(&user.join("profiles/-24b23f00").join(MODELS_FILE)),
            json!([])
        );
        let work_value = read(&work.join(MODELS_FILE));
        assert_eq!(
            work_value[1]["models"].as_array().unwrap().len(),
            1,
            "the user's own model stays"
        );
    }

    #[test]
    fn finds_profile_files_without_storage_json() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let user = &paths.vscode_user_dir;
        create_state_db(&user.join("globalStorage/state.vscdb"), &[]);
        let work = user.join("profiles/502346");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join(MODELS_FILE), "[]").unwrap();
        std::fs::create_dir_all(user.join("profiles/empty")).unwrap();

        let harness = VsCode::new(&paths);
        let plan = install(&harness, &creds(), &[spec("m", false)], None);
        assert_eq!(
            written(&plan),
            [user.join(MODELS_FILE), work.join(MODELS_FILE)]
        );
        assert_eq!(plan.notes, ["Profiles: Default, 502346."]);
    }

    #[test]
    fn walks_profile_folders_only_when_the_list_is_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let user = &paths.vscode_user_dir;
        create_state_db(&user.join("globalStorage/state.vscdb"), &[]);
        write_profiles(user, json!([{"location": "-24b23f00", "name": "DSA"}]));
        let old = user.join("profiles/old");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join(MODELS_FILE), "[]").unwrap();
        let harness = VsCode::new(&paths);
        let dsa = user.join("profiles/-24b23f00").join(MODELS_FILE);

        let plan = install(&harness, &creds(), &[spec("m", false)], None);
        assert_eq!(written(&plan), [user.join(MODELS_FILE), dsa.clone()]);

        write_profiles(
            user,
            json!([{"location": "-24b23f00", "name": "DSA"}, {"location": 42, "name": "New"}]),
        );
        let plan = install(&harness, &creds(), &[spec("m", false)], None);
        assert_eq!(
            written(&plan),
            [user.join(MODELS_FILE), dsa, old.join(MODELS_FILE)]
        );
    }

    #[test]
    fn skips_profiles_it_cannot_parse() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let user = &paths.vscode_user_dir;
        create_state_db(&user.join("globalStorage/state.vscdb"), &[]);
        write_profiles(user, json!([{"location": "-24b23f00", "name": "DSA"}]));
        let dsa = user.join("profiles/-24b23f00");
        std::fs::create_dir_all(&dsa).unwrap();
        std::fs::write(dsa.join(MODELS_FILE), "[{").unwrap();
        let harness = VsCode::new(&paths);

        let plan = install(&harness, &creds(), &[spec("m", false)], None);
        assert_eq!(written(&plan), [user.join(MODELS_FILE)]);
        assert!(
            plan.notes
                .iter()
                .any(|n| n.starts_with("Skipped the DSA profile: parsing ")),
            "{:?}",
            plan.notes
        );

        std::fs::write(user.join(MODELS_FILE), "[{").unwrap();
        let err = harness
            .plan_install(
                &InstallCtx {
                    creds: &creds(),
                    models: &[spec("m", false)],
                    config: &Config::default(),
                },
                None,
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("parsing"), "{err:#}");
    }

    #[test]
    fn tracks_every_secret_it_creates() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let user = &paths.vscode_user_dir;
        create_state_db(&user.join("globalStorage/state.vscdb"), &[]);
        write_profiles(user, json!([{"location": "502346", "name": "Work"}]));
        let entry = |secret: &str| {
            json!([{"name": "litellm", "vendor": "customendpoint",
                    "apiKey": format!("${{input:{secret}}}"), "models": []}])
            .to_string()
        };
        std::fs::write(user.join(MODELS_FILE), entry("chat.lm.secret.a")).unwrap();
        let work = user.join("profiles/502346");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join(MODELS_FILE), entry("chat.lm.secret.b")).unwrap();

        let harness = VsCode::new(&paths);
        let record = install(&harness, &creds(), &[spec("m", false)], None)
            .record
            .unwrap();
        assert_eq!(
            record.created_secrets,
            ["chat.lm.secret.a", "chat.lm.secret.b"]
        );
        assert_eq!(
            deleted_secrets(&harness.plan_uninstall(&record).unwrap()),
            [
                secret_key("chat.lm.secret.a"),
                secret_key("chat.lm.secret.b")
            ]
        );
    }

    #[test]
    fn forgets_deleted_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        create_state_db(
            &paths.vscode_user_dir.join("globalStorage/state.vscdb"),
            &[],
        );
        let old = HarnessRecord {
            profiles: [(
                "profiles/gone".to_string(),
                ProfileRecord {
                    added_models: vec!["x".into()],
                    created_provider: true,
                },
            )]
            .into(),
            ..HarnessRecord::default()
        };
        let next = install(
            &VsCode::new(&paths),
            &creds(),
            &[spec("m", false)],
            Some(&old),
        )
        .record
        .unwrap();
        assert_eq!(next.profiles.keys().collect::<Vec<_>>(), [""]);
    }

    #[test]
    fn reads_records_from_before_profiles_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let user = &paths.vscode_user_dir;
        create_state_db(&user.join("globalStorage/state.vscdb"), &[]);
        std::fs::create_dir_all(user).unwrap();
        std::fs::write(
            user.join(MODELS_FILE),
            r#"[{"name": "litellm", "vendor": "customendpoint", "models": [{"id": "a"}, {"id": "b"}]}]"#,
        )
        .unwrap();
        let old = HarnessRecord {
            added_models: vec!["a".into()],
            created_provider: true,
            created_secret: Some(DEFAULT_SECRET.into()),
            ..HarnessRecord::default()
        };
        let harness = VsCode::new(&paths);

        let uninstall = harness.plan_uninstall(&old).unwrap();
        let Change::File { after, .. } = &uninstall.changes[0] else {
            panic!("expected file change")
        };
        assert_eq!(
            jsonc::read_value(after).unwrap()[0]["models"],
            json!([{"id": "b"}])
        );
        assert_eq!(deleted_secrets(&uninstall), [secret_key(DEFAULT_SECRET)]);

        let next = install(&harness, &creds(), &[spec("b", false)], Some(&old))
            .record
            .unwrap();
        assert!(next.added_models.is_empty());
        assert_eq!(next.created_secret, None);
        assert_eq!(next.created_secrets, [DEFAULT_SECRET]);
        assert_eq!(
            next.profiles[""],
            ProfileRecord {
                added_models: vec![],
                created_provider: true,
            },
            "\"a\" was deselected and removed; \"b\" was already there"
        );
    }

    #[test]
    fn decodes_profile_locations() {
        let harness = VsCode {
            user_dir: PathBuf::from("/U"),
        };
        assert_eq!(
            harness.profile_dir(&json!("abc")),
            Some(PathBuf::from("/U/profiles/abc"))
        );
        assert_eq!(
            harness.profile_dir(&json!("file:///Users/me/Application%20Support/p")),
            Some(PathBuf::from("/Users/me/Application Support/p"))
        );
        assert_eq!(
            harness.profile_dir(&json!({"$mid": 1, "path": "/x/y", "scheme": "file"})),
            Some(PathBuf::from("/x/y"))
        );
        assert_eq!(
            harness.key_for(Path::new("/U/profiles/abc")),
            "profiles/abc"
        );
        assert_eq!(
            harness.dir_for("profiles/abc"),
            PathBuf::from("/U/profiles/abc")
        );
    }
}
