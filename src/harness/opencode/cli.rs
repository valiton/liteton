use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{CredentialPlan, CredentialStore, V2, Version};
use crate::config::{HarnessRecord, key_fingerprint};
use crate::harness::{Change, CommandBackup, CommandStep, PROVIDER_ID, command};

/// The credential liteton owns in opencode 2's credential store.
pub const CREDENTIAL_ID: &str = "cred_liteton_litellm";

const LABEL: &str = "opencode credentials";

/// The `opencode` binary. opencode 1 and 2 both ship one by that name.
pub struct OpencodeCli {
    bin: PathBuf,
    major: OnceCell<Option<u32>>,
}

impl OpencodeCli {
    pub fn new(bin: &Path) -> Self {
        Self {
            bin: bin.to_path_buf(),
            major: OnceCell::new(),
        }
    }

    pub fn major(&self) -> Option<u32> {
        *self.major.get_or_init(|| {
            let out = command::run(&self.bin, &CommandStep::new(&["--version"])).ok()?;
            out.split_whitespace()
                .find_map(|word| word.trim_start_matches('v').split('.').next()?.parse().ok())
        })
    }

    fn require(&self, major: u32) -> Result<()> {
        match self.major() {
            Some(found) if found == major => Ok(()),
            Some(found) => bail!(
                "`{}` is opencode {found}; opencode {major} is needed to store the API key",
                self.bin.display()
            ),
            None => bail!("opencode {major} was not found on PATH"),
        }
    }

    /// Ids of the stored credentials for `integration`. `auth list` never prints secrets.
    fn credential_ids(&self, integration: &str) -> Result<Vec<String>> {
        let out = command::run(&self.bin, &auth(&["list", "--format", "json"]))?;
        let list: Value =
            serde_json::from_str(&out).context("parsing `opencode auth list --format json`")?;
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["id"] == integration)
            .flat_map(|i| i["connections"].as_array().into_iter().flatten())
            .filter_map(|c| c["id"].as_str().map(str::to_string))
            .collect())
    }

    fn change(&self, summary: Vec<String>, steps: Vec<CommandStep>, backup: bool) -> Change {
        Change::Command {
            label: LABEL.into(),
            summary,
            program: self.bin.clone(),
            steps,
            backup: backup.then(|| CommandBackup {
                step: auth(&["export", PROVIDER_ID]),
                file_name: "opencode-credentials.json".into(),
            }),
        }
    }
}

fn auth(args: &[&str]) -> CommandStep {
    CommandStep::new(&[&["auth"], args].concat())
}

/// opencode 2 keeps credentials in its database (auth.json is only imported once), so the key
/// goes in through `opencode auth`. liteton adds its own credential and makes it active.
pub struct CliCredentials {
    cli: Rc<OpencodeCli>,
}

impl CliCredentials {
    pub fn new(cli: Rc<OpencodeCli>) -> Self {
        Self { cli }
    }

    fn removal(&self) -> Result<Option<Change>> {
        self.cli.require(V2::CLI_MAJOR)?;
        if !self
            .cli
            .credential_ids(PROVIDER_ID)?
            .iter()
            .any(|id| id == CREDENTIAL_ID)
        {
            return Ok(None);
        }
        Ok(Some(self.cli.change(
            vec!["remove the \"liteton\" LiteLLM credential (other credentials are kept)".into()],
            vec![auth(&["logout", PROVIDER_ID, CREDENTIAL_ID])],
            true,
        )))
    }
}

impl CredentialStore for CliCredentials {
    fn plan_set(&self, api_key: &str, record: Option<&HarnessRecord>) -> Result<CredentialPlan> {
        self.cli.require(V2::CLI_MAJOR)?;
        let existing = self.cli.credential_ids(PROVIDER_ID)?;
        let ours = existing.iter().any(|id| id == CREDENTIAL_ID);
        let fingerprint = key_fingerprint(api_key);
        if ours && record.and_then(|r| r.key_fingerprint.as_ref()) == Some(&fingerprint) {
            return Ok(CredentialPlan {
                fingerprint: Some(fingerprint),
                ..CredentialPlan::default()
            });
        }
        let mut steps = Vec::new();
        let mut summary = Vec::new();
        if ours {
            steps.push(auth(&["logout", PROVIDER_ID, CREDENTIAL_ID]));
            summary.push("replace the API key of the \"liteton\" LiteLLM credential".to_string());
        } else {
            summary.push("add a LiteLLM credential \"liteton\" with the API key".to_string());
        }
        let credential = json!([{
            "id": CREDENTIAL_ID,
            "integrationID": PROVIDER_ID,
            "label": "liteton",
            "active": true,
            "value": {"type": "key", "key": api_key},
        }]);
        steps.push(auth(&["import"]).with_stdin(credential.to_string()));
        steps.push(auth(&["switch", PROVIDER_ID, CREDENTIAL_ID]));
        if existing.iter().any(|id| id != CREDENTIAL_ID) {
            summary.push("make it the active LiteLLM credential (the others are kept)".to_string());
        }
        Ok(CredentialPlan {
            change: Some(self.cli.change(summary, steps, !existing.is_empty())),
            created: (!ours).then(|| CREDENTIAL_ID.to_string()),
            fingerprint: Some(fingerprint),
            note: None,
        })
    }

    /// Without a working opencode 2 CLI, uninstall still goes ahead and says what is left.
    fn plan_remove(&self) -> Result<CredentialPlan> {
        Ok(match self.removal() {
            Ok(change) => CredentialPlan {
                change,
                ..CredentialPlan::default()
            },
            Err(e) => CredentialPlan {
                note: Some(format!(
                    "{e:#}. Remove the \"liteton\" credential later with `opencode auth logout {PROVIDER_ID} {CREDENTIAL_ID}`."
                )),
                ..CredentialPlan::default()
            },
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::harness::HarnessPaths;
    use crate::harness::tests::paths_in;

    /// Stands in for the `opencode` CLI: logs every call and stdin, answers `auth list` from list.json.
    pub fn fake_cli(paths: &HarnessPaths, list: &str) {
        let bin = &paths.opencode_bin;
        let dir = bin.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("list.json"), list).unwrap();
        std::fs::write(
            bin,
            r#"#!/bin/sh
dir=$(dirname "$0")
echo "$@" >> "$dir/calls.log"
case "$1 $2" in
  "--version "*) echo "opencode v2.0.29" ;;
  "auth list") cat "$dir/list.json" ;;
  "auth import") cat >> "$dir/stdin.log" ;;
  "auth export") echo '[{"id": "x"}]' ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn calls(paths: &HarnessPaths) -> String {
        std::fs::read_to_string(paths.opencode_bin.with_file_name("calls.log")).unwrap_or_default()
    }

    #[test]
    fn reads_versions_and_credential_ids() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        fake_cli(
            &paths,
            r#"[{"id": "openai", "connections": [{"id": "cred_oa"}]},
                {"id": "litellm", "connections": [{"id": "cred_a"}, {"id": "cred_b"}]}]"#,
        );
        let cli = OpencodeCli::new(&paths.opencode_bin);
        assert_eq!(cli.major(), Some(2));
        assert_eq!(cli.major(), Some(2));
        assert_eq!(
            calls(&paths).lines().filter(|l| *l == "--version").count(),
            1,
            "the version is asked once"
        );
        assert_eq!(
            cli.credential_ids(PROVIDER_ID).unwrap(),
            ["cred_a", "cred_b"]
        );
        assert!(cli.require(1).is_err());
    }

    #[test]
    fn uninstall_without_the_cli_leaves_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let store = CliCredentials::new(Rc::new(OpencodeCli::new(&paths.opencode_bin)));
        let plan = store.plan_remove().unwrap();
        assert!(plan.change.is_none());
        assert!(plan.note.unwrap().contains("opencode auth logout litellm"));
    }
}
