use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

use crate::policy::ActionContext;
use crate::types::{ActionOutcome, ActionSpec, RiskLevel};
use crate::utils::{clean_path, expand_tilde};

const SORTED_DIR: &str = "robit_sorted";

#[derive(Default)]
pub struct OrganizeDirectoryAction;

#[derive(Deserialize)]
struct OrganizeParams {
    path: String,
    mode: Option<String>,
    dry_run: Option<bool>,
}

impl OrganizeDirectoryAction {
    fn parse_params(&self, params: &serde_json::Value) -> Result<OrganizeParams> {
        serde_json::from_value(params.clone()).map_err(|err| anyhow!("invalid params: {err}"))
    }

    fn bucket_for(path: &Path) -> String {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if !ext.is_empty() => ext.to_lowercase(),
            _ => "no_ext".to_string(),
        }
    }

    fn ensure_unique_destination(dest: &Path, file_name: &str) -> PathBuf {
        let mut candidate = dest.join(file_name);
        if !candidate.exists() {
            return candidate;
        }

        let mut index = 1;
        loop {
            let alt_name = format!("{file_name}-{index}");
            candidate = dest.join(&alt_name);
            if !candidate.exists() {
                return candidate;
            }
            index += 1;
        }
    }
}

impl crate::actions::ActionHandler for OrganizeDirectoryAction {
    fn name(&self) -> &'static str {
        "fs.organize_directory"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "Organize files in a directory by extension.".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "mode": { "type": "string", "enum": ["extension"] },
                    "dry_run": { "type": "boolean" }
                },
                "required": ["path"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "moved": { "type": "integer" },
                    "buckets": { "type": "array", "items": { "type": "string" } },
                    "destination": { "type": "string" },
                    "dry_run": { "type": "boolean" }
                }
            }),
            risk: RiskLevel::Medium,
            requires_approval: true,
            capabilities: vec!["filesystem".to_string()],
        }
    }

    fn validate(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<()> {
        let params = self.parse_params(params)?;
        let target = expand_tilde(&params.path);
        ctx.policy.check_path_allowed(&target)?;
        if !target.exists() {
            return Err(anyhow!("path does not exist: {}", target.display()));
        }
        if !target.is_dir() {
            return Err(anyhow!("path is not a directory: {}", target.display()));
        }
        let mode = params.mode.unwrap_or_else(|| "extension".to_string());
        if mode != "extension" {
            return Err(anyhow!("unsupported mode: {mode}"));
        }
        Ok(())
    }

    fn execute(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<ActionOutcome> {
        let params = self.parse_params(params)?;
        let target = clean_path(&expand_tilde(&params.path));
        let dry_run = ctx.dry_run || params.dry_run.unwrap_or(false);

        let sorted_root = target.join(SORTED_DIR);
        let mut moved = 0usize;
        let mut buckets = Vec::new();

        for entry in fs::read_dir(&target)? {
            let entry = entry?;
            let path = entry.path();
            if path.file_name().and_then(|name| name.to_str()).map_or(false, |name| name.starts_with('.')) {
                continue;
            }
            if path.is_dir() {
                if path == sorted_root {
                    continue;
                }
                continue;
            }

            let bucket = Self::bucket_for(&path);
            if !buckets.contains(&bucket) {
                buckets.push(bucket.clone());
            }
            let dest_dir = sorted_root.join(&bucket);
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("invalid file name"))?;
            let dest_path = Self::ensure_unique_destination(&dest_dir, file_name);

            if !dry_run {
                fs::create_dir_all(&dest_dir)?;
                fs::rename(&path, &dest_path)?;
            }
            moved += 1;
        }

        let summary = if dry_run {
            format!(
                "dry run: would organize {moved} files into {} buckets at {}",
                buckets.len(),
                sorted_root.display()
            )
        } else {
            format!(
                "organized {moved} files into {} buckets at {}",
                buckets.len(),
                sorted_root.display()
            )
        };

        Ok(ActionOutcome {
            summary,
            data: json!({
                "moved": moved,
                "buckets": buckets,
                "destination": sorted_root.to_string_lossy(),
                "dry_run": dry_run,
            }),
        })
    }
}
