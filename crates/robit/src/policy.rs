use anyhow::{anyhow, Result};
use std::env;
use std::path::{Path, PathBuf};

use crate::types::RiskLevel;

#[derive(Clone, Debug)]
pub struct Policy {
    pub allowed_roots: Vec<PathBuf>,
    pub approval_risk_levels: Vec<RiskLevel>,
}

#[derive(Clone, Debug)]
pub struct ActionContext {
    pub cwd: PathBuf,
    pub dry_run: bool,
    pub policy: Policy,
}

impl Policy {
    pub fn default_with_home() -> Self {
        let mut roots = Vec::new();
        if let Ok(cwd) = env::current_dir() {
            roots.push(cwd);
        }
        if let Ok(home) = env::var("HOME") {
            roots.push(PathBuf::from(home));
        }
        Self {
            allowed_roots: roots,
            approval_risk_levels: vec![RiskLevel::Medium, RiskLevel::High],
        }
    }

    pub fn requires_approval(&self, risk: RiskLevel, explicit: bool) -> bool {
        if explicit {
            return true;
        }
        self.approval_risk_levels.iter().any(|level| *level == risk)
    }

    pub fn check_path_allowed(&self, path: &Path) -> Result<()> {
        let canonical = if path.exists() {
            path.canonicalize()
                .map_err(|err| anyhow!("failed to canonicalize path: {err}"))?
        } else {
            path.to_path_buf()
        };

        for root in &self.allowed_roots {
            let root_canonical = if root.exists() {
                root.canonicalize()
                    .map_err(|err| anyhow!("failed to canonicalize root: {err}"))?
            } else {
                root.to_path_buf()
            };
            if canonical.starts_with(&root_canonical) {
                return Ok(());
            }
        }

        Err(anyhow!(
            "path not allowed by policy: {}",
            canonical.display()
        ))
    }
}
