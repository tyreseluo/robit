pub mod adapter;
pub mod actions;
pub mod ai;
pub mod config;
pub mod engine;
pub mod protocol;
pub mod planner;
pub mod policy;
pub mod preflight;
pub mod types;
pub mod utils;

pub use actions::{ActionHandler, ActionRegistry};
pub use actions::default_registry;
pub use ai::{AiChatMessage, AiChatRole, AiDecision, AiPlanner};
#[cfg(feature = "ai-http")]
pub use ai::{AiClient, AiConfig, AiProvider};
#[cfg(feature = "ai-omnix-mlx")]
pub use ai::{MlxQwenClient, MlxQwenConfig};
pub use engine::Engine;
pub use preflight::{PreflightConfig, PreflightEngine, PreflightReport};
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
    PlanStep, RiskLevel,
};
