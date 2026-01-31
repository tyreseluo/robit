use std::collections::HashMap;

use anyhow::Result;
use serde_json::json;

use crate::adapter::Adapter;
use crate::policy::ActionContext;
use crate::types::{
    ActionOutcome, ActionRequest, ActionSpec, InboundMessage, OutboundMessage, PlannerResponse,
};
use crate::{ActionRegistry, Policy, RulePlanner};

struct PendingAction {
    request: ActionRequest,
    spec: ActionSpec,
    sender: String,
    channel: String,
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
        channel: &str,
        request: ActionRequest,
        spec: ActionSpec,
    ) -> String {
        let id = format!("appr-{}", self.next_id);
        self.next_id += 1;
        self.pending.insert(
            id.clone(),
            PendingAction {
                request,
                spec,
                sender: sender.to_string(),
                channel: channel.to_string(),
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

#[derive(Clone, Copy)]
enum ApprovalDecision {
    Approve,
    Deny,
}

pub struct Engine {
    registry: ActionRegistry,
    planner: RulePlanner,
    ctx: ActionContext,
    approvals: ApprovalStore,
    next_message_id: u64,
}

impl Engine {
    pub fn new(registry: ActionRegistry, planner: RulePlanner, policy: Policy) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        Ok(Self {
            registry,
            planner,
            ctx: ActionContext {
                cwd,
                dry_run: true,
                policy,
            },
            approvals: ApprovalStore::new(),
            next_message_id: 1,
        })
    }

    pub fn handle_message(&mut self, msg: InboundMessage) -> Vec<OutboundMessage> {
        let text = msg.text.trim();
        if text.is_empty() {
            return Vec::new();
        }

        if let Some(response) = self.handle_control(&msg) {
            return vec![response];
        }

        if let Some(response) = self.handle_approval(&msg) {
            return response;
        }

        match self.planner.plan(text) {
            PlannerResponse::Action(request) => self.handle_action_request(&msg, request),
            PlannerResponse::NeedInput { prompt } => vec![self.reply(
                &msg,
                prompt,
                "need_input",
                serde_json::Value::Null,
            )],
            PlannerResponse::Unknown { message } => vec![self.reply(
                &msg,
                format!("no plan: {message}"),
                "unknown",
                serde_json::Value::Null,
            )],
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
                let outcomes = self.execute_action(&pending.request, &pending.spec, msg);
                Some(outcomes)
            }
        }
    }

    fn handle_action_request(
        &mut self,
        msg: &InboundMessage,
        request: ActionRequest,
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
        let needs_approval = self
            .ctx
            .policy
            .requires_approval(spec.risk, spec.requires_approval);

        if let Err(err) = action.validate(&self.ctx, &request.params) {
            return vec![self.reply(
                msg,
                format!("validation failed: {err}"),
                "error",
                serde_json::Value::Null,
            )];
        }

        if needs_approval {
            let approval_id = self.approvals.create(
                &msg.sender,
                &msg.channel,
                request,
                spec.clone(),
            );
            let text = format!(
                "approval required for '{}'. reply 'approve {}' or 'deny {}'",
                spec.name, approval_id, approval_id
            );
            return vec![self.reply(
                msg,
                text,
                "approval_request",
                json!({"approval_id": approval_id}),
            )];
        }

        self.execute_action(&request, &spec, msg)
    }

    fn execute_action(
        &mut self,
        request: &ActionRequest,
        spec: &ActionSpec,
        msg: &InboundMessage,
    ) -> Vec<OutboundMessage> {
        let Some(action) = self.registry.get(&request.name) else {
            return vec![self.reply(
                msg,
                format!("unknown action: {}", request.name),
                "error",
                serde_json::Value::Null,
            )];
        };

        if let Err(err) = action.validate(&self.ctx, &request.params) {
            return vec![self.reply(
                msg,
                format!("validation failed: {err}"),
                "error",
                serde_json::Value::Null,
            )];
        }

        match action.execute(&self.ctx, &request.params) {
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

    fn help_text(&self) -> String {
        let mut text = String::new();
        text.push_str("commands:\n");
        text.push_str("  help           show this help\n");
        text.push_str("  actions        list actions\n");
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
