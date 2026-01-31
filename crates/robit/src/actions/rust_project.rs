use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::policy::ActionContext;
use crate::types::{ActionOutcome, ActionSpec, RiskLevel};
use crate::utils::{clean_path, expand_tilde};

#[derive(Default)]
pub struct RustProjectAction;

#[derive(Deserialize)]
struct RustProjectParams {
    path: String,
    name: String,
    run: Option<bool>,
    message: Option<String>,
}

impl RustProjectAction {
    fn parse_params(&self, params: &serde_json::Value) -> Result<RustProjectParams> {
        serde_json::from_value(params.clone()).map_err(|err| anyhow!("invalid params: {err}"))
    }

    fn ensure_base_dir(&self, base: &PathBuf, dry_run: bool) -> Result<()> {
        if base.exists() {
            if base.is_dir() {
                return Ok(());
            }
            return Err(anyhow!("path is not a directory: {}", base.display()));
        }
        if dry_run {
            return Ok(());
        }
        fs::create_dir_all(base)
            .with_context(|| format!("failed to create directory: {}", base.display()))
    }
}

impl crate::actions::ActionHandler for RustProjectAction {
    fn name(&self) -> &'static str {
        "rust.new_project"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "Create a new Rust project and set main.rs to print a message.".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "name": { "type": "string" },
                    "run": { "type": "boolean" },
                    "message": { "type": "string" }
                },
                "required": ["path", "name"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "project_dir": { "type": "string" },
                    "ran": { "type": "boolean" },
                    "stdout": { "type": "string" }
                }
            }),
            risk: RiskLevel::Medium,
            requires_approval: true,
            capabilities: vec!["filesystem".to_string(), "process".to_string()],
        }
    }

    fn validate(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<()> {
        let params = self.parse_params(params)?;
        let base = expand_tilde(&params.path);
        ctx.policy.check_path_allowed(&base)?;
        if params.name.trim().is_empty() {
            return Err(anyhow!("project name is required"));
        }
        Ok(())
    }

    fn execute(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<ActionOutcome> {
        let params = self.parse_params(params)?;
        let base = clean_path(&expand_tilde(&params.path));
        let project_name = params.name.trim();
        let run = params.run.unwrap_or(false);
        let message = params
            .message
            .unwrap_or_else(|| "hello world".to_string());
        let dry_run = ctx.dry_run;

        self.ensure_base_dir(&base, dry_run)?;
        let project_dir = base.join(project_name);

        if project_dir.exists() {
            return Err(anyhow!("project already exists: {}", project_dir.display()));
        }

        if !dry_run {
            let status = Command::new("cargo")
                .arg("new")
                .arg(project_name)
                .current_dir(&base)
                .status()
                .context("failed to run cargo new")?;
            if !status.success() {
                return Err(anyhow!("cargo new failed"));
            }

            let main_path = project_dir.join("src").join("main.rs");
            let main_body = format!("fn main() {{\n    println!(\"{}\");\n}}\n", message);
            fs::write(&main_path, main_body)
                .with_context(|| format!("failed to write {}", main_path.display()))?;
        }

        let mut stdout = String::new();
        if run && !dry_run {
            let output = Command::new("cargo")
                .arg("run")
                .current_dir(&project_dir)
                .output()
                .context("failed to run cargo run")?;
            stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !output.status.success() {
                return Err(anyhow!("cargo run failed"));
            }
        }

        let summary = if dry_run {
            format!(
                "dry run: would create rust project '{}' in {}",
                project_name,
                base.display()
            )
        } else if run {
            format!(
                "created rust project '{}' in {} and ran it",
                project_name,
                base.display()
            )
        } else {
            format!(
                "created rust project '{}' in {}",
                project_name,
                base.display()
            )
        };

        Ok(ActionOutcome {
            summary,
            data: json!({
                "project_dir": project_dir.to_string_lossy(),
                "ran": run,
                "stdout": stdout,
            }),
        })
    }
}
