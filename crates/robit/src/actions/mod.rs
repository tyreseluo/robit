use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::policy::ActionContext;
use crate::types::{ActionOutcome, ActionSpec};

pub mod fs_organize;
pub mod rust_project;

pub trait ActionHandler: Send + Sync {
    fn name(&self) -> &'static str;
    fn spec(&self) -> ActionSpec;
    fn validate(&self, ctx: &ActionContext, params: &Value) -> Result<()>;
    fn execute(&self, ctx: &ActionContext, params: &Value) -> Result<ActionOutcome>;
}

#[derive(Default)]
pub struct ActionRegistry {
    actions: HashMap<String, Arc<dyn ActionHandler>>,
}

impl ActionRegistry {
    pub fn new() -> Self {
        Self {
            actions: HashMap::new(),
        }
    }

    pub fn register<A: ActionHandler + 'static>(&mut self, action: A) {
        self.actions
            .insert(action.name().to_string(), Arc::new(action));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ActionHandler>> {
        self.actions.get(name).cloned()
    }

    pub fn list_specs(&self) -> Vec<ActionSpec> {
        self.actions.values().map(|action| action.spec()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}
