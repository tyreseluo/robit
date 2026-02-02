# Robit Preflight

## Purpose
Preflight is a lightweight gate that runs before any action executes. It is
designed to fail fast for unsafe requests, provide a consistent approval
summary, and keep logs/audit data consistent across adapters.

## Scope
- File operations (read/write/move/delete)
- System control (shell/process/service)
- Web & research (search, fetch, browser automation)

## Action Schema (minimum)
- name
- params (JSON)
- risk (low/medium/high)
- requires_approval (bool)
- capabilities (strings like filesystem, shell, network, browser, system_control)

## Preflight Checks (minimum)
1) Capability gate
   - Allowlist/denylist by capability
2) Path policy
   - Enforce allowed roots
   - Blocked roots override
3) Risk + approval policy
   - low/medium/high routing
4) Dry-run mode
   - Optional default for non-destructive testing

## Approval Flow
1) AI planner proposes an action
2) Preflight runs and returns a report
3) If approval is needed, user receives a compact summary
4) User approves/denies
5) Action executes with a second preflight check

## Logging & Audit (minimum)
- input message id
- action name + params
- preflight report (allowed, reasons)
- approval decision
- execution outcome

## Default Config Loading
- If `ROBIT_CONFIG_PATH` is set, robit loads that file on startup.
- Otherwise it looks for `configs/policy.toml` in the current working directory.
- If no file is found, built-in defaults are used.

## Notes
Preflight is intentionally conservative. When in doubt, require approval or
return a need_input response to gather more detail.
