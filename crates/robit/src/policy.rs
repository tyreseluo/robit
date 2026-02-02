use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};

use crate::types::RiskLevel;
use crate::utils::expand_tilde;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PolicyConfig {
    pub allowed_roots: Option<Vec<String>>,
    pub approval_risk_levels: Option<Vec<String>>,
}

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

    pub fn apply_config(self, config: PolicyConfig) -> Result<Self> {
        let mut policy = self;
        if let Some(roots) = config.allowed_roots {
            policy.allowed_roots = roots.into_iter().map(|root| expand_tilde(&root)).collect();
        }
        if let Some(levels) = config.approval_risk_levels {
            let mut parsed = Vec::new();
            for level in levels {
                parsed.push(parse_risk_level(&level)?);
            }
            policy.approval_risk_levels = parsed;
        }
        Ok(policy)
    }
}

fn parse_risk_level(raw: &str) -> Result<RiskLevel> {
    match raw.trim().to_lowercase().as_str() {
        "low" => Ok(RiskLevel::Low),
        "medium" => Ok(RiskLevel::Medium),
        "high" => Ok(RiskLevel::High),
        other => Err(anyhow!("unknown risk level: {}", other)),
    }
}
