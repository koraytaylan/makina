# Makina Plan 0002 — Governance & Persistence

Structured-text task list for the first post-MVP plan, derived from
[`docs/trial/trial-findings.md`](../../trial/trial-findings.md) and the
[`Deterministic governance`](../0001-Initial/FUTURE.md) direction. Closes the
two correctness gaps the trial surfaced (ACP permission flow; `.tasks/*.json`
persistence) and pays down two cheap debts. See [SCOPE.md](SCOPE.md) for what
is in and out, and [ARCHITECTURE.md](ARCHITECTURE.md) for the design.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch
  `task/{id}` and worktree `.worktrees/{id}/`).
- **Depends on** lists *direct* structural prerequisites only; the Planner
  adds further dependency edges automatically for tasks that touch the same
  files or areas.
- **Done when** is the verifiable acceptance check used by gates and the
  Reviewer.

---

## 0009 — Cleanups

### fsm-merge-conflict-event — Dedicated MergeConflict FSM event
Add a `TaskEvent::MergeConflict` variant to
`crates/makina-core/src/state_machine.rs` with the single transition
`(InReview, MergeConflict) → Failed`; add it to `legal_events(InReview)` and
the module doc-comment transition table. Switch the supervisor squash-merge
**conflict** site (`actors/supervisor.rs`, the `ReviewCapReached` emission near
the merger's conflict branch) to `MergeConflict`. Leave the hard-merge failure
site (already `HardError`) and the reviewer-cap site (`ReviewCapReached`)
unchanged. Keep the FSM total: the new event is illegal from every
non-`InReview` state.
- **Depends on:** —
- **Done when:** `cargo test -p makina-core` passes with new assertions that
  `(InReview, MergeConflict) → Failed` and that `MergeConflict` is rejected
  with an illegal-transition error from `New`, `Ready`, `InProgress`, `Done`,
  and `Failed`, and the squash-merge conflict path emits `MergeConflict`.

### dedup-extract-json — Share the extract_json_object helper
Create `crates/makina-core/src/json.rs` (declared `pub(crate) mod json;` in
`lib.rs`) holding one `pub(crate) fn extract_json_object`. Remove the copy in
`interpreter.rs` and the "local copy" in `roles.rs`; both call
`crate::json::extract_json_object`. Move the helper's unit tests into
`json.rs`.
- **Depends on:** —
- **Done when:** `extract_json_object` is defined exactly once under
  `crates/makina-core/src`, and `cargo test -p makina-core` and
  `cargo clippy -p makina-core -- -D warnings` both pass.

---

## 0010 — Minimal Viable Governance

### verify-permission-trigger — Confirm what triggers session/request_permission
Empirically determine, against a real `gemini --acp`, whether the agent emits
`session/request_permission` with the current empty `clientCapabilities`, or
whether advertising an `fs` capability is required. Capture the exact request
params and the expected response shape. This decides the `ClientCapabilities`
literal only; the rest of the gateway design is unaffected.
- **Depends on:** —
- **Done when:** a written note records the observed trigger and the concrete
  `session/request_permission` params and expected response shape, and an
  `#[ignore]` probe test capturing them exists where practical.

### audit-entry-type — Shared AuditEntry and AuditSink seam
Add `crates/makina-core/src/governance.rs` (`pub mod governance;` in `lib.rs`)
defining a serde-`Serialize` `AuditEntry` (timestamp, run, task or session,
tool name and kind and id and title, decision and option id, policy name and
reason, working dir) and an `AuditSink` trait with a `record` method, plus a
no-op default sink for tests.
- **Depends on:** —
- **Done when:** `cargo test -p makina-core` covers an `AuditEntry`
  round-trip that serializes to a stable single-line JSON shape and a test that
  the no-op sink accepts a record.

### acp-permission-types — ACP permission wire types and capability
In `crates/makina-acp/src/protocol.rs` add the
`session/request_permission` method constant, `RequestPermissionParams`
(deserialize, with unknown tool-call fields preserved via a flattened map),
`PermissionOption` and `PermissionOptionKind`, the serialize
`PermissionResponse`/outcome types, and an `OutgoingResponse` JSON-RPC
envelope. Set `ClientCapabilities` per the `verify-permission-trigger`
finding.
- **Depends on:** verify-permission-trigger
- **Done when:** `cargo test -p makina-acp` passes new round-trips: a real
  `session/request_permission` params payload deserializes (including an
  unknown tool-call field that survives), and `PermissionResponse` serializes
  to the exact ACP outcome shape; request classification is unchanged.

### worktree-permission-policy — Worktree-scoped auto-allow policy
Add `crates/makina-acp/src/permission.rs` defining a `PermissionPolicy` trait
(a `decide` method over a request context returning an allow/deny decision) and
the default `WorktreePolicy` that auto-allows with the offered allow-once
option when the session working dir is the configured per-task worktree, with a
non-empty reason. Select the allow-once option id deterministically from the
offered options.
- **Depends on:** acp-permission-types
- **Done when:** `cargo test -p makina-acp` proves that, given a request
  offering allow-once, allow-always, and reject options inside a worktree
  context, the policy returns allow with the allow-once option id and a
  non-empty reason, and that the trait is object-safe behind an `Arc`.

### transport-permission-response — Reader answers permission requests
Add a `send_response` primitive to the transport sender in
`crates/makina-acp/src/transport.rs`. Replace the inbound-request drop
(`IncomingKind::Request => {}`) with: parse the permission params, invoke an
injected permission policy with the session working dir, write the JSON-RPC
response, and emit an audit entry to an injected audit sink. Thread the policy,
working dir, and audit sink through the transport constructor, reader loop, and
message router; other inbound request methods are logged and skipped so they
never hang.
- **Depends on:** acp-permission-types, worktree-permission-policy, audit-entry-type
- **Done when:** an in-memory duplex-transport test (no subprocess) in which the
  scripted peer sends `session/request_permission` mid-turn asserts the client
  writes a response selecting the allow-once option id, the turn then completes,
  and exactly one audit entry with an allow decision is captured by a test sink;
  `cargo test -p makina-acp` passes.

### gateway-threading — Thread policy and audit through backend and client
Add policy and audit fields to the ACP command and backend types (defaulting to
the worktree policy built from the session working dir), and pass them through
the client connect path, the transport spawn, and the transport test seam as
arguments.
- **Depends on:** transport-permission-response
- **Done when:** `cargo test -p makina-acp` passes, including a backend-trait
  test that a full mock-agent turn with an interleaved permission request
  completes through the agent session and records an audit entry, and existing
  ACP tests pass with the added constructor parameters.

### supervisor-audit-writer — Supervisor owns the audit ledger write
Implement an audit sink in `makina-core` that the Supervisor injects into the
backend, appending JSONL to `.tasks/{slug}/audit.jsonl` (one line per decision),
preserving the invariant that only the Supervisor writes under `.tasks/`. Wire
the run and task ids into the entries. Update `crates/makina/src/main.rs` to
construct the backend with the worktree policy and this sink.
- **Depends on:** gateway-threading
- **Done when:** an integration test drives a run that triggers a permission
  decision and asserts `.tasks/{slug}/audit.jsonl` exists, contains one JSON
  line per decision with the run and task ids and the decision and reason
  populated, and that re-running appends rather than truncates; the workspace
  test suite passes.

### drop-yolo-workaround — Remove the --yolo dependency
Update `crates/makina/tests/e2e.rs`: default the ACP args to `--acp` without
`--yolo`, and rewrite the explanatory comment to describe the gateway. Confirm
via the deterministic gateway test that default-mode permission prompts are
answered without `--yolo`.
- **Depends on:** supervisor-audit-writer
- **Done when:** no functional `--yolo` usage remains under `crates/`, the
  deterministic permission test is the asserting gate, and the ignored e2e
  invocation documents plain `--acp`.

---

## 0011 — Task-Graph Persistence

### persist-module — Read and write .tasks/{slug}.json
Add `crates/makina-core/src/persist.rs` with a path helper, an atomic
`persist_graph` (serialize pretty JSON, write to a temp file inside `.tasks/`,
then rename over `{slug}.json`), a `load_graph` returning an optional graph, and
a persist error type. Use the async filesystem plus a synchronous rename; add no
new production dependency. Register the module in `lib.rs`.
- **Depends on:** —
- **Done when:** unit tests show a `TaskGraph` round-trips through
  `persist_graph` then `load_graph` to an equal value; omitted optional fields
  stay omitted (no `null`) in the written file; `load_graph` returns none for a
  missing file; and an interrupted write never leaves a partial `{slug}.json`.

### supervisor-write-path — Persist the graph on every FSM transition
Add a driver-context persist helper that snapshots the graph under the lock and
writes it via `persist_graph` outside the lock, and call it after each locked
mutation block in the task driver, the develop-until-gates loop, and the
scheduler (including the wall-clock cap and the new-to-ready advance).
Persistence failures are best-effort: logged, never failing the run. The repo
root comes from the worktree manager on the driver context.
- **Depends on:** persist-module
- **Done when:** an integration test drives a graph to terminal states against a
  temp repo and asserts `.tasks/{slug}.json` exists and its final on-disk
  contents (states, iteration counts, timestamps) match the task-graph
  snapshot; existing supervisor integration tests pass unchanged.

### orchestrator-seed-write — Seed the artifact on a fresh OpenRun
In the core api `open_run`, after interpreting a fresh task-list file with no
existing artifact, write the seeded graph via `persist_graph` using the worktree
manager's repo root, so the artifact exists immediately.
- **Depends on:** persist-module
- **Done when:** opening a run on a task list in a temp repo with no
  pre-existing `.tasks/{slug}.json` creates the file with all tasks in the
  `new` state, asserted before any start-run command.

### resume-recovery-rule — Reset non-terminal states for safe resume
Add a pure `recover_for_resume` function that resets in-progress and in-review
tasks to ready, preserves done and failed tasks and the cumulative iteration
counters, and leaves new and ready tasks unchanged.
- **Depends on:** persist-module
- **Done when:** unit tests show in-progress becomes ready and in-review becomes
  ready, done and failed are unchanged, counters are preserved, and the result
  passes the task-graph validation.

### orchestrator-read-path — Resume from an existing artifact on OpenRun
In the core api `open_run`, before interpreting, attempt to load an existing
artifact for the slug; if present, apply the resume recovery rule, validate it,
and register that graph instead of re-interpreting the task-list file (the JSON
artifact is the source of truth). Otherwise fall back to interpret and
seed-write.
- **Depends on:** resume-recovery-rule, orchestrator-seed-write, supervisor-write-path
- **Done when:** an integration test opens a run, advances some tasks to done
  and one to in-progress (persisted to disk), then issues a second open-run on
  the same repo and slug and asserts the registered graph preserves the done
  task and shows the formerly in-progress task as ready (not new), without
  re-reading the task-list file.

### gitignore-commit-policy — Verify ignore rules and document commit policy
Confirm `.gitignore` keeps `/.worktrees/` ignored and `.tasks/` tracked (no edit
expected). Document that this increment writes `.tasks/{slug}.json` but does not
auto git-commit it, and note the tradeoff in the persistence module docs.
- **Depends on:** supervisor-write-path
- **Done when:** a check confirms `.worktrees/` is gitignored and `.tasks/` is
  not, the persisted file appears in `git status` of the working tree, and the
  commit-policy decision is recorded in the module docs.
