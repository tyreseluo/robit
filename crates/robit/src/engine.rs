use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::adapter::Adapter;
use crate::ai::{AiChatMessage, AiChatRole, AiDecision, AiPlanner};
use crate::preflight::{PreflightConfig, PreflightEngine, PreflightReport};
use crate::protocol::{
    ActionListResultPayload, ApprovalDecisionPayload, ConfigMode, ConfigUpdatePayload,
    ProtocolBody, ProtocolEvent, ResponsePayload, RoomScopePayload,
};
use crate::policy::ActionContext;
use crate::types::{
    ActionOutcome, ActionRequest, ActionSpec, InboundMessage, OutboundMessage, PlannerResponse,
    PlanStep, RiskLevel,
};
use crate::config;
use crate::{ActionRegistry, Policy, RulePlanner};

struct PendingAction {
    request: ActionRequest,
    spec: ActionSpec,
    sender: String,
    config: RoomConfig,
    plan: Option<PlanContext>,
}

#[derive(Clone)]
struct PendingInput {
    action: String,
    params: serde_json::Value,
    missing: Vec<String>,
    prompt: String,
}

#[derive(Clone)]
struct PlanResultItem {
    action: String,
    summary: String,
    data: serde_json::Value,
}

#[derive(Clone)]
struct PlanProgress {
    id: String,
    total_steps: usize,
    results: Vec<PlanResultItem>,
}

#[derive(Clone)]
struct PlanContext {
    plan_id: String,
    remaining: Vec<PlanStep>,
    auto_approve: bool,
    completed_steps: usize,
    total_steps: usize,
}

struct ApprovalStore {
    next_id: u64,
    pending: HashMap<String, PendingAction>,
    latest_by_sender: HashMap<String, String>,
}

impl ApprovalStore {
    fn new() -> Self {
        Self {
            next_id: 1,
            pending: HashMap::new(),
            latest_by_sender: HashMap::new(),
        }
    }

    fn create(
        &mut self,
        sender: &str,
        request: ActionRequest,
        spec: ActionSpec,
        config: RoomConfig,
        plan: Option<PlanContext>,
    ) -> String {
        let id = format!("appr-{}", self.next_id);
        self.next_id += 1;
        self.pending.insert(
            id.clone(),
            PendingAction {
                request,
                spec,
                sender: sender.to_string(),
                config,
                plan,
            },
        );
        self.latest_by_sender
            .insert(sender.to_string(), id.clone());
        id
    }

    fn take(&mut self, id: &str) -> Option<PendingAction> {
        if let Some(pending) = self.pending.remove(id) {
            self.latest_by_sender.remove(&pending.sender);
            return Some(pending);
        }
        None
    }

    fn latest_for_sender(&self, sender: &str) -> Option<String> {
        self.latest_by_sender.get(sender).cloned()
    }
}

struct ConversationStore {
    max_messages: usize,
    history: HashMap<(String, String), Vec<AiChatMessage>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedConversation {
    workspace_id: String,
    room_id: String,
    messages: Vec<AiChatMessage>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedStore {
    max_messages: usize,
    conversations: Vec<PersistedConversation>,
}

impl ConversationStore {
    fn new(max_messages: usize) -> Self {
        Self {
            max_messages: max_messages.max(2),
            history: HashMap::new(),
        }
    }

    fn key_for(&self, msg: &InboundMessage) -> (String, String) {
        let workspace = msg
            .workspace_id
            .clone()
            .unwrap_or_else(|| "default".to_string());
        (workspace, msg.channel.clone())
    }

    fn key_for_parts(&self, workspace_id: &str, room_id: &str) -> (String, String) {
        (workspace_id.to_string(), room_id.to_string())
    }

    fn history_for(&self, key: &(String, String)) -> Vec<AiChatMessage> {
        self.history.get(key).cloned().unwrap_or_default()
    }

    fn record_exchange(
        &mut self,
        key: &(String, String),
        user_input: &str,
        replies: &[OutboundMessage],
    ) {
        let entry = self.history.entry(key.clone()).or_default();
        entry.push(AiChatMessage {
            role: AiChatRole::User,
            content: user_input.trim().to_string(),
        });
        for reply in replies {
            if reply.text.trim().is_empty() {
                continue;
            }
            entry.push(AiChatMessage {
                role: AiChatRole::Assistant,
                content: reply.text.trim().to_string(),
            });
        }
        if entry.len() > self.max_messages {
            let start = entry.len().saturating_sub(self.max_messages);
            entry.drain(0..start);
        }
    }

    fn record_context(&mut self, key: &(String, String), role: AiChatRole, content: &str) {
        let text = content.trim();
        if text.is_empty() {
            return;
        }
        let entry = self.history.entry(key.clone()).or_default();
        entry.push(AiChatMessage {
            role,
            content: text.to_string(),
        });
        if entry.len() > self.max_messages {
            let start = entry.len().saturating_sub(self.max_messages);
            entry.drain(0..start);
        }
    }

    fn load_from_path(&mut self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(path)?;
        let store: PersistedStore = serde_json::from_str(&content)?;
        self.history.clear();
        for convo in store.conversations {
            let key = (convo.workspace_id, convo.room_id);
            let mut messages = convo.messages;
            if messages.len() > self.max_messages {
                let start = messages.len().saturating_sub(self.max_messages);
                messages.drain(0..start);
            }
            self.history.insert(key, messages);
        }
        Ok(())
    }

    fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut conversations = Vec::new();
        for ((workspace_id, room_id), messages) in &self.history {
            conversations.push(PersistedConversation {
                workspace_id: workspace_id.clone(),
                room_id: room_id.clone(),
                messages: messages.clone(),
            });
        }
        let store = PersistedStore {
            max_messages: self.max_messages,
            conversations,
        };
        let data = serde_json::to_string_pretty(&store)?;
        fs::write(path, data)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ApprovalDecision {
    Approve,
    ApproveAll,
    Deny,
}

pub struct Engine {
    registry: ActionRegistry,
    planner: RulePlanner,
    ai_backend: Option<std::sync::Arc<dyn AiPlanner>>,
    ai_backend_label: Option<String>,
    ctx: ActionContext,
    preflight: PreflightEngine,
    approvals: ApprovalStore,
    next_message_id: u64,
    next_plan_id: u64,
    pending_inputs: HashMap<(String, String), PendingInput>,
    plans: HashMap<String, PlanProgress>,
    seen_messages: HashSet<String>,
    scope: RoomScope,
    config_store: ConfigStore,
    conversations: ConversationStore,
    conversation_persist_path: Option<PathBuf>,
}

impl Engine {
    pub fn new(registry: ActionRegistry, planner: RulePlanner, policy: Policy) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let mut policy = policy;
        let mut preflight_config = PreflightConfig::default();
        match config::load_default_config(policy.clone(), preflight_config.clone()) {
            Ok((loaded_policy, loaded_preflight)) => {
                policy = loaded_policy;
                preflight_config = loaded_preflight;
            }
            Err(err) => {
                eprintln!("robit config load failed: {err}");
            }
        }
        Ok(Self {
            registry,
            planner,
            ai_backend: None,
            ai_backend_label: None,
            ctx: ActionContext {
                cwd,
                dry_run: true,
                policy,
            },
            preflight: PreflightEngine::new(preflight_config),
            approvals: ApprovalStore::new(),
            next_message_id: 1,
            next_plan_id: 1,
            pending_inputs: HashMap::new(),
            plans: HashMap::new(),
            seen_messages: HashSet::new(),
            scope: RoomScope::default(),
            config_store: ConfigStore::default(),
            conversations: ConversationStore::new(50),
            conversation_persist_path: None,
        })
    }

    pub fn set_ai_backend(&mut self, backend: Option<std::sync::Arc<dyn AiPlanner>>) {
        self.set_ai_backend_with_label(backend, None);
    }

    pub fn set_ai_backend_with_label(
        &mut self,
        backend: Option<std::sync::Arc<dyn AiPlanner>>,
        label: Option<String>,
    ) {
        self.ai_backend = backend;
        self.ai_backend_label = label;
    }

    #[cfg(feature = "ai-http")]
    pub fn set_ai_client(&mut self, ai_client: Option<crate::ai::AiClient>) {
        let label = ai_client
            .as_ref()
            .map(|client| format!("http:{}", client.model_name()));
        let backend = ai_client.map(|client| {
            std::sync::Arc::new(client) as std::sync::Arc<dyn AiPlanner>
        });
        self.set_ai_backend_with_label(backend, label);
    }

    pub fn enable_conversation_persistence(&mut self, path: PathBuf) {
        self.conversation_persist_path = Some(path.clone());
        if let Err(err) = self.conversations.load_from_path(&path) {
            eprintln!("robit context load failed: {err}");
        }
    }

    pub fn set_preflight_config(&mut self, config: PreflightConfig) {
        self.preflight.set_config(config);
    }

    fn log_preflight(&self, report: &PreflightReport) {
        if let Ok(json) = serde_json::to_string(report) {
            eprintln!("robit preflight: {json}");
        }
    }

    fn conversation_key_for(&self, msg: &InboundMessage) -> (String, String) {
        let (workspace_id, room_id) = self.conversations.key_for(msg);
        self.decorate_conversation_key(workspace_id, room_id)
    }

    fn conversation_key_parts(&self, workspace_id: &str, room_id: &str) -> (String, String) {
        self.decorate_conversation_key(workspace_id.to_string(), room_id.to_string())
    }

    fn decorate_conversation_key(
        &self,
        workspace_id: String,
        room_id: String,
    ) -> (String, String) {
        let decorated_room = if let Some(label) = self.ai_backend_label.as_deref() {
            format!("{room_id}::ai={label}")
        } else {
            room_id
        };
        (workspace_id, decorated_room)
    }

    pub fn handle_message(&mut self, msg: InboundMessage) -> Vec<OutboundMessage> {
        self.handle_message_with_config(msg, None)
    }

    pub fn handle_protocol_event(&mut self, event: ProtocolEvent) -> Vec<ProtocolEvent> {
        if event.schema_version != "robit.v1" {
            return Vec::new();
        }

        match event.body {
            ProtocolBody::Message(payload) => {
                if !self.scope.allows(&payload.workspace_id, &payload.room_id) {
                    return Vec::new();
                }
                if self.seen_messages.contains(&payload.message_id) {
                    return Vec::new();
                }
                self.seen_messages.insert(payload.message_id.clone());
                let convo_key = self.conversation_key_parts(&payload.workspace_id, &payload.room_id);
                if payload
                    .metadata
                    .get("context_only")
                    .and_then(|value| value.as_bool())
                    == Some(true)
                {
                    let role = payload
                        .metadata
                        .get("role")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_lowercase())
                        .map(|value| if value == "assistant" { AiChatRole::Assistant } else { AiChatRole::User })
                        .unwrap_or(AiChatRole::User);
                    self.record_context_and_persist(&convo_key, role, &payload.text);
                    return Vec::new();
                }
                let room_cfg = self
                    .config_store
                    .effective_for(&payload.workspace_id, &payload.room_id);
                let msg = InboundMessage {
                    id: event.id,
                    text: payload.text,
                    sender: payload.sender_id,
                    channel: payload.room_id,
                    workspace_id: Some(payload.workspace_id),
                    metadata: payload.metadata,
                };
                let replies = self.handle_message_with_config(msg, Some(room_cfg.clone()));
                replies
                    .into_iter()
                    .map(|reply| self.wrap_response(reply))
                    .collect()
            }
            ProtocolBody::ApprovalDecision(payload) => self.handle_approval_decision(payload),
            ProtocolBody::RoomScope(payload) => {
                self.scope.update(payload);
                Vec::new()
            }
            ProtocolBody::ConfigUpdate(payload) => {
                self.config_store.apply(payload);
                Vec::new()
            }
            ProtocolBody::ActionListRequest(_) => {
                let actions = self.registry.list_specs();
                vec![ProtocolEvent::new(ProtocolBody::ActionListResult(
                    ActionListResultPayload { actions },
                ))]
            }
            ProtocolBody::Ping(_) => vec![ProtocolEvent::new(ProtocolBody::Pong(
                crate::protocol::PongPayload { in_reply_to: event.id },
            ))],
            _ => Vec::new(),
        }
    }
    pub fn run_with_adapter<A: Adapter>(&mut self, adapter: &mut A) -> Result<()> {
        loop {
            let Some(msg) = adapter.recv()? else {
                break;
            };
            if msg.text.trim().is_empty() {
                continue;
            }
            let responses = self.handle_message(msg);
            for response in responses {
                adapter.send(response)?;
            }
        }
        Ok(())
    }

    fn handle_message_with_config(
        &mut self,
        msg: InboundMessage,
        room_cfg: Option<RoomConfig>,
    ) -> Vec<OutboundMessage> {
        let text = msg.text.trim();
        if text.is_empty() {
            return Vec::new();
        }

        let convo_key = self.conversation_key_for(&msg);
        let room_cfg = room_cfg.unwrap_or_default();

        if let Some(response) = self.handle_control(&msg) {
            self.record_exchange_and_persist(&convo_key, text, &[response.clone()]);
            return vec![response];
        }

        if let Some(response) = self.handle_approval(&msg) {
            self.record_exchange_and_persist(&convo_key, text, &response);
            return response;
        }

        let mut pending_for_ai = None;
        if let Some(pending) = self.pending_inputs.remove(&convo_key) {
            let ctx = self.build_context(&room_cfg);
            if let Some(request) = self.resolve_pending_input(&pending, text, &ctx) {
                let replies = self.handle_action_request(&msg, request, Some(room_cfg.clone()));
                self.record_exchange_and_persist(&convo_key, text, &replies);
                return replies;
            }
            pending_for_ai = Some(pending);
        }

        let history = self.conversations.history_for(&convo_key);
        if let Some(ai_backend) = &self.ai_backend {
            let ai_input =
                self.build_ai_input(text, &msg, &room_cfg, pending_for_ai.as_ref(), &history);
            match ai_backend.plan_with_history(&ai_input, &self.registry.list_specs(), &history) {
                Ok(AiDecision::Action(request)) => {
                    let replies = self.handle_action_request(&msg, request, Some(room_cfg.clone()));
                    self.record_exchange_and_persist(&convo_key, text, &replies);
                    return replies;
                }
                Ok(AiDecision::NeedInput {
                    prompt,
                    action,
                    params,
                    missing,
                }) => {
                    if let Some(action) = action {
                        if !missing.is_empty() {
                            self.pending_inputs.insert(
                                convo_key.clone(),
                                PendingInput {
                                    action,
                                    params,
                                    missing,
                                    prompt: prompt.clone(),
                                },
                            );
                        }
                    }
                    let reply = self.reply(
                        &msg,
                        prompt,
                        "need_input",
                        serde_json::Value::Null,
                    );
                    self.record_exchange_and_persist(&convo_key, text, &[reply.clone()]);
                    return vec![reply];
                }
                Ok(AiDecision::Chat { message }) => {
                    let reply_text = if message.trim().is_empty() {
                        "我在这儿，可以继续说说你的需求。".to_string()
                    } else {
                        message
                    };
                    let reply = self.reply(&msg, reply_text, "chat", serde_json::Value::Null);
                    self.record_exchange_and_persist(&convo_key, text, &[reply.clone()]);
                    return vec![reply];
                }
                Ok(AiDecision::Plan { steps, message }) => {
                    let mut replies = Vec::new();
                    if let Some(note) = message {
                        if !note.trim().is_empty() {
                            replies.push(self.reply(&msg, note, "plan", serde_json::Value::Null));
                        }
                    }
                    let plan_replies = self.handle_plan_request(&msg, steps, Some(room_cfg.clone()));
                    replies.extend(plan_replies);
                    self.record_exchange_and_persist(&convo_key, text, &replies);
                    return replies;
                }
                Ok(AiDecision::Unknown { message }) => {
                    if message == "AI response format invalid; please retry." {
                        if let Some(steps) = heuristic_plan_for(text) {
                            let plan_replies =
                                self.handle_plan_request(&msg, steps, Some(room_cfg.clone()));
                            self.record_exchange_and_persist(&convo_key, text, &plan_replies);
                            return plan_replies;
                        }
                        let retry_input = format!(
                            "RETRY: Return valid JSON only (no prose). Keep it minimal. {}",
                            ai_input
                        );
                        if let Ok(retry_decision) = ai_backend.plan_with_history(
                            &retry_input,
                            &self.registry.list_specs(),
                            &history,
                        ) {
                            if !matches!(retry_decision, AiDecision::Unknown { .. }) {
                                match retry_decision {
                                    AiDecision::Action(request) => {
                                        let replies = self.handle_action_request(
                                            &msg,
                                            request,
                                            Some(room_cfg.clone()),
                                        );
                                        self.record_exchange_and_persist(&convo_key, text, &replies);
                                        return replies;
                                    }
                                    AiDecision::NeedInput { prompt, action, params, missing } => {
                                        if let Some(action) = action {
                                            if !missing.is_empty() {
                                                self.pending_inputs.insert(
                                                    convo_key.clone(),
                                                    PendingInput {
                                                        action,
                                                        params,
                                                        missing,
                                                        prompt: prompt.clone(),
                                                    },
                                                );
                                            }
                                        }
                                        let reply = self.reply(
                                            &msg,
                                            prompt,
                                            "need_input",
                                            serde_json::Value::Null,
                                        );
                                        self.record_exchange_and_persist(&convo_key, text, &[reply.clone()]);
                                        return vec![reply];
                                    }
                                    AiDecision::Chat { message } => {
                                        let reply = self.reply(&msg, message, "chat", serde_json::Value::Null);
                                        self.record_exchange_and_persist(&convo_key, text, &[reply.clone()]);
                                        return vec![reply];
                                    }
                                    AiDecision::Plan { steps, message } => {
                                        let mut replies = Vec::new();
                                        if let Some(note) = message {
                                            if !note.trim().is_empty() {
                                                replies.push(self.reply(&msg, note, "plan", serde_json::Value::Null));
                                            }
                                        }
                                        let plan_replies = self.handle_plan_request(&msg, steps, Some(room_cfg.clone()));
                                        replies.extend(plan_replies);
                                        self.record_exchange_and_persist(&convo_key, text, &replies);
                                        return replies;
                                    }
                                    AiDecision::Unknown { message } => {
                                        let reply = self.reply(&msg, message, "chat", serde_json::Value::Null);
                                        self.record_exchange_and_persist(&convo_key, text, &[reply.clone()]);
                                        return vec![reply];
                                    }
                                }
                            }
                        }
                    }
                    let reply_text = if message.trim().is_empty() {
                        "我暂时没把握这个请求，可以再具体一点吗？".to_string()
                    } else {
                        message
                    };
                    let reply = self.reply(&msg, reply_text, "chat", serde_json::Value::Null);
                    self.record_exchange_and_persist(&convo_key, text, &[reply.clone()]);
                    return vec![reply];
                }
                Err(err) => {
                    eprintln!("robit ai error: {err}");
                }
            }
        }

        match self.planner.plan(text) {
            PlannerResponse::Action(request) => {
                let replies = self.handle_action_request(&msg, request, Some(room_cfg.clone()));
                self.record_exchange_and_persist(&convo_key, text, &replies);
                replies
            }
            PlannerResponse::NeedInput { prompt } => {
                let reply = self.reply(&msg, prompt, "need_input", serde_json::Value::Null);
                self.record_exchange_and_persist(&convo_key, text, &[reply.clone()]);
                vec![reply]
            }
            PlannerResponse::Unknown { message } => {
                let reply = self.reply(
                    &msg,
                    format!(
                        "我还没学会处理这个请求（{message}）。可以试试输入 actions 查看动作列表，或用 action:xxx 明确指令。",
                    ),
                    "unknown",
                    serde_json::Value::Null,
                );
                self.record_exchange_and_persist(&convo_key, text, &[reply.clone()]);
                vec![reply]
            }
        }
    }

    fn handle_control(&mut self, msg: &InboundMessage) -> Option<OutboundMessage> {
        match msg.text.trim() {
            "help" => Some(self.reply(
                msg,
                self.help_text(),
                "info",
                serde_json::Value::Null,
            )),
            "actions" => Some(self.reply(
                msg,
                self.actions_text(),
                "info",
                serde_json::Value::Null,
            )),
            "backend" | "model" | "ai" => Some(self.reply(
                msg,
                self.backend_text(),
                "info",
                serde_json::Value::Null,
            )),
            "dry-run on" => {
                self.ctx.dry_run = true;
                Some(self.reply(msg, "dry-run enabled", "info", serde_json::Value::Null))
            }
            "dry-run off" => {
                self.ctx.dry_run = false;
                Some(self.reply(msg, "dry-run disabled", "info", serde_json::Value::Null))
            }
            _ => None,
        }
    }

    fn handle_approval(&mut self, msg: &InboundMessage) -> Option<Vec<OutboundMessage>> {
        let trimmed = msg.text.trim();
        if trimmed.is_empty() {
            return None;
        }
        let lower = trimmed.to_lowercase();
        let explicit = lower == "approve"
            || lower == "deny"
            || lower.starts_with("approve ")
            || lower.starts_with("deny ")
            || lower.starts_with("approve-all")
            || lower.starts_with("approve all")
            || lower.starts_with("approve plan");
        let has_pending = self.approvals.latest_for_sender(&msg.sender).is_some();
        if !explicit && !has_pending {
            return None;
        }

        let (decision, id) = parse_approval_command(trimmed)?;

        let pending_id = if let Some(id) = id {
            id
        } else if let Some(latest) = self.approvals.latest_for_sender(&msg.sender) {
            latest
        } else {
            return Some(vec![self.reply(
                msg,
                "no pending approvals",
                "info",
                serde_json::Value::Null,
            )]);
        };

        let Some(pending) = self.approvals.take(&pending_id) else {
            return Some(vec![self.reply(
                msg,
                format!("approval id not found: {pending_id}"),
                "error",
                serde_json::Value::Null,
            )]);
        };

        match decision {
            ApprovalDecision::Deny => Some(vec![self.reply(
                msg,
                format!("action '{}' cancelled", pending.spec.name),
                "cancelled",
                serde_json::Value::Null,
            )]),
            ApprovalDecision::Approve | ApprovalDecision::ApproveAll => {
                let mut plan_ctx = pending.plan;
                let has_plan = plan_ctx.is_some();
                if let (ApprovalDecision::ApproveAll, Some(plan)) = (&decision, plan_ctx.as_mut()) {
                    plan.auto_approve = true;
                }
                let mut outcomes = self.execute_action(
                    &pending.request,
                    &pending.spec,
                    msg,
                    Some(pending.config.clone()),
                );
                if let Some(plan) = plan_ctx.as_ref() {
                    if let Some(outcome) = extract_outcome_from_replies(&outcomes) {
                        self.record_plan_result(&plan.plan_id, &pending.spec.name, &outcome);
                    }
                }
                if let Some(plan) = plan_ctx {
                    let succeeded = outcomes.iter().any(|reply| {
                        reply
                            .metadata
                            .get("kind")
                            .and_then(|v| v.as_str())
                            == Some("action_result")
                    });
                    if succeeded {
                        let mut more = self.execute_plan_steps(
                            msg,
                            plan.remaining,
                            pending.config,
                            plan.auto_approve,
                            Some(plan.plan_id),
                            plan.completed_steps + 1,
                            plan.total_steps,
                        );
                        outcomes.append(&mut more);
                    } else if let Some(summary) = self.finish_plan(&plan.plan_id, msg, true) {
                        outcomes.push(summary);
                    }
                }
                if has_plan {
                    Some(filter_plan_result_replies(outcomes))
                } else {
                    Some(outcomes)
                }
            }
        }
    }

    fn handle_plan_request(
        &mut self,
        msg: &InboundMessage,
        steps: Vec<PlanStep>,
        room_cfg: Option<RoomConfig>,
    ) -> Vec<OutboundMessage> {
        if steps.is_empty() {
            return vec![self.reply(
                msg,
                "plan is empty".to_string(),
                "error",
                serde_json::Value::Null,
            )];
        }
        let plan_id = self.next_plan_id();
        self.start_plan_progress(&plan_id, steps.len());
        let room_cfg = room_cfg.unwrap_or_default();
        let total_steps = steps.len();
        self.execute_plan_steps(
            msg,
            steps,
            room_cfg,
            false,
            Some(plan_id),
            0,
            total_steps,
        )
    }

    fn execute_plan_steps(
        &mut self,
        msg: &InboundMessage,
        steps: Vec<PlanStep>,
        room_cfg: RoomConfig,
        auto_approve: bool,
        plan_id: Option<String>,
        completed_steps: usize,
        total_steps: usize,
    ) -> Vec<OutboundMessage> {
        let mut replies = Vec::new();
        let mut completed = completed_steps;
        let mut index = 0usize;
        let plan_label = plan_id.clone().unwrap_or_else(|| "plan".to_string());
        let mut awaiting_approval = false;
        let mut stopped_early = false;

        while index < steps.len() {
            let step = steps[index].clone();
            let step_no = completed + 1;
            let request = ActionRequest {
                name: step.action.clone(),
                params: step.params.clone(),
                raw_input: msg.text.clone(),
            };
            let Some(action) = self.registry.get(&request.name) else {
                replies.push(self.reply(
                    msg,
                    format!("unknown action in plan: {}", request.name),
                    "error",
                    serde_json::Value::Null,
                ));
                break;
            };
            let spec = action.spec();
            if !room_cfg.allows_action(&spec.name) {
                replies.push(self.reply(
                    msg,
                    format!("action not allowed: {}", spec.name),
                    "error",
                    serde_json::Value::Null,
                ));
                break;
            }
            let mut needs_approval = self.requires_approval(&spec, &room_cfg);
            if step.requires_approval == Some(true) {
                needs_approval = true;
            }
            let ctx = self.build_context(&room_cfg);
            let preflight = match self.preflight.check(&spec, &request.params, &ctx) {
                Ok(report) => report,
                Err(err) => {
                    replies.push(self.reply(
                        msg,
                        format!("preflight failed: {err}"),
                        "error",
                        serde_json::Value::Null,
                    ));
                    break;
                }
            };
            self.log_preflight(&preflight);
            if !preflight.allowed && self.preflight.config().strict {
                replies.push(self.reply(
                    msg,
                    format!("preflight blocked: {}", preflight.summary()),
                    "error",
                    serde_json::Value::Null,
                ));
                break;
            }
            if let Err(err) = action.validate(&ctx, &request.params) {
                replies.push(self.reply(
                    msg,
                    format!("validation failed: {err}"),
                    "error",
                    serde_json::Value::Null,
                ));
                break;
            }

            if needs_approval && !auto_approve {
                let remaining = steps[index + 1..].to_vec();
                let plan_ctx = PlanContext {
                    plan_id: plan_label.clone(),
                    remaining,
                    auto_approve: false,
                    completed_steps: completed,
                    total_steps,
                };
                let approval_id = self.approvals.create(
                    &msg.sender,
                    request,
                    spec.clone(),
                    room_cfg.clone(),
                    Some(plan_ctx),
                );
                let hint = PlanApprovalHint {
                    plan_id: plan_label.clone(),
                    step_index: step_no,
                    total_steps,
                    allow_approve_all: true,
                };
                let text = format_approval_prompt(
                    &spec,
                    &step.params,
                    &ctx,
                    &approval_id,
                    Some(&preflight),
                    Some(hint),
                );
                replies.push(self.reply(
                    msg,
                    text,
                    "approval_request",
                    json!({"approval_id": approval_id, "plan_id": plan_label, "step": step_no}),
                ));
                awaiting_approval = true;
                break;
            }

            match action.execute(&ctx, &request.params) {
                Ok(outcome) => {
                    self.record_plan_result(&plan_label, &spec.name, &outcome);
                    replies.push(self.reply_with_outcome(msg, outcome, &spec));
                    completed += 1;
                    index += 1;
                }
                Err(err) => {
                    replies.push(self.reply(
                        msg,
                        format!("error: {err}"),
                        "error",
                        serde_json::Value::Null,
                    ));
                    stopped_early = true;
                    break;
                }
            }
        }

        if !awaiting_approval && !plan_label.is_empty() {
            if let Some(summary) = self.finish_plan(&plan_label, msg, stopped_early) {
                replies.push(summary);
            }
        }

        if !plan_label.is_empty() {
            filter_plan_result_replies(replies)
        } else {
            replies
        }
    }

    fn start_plan_progress(&mut self, plan_id: &str, total_steps: usize) {
        self.plans.entry(plan_id.to_string()).or_insert(PlanProgress {
            id: plan_id.to_string(),
            total_steps,
            results: Vec::new(),
        });
    }

    fn record_plan_result(&mut self, plan_id: &str, action: &str, outcome: &ActionOutcome) {
        let Some(plan) = self.plans.get_mut(plan_id) else {
            return;
        };
        plan.results.push(PlanResultItem {
            action: action.to_string(),
            summary: outcome.summary.clone(),
            data: outcome.data.clone(),
        });
    }

    fn finish_plan(
        &mut self,
        plan_id: &str,
        msg: &InboundMessage,
        stopped_early: bool,
    ) -> Option<OutboundMessage> {
        let plan = self.plans.remove(plan_id)?;
        if plan.results.is_empty() {
            return None;
        }
        let status = if stopped_early {
            "plan_stopped"
        } else {
            "plan_completed"
        };
        let summary_text = self.summarize_plan(&plan);
        Some(self.reply(
            msg,
            summary_text,
            status,
            json!({"plan_id": plan.id, "steps": plan.results.len(), "total_steps": plan.total_steps}),
        ))
    }

    fn summarize_plan(&self, plan: &PlanProgress) -> String {
        if let Some(summary) = summarize_system_status(plan) {
            return summary;
        }
        let details = plan_result_details(plan);
        if let Some(ai_backend) = &self.ai_backend {
            let prompt = format!(
                "Summarize the following execution results for the user. Return type=chat only.\nResults:\n{details}"
            );
            if let Ok(decision) = ai_backend.plan_with_history(&prompt, &[], &[]) {
                if let AiDecision::Chat { message } = decision {
                    let trimmed = message.trim();
                    if !trimmed.is_empty()
                        && !trimmed.contains("[result]")
                        && !trimmed.to_lowercase().contains("please provide")
                    {
                        return message;
                    }
                }
            }
        }
        format_plan_summary_fallback(plan)
    }

    fn resolve_pending_input(
        &self,
        pending: &PendingInput,
        text: &str,
        ctx: &ActionContext,
    ) -> Option<ActionRequest> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        let mut params = pending.params.clone();
        let mut filled = false;

        let value = if is_current_directory(trimmed) {
            ctx.cwd.to_string_lossy().to_string()
        } else {
            trimmed.to_string()
        };

        if pending.missing.len() == 1 {
            params = insert_param(params, &pending.missing[0], &value);
            filled = true;
        } else {
            for key in &pending.missing {
                if is_path_key(key) {
                    params = insert_param(params, key, &value);
                    filled = true;
                    break;
                }
            }
        }

        if filled {
            return Some(ActionRequest {
                name: pending.action.clone(),
                params,
                raw_input: trimmed.to_string(),
            });
        }
        None
    }

    fn build_ai_input(
        &self,
        text: &str,
        msg: &InboundMessage,
        room_cfg: &RoomConfig,
        pending: Option<&PendingInput>,
        history: &[AiChatMessage],
    ) -> String {
        let mut parts = Vec::new();
        let cwd = self.build_context(room_cfg).cwd;
        let home = std::env::var("HOME").unwrap_or_else(|_| "".to_string());
        parts.push(format!("Context:\n- cwd: {}\n- home: {}\n- room: {}\n- workspace: {}",
            cwd.to_string_lossy(),
            home,
            msg.channel,
            msg.workspace_id.clone().unwrap_or_else(|| "default".to_string())
        ));
        if let Some(pending) = pending {
            if !pending.missing.is_empty() {
                parts.push(format!(
                    "Follow-up:\n- pending_action: {}\n- missing: {:?}\n- prompt: {}",
                    pending.action,
                    pending.missing,
                    pending.prompt
                ));
            }
        }
        let mut user_text = text.to_string();
        if is_affirmation(text) || is_followup_reference(text) {
            let prev_assistant = last_assistant_message(history).unwrap_or_default();
            let prev_user = last_user_message(history).unwrap_or_default();
            if !prev_assistant.is_empty() || !prev_user.is_empty() {
                user_text = format!(
                    "User confirmed or referenced the previous request. User reply: {text}. Previous user request: {prev_user}. Previous assistant message: {prev_assistant}"
                );
            }
        }
        parts.push(format!("User: {user_text}"));
        parts.join("\n\n")
    }

    fn handle_action_request(
        &mut self,
        msg: &InboundMessage,
        request: ActionRequest,
        room_cfg: Option<RoomConfig>,
    ) -> Vec<OutboundMessage> {
        let Some(action) = self.registry.get(&request.name) else {
            return vec![self.reply(
                msg,
                format!("unknown action: {}", request.name),
                "error",
                serde_json::Value::Null,
            )];
        };
        let spec = action.spec();
        let room_cfg = room_cfg.unwrap_or_default();
        if !room_cfg.allows_action(&spec.name) {
            return vec![self.reply(
                msg,
                format!("action not allowed: {}", spec.name),
                "error",
                serde_json::Value::Null,
            )];
        }
        let needs_approval = self.requires_approval(&spec, &room_cfg);

        let ctx = self.build_context(&room_cfg);
        let preflight = match self.preflight.check(&spec, &request.params, &ctx) {
            Ok(report) => report,
            Err(err) => {
                return vec![self.reply(
                    msg,
                    format!("preflight failed: {err}"),
                    "error",
                    serde_json::Value::Null,
                )]
            }
        };
        self.log_preflight(&preflight);
        if !preflight.allowed && self.preflight.config().strict {
            return vec![self.reply(
                msg,
                format!("preflight blocked: {}", preflight.summary()),
                "error",
                serde_json::Value::Null,
            )];
        }
        if let Err(err) = action.validate(&ctx, &request.params) {
            return vec![self.reply(
                msg,
                format!("validation failed: {err}"),
                "error",
                serde_json::Value::Null,
            )];
        }

        if needs_approval {
            let params_snapshot = request.params.clone();
            let approval_id = self.approvals.create(
                &msg.sender,
                request,
                spec.clone(),
                room_cfg.clone(),
                None,
            );
            let text =
                format_approval_prompt(&spec, &params_snapshot, &ctx, &approval_id, Some(&preflight), None);
            return vec![self.reply(
                msg,
                text,
                "approval_request",
                json!({"approval_id": approval_id}),
            )];
        }

        self.execute_action(&request, &spec, msg, Some(room_cfg))
    }

    fn execute_action(
        &mut self,
        request: &ActionRequest,
        spec: &ActionSpec,
        msg: &InboundMessage,
        room_cfg: Option<RoomConfig>,
    ) -> Vec<OutboundMessage> {
        let Some(action) = self.registry.get(&request.name) else {
            return vec![self.reply(
                msg,
                format!("unknown action: {}", request.name),
                "error",
                serde_json::Value::Null,
            )];
        };

        let room_cfg = room_cfg.unwrap_or_default();
        let ctx = self.build_context(&room_cfg);
        let preflight = match self.preflight.check(spec, &request.params, &ctx) {
            Ok(report) => report,
            Err(err) => {
                return vec![self.reply(
                    msg,
                    format!("preflight failed: {err}"),
                    "error",
                    serde_json::Value::Null,
                )]
            }
        };
        self.log_preflight(&preflight);
        if !preflight.allowed && self.preflight.config().strict {
            return vec![self.reply(
                msg,
                format!("preflight blocked: {}", preflight.summary()),
                "error",
                serde_json::Value::Null,
            )];
        }
        if let Err(err) = action.validate(&ctx, &request.params) {
            return vec![self.reply(
                msg,
                format!("validation failed: {err}"),
                "error",
                serde_json::Value::Null,
            )];
        }

        match action.execute(&ctx, &request.params) {
            Ok(outcome) => vec![self.reply_with_outcome(msg, outcome, spec)],
            Err(err) => vec![self.reply(
                msg,
                format!("error: {err}"),
                "error",
                serde_json::Value::Null,
            )],
        }
    }

    fn reply(&mut self, msg: &InboundMessage, text: impl Into<String>, kind: &str, data: serde_json::Value) -> OutboundMessage {
        let id = self.next_message_id();
        OutboundMessage {
            id,
            in_reply_to: Some(msg.id.clone()),
            text: text.into(),
            recipient: msg.sender.clone(),
            channel: msg.channel.clone(),
            workspace_id: msg.workspace_id.clone(),
            metadata: json!({
                "kind": kind,
                "data": data,
            }),
        }
    }

    fn reply_with_outcome(
        &mut self,
        msg: &InboundMessage,
        outcome: ActionOutcome,
        spec: &ActionSpec,
    ) -> OutboundMessage {
        let id = self.next_message_id();
        OutboundMessage {
            id,
            in_reply_to: Some(msg.id.clone()),
            text: format!("ok: {}", outcome.summary),
            recipient: msg.sender.clone(),
            channel: msg.channel.clone(),
            workspace_id: msg.workspace_id.clone(),
            metadata: json!({
                "kind": "action_result",
                "action": spec.name,
                "data": outcome.data,
            }),
        }
    }

    fn next_message_id(&mut self) -> String {
        let id = self.next_message_id;
        self.next_message_id += 1;
        format!("out-{id}")
    }

    fn next_plan_id(&mut self) -> String {
        let id = self.next_plan_id;
        self.next_plan_id += 1;
        format!("plan-{id}")
    }

    fn record_exchange_and_persist(
        &mut self,
        key: &(String, String),
        user_input: &str,
        replies: &[OutboundMessage],
    ) {
        self.conversations
            .record_exchange(key, user_input, replies);
        self.persist_conversations();
    }

    fn record_context_and_persist(
        &mut self,
        key: &(String, String),
        role: AiChatRole,
        content: &str,
    ) {
        self.conversations.record_context(key, role, content);
        self.persist_conversations();
    }

    fn persist_conversations(&self) {
        let Some(path) = &self.conversation_persist_path else {
            return;
        };
        if let Err(err) = self.conversations.save_to_path(path) {
            eprintln!("robit context save failed: {err}");
        }
    }

    fn help_text(&self) -> String {
        let mut text = String::new();
        text.push_str("commands:\n");
        text.push_str("  help           show this help\n");
        text.push_str("  actions        list actions\n");
        text.push_str("  backend        show ai backend\n");
        text.push_str("  dry-run on     enable dry-run mode\n");
        text.push_str("  dry-run off    disable dry-run mode\n");
        text.push_str("  approve <id>   approve pending action\n");
        text.push_str("  approve-all <id> approve this and remaining plan steps\n");
        text.push_str("  deny <id>      deny pending action\n\n");
        text.push_str("examples:\n");
        text.push_str("  action:fs.write_file {\"path\":\"./notes.txt\",\"content\":\"hello world\"}\n");
        text.push_str("  action:fs.read_file path=./notes.txt\n");
        text.push_str("  action:fs.replace_text {\"path\":\"./notes.txt\",\"find\":\"hello\",\"replace\":\"hi\"}\n");
        text.push_str("  action:fs.list_dir path=./\n");
        text.push_str("  action:shell.run command=\"ls -la\"\n");
        text.push_str("  action:web.fetch_url url=https://example.com\n");
        text.push_str("  整理桌面\n");
        text
    }

    fn actions_text(&self) -> String {
        let mut lines: Vec<String> = self
            .registry
            .list_specs()
            .into_iter()
            .map(|spec| format!("{} v{} - {}", spec.name, spec.version, spec.description))
            .collect();
        lines.sort();
        lines.join("\n")
    }

    fn backend_text(&self) -> String {
        match (&self.ai_backend, &self.ai_backend_label) {
            (Some(_), Some(label)) => format!("ai backend: {label}"),
            (Some(_), None) => "ai backend: custom".to_string(),
            (None, Some(label)) => format!("ai backend: {label}"),
            (None, None) => "ai backend: none".to_string(),
        }
    }

    fn build_context(&self, room_cfg: &RoomConfig) -> ActionContext {
        let mut ctx = self.ctx.clone();
        if let Some(dry_run) = room_cfg.dry_run_default {
            ctx.dry_run = dry_run;
        }
        ctx
    }

    fn requires_approval(&self, spec: &ActionSpec, room_cfg: &RoomConfig) -> bool {
        if spec.requires_approval {
            return true;
        }

        if let Some(policy) = &room_cfg.risk_policy {
            if policy.low_auto_execute && spec.risk == RiskLevel::Low {
                return false;
            }
            return policy.approval_for.iter().any(|level| *level == spec.risk);
        }

        self.ctx
            .policy
            .requires_approval(spec.risk, spec.requires_approval)
    }

    fn wrap_response(&mut self, reply: OutboundMessage) -> ProtocolEvent {
        let kind = reply
            .metadata
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or("info")
            .to_string();
        ProtocolEvent::new(ProtocolBody::Response(ResponsePayload {
            in_reply_to: reply.in_reply_to.unwrap_or_default(),
            room_id: reply.channel,
            workspace_id: reply.workspace_id.unwrap_or_else(|| "default".to_string()),
            kind,
            text: reply.text,
            metadata: reply.metadata,
        }))
    }

    fn handle_approval_decision(
        &mut self,
        payload: ApprovalDecisionPayload,
    ) -> Vec<ProtocolEvent> {
        let Some(pending) = self.approvals.take(&payload.approval_id) else {
            return Vec::new();
        };
        let msg = InboundMessage {
            id: payload.in_reply_to.clone(),
            text: String::new(),
            sender: payload.sender_id,
            channel: payload.room_id,
            workspace_id: Some(payload.workspace_id),
            metadata: serde_json::Value::Null,
        };
        match payload.decision.as_str() {
            "approve" | "approve_all" | "approve-all" => {
                let mut plan_ctx = pending.plan;
                let has_plan = plan_ctx.is_some();
                if payload.decision != "approve" {
                    if let Some(plan) = plan_ctx.as_mut() {
                        plan.auto_approve = true;
                    }
                }
                let mut replies = self
                    .execute_action(&pending.request, &pending.spec, &msg, Some(pending.config.clone()));
                if let Some(plan) = plan_ctx {
                    let succeeded = replies.iter().any(|reply| {
                        reply
                            .metadata
                            .get("kind")
                            .and_then(|v| v.as_str())
                            == Some("action_result")
                    });
                    if succeeded {
                        let mut more = self.execute_plan_steps(
                            &msg,
                            plan.remaining,
                            pending.config,
                            plan.auto_approve,
                            Some(plan.plan_id),
                            plan.completed_steps + 1,
                            plan.total_steps,
                        );
                        replies.append(&mut more);
                    }
                }
                let filtered = if has_plan {
                    filter_plan_result_replies(replies)
                } else {
                    replies
                };
                filtered
                    .into_iter()
                    .map(|reply| self.wrap_response(reply))
                    .collect()
            }
            "deny" => {
                let reply = self.reply(
                    &msg,
                    format!("action '{}' cancelled", pending.spec.name),
                    "cancelled",
                    serde_json::Value::Null,
                );
                vec![self.wrap_response(reply)]
            }
            _ => Vec::new(),
        }
    }
}

fn parse_approval_command(input: &str) -> Option<(ApprovalDecision, Option<String>)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_lowercase();
    if lower == "yes" || lower == "y" || lower == "approve" {
        return Some((ApprovalDecision::Approve, None));
    }
    if is_affirmation(&lower) || is_followup_reference(&lower) {
        return Some((ApprovalDecision::ApproveAll, None));
    }
    if lower == "approve-all" || lower == "approve all" || lower == "approve plan" {
        return Some((ApprovalDecision::ApproveAll, None));
    }
    if lower == "no" || lower == "n" || lower == "deny" || lower == "reject" {
        return Some((ApprovalDecision::Deny, None));
    }

    if let Some(rest) = lower.strip_prefix("approve ") {
        return Some((ApprovalDecision::Approve, Some(rest.trim().to_string())));
    }
    if let Some(rest) = lower.strip_prefix("approve-all ") {
        return Some((ApprovalDecision::ApproveAll, Some(rest.trim().to_string())));
    }
    if let Some(rest) = lower.strip_prefix("approve all ") {
        return Some((ApprovalDecision::ApproveAll, Some(rest.trim().to_string())));
    }
    if let Some(rest) = lower.strip_prefix("approve plan ") {
        return Some((ApprovalDecision::ApproveAll, Some(rest.trim().to_string())));
    }
    if let Some(rest) = lower.strip_prefix("deny ") {
        return Some((ApprovalDecision::Deny, Some(rest.trim().to_string())));
    }

    None
}

struct PlanApprovalHint {
    plan_id: String,
    step_index: usize,
    total_steps: usize,
    allow_approve_all: bool,
}

fn format_approval_prompt(
    spec: &ActionSpec,
    params: &serde_json::Value,
    ctx: &ActionContext,
    approval_id: &str,
    preflight: Option<&PreflightReport>,
    plan_hint: Option<PlanApprovalHint>,
) -> String {
    let risk = match spec.risk {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
    };
    let params_text = format_params_compact(params);
    let preflight_text = preflight
        .map(|report| report.summary())
        .unwrap_or_else(|| "n/a".to_string());
    let mut text = format!(
        "需要审批：{name}\n描述：{desc}\n风险：{risk}  |  dry-run：{dry_run}\n预检：{preflight}\n参数：{params}",
        name = spec.name,
        desc = spec.description,
        risk = risk,
        dry_run = ctx.dry_run,
        preflight = preflight_text,
        params = params_text,
    );
    if let Some(hint) = plan_hint {
        text.push_str(&format!(
            "\n计划：{plan}  |  步骤：{step}/{total}",
            plan = hint.plan_id,
            step = hint.step_index,
            total = hint.total_steps
        ));
        if hint.allow_approve_all {
            text.push_str(&format!(
                "\n回复 approve-all {id} 一次性同意后续步骤",
                id = approval_id
            ));
        }
    }
    text.push_str(&format!(
        "\n回复 approve {id} 执行，或 deny {id} 取消",
        id = approval_id
    ));
    text
}

fn format_params_compact(params: &serde_json::Value) -> String {
    use serde_json::Value;
    match params {
        Value::Null => "none".to_string(),
        Value::Object(map) => {
            if map.is_empty() {
                return "none".to_string();
            }
            let mut parts = Vec::new();
            for (key, value) in map.iter().take(4) {
                parts.push(format!("{}={}", key, compact_value(value)));
            }
            if map.len() > 4 {
                parts.push("...".to_string());
            }
            parts.join(", ")
        }
        _ => compact_value(params),
    }
}

fn compact_value(value: &serde_json::Value) -> String {
    let raw = match value {
        serde_json::Value::String(text) => text.clone(),
        _ => value.to_string(),
    };
    if raw.len() > 60 {
        format!("{}...", &raw[..57])
    } else {
        raw
    }
}

fn extract_outcome_from_replies(replies: &[OutboundMessage]) -> Option<ActionOutcome> {
    for reply in replies {
        let kind = reply
            .metadata
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if kind != "action_result" {
            continue;
        }
        let summary = reply
            .text
            .strip_prefix("ok: ")
            .unwrap_or(reply.text.as_str())
            .to_string();
        let data = reply
            .metadata
            .get("data")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Null);
        return Some(ActionOutcome { summary, data });
    }
    None
}

fn plan_result_details(plan: &PlanProgress) -> String {
    let mut lines = Vec::new();
    for (idx, item) in plan.results.iter().enumerate() {
        let prefix = format!("Step {} ({})", idx + 1, item.action);
        if item.action == "shell.run" {
            let command = item
                .data
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let stdout = item
                .data
                .get("stdout")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            lines.push(format!(
                "{prefix}: command={command}\nstdout:\n{stdout}"
            ));
        } else {
            lines.push(format!("{prefix}: {}", item.summary));
        }
    }
    lines.join("\n")
}

fn format_plan_summary_fallback(plan: &PlanProgress) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "完成计划（{}/{} 步）：",
        plan.results.len(),
        plan.total_steps
    ));
    for item in &plan.results {
        if item.action == "shell.run" {
            let command = item
                .data
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let stdout = item
                .data
                .get("stdout")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let snippet = truncate_text(stdout, 1200);
            lines.push(format!("- command: {command}\n{snippet}"));
        } else {
            lines.push(format!("- {}: {}", item.action, item.summary));
        }
    }
    lines.join("\n")
}

fn summarize_system_status(plan: &PlanProgress) -> Option<String> {
    let mut uptime = None;
    let mut vm_stat = None;
    let mut df = None;
    let mut ps = None;
    let mut ifconfig = None;

    for item in &plan.results {
        if item.action != "shell.run" {
            continue;
        }
        let command = item
            .data
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let stdout = item
            .data
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match command {
            "uptime" => uptime = Some(stdout.to_string()),
            "vm_stat" => vm_stat = Some(stdout.to_string()),
            "df -h" => df = Some(stdout.to_string()),
            cmd if cmd.contains("ps aux") => ps = Some(stdout.to_string()),
            "ifconfig" => ifconfig = Some(stdout.to_string()),
            _ => {}
        }
    }

    if uptime.is_none() && vm_stat.is_none() && df.is_none() && ps.is_none() && ifconfig.is_none() {
        return None;
    }

    let mut lines = Vec::new();
    lines.push("系统状态摘要：".to_string());
    if let Some(uptime_out) = &uptime {
        let summary = parse_uptime_summary(uptime_out);
        lines.push(format!("- Uptime/Load: {summary}"));
    }
    if let Some(vm_stat_out) = &vm_stat {
        if let Some(mem) = parse_vm_stat_summary(vm_stat_out) {
            lines.push(format!(
                "- Memory: used {} / total {} (free {})",
                mem.used_gib, mem.total_gib, mem.free_gib
            ));
        }
    }
    if let Some(df_out) = &df {
        if let Some(disk) = parse_df_summary(df_out) {
            lines.push(format!(
                "- Disk {}: used {} / {} (avail {}, {} used)",
                disk.mount, disk.used, disk.size, disk.avail, disk.capacity
            ));
        }
    }
    if let Some(ps_out) = &ps {
        let top = parse_ps_summary(ps_out);
        if !top.is_empty() {
            lines.push(format!("- Top processes: {}", top));
        }
    }
    if let Some(if_out) = &ifconfig {
        let count = if_out.lines().filter(|line| !line.starts_with('\t') && line.contains(':')).count();
        if count > 0 {
            lines.push(format!("- Network interfaces: {}", count));
        }
    }

    lines.push("\n原始输出：".to_string());
    if let Some(uptime_out) = uptime {
        lines.push(format!("[uptime]\n{}", truncate_text(&uptime_out, 1200)));
    }
    if let Some(vm_stat_out) = vm_stat {
        lines.push(format!("[vm_stat]\n{}", truncate_text(&vm_stat_out, 1600)));
    }
    if let Some(df_out) = df {
        lines.push(format!("[df -h]\n{}", truncate_text(&df_out, 1200)));
    }
    if let Some(ps_out) = ps {
        lines.push(format!("[ps aux]\n{}", truncate_text(&ps_out, 1600)));
    }
    if let Some(if_out) = ifconfig {
        lines.push(format!("[ifconfig]\n{}", truncate_text(&if_out, 1200)));
    }

    Some(lines.join("\n"))
}

struct MemSummary {
    used_gib: String,
    free_gib: String,
    total_gib: String,
}

fn parse_vm_stat_summary(output: &str) -> Option<MemSummary> {
    let page_size = output
        .lines()
        .find_map(|line| {
            let marker = "page size of ";
            if let Some(idx) = line.find(marker) {
                let rest = &line[idx + marker.len()..];
                let bytes = rest.split_whitespace().next()?.trim();
                bytes.parse::<u64>().ok()
            } else {
                None
            }
        })
        .unwrap_or(4096);

    let mut counts = std::collections::HashMap::new();
    for line in output.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_lowercase();
            let value = value.trim().trim_end_matches('.').replace('.', "");
            if let Ok(num) = value.parse::<u64>() {
                counts.insert(key, num);
            }
        }
    }

    let free = counts.get("pages free").cloned().unwrap_or(0);
    let speculative = counts.get("pages speculative").cloned().unwrap_or(0);
    let active = counts.get("pages active").cloned().unwrap_or(0);
    let inactive = counts.get("pages inactive").cloned().unwrap_or(0);
    let wired = counts.get("pages wired down").cloned().unwrap_or(0);
    let compressed = counts
        .get("pages occupied by compressor")
        .cloned()
        .unwrap_or(0);

    let free_pages = free + speculative;
    let used_pages = active + inactive + wired + compressed;
    let total_pages = used_pages + free_pages;
    if total_pages == 0 {
        return None;
    }
    let to_gib = |pages: u64| -> f64 {
        let bytes = pages * page_size;
        bytes as f64 / 1024.0 / 1024.0 / 1024.0
    };
    let used_gib = to_gib(used_pages);
    let free_gib = to_gib(free_pages);
    let total_gib = to_gib(total_pages);
    Some(MemSummary {
        used_gib: format!("{:.2} GiB", used_gib),
        free_gib: format!("{:.2} GiB", free_gib),
        total_gib: format!("{:.2} GiB", total_gib),
    })
}

struct DiskSummary {
    mount: String,
    size: String,
    used: String,
    avail: String,
    capacity: String,
}

fn parse_df_summary(output: &str) -> Option<DiskSummary> {
    let mut best: Option<DiskSummary> = None;
    for line in output.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            continue;
        }
        let mount = parts[parts.len() - 1];
        let capacity = parts[parts.len() - 3];
        let avail = parts[parts.len() - 4];
        let used = parts[parts.len() - 5];
        let size = parts[parts.len() - 6];
        let summary = DiskSummary {
            mount: mount.to_string(),
            size: size.to_string(),
            used: used.to_string(),
            avail: avail.to_string(),
            capacity: capacity.to_string(),
        };
        if mount == "/System/Volumes/Data" {
            return Some(summary);
        }
        if mount == "/" {
            best = Some(summary);
        }
    }
    best
}

fn parse_uptime_summary(output: &str) -> String {
    let line = output.lines().next().unwrap_or("").trim();
    if let Some(idx) = line.find("load averages:") {
        let load = line[idx + "load averages:".len()..].trim();
        return format!("{line} (load {load})");
    }
    line.to_string()
}

fn parse_ps_summary(output: &str) -> String {
    let mut names = Vec::new();
    for line in output.lines().skip(1).take(5) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 11 {
            continue;
        }
        let cmd = parts[10];
        names.push(cmd.to_string());
    }
    names.join(", ")
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut out = text.to_string();
    out.truncate(limit);
    out.push_str("\n...[truncated]");
    out
}

fn filter_plan_result_replies(replies: Vec<OutboundMessage>) -> Vec<OutboundMessage> {
    let mut has_summary = false;
    for reply in &replies {
        let kind = reply
            .metadata
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if kind == "plan_completed" || kind == "plan_stopped" {
            has_summary = true;
            break;
        }
    }
    if !has_summary {
        return replies;
    }

    let mut summary: Option<OutboundMessage> = None;
    let mut keep: Vec<OutboundMessage> = Vec::new();
    for reply in replies.into_iter() {
        let kind = reply
            .metadata
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if kind == "plan_completed" || kind == "plan_stopped" {
            summary = Some(reply);
        } else if kind == "approval_request" {
            keep.push(reply);
        }
    }
    if let Some(summary) = summary {
        keep.push(summary);
    }
    keep
}

fn insert_param(mut params: serde_json::Value, key: &str, value: &str) -> serde_json::Value {
    match &mut params {
        serde_json::Value::Object(map) => {
            map.insert(key.to_string(), serde_json::Value::String(value.to_string()));
            params
        }
        _ => serde_json::json!({ key: value }),
    }
}

fn is_path_key(key: &str) -> bool {
    matches!(
        key.to_lowercase().as_str(),
        "path" | "dir" | "directory" | "cwd" | "folder"
    )
}

fn is_current_directory(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "." | "current" | "current dir" | "current directory" | "当前目录" | "当前文件夹"
    )
}

fn is_affirmation(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "yes" | "y" | "ok" | "okay" | "sure" | "好的" | "好" | "可以" | "行" | "嗯"
    )
}

fn is_followup_reference(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    let zh_matches = [
        "按你", "照你", "就按", "就照", "就这样", "就这么办", "按上面", "按刚才", "你说的", "上面说", "刚才说",
        "之前说", "那个", "那样", "就那个",
    ];
    let en_matches = [
        "as you said",
        "as you suggested",
        "do that",
        "do it",
        "that one",
        "the one you mentioned",
        "the thing you mentioned",
    ];
    zh_matches.iter().any(|kw| lower.contains(kw)) || en_matches.iter().any(|kw| lower.contains(kw))
}

fn last_assistant_message(history: &[AiChatMessage]) -> Option<String> {
    history
        .iter()
        .rev()
        .find(|msg| matches!(msg.role, AiChatRole::Assistant))
        .map(|msg| msg.content.clone())
}

fn last_user_message(history: &[AiChatMessage]) -> Option<String> {
    history
        .iter()
        .rev()
        .find(|msg| matches!(msg.role, AiChatRole::User))
        .map(|msg| msg.content.clone())
}

fn heuristic_plan_for(text: &str) -> Option<Vec<PlanStep>> {
    let lower = text.to_lowercase();
    let mut steps = Vec::new();
    let wants_status = lower.contains("系统状态")
        || lower.contains("system status")
        || lower.contains("status")
        || lower.contains("状态");
    let wants_cpu = lower.contains("cpu") || lower.contains("负载") || lower.contains("load");
    let wants_mem = lower.contains("内存") || lower.contains("memory");
    let wants_disk = lower.contains("磁盘") || lower.contains("disk");
    let wants_proc = lower.contains("进程") || lower.contains("process");
    let wants_net = lower.contains("网络") || lower.contains("network");

    if wants_status || wants_cpu {
        steps.push(PlanStep {
            id: Some("s1".to_string()),
            action: "shell.run".to_string(),
            params: json!({ "command": "uptime" }),
            note: Some("Check uptime / load".to_string()),
            requires_approval: Some(true),
        });
    }
    if wants_status || wants_mem {
        steps.push(PlanStep {
            id: Some("s2".to_string()),
            action: "shell.run".to_string(),
            params: json!({ "command": "vm_stat" }),
            note: Some("Check memory stats".to_string()),
            requires_approval: Some(true),
        });
    }
    if wants_status || wants_disk {
        steps.push(PlanStep {
            id: Some("s3".to_string()),
            action: "shell.run".to_string(),
            params: json!({ "command": "df -h" }),
            note: Some("Check disk usage".to_string()),
            requires_approval: Some(true),
        });
    }
    if wants_status || wants_proc {
        steps.push(PlanStep {
            id: Some("s4".to_string()),
            action: "shell.run".to_string(),
            params: json!({ "command": "ps aux | sort -nrk 3,3 | head -5" }),
            note: Some("Check top processes".to_string()),
            requires_approval: Some(true),
        });
    }
    if wants_net {
        steps.push(PlanStep {
            id: Some("s5".to_string()),
            action: "shell.run".to_string(),
            params: json!({ "command": "ifconfig" }),
            note: Some("Check network interfaces".to_string()),
            requires_approval: Some(true),
        });
    }

    if steps.is_empty() {
        None
    } else {
        Some(steps)
    }
}

#[derive(Clone, Default)]
struct RoomConfig {
    risk_policy: Option<RiskPolicyConfig>,
    action_allowlist: Option<HashSet<String>>,
    action_denylist: Option<HashSet<String>>,
    dry_run_default: Option<bool>,
}

impl RoomConfig {
    fn allows_action(&self, name: &str) -> bool {
        if let Some(deny) = &self.action_denylist {
            if deny.contains(name) {
                return false;
            }
        }
        if let Some(allow) = &self.action_allowlist {
            return allow.contains(name);
        }
        true
    }

    fn apply_override(&mut self, other: &RoomConfig) {
        if other.risk_policy.is_some() {
            self.risk_policy = other.risk_policy.clone();
        }
        if other.action_allowlist.is_some() {
            self.action_allowlist = other.action_allowlist.clone();
        }
        if other.action_denylist.is_some() {
            self.action_denylist = other.action_denylist.clone();
        }
        if other.dry_run_default.is_some() {
            self.dry_run_default = other.dry_run_default;
        }
    }
}

#[derive(Clone, Default)]
struct RiskPolicyConfig {
    low_auto_execute: bool,
    approval_for: Vec<RiskLevel>,
}

#[derive(Default)]
struct ConfigStore {
    global: RoomConfig,
    workspaces: HashMap<String, RoomConfig>,
    rooms: HashMap<(String, String), RoomConfig>,
}

impl ConfigStore {
    fn apply(&mut self, payload: ConfigUpdatePayload) {
        let (mode, scope) = (payload.mode.unwrap_or(ConfigMode::Merge), payload.scope);
        let new_config = RoomConfig {
            risk_policy: payload.risk_policy.map(|policy| RiskPolicyConfig {
                low_auto_execute: policy.low_auto_execute.unwrap_or(true),
                approval_for: policy.approval_for.unwrap_or_else(|| vec![RiskLevel::Medium, RiskLevel::High]),
            }),
            action_allowlist: payload
                .action_allowlist
                .map(|items| items.into_iter().collect()),
            action_denylist: payload
                .action_denylist
                .map(|items| items.into_iter().collect()),
            dry_run_default: payload.dry_run_default,
        };

        match scope {
            Some(scope) => {
                let ws = scope.workspace_id.clone();
                let room = scope.room_id.clone();
                if let (Some(ws), Some(room)) = (ws, room) {
                    Self::apply_to_target(&mut self.rooms, (ws, room), new_config, mode);
                } else if let Some(ws) = scope.workspace_id {
                    Self::apply_to_target(&mut self.workspaces, ws, new_config, mode);
                } else {
                    Self::apply_to_global(&mut self.global, new_config, mode);
                }
            }
            None => Self::apply_to_global(&mut self.global, new_config, mode),
        }
    }

    fn apply_to_global(base: &mut RoomConfig, new_config: RoomConfig, mode: ConfigMode) {
        match mode {
            ConfigMode::Replace => *base = new_config,
            ConfigMode::Merge => Self::merge_config(base, new_config),
        }
    }

    fn apply_to_target<K: std::hash::Hash + Eq>(
        map: &mut HashMap<K, RoomConfig>,
        key: K,
        new_config: RoomConfig,
        mode: ConfigMode,
    ) {
        match mode {
            ConfigMode::Replace => {
                map.insert(key, new_config);
            }
            ConfigMode::Merge => {
                let entry = map.entry(key).or_default();
                Self::merge_config(entry, new_config);
            }
        }
    }

    fn merge_config(base: &mut RoomConfig, new_config: RoomConfig) {
        if let Some(list) = new_config.action_allowlist {
            let allow = base.action_allowlist.get_or_insert_with(HashSet::new);
            allow.extend(list);
        }
        if let Some(list) = new_config.action_denylist {
            let deny = base.action_denylist.get_or_insert_with(HashSet::new);
            deny.extend(list);
        }
        if new_config.risk_policy.is_some() {
            base.risk_policy = new_config.risk_policy;
        }
        if new_config.dry_run_default.is_some() {
            base.dry_run_default = new_config.dry_run_default;
        }
    }

    fn effective_for(&self, workspace_id: &str, room_id: &str) -> RoomConfig {
        let mut config = self.global.clone();
        if let Some(ws) = self.workspaces.get(workspace_id) {
            config.apply_override(ws);
        }
        if let Some(room) = self.rooms.get(&(workspace_id.to_string(), room_id.to_string())) {
            config.apply_override(room);
        }
        config
    }
}

#[derive(Default)]
struct RoomScope {
    enforced: bool,
    allowed: HashSet<(String, String)>,
}

impl RoomScope {
    fn update(&mut self, payload: RoomScopePayload) {
        let mode = payload.mode.unwrap_or(ConfigMode::Replace);
        if matches!(mode, ConfigMode::Replace) {
            self.allowed.clear();
        }
        for ws in payload.workspaces {
            for room in ws.rooms {
                self.allowed
                    .insert((ws.workspace_id.clone(), room.room_id));
            }
        }
        self.enforced = true;
    }

    fn allows(&self, workspace_id: &str, room_id: &str) -> bool {
        if !self.enforced {
            return true;
        }
        self.allowed
            .contains(&(workspace_id.to_string(), room_id.to_string()))
    }
}
