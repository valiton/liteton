use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use crate::config::Credentials;

use super::budget::BudgetInfo;
use super::models::{ModelSpec, parse_model_group_info, parse_model_info};

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

#[derive(Debug)]
pub enum Fetch<T> {
    Ok(T),
    /// The endpoint exists but this key may not use it (401/403/404).
    Denied(StatusCode),
}

impl Client {
    pub fn new(creds: &Credentials) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("liteton/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base_url: creds.base_url.clone(),
            api_key: creds.api_key.clone(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get_json(&self, path: &str) -> Result<Fetch<Value>> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .with_context(|| format!("requesting {}", self.url(path)))?;
        let status = resp.status();
        if matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            return Ok(Fetch::Denied(status));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "{} returned {status}: {}",
                self.url(path),
                truncate(&body, 300)
            );
        }
        Ok(Fetch::Ok(
            resp.json()
                .await
                .with_context(|| format!("decoding {}", self.url(path)))?,
        ))
    }

    /// Model ids the key may use; also validates the key.
    pub async fn list_model_ids(&self) -> Result<Vec<String>> {
        match self.get_json("/v1/models").await? {
            Fetch::Ok(body) => Ok(body["data"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|m| m["id"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()),
            Fetch::Denied(StatusCode::NOT_FOUND) => bail!(
                "{} has no /v1/models endpoint, is this a LiteLLM proxy?",
                self.base_url
            ),
            Fetch::Denied(status) => bail!("the API key was rejected ({status})"),
        }
    }

    /// Chat models with limits, capabilities and pricing. `/model/info` carries cache prices
    /// and long-context tiers; `/model_group/info` is the fallback when that route is blocked.
    /// `/model/info` lists load-balanced deployments separately, so the first one per name wins.
    pub async fn models(&self) -> Result<Vec<ModelSpec>> {
        let ids = self.list_model_ids().await?;
        let mut specs = match self.get_json("/model/info").await? {
            Fetch::Ok(body) => parse_model_info(&body),
            Fetch::Denied(_) => match self.get_json("/model_group/info").await? {
                Fetch::Ok(body) => parse_model_group_info(&body),
                Fetch::Denied(_) => Vec::new(),
            },
        };
        let has_info = !specs.is_empty();
        specs.retain(|spec| ids.is_empty() || ids.contains(&spec.id));
        for id in &ids {
            if !specs.iter().any(|spec| &spec.id == id) && (!has_info || !looks_non_chat(id)) {
                specs.push(ModelSpec::bare(id));
            }
        }
        specs.sort_by(|a, b| a.id.cmp(&b.id));
        specs.dedup_by(|a, b| a.id == b.id);
        Ok(specs)
    }

    pub async fn key_info(&self) -> Result<Fetch<Value>> {
        self.get_json("/key/info").await
    }

    pub async fn user_info(&self) -> Result<Fetch<Value>> {
        self.get_json("/user/info").await
    }

    /// Sends a 1-token completion and reads the LiteLLM budget headers from the response.
    pub async fn ping_budget(&self, model: &str) -> Result<BudgetInfo> {
        let resp = self
            .http
            .post(self.url("/v1/chat/completions"))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "."}],
                "max_tokens": 1,
            }))
            .send()
            .await
            .context("sending the budget ping")?;
        let headers: HeaderMap = resp.headers().clone();
        let status = resp.status();
        BudgetInfo::from_headers(&headers).ok_or_else(|| {
            anyhow!("the ping to {model} returned {status} without x-litellm-key-spend headers")
        })
    }
}

/// `/v1/models` also lists embedding and image models; their names are the only hint we have.
fn looks_non_chat(id: &str) -> bool {
    let id = id.to_lowercase();
    [
        "embed",
        "whisper",
        "tts",
        "dall-e",
        "image",
        "rerank",
        "moderation",
    ]
    .iter()
    .any(|k| id.contains(k))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}
