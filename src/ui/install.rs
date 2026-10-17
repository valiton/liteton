use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::ValueEnum;
use crossterm::style::Stylize;
use tokio::runtime::Runtime;

use super::{change_preview, format_cost, format_tokens};
use crate::cli::InstallArgs;
use crate::config::{self, Config, Credentials, InstallState};
use crate::harness::{self, Harness, HarnessId, HarnessPaths, InstallCtx, Plan, cursor, opencode};
use crate::litellm::budget::cheapest;
use crate::litellm::{Client, ModelSpec};
use crate::vscdb::{SecretWriter, StateDb};

pub fn install(rt: &Runtime, args: InstallArgs) -> Result<()> {
    cliclack::intro(" liteton install ")?;
    let mut config = Config::load()?;
    let creds = super::prompts::ensure_credentials(rt, &mut config, args.yes)?;
    install_with(rt, args, &mut config, creds)
}

/// The install flow once credentials are known; also continues a `liteton login`.
pub fn install_with(
    rt: &Runtime,
    args: InstallArgs,
    config: &mut Config,
    creds: Credentials,
) -> Result<()> {
    let paths = HarnessPaths::default();
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
    if selected.contains(&HarnessId::Opencode) || selected.contains(&HarnessId::Kilo) {
        decide_opencode_long_context(config, &chosen, args.yes)?;
    }
    if selected.contains(&HarnessId::Kilo) {
        decide_kilo_models(config, &chosen, args.yes)?;
    }

    let ctx = InstallCtx {
        creds: &creds,
        models: &chosen,
        config,
    };
    let recorded = state.clone();
    let plan = |id: HarnessId| {
        let harness = harness::get(&paths, id);
        let record = recorded
            .harnesses
            .get(id.key())
            .cloned()
            .or_else(|| harness.inherited_record(&recorded));
        harness.plan_install(&ctx, record.as_ref())
    };
    let mut plans = Vec::new();
    for id in &selected {
        match plan(*id) {
            Ok(plan) => plans.push(plan),
            Err(e) => cliclack::log::error(format!("{}: {e:#}", id.display_name()))?,
        }
    }
    if plans.is_empty() {
        cliclack::outro_cancel("Nothing to configure")?;
        return Ok(());
    }
    let applied = run_plans(plans, &mut state, args.yes, args.dry_run, &plan)?;
    if applied.is_empty() {
        return Ok(());
    }
    let hints: Vec<&str> = harnesses
        .iter()
        .filter(|h| applied.contains(&h.id()))
        .map(|h| h.hint())
        .collect();
    cliclack::outro(hints.join("\n"))?;
    Ok(())
}

pub fn uninstall(harness_ids: Vec<HarnessId>, yes: bool, dry_run: bool) -> Result<()> {
    cliclack::intro(" liteton uninstall ")?;
    let paths = HarnessPaths::default();
    let mut state = InstallState::load()?;
    let recorded: Vec<HarnessId> = HarnessId::value_variants()
        .iter()
        .copied()
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
    let records = state.harnesses.clone();
    let replan = |id: HarnessId| match records.get(id.key()) {
        Some(record) => harness::get(&paths, id).plan_uninstall(record),
        None => bail!("nothing installed by liteton"),
    };
    if !run_plans(plans, &mut state, yes, dry_run, &replan)?.is_empty() {
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
        .flat_map(|r| r.all_added_models())
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

/// opencode prices long prompts only from 200k on. Asks once whether later tiers may stand in,
/// and remembers the answer in config.toml.
fn decide_opencode_long_context(
    config: &mut Config,
    models: &[ModelSpec],
    yes: bool,
) -> Result<()> {
    let affected: Vec<(&str, u64)> = models
        .iter()
        .filter_map(|m| {
            opencode::approximated_tier(m).map(|tier| (m.id.as_str(), tier.above_tokens))
        })
        .collect();
    if affected.is_empty() {
        return Ok(());
    }
    let setting = format!(
        "{} in {}",
        "opencode_approximate_long_context".bold(),
        config::config_file().display()
    );
    match config.opencode_approximate_long_context {
        Some(true) => cliclack::log::remark(format!(
            "opencode 1 and Kilo Code: long-context rates are approximated from 200k on.\n{}",
            format!("Set by {setting}").dark_grey()
        ))?,
        Some(false) => {}
        None if yes => {}
        None => {
            let width = affected.iter().map(|(id, _)| id.len()).max().unwrap_or(0);
            let list: Vec<String> = affected
                .iter()
                .map(|(id, above)| {
                    format!(
                        "  {id:width$}  {}",
                        format!("above {}", format_tokens(Some(*above))).yellow()
                    )
                })
                .collect();
            let mut thresholds: Vec<u64> = affected.iter().map(|(_, above)| *above).collect();
            thresholds.sort_unstable();
            thresholds.dedup();
            let until = match thresholds.as_slice() {
                [only] => format_tokens(Some(*only)),
                _ => "each model's threshold".to_string(),
            };
            cliclack::log::warning(format!(
                "opencode 1 and Kilo Code can only price long prompts from 200k tokens on.\n\
                 These models switch to their long-context rate later:\n\
                 {}\n\n\
                 Yes: the long-context rate is shown from 200k on,\n\
                 \x20    overestimating prompts between 200k and {until}.\n\
                 No:  the normal rate is shown for every prompt.\n\
                 {}",
                list.join("\n"),
                "Either way, LiteLLM bills the real rate.".dark_grey()
            ))?;
            let approximate =
                cliclack::confirm("Approximate long-context pricing in opencode 1 and Kilo Code?")
                    .initial_value(false)
                    .interact()?;
            config.opencode_approximate_long_context = Some(approximate);
            config.save()?;
            cliclack::log::remark(format!("Saved. To change it later, edit {setting}"))?;
        }
    }
    Ok(())
}

fn decide_kilo_models(config: &mut Config, models: &[ModelSpec], yes: bool) -> Result<()> {
    let model = pick_model(
        "Default model for Kilo Code",
        config.kilo_model.as_deref(),
        models.first(),
        models,
        yes,
    )?;
    let small_model = pick_model(
        "Small model for Kilo Code (titles and summaries)",
        config.kilo_small_model.as_deref(),
        cheapest(models),
        models,
        yes,
    )?;
    if config.kilo_model.as_ref() != Some(&model)
        || config.kilo_small_model.as_ref() != Some(&small_model)
    {
        config.kilo_model = Some(model.clone());
        config.kilo_small_model = Some(small_model.clone());
        config.save()?;
    }
    cliclack::log::remark(format!(
        "Kilo Code: {model}, and {small_model} for titles and summaries.\n{}",
        format!(
            "Change them with {} and {} in {}",
            "kilo_model".bold(),
            "kilo_small_model".bold(),
            config::config_file().display()
        )
        .dark_grey()
    ))?;
    Ok(())
}

fn pick_model(
    prompt: &str,
    saved: Option<&str>,
    suggested: Option<&ModelSpec>,
    models: &[ModelSpec],
    yes: bool,
) -> Result<String> {
    if let Some(id) = saved.filter(|id| models.iter().any(|m| m.id == *id)) {
        return Ok(id.to_string());
    }
    let suggested = suggested.unwrap_or(&models[0]).id.clone();
    if yes {
        return Ok(suggested);
    }
    let mut select = cliclack::select(prompt).initial_value(suggested);
    for model in models {
        let price = format!(
            "{} / {} per 1M tokens",
            format_cost(model.pricing.input),
            format_cost(model.pricing.output)
        );
        select = select.item(model.id.clone(), &model.id, price);
    }
    Ok(select.interact()?)
}

/// Previews, confirms, closes apps if needed, backs up, applies and records each plan.
/// Returns the harnesses that were written. opencode 1 and 2 share a config file, so a plan
/// whose file an earlier plan just wrote is made again with `replan` on top of that write.
fn run_plans(
    plans: Vec<Plan>,
    state: &mut InstallState,
    yes: bool,
    dry_run: bool,
    replan: &dyn Fn(HarnessId) -> Result<Plan>,
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
    let mut written: Vec<PathBuf> = Vec::new();
    for plan in plans {
        let name = plan.harness.display_name();
        let secrets = match unlock_secrets(&plan) {
            Ok(secrets) => secrets,
            Err(e) => {
                cliclack::log::error(format!("{name}: {e:#}"))?;
                continue;
            }
        };
        match apply_or_replan(plan, &written, replan, &backup_dir, secrets.as_ref()) {
            Ok(plan) => {
                written.extend(plan.changes.iter().filter_map(|c| match c {
                    harness::Change::File { path, .. } => Some(path.clone()),
                    _ => None,
                }));
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

/// Applies `plan`, or a fresh one when an earlier plan of this run changed its file.
/// Returns the plan that was applied.
fn apply_or_replan(
    plan: Plan,
    written: &[PathBuf],
    replan: &dyn Fn(HarnessId) -> Result<Plan>,
    backup_dir: &Path,
    secrets: Option<&SecretWriter>,
) -> Result<Plan> {
    match harness::apply::apply(&plan, backup_dir, secrets) {
        Ok(()) => Ok(plan),
        Err(e)
            if e.downcast_ref::<harness::apply::StalePlan>()
                .is_some_and(|stale| written.contains(&stale.path)) =>
        {
            let fresh = replan(plan.harness)?;
            harness::apply::apply(&fresh, backup_dir, secrets)?;
            Ok(fresh)
        }
        Err(e) => Err(e),
    }
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
        .find_map(|c| match c {
            harness::Change::DbItem { db, .. } => Some(db.clone()),
            _ => None,
        })
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
