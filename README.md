# Robit

Robit is a pure‑Rust personal AI assistant framework. It listens to messages (stdin, Robrix/Matrix, or other adapters), asks a model to plan actions, executes those actions with a policy + approval gate, and responds with results.

It is designed for local‑first automation: file operations, shell commands, browser control, and web/research, all running on your machine with explicit approvals.

## Key Capabilities

- **Plan → Execute**: LLM produces a structured plan; Robit executes step‑by‑step with approvals.
- **Policy + Preflight**: path allowlists, capability allow/deny, blocked roots, and risk gating.
- **Adapters**: stdin for local CLI testing, Robrix for Matrix rooms.
- **Actions**: filesystem, shell, browser, web fetch/search.
- **Local or HTTP models**: OpenAI/DeepSeek via HTTP or local OminiX‑MLX (Qwen3).

## Project Status (WIP)

Robit is actively evolving. The core pipeline is stable, but some areas are still in progress:

- Stronger AI JSON parsing and fallback handling
- Richer system‑status diagnostics (more structured metrics)
- Additional adapters (Slack/Discord/webhooks)
- GUI automation actions

## Architecture (High Level)

1. **Inbound message** → adapter
2. **AI planner** (LLM) returns `action` or `plan`
3. **Preflight + Policy** checks
4. **Approval** (optional, per step or approve‑all)
5. **Execution** → Action outcome
6. **Summary** → response back to user

## System Status (Current Behavior)

When users ask for “system status / CPU / memory / disk / network”, Robit generates a multi‑step plan (usually `shell.run`) and then summarizes the results into **one response**.

Typical probes on macOS:

- `uptime` (load averages)
- `vm_stat` (memory)
- `df -h` (disk usage)
- `ps aux | sort -nrk 3,3 | head -5` (top processes)
- `ifconfig` (network interfaces)

The summary includes parsed metrics plus raw output blocks (for now). This can be customized later.

## Action Schema (Contract)

Every action advertises an `ActionSpec`:

```json
{
  "name": "fs.read_file",
  "version": "1",
  "description": "Read a text file (optionally truncated).",
  "params_schema": { "type": "object", "properties": { "path": {"type": "string"} }, "required": ["path"] },
  "result_schema": { "type": "object" },
  "risk": "Low",
  "requires_approval": false,
  "capabilities": ["filesystem"]
}
```

## Plan Schema (AI Output)

Robit expects plans in this format:

```json
{
  "type": "plan",
  "steps": [
    {
      "id": "s1",
      "action": "shell.run",
      "params": {"command": "uptime"},
      "note": "Check uptime",
      "requires_approval": true
    }
  ]
}
```

If a step requires approval, Robit pauses and asks the user. Users can reply:

- `approve <id>`
- `approve-all <id>` (approve remaining steps)
- `deny <id>`

## Protocol / Message Format (robrix integration)

Robit uses a simple JSON protocol for adapters. All messages are wrapped in:

```json
{
  "schema_version": "robit.v1",
  "id": "evt-xxx",
  "body": { ... }
}
```

Key payloads:

**Inbound Message**
```json
{
  "type": "message",
  "workspace_id": "workspace",
  "room_id": "room",
  "message_id": "msg-123",
  "sender_id": "@user",
  "text": "hello",
  "metadata": {}
}
```

**Outbound Response**
```json
{
  "type": "response",
  "workspace_id": "workspace",
  "room_id": "room",
  "in_reply_to": "msg-123",
  "kind": "chat | approval_request | action_result | plan_completed",
  "text": "..."
}
```

**Approval Decision**
```json
{
  "type": "approval_decision",
  "approval_id": "appr-1",
  "decision": "approve | deny | approve_all",
  "workspace_id": "workspace",
  "room_id": "room",
  "sender_id": "@user",
  "in_reply_to": "msg-123"
}
```

## Quick Start (stdin)

```bash
cargo run -p robit
```

Examples:

```text
action:fs.list_dir path=./
action:fs.write_file {"path":"./notes.txt","content":"hello world"}
action:fs.read_file path=./notes.txt
action:shell.run command="ls -la"
action:browser.open_url url=https://example.com
action:web.fetch_url url=https://example.com
```

Approvals:

```text
approve appr-1
approve-all appr-1
deny appr-1
```

## Using Robrix (Matrix)

Robrix embeds Robit and forwards Matrix room messages to it.

At runtime, register actions via Robit’s default registry:

```rust
let registry = robit::default_registry();
let planner = RulePlanner::new();
let policy = Policy::default_with_home();
let mut engine = Engine::new(registry, planner, policy)?;
```

Robrix is expected to manage room/workspace scopes and pass messages into the Robit engine.

## Default Actions

Filesystem:
- `fs.read_file`
- `fs.write_file`
- `fs.replace_text`
- `fs.list_dir`
- `fs.ensure_dir`
- `fs.organize_directory`

System control:
- `shell.run` (macOS/Linux)

Browser:
- `browser.open_url`

Web:
- `web.fetch_url`
- `web.search_brave` (requires Brave Search API key in params)

## Configuration

Robit auto‑loads config from:

1) `ROBIT_CONFIG_PATH`, or  
2) `./configs/policy.toml`, or  
3) repo root `configs/policy.toml`

Example (`configs/policy.toml`):

```toml
[preflight]
enabled = true
strict = true
allowed_capabilities = ["filesystem", "shell", "process", "network", "browser"]
denied_capabilities = ["system_control"]
blocked_roots = ["/System", "/Library"]
enforce_policy_roots = true
path_keys = ["path","dir","directory","cwd","file","target","src","dst","source","destination"]

[policy]
allowed_roots = ["~/Projects", "~/Desktop"]
approval_risk_levels = ["medium", "high"]
```

## Models

### HTTP (OpenAI / DeepSeek)
Enabled by default via the `ai-http` feature.

### Local (OminiX‑MLX / Qwen3)
Enable with feature `robit-omnix-mlx` (in Robrix: `--features robit,robit-omnix-mlx`).

Robrix config in `robrix/src/robit_runtime.rs`:

```rust
const ROBIT_AI_BACKEND: &str = "omnix-mlx";
const ROBIT_MLX_MODEL_DIR: &str = "/path/to/OminiX-MLX/models/Qwen3-4B";
```

## Safety Notes

- **All risky actions require approval** by default.
- **Preflight checks** enforce allowed paths and capabilities.
- Use `dry-run on/off` to simulate or actually execute commands.

## Development

```bash
cargo build -p robit
```

If you add new actions, register them in `default_registry()` so all adapters can use them.

## Contributing

Robit is open to community contributions. Good starter areas:

- New actions (GUI automation, system services, browser workflows)
- Better system‑status parsing and summaries
- More adapters (Slack/Discord/webhooks)
- Hardening AI JSON parsing and fallback logic

### Add a new action (quick guide)

1) Implement `ActionHandler` in `crates/robit/src/actions/`
2) Register it in `default_registry()` so all adapters pick it up
3) Add docs/examples in this README if it’s user‑facing

### Code Style

- Keep actions small and predictable
- Use `preflight` + `policy` checks for any filesystem / shell / network access
- Default risky actions to `requires_approval = true`
