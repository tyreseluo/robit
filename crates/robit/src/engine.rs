use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::adapter::Adapter;
use crate::ai::{AiChatMessage, AiChatRole, AiDecision, AiPlanner};
use crate::protocol::{
    ActionListResultPayload, ApprovalDecisionPayload, ConfigMode, ConfigUpdatePayload,
    ProtocolBody, ProtocolEvent, ResponsePayload, RoomScopePayload,
};
use crate::policy::ActionContext;
use crate::types::{
    ActionOutcome, ActionRequest, ActionSpec, InboundMessage, OutboundMessage, PlannerResponse,
    RiskLevel,
};
use crate::{ActionRegistry, Policy, RulePlanner};

struct PendingAction {
    request: ActionRequest,
    spec: ActionSpec,
    sender: String,
    config: RoomConfig,
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
    Deny,
}

pub struct Engine {
    registry: ActionRegistry,
    planner: RulePlanner,
    ai_backend: Option<std::sync::Arc<dyn AiPlanner>>,
    ai_backend_label: Option<String>,
    ctx: ActionContext,
    approvals: ApprovalStore,
    next_message_id: u64,
    seen_messages: HashSet<String>,
    scope: RoomScope,
    config_store: ConfigStore,
    conversations: ConversationStore,
    conversation_persist_path: Option<PathBuf>,
}

impl Engine {
    pub fn new(registry: ActionRegistry, planner: RulePlanner, policy: Policy) -> Result<Self> {
        let cwd = std::env::current_dir()?;
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
            approvals: ApprovalStore::new(),
            next_message_id: 1,
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

        if let Some(response) = self.handle_control(&msg) {
            self.record_exchange_and_persist(&convo_key, text, &[response.clone()]);
            return vec![response];
        }

        if let Some(response) = self.handle_approval(&msg) {
            self.record_exchange_and_persist(&convo_key, text, &response);
            return response;
        }

        let history = self.conversations.history_for(&convo_key);
        if let Some(ai_backend) = &self.ai_backend {
            match ai_backend.plan_with_history(text, &self.registry.list_specs(), &history) {
                Ok(AiDecision::Action(request)) => {
                    let replies = self.handle_action_request(&msg, request, room_cfg);
                    self.record_exchange_and_persist(&convo_key, text, &replies);
                    return replies;
                }
                Ok(AiDecision::NeedInput { prompt }) => {
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
                Ok(AiDecision::Unknown { message }) => {
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
                let replies = self.handle_action_request(&msg, request, room_cfg);
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
        let (decision, id) = parse_approval_command(msg.text.trim())?;

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
            ApprovalDecision::Approve => {
                let outcomes = self.execute_action(
                    &pending.request,
                    &pending.spec,
                    msg,
                    Some(pending.config),
                );
                Some(outcomes)
            }
        }
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
            );
            let text = format_approval_prompt(&spec, &params_snapshot, &ctx, &approval_id);
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
        text.push_str("  deny <id>      deny pending action\n\n");
        text.push_str("examples:\n");
        text.push_str("  action:rust.new_project path=./ name=demo run=false\n");
        text.push_str("  新建一个rust项目 在 ./ 下 名为 demo\n");
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
            "approve" => self
                .execute_action(&pending.request, &pending.spec, &msg, Some(pending.config))
                .into_iter()
                .map(|reply| self.wrap_response(reply))
                .collect(),
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
    if lower == "no" || lower == "n" || lower == "deny" || lower == "reject" {
        return Some((ApprovalDecision::Deny, None));
    }

    if let Some(rest) = lower.strip_prefix("approve ") {
        return Some((ApprovalDecision::Approve, Some(rest.trim().to_string())));
    }
    if let Some(rest) = lower.strip_prefix("deny ") {
        return Some((ApprovalDecision::Deny, Some(rest.trim().to_string())));
    }

    None
}

fn format_approval_prompt(
    spec: &ActionSpec,
    params: &serde_json::Value,
    ctx: &ActionContext,
    approval_id: &str,
) -> String {
    let risk = match spec.risk {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
    };
    let params_text = format_params_compact(params);
    format!(
        "需要审批：{name}\n描述：{desc}\n风险：{risk}  |  dry-run：{dry_run}\n参数：{params}\n回复 approve {id} 执行，或 deny {id} 取消",
        name = spec.name,
        desc = spec.description,
        risk = risk,
        dry_run = ctx.dry_run,
        params = params_text,
        id = approval_id
    )
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
