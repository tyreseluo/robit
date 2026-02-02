use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::types::{ActionRequest, ActionSpec, PlanStep};

#[derive(Clone, Debug)]
pub enum AiDecision {
    Action(ActionRequest),
    NeedInput {
        prompt: String,
        action: Option<String>,
        params: Value,
        missing: Vec<String>,
    },
    Chat { message: String },
    Plan { steps: Vec<PlanStep>, message: Option<String> },
    Unknown { message: String },
}

pub trait AiPlanner: Send + Sync {
    fn plan_with_history(
        &self,
        input: &str,
        actions: &[ActionSpec],
        history: &[AiChatMessage],
    ) -> Result<AiDecision>;
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

#[cfg(feature = "ai-http")]
use reqwest::blocking::Client;

#[cfg(feature = "ai-http")]
#[derive(Clone, Copy, Debug)]
pub enum AiProvider {
    OpenAI,
    DeepSeek,
}

#[cfg(feature = "ai-http")]
#[derive(Clone, Debug)]
pub struct AiConfig {
    pub provider: AiProvider,
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
    pub temperature: Option<f64>,
}

#[cfg(feature = "ai-http")]
#[derive(Clone, Debug)]
pub struct AiClient {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    temperature: f64,
}

#[cfg(feature = "ai-http")]
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
        let system = system_prompt_with_backend(Some(&self.model));
        let action_specs = serde_json::to_string(actions).unwrap_or_else(|_| "[]".to_string());
        let user = format!(
            "{system}\n\nUser request:\n{input}\n\nAvailable actions (JSON):\n{action_specs}\n\nReturn JSON only.",
            system = system,
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

    pub fn model_name(&self) -> &str {
        &self.model
    }
}

#[cfg(feature = "ai-http")]
impl AiPlanner for AiClient {
    fn plan_with_history(
        &self,
        input: &str,
        actions: &[ActionSpec],
        history: &[AiChatMessage],
    ) -> Result<AiDecision> {
        AiClient::plan_with_history(self, input, actions, history)
    }
}

#[cfg(feature = "ai-omnix-mlx")]
mod omnix {
    use super::{
        parse_decision, system_prompt_with_backend, AiChatMessage, AiChatRole, AiDecision,
        AiPlanner, ActionSpec,
    };
    use anyhow::{anyhow, Context, Result};
    use mlx_lm_utils::tokenizer::{
        load_model_chat_template_from_file, ApplyChatTemplateArgs, Conversation, Role, Tokenizer,
    };
    use mlx_rs::ops::indexing::{IndexOp, NewAxis};
    use mlx_rs::transforms::eval;
    use mlx_rs::Array;
    use qwen3_mlx::{load_model, Generate, KVCache, Model};
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[derive(Clone, Debug)]
    pub struct MlxQwenConfig {
        pub model_dir: PathBuf,
        pub temperature: f32,
        pub max_tokens: usize,
    }

    pub struct MlxQwenClient {
        model: Mutex<Model>,
        tokenizer: Mutex<Tokenizer>,
        chat_template: String,
        model_id: String,
        temperature: f32,
        max_tokens: usize,
    }

    impl MlxQwenClient {
        pub fn new(config: MlxQwenConfig) -> Result<Self> {
            let model_dir = config.model_dir;
            if !model_dir.exists() {
                return Err(anyhow!(
                    "model dir not found: {}",
                    model_dir.display()
                ));
            }
            for required in [
                "config.json",
                "model.safetensors.index.json",
                "tokenizer.json",
                "tokenizer_config.json",
            ] {
                let path = model_dir.join(required);
                if !path.is_file() {
                    return Err(anyhow!(
                        "missing required model file: {}",
                        path.display()
                    ));
                }
            }
            let model_id = model_dir
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("qwen3")
                .to_string();

            let tokenizer_file = model_dir.join("tokenizer.json");
            let tokenizer_config_file = model_dir.join("tokenizer_config.json");
            let tokenizer = Tokenizer::from_file(&tokenizer_file)
                .map_err(|err| anyhow!("failed to load tokenizer: {err:?}"))?;
            let chat_template = load_model_chat_template_from_file(&tokenizer_config_file)?
                .ok_or_else(|| anyhow!("chat template not found in tokenizer_config.json"))?;

            let model = load_model(&model_dir).context("failed to load qwen3 model")?;

            Ok(Self {
                model: Mutex::new(model),
                tokenizer: Mutex::new(tokenizer),
                chat_template,
                model_id,
                temperature: config.temperature,
                max_tokens: config.max_tokens,
            })
        }

        fn build_conversation(
            &self,
            input: &str,
            actions_json: &str,
            history: &[AiChatMessage],
        ) -> Vec<Conversation<Role, String>> {
            let mut conversations = Vec::new();
            for message in history {
                let role = match message.role {
                    AiChatRole::User => Role::User,
                    AiChatRole::Assistant => Role::Assistant,
                };
                conversations.push(Conversation {
                    role,
                    content: message.content.clone(),
                });
            }
            let system = system_prompt_with_backend(Some(&self.model_id));
            let user = format!(
                "{system}\n\nUser request:\n{input}\n\nAvailable actions (JSON):\n{actions_json}\n\nReturn JSON only.",
                system = system,
                input = input,
                actions_json = actions_json
            );
            conversations.push(Conversation {
                role: Role::User,
                content: user,
            });
            conversations
        }

        fn encode_prompt(
            &self,
            conversations: Vec<Conversation<Role, String>>,
        ) -> Result<Array> {
            let mut tokenizer = self.tokenizer.lock().unwrap();
            let args = ApplyChatTemplateArgs {
                conversations: vec![conversations.into()],
                documents: None,
                model_id: &self.model_id,
                chat_template_id: None,
                add_generation_prompt: None,
                continue_final_message: None,
            };
            let encodings = tokenizer
                .apply_chat_template_and_encode(self.chat_template.clone(), args)
                .context("failed to apply chat template")?;
            let prompt: Vec<u32> = encodings
                .iter()
                .flat_map(|encoding| encoding.get_ids())
                .copied()
                .collect();
            Ok(Array::from(&prompt[..]).index(NewAxis))
        }

        fn generate_text(&self, prompt_tokens: &Array) -> Result<String> {
            let mut model = self.model.lock().unwrap();
            let mut cache = Vec::new();
            let generator =
                Generate::<KVCache>::new(&mut *model, &mut cache, self.temperature, prompt_tokens);

            let mut tokens = Vec::new();
            let mut output = String::new();

            for (i, token) in generator.enumerate() {
                let token = token?;
                let token_id = token.item::<u32>();
                if token_id == 151643 || token_id == 151645 {
                    break;
                }
                tokens.push(token);
                if tokens.len() % 5 == 0 {
                    self.decode_tokens(&mut tokens, &mut output)?;
                }
                if i >= self.max_tokens.saturating_sub(1) {
                    break;
                }
            }
            if !tokens.is_empty() {
                self.decode_tokens(&mut tokens, &mut output)?;
            }
            Ok(output)
        }

        fn decode_tokens(&self, tokens: &mut Vec<Array>, output: &mut String) -> Result<()> {
            eval(tokens.iter())?;
            let slice: Vec<u32> = tokens.drain(..).map(|t| t.item::<u32>()).collect();
            if slice.is_empty() {
                return Ok(());
            }
            let tokenizer = self.tokenizer.lock().unwrap();
            let text = tokenizer
                .decode(&slice, true)
                .map_err(|err| anyhow!("decode error: {err:?}"))?;
            output.push_str(&text);
            Ok(())
        }
    }

    impl AiPlanner for MlxQwenClient {
        fn plan_with_history(
            &self,
            input: &str,
            actions: &[ActionSpec],
            history: &[AiChatMessage],
        ) -> Result<AiDecision> {
            let actions_json =
                serde_json::to_string(actions).unwrap_or_else(|_| "[]".to_string());
            let conversations = self.build_conversation(input, &actions_json, history);
            let prompt_tokens = self.encode_prompt(conversations)?;
            let response = self.generate_text(&prompt_tokens)?;
            parse_decision(response.trim(), input)
        }
    }

}

#[cfg(feature = "ai-omnix-mlx")]
pub use omnix::{MlxQwenClient, MlxQwenConfig};

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
    steps: Option<Vec<PlanStepPayload>>,
    #[serde(default)]
    missing: Option<Vec<String>>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct PlanStepPayload {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    requires_approval: Option<bool>,
}

fn parse_decision(content: &str, raw_input: &str) -> Result<AiDecision> {
    let trimmed = content.trim();
    let payload = parse_payload_from_text(content);
    let payload = match payload {
        Some(payload) => payload,
        None => {
            if looks_like_json(trimmed) {
                return Ok(AiDecision::Unknown {
                    message: "AI response format invalid; please retry.".to_string(),
                });
            }
            if !trimmed.is_empty() {
                return Ok(AiDecision::Chat {
                    message: trimmed.to_string(),
                });
            }
            AiDecisionPayload {
                r#type: "unknown".to_string(),
                name: None,
                action: None,
                params: None,
                steps: None,
                missing: None,
                message: Some("AI response was empty".to_string()),
                prompt: None,
            }
        }
    };

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
        let missing = payload.missing.unwrap_or_default();
        let params = payload.params.unwrap_or_else(|| json!({}));
        let action = payload.action.or(payload.name);
        return Ok(AiDecision::NeedInput {
            prompt,
            action,
            params,
            missing,
        });
    }

    if ty == "plan" || payload.steps.is_some() {
        let steps_payload = payload.steps.unwrap_or_default();
        if steps_payload.is_empty() {
            return Ok(AiDecision::Unknown {
                message: payload
                    .message
                    .unwrap_or_else(|| "plan has no steps".to_string()),
            });
        }
        let mut steps = Vec::with_capacity(steps_payload.len());
        for step in steps_payload {
            let action = step
                .action
                .or(step.name)
                .ok_or_else(|| anyhow!("plan step missing action name"))?;
            steps.push(PlanStep {
                id: step.id,
                action,
                params: step.params.unwrap_or_else(|| json!({})),
                note: step.note,
                requires_approval: step.requires_approval,
            });
        }
        return Ok(AiDecision::Plan {
            steps,
            message: payload.message,
        });
    }

    if ty == "chat" {
        let message = payload
            .message
            .unwrap_or_else(|| "".to_string());
        return Ok(AiDecision::Chat { message });
    }

    let message = payload
        .message
        .unwrap_or_else(|| "no plan".to_string());
    Ok(AiDecision::Unknown { message })
}

fn looks_like_json(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('{') || trimmed.contains("\"type\"") || trimmed.contains("{\"type\"")
}

fn parse_payload_from_text(content: &str) -> Option<AiDecisionPayload> {
    let sanitized = sanitize_ai_output(content);
    for candidate in json_candidates(&sanitized) {
        if let Some(payload) = try_parse_payload(&candidate) {
            return Some(payload);
        }
    }
    if let Some(candidate) = fallback_json_slice(&sanitized) {
        if let Some(payload) = try_parse_payload(&candidate) {
            return Some(payload);
        }
    }
    None
}

fn sanitize_ai_output(content: &str) -> String {
    let mut out = String::new();
    let mut in_think = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("<think") {
            if trimmed.contains("</think") {
                in_think = false;
            } else {
                in_think = true;
            }
            continue;
        }
        if trimmed.contains("</think") {
            in_think = false;
            continue;
        }
        if in_think {
            continue;
        }
        if trimmed.starts_with("```") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn json_candidates(text: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    let mut in_str = false;
    let mut escape = false;
    for (idx, ch) in text.char_indices() {
        if in_str {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => {
                if depth == 0 {
                    start = Some(idx);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(start_idx) = start {
                        candidates.push(text[start_idx..=idx].to_string());
                        start = None;
                    }
                }
            }
            _ => {}
        }
    }
    candidates
}

fn fallback_json_slice(text: &str) -> Option<String> {
    let trimmed = text.trim();
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

fn try_parse_payload(candidate: &str) -> Option<AiDecisionPayload> {
    if let Ok(payload) = serde_json::from_str::<AiDecisionPayload>(candidate) {
        return Some(payload);
    }
    let cleaned = strip_trailing_commas(candidate);
    serde_json::from_str::<AiDecisionPayload>(&cleaned).ok()
}

fn strip_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_str = false;
    let mut escape = false;
    while let Some(ch) = chars.next() {
        if in_str {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            out.push(ch);
            continue;
        }
        if ch == '"' {
            in_str = true;
            out.push(ch);
            continue;
        }
        if ch == ',' {
            let mut clone = chars.clone();
            while let Some(next) = clone.peek() {
                if next.is_whitespace() {
                    clone.next();
                } else {
                    break;
                }
            }
            if matches!(clone.peek(), Some('}') | Some(']')) {
                continue;
            }
        }
        out.push(ch);
    }
    out
}

fn system_prompt_base() -> &'static str {
    "You are an action planner for robit.\n\
Return JSON only, no markdown, no extra text.\n\
Do not include <think> tags or reasoning.\n\
Allowed output schemas:\n\
1) {\"type\":\"action\",\"name\":\"...\",\"params\":{...}}\n\
2) {\"type\":\"need_input\",\"prompt\":\"...\",\"action\":\"...\",\"params\":{...},\"missing\":[\"path\"]}\n\
3) {\"type\":\"plan\",\"steps\":[{\"id\":\"s1\",\"action\":\"...\",\"params\":{...},\"note\":\"...\",\"requires_approval\":false}]}\n\
4) {\"type\":\"chat\",\"message\":\"...\"}\n\
5) {\"type\":\"unknown\",\"message\":\"...\"}\n\
Pick an action only from the provided action list.\n\
Use conversation context to fill missing details.\n\
If the user is chatting or the request doesn't map to an action, respond with type=chat.\n\
If the task needs multiple actions, respond with type=plan.\n\
If you ask for missing info, return type=need_input and include action + missing fields.\n\
If the user mentions desktop/桌面, interpret as ~/Desktop.\n\
If the user says current directory/当前目录 and a Context block provides cwd, use it.\n\
If the user input looks like a shell command (e.g. ls, pwd), plan using shell.run unless a safer fs action fits.\n\
If the user asks about system status (cpu/memory/disk/network/uptime), respond with a plan of read-only shell.run probes."
}

fn system_prompt_with_backend(backend: Option<&str>) -> String {
    let mut prompt = system_prompt_base().to_string();
    if let Some(label) = backend {
        let label = label.trim();
        if !label.is_empty() {
            prompt.push_str("\nCurrent model/backend: ");
            prompt.push_str(label);
            prompt.push_str(
                ". If the user asks about the model or backend, answer using this value.",
            );
        }
    }
    prompt
}
