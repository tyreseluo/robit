use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::policy::ActionContext;
use crate::types::{ActionOutcome, ActionSpec};

pub mod fs_organize;
pub mod fs_ops;
pub mod shell;
pub mod browser;
#[cfg(feature = "web")]
pub mod web;

pub fn default_registry() -> ActionRegistry {
    let mut registry = ActionRegistry::new();
    registry.register(fs_organize::OrganizeDirectoryAction::default());
    registry.register(fs_ops::ReadFileAction::default());
    registry.register(fs_ops::WriteFileAction::default());
    registry.register(fs_ops::ReplaceTextAction::default());
    registry.register(fs_ops::ListDirAction::default());
    registry.register(fs_ops::EnsureDirAction::default());
    registry.register(shell::ShellRunAction::default());
    registry.register(browser::BrowserOpenUrlAction::default());
    #[cfg(feature = "web")]
    {
        registry.register(web::FetchUrlAction::default());
        registry.register(web::BraveSearchAction::default());
    }
    registry
}

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
