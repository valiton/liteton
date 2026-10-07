mod cli;
mod config;
mod harness;
mod jsonc;
mod litellm;
mod ui;
mod vscdb;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match cli.command.unwrap_or(Command::Dashboard) {
        Command::Login { base_url, api_key } => ui::prompts::login(&rt, base_url, api_key),
        Command::Logout => ui::prompts::logout(),
        Command::Install(args) => ui::install::install(&rt, args),
        Command::Uninstall {
            harness,
            yes,
            dry_run,
        } => ui::install::uninstall(harness, yes, dry_run),
        Command::Models { json } => ui::models::models(&rt, json),
        Command::Usage { json, ping } => ui::usage_bar::usage(&rt, json, ping),
        Command::Dashboard => ui::dashboard::dashboard(&rt),
    }
}
