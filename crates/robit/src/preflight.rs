use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::policy::ActionContext;
use crate::types::{ActionSpec, RiskLevel};
use crate::utils::{clean_path, expand_tilde};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreflightConfig {
    pub enabled: bool,
    pub strict: bool,
    pub allowed_capabilities: Vec<String>,
    pub denied_capabilities: Vec<String>,
    pub blocked_roots: Vec<PathBuf>,
    pub enforce_policy_roots: bool,
    pub path_keys: Vec<String>,
}

impl Default for PreflightConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            strict: true,
            allowed_capabilities: Vec::new(),
            denied_capabilities: Vec::new(),
            blocked_roots: Vec::new(),
            enforce_policy_roots: true,
            path_keys: vec![
                "path".to_string(),
                "dir".to_string(),
                "directory".to_string(),
                "cwd".to_string(),
                "file".to_string(),
                "target".to_string(),
                "src".to_string(),
                "dst".to_string(),
                "source".to_string(),
                "destination".to_string(),
            ],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreflightReport {
    pub action: String,
    pub risk: RiskLevel,
    pub requires_approval: bool,
    pub allowed: bool,
    pub reasons: Vec<String>,
    pub capabilities: Vec<String>,
    pub paths: Vec<String>,
}

impl PreflightReport {
    pub fn summary(&self) -> String {
        if self.allowed {
            "ok".to_string()
        } else if self.reasons.is_empty() {
            "blocked".to_string()
        } else {
            format!("blocked: {}", self.reasons.join("; "))
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreflightEngine {
    config: PreflightConfig,
}

impl PreflightEngine {
    pub fn new(config: PreflightConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &PreflightConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: PreflightConfig) {
        self.config = config;
    }

    pub fn check(
        &self,
        spec: &ActionSpec,
        params: &Value,
        ctx: &ActionContext,
    ) -> Result<PreflightReport> {
        if !self.config.enabled {
            return Ok(PreflightReport {
                action: spec.name.clone(),
                risk: spec.risk,
                requires_approval: spec.requires_approval,
                allowed: true,
                reasons: Vec::new(),
                capabilities: spec.capabilities.clone(),
                paths: Vec::new(),
            });
        }

        let allowed_set: HashSet<String> = self
            .config
            .allowed_capabilities
            .iter()
            .map(|cap| cap.to_lowercase())
            .collect();
        let denied_set: HashSet<String> = self
            .config
            .denied_capabilities
            .iter()
            .map(|cap| cap.to_lowercase())
            .collect();

        let mut reasons = Vec::new();

        for cap in &spec.capabilities {
            let cap_norm = cap.to_lowercase();
            if denied_set.contains(&cap_norm) {
                reasons.push(format!("capability denied: {cap}"));
            }
            if !allowed_set.is_empty() && !allowed_set.contains(&cap_norm) {
                reasons.push(format!("capability not allowed: {cap}"));
            }
        }

        let paths = collect_paths(params, &self.config.path_keys);
        let mut normalized_paths = Vec::new();
        for raw in &paths {
            let expanded = expand_tilde(raw);
            let normalized = clean_path(&expanded);
            normalized_paths.push(normalized.clone());

            for blocked in &self.config.blocked_roots {
                let blocked_norm = clean_path(&expand_tilde(&blocked.to_string_lossy()));
                if is_under(&normalized, &blocked_norm) {
                    reasons.push(format!(
                        "path blocked by policy: {}",
                        normalized.display()
                    ));
                }
            }

            if self.config.enforce_policy_roots {
                if let Err(err) = ctx.policy.check_path_allowed(&normalized) {
                    reasons.push(format!("path not allowed: {}", err));
                }
            }
        }

        let allowed = reasons.is_empty();
        let report = PreflightReport {
            action: spec.name.clone(),
            risk: spec.risk,
            requires_approval: spec.requires_approval,
            allowed,
            reasons,
            capabilities: spec.capabilities.clone(),
            paths: normalized_paths
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect(),
        };

        Ok(report)
    }
}

fn collect_paths(value: &Value, path_keys: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    collect_paths_inner(value, None, path_keys, &mut out);
    out
}

fn collect_paths_inner(
    value: &Value,
    current_key: Option<&str>,
    path_keys: &[String],
    out: &mut Vec<String>,
) {
    match value {
        Value::String(text) => {
            if let Some(key) = current_key {
                if path_keys.iter().any(|allowed| allowed.eq_ignore_ascii_case(key)) {
                    out.push(text.to_string());
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_paths_inner(item, current_key, path_keys, out);
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                collect_paths_inner(child, Some(key.as_str()), path_keys, out);
            }
        }
        _ => {}
    }
}

fn is_under(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}
