use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;

use crate::policy::ActionContext;
use crate::types::{ActionOutcome, ActionSpec, RiskLevel};
use crate::utils::{clean_path, expand_tilde};

#[derive(Default)]
pub struct ShellRunAction;

#[derive(Deserialize)]
struct ShellRunParams {
    command: String,
    cwd: Option<String>,
    dry_run: Option<bool>,
}

impl ShellRunAction {
    fn parse_params(&self, params: &serde_json::Value) -> Result<ShellRunParams> {
        serde_json::from_value(params.clone()).map_err(|err| anyhow!("invalid params: {err}"))
    }

    fn resolve_cwd(&self, ctx: &ActionContext, cwd: &Option<String>) -> Result<Option<PathBuf>> {
        let Some(raw) = cwd else {
            return Ok(None);
        };
        let path = clean_path(&expand_tilde(raw));
        ctx.policy.check_path_allowed(&path)?;
        if !path.exists() {
            return Err(anyhow!("cwd does not exist: {}", path.display()));
        }
        if !path.is_dir() {
            return Err(anyhow!("cwd is not a directory: {}", path.display()));
        }
        Ok(Some(path))
    }
}

impl crate::actions::ActionHandler for ShellRunAction {
    fn name(&self) -> &'static str {
        "shell.run"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "Run a shell command (macOS/Linux).".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "dry_run": { "type": "boolean" }
                },
                "required": ["command"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "exit_code": { "type": "integer" },
                    "stdout": { "type": "string" },
                    "stderr": { "type": "string" },
                    "truncated": { "type": "boolean" },
                    "dry_run": { "type": "boolean" }
                }
            }),
            risk: RiskLevel::High,
            requires_approval: true,
            capabilities: vec!["shell".to_string(), "process".to_string()],
        }
    }

    fn validate(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<()> {
        let params = self.parse_params(params)?;
        if params.command.trim().is_empty() {
            return Err(anyhow!("command cannot be empty"));
        }
        self.resolve_cwd(ctx, &params.cwd)?;
        Ok(())
    }

    fn execute(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<ActionOutcome> {
        let params = self.parse_params(params)?;
        let dry_run = ctx.dry_run || params.dry_run.unwrap_or(false);
        let cwd = self.resolve_cwd(ctx, &params.cwd)?;
        let command = params.command.trim().to_string();

        if dry_run {
            return Ok(ActionOutcome {
                summary: format!("dry run: would run `{}`", command),
                data: json!({
                    "command": command,
                    "cwd": cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
                    "exit_code": null,
                    "stdout": "",
                    "stderr": "",
                    "truncated": false,
                    "dry_run": true
                }),
            });
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-lc").arg(&command);
        if let Some(dir) = &cwd {
            cmd.current_dir(dir);
        }
        let output = cmd.output().map_err(|err| anyhow!("failed to run command: {err}"))?;
        let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let mut truncated = false;
        const LIMIT: usize = 4000;
        if stdout.len() > LIMIT {
            stdout.truncate(LIMIT);
            truncated = true;
        }
        if stderr.len() > LIMIT {
            stderr.truncate(LIMIT);
            truncated = true;
        }
        let exit_code = output.status.code().unwrap_or(-1);
        let summary = if output.status.success() {
            format!("command exited with {exit_code}")
        } else {
            format!("command failed with {exit_code}")
        };

        Ok(ActionOutcome {
            summary,
            data: json!({
                "command": command,
                "cwd": cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
                "truncated": truncated,
                "dry_run": false
            }),
        })
    }
}
