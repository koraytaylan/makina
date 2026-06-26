# Scope — Plan 0040

> Add turn-request timeout to ACP protocol, wire session/cancel into client API, and thread usage/stop_reason through observability and run/task context into audit ledger.

## Why this plan

**1. No turn timeout hangs the orchestrator indefinitely when an agent wedges.** Review finding: `Transport::send_request` (`crates/makina-acp/src/transport.rs:157-202`) awaits a `oneshot` receiver indefinitely — only termination by reader EOF wakes it. A wedged agent that holds stdout open but never replies stalls the orchestrator forever. The Supervisor's per-task wall-clock cap (`wall_clock_secs`) is minutes-scale and is the only backstop. The Developer/Reviewer idle-watchdog (`crates/makina-core/src/developer.rs:290-309`) mitigates silent stalls but does not address hung protocol turns. Real-world agents can hang mid-response for seconds; a deadline on the short control RPCs is table stakes. (It cannot be a flat per-turn cap: `send_request` is the *shared* path for the long `session/prompt` turn too, so a flat 30s would abort legitimate prompts — the deadline is therefore per-call, applied only to control RPCs.) This is **the most significant protocol-level risk** in the review.

**2. session/cancel is declared but never sent; graceful cancellation is teardown-only.** Review finding: `METHOD_SESSION_CANCEL` and `CancelParams` are declared (`crates/makina-acp/src/protocol.rs:54`), but nothing in `client.rs` or `backend.rs` ever invokes it. Cancellation is hard shutdown via `terminate()` (SIGTERM -> 200ms grace -> SIGKILL, `crates/makina-acp/src/client.rs:628-696`). There is no polite way to cancel an in-flight turn while keeping the session alive — a legitimate use case when a task is interrupted mid-turn (user click, supervisor timeout, etc.).

**3. usage and stop_reason are dropped at the trait boundary; metrics pane cannot show token counts or stop reasons.** Review findings: `usage` is hard-coded to `None` (`crates/makina-acp/src/backend.rs:604-611`); the protocol parses `PromptResult.usage` but `AcpResponseChunk::TurnComplete(StopReason)` carries only the reason — the reason is dropped when crossing the trait boundary to `ResponseEvent::TurnComplete { usage: None }` (`crates/makina-acp/src/backend.rs:598-612`). Token counts never light up in the TUI's per-role metrics, and a `Refusal` is indistinguishable from `EndTurn` at the orchestrator level.

**4. WorktreePolicy does no path validation inside tool_call; sandboxing is only by session working_dir.** Review finding: `WorktreePolicy::decide()` (`crates/makina-acp/src/permission.rs:95-136`) keys off the session `working_dir`, not the actual tool-call target paths. If an agent cwd() correctly to the worktree but a tool_call attempts to write outside it, the policy allows it. The `ToolCall` struct (`crates/makina-acp/src/protocol.rs:640`) has no typed path fields; target paths live untyped at `tool_call.extra["locations"][n]["path"]` (`protocol.rs:977`). Validating those locations is documented as future work but should be added now.

**5. run_id and task_id in audit entries are placeholders; audit records cannot be correlated back to a run.** Review finding: `AuditEntry` fields `run_id` and `task_id` are set to hardcoded values (`"acp-transport"` and `None`) in `route_message` (`crates/makina-acp/src/transport.rs:503-504`). The transport has no run/task context, so audit entries are indistinguishable at playback time. Running concurrent agents across different tasks means audit logs mix without correlation. Run_id and task_id must be threaded through `Transport::new()` from the `AcpBackend::spawn` call site where the orchestrator context is available.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001-0002):

- **0001 — Turn-Timeout-And-Cancel-Wiring.** Add a per-call deadline to `Transport::send_request` (`timeout: Option<Duration>`): short control RPCs are bounded by `CONTROL_REQUEST_TIMEOUT_SECS` (default 30s) so a wedged agent cannot hang the orchestrator, while the long `session/prompt` turn passes `None` and is left uncapped. Implement `session/cancel` as a new `AcpSession::cancel` method that fires through a sender clone stored at construction (so it works even while the client is moved into the turn worker), wired to the transport's `send_notification`. Surface a `TurnTimeout` error variant.
- **0002 — Protocol-Observability-And-Isolation.** Thread usage and stop_reason through the AgentBackend trait so the orchestrator observes token counts and stop reasons. Thread run_id and task_id through Transport and AuditEntry so audit records correlate to specific runs and tasks. Add path validation inside WorktreePolicy's tool_call decision to catch sandbox violations early.

## Origin -> workstream mapping

| Finding | Addressed by |
|---|---|
| Turn timeout missing — `Transport::send_request` awaits a `oneshot` receiver indefinitely; a wedged agent hangs the orchestrator (the most significant protocol-level risk). | `0001` |
| session/cancel is declared (`METHOD_SESSION_CANCEL`, `CancelParams`) but never sent; graceful in-session cancellation is not available — only hard teardown via `terminate()`. | `0001` |
| usage is hard-coded to `None` at the trait boundary; token counts never propagate to the orchestrator or TUI metrics pane. | `0002` |
| stop_reason is dropped when `AcpResponseChunk::TurnComplete(StopReason)` crosses to `ResponseEvent::TurnComplete`; `Refusal` is indistinguishable from `EndTurn`. | `0002` |
| WorktreePolicy validates only the session cwd, not the tool-call target paths at `tool_call.extra["locations"][n]["path"]`. | `0002` |
| run_id and task_id are hardcoded placeholders (`"acp-transport"`, `None`) in `route_message`; audit records cannot be correlated to runs. | `0002` |

## Locked decisions

- **The timeout is a per-call deadline bounding short control RPCs only, not a flat per-turn cap.** A flat `tokio::time::timeout` on every `send_request` would abort legitimate long `session/prompt` turns, because the prompt response is awaited through the *same* `send_request` path (`crates/makina-acp/src/client.rs:607-610`) shared by initialize / session_new / set_mode / set_config_option / prompt. So `send_request` takes an explicit `timeout: Option<Duration>`: control RPCs pass `Some(Duration::from_secs(CONTROL_REQUEST_TIMEOUT_SECS))` (default 30s, named `CONTROL_REQUEST_TIMEOUT_SECS` — *not* `TURN_TIMEOUT_SECS`), the prompt call passes `None` (uncapped). On elapse `send_request` returns `AcpError::TurnTimeout { secs }`. The wall-clock cap (minutes-scale) remains the outer bound for the entire task. Per-task override of the control deadline is deferred (plan 0041+); the constant can be adjusted in a follow-up without changing the architecture.
- **session/cancel is a fire-and-forget notification, not a request.** The agent is not required to ACK the cancel (no JSON-RPC response expected). This matches the ACP spec (cancel is a notification type). Cancellation is best-effort: if the agent is already done, the notification is a no-op; if the agent receives it mid-turn it should exit gracefully. Orchestrator-level timeout remains the ultimate backstop.
- **run_id and task_id are threaded at Transport construction time and are immutable.** The values come from the Supervisor's SessionConfig at spawn time. They are captured once and never changed, ensuring audit entries are always correct even if the in-memory task state diverges. Audit records are the ground truth; task/run names in memory are derived.
- **WorktreePolicy path validation is conservative (deny-by-default for ambiguous paths).** If a path cannot be canonicalized or is ambiguous, the decision is to deny and log a warning. This errs on the side of security. False-positives (legitimate paths denied) are acceptable and will be tuned based on real-world feedback; false-negatives (sandbox escapes allowed) are not.

## Out of scope

- Per-task turn timeout override (configurable timeout per SessionConfig). The 30-second constant is the MVP. Making it per-task requires modifying SessionConfig and propagating through AcpBackend. Deferred to plan 0041+.
- Executor (agent-side) support for session/cancel. This plan wires cancel in the Makina client; agents must implement the handler themselves. The Makina change is purely the client-side send.
- Syntax highlighting or language detection in code blocks. Usage and stop_reason are now available to the orchestrator; rendering enhancements are deferred to a TUI plan.
- Audit log playback UI (querying by run_id/task_id). Audit entries now carry the context; UI to filter/search them is deferred.
- Full sandboxed enforcement of tool_call paths (capability-based security). WorktreePolicy now validates paths; full capability-based sandboxing (e.g., seccomp, pledge) is out of scope.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
