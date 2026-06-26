# XAgent Plan 0040 — ACP-Protocol-Hardening-And-Observability

This plan hardens the ACP client against two critical protocol risks: indefinite hangs when an agent wedges (missing turn timeout) and no graceful in-session cancellation (session/cancel unwired). It threads token usage and stop_reason through the AgentBackend trait for observability in the TUI metrics pane. It threads run_id and task_id through Transport and AuditEntry construction so audit records correlate to specific runs and tasks, eliminating placeholder hardcoding. It adds path validation inside WorktreePolicy's tool_call decision path to catch sandbox violations early.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`;
  abbreviated as "cargo test/clippy/fmt green" thereafter.
- GPU tests self-skip without an adapter.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0001 — Turn-Timeout-And-Cancel-Wiring

### turn-timeout-constant-and-error — Add CONTROL_REQUEST_TIMEOUT_SECS Constant and TurnTimeout Error Variant

Today `Transport::send_request` awaits a `oneshot` receiver indefinitely. To bound *short control RPCs* (initialize / session_new / set_mode / set_config_option) we need a default timeout duration constant and an error type to represent timeout failures. (The long `session/prompt` turn is deliberately *not* capped by this constant — see `transport-send-request-timeout`.)

**Steps:**

1. In `crates/makina-acp/src/transport.rs`, add a module-level constant near the top: `const CONTROL_REQUEST_TIMEOUT_SECS: u64 = 30;` (30 seconds is the default bound for short control RPCs). The name is deliberately *not* `TURN_TIMEOUT_SECS`: a flat per-turn cap would abort legitimate long prompt turns; this constant only bounds wedged *control* RPCs.
2. In `crates/makina-acp/src/error.rs` (the error module — confirmed at `crates/makina-acp/src/error.rs`), add a new variant to the `AcpError` enum. The enum uses `#[derive(thiserror::Error)]` + `#[non_exhaustive]`, so add the variant with a `#[error(...)]` attribute rather than a hand-written `Display`/`Error` impl:
```rust
/// A bounded control RPC exceeded its deadline without a matching response.
#[error("agent turn timed out after {secs}s")]
TurnTimeout { secs: u64 },
```
3. No error-conversion change is required: `map_error` in `crates/makina-acp/src/backend.rs:89-95` already has a catch-all `other => BackendError::Transport { .. }` arm that absorbs the new variant. Leave that arm as-is (do not add an explicit `TurnTimeout` match) unless a clippy lint forces it.

- **Depends on:** —
- **Done when:** The constant `CONTROL_REQUEST_TIMEOUT_SECS` is defined and visible in transport.rs. The `AcpError::TurnTimeout { secs }` variant exists (via `#[error(...)]`) and can be constructed. `map_error` still maps it through the catch-all to `BackendError::Transport`. Existing code compiles without errors. cargo test/clippy/fmt green.

---

### transport-send-request-timeout — Bound send_request with a per-call deadline (short control RPCs only)

Now that the constant and error exist, bound the `oneshot` receive inside `send_request` — but **only for short control RPCs**. A flat `tokio::time::timeout` on every `send_request` would abort every legitimate agent prompt turn longer than the cap: the `session/prompt` response is awaited through `send_request(METHOD_SESSION_PROMPT)` (`crates/makina-acp/src/client.rs:607-610`), and `send_request` is the *shared* path for `initialize` / `session_new` / `set_mode` / `set_config_option` / `prompt`. So thread an explicit per-call deadline: control RPCs pass a short default; the prompt call passes `None` (uncapped — the minutes-scale Supervisor wall-clock cap is its outer backstop).

**Steps:**

1. In `crates/makina-acp/src/transport.rs`, add a `timeout: Option<std::time::Duration>` parameter to `TransportSender::send_request` (around line 157) — and to the delegating `Transport::send_request` (around line 320). Import `std::time::Duration`.
2. Find the `match rx.await { Ok(result) => result, Err(_) => ... }` block (around line 192). Wrap the receive only when a deadline is supplied:
```rust
let recv = async {
    match rx.await {
        Ok(result) => result,
        Err(_) => Err(self
            .inner
            .shared
            .ended_error()
            .unwrap_or_else(|| AcpError::Transport("reader task ended".into()))),
    }
};
match timeout {
    Some(dur) => match tokio::time::timeout(dur, recv).await {
        Ok(result) => result,
        Err(_elapsed) => Err(AcpError::TurnTimeout {
            secs: dur.as_secs(),
        }),
    },
    None => recv.await,
}
```
3. Update the call sites: the control RPCs in `client.rs` (initialize, `session/new` at line 419, `set_mode`, `set_config_option`) pass `Some(Duration::from_secs(CONTROL_REQUEST_TIMEOUT_SECS))`; the **prompt** call (`client.rs:608-610`, `METHOD_SESSION_PROMPT`) passes `None` so a legitimate long turn is never aborted. Re-export or reference `CONTROL_REQUEST_TIMEOUT_SECS` as needed (it lives in `transport.rs`).
4. Write a unit test `test_control_request_times_out_when_no_response` that constructs a `TransportSender` over a mock read/write pair, calls `send_request` with `Some(Duration::from_millis(50))` (a short injected deadline — a module-level const cannot be reassigned per-test), writes NO matching response, and asserts the call returns `AcpError::TurnTimeout`. Add `test_prompt_send_request_is_not_capped` (or assert via `None`) that a `send_request(..., None)` call does *not* time out within the short window.

- **Depends on:** turn-timeout-constant-and-error
- **Done when:** `send_request` takes an `Option<Duration>` deadline. Wedged short control RPCs return `AcpError::TurnTimeout` instead of hanging; legitimate long prompt turns (passed `None`) are NOT aborted. `test_control_request_times_out_when_no_response` passes and the `None`/prompt path does not time out. Existing transport tests remain green. cargo test/clippy/fmt green.

---

### session-cancel-transport-method — Add TransportSender::send_cancel Method

Now that the timeout is in place, we need a way for the AcpSession to send a graceful `session/cancel` notification to the agent.

**Steps:**

1. In `crates/makina-acp/src/transport.rs`, locate the `impl TransportSender<W>` block (around line 120). After the `send_notification` method (line 205), add a new public async method:
```rust
pub async fn send_cancel(&self, session_id: &str) -> Result<()> {
    self.send_notification(
        crate::protocol::METHOD_SESSION_CANCEL,
        crate::protocol::CancelParams {
            session_id: session_id.to_string(),
        },
    ).await
}
```
2. Ensure `crate::protocol::CancelParams` is imported and available. `protocol.rs` already exports `CancelParams` (around line 447, with field `session_id: String`) and `METHOD_SESSION_CANCEL` (line 54).
3. Write a unit test `test_send_cancel_serializes_notification` that constructs a transport sender with a mock peer, calls `send_cancel('test-session')`, and asserts the peer reads the correct JSON-RPC notification.

- **Depends on:** turn-timeout-constant-and-error
- **Done when:** The `send_cancel` method is present on `TransportSender`. It sends a `session/cancel` notification with the provided session_id. The test passes. Existing tests remain green. cargo test/clippy/fmt green.

---

### acp-client-store-session-id — Expose AcpClient's Session ID and Transport Sender for Cancel

To send `session/cancel`, the `AcpSession` needs the agent's `session_id` *and* a clonable transport sender. Both already live on `AcpClient` — the `session_id` field already exists (`crates/makina-acp/src/client.rs:265`, private), is initialized to `String::new()` in `from_parts` (`client.rs:361-374`), and is populated from the `session/new` reply inside the private `handshake` method (`client.rs:423`). There is **no** `AcpClient::new` and no public `initialize` method — the handshake is private and runs inside `connect` / `with_transport`. A `session_id()` getter already exists at `client.rs:431`. This task only adds the accessors `acp-session-cancel-method` needs; it does **not** change when or how the id is stored.

**Steps:**

1. In `crates/makina-acp/src/client.rs`, confirm the existing `session_id` field (`client.rs:265`) and the existing `session_id()` getter (`client.rs:431`). No change to storage is needed — the value is already captured at handshake time and is immutable thereafter.
2. Add a public accessor that returns a *clone* of the transport's send side so a caller can fire a cancel even while the client is moved into a turn worker:
```rust
/// A cloned, `'static` send side for out-of-band notifications (e.g. cancel).
pub fn sender_clone(&self) -> crate::transport::TransportSender<BoxedWriter> {
    self.transport.sender().clone()
}
```
(`Transport::sender()` at `client.rs`/`transport.rs:315` already borrows the send side; `TransportSender` is `Arc`-backed and `'static`, `transport.rs:124-130`.) Make `TransportSender` reachable from `backend.rs` (it is already `pub` in `transport.rs`).
3. Write a unit test `test_acp_client_exposes_session_id_and_sender` that builds a client via `with_transport` against a mock agent returning a known session_id, asserts `client.session_id()` matches, and asserts `client.sender_clone()` returns without panicking.

- **Depends on:** —
- **Done when:** `AcpClient` exposes its `session_id` (existing getter) and a `sender_clone()` returning a `'static` `TransportSender`. The test verifies the session_id matches and a sender clone is obtainable. Existing tests remain green. cargo test/clippy/fmt green.

---

### acp-session-cancel-method — Add AcpSession::cancel Wired Through a Stored Sender Clone

Now that the transport can send cancel and the client exposes a sender clone + session_id, expose cancel at the session level. **Critical:** `AcpSession.client` is `Option<AcpClient>` (`crates/makina-acp/src/backend.rs:377`) and is `None` *during an in-flight turn* — the client is moved into the prompt worker task (`backend.rs:477`, `tokio::spawn(async move { … client … })`), which is exactly when a cancel is needed. So `cancel()` must NOT reach through `self.client`. Instead store the cancel channel directly on `AcpSession` at construction.

**Steps:**

1. In `crates/makina-acp/src/backend.rs`, add two fields to the `AcpSession` struct (`backend.rs:373`): `cancel_sender: crate::transport::TransportSender<BoxedWriter>` and `session_id: String`. These outlive the turn because `TransportSender` is `Arc`-backed and `'static` (`transport.rs:124-130`), so the cancel works even while `self.client` is `None`.
2. In `AcpSession::from_client` (`backend.rs:402`), capture both before the client may move: `let cancel_sender = client.sender_clone(); let session_id = client.session_id().to_string();` and store them in the constructed `Self { .. }`.
3. Add a public async method on `AcpSession` that fires the cancel through the stored sender (NOT `self.client`):
```rust
pub async fn cancel(&self) -> Result<(), BackendError> {
    self.cancel_sender
        .send_cancel(&self.session_id)
        .await
        .map_err(map_error)
}
```
4. Write a unit test `test_acp_session_cancel_sends_cancel_notification` that builds an `AcpSession` via `from_client` over a mock-agent transport with a known session_id, calls `cancel()`, and asserts the peer reads the `session/cancel` JSON-RPC notification carrying that session_id (the cancel succeeds even with no turn in flight).

- **Depends on:** session-cancel-transport-method, acp-client-store-session-id
- **Done when:** `AcpSession` has a public `cancel()` async method that fires a `session/cancel` notification through a sender clone stored at construction, working even while the client is moved into a turn worker (`self.client == None`). The method returns `Result<(), BackendError>`. The test passes. Existing tests remain green. cargo test/clippy/fmt green.

---

## 0002 — Protocol-Observability-And-Isolation

### usage-field-acp-response-chunk — Add Usage Field to AcpResponseChunk::TurnComplete

Today `AcpResponseChunk::TurnComplete(stop_reason)` carries only the reason. We need to thread `usage` (token counts) as well.

**Steps:**

1. In `crates/makina-acp/src/client.rs`, locate the `AcpResponseChunk` enum (around line 199-242). Find the `TurnComplete(StopReason)` variant (line 241).
2. Change it to a struct variant: `TurnComplete { stop_reason: StopReason, usage: Option<TurnUsage> }` where `TurnUsage` is the existing type from `protocol.rs:435` (it derives `Clone`; the enum is `Clone + PartialEq + Eq`, and `TurnUsage` must satisfy those — add `PartialEq, Eq` to `TurnUsage`'s derives in `protocol.rs` if missing).
3. There is **no** `PromptUpdate::EndTurn` arm. The chunk is built from the `StreamState` machine in `PromptStream::poll_next`. Two edits:
   - Widen the state variant from `Completing(StopReason)` to also carry usage: `Completing(StopReason, Option<TurnUsage>)` (`crates/makina-acp/src/client.rs:901`).
   - At the point the prompt future resolves (`client.rs:1021`), capture both: `this.state = StreamState::Completing(result.stop_reason, result.usage);`.
   - In the `Completing` arm (`client.rs:1031-1038`), destructure both and build the struct variant:
```rust
let StreamState::Completing(stop_reason, usage) =
    std::mem::replace(&mut this.state, StreamState::Done)
else {
    unreachable!("state checked in match arm")
};
return Poll::Ready(Some(Ok(AcpResponseChunk::TurnComplete { stop_reason, usage })));
```
4. Add a unit test `test_prompt_result_with_usage_is_included_in_chunk` that drives a `PromptStream` (via `with_transport` / a mock agent) whose `session/prompt` result carries `usage`, and asserts the terminal `AcpResponseChunk::TurnComplete { stop_reason, usage }` carries both the reason and the expected token counts.

- **Depends on:** session-cancel-transport-method, acp-client-store-session-id
- **Done when:** `AcpResponseChunk::TurnComplete` is a struct variant carrying `{ stop_reason, usage }`, built from the widened `StreamState::Completing(StopReason, Option<TurnUsage>)` populated from `PromptResult.usage` at `client.rs:1021`. The test passes. Existing tests remain green. cargo test/clippy/fmt green.

---

### usage-through-backend-trait — Thread Usage Through ResponseEvent::TurnComplete

Now that `AcpResponseChunk` carries usage, thread it into `ResponseEvent`. **Type mismatch to bridge:** the chunk carries `protocol::TurnUsage` (`crates/makina-acp/src/protocol.rs:435`) but `ResponseEvent::TurnComplete.usage` requires `Option<api::UsageStats>` (`crates/makina-core/src/backend.rs:242-245`). These are different types, so an explicit conversion is required — they are not interchangeable. (Both have `input_tokens: Option<u64>` and `output_tokens: Option<u64>`, so the mapping is field-for-field.)

**Steps:**

1. In `crates/makina-acp/src/backend.rs`, locate the `TurnComplete` match arm (`backend.rs:598`). After `usage-field-acp-response-chunk` it destructures `Ok(AcpResponseChunk::TurnComplete { stop_reason, usage })`. Today the body emits `ResponseEvent::TurnComplete { usage: None }`.
2. Convert `protocol::TurnUsage` to `api::UsageStats` at the bridge and emit it:
```rust
let usage = usage.map(|u| api::UsageStats {
    input_tokens: u.input_tokens,
    output_tokens: u.output_tokens,
});
let _ = event_tx.send(Ok(ResponseEvent::TurnComplete { usage })).await;
```
3. Update the arm's doc comment: it now forwards the real `usage` (converted) instead of the placeholder `None`. `stop_reason` is still dropped here (recorded separately by `stop-reason-session-state`).
4. Confirm `ResponseEvent::TurnComplete` in `crates/makina-core/src/backend.rs:242-246` has `usage: Option<api::UsageStats>` (it does) and that `api::UsageStats` has `input_tokens`/`output_tokens` (it does, `api.rs:903-907`).
5. Write a test `test_acp_backend_emits_usage_in_turn_complete` that runs a turn (via `from_client` over a mock agent) whose `session/prompt` result carries usage, and verifies the emitted `ResponseEvent::TurnComplete` holds the expected `api::UsageStats` counts.

- **Depends on:** usage-field-acp-response-chunk
- **Done when:** `ResponseEvent::TurnComplete` emits the actual `usage` from the agent (converted `protocol::TurnUsage` → `api::UsageStats`) instead of `None`. Downstream consumers (TUI, orchestrator) can access token counts. The test passes. Existing tests remain green. cargo test/clippy/fmt green.

---

### stop-reason-session-state — Store Stop Reason in AcpSession State

The `stop_reason` from the agent is now in the chunk, but it is not yet accessible to downstream consumers. Store it in AcpSession state so it can be queried after a turn completes.

**Steps:**

1. In `crates/makina-acp/src/backend.rs`, add a shared cell to the `AcpSession` struct (`backend.rs:373`): `last_stop_reason: std::sync::Arc<std::sync::Mutex<Option<StopReason>>>`, initialized to `Arc::new(Mutex::new(None))` in `from_client` (`backend.rs:402`). A plain field will NOT work: the `TurnComplete` arm lives in the free function `run_turn` (`backend.rs:548-598`), which operates on `&mut AcpClient` in the spawned worker task and has **no** access to `&mut self` / `AcpSession`. The `Arc<Mutex<…>>` is the write-back channel.
2. Pass a clone of that `Arc` into the worker. In `AcpSession::prompt` (`backend.rs:462`), `let stop_cell = Arc::clone(&self.last_stop_reason);` and move it into the `tokio::spawn` closure (`backend.rs:477`); thread it as a parameter to `run_turn`. In `run_turn`'s `Ok(AcpResponseChunk::TurnComplete { stop_reason, usage })` arm (`backend.rs:598`), after emitting the event, write it: `*stop_cell.lock().unwrap() = Some(stop_reason);` (`StopReason` derives `Clone`, not `Copy`).
3. Add a public getter on `AcpSession`: `pub fn last_stop_reason(&self) -> Option<StopReason> { self.last_stop_reason.lock().unwrap().clone() }`.
4. Write a unit test `test_acp_session_stores_last_stop_reason` that runs a full turn through the session (drives the `ResponseStream` to completion so the worker writes the cell), then asserts `session.last_stop_reason()` matches the mock agent's reason.

- **Depends on:** usage-field-acp-response-chunk
- **Done when:** AcpSession stores and exposes the stop_reason from the last completed turn. The test passes. Existing tests remain green. cargo test/clippy/fmt green.

---

### transport-accept-run-task-ids — Thread run_id and task_id Into Transport::new

Audit entries currently have hardcoded placeholders for run_id and task_id. We need to inject the actual values at Transport construction time.

**Steps:**

1. In `crates/makina-acp/src/transport.rs`, modify the signature of `Transport::new` (line 274) to accept two additional parameters: `run_id: String` and `task_id: Option<String>`.
2. In the `SenderInner` struct (around line 108-122, alongside `policy` / `working_dir` / `audit_sink`), add two new fields: `run_id: String` and `task_id: Option<String>`.
3. When constructing `SenderInner` inside `Transport::new`, populate these fields with the supplied values.
4. In `route_message`, when constructing the `AuditEntry` (line 501), use the injected values:
```rust
run_id: sender.inner.run_id.clone(),
task_id: sender.inner.task_id.clone(),
```
Instead of the hardcoded `"acp-transport"` and `None`.
5. Update the `duplex_transport()` test helper (line 584) to pass dummy values (e.g., `"test-run"` and `None`).
6. Write a unit test `test_audit_entry_carries_injected_run_and_task_ids` that constructs a transport with known run/task ids, triggers a permission request, and asserts the emitted audit entry has the correct values.

- **Depends on:** turn-timeout-constant-and-error, session-cancel-transport-method
- **Done when:** Transport::new accepts run_id and task_id parameters. AuditEntry records contain the actual run_id and task_id instead of placeholders. The test passes. Existing tests are updated and pass. cargo test/clippy/fmt green.

---

### acp-backend-pass-run-task-ids — Thread run_id/task_id From spawn Through connect to Transport::new

`AcpBackend::spawn` does **not** call `AcpClient::new` (there is no such method) or `Transport::new` directly. It builds an `AcpCommand` via `command_for(config.working_dir)` and calls `AcpClient::connect(command)` (`crates/makina-acp/src/backend.rs:264-265`). `Transport::new` is reached at **two** sites: inside the free function `spawn_transport` (`client.rs:780`, used by `connect`) and inside `AcpClient::with_transport` (`client.rs:348`, the test seam). Thread run_id/task_id down to **both**. `SessionConfig` already has `task_id` (`crates/makina-core/src/backend.rs:126`) — add only `run_id`.

**Steps:**

1. In `crates/makina-core/src/backend.rs`, add **only** a `run_id` field to `SessionConfig` (after `task_id`, line 126): `#[serde(default)] pub run_id: String,`. Do NOT re-add `task_id` — it already exists at line 126.
2. Update **every** `SessionConfig` struct literal to set `run_id` (e.g. `run_id: "test-run".into()` or `String::new()`): the three in `crates/makina-core/src/backend.rs` tests (lines 435-443, 490-498, 512-520) and the one in `crates/makina-acp/src/backend.rs` tests (lines 700-708). (These already set `task_id: None`; just add the `run_id` line.)
3. Carry the ids on `AcpCommand`: in `crates/makina-acp/src/client.rs`, add `pub run_id: String` and `pub task_id: Option<String>` to `AcpCommand` (`client.rs:69`) and to its builder/`new`. In `command_for` (`crates/makina-acp/src/backend.rs:201`), set them from `config.run_id` / `config.task_id`. `spawn_transport` (`client.rs:720-781`) then passes `command.run_id.clone()` / `command.task_id.clone()` into its `Transport::new` call (`client.rs:780`).
4. Add `run_id: String` and `task_id: Option<String>` parameters to `AcpClient::with_transport` (`client.rs:332`) and pass them into its `Transport::new` call (`client.rs:348`). Update `spawn_with_transport` and any in-crate caller of `with_transport` to pass through the ids (default `String::new()` / `None` in tests).
5. Update the `duplex_transport()` test helper (`crates/makina-acp/src/transport.rs:584-601`) and the `transport-accept-run-task-ids` tests to pass the new `Transport::new` arguments (e.g. `"test-run".into(), None`).
6. Write a test `test_acp_backend_spawn_threads_run_task_ids` that spawns a session via the `spawn_with_transport`/`from_client` seam with known run/task ids and asserts a resulting `AuditEntry` (from a permission request) carries those exact ids.

- **Depends on:** transport-accept-run-task-ids
- **Done when:** `SessionConfig` carries `run_id` (and the pre-existing `task_id`). The ids thread from `spawn` → `command_for`/`AcpCommand` → `connect`/`spawn_transport` and via `with_transport` into **both** `Transport::new` call sites. `AuditEntry` records emitted by the session carry the correct run/task ids. The test passes. Every `SessionConfig` literal compiles. cargo test/clippy/fmt green.

---

### worktree-policy-path-validation — Add Path Validation Inside WorktreePolicy::decide

WorktreePolicy today only checks the session's cwd, not the actual paths in the tool_call. Add validation that the tool-call target paths lie under the worktree. **The `ToolCall` struct (`crates/makina-acp/src/protocol.rs:640`) has NO typed path fields** — only `tool_call_id` / `status` / `title` / `kind` and a flattened `extra: HashMap<String, serde_json::Value>`. Target paths live untyped at `extra["locations"][n]["path"]` (see the real payload at `protocol.rs:977` — `"locations": [ { "path": "/tmp/fs-probe.txt" } ]` — and the round-trip test asserting `locations` survives at `protocol.rs:1011`).

**Steps:**

1. In `crates/makina-acp/src/permission.rs`, locate the `WorktreePolicy::decide` method (line 96). The allow path runs only after `ctx.working_dir == self.worktree`; add the location validation inside that branch, *before* returning the `allow_once` decision.
2. Read `ctx.tool_call.extra.get("locations")`. If absent or not a JSON array, **allow** (fall back to the cwd check — there is nothing to validate). Otherwise iterate the array; for each element read `element["path"]` as a string and collect the candidate paths.
3. For each candidate path, validate it lies under `self.worktree` *without* denying merely because the file does not exist yet (tool calls legitimately create new files, so `std::fs::canonicalize` on the full path would error). Canonicalize the path's existing **parent** and rejoin the final component (or apply lexical normalization), then check the result is under `self.worktree`. A `canonicalize` error on a not-yet-created file must NOT by itself cause a deny.
4. If any location escapes the worktree, log a warning via `tracing::warn!(..)` (use the fully-qualified `tracing::warn!` — the crate logs through `tracing`, no `use` import needed) and return:
```rust
return PermissionDecision {
    allow: false,
    option_id: None,
    reason: "path escapes worktree".into(),
};
```
(`PermissionDecision` has exactly `{ allow, option_id, reason }` and derives no `Default`, so list every field — no `..Default::default()` rest syntax.)
5. Write a unit test `test_worktree_policy_denies_paths_outside_worktree` that builds a `PermissionRequestContext` whose `tool_call.extra` contains `"locations": [ { "path": "<a path outside the worktree>" } ]`, calls `decide`, and asserts `allow == false` with reason `"path escapes worktree"`.
6. Write a test `test_worktree_policy_allows_paths_inside_worktree` whose `extra["locations"]` paths sit inside the worktree (including a not-yet-existing file under the worktree) and asserts `allow == true`.

- **Depends on:** session-cancel-transport-method, usage-field-acp-response-chunk
- **Done when:** WorktreePolicy validates that the `tool_call.extra["locations"][n]["path"]` targets lie under the worktree; escaping paths are denied with `PermissionDecision { allow: false, option_id: None, reason: "path escapes worktree" }`, a missing `locations` falls back to allow, and a not-yet-created file under the worktree is NOT denied. The tests pass. Existing tests remain green. cargo test/clippy/fmt green.

---

**End of plan 0040 TASKS.** When every "Done when" bullet is green, the ACP
client is protocol-hardened: a per-call deadline bounds wedged *control* RPCs
(initialize / session_new / set_mode / set_config_option) so a stuck agent can no
longer hang the orchestrator, while legitimate long `session/prompt` turns are
left uncapped (the minutes-scale wall-clock cap is their outer backstop);
`session/cancel` is wired end-to-end from the transport through the AcpSession API
(via a sender clone that fires even while the client is in a turn worker) for
graceful in-flight cancellation; token usage and stop_reason propagate through the
AgentBackend trait (with an explicit `protocol::TurnUsage` → `api::UsageStats`
conversion) so the TUI metrics pane can show counts and distinguish Refusal from
EndTurn; run_id and task_id thread from `AcpBackend::spawn` into every
`AuditEntry` so audit records correlate to specific runs and tasks; and
WorktreePolicy validates `tool_call.extra["locations"]` target paths to catch
sandbox violations early — all with the gate commands green.
