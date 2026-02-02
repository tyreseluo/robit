use std::process::Command;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;

use crate::policy::ActionContext;
use crate::types::{ActionOutcome, ActionSpec, RiskLevel};

#[derive(Default)]
pub struct BrowserOpenUrlAction;

#[derive(Deserialize)]
struct BrowserOpenParams {
    url: String,
    app: Option<String>,
    dry_run: Option<bool>,
}

impl BrowserOpenUrlAction {
    fn parse_params(&self, params: &serde_json::Value) -> Result<BrowserOpenParams> {
        serde_json::from_value(params.clone()).map_err(|err| anyhow!("invalid params: {err}"))
    }
}

impl crate::actions::ActionHandler for BrowserOpenUrlAction {
    fn name(&self) -> &'static str {
        "browser.open_url"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "Open a URL in a browser (macOS `open`).".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "app": { "type": "string" },
                    "dry_run": { "type": "boolean" }
                },
                "required": ["url"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "app": { "type": "string" },
                    "dry_run": { "type": "boolean" }
                }
            }),
            risk: RiskLevel::Medium,
            requires_approval: true,
            capabilities: vec!["browser".to_string()],
        }
    }

    fn validate(&self, _ctx: &ActionContext, params: &serde_json::Value) -> Result<()> {
        let params = self.parse_params(params)?;
        if params.url.trim().is_empty() {
            return Err(anyhow!("url cannot be empty"));
        }
        Ok(())
    }

    fn execute(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<ActionOutcome> {
        let params = self.parse_params(params)?;
        let dry_run = ctx.dry_run || params.dry_run.unwrap_or(false);
        let url = params.url.trim().to_string();
        let app = params.app.unwrap_or_else(|| "Google Chrome".to_string());

        if dry_run {
            return Ok(ActionOutcome {
                summary: format!("dry run: would open {url} in {app}"),
                data: json!({
                    "url": url,
                    "app": app,
                    "dry_run": true
                }),
            });
        }

        let status = Command::new("open")
            .arg("-a")
            .arg(&app)
            .arg(&url)
            .status()
            .map_err(|err| anyhow!("failed to open browser: {err}"))?;

        if !status.success() {
            return Err(anyhow!("open command failed"));
        }

        Ok(ActionOutcome {
            summary: format!("opened {url} in {app}"),
            data: json!({
                "url": url,
                "app": app,
                "dry_run": false
            }),
        })
    }
}
