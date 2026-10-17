use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub struct CommandStep {
    pub args: Vec<String>,
    pub stdin: Option<String>,
}

impl CommandStep {
    pub fn new(args: &[&str]) -> Self {
        Self {
            args: args.iter().map(ToString::to_string).collect(),
            stdin: None,
        }
    }

    pub fn with_stdin(self, stdin: String) -> Self {
        Self {
            stdin: Some(stdin),
            ..self
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandBackup {
    pub step: CommandStep,
    pub file_name: String,
}

pub fn run(program: &Path, step: &CommandStep) -> Result<String> {
    let shown = format!("{} {}", program.display(), step.args.join(" "));
    let mut child = Command::new(program)
        .args(&step.args)
        .stdin(if step.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {shown}"))?;
    if let Some(input) = &step.stdin {
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(input.as_bytes())
            .with_context(|| format!("writing to {shown}"))?;
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("running {shown}"))?;
    if !output.status.success() {
        bail!(
            "{shown} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
