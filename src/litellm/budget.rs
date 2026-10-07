use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::Value;

use super::client::{Client, Fetch};
use super::models::ModelSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetSource {
    Key,
    User,
    Headers,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BudgetInfo {
    pub source: BudgetSource,
    pub spend: f64,
    pub max_budget: Option<f64>,
    pub budget_duration: Option<String>,
    pub budget_reset_at: Option<DateTime<Utc>>,
    pub key_alias: Option<String>,
}

impl BudgetInfo {
    pub fn remaining(&self) -> Option<f64> {
        self.max_budget.map(|max| (max - self.spend).max(0.0))
    }

    /// Fraction of the budget used, clamped to 0..=1.
    pub fn ratio(&self) -> Option<f64> {
        self.max_budget
            .filter(|max| *max > 0.0)
            .map(|max| (self.spend / max).clamp(0.0, 1.0))
    }

    /// Parses `/key/info` (`{"info": {...}}`) or `/user/info` (`{"user_info": {...}}`).
    pub fn from_info(body: &Value, source: BudgetSource) -> Option<Self> {
        let info = match source {
            BudgetSource::Key => &body["info"],
            BudgetSource::User => &body["user_info"],
            BudgetSource::Headers => return None,
        };
        if !info.is_object() {
            return None;
        }
        Some(Self {
            source,
            spend: info["spend"].as_f64().unwrap_or(0.0),
            max_budget: info["max_budget"].as_f64(),
            budget_duration: info["budget_duration"].as_str().map(str::to_string),
            budget_reset_at: info["budget_reset_at"].as_str().and_then(parse_timestamp),
            key_alias: info["key_alias"].as_str().map(str::to_string),
        })
    }

    pub fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let number = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok();
        let spend = number("x-litellm-key-spend")?;
        Some(Self {
            source: BudgetSource::Headers,
            spend,
            max_budget: number("x-litellm-key-max-budget"),
            budget_duration: None,
            budget_reset_at: None,
            key_alias: None,
        })
    }
}

/// LiteLLM timestamps come with or without a timezone suffix; naive ones are UTC.
fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|dt| dt.and_utc())
        })
}

/// Key budget first, then the user's budget when the key has none, then (opt-in) response headers.
/// `Ok(None)` means the key may not read its budget and pinging wasn't allowed.
pub async fn fetch_budget(
    client: &Client,
    models: &[ModelSpec],
    allow_ping: bool,
) -> Result<Option<BudgetInfo>> {
    let key = match client.key_info().await? {
        Fetch::Ok(body) => BudgetInfo::from_info(&body, BudgetSource::Key),
        Fetch::Denied(_) => None,
    };
    if let Some(key) = &key
        && key.max_budget.is_some()
    {
        return Ok(Some(key.clone()));
    }
    if let Fetch::Ok(body) = client.user_info().await?
        && let Some(user) = BudgetInfo::from_info(&body, BudgetSource::User)
        && user.max_budget.is_some()
    {
        return Ok(Some(user));
    }
    if key.is_some() || !allow_ping {
        return Ok(key);
    }
    let Some(model) = cheapest(models) else {
        bail!("no model available for the budget ping")
    };
    client.ping_budget(&model.id).await.map(Some)
}

fn cheapest(models: &[ModelSpec]) -> Option<&ModelSpec> {
    models.iter().min_by(|a, b| {
        let cost = |m: &ModelSpec| {
            m.pricing.input.unwrap_or(f64::MAX) + m.pricing.output.unwrap_or(f64::MAX)
        };
        cost(a).total_cmp(&cost(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use serde_json::json;

    #[test]
    fn parses_key_info() {
        let body = json!({"key": "x", "info": {
            "spend": 10.0, "max_budget": 20.0, "budget_duration": "30d",
            "budget_reset_at": "2026-11-01T00:00:00", "key_alias": "me"
        }});
        let info = BudgetInfo::from_info(&body, BudgetSource::Key).unwrap();
        assert_eq!(info.remaining(), Some(10.0));
        assert_eq!(info.ratio(), Some(0.5));
        assert_eq!(
            info.budget_reset_at.unwrap().to_rfc3339(),
            "2026-11-01T00:00:00+00:00"
        );
        assert_eq!(info.key_alias.as_deref(), Some("me"));
    }

    #[test]
    fn parses_user_info_without_budget() {
        let body = json!({"user_info": {"spend": 3.5, "max_budget": null}});
        let info = BudgetInfo::from_info(&body, BudgetSource::User).unwrap();
        assert_eq!(info.spend, 3.5);
        assert_eq!(info.remaining(), None);
        assert_eq!(info.ratio(), None);
    }

    #[test]
    fn parses_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-litellm-key-spend", HeaderValue::from_static("25.5"));
        headers.insert("x-litellm-key-max-budget", HeaderValue::from_static("20"));
        let info = BudgetInfo::from_headers(&headers).unwrap();
        assert_eq!(info.remaining(), Some(0.0));
        assert_eq!(info.ratio(), Some(1.0));

        let mut unlimited = HeaderMap::new();
        unlimited.insert("x-litellm-key-spend", HeaderValue::from_static("1.25"));
        assert_eq!(
            BudgetInfo::from_headers(&unlimited).unwrap().max_budget,
            None
        );
        assert!(BudgetInfo::from_headers(&HeaderMap::new()).is_none());
    }
}
