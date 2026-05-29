# Architecture — Plan 0002 (deltas)

> Deltas to [`0001-Initial/ARCHITECTURE.md`](../0001-Initial/ARCHITECTURE.md).
> Only what changes for this increment. Line references are to the tree at
> authoring time and may drift; the symbol names are the stable anchors.

Three workstreams: **A. Cleanups**, **B. Minimal viable governance**,
**C. Task-graph persistence**. A and C are independent of B; B and C both touch
`.tasks/` and the Supervisor, so their audit/persist writes share the
single-writer discipline.

---

## A. Cleanups

### A1. Dedicated `MergeConflict` FSM event

**Problem.** `ReviewCapReached` is overloaded. The reviewer-cap exhaustion path
(`supervisor.rs:1444`) and the squash-merge **conflict** path
(`supervisor.rs:1408`) both emit it. The conflict site even carries a comment
(`supervisor.rs:1399–1408`) noting it "keeps using `ReviewCapReached`" because
`HardError` is "reserved for genuinely hard" failures. The result: a merge
conflict and a reviewer-cap exhaustion are indistinguishable in the task record.

**Correction from the design phase.** The *hard*-merge-error site
(`supervisor.rs:1368`) **already** uses `TaskEvent::HardError` (added in task
25). Only the *conflict* site still overloads `ReviewCapReached`. So this
cleanup is narrow: one new event, one call site changed.

**Change.** In `crates/makina-core/src/state_machine.rs`:
- Add `TaskEvent::MergeConflict` to the enum (`:67`).
- Add `(InReview, MergeConflict) => Ok(Failed)` to `transition()` (`:188`),
  beside the existing `(InReview, ReviewCapReached)` / `(InReview, HardError)`
  arms (`:211`–`:212`).
- Add `MergeConflict` to `legal_events(InReview)` (`:234`).
- Add the row to the module doc-comment transition table (`:35`–`:45`).
- The FSM stays **total**: `MergeConflict` is illegal from every non-`InReview`
  state (falls through to the `IllegalTransition` arm) — no other change.

In `crates/makina-core/src/actors/supervisor.rs`:
- At the conflict site (`:1408`), emit `TaskEvent::MergeConflict` instead of
  `ReviewCapReached`; update the surrounding comment (`:1399`–`:1408`).
- Leave the hard-merge site (`:1368`, `HardError`) and the reviewer-cap site
  (`:1444`, `ReviewCapReached`) unchanged.

The exhaustive `legal_events`/illegal-pairs tests in `state_machine.rs`
(`:276`+) must include the new variant so totality is re-proven.

### A2. Dedup `extract_json_object`

Byte-identical in `crates/makina-core/src/interpreter.rs:790` and
`crates/makina-core/src/roles.rs:324` (the latter a self-described "local
copy"). Move it once into a crate-internal module
`crates/makina-core/src/json.rs` (`pub(crate) mod json;` in `lib.rs`), exposing
`pub(crate) fn extract_json_object(&str) -> Option<&str>`. Both call sites use
`crate::json::extract_json_object`. The five unit tests currently in
`interpreter.rs` (`:1374`–`:1401`) move to `json.rs`'s test module.

---

## B. Minimal viable governance (action gateway + audit)

### Why it's needed

In gemini's default ("Prompts for approval") mode, every file-write attempt is
preceded by a server→client `session/request_permission` JSON-RPC request. The
transport currently drops all inbound requests
(`crates/makina-acp/src/transport.rs:393`, `IncomingKind::Request => {}`), so
gemini blocks until the wall-clock cap fires. The trial worked around this with
`--yolo` (auto-approve everything), which bypasses all consent.

A `session/request_permission` request **is** an agent-initiated action seeking
authorization. Intercepting it, applying a policy, replying, and auditing the
decision is exactly the "Deterministic governance" direction at its smallest
slice — and it is the architectural home for the eventual policy engine.

### The interception path, by file

**`crates/makina-acp/src/protocol.rs`** — wire types. `classify()` (`:89`)
already maps `session/request_permission` to `IncomingKind::Request` (proven by
the test at `:568`). Add:
- `pub const METHOD_SESSION_REQUEST_PERMISSION: &str = "session/request_permission";`
- `RequestPermissionParams` (deserialize): `session_id`, the offered `options`,
  and the pending `tool_call`. Model only the fields we audit
  (`tool_call_id`, `title`, `kind`); capture the rest via
  `#[serde(flatten)] extra: HashMap<String, Value>` so a richer tool call never
  fails to parse.
- `PermissionOption` + `PermissionOptionKind` (`allow_once` / `allow_always` /
  `reject_once` / `reject_always`, snake_case, with an `Other` catch-all).
- `PermissionResponse` (serialize) producing the ACP `result` outcome shape, and
  a new `OutgoingResponse { jsonrpc, id, result }` envelope — the transport has
  no server-bound *response* primitive today (only request/notification).
- `ClientCapabilities` (`:213`, currently `{}`): set per the
  `verify-permission-trigger` finding (see Decisions).

**`crates/makina-acp/src/permission.rs`** (new) — the policy seam.
```text
trait PermissionPolicy { fn decide(&self, &PermissionRequestContext) -> PermissionDecision; }
struct PermissionDecision { allow: bool, option_id: Option<String>, reason: String }
struct PermissionRequestContext<'a> { session_id, tool, options, working_dir }
```
Default `WorktreePolicy`: when the session `working_dir` is the configured
per-task worktree, auto-allow, selecting the offered `allow_once` option
(least privilege over `allow_always`), with a non-empty reason string. The MVP
does **not** validate that the tool's target path is inside the worktree — that
is the sandboxing direction (FUTURE); auditing the decision is enough now.
Document that limitation in the policy's doc-comment.

**`crates/makina-acp/src/transport.rs`** — replace the drop at `:393`. In the
reader loop (which already owns the write half and the notification sender),
handle `IncomingKind::Request` **inline**: parse `RequestPermissionParams`,
call the injected `Arc<dyn PermissionPolicy>` with the session `working_dir`,
write the JSON-RPC `OutgoingResponse` via a new
`TransportSender::send_response(id, result)`, and emit an `AuditEntry` via the
injected `Arc<dyn AuditSink>`. Inline (not bridged up to the session) because
the policy is synchronous and deterministic; bridging would add a stalling
async round-trip per permission request. Unmodeled inbound request methods are
logged-and-skipped — **never** left to hang.

**Threading.** Add `policy` + `audit` to `AcpCommand`/`AcpBackend`, passed
through `AcpClient::connect` → `spawn_transport` → `Transport::new`, and through
`AcpClient::with_transport` (test seam) as arguments. `working_dir` already
flows via `SessionConfig` to `AcpBackend::spawn`.

### The audit ledger — first real governance artifact

`AuditEntry` (serde `Serialize`) + `AuditSink` trait live in a new
`crates/makina-core/src/governance.rs` (so both crates share the type). Fields:
`timestamp`, `run`, `task`/`session_id`, `tool` (name/kind/id/title),
`decision` + `option_id`, `policy` name + `reason`, `working_dir`.

The default sink is **Supervisor-owned**: it appends one JSON line per decision
to `.tasks/{slug}/audit.jsonl`. Routing the file write through a Supervisor-held
sink preserves the documented invariant "Only the Supervisor writes to
`.tasks/`" (`0001-Initial/ARCHITECTURE.md` Key boundaries) — the gateway in
`makina-acp` *produces* decisions; the Supervisor *persists* them. JSONL is
append-only, durable, and diff-reviewable, matching "reviewable via diff,
recoverable from history".

### Removing `--yolo`

Once interception + auto-allow works, default-mode permission prompts are
answered inline and the turn proceeds via normal `session/update`. The
`MAKINA_ACP_ARGS=--acp,--yolo` workaround in `crates/makina/tests/e2e.rs` is no
longer needed; default args return to `--acp`. The asserting gate is a
no-subprocess duplex-transport test: a scripted peer sends
`session/request_permission` mid-turn, and the test asserts the allow-once
response is written, the turn completes, and exactly one `decision=allow`
audit entry is captured. This is deterministic and runs in CI without a real
agent; the real-agent confirmation stays an `#[ignore]` test.

---

## C. Task-graph persistence

### Why it's needed

The runtime schema is fully specified (`docs/spec/runtime-artifact-schema.md`)
and the Rust types in `crates/makina-core/src/task.rs` (`TaskGraph`, `Task`,
`TaskState`) are already serde-ready, with `skip_serializing_if`/
`#[serde(default)]` matching the "omitted-not-null" rule. But nothing writes or
reads the file: the graph lives only in `Arc<Mutex<TaskGraph>>`, every
`OpenRun` re-parses the `.md`, and a crash loses all in-flight state. The VISION
"tracked artifact / recoverable / diff-reviewable" principle is unmet.

### Write path — the choke point

`apply_event_locked` (`supervisor.rs:1630`) is the single mutation function,
called under the `Arc<Mutex<TaskGraph>>` guard at every transition site
(`:1086` New→Ready, `:1263` Dispatched, `:1382` Approved, `:1408` MergeConflict,
`:1444` ReviewCapReached, `:1464` Rejected, `:1535`/`:1605` HardError,
`:1553`/`:1568`/`:1587` gate loop) plus `:1008` WallClockCapReached in the
scheduler.

Add a new module `crates/makina-core/src/persist.rs`:
- `tasks_path(repo_root, slug) -> PathBuf` → `repo_root/.tasks/{slug}.json`
- `persist_graph(graph, repo_root)` — serialize pretty JSON + trailing `\n`,
  write to a temp file **in the same `.tasks/` dir**, then `std::fs::rename`
  over `{slug}.json`. Atomic on the same filesystem: a crash mid-write leaves
  either the old complete file or the temp file, never a torn artifact. Uses
  `tokio::fs` + `std::fs::rename` — no new production dependency.
- `load_graph(repo_root, slug) -> Option<TaskGraph>`
- `recover_for_resume(&mut TaskGraph)` (see Read path)

Add `DriverContext::persist`: clone the graph under the lock, then serialize +
write **outside** the lock (minimizes hold time; the clone is cheap for graphs
< 100 tasks). Call `ctx.persist().await` after each locked mutation block in
`task_driver` / `develop_until_gates_pass` / the scheduler, plus a seed write at
run start. `repo_root` comes from `DriverContext.worktree_manager.repo_root`
(`worktree.rs:133`; already on the context). Persistence failure is
**best-effort** — log a warning, do not fail the run; an otherwise-healthy
in-memory run must not die on a transient disk error. This is now the **only**
code path that writes the file, so the single-writer invariant is actually
enforced for the first time.

### Read / resume path

In `CoreApi::open_run` (`orchestrator.rs:340`), after deriving `slug` and before
calling `interpret` (`:362`):
1. `load_graph(repo_root, slug)`; `repo_root` from
   `self.state.worktree_manager.repo_root`.
2. **`Some(graph)`** → `recover_for_resume(&mut graph)`, `validate()`, register
   it. Do **not** re-interpret the `.md` — the JSON is the source of truth once
   emitted (schema §1; convention §2).
3. **`None`** → interpret the `.md` as today, then **seed-write** the fresh
   graph so the artifact exists from the first `OpenRun`.

`recover_for_resume` (pure, unit-testable): a process that died mid-run leaves
tasks in `in-progress`/`in-review` whose worktrees may be stale.
- `in-progress` → `ready`, `in-review` → `ready` (re-dispatch from scratch).
- `done` / `failed` preserved (terminal; never re-run).
- `new` / `ready` unchanged (the scheduler recomputes readiness from `done`
  deps anyway).
- `gate_iterations` / `review_iterations` **preserved** (schema §7: cumulative,
  never reset) so a crash-loop still hits its caps.

Deeper crash recovery (WAL, op-replay, partial-diff reconciliation) is out of
scope per FUTURE.md and schema §11.

### Concurrency

Multiple drivers mutate the shared graph; each persist clones under the existing
graph lock (consistent snapshot) then does pure async I/O holding no lock. The
file is rewritten in full each time and replaced atomically, so concurrent
drivers produce a sequence of fully-valid files (last rename wins; the file
converges as every subsequent transition rewrites the whole graph). No new lock
is taken, so the documented lock ordering (semaphore → graph → merge) is
preserved — no deadlock surface. Cost: one clone + serialize + write per
transition, negligible against minutes-scale agent turns.

### Git

`.gitignore` is already correct (`/.worktrees/` ignored; `.tasks/` explicitly
tracked) — **verify only, no edit**. This increment **writes** `.tasks/{slug}.json`
but does **not** auto git-commit it: committing on every transition would create
dozens of noisy commits per task and collide with the per-task squash-merge into
`develop`. Commit is left to CI / the user / a future increment. The "every
state change is a reviewable diff" property holds for *inspection* (`git diff`
shows live progress); committing into history is deferred.

### Transparency to existing tests

All supervisor integration tests build `WorktreeManager::new(tempdir, …)`, so
persistence writes into a per-test temp repo that is auto-cleaned. Tests assert
on `TaskGraphSnapshot`/`RunReport`, not on filesystem absence, so they stay
green — no disable-persistence flag is needed.

---

## Decisions & open questions

**Locked (consistent with the "minimal viable" scope):**

| Decision | Choice |
|----------|--------|
| Persist strategy | Clone-under-lock, serialize/write outside lock |
| Persist failure | Best-effort: log + warning event, never fail the run |
| Resume rule | Non-terminal → `ready`; terminal + counters preserved |
| `.md` vs `.json` conflict | JSON artifact wins (source of truth once emitted) |
| Audit granularity | Per-run `.tasks/{slug}/audit.jsonl` |
| Audit writer | Supervisor-owned sink (preserves single-`.tasks/`-writer) |
| Policy crate | `makina-acp`; `PermissionPolicy` trait is the migration seam |
| Option preference | `allow_once` (least privilege over `allow_always`) |
| `.tasks/` git | Write-only this increment (no auto-commit) |

**Empirical unknown — resolved by the first governance task
(`verify-permission-trigger`):** does real `gemini --acp` emit
`session/request_permission` with the current empty `ClientCapabilities`, or
must an `fs` capability be advertised? Advertising
`fs.{read,write}TextFile = true` tells the agent the *client* will perform file
IO via `fs/read_text_file` / `fs/write_text_file` server→client requests, which
this plan does **not** implement — so advertising it may reroute writes through
us. The safe hypothesis is to keep empty capabilities and rely purely on
intercepting `session/request_permission`; the task confirms this against a real
agent before the `ClientCapabilities` literal is fixed. Everything else in the
gateway is unaffected.

**Deferred minor questions** (record outcome during implementation, not
blocking): exact reject-outcome encoding (unused by the auto-allow policy);
whether to keep `started_at` on an `in-progress`→`ready` reset (recommend keep);
whether the squash-merge should stage `.tasks/{slug}.json` (deferred with the
commit policy).
