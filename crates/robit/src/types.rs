use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionSpec {
    pub name: String,
    pub version: String,
    pub description: String,
    pub params_schema: Value,
    pub result_schema: Value,
    pub risk: RiskLevel,
    pub requires_approval: bool,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionRequest {
    pub name: String,
    pub params: Value,
    pub raw_input: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: Option<String>,
    pub action: String,
    pub params: Value,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub requires_approval: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionOutcome {
    pub summary: String,
    pub data: Value,
}

#[derive(Clone, Debug)]
pub enum PlannerResponse {
    Action(ActionRequest),
    NeedInput { prompt: String },
    Unknown { message: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InboundMessage {
    pub id: String,
    pub text: String,
    pub sender: String,
    pub channel: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    pub metadata: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub id: String,
    pub in_reply_to: Option<String>,
    pub text: String,
    pub recipient: String,
    pub channel: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    pub metadata: Value,
}
