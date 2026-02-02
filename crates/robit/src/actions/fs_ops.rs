use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use crate::policy::ActionContext;
use crate::types::{ActionOutcome, ActionSpec, RiskLevel};
use crate::utils::{clean_path, expand_tilde};

#[derive(Default)]
pub struct ReadFileAction;

#[derive(Default)]
pub struct WriteFileAction;

#[derive(Default)]
pub struct ReplaceTextAction;

#[derive(Default)]
pub struct ListDirAction;

#[derive(Default)]
pub struct EnsureDirAction;

#[derive(Deserialize)]
struct ReadFileParams {
    path: String,
    max_chars: Option<usize>,
}

#[derive(Deserialize)]
struct WriteFileParams {
    path: String,
    content: String,
    mode: Option<String>,
    create_parents: Option<bool>,
    dry_run: Option<bool>,
}

#[derive(Deserialize)]
struct ReplaceTextParams {
    path: String,
    find: String,
    replace: String,
    all: Option<bool>,
    count: Option<usize>,
    dry_run: Option<bool>,
}

#[derive(Deserialize)]
struct ListDirParams {
    path: String,
    include_hidden: Option<bool>,
    max_entries: Option<usize>,
}

#[derive(Deserialize)]
struct EnsureDirParams {
    path: String,
    create_parents: Option<bool>,
    dry_run: Option<bool>,
}

fn parse_params<T: DeserializeOwned>(params: &serde_json::Value) -> Result<T> {
    serde_json::from_value(params.clone()).map_err(|err| anyhow!("invalid params: {err}"))
}

fn resolve_path(raw: &str) -> PathBuf {
    clean_path(&expand_tilde(raw))
}

fn ensure_allowed_path(ctx: &ActionContext, path: &Path) -> Result<()> {
    ctx.policy.check_path_allowed(path)
}

impl crate::actions::ActionHandler for ReadFileAction {
    fn name(&self) -> &'static str {
        "fs.read_file"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "Read a text file (optionally truncated).".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "max_chars": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "truncated": { "type": "boolean" },
                    "chars": { "type": "integer" },
                    "total_chars": { "type": "integer" }
                }
            }),
            risk: RiskLevel::Low,
            requires_approval: false,
            capabilities: vec!["filesystem".to_string()],
        }
    }

    fn validate(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<()> {
        let params: ReadFileParams = parse_params(params)?;
        let path = resolve_path(&params.path);
        ensure_allowed_path(ctx, &path)?;
        if !path.exists() {
            return Err(anyhow!("path does not exist: {}", path.display()));
        }
        if !path.is_file() {
            return Err(anyhow!("path is not a file: {}", path.display()));
        }
        Ok(())
    }

    fn execute(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<ActionOutcome> {
        let params: ReadFileParams = parse_params(params)?;
        let path = resolve_path(&params.path);
        ensure_allowed_path(ctx, &path)?;

        let content = fs::read_to_string(&path)?;
        let total_chars = content.chars().count();
        let max_chars = params.max_chars.unwrap_or(20_000).max(1);
        let truncated = total_chars > max_chars;
        let output = if truncated {
            content.chars().take(max_chars).collect::<String>()
        } else {
            content.clone()
        };
        let out_chars = output.chars().count();
        let summary = if truncated {
            format!(
                "read {out_chars} chars (truncated) from {}",
                path.display()
            )
        } else {
            format!("read {out_chars} chars from {}", path.display())
        };

        Ok(ActionOutcome {
            summary,
            data: json!({
                "path": path.to_string_lossy(),
                "content": output,
                "truncated": truncated,
                "chars": out_chars,
                "total_chars": total_chars
            }),
        })
    }
}

impl crate::actions::ActionHandler for WriteFileAction {
    fn name(&self) -> &'static str {
        "fs.write_file"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "Write text to a file (overwrite, append, or create_only).".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "mode": { "type": "string", "enum": ["overwrite", "append", "create_only"] },
                    "create_parents": { "type": "boolean" },
                    "dry_run": { "type": "boolean" }
                },
                "required": ["path", "content"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "bytes": { "type": "integer" },
                    "mode": { "type": "string" },
                    "dry_run": { "type": "boolean" }
                }
            }),
            risk: RiskLevel::Medium,
            requires_approval: true,
            capabilities: vec!["filesystem".to_string()],
        }
    }

    fn validate(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<()> {
        let params: WriteFileParams = parse_params(params)?;
        let path = resolve_path(&params.path);
        ensure_allowed_path(ctx, &path)?;
        let mode = params.mode.unwrap_or_else(|| "overwrite".to_string());
        if mode != "overwrite" && mode != "append" && mode != "create_only" {
            return Err(anyhow!("unsupported mode: {mode}"));
        }
        if mode == "create_only" && path.exists() {
            return Err(anyhow!("file already exists: {}", path.display()));
        }
        if let Some(parent) = path.parent() {
            if !parent.exists() && params.create_parents != Some(true) {
                return Err(anyhow!(
                    "parent directory does not exist: {}",
                    parent.display()
                ));
            }
        }
        Ok(())
    }

    fn execute(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<ActionOutcome> {
        let params: WriteFileParams = parse_params(params)?;
        let path = resolve_path(&params.path);
        ensure_allowed_path(ctx, &path)?;
        let mode = params.mode.unwrap_or_else(|| "overwrite".to_string());
        let create_parents = params.create_parents.unwrap_or(true);
        let dry_run = ctx.dry_run || params.dry_run.unwrap_or(false);
        let bytes = params.content.as_bytes().len();

        if !dry_run {
            if create_parents {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
            }
            match mode.as_str() {
                "overwrite" => {
                    fs::write(&path, params.content)?;
                }
                "append" => {
                    let mut file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)?;
                    file.write_all(params.content.as_bytes())?;
                }
                "create_only" => {
                    let mut file = OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&path)?;
                    file.write_all(params.content.as_bytes())?;
                }
                _ => {}
            }
        }

        let summary = if dry_run {
            format!(
                "dry run: would write {bytes} bytes to {}",
                path.display()
            )
        } else {
            format!("wrote {bytes} bytes to {}", path.display())
        };

        Ok(ActionOutcome {
            summary,
            data: json!({
                "path": path.to_string_lossy(),
                "bytes": bytes,
                "mode": mode,
                "dry_run": dry_run
            }),
        })
    }
}

impl crate::actions::ActionHandler for ReplaceTextAction {
    fn name(&self) -> &'static str {
        "fs.replace_text"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "Replace text in a file.".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "find": { "type": "string" },
                    "replace": { "type": "string" },
                    "all": { "type": "boolean" },
                    "count": { "type": "integer", "minimum": 1 },
                    "dry_run": { "type": "boolean" }
                },
                "required": ["path", "find", "replace"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "replaced": { "type": "integer" },
                    "dry_run": { "type": "boolean" }
                }
            }),
            risk: RiskLevel::Medium,
            requires_approval: true,
            capabilities: vec!["filesystem".to_string()],
        }
    }

    fn validate(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<()> {
        let params: ReplaceTextParams = parse_params(params)?;
        if params.find.is_empty() {
            return Err(anyhow!("find string cannot be empty"));
        }
        let path = resolve_path(&params.path);
        ensure_allowed_path(ctx, &path)?;
        if !path.exists() {
            return Err(anyhow!("path does not exist: {}", path.display()));
        }
        if !path.is_file() {
            return Err(anyhow!("path is not a file: {}", path.display()));
        }
        Ok(())
    }

    fn execute(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<ActionOutcome> {
        let params: ReplaceTextParams = parse_params(params)?;
        let path = resolve_path(&params.path);
        ensure_allowed_path(ctx, &path)?;
        let dry_run = ctx.dry_run || params.dry_run.unwrap_or(false);
        let content = fs::read_to_string(&path)?;

        let do_all = params.all.unwrap_or(params.count.is_none());
        let (updated, replaced) = if do_all {
            let count = content.matches(&params.find).count();
            (content.replace(&params.find, &params.replace), count)
        } else {
            let count = params.count.unwrap_or(1).max(1);
            replace_n(&content, &params.find, &params.replace, count)
        };

        if !dry_run && replaced > 0 {
            fs::write(&path, updated)?;
        }

        let summary = if dry_run {
            format!(
                "dry run: would replace {replaced} occurrence(s) in {}",
                path.display()
            )
        } else {
            format!("replaced {replaced} occurrence(s) in {}", path.display())
        };

        Ok(ActionOutcome {
            summary,
            data: json!({
                "path": path.to_string_lossy(),
                "replaced": replaced,
                "dry_run": dry_run
            }),
        })
    }
}

impl crate::actions::ActionHandler for ListDirAction {
    fn name(&self) -> &'static str {
        "fs.list_dir"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "List entries in a directory.".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "include_hidden": { "type": "boolean" },
                    "max_entries": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "entries": { "type": "array" },
                    "truncated": { "type": "boolean" }
                }
            }),
            risk: RiskLevel::Low,
            requires_approval: false,
            capabilities: vec!["filesystem".to_string()],
        }
    }

    fn validate(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<()> {
        let params: ListDirParams = parse_params(params)?;
        let path = resolve_path(&params.path);
        ensure_allowed_path(ctx, &path)?;
        if !path.exists() {
            return Err(anyhow!("path does not exist: {}", path.display()));
        }
        if !path.is_dir() {
            return Err(anyhow!("path is not a directory: {}", path.display()));
        }
        Ok(())
    }

    fn execute(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<ActionOutcome> {
        let params: ListDirParams = parse_params(params)?;
        let path = resolve_path(&params.path);
        ensure_allowed_path(ctx, &path)?;
        let include_hidden = params.include_hidden.unwrap_or(false);
        let max_entries = params.max_entries.unwrap_or(200).max(1);

        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_string_lossy()
                .to_string();
            if !include_hidden && name.starts_with('.') {
                continue;
            }
            let file_type = entry.file_type()?;
            let kind = if file_type.is_dir() {
                "dir"
            } else if file_type.is_file() {
                "file"
            } else {
                "other"
            };
            let size = entry.metadata().ok().map(|meta| meta.len());
            let entry_json = if let Some(size) = size {
                json!({"name": name, "kind": kind, "size": size})
            } else {
                json!({"name": name, "kind": kind})
            };
            entries.push(entry_json);
            if entries.len() >= max_entries {
                truncated = true;
                break;
            }
        }

        let summary = if truncated {
            format!(
                "listed {} entries (truncated) in {}",
                entries.len(),
                path.display()
            )
        } else {
            format!("listed {} entries in {}", entries.len(), path.display())
        };

        Ok(ActionOutcome {
            summary,
            data: json!({
                "path": path.to_string_lossy(),
                "entries": entries,
                "truncated": truncated
            }),
        })
    }
}

impl crate::actions::ActionHandler for EnsureDirAction {
    fn name(&self) -> &'static str {
        "fs.ensure_dir"
    }

    fn spec(&self) -> ActionSpec {
        ActionSpec {
            name: self.name().to_string(),
            version: "1".to_string(),
            description: "Ensure a directory exists.".to_string(),
            params_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "create_parents": { "type": "boolean" },
                    "dry_run": { "type": "boolean" }
                },
                "required": ["path"]
            }),
            result_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "created": { "type": "boolean" },
                    "dry_run": { "type": "boolean" }
                }
            }),
            risk: RiskLevel::Medium,
            requires_approval: true,
            capabilities: vec!["filesystem".to_string()],
        }
    }

    fn validate(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<()> {
        let params: EnsureDirParams = parse_params(params)?;
        let path = resolve_path(&params.path);
        ensure_allowed_path(ctx, &path)?;
        if path.exists() && !path.is_dir() {
            return Err(anyhow!("path exists and is not a directory: {}", path.display()));
        }
        Ok(())
    }

    fn execute(&self, ctx: &ActionContext, params: &serde_json::Value) -> Result<ActionOutcome> {
        let params: EnsureDirParams = parse_params(params)?;
        let path = resolve_path(&params.path);
        ensure_allowed_path(ctx, &path)?;
        let create_parents = params.create_parents.unwrap_or(true);
        let dry_run = ctx.dry_run || params.dry_run.unwrap_or(false);
        let existed = path.exists();

        if !dry_run && !existed {
            if create_parents {
                fs::create_dir_all(&path)?;
            } else {
                fs::create_dir(&path)?;
            }
        }

        let created = !existed;
        let summary = if dry_run {
            format!(
                "dry run: would ensure directory exists at {}",
                path.display()
            )
        } else if created {
            format!("created directory {}", path.display())
        } else {
            format!("directory already exists at {}", path.display())
        };

        Ok(ActionOutcome {
            summary,
            data: json!({
                "path": path.to_string_lossy(),
                "created": created,
                "dry_run": dry_run
            }),
        })
    }
}

fn replace_n(haystack: &str, needle: &str, replacement: &str, limit: usize) -> (String, usize) {
    if needle.is_empty() || limit == 0 {
        return (haystack.to_string(), 0);
    }
    let mut out = String::with_capacity(haystack.len());
    let mut start = 0;
    let mut replaced = 0;
    while replaced < limit {
        let Some(pos) = haystack[start..].find(needle) else {
            break;
        };
        let idx = start + pos;
        out.push_str(&haystack[start..idx]);
        out.push_str(replacement);
        start = idx + needle.len();
        replaced += 1;
    }
    out.push_str(&haystack[start..]);
    (out, replaced)
}
