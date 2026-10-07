use clap::{Parser, Subcommand};

use crate::harness::HarnessId;

#[derive(Debug, Parser)]
#[command(
    name = "liteton",
    version,
    about = "Set up coding harnesses to use your LiteLLM proxy"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Save the LiteLLM base URL and API key (key goes to the macOS Keychain).
    Login {
        /// LiteLLM base URL, e.g. https://litellm.example.com
        #[arg(long)]
        base_url: Option<String>,
        /// API key. Prefer the interactive prompt or LITETON_API_KEY so it doesn't land in shell history.
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Remove the saved base URL and API key.
    Logout,
    /// Configure harnesses to use LiteLLM models.
    Install(InstallArgs),
    /// Remove what liteton added to harness configs.
    Uninstall {
        /// Harness(es) to uninstall from. Prompts when omitted.
        #[arg(long, value_enum, num_args = 1..)]
        harness: Vec<HarnessId>,
        /// Skip the confirmation prompt.
        #[arg(short, long)]
        yes: bool,
        /// Show the changes without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// List available models with pricing and limits.
    Models {
        #[arg(long)]
        json: bool,
    },
    /// Show spend and remaining budget for your key.
    Usage {
        #[arg(long)]
        json: bool,
        /// If budget endpoints are blocked, read budget headers from a 1-token request (costs a tiny amount).
        #[arg(long)]
        ping: bool,
    },
    /// Full-screen dashboard with usage, models and harness status.
    Dashboard,
}

#[derive(Debug, clap::Args)]
pub struct InstallArgs {
    /// Harness(es) to configure. Prompts when omitted.
    #[arg(long, value_enum, num_args = 1..)]
    pub harness: Vec<HarnessId>,
    /// Model ids to install. Prompts when omitted.
    #[arg(long, num_args = 1..)]
    pub models: Vec<String>,
    /// Skip confirmation prompts (Cursor warnings are still printed).
    #[arg(short, long)]
    pub yes: bool,
    /// Show the changes without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}
