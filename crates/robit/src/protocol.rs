use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{ActionSpec, RiskLevel};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProtocolEvent {
    pub schema_version: String,
    pub id: String,
    pub timestamp: Option<String>,
    #[serde(flatten)]
    pub body: ProtocolBody,
}

impl ProtocolEvent {
    pub fn new(body: ProtocolBody) -> Self {
        Self {
            schema_version: "robit.v1".to_string(),
            id: format!("evt-{}", uuid()),
            timestamp: None,
            body,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ProtocolBody {
    Message(MessagePayload),
    Response(ResponsePayload),
    ConfigUpdate(ConfigUpdatePayload),
    RoomScope(RoomScopePayload),
    ActionListRequest(ActionListRequestPayload),
    ActionListResult(ActionListResultPayload),
    ApprovalDecision(ApprovalDecisionPayload),
    Ping(PingPayload),
    Pong(PongPayload),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessagePayload {
    pub message_id: String,
    pub room_id: String,
    pub workspace_id: String,
    pub sender_id: String,
    pub text: String,
    #[serde(default)]
    pub event_kind: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponsePayload {
    pub in_reply_to: String,
    pub room_id: String,
    pub workspace_id: String,
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigUpdatePayload {
    pub scope: Option<ConfigScope>,
    pub mode: Option<ConfigMode>,
    pub provider_binding: Option<ProviderBinding>,
    pub risk_policy: Option<RiskPolicy>,
    pub action_allowlist: Option<Vec<String>>,
    pub action_denylist: Option<Vec<String>>,
    pub dry_run_default: Option<bool>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigScope {
    pub workspace_id: Option<String>,
    pub room_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConfigMode {
    Merge,
    Replace,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderBinding {
    pub model: String,
    pub temperature: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskPolicy {
    pub low_auto_execute: Option<bool>,
    pub approval_for: Option<Vec<RiskLevel>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomScopePayload {
    pub mode: Option<ConfigMode>,
    pub workspaces: Vec<WorkspaceScope>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceScope {
    pub workspace_id: String,
    pub name: Option<String>,
    pub rooms: Vec<RoomScopeItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomScopeItem {
    pub room_id: String,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionListRequestPayload {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionListResultPayload {
    pub actions: Vec<ActionSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalDecisionPayload {
    pub approval_id: String,
    pub decision: String,
    pub room_id: String,
    pub workspace_id: String,
    pub sender_id: String,
    pub in_reply_to: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PingPayload {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PongPayload {
    pub in_reply_to: String,
}

fn uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{now:x}")
}
