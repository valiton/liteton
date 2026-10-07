use std::collections::BTreeSet;

use anyhow::{Result, bail};
use crossterm::style::Stylize;
use tokio::runtime::Runtime;

use super::{change_preview, format_cost, format_tokens};
use crate::cli::InstallArgs;
use crate::config::{self, Config, InstallState};
use crate::harness::{self, Harness, HarnessId, HarnessPaths, InstallCtx, Plan, cursor};
use crate::litellm::{Client, ModelSpec};
use crate::vscdb::{SecretWriter, StateDb};

pub fn install(rt: &Runtime, args: InstallArgs) -> Result<()> {
    cliclack::intro(" liteton install ")?;
    let paths = HarnessPaths::default();
    let mut config = Config::load()?;
    let creds = super::prompts::ensure_credentials(rt, &mut config, args.yes)?;

    let spinner = cliclack::spinner();
    spinner.start("Fetching models from LiteLLM");
    let models = match rt.block_on(Client::new(&creds)?.models()) {
        Ok(models) if models.is_empty() => {
            spinner.error("LiteLLM returned no chat models for this key");
            bail!("no models available");
        }
        Ok(models) => {
            spinner.stop(format!("{} models available", models.len()));
            models
        }
        Err(e) => {
            spinner.error(format!("{e:#}"));
            return Err(e);
        }
    };

    let mut state = InstallState::load()?;
    let harnesses = harness::all(&paths);
    let mut selected = select_harnesses(&harnesses, &state, &args)?;
    if selected.is_empty() {
        cliclack::outro_cancel("No harness selected")?;
        return Ok(());
    }
    let chosen = select_models(&models, &state, &args)?;
    if chosen.is_empty() {
        cliclack::outro_cancel("No model selected")?;
        return Ok(());
    }

    if selected.contains(&HarnessId::Cursor) && !confirm_cursor(&creds.base_url, args.yes)? {
        selected.retain(|id| *id != HarnessId::Cursor);
    }

    let ctx = InstallCtx {
        creds: &creds,
        models: &chosen,
        config: &config,
    };
    let mut plans = Vec::new();
    for id in &selected {
        let harness = harness::get(&paths, *id);
        match harness.plan_install(&ctx, state.harnesses.get(id.key())) {
            Ok(plan) => plans.push(plan),
            Err(e) => cliclack::log::error(format!("{}: {e:#}", id.display_name()))?,
        }
    }
    if plans.is_empty() {
        cliclack::outro_cancel("Nothing to configure")?;
        return Ok(());
    }
    let applied = run_plans(plans, &mut state, args.yes, args.dry_run)?;
    if applied.is_empty() {
        return Ok(());
    }
    let mut hints = Vec::new();
    for id in &applied {
        hints.push(match id {
            HarnessId::Vscode => {
                "VSCode: open the chat model picker; the models are listed under \"litellm\"."
            }
            HarnessId::Opencode => {
                "opencode: run /models; the models are listed under \"LiteLLM\"."
            }
            HarnessId::Cursor => "Cursor: the models are enabled in Settings > Models.",
        });
    }
    cliclack::outro(hints.join("\n"))?;
    Ok(())
}

pub fn uninstall(harness_ids: Vec<HarnessId>, yes: bool, dry_run: bool) -> Result<()> {
    cliclack::intro(" liteton uninstall ")?;
    let paths = HarnessPaths::default();
    let mut state = InstallState::load()?;
    let recorded: Vec<HarnessId> = [HarnessId::Vscode, HarnessId::Opencode, HarnessId::Cursor]
        .into_iter()
        .filter(|id| state.harnesses.contains_key(id.key()))
        .collect();
    if recorded.is_empty() {
        cliclack::outro("liteton has not installed anything yet")?;
        return Ok(());
    }
    let selected = if !harness_ids.is_empty() {
        harness_ids
    } else if yes {
        recorded.clone()
    } else {
        let mut prompt = cliclack::multiselect("Remove liteton's configuration from")
            .initial_values(recorded.clone());
        for id in &recorded {
            prompt = prompt.item(*id, id.display_name(), "");
        }
        prompt.interact()?
    };

    let mut plans = Vec::new();
    for id in selected {
        let Some(record) = state.harnesses.get(id.key()) else {
            cliclack::log::remark(format!(
                "{}: nothing installed by liteton",
                id.display_name()
            ))?;
            continue;
        };
        match harness::get(&paths, id).plan_uninstall(record) {
            Ok(plan) => plans.push(plan),
            Err(e) => cliclack::log::error(format!("{}: {e:#}", id.display_name()))?,
        }
    }
    if plans.is_empty() {
        cliclack::outro_cancel("Nothing to remove")?;
        return Ok(());
    }
    if !run_plans(plans, &mut state, yes, dry_run)?.is_empty() {
        cliclack::outro("Removed")?;
    }
    Ok(())
}

fn select_harnesses(
    harnesses: &[Box<dyn Harness>],
    state: &InstallState,
    args: &InstallArgs,
) -> Result<Vec<HarnessId>> {
    let preselect = |h: &dyn Harness| {
        h.id() != HarnessId::Cursor && (h.detect() || state.harnesses.contains_key(h.id().key()))
    };
    if !args.harness.is_empty() {
        return Ok(args.harness.clone());
    }
    if args.yes {
        return Ok(harnesses
            .iter()
            .filter(|h| preselect(h.as_ref()))
            .map(|h| h.id())
            .collect());
    }
    let mut prompt = cliclack::multiselect("Which harnesses should use LiteLLM?")
        .initial_values(
            harnesses
                .iter()
                .filter(|h| preselect(h.as_ref()))
                .map(|h| h.id())
                .collect(),
        )
        .required(false);
    for h in harnesses {
        let mut hint = vec![if h.detect() { "detected" } else { "not found" }];
        if h.is_installed() {
            hint.push("configured");
        }
        if h.id() == HarnessId::Cursor {
            hint.push("experimental, needs a public URL");
        }
        prompt = prompt.item(h.id(), h.id().display_name(), hint.join(", "));
    }
    Ok(prompt.interact()?)
}

fn select_models(
    models: &[ModelSpec],
    state: &InstallState,
    args: &InstallArgs,
) -> Result<Vec<ModelSpec>> {
    if !args.models.is_empty() {
        let unknown: Vec<&String> = args
            .models
            .iter()
            .filter(|id| !models.iter().any(|m| &&m.id == id))
            .collect();
        if !unknown.is_empty() {
            bail!(
                "unknown model(s): {}",
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        return Ok(models
            .iter()
            .filter(|m| args.models.contains(&m.id))
            .cloned()
            .collect());
    }
    let previous: BTreeSet<&String> = state
        .harnesses
        .values()
        .flat_map(|r| &r.added_models)
        .collect();
    let initial: Vec<String> = if previous.is_empty() {
        models.iter().map(|m| m.id.clone()).collect()
    } else {
        models
            .iter()
            .filter(|m| previous.contains(&m.id))
            .map(|m| m.id.clone())
            .collect()
    };
    if args.yes {
        return Ok(models
            .iter()
            .filter(|m| initial.contains(&m.id))
            .cloned()
            .collect());
    }
    let mut prompt = cliclack::multiselect("Which models?")
        .initial_values(initial)
        .required(false)
        .max_rows(15);
    if models.len() > 15 {
        prompt = prompt.filter_mode();
    }
    for m in models {
        let mut hint = format!(
            "{} in · {} out per 1M",
            format_cost(m.pricing.input),
            format_cost(m.pricing.output)
        );
        if let Some(tier) = m.long_context()
            && tier.pricing.input.is_some()
        {
            hint.push_str(&format!(
                " · {} in >{}",
                format_cost(tier.pricing.input),
                format_tokens(Some(tier.above_tokens))
            ));
        }
        if m.context_window.is_some() {
            hint.push_str(&format!(" · {} ctx", format_tokens(m.context_window)));
        }
        if m.reasoning {
            hint.push_str(" · reasoning");
        }
        prompt = prompt.item(m.id.clone(), &m.id, hint);
    }
    let ids = prompt.interact()?;
    Ok(models
        .iter()
        .filter(|m| ids.contains(&m.id))
        .cloned()
        .collect())
}

/// Cursor is opt-in: show why it may not work, refuse URLs it can never reach, then ask.
fn confirm_cursor(base_url: &str, yes: bool) -> Result<bool> {
    for warning in cursor::WARNINGS {
        cliclack::log::warning(warning)?;
    }
    if let Err(e) = cursor::ensure_public_url(base_url) {
        cliclack::log::error(format!("Skipping Cursor: {e:#}"))?;
        return Ok(false);
    }
    if yes {
        return Ok(true);
    }
    Ok(cliclack::confirm("Configure Cursor anyway?")
        .initial_value(false)
        .interact()?)
}

/// Previews, confirms, closes apps if needed, backs up, applies and records each plan.
/// Returns the harnesses that were written.
fn run_plans(
    plans: Vec<Plan>,
    state: &mut InstallState,
    yes: bool,
    dry_run: bool,
) -> Result<Vec<HarnessId>> {
    let mut plans: Vec<Plan> = plans
        .into_iter()
        .filter(|p| {
            !p.changes.is_empty() || p.record.as_ref() != state.harnesses.get(p.harness.key())
        })
        .collect();
    if plans.is_empty() {
        cliclack::outro("Everything is already up to date")?;
        return Ok(vec![]);
    }
    for plan in &plans {
        let body: String = if plan.changes.is_empty() {
            "no file changes".dark_grey().to_string()
        } else {
            plan.changes
                .iter()
                .map(change_preview)
                .collect::<Vec<_>>()
                .join("\n")
        };
        cliclack::note(plan.harness.display_name(), body.trim_end())?;
        for note in &plan.notes {
            cliclack::log::remark(note)?;
        }
    }
    if dry_run {
        cliclack::outro("Dry run, nothing was written")?;
        return Ok(vec![]);
    }
    if !yes
        && !cliclack::confirm("Apply these changes?")
            .initial_value(true)
            .interact()?
    {
        cliclack::outro_cancel("Nothing was written")?;
        return Ok(vec![]);
    }

    plans.retain(|plan| match ensure_app_closed(plan, yes) {
        Ok(true) => true,
        Ok(false) => false,
        Err(e) => {
            let _ = cliclack::log::error(format!("{}: {e:#}", plan.harness.display_name()));
            false
        }
    });

    let backup_dir =
        config::backups_dir().join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
    let mut applied = Vec::new();
    for plan in &plans {
        let name = plan.harness.display_name();
        let secrets = match unlock_secrets(plan) {
            Ok(secrets) => secrets,
            Err(e) => {
                cliclack::log::error(format!("{name}: {e:#}"))?;
                continue;
            }
        };
        match harness::apply::apply(plan, &backup_dir, secrets.as_ref()) {
            Ok(()) => {
                match &plan.record {
                    Some(record) => state
                        .harnesses
                        .insert(plan.harness.key().to_string(), record.clone()),
                    None => state.harnesses.remove(plan.harness.key()),
                };
                state.save()?;
                cliclack::log::success(format!("{name} updated"))?;
                applied.push(plan.harness);
            }
            Err(e) => cliclack::log::error(format!("{name}: {e:#}"))?,
        }
    }
    if backup_dir.exists() {
        cliclack::log::info(format!("Backups saved in {}", backup_dir.display()))?;
    }
    Ok(applied)
}

/// The app keeps its database in memory and would overwrite our edits, so it must not run.
fn ensure_app_closed(plan: &Plan, yes: bool) -> Result<bool> {
    let Some(app) = plan.electron_app() else {
        return Ok(true);
    };
    if !app.is_running() {
        return Ok(true);
    }
    if app.is_parent_terminal() {
        bail!(
            "{} must be closed to write its settings, but liteton is running inside it. Run liteton from another terminal (e.g. Terminal.app).",
            app.name()
        );
    }
    if !yes
        && !cliclack::confirm(format!(
            "{} must be closed while liteton writes its settings. Quit it now?",
            app.name()
        ))
        .initial_value(true)
        .interact()?
    {
        cliclack::log::remark(format!("Skipping {}", plan.harness.display_name()))?;
        return Ok(false);
    }
    let spinner = cliclack::spinner();
    spinner.start(format!("Quitting {}", app.name()));
    match app.quit() {
        Ok(()) => {
            spinner.stop(format!("{} closed", app.name()));
            Ok(true)
        }
        Err(e) => {
            spinner.error(format!("{e:#}"));
            Err(e)
        }
    }
}

fn unlock_secrets(plan: &Plan) -> Result<Option<SecretWriter>> {
    if !plan.writes_secrets() {
        return Ok(None);
    }
    let Some(app) = plan.electron_app() else {
        return Ok(None);
    };
    let db_path = plan
        .changes
        .iter()
        .find(|c| matches!(c, harness::Change::DbItem { .. }))
        .map(|c| c.path().to_path_buf())
        .unwrap();
    cliclack::log::info(format!(
        "macOS may ask you to allow access to \"{}\" in the Keychain; choose Allow.",
        app.keychain_item().0
    ))?;
    let writer = app.unlock_secrets(&StateDb::open_readonly(&db_path)?)?;
    if !writer.verified {
        cliclack::log::warning(format!(
            "{} has no saved secrets yet, so the encryption could not be double-checked.",
            app.name()
        ))?;
    }
    Ok(Some(writer))
}
