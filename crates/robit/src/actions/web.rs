use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::policy::ActionContext;
use crate::types::{ActionOutcome, ActionSpec, RiskLevel};

#[derive(Default)]
pub struct FetchUrlAction;

#[derive(Default)]
pub struct BraveSearchAction;

#[derive(Deserialize)]
struct FetchUrlParams {
    url: String,
    max_chars: Option<usize>,
}

#[derive(Deserialize)]
struct BraveSearchParams {
    query: String,
    api_key: String,
    count: Option<u32>,
}

impl FetchUrlAction {
    fn parse_params(&self, params: &serde_json::Value) -> Result<FetchUrlParams> {
        serde_json::from_value(params.clone()).map_err(|err| anyhow!("invalid params: {err}"))
    }
}

impl BraveSearchAction {
    fn parse_params(&self, params: &serde_json::Value) -> Result<BraveSearchParams> {
        serde_json::from_value(params.clone()).map_err(|err| anyhow!("invalid params: {err}"))
    }
}

impl crate::actions::ActionHandler for FetchUrlAction {
    fn name(&self) -> &'static str {
        "web.fetch_url"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "Fetch a URL via HTTP GET.".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "max_chars": { "type": "integer", "minimum": 1 }
                },
                "required": ["url"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "status": { "type": "integer" },
                    "content_type": { "type": "string" },
                    "body": { "type": "string" },
                    "truncated": { "type": "boolean" }
                }
            }),
            risk: RiskLevel::Medium,
            requires_approval: true,
            capabilities: vec!["network".to_string()],
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
        if ctx.dry_run {
            return Ok(ActionOutcome {
                summary: format!("dry run: would fetch {}", params.url),
                data: json!({
                    "url": params.url,
                    "status": null,
                    "content_type": null,
                    "body": "",
                    "truncated": false
                }),
            });
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("failed to build http client")?;
        let resp = client
            .get(&params.url)
            .send()
            .context("failed to fetch url")?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().unwrap_or_default();
        let max_chars = params.max_chars.unwrap_or(20_000).max(1);
        let truncated = body.chars().count() > max_chars;
        let out = if truncated {
            body.chars().take(max_chars).collect::<String>()
        } else {
            body
        };
        let summary = format!("fetched {} ({})", params.url, status.as_u16());

        Ok(ActionOutcome {
            summary,
            data: json!({
                "url": params.url,
                "status": status.as_u16(),
                "content_type": content_type,
                "body": out,
                "truncated": truncated
            }),
        })
    }
}

impl crate::actions::ActionHandler for BraveSearchAction {
    fn name(&self) -> &'static str {
        "web.search_brave"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "Search the web via Brave Search API.".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "api_key": { "type": "string" },
                    "count": { "type": "integer", "minimum": 1, "maximum": 20 }
                },
                "required": ["query", "api_key"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "results": { "type": "array" }
                }
            }),
            risk: RiskLevel::Medium,
            requires_approval: true,
            capabilities: vec!["network".to_string()],
        }
    }

    fn validate(&self, _ctx: &ActionContext, params: &serde_json::Value) -> Result<()> {
        let params = self.parse_params(params)?;
        if params.query.trim().is_empty() {
            return Err(anyhow!("query cannot be empty"));
        }
        if params.api_key.trim().is_empty() {
            return Err(anyhow!("api_key cannot be empty"));
        }
        Ok(())
    }

    fn execute(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<ActionOutcome> {
        let params = self.parse_params(params)?;
        if ctx.dry_run {
            return Ok(ActionOutcome {
                summary: format!("dry run: would search brave for '{}'", params.query),
                data: json!({
                    "query": params.query,
                    "results": []
                }),
            });
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("failed to build http client")?;
        let mut url = reqwest::Url::parse("https://api.search.brave.com/res/v1/web/search")
            .map_err(|err| anyhow!("invalid brave url: {err}"))?;
        url.query_pairs_mut()
            .append_pair("q", &params.query);
        if let Some(count) = params.count {
            url.query_pairs_mut()
                .append_pair("count", &count.to_string());
        }
        let resp = client
            .get(url)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", params.api_key)
            .send()
            .context("failed to call brave search")?;
        let status = resp.status();
        let value: serde_json::Value = resp.json().unwrap_or_else(|_| json!({}));
        if !status.is_success() {
            return Err(anyhow!("brave search error {status}: {value}"));
        }
        let results = value
            .get("web")
            .and_then(|v| v.get("results"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(ActionOutcome {
            summary: format!("brave search ok ({})", results.len()),
            data: json!({
                "query": params.query,
                "results": results
            }),
        })
    }
}
