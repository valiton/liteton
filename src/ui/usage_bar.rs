use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use crossterm::style::{Color, Stylize};
use tokio::runtime::Runtime;

use super::{format_usd, print_line};
use crate::config::Config;
use crate::litellm::budget::{BudgetSource, fetch_budget};
use crate::litellm::{BudgetInfo, Client};

const BAR_WIDTH: usize = 32;

pub fn usage(rt: &Runtime, json: bool, ping: bool) -> Result<()> {
    let creds = super::require_credentials(&Config::load()?)?;
    let client = Client::new(&creds)?;
    let info = rt.block_on(async {
        let models = if ping {
            client.models().await?
        } else {
            Vec::new()
        };
        fetch_budget(&client, &models, ping).await
    })?
    .ok_or_else(|| {
        anyhow!(
            "this key may not read /key/info or /user/info; rerun with --ping to read the budget from response headers"
        )
    })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }
    let mut title = format!("LiteLLM usage · {}", creds.base_url);
    if let Some(alias) = &info.key_alias {
        title.push_str(&format!(" · key \"{alias}\""));
    }
    print_line(&format!("\n  {}\n", title.bold()));
    for line in render(&info, Utc::now()) {
        print_line(&format!("  {line}"));
    }
    println!();
    Ok(())
}

/// Green below 70%, yellow below 90%, red above.
pub fn level_color(ratio: f64) -> Color {
    if ratio >= 0.9 {
        Color::Red
    } else if ratio >= 0.7 {
        Color::Yellow
    } else {
        Color::Green
    }
}

pub fn render(info: &BudgetInfo, now: DateTime<Utc>) -> Vec<String> {
    let mut lines = Vec::new();
    match (info.max_budget, info.ratio()) {
        (Some(max), ratio) => {
            let ratio = ratio.unwrap_or(1.0);
            let filled = (ratio * BAR_WIDTH as f64).round() as usize;
            let bar = format!(
                "{}{}",
                "█".repeat(filled).with(level_color(ratio)),
                "░".repeat(BAR_WIDTH - filled).dark_grey()
            );
            lines.push(format!(
                "{bar}  {} / {}  ({:.0}%)",
                format_usd(info.spend).bold(),
                format_usd(max),
                ratio * 100.0
            ));
            let mut detail = format!("{} remaining", format_usd(info.remaining().unwrap_or(0.0)));
            if let Some(reset) = reset_text(info, now) {
                detail.push_str(&format!(" · {reset}"));
            }
            lines.push(detail.dark_grey().to_string());
        }
        (None, _) => {
            lines.push(format!(
                "{} spent · no budget limit (unlimited)",
                format_usd(info.spend).bold()
            ));
        }
    }
    let source = match info.source {
        BudgetSource::Key => "key budget",
        BudgetSource::User => "user budget (the key has none of its own)",
        BudgetSource::Headers => "from response headers",
    };
    lines.push(format!("{}", source.dark_grey()));
    lines
}

pub fn reset_text(info: &BudgetInfo, now: DateTime<Utc>) -> Option<String> {
    let period = info
        .budget_duration
        .as_ref()
        .map(|d| format!(" ({d} budget)"))
        .unwrap_or_default();
    let reset = info.budget_reset_at?;
    let left = reset - now;
    let when = if left.num_days() >= 1 {
        format!("{}d {}h", left.num_days(), left.num_hours() % 24)
    } else if left.num_hours() >= 1 {
        format!("{}h {}m", left.num_hours(), left.num_minutes() % 60)
    } else if left.num_minutes() >= 1 {
        format!("{}m", left.num_minutes())
    } else {
        return Some(format!("resets now{period}"));
    };
    Some(format!("resets in {when}{period}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(spend: f64, max: Option<f64>) -> BudgetInfo {
        BudgetInfo {
            source: BudgetSource::Key,
            spend,
            max_budget: max,
            budget_duration: Some("30d".into()),
            budget_reset_at: Some("2026-10-19T12:00:00Z".parse().unwrap()),
            key_alias: None,
        }
    }

    fn plain(lines: Vec<String>) -> String {
        crate::ui::strip_ansi(&lines.join("\n"))
    }

    #[test]
    fn renders_half_used_budget() {
        let now: DateTime<Utc> = "2026-10-07T10:00:00Z".parse().unwrap();
        let text = plain(render(&info(10.0, Some(20.0)), now));
        assert!(
            text.contains(&format!("{}{}", "█".repeat(16), "░".repeat(16))),
            "{text}"
        );
        assert!(text.contains("$10.00 / $20.00  (50%)"));
        assert!(text.contains("$10.00 remaining · resets in 12d 2h (30d budget)"));
    }

    #[test]
    fn renders_unlimited_and_overspent() {
        let now: DateTime<Utc> = "2026-10-07T10:00:00Z".parse().unwrap();
        assert!(plain(render(&info(3.5, None), now)).contains("$3.50 spent · no budget limit"));
        let over = plain(render(&info(25.0, Some(20.0)), now));
        assert!(over.contains(&"█".repeat(BAR_WIDTH)));
        assert!(over.contains("$0.00 remaining"));
    }
}
