# Makina Trial Findings (plan 0001 — task 34)

> **Purpose:** Record what worked, what broke, and which `FUTURE.md` directions
> the test-drive makes most urgent, to inform prioritization for plan 0002.
> Run evidence lives in [`docs/trial/e2e-run.md`](e2e-run.md).

---

## 1. What worked

### The full loop ran end-to-end with a real agent

The automated harness (`crates/makina/tests/e2e.rs`) drove `CoreApi` — the
exact backend the TUI binds — through a complete cycle against a real
`gemini --acp` process:

| stage | result |
|-------|--------|
| Planner (deterministic interpreter + EdgeInferrer) | 2 tasks, correct dep edge |
| Worktree creation | `task/format-duration` off `develop` |
| Developer (gemini) | correct, idiomatic, documented Rust + unit tests on first attempt |
| Gates (`cargo test`, `cargo clippy`, `cargo fmt`) | all passed first try — 0 gate iterations |
| Reviewer (gemini) | emitted parseable `{"verdict":"approve"}` — 0 review iterations |
| Squash-merge | `task(format-duration): Add a human-readable duration formatter` on `develop` |
| Dependency unlock | `kebab-validate` advanced `Ready → InProgress` immediately after |

Total time: **~124s** on a warm `cargo` build cache. 21 events, 2 agent prompts,
9 response chunks, 2 turns. The live repo was never touched (temp-clone isolation
worked perfectly).

### The architecture held up

Every major structural bet from plan 0001 was validated:

- **Star actor topology (kameo)** — Supervisor as hub, isolated spokes;
  failures did not propagate between actors.
- **Explicit task FSM** — all states (`Ready`, `InProgress`, `InReview`, `Done`)
  reached in the correct order; the transition rules held under a real agent.
- **Worktree-per-task isolation** — Developer and Reviewer shared one worktree;
  merge landed cleanly; teardown was complete.
- **`NoopBackend` + integration harness** — the entire engine was verified
  deterministically without a model for all 30+ non-e2e tests. This was a large
  confidence win: by the time the real agent ran, the mechanics were already
  proven.
- **Agent-backend trait seam (`makina-acp`)** — swapping `NoopBackend` for
  `AcpBackend(gemini)` was one constructor change; the orchestration code was
  untouched.
- **Two-layer TOML config + gates** — project `makina.toml` + global
  `~/.makina/config.toml` merged and validated at startup; gates ran in the
  correct worktree context.
- **Termination caps** — `gate_iterations`, `reviewer_iterations`,
  `wall_clock_secs` all exercised in non-e2e tests; the cap infrastructure was
  there and trustworthy.
- **TUI over `makina-core::api` thin-shell boundary** — the harness and the TUI
  drive the same `Arc<dyn Api>` surface; the TUI is pure presentation with no
  orchestration logic.
- **Concurrency with merge lock** — parallel task scheduling and serialized
  squash-merges work with `NoopBackend`; the design held.

### "Deterministic governance via gates" is real

The exit-code-zero gate chain (test → clippy → fmt) gating Developer→Reviewer
is a working instance of the governance pitch: agent work does not advance to
review unless it passes objective checks. The trial's clean first-pass confirms
it is not theoretical.

---

## 2. What broke / gaps

### ACP permission flow — most impactful

**What happened:** plain `gemini --acp` hung the Developer turn indefinitely.
Gemini (in its default `"Prompts for approval"` mode) streams partial output and
then sends a **server→client** `session/request_permission` JSON-RPC request
(with `allow_once` / `allow_always` / `reject` options and the pending
`write_file` tool call) before writing any file.

**Root cause in the code:** `crates/makina-acp/src/transport.rs` advertises
empty `clientCapabilities {}` and silently drops all inbound server→client
requests:

```rust
IncomingKind::Request => {}
// "permission prompts are out of scope for the MVP turn;
//  we neither answer nor fail on them"
```

So gemini blocks forever waiting for a response that never comes. The per-task
`wall_clock_secs` cap would eventually fire, but at 1200s that means a 20-minute
hang per task.

**Workaround used:** pass `--yolo` to gemini (auto-approves all tool calls, no
permission request emitted) via `MAKINA_ACP_ARGS=--acp,--yolo`. This is pure
agent CLI config via the existing `makina.toml` / env seam; no engine change was
needed for the trial. But `--yolo` bypasses all user consent — acceptable in a
sandboxed throwaway clone, not acceptable in production.

**Why this matters beyond the workaround:** a `session/request_permission`
request IS an agent-initiated action seeking authorization. Intercepting it,
applying a policy, and auditing the decision is precisely what the governance
direction describes. The bug is therefore a concrete preview of the next problem
to solve.

### Squash-merge conflict reconciliation — documented seam, not implemented

The architecture (ARCHITECTURE.md §Concurrency) and the supervisor code both
document "reconcile via agent prompt" as the correct path for straggler merge
conflicts. The implemented path safely aborts (restores `develop`, emits
`ReviewCapReached` → `Failed`) rather than attempting reconciliation.
`develop` was never corrupted in any test — the safety property holds — but
conflicting tasks currently fail rather than recover. The agent-reconcile branch
is a dead code path with a comment.

### FSM failure modeling — `ReviewCapReached` overloaded

Hard errors at review time (non-conflict merge failures, reviewer actor crashes)
and merge conflicts both reach `Failed` via `ReviewCapReached`
(`crates/makina-core/src/actors/supervisor.rs`, line ~1408 and ~1444). The event
was designed for reviewer iteration exhaustion. Using it for structurally
different failure causes works (the FSM reaches `Failed` correctly) but obscures
the reason in the task record. A dedicated `MergeConflict` event and a
`HardError` terminal event would make failure reasons inspectable.

### Model-backed Planner path not exercised end-to-end

The `ModelInterpreter` (direct model call path, implemented in task 18) was
verified in isolation (`crates/makina-acp/tests/model_interpreter_real.rs`).
The e2e trial used the deterministic `StructuredTextInterpreter` (sufficient for
the well-formed dogfood task list). The model-planning path has not been
exercised inside the full orchestration loop. For task lists with ambiguous
descriptions or implicit dependencies, this path is untested in practice.

### `.tasks/{slug}.json` persistence never written — task graph is in-memory only

**What the VISION and spec promise:** The VISION principle states "Work state is
a tracked artifact … Task graph, progress, reviewer outcomes — all materialize
as files in the repo … reviewable via diff, recoverable from history."
`docs/spec/runtime-artifact-schema.md` designates `.tasks/{slug}.json` as the
runtime "source of truth" that the Supervisor writes and owns; the "Supervisor is
the only `.tasks/` writer" is a documented invariant.

**What the implementation actually does:** The `TaskGraph` lives exclusively
in-memory, held behind `Arc<Mutex<TaskGraph>>` inside `CoreApi` / the Supervisor.
On every `OpenRun` call the engine re-parses the `.md` input from scratch. There
is no write path: the Supervisor never serializes a `TaskStatus` update to
`.tasks/{slug}.json`, and there is no read path on restart. The schema types and a
sample file exist from the `runtime-artifact-schema` task; only the runtime
write/read path is absent.

**Consequences:**
- No crash recovery: if the Supervisor actor panics or the process is killed
  mid-run, all in-flight task state is lost.
- No diff-reviewable state history: `.tasks/` stays empty, so `git diff` shows
  nothing about task progress — the VISION "reviewable via diff" property is
  vacuously false.
- The "Supervisor is the only `.tasks/` writer" invariant holds only because
  nobody writes it at all.

### `extract_json_object` helper duplicated

The function is defined independently in both
`crates/makina-core/src/interpreter.rs` (line 790) and
`crates/makina-core/src/roles.rs` (line 324), with a comment in `roles.rs`
acknowledging it as a "local copy". Minor technical debt; the fix is a shared
private helper in `makina-core`.

---

## 3. Slowness, cost, and test-quality observations

### Latency

A real agent turn is minutes-scale. A cold `cargo` gate run in a fresh worktree
(no shared `target/`) adds significant time — the trial benefited from a warm
build cache on a shared host directory. Budget generously: the harness uses a
20-minute observe deadline and `wall_clock_secs = 1200` per task. Shared `target/`
mounts or `sccache` would help materially.

### No cost accounting

There is no per-turn, per-task, or per-run token or spend tracking. For
single-file tasks with a capable model (gemini first-passed everything), the
effective cost is low. For tasks requiring iteration, cost is unbounded until
the gate or reviewer cap fires. This becomes a real concern once tasks grow in
complexity or volume.

### No hang detection below the wall-clock cap

If an agent process stalls for a reason other than the permission-request
protocol gap (e.g., a model API timeout, a subprocess crash mid-stream), the
only backstop is the per-task `wall_clock_secs` cap. There is no shorter
idle-output timeout or cooperative heartbeat.

### Concurrency test design

The concurrency tests (`crates/makina-core/tests/concurrency.rs`) use an
N-party `Barrier` to force simultaneous active sessions — a clever, timing-free
approach. Under very heavy parallel test execution (many test workers running
all integration tests at once), shared `tempdir` creation and barrier
party-count assumptions can race if the OS or test runner overloads. The product
logic is sound; the concern is test infrastructure hardening for high-parallelism
CI environments.

---

## 4. Prioritized next directions

These map directly to entries in [`FUTURE.md`](../plan/0001-Initial/FUTURE.md).
Ranked by how strongly the trial's evidence motivates each.

### 1. Deterministic governance — action gateway + policy engine + audit log [FUTURE: "Deterministic governance"]

**Trial evidence:** the `session/request_permission` finding is not an edge case;
it is the normal operating mode for gemini without `--yolo`. Every file-write
attempt triggers a permission request. Makina currently drops these silently and
hangs. The correct engine response is: intercept the request, evaluate a policy
(e.g., "inside isolated worktree: auto-allow"), record the decision, and reply.
That is the action gateway. Implementing this also eliminates the need for
`--yolo` and gives the system the first real governance artifact (the audit log
entry per permission decision).

This is the wedge the project is built around. The trial converts it from
aspiration to next-step: without it, the engine requires `--yolo` for every
supported agent that has a default-mode permission model.

### 2. Task-graph persistence — write/read `.tasks/{slug}.json` at runtime [HIGH]

**Trial evidence:** the VISION "tracked artifact / recoverable / diff-reviewable"
guarantee is completely unmet. Every run is stateless: a crash loses all in-flight
task progress, `git diff` shows nothing about task state, and there is no path
back from a partial run without starting over. The schema and types are already
designed (`docs/spec/runtime-artifact-schema.md`, the sample file); the only missing
piece is the Supervisor serializing a `TaskStatus` update to `.tasks/{slug}.json`
on every FSM transition, and reading those files back on `OpenRun` when they exist.

**Why HIGH:** this closes the largest gap between the VISION spec and the running
code. It is also a prerequisite for the "Multiple task sources" direction (item 8
below): GitHub Issues → `.tasks/*.json` only makes sense once `.tasks/*.json` is
a live, maintained artifact — not an empty directory. Crash recovery alone
justifies the priority: any real workload risks losing agent turns to transient
failures.

### 3. Hard enforcement via sandboxing [FUTURE: "Hard enforcement via sandboxing"]

**Trial evidence:** `--yolo` auto-approves tool calls globally inside a process
that can reach the filesystem, the network, and spawned subprocesses. Safe
auto-approval (the default behavior inside the worktree) requires the per-task
sandbox to be the boundary, not the agent CLI's own flag. The gateway (direction
1) decides; the sandbox enforces. Without sandboxing, the gateway is
declarative-audit only. The trial makes both directions urgent as a pair: the
gateway handles the protocol; the sandbox provides the teeth.

Linux-first implementation is the practical path; macOS sandbox primitives are
weaker. This is the harder engineering problem — schedule after direction 1.

### 4. Hang detection as an explicit subsystem [FUTURE: "Hang detection"]

**Trial evidence:** a hung `--acp` turn (the first real run without `--yolo`)
consumed the wall-clock deadline silently. Even with the permission flow fixed,
agent processes can stall mid-stream for other reasons (model API timeout,
subprocess crash, lost pipe). An idle-output timeout (e.g., 30s with no new
stream chunk) would catch these cases 40x faster than the 1200s wall-clock cap
and emit a useful event rather than a silent expiry.

### 5. Cost accounting and budget caps [FUTURE: "Cost accounting and budget caps"]

**Trial evidence:** the trial's single task was cheap, but the path to expensive
is short: a task requiring gate iteration or reviewer rejection cycles spends
tokens proportionally, with no visibility and no stop condition other than the
iteration cap. Adding per-task `UsageReport` reporting and a budget terminal
condition transforms the cap from time-only to cost-aware — necessary before
running larger task lists against paid APIs.

### 6. Richer reviewer-side rule kinds [FUTURE: "Richer reviewer-side rule kinds"]

**Trial evidence:** the Reviewer is LLM-based with no deterministic constraints
on its verdict. The MVP gates (test/clippy/fmt) are deterministic and passed
first try; the Reviewer approved immediately. For tasks where the gate suite is
weaker, the Reviewer is the last line of defense — and it is an LLM that can
be inconsistent or hallucinate an approval. Deterministic reviewer rules
(interpreted lint output, required test coverage thresholds, AST predicates)
would make the governance pitch firmer and reduce reviewer-cap failures caused
by LLM inconsistency rather than actual code quality problems.

### 7. Agent-driven conflict reconciliation [FUTURE: "squash-merge" / orchestration]

**Trial evidence:** the conflict path safely aborts today; `develop` was never
corrupted. But for concurrent tasks that the EdgeInferrer did not serialize
(e.g., tasks touching different files in the same module that both add a public
symbol), straggler conflicts will occur as task volume grows. The agent-reconcile
prompt path (already designed in supervisor comments) turns a `Failed` task into
a recoverable one. Lower urgency than 1–6 since it requires more agent work per
task and only fires on actual conflicts, but it becomes a real reliability gap
at scale.

### 8. Multiple task sources [FUTURE: "Multiple task sources"]

**Trial evidence:** the dogfood list was hand-authored in the structured-text
convention. The format is expressive enough for small lists but friction-heavy
for real backlogs. GitHub Issues integration would close the loop from "ticket
in the backlog" to "commit on develop" without a format translation step.
Lower urgency in plan 0002 — the existing file-source is sufficient for
continued dogfooding — but the plugin contract shape is now informed by one
complete implementation.

---

## 5. Summary judgment

The MVP delivered what it claimed: a working orchestration spine, a real-agent
full loop, deterministic governance via gates, and a clean TUI-over-api
architecture. The `NoopBackend` approach paid off — 30+ integration tests pass
without a model, and the real-agent run worked on the first engine-level attempt.

The two most important findings are the ACP permission gap and the missing
`.tasks/{slug}.json` persistence. The ACP gap is the concrete, operational form
of the governance problem the project exists to solve; the persistence gap means
the VISION "tracked artifact / recoverable / diff-reviewable" principle is
completely unmet at runtime. Plan 0002's first two priorities should be (1) the
action gateway + permission flow in the ACP client, and (2) the Supervisor
write/read path for `.tasks/*.json` on every FSM transition, paired with the
sandbox work that gives enforcement power. The rest — hang detection, cost
accounting, richer reviewer rules, conflict reconciliation — are meaningful
improvements but lower urgency than closing these two correctness gaps.
