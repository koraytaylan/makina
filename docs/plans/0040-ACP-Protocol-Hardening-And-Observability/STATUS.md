# Plan 0040 — ACP-Protocol-Hardening-And-Observability — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-06-26, against develop._

- **Goal:** Protocol-hardened ACP client with a per-call control-RPC timeout (short control RPCs are bounded; long `session/prompt` turns are left uncapped), graceful session/cancel, full token usage and stop_reason observability, and audit records correlated to runs/tasks; WorktreePolicy validates tool_call paths to catch sandbox violations early.
- **Root cause:** Protocol timeout risk. The ACP transport awaits responses indefinitely without a deadline. When an agent wedges mid-control-RPC (e.g., never replies to initialize / session_new / set_mode / set_config_option), the orchestrator stalls indefinitely, holding the Supervisor's driver slot and blocking the run. A flat per-turn timeout is *not* the fix — it would abort legitimate long prompt turns, which are awaited through the same `send_request` path — so the deadline is threaded per-call: control RPCs are bounded, the prompt turn is not. The only outer backstop is the wall-clock cap (minutes-scale). This is the review's primary finding and the highest-priority protocol gap.
- **Approach:** Two workstreams, eleven tasks: Workstream 0001 (Turn-Timeout-And-Cancel-Wiring) adds the control-RPC timeout constant + `TurnTimeout` error, bounds `send_request` via a per-call deadline (control RPCs bounded, prompt turn uncapped), and wires session/cancel through the transport and session API via a stored sender clone (5 tasks). Workstream 0002 (Protocol-Observability-And-Isolation) threads usage (with an explicit `protocol::TurnUsage` → `api::UsageStats` conversion) and stop_reason through the ACP backend trait (3 tasks), injects run_id/task_id into audit entries (2 tasks), and adds `tool_call.extra["locations"]` path validation to WorktreePolicy (1 task). All tasks are executable by junior engineers with the precise file:line anchors and expected behaviors embedded in each task's steps and done-when. Tests are comprehensive: protocol timeouts and cancel are covered with mocked transports; usage/stop_reason propagation is verified end-to-end; audit entries are checked for correctness; path validation is tested with synthetic contexts. All gate commands (cargo test/clippy/fmt) pass.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Turn-Timeout-And-Cancel-Wiring | `turn-timeout-constant-and-error`, `transport-send-request-timeout`, `session-cancel-transport-method`, `acp-client-store-session-id`, `acp-session-cancel-method` | 📋 Planned |
| 0002 | Protocol-Observability-And-Isolation | `usage-field-acp-response-chunk`, `usage-through-backend-trait`, `stop-reason-session-state`, `transport-accept-run-task-ids`, `acp-backend-pass-run-task-ids`, `worktree-policy-path-validation` | 📋 Planned |
