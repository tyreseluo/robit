use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use crate::policy::{Policy, PolicyConfig};
use crate::preflight::PreflightConfig;

#[derive(Debug, Deserialize)]
struct RobitConfigFile {
    preflight: Option<PreflightConfig>,
    policy: Option<PolicyConfig>,
}

pub(crate) fn load_default_config(
    base_policy: Policy,
    base_preflight: PreflightConfig,
) -> Result<(Policy, PreflightConfig)> {
    let Some(path) = default_config_path() else {
        return Ok((base_policy, base_preflight));
    };
    load_config_from_path(&path, base_policy, base_preflight)
}

fn load_config_from_path(
    path: &Path,
    base_policy: Policy,
    base_preflight: PreflightConfig,
) -> Result<(Policy, PreflightConfig)> {
    if !path.exists() {
        return Ok((base_policy, base_preflight));
    }
    let content = fs::read_to_string(path)?;
    let parsed: RobitConfigFile = toml::from_str(&content)?;
    let policy = if let Some(cfg) = parsed.policy {
        base_policy.apply_config(cfg)?
    } else {
        base_policy
    };
    let preflight = parsed.preflight.unwrap_or(base_preflight);
    Ok((policy, preflight))
}

fn default_config_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("ROBIT_CONFIG_PATH") {
        if !path.trim().is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let local = PathBuf::from("configs/policy.toml");
    if local.exists() {
        return Some(local);
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(repo_root) = manifest_dir.parent().and_then(|parent| parent.parent()) {
        let candidate = repo_root.join("configs").join("policy.toml");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}
