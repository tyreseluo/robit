pub mod adapter;
pub mod actions;
pub mod engine;
pub mod protocol;
pub mod planner;
pub mod policy;
pub mod types;
pub mod utils;

pub use actions::{ActionHandler, ActionRegistry};
pub use engine::Engine;
pub use protocol::{
    ActionListRequestPayload, ActionListResultPayload, ApprovalDecisionPayload, ConfigMode,
    ConfigScope, ConfigUpdatePayload, MessagePayload, PingPayload, PongPayload, ProtocolBody,
    ProtocolEvent, ProviderBinding, ResponsePayload, RiskPolicy, RoomScopePayload, RoomScopeItem,
    WorkspaceScope,
};
pub use planner::RulePlanner;
pub use policy::{ActionContext, Policy};
pub use types::{
    ActionOutcome, ActionRequest, ActionSpec, InboundMessage, OutboundMessage, PlannerResponse,
    RiskLevel,
};
