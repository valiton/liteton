use anyhow::Result;
use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};
use crossterm::style::Stylize;
use tokio::runtime::Runtime;

use super::{format_cost, format_tokens, print_line};
use crate::config::Config;
use crate::litellm::{Client, ModelSpec, Pricing};

pub fn models(rt: &Runtime, json: bool) -> Result<()> {
    let creds = super::require_credentials(&Config::load()?)?;
    let models = rt.block_on(Client::new(&creds)?.models())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&models)?);
        return Ok(());
    }
    print_line(&table(&models).to_string());
    print_line(&format!(
        "{} Source: {}",
        "Prices are USD per 1M tokens.".dark_grey(),
        creds.base_url
    ));
    if models.iter().any(|model| model.long_context().is_some()) {
        print_line(&format!(
            "The first price is the normal rate. {} applies to the whole request once the prompt passes {}.",
            "Yellow".yellow().bold(),
            "Long".yellow()
        ));
    }
    Ok(())
}

/// Cells carry ANSI styling; `print_line` strips it when styling is off.
fn table(models: &[ModelSpec]) -> Table {
    let mut table = Table::new();
    table
        .load_style(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(
            [
                "Model",
                "Context",
                "Max out",
                "Long",
                "Input",
                "Output",
                "Cache read",
                "Cache write",
                "Tools",
                "Vision",
                "Reasoning",
            ]
            .map(|name| Cell::new(name.cyan().bold())),
        );
    for model in models {
        let long = model.long_context();
        let price = |pick: fn(&Pricing) -> Option<f64>| {
            price_cell(
                pick(&model.pricing),
                long.and_then(|tier| pick(&tier.pricing)),
            )
        };
        table.add_row(vec![
            Cell::new(model.id.as_str().bold()),
            right(format_tokens(model.context_window)),
            right(format_tokens(model.max_output_tokens)),
            match long {
                Some(tier) => {
                    right(format!(">{}", format_tokens(Some(tier.above_tokens))).yellow())
                }
                None => right("—".dark_grey()),
            },
            price(|p| p.input),
            price(|p| p.output),
            price(|p| p.cache_read),
            price(|p| p.cache_write),
            mark(model.tool_calling),
            mark(model.vision),
            mark(model.reasoning),
        ]);
    }
    table
}

fn right(text: impl ToString) -> Cell {
    Cell::new(text).set_alignment(CellAlignment::Right)
}

fn mark(on: bool) -> Cell {
    let text = if on {
        "✓".green().to_string()
    } else {
        "·".dark_grey().to_string()
    };
    Cell::new(text).set_alignment(CellAlignment::Center)
}

/// Base rate in the terminal's normal text color, long-context rate in its yellow.
fn price_cell(base: Option<f64>, long: Option<f64>) -> Cell {
    match long {
        Some(long) => right(format!(
            "{} {} {}",
            format_cost(base),
            "→".dark_grey(),
            format_cost(Some(long)).yellow()
        )),
        None => right(format_cost(base)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::litellm::PriceTier;
    use crate::ui::strip_ansi;

    #[test]
    fn plain_table_shows_one_tier_per_row() {
        let mut luna = ModelSpec::bare("gpt-6-luna");
        luna.pricing = Pricing {
            input: Some(0.1),
            output: Some(0.5),
            cache_read: Some(0.01),
            cache_write: Some(0.125),
        };
        luna.tiers = vec![
            PriceTier {
                above_tokens: 272_000,
                pricing: Pricing {
                    input: Some(0.2),
                    output: Some(0.75),
                    ..Default::default()
                },
            },
            PriceTier {
                above_tokens: 512_000,
                pricing: Pricing {
                    cache_read: Some(0.04),
                    ..Default::default()
                },
            },
        ];
        let mut fable = ModelSpec::bare("claude-fable-5-1");
        fable.pricing.input = Some(10.0);

        let mut table = table(&[luna, fable]);
        table.set_width(200);
        let text = strip_ansi(&table.to_string());
        assert!(!text.contains('\x1b'));
        let luna_row = text.lines().find(|l| l.contains("gpt-6-luna")).unwrap();
        assert!(luna_row.contains(">272k"));
        assert!(luna_row.contains("$0.10 → $0.20") && luna_row.contains("$0.50 → $0.75"));
        assert!(
            !luna_row.contains("$0.04"),
            "cache read stays on the 272k tier"
        );
        let fable_row = text.lines().find(|l| l.contains("claude-fable")).unwrap();
        assert!(fable_row.contains("$10.00") && !fable_row.contains('→'));
    }
}
