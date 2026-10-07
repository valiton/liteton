pub mod dashboard;
pub mod install;
pub mod models;
pub mod prompts;
pub mod usage_bar;

use anyhow::{Result, anyhow};
use crossterm::style::Stylize;
use similar::{ChangeTag, TextDiff};

use crate::config::{self, Config, Credentials};
use crate::harness::{Change, DbValue};

pub fn require_credentials(config: &Config) -> Result<Credentials> {
    config::load_credentials(config)?
        .ok_or_else(|| anyhow!("not logged in; run `liteton login` first"))
}

/// Styling only goes to terminals, and never when NO_COLOR is set.
pub fn use_styling() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn print_line(line: &str) {
    if use_styling() {
        println!("{line}");
    } else {
        println!("{}", strip_ansi(line));
    }
}

pub fn format_cost(cost: Option<f64>) -> String {
    match cost {
        Some(0.0) => "free".into(),
        Some(c) if c < 0.01 => format!("${c:.4}"),
        Some(c) => format!("${c:.2}"),
        None => "-".into(),
    }
}

pub fn format_tokens(tokens: Option<u64>) -> String {
    match tokens {
        Some(t) if t >= 1_000_000 && t % 1_000_000 == 0 => format!("{}M", t / 1_000_000),
        Some(t) if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1e6),
        Some(t) if t >= 1000 => format!("{}k", t / 1000),
        Some(t) => t.to_string(),
        None => "-".into(),
    }
}

pub fn format_usd(amount: f64) -> String {
    format!("${amount:.2}")
}

/// Human-readable preview of one change: a coloured unified diff for files, a summary for
/// database rows. Secrets never appear.
pub fn change_preview(change: &Change) -> String {
    match change {
        Change::File {
            path,
            before,
            after,
            private,
            summary,
        } => {
            let title = if before.is_some() {
                format!("{}", path.display())
            } else {
                format!("{} (new file)", path.display())
            };
            let mut out = format!("{}\n", title.bold());
            if *private {
                for line in summary {
                    out.push_str(&format!("{}{line}\n", " ~ ".yellow()));
                }
                return out;
            }
            let diff = TextDiff::from_lines(before.as_deref().unwrap_or(""), after.as_str());
            for (i, group) in diff.grouped_ops(2).iter().enumerate() {
                if i > 0 {
                    out.push_str(&format!("{}\n", "   ⋯".dark_grey()));
                }
                for op in group {
                    for change in diff.iter_changes(op) {
                        let line = change.value().trim_end_matches('\n');
                        let rendered = match change.tag() {
                            ChangeTag::Delete => format!("{}", format!(" - {line}").red()),
                            ChangeTag::Insert => format!("{}", format!(" + {line}").green()),
                            ChangeTag::Equal => format!("{}", format!("   {line}").dark_grey()),
                        };
                        out.push_str(&rendered);
                        out.push('\n');
                    }
                }
            }
            out
        }
        Change::DbItem {
            label,
            summary,
            value,
            db,
            ..
        } => {
            let mut out = format!(
                "{}\n{}\n",
                label.clone().bold(),
                format!("   {}", db.display()).dark_grey()
            );
            for line in summary {
                let marker = if matches!(value, DbValue::Delete) {
                    " - ".red()
                } else {
                    " ~ ".yellow()
                };
                out.push_str(&format!("{marker}{line}\n"));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers() {
        assert_eq!(format_tokens(Some(400_000)), "400k");
        assert_eq!(format_tokens(Some(1_000_000)), "1M");
        assert_eq!(format_tokens(Some(1_048_576)), "1.0M");
        assert_eq!(format_cost(Some(0.05)), "$0.05");
        assert_eq!(format_cost(Some(0.005)), "$0.0050");
        assert_eq!(format_cost(None), "-");
    }

    #[test]
    fn private_previews_never_show_contents() {
        let change = Change::File {
            path: "/tmp/auth.json".into(),
            before: Some("{\"other\": {\"key\": \"sk-other\"}}\n".into()),
            after: "{\"other\": {\"key\": \"sk-other\"}, \"litellm\": {\"key\": \"sk-secret\"}}\n"
                .into(),
            private: true,
            summary: vec!["add the key".into()],
        };
        let preview = strip_ansi(&change_preview(&change));
        assert!(!preview.contains("sk-secret") && !preview.contains("sk-other"));
        assert!(preview.contains("add the key"));
    }
}
