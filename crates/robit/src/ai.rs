use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::types::{ActionRequest, ActionSpec};

#[derive(Clone, Debug)]
pub enum AiDecision {
    Action(ActionRequest),
    NeedInput { prompt: String },
    Unknown { message: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum AiChatRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AiChatMessage {
    pub role: AiChatRole,
    pub content: String,
}

#[derive(Clone, Copy, Debug)]
pub enum AiProvider {
    OpenAI,
    DeepSeek,
}

#[derive(Clone, Debug)]
pub struct AiConfig {
    pub provider: AiProvider,
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
    pub temperature: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct AiClient {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    temperature: f64,
}

impl AiClient {
    pub fn new(config: AiConfig) -> Result<Self> {
        if config.api_key.trim().is_empty() {
            return Err(anyhow!("api key is empty"));
        }
        let base_url = match config.base_url {
            Some(url) => url,
            None => match config.provider {
                AiProvider::OpenAI => "https://api.openai.com/v1".to_string(),
                AiProvider::DeepSeek => "https://api.deepseek.com/v1".to_string(),
            },
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(25))
            .build()
            .context("failed to build http client")?;
        Ok(Self {
            client,
            api_key: config.api_key,
            base_url,
            model: config.model,
            temperature: config.temperature.unwrap_or(0.2),
        })
    }

    pub fn plan(&self, input: &str, actions: &[ActionSpec]) -> Result<AiDecision> {
        self.plan_with_history(input, actions, &[])
    }

    pub fn plan_with_history(
        &self,
        input: &str,
        actions: &[ActionSpec],
        history: &[AiChatMessage],
    ) -> Result<AiDecision> {
        let system = system_prompt();
        let action_specs = serde_json::to_string(actions).unwrap_or_else(|_| "[]".to_string());
        let user = format!(
            "User request:\n{input}\n\nAvailable actions (JSON):\n{action_specs}\n\nReturn JSON only.",
            input = input,
            action_specs = action_specs
        );
        let mut messages = Vec::with_capacity(2 + history.len());
        messages.push(json!({"role": "system", "content": system}));
        for message in history {
            let role = match message.role {
                AiChatRole::User => "user",
                AiChatRole::Assistant => "assistant",
            };
            messages.push(json!({"role": role, "content": message.content}));
        }
        messages.push(json!({"role": "user", "content": user}));
        let body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": self.temperature,
            "stream": false
        });
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .context("failed to send ai request")?;
        let status = resp.status();
        let value: Value = resp.json().context("failed to parse ai response")?;
        if !status.is_success() {
            return Err(anyhow!("ai http error {status}: {value}"));
        }
        let content = value
            .get("choices")
            .and_then(|v| v.get(0))
            .and_then(|v| v.get("message"))
            .and_then(|v| v.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        parse_decision(content, input)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct AiDecisionPayload {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
}

fn parse_decision(content: &str, raw_input: &str) -> Result<AiDecision> {
    let json_text = extract_json(content).unwrap_or_else(|| content.trim().to_string());
    let payload: AiDecisionPayload = serde_json::from_str(&json_text)
        .unwrap_or(AiDecisionPayload {
            r#type: "unknown".to_string(),
            name: None,
            action: None,
            params: None,
            message: Some("AI response was not valid JSON".to_string()),
            prompt: None,
        });

    let ty = payload.r#type.to_lowercase();
    if ty == "action" || payload.name.is_some() || payload.action.is_some() {
        let name = payload
            .name
            .or(payload.action)
            .ok_or_else(|| anyhow!("missing action name"))?;
        return Ok(AiDecision::Action(ActionRequest {
            name,
            params: payload.params.unwrap_or_else(|| json!({})),
            raw_input: raw_input.to_string(),
        }));
    }

    if ty == "need_input" {
        let prompt = payload
            .prompt
            .or(payload.message)
            .unwrap_or_else(|| "need more input".to_string());
        return Ok(AiDecision::NeedInput { prompt });
    }

    let message = payload
        .message
        .unwrap_or_else(|| "no plan".to_string());
    Ok(AiDecision::Unknown { message })
}

fn extract_json(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed.to_string());
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(trimmed[start..=end].to_string())
}

fn system_prompt() -> &'static str {
    "You are an action planner for robit.\n\
Return JSON only, no markdown, no extra text.\n\
Allowed output schemas:\n\
1) {\"type\":\"action\",\"name\":\"...\",\"params\":{...}}\n\
2) {\"type\":\"need_input\",\"prompt\":\"...\"}\n\
3) {\"type\":\"unknown\",\"message\":\"...\"}\n\
Pick an action only from the provided action list.\n\
Use conversation context to fill missing details.\n\
If the user mentions desktop/桌面, interpret as ~/Desktop."
}
