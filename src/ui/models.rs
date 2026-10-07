use anyhow::Result;
use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};
use tokio::runtime::Runtime;

use super::{format_cost, format_tokens};
use crate::config::Config;
use crate::litellm::{Client, ModelSpec};

pub fn models(rt: &Runtime, json: bool) -> Result<()> {
    let creds = super::require_credentials(&Config::load()?)?;
    let models = rt.block_on(Client::new(&creds)?.models())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&models)?);
    } else {
        println!("{}", table(&models));
        println!("Prices are USD per 1M tokens, from {}", creds.base_url);
    }
    Ok(())
}

pub fn table(models: &[ModelSpec]) -> Table {
    let mut table = Table::new();
    table
        .load_style(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "Model",
            "Context",
            "Max out",
            "Input",
            "Output",
            "Cache read",
            "Tools",
            "Vision",
            "Reasoning",
        ]);
    let flag = |b: bool| if b { "✓" } else { "" };
    for m in models {
        let right = |text: String| Cell::new(text).set_alignment(CellAlignment::Right);
        let center = |text: &str| Cell::new(text).set_alignment(CellAlignment::Center);
        table.add_row(vec![
            Cell::new(&m.id),
            right(format_tokens(m.context_window)),
            right(format_tokens(m.max_output_tokens)),
            right(format_cost(m.input_cost)),
            right(format_cost(m.output_cost)),
            right(format_cost(m.cache_read_cost)),
            center(flag(m.tool_calling)),
            center(flag(m.vision)),
            center(flag(m.reasoning)),
        ]);
    }
    table
}
