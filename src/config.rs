use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const KEYCHAIN_SERVICE: &str = "liteton";
const KEYCHAIN_ACCOUNT: &str = "api-key";

pub const DEFAULT_REASONING_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_reasoning_efforts: BTreeMap<String, Vec<String>>,
    /// opencode prices long prompts only from 200k tokens on. `true` writes a later tier
    /// (e.g. above 272k) there, so opencode overestimates requests between 200k and that
    /// threshold. Unset means `liteton install` asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_approximate_long_context: Option<bool>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_file();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_file();
        ensure_private_dir(path.parent().unwrap())?;
        fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn reasoning_efforts_for(&self, model_id: &str) -> Vec<String> {
        if let Some(levels) = self.model_reasoning_efforts.get(model_id) {
            return levels.clone();
        }
        match &self.reasoning_efforts {
            Some(levels) => levels.clone(),
            None => DEFAULT_REASONING_EFFORTS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub base_url: String,
    pub api_key: String,
}

pub fn load_credentials(config: &Config) -> Result<Option<Credentials>> {
    let base_url = std::env::var("LITETON_BASE_URL")
        .ok()
        .or_else(|| config.base_url.clone());
    let Some(base_url) = base_url else {
        return Ok(None);
    };
    let api_key = match std::env::var("LITETON_API_KEY") {
        Ok(key) => Some(key),
        Err(_) => keychain_get_api_key()?,
    };
    Ok(api_key.map(|api_key| Credentials {
        base_url: normalize_base_url(&base_url),
        api_key,
    }))
}

pub fn save_credentials(config: &mut Config, creds: &Credentials) -> Result<()> {
    config.base_url = Some(creds.base_url.clone());
    config.save()?;
    security_framework::passwords::set_generic_password(
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        creds.api_key.as_bytes(),
    )
    .context("storing the API key in the Keychain")
}

pub fn clear_credentials(config: &mut Config) -> Result<()> {
    config.base_url = None;
    config.save()?;
    match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
    {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
        Err(e) => Err(anyhow!(e).context("removing the API key from the Keychain")),
    }
}

const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

fn keychain_get_api_key() -> Result<Option<String>> {
    match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
        Ok(bytes) => Ok(Some(
            String::from_utf8(bytes).context("Keychain API key is not valid UTF-8")?,
        )),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(e) => Err(anyhow!(e).context("reading the API key from the Keychain")),
    }
}

pub fn normalize_base_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().expect("could not determine the home directory")
}

pub fn xdg_config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
}

pub fn xdg_data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share"))
}

pub fn config_dir() -> PathBuf {
    xdg_config_home().join("liteton")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn backups_dir() -> PathBuf {
    config_dir().join("backups")
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("securing {}", path.display()))
}

/// Remove what liteton added to each "harness", so `uninstall` can remove exactly that
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstallState {
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HarnessRecord {
    /// Models liteton created (models that already existed and were only updated are not listed).
    #[serde(default)]
    pub added_models: Vec<String>,
    /// liteton created the provider entry itself.
    #[serde(default)]
    pub created_provider: bool,
    /// Secret store key liteton created (VSCode `chat.lm.secret.*`, opencode auth entry, Cursor key).
    #[serde(default)]
    pub created_secret: Option<String>,
    /// Values liteton overwrote, restored on uninstall (used for Cursor settings).
    #[serde(default)]
    pub previous: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_fingerprint: Option<String>,
    /// VSCode: what liteton added per profile, keyed by the profile folder relative to the User
    /// folder ("" = Default).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, ProfileRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProfileRecord {
    #[serde(default)]
    pub added_models: Vec<String>,
    #[serde(default)]
    pub created_provider: bool,
}

impl HarnessRecord {
    /// Per-profile records. A VSCode record from before profiles counts as the Default profile's.
    pub fn profile_records(&self) -> BTreeMap<String, ProfileRecord> {
        let mut profiles = self.profiles.clone();
        if !self.added_models.is_empty() || self.created_provider {
            let default = profiles.entry(String::new()).or_default();
            for id in &self.added_models {
                if !default.added_models.contains(id) {
                    default.added_models.push(id.clone());
                }
            }
            default.created_provider |= self.created_provider;
        }
        profiles
    }

    /// Every model liteton added, across profiles.
    pub fn all_added_models(&self) -> impl Iterator<Item = &String> {
        self.added_models
            .iter()
            .chain(self.profiles.values().flat_map(|p| &p.added_models))
    }
}

pub fn key_fingerprint(api_key: &str) -> String {
    use sha1::Digest;
    let digest = sha1::Sha1::digest(format!("liteton:{api_key}").as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

impl InstallState {
    pub fn load() -> Result<Self> {
        let path = state_file();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = state_file();
        ensure_private_dir(path.parent().unwrap())?;
        fs::write(&path, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("writing {}", path.display()))
    }
}

fn state_file() -> PathBuf {
    config_dir().join("state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_base_url() {
        assert_eq!(normalize_base_url("https://x.dev/v1/"), "https://x.dev");
        assert_eq!(normalize_base_url(" https://x.dev// "), "https://x.dev");
        assert_eq!(
            normalize_base_url("https://x.dev/litellm"),
            "https://x.dev/litellm"
        );
    }

    #[test]
    fn reasoning_overrides() {
        let mut config = Config::default();
        assert_eq!(
            config.reasoning_efforts_for("m").len(),
            DEFAULT_REASONING_EFFORTS.len()
        );
        config
            .model_reasoning_efforts
            .insert("m".into(), vec!["low".into()]);
        assert_eq!(config.reasoning_efforts_for("m"), vec!["low".to_string()]);
    }
}
