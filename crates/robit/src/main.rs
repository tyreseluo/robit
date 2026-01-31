use anyhow::Result;

use robit::actions::fs_organize::OrganizeDirectoryAction;
use robit::actions::rust_project::RustProjectAction;
use robit::adapter::stdin::StdinAdapter;
use robit::adapter::Adapter;
use robit::{ActionRegistry, Engine, Policy, RulePlanner};

fn main() -> Result<()> {
    let mut registry = ActionRegistry::new();
    registry.register(OrganizeDirectoryAction::default());
    registry.register(RustProjectAction::default());

    let planner = RulePlanner::new();
    let policy = Policy::default_with_home();
    let mut engine = Engine::new(registry, planner, policy)?;

    println!("robit stdin ready. type 'help' for commands. ctrl-d to exit.");

    let mut adapter = StdinAdapter::new();
    engine.run_with_adapter(&mut adapter)
}
