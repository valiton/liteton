use anyhow::{Result, bail};
use tokio::runtime::Runtime;

use crate::config::{self, Config, Credentials, normalize_base_url};
use crate::litellm::Client;

pub fn login(rt: &Runtime, base_url: Option<String>, api_key: Option<String>) -> Result<()> {
    cliclack::intro(" liteton login ")?;
    let mut config = Config::load()?;
    prompt_and_save(rt, &mut config, base_url, api_key)?;
    cliclack::outro(format!(
        "Saved. Base URL in {}, API key in the macOS Keychain.",
        config::config_dir().join("config.toml").display()
    ))?;
    Ok(())
}

pub fn logout() -> Result<()> {
    let mut config = Config::load()?;
    config::clear_credentials(&mut config)?;
    println!("Removed the saved base URL and API key.");
    Ok(())
}

/// Saved credentials, or an interactive login when there are none (unless `non_interactive`).
pub fn ensure_credentials(
    rt: &Runtime,
    config: &mut Config,
    non_interactive: bool,
) -> Result<Credentials> {
    if let Some(creds) = config::load_credentials(config)? {
        cliclack::log::info(format!("Using {}", creds.base_url))?;
        return Ok(creds);
    }
    if non_interactive {
        bail!(
            "no saved credentials; run `liteton login` or set LITETON_BASE_URL and LITETON_API_KEY"
        );
    }
    cliclack::log::step("No saved credentials yet, let's log in.")?;
    prompt_and_save(rt, config, None, None)
}

fn prompt_and_save(
    rt: &Runtime,
    config: &mut Config,
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<Credentials> {
    let base_url = match base_url {
        Some(url) => url,
        None => {
            let mut input = cliclack::input("LiteLLM base URL")
                .placeholder("https://litellm.example.com")
                .validate(|value: &String| {
                    if value.starts_with("http://") || value.starts_with("https://") {
                        Ok(())
                    } else {
                        Err("must start with http:// or https://")
                    }
                });
            if let Some(existing) = &config.base_url {
                input = input.default_input(existing);
            }
            input.interact()?
        }
    };
    let api_key = match api_key.or_else(|| std::env::var("LITETON_API_KEY").ok()) {
        Some(key) => key,
        None => cliclack::password("API key").mask('•').interact()?,
    };
    let creds = Credentials {
        base_url: normalize_base_url(&base_url),
        api_key: api_key.trim().to_string(),
    };

    let spinner = cliclack::spinner();
    spinner.start("Checking the key against LiteLLM");
    let client = Client::new(&creds)?;
    match rt.block_on(client.list_model_ids()) {
        Ok(ids) => spinner.stop(format!("Key works, {} models available", ids.len())),
        Err(e) => {
            spinner.error(format!("{e:#}"));
            return Err(e);
        }
    }
    config::save_credentials(config, &creds)?;
    Ok(creds)
}
