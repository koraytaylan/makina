# Architecture — Plan 0040 (deltas)

> The concrete deltas. This plan touches
> `crates/makina-acp/src/transport.rs`, `crates/makina-acp/src/error.rs`,
> `crates/makina-acp/src/client.rs`, `crates/makina-acp/src/backend.rs`,
> `crates/makina-acp/src/permission.rs`, `crates/makina-acp/src/protocol.rs`,
> and `crates/makina-core/src/backend.rs`.
> Line numbers are hints; locate by symbol.

## 0001 — Turn-Timeout-And-Cancel-Wiring

Today `Transport::send_request`
(`crates/makina-acp/src/transport.rs:157–202`) awaits a `oneshot` receiver with
no timeout. It is woken only when the reader task routes a response (matched by
id) or when the reader ends (EOF/error), so a wedged agent that holds stdout
open but never replies stalls the orchestrator forever. The Developer/Reviewer
actors have a separate idle-output watchdog
(`crates/makina-core/src/developer.rs:290–309`), but the transport protocol
itself has no turn-level timeout. Cancellation is teardown-only: `METHOD_SESSION_CANCEL`
and `CancelParams` are declared (`crates/makina-acp/src/protocol.rs:54`), yet
nothing in `client.rs` or `backend.rs` ever sends the notification — the only
way to stop an in-flight turn is hard shutdown via `terminate()`
(`crates/makina-acp/src/client.rs:628–696`).

**Edits:**

**Add a control-RPC timeout constant.** Define a module-level constant in
`transport.rs` that supplies the default deadline for the *short control RPCs*.
It is deliberately **not** named `TURN_TIMEOUT_SECS`: a flat per-turn cap is
wrong here (see below). A constant is acceptable for the MVP; a per-task override
is future work:

```rust
/// Default deadline for a short control RPC (initialize / session_new /
/// set_mode / set_config_option). A wedged agent that never replies to one of
/// these must not stall the orchestrator past this window. The long
/// `session/prompt` turn is NOT bounded by this — the minutes-scale wall-clock
/// cap is its outer backstop.
const CONTROL_REQUEST_TIMEOUT_SECS: u64 = 30;
```

**Add a `TurnTimeout` error variant.** In `crates/makina-acp/src/error.rs`, add
the variant to `AcpError`. The enum is `#[derive(thiserror::Error)]` +
`#[non_exhaustive]`, so the variant carries an `#[error(...)]` attribute (no
hand-written `Display`/`Error` impl):

```rust
/// A bounded control RPC exceeded its deadline without a matching response.
#[error("agent turn timed out after {secs}s")]
TurnTimeout { secs: u64 },
```

`map_error` (`crates/makina-acp/src/backend.rs:89-95`) already has a catch-all
`other => BackendError::Transport { .. }` arm that absorbs this new variant — no
conversion code is added.

**Thread a per-call deadline into `send_request`, do NOT flat-cap every turn.**
A flat `tokio::time::timeout` on every `send_request` would abort legitimate
long prompt turns: the `session/prompt` response is awaited through the *same*
`send_request` (`crates/makina-acp/src/client.rs:607-610`), which is the shared
path for initialize / session_new / set_mode / set_config_option / prompt. So
`send_request` gains a `timeout: Option<Duration>` parameter — control RPCs pass
`Some(Duration::from_secs(CONTROL_REQUEST_TIMEOUT_SECS))`, the prompt call passes
`None`:

```rust
// Bound only when a deadline is supplied; the prompt turn passes `None`.
match timeout {
    Some(dur) => match tokio::time::timeout(dur, recv).await {
        Ok(result) => result,
        Err(_elapsed) => Err(AcpError::TurnTimeout { secs: dur.as_secs() }),
    },
    None => recv.await,
}
```

**Add `send_cancel` to the transport.** In `impl TransportSender<W>`, add a
fire-and-forget notification that wraps the already-declared `CancelParams`
(`crates/makina-acp/src/protocol.rs:444–449`):

```rust
/// Send a `session/cancel` notification. Best-effort and ACK-free (a
/// notification, not a request), so it can never itself deadlock or time out.
pub async fn send_cancel(&self, session_id: &str) -> Result<()> {
    self.send_notification(
        crate::protocol::METHOD_SESSION_CANCEL,
        crate::protocol::CancelParams { session_id: session_id.to_string() },
    ).await
}
```

**Expose the existing session id + a sender clone, then `cancel` at the session
level.** `AcpClient` already stores `session_id` (`client.rs:265`, populated in
the private `handshake` at `client.rs:423`; getter at `client.rs:431`) — this
plan does not change that. The wrinkle: `AcpSession.client` is
`Option<AcpClient>` (`backend.rs:377`) and is **`None` during an in-flight turn**
because the client is moved into the prompt worker task (`backend.rs:477`) —
exactly when a cancel is needed. So `cancel()` must not reach through
`self.client`. Instead `AcpSession` stores a **cloned `TransportSender`**
(`Arc`-backed and `'static`, `transport.rs:124-130`) plus the `session_id`
`String` captured at `from_client` construction, and fires through them:

```rust
/// Graceful, in-session cancel of the current turn. Keeps the session alive —
/// unlike `terminate()`, which is a hard SIGTERM→SIGKILL teardown. Fires through
/// a sender clone stored at construction, so it works even while `self.client`
/// is `None` (moved into the turn worker).
pub async fn cancel(&self) -> Result<(), BackendError> {
    self.cancel_sender.send_cancel(&self.session_id).await.map_err(map_error)
}
```

**Properties that make this safe:**

- The deadline is per-call and recomputed per call: control RPCs get a uniform
  `CONTROL_REQUEST_TIMEOUT_SECS` bound while the prompt turn passes `None` and is
  uncapped, so a legitimate long turn is never aborted; a `TurnTimeout` is caught
  like any transport error (the `map_error` catch-all maps it to
  `BackendError::Transport`) and simply propagated to the Supervisor — no special
  recovery path is introduced.
- `send_cancel` is a fire-and-forget notification (like every other
  notification), so it cannot ACK-wait or deadlock; if the agent is already done
  the cancel is a harmless no-op, and the orchestrator-level wall-clock cap stays
  the ultimate backstop.
- `session_id` is captured once at handshake time (in the private `handshake`,
  `client.rs:423`) and is immutable thereafter; `AcpSession` snapshots that id and
  a sender clone at `from_client` construction, so `cancel` always names the live
  session and fires even while the client is moved into the turn worker — it can
  never target a stale or empty id.

## 0002 — Protocol-Observability-And-Isolation

Today `usage` and `stop_reason` from the ACP `PromptResult` are dropped at the
trait boundary: `AcpResponseChunk::TurnComplete(stop_reason)` is converted to
`ResponseEvent::TurnComplete { usage: None }`
(`crates/makina-acp/src/backend.rs:598–612`), so the orchestrator never sees
token counts and a `Refusal` is indistinguishable from `EndTurn`. The `run_id`
and `task_id` on every `AuditEntry` are hardcoded placeholders (`"acp-transport"`
and `None`) in `route_message` (`crates/makina-acp/src/transport.rs:501–515`),
so audit records cannot be correlated back to a run at playback time. And
`WorktreePolicy::decide` (`crates/makina-acp/src/permission.rs:95–136`) keys off
the session `working_dir`, not the actual tool-call target paths, so an agent
that `cwd`s correctly but writes outside the worktree is allowed.

**Edits:**

**Thread `usage` through the chunk.** In `crates/makina-acp/src/client.rs`, make
`AcpResponseChunk::TurnComplete` a struct variant carrying both fields. There is
**no** `PromptUpdate::EndTurn` arm — the chunk is emitted by the `StreamState`
machine in `PromptStream::poll_next`, so widen `StreamState::Completing(StopReason)`
(`client.rs:901`) to `Completing(StopReason, Option<TurnUsage>)`, capture
`result.usage` where the prompt future resolves (`client.rs:1021`), and build the
struct variant in the `Completing` arm (`client.rs:1038`). `TurnUsage`
(`protocol.rs:435`) must derive `PartialEq, Eq` since the enum is `Clone + Eq`:

```rust
/// Carries both the stop reason and parsed token usage so neither is dropped
/// when the chunk crosses the trait boundary into `ResponseEvent`.
TurnComplete { stop_reason: StopReason, usage: Option<TurnUsage> },
```

**Emit the real `usage` instead of `None`.** In the `TurnComplete` match arm
(`crates/makina-acp/src/backend.rs:598`), extract both fields and forward the
actual usage. The chunk carries `protocol::TurnUsage`, but
`ResponseEvent::TurnComplete { usage }` (`crates/makina-core/src/backend.rs:242–245`)
requires `Option<api::UsageStats>` — a **different type**, so convert explicitly
(both have `input_tokens` / `output_tokens: Option<u64>`):

```rust
// Convert protocol::TurnUsage → api::UsageStats and forward the agent's token
// counts to the orchestrator / TUI metrics pane, instead of the placeholder
// `None` that dropped them before.
let usage = usage.map(|u| api::UsageStats {
    input_tokens: u.input_tokens,
    output_tokens: u.output_tokens,
});
let _ = event_tx.send(Ok(ResponseEvent::TurnComplete { usage })).await;
```

**Record `stop_reason` in session state.** For the MVP, store the reason on
`AcpSession` rather than minting a new `ResponseEvent` variant (which would
churn every consumer); a getter exposes it to tests and auditing downstream. The
`TurnComplete` arm lives in the free function `run_turn` (`backend.rs:548-598`),
which the worker task runs against `&mut AcpClient` with no access to
`AcpSession`, so the field is a **shared cell** the worker writes through and the
session reads back:

```rust
/// Stop reason of the last completed turn (e.g. `EndTurn` vs `Refusal`).
/// An Arc<Mutex<…>> because the worker task (`run_turn`) writes it while the
/// session — which has no &mut access inside the worker — reads it via a getter.
last_stop_reason: Arc<Mutex<Option<StopReason>>>,
```

**Inject `run_id` / `task_id` into the transport.** Extend `Transport::new`
(`crates/makina-acp/src/transport.rs:274`) to accept `run_id: String` and
`task_id: Option<String>`, store them on `SenderInner` beside `policy` /
`working_dir` / `audit_sink`, and use them when constructing the `AuditEntry`
(`crates/makina-acp/src/transport.rs:501`) in place of the placeholders:

```rust
// Real run/task context captured at spawn time — audit records are now
// correlatable instead of all reading `"acp-transport"` / `None`.
run_id: sender.inner.run_id.clone(),
task_id: sender.inner.task_id.clone(),
```

`SessionConfig` (`crates/makina-core/src/backend.rs:76`) already has `task_id`
(line 126) and gains only a `run_id` field. `AcpBackend::spawn` does not call
`AcpClient::new` (no such method) or `Transport::new` directly: it builds an
`AcpCommand` via `command_for` and calls `AcpClient::connect(command)`
(`backend.rs:264-265`). The ids ride on `AcpCommand` and thread down to **both**
`Transport::new` call sites — `spawn_transport` (`client.rs:780`, used by
`connect`) and `AcpClient::with_transport` (`client.rs:348`, the test seam).

**Validate tool-call paths in the policy.** Extend `WorktreePolicy::decide`
(`crates/makina-acp/src/permission.rs:96`) to check the tool-call target paths,
not just the session cwd. `ToolCall` (`protocol.rs:640`) has no typed path
fields; paths live untyped at `extra["locations"][n]["path"]` (`protocol.rs:977`).
`PermissionDecision` has exactly `{ allow, option_id, reason }` and no `Default`,
so each deny lists every field. A missing `locations` falls back to allow; a path
that escapes the worktree is denied and logged:

```rust
// Beyond matching the session cwd, every tool-call location must resolve to
// somewhere under the worktree. Resolve via the existing PARENT (so a not-yet-
// created file is not falsely denied), then check containment.
if let Some(locations) = ctx.tool_call.extra.get("locations").and_then(|v| v.as_array()) {
    for loc in locations {
        if let Some(path) = loc.get("path").and_then(|p| p.as_str())
            && !resolves_under(path, &self.worktree)
        {
            warn!(?path, worktree = ?self.worktree, "tool-call path escapes worktree");
            return PermissionDecision {
                allow: false,
                option_id: None,
                reason: "path escapes worktree".into(),
            };
        }
    }
}
// No `locations` (or all inside): fall through to the existing cwd allow path.
```

**Properties that make this safe:**

- `usage` and `stop_reason` are already parsed by the protocol layer, so
  threading them is pure propagation with no new parsing; emitting the real usage
  changes only what value flows, not the `ResponseEvent` shape, and recording the
  stop reason in session state touches no existing consumer.
- `run_id` and `task_id` are captured at spawn time when the orchestrator context
  is known and are immutable at runtime, so audit entries are always correct even
  if in-memory task state later diverges — the audit record is the ground truth.
- Path validation is synchronous (the only IO is resolving each location's parent),
  additive (it runs *before* the existing cwd allow, so no prior decision regresses),
  and audited (every deny is logged). It denies escaping paths but does NOT deny a
  not-yet-created file under the worktree (resolved via its parent) nor a tool call
  that carries no `locations` — so it tightens the sandbox without breaking
  legitimate file-creation tool calls.

## Test strategy

- **0001 (timeout + cancel).** `test_control_request_times_out_when_no_response`
  drives a `TransportSender` over a mock read/write pair, calls `send_request`
  with a short *injected* `Some(Duration)` (a module-level const cannot be
  reassigned per-test), writes no matching response, and asserts it returns
  `AcpError::TurnTimeout`; a companion assertion confirms a `None` (prompt-path)
  call is NOT capped within the same window. `test_send_cancel_serializes_notification`
  calls `send_cancel` and asserts the peer reads the correct `session/cancel`
  JSON-RPC notification; `test_acp_client_exposes_session_id_and_sender` asserts the
  id from a mock handshake lands in `client.session_id()` and a sender clone is
  obtainable; and `test_acp_session_cancel_sends_cancel_notification` asserts
  `cancel()` (fired through the stored sender clone, with no turn in flight) routes
  the cancel through the transport with that id.
- **0002 (observability + isolation).**
  `test_prompt_result_with_usage_is_included_in_chunk` drives a `PromptStream` whose
  `session/prompt` result carries usage and asserts the terminal
  `AcpResponseChunk::TurnComplete { stop_reason, usage }` carries both;
  `test_acp_backend_emits_usage_in_turn_complete` runs a turn with a mocked
  usage-carrying response and asserts the emitted `ResponseEvent::TurnComplete`
  holds the expected converted `api::UsageStats` counts;
  `test_acp_session_stores_last_stop_reason` asserts the session exposes the
  agent's reason. `test_audit_entry_carries_injected_run_and_task_ids` and
  `test_acp_backend_spawn_threads_run_task_ids` assert the injected ids reach the
  emitted `AuditEntry`. `test_worktree_policy_denies_paths_outside_worktree`
  and `test_worktree_policy_allows_paths_inside_worktree` exercise
  `decide` with synthetic `PermissionRequestContext` fixtures whose
  `tool_call.extra["locations"]` paths sit outside / inside the worktree.
- All tests keep `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior work

- **0021 / ACP-Timeouts-and-Turn-Hygiene.** 0001 adds the protocol-level
  control-RPC deadline that 0021's actor-side idle watchdog could not provide: the
  watchdog catches a silently-stalling actor, while `CONTROL_REQUEST_TIMEOUT_SECS`
  bounds a wedged control RPC (the long prompt turn stays uncapped). The two are
  complementary backstops at different layers, and the minutes-scale wall-clock cap
  remains the outermost.
- **0024 / Permission-Policy-and-Sandbox-Teeth.** 0002 extends
  `WorktreePolicy::decide` with the tool-call path validation that 0024
  documented as future work, reusing its `PermissionDecision` shape and audited
  decision path — the session-cwd check stays, and path validation is added
  ahead of the allow, so no existing decision can regress.
- **0024 / Per-Role-Metrics.** 0002 finally lights up the per-role token counts
  that the metrics pane was built to show: `usage` now propagates from the
  `PromptResult` through `ResponseEvent::TurnComplete` instead of being dropped as
  `None`, so the existing metrics surface needs no new plumbing on its side.
