# Architecture — Plan 0004 (deltas)

> Deltas to [`0001-Initial/ARCHITECTURE.md`](../0001-Initial/ARCHITECTURE.md),
> plan 0002, and plan 0003. File:line references were grounded against
> `develop @ d00a180` (immediately after the plan-0003 merge); symbol names are
> the stable anchors.

Four workstreams: **A. model-backed ingestion**, **B. ingestion validator /
linter**, **C. Qualifier entry-gate**, **D. review & approval gate**. B and C
are pure `TaskGraph → Vec<IngestionIssue>` functions and can land independently
(fixture-tested, no model). A wires the model interpreter into the TUI. D is the
integrator: it threads the `IngestionReport` onto the run, renders it, and gates
`StartRun` — so it depends on A (a graph to review) plus B and C (the issues to
surface).

---

## The target ingestion flow

```
OpenRun(path)
  ├─ artifact-first (UNCHANGED): .makina/tasks/{slug}.json exists?  → load it, skip interpret
  └─ fresh interpret:
        backend present?  → ModelInterpreter         (model → JSON TaskGraph)   ← default
        no backend?       → StructuredTextInterpreter (deterministic fallback)
        … EdgeInferrer composed on top of whichever
     → validate(&graph)   → Vec<IngestionIssue>   (structural; collect ALL)
     → qualify(&graph)    → Vec<IngestionIssue>   (semantic actionability)
     → IngestionReport { issues }                 (interpreter failure also folds in here)
     → register Pending run carrying graph + report; seed-persist the graph
StartRun(run)
  → REFUSED while report.is_blocked();  unchanged once clean
ReinterpretRun(run)  (new)
  → re-run the interpreter bypassing the persisted artifact; replace graph + report
```

`OpenRun` already `.await`s the interpreter, so a multi-second model call fits
the existing async command path; the run simply appears in `Pending` once
interpret returns. (A dedicated "Interpreting…" status is intentionally **not**
added — see decisions.)

---

## A. Model-backed ingestion (`crates/makina/src/main.rs`, `crates/makina-core/src/interpreter.rs`)

### Today
`main.rs` builds the interpreter hard-coded as
`Arc::new(EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new())))`
(`main.rs:~103-114`) and passes it to `CoreApi::with_audit_registry(...)`. The
ACP backend is constructed alongside (`AcpBackend::new(...)`, `main.rs:~94-100`)
but is used **only** for task execution (Developer / Reviewer), never for
planning. The model path already exists and is unreachable:
`ModelInterpreter` (`interpreter.rs:~672`), its `new(Arc<dyn AgentBackend>)`
(`interpreter.rs:~684`), `PLANNER_SYSTEM_PROMPT` (`interpreter.rs:~603`),
`parse_model_response` (`interpreter.rs:~771`), and the factory
`build_planner_interpreter(mechanism, Option<Arc<dyn AgentBackend>>)`
(`interpreter.rs`, tested at `interpreter.rs:~1327-1368`) which returns a
`ModelInterpreter` for `OneShotAgent + Some(backend)`, a `StructuredTextInterpreter`
for `OneShotAgent + None`, and `MechanismNotSupported` for `DirectApi` (see
[`docs/spec/planner-model-mechanism.md`](../../spec/planner-model-mechanism.md)).

### Change
Select the interpreter through the factory instead of hard-coding it:
`let inner = build_planner_interpreter(mechanism, backend_for_planning)?;`
then keep the existing decorator: `Arc::new(EdgeInferrer::new(inner))`. The
`mechanism` comes from config (default `OneShotAgent`); `backend_for_planning` is
`Some(Arc::clone(&backend))` in production (→ model) and `None` offline / in
tests (→ deterministic). `build_planner_interpreter` already encodes the
fallback, so "model is the default" is a one-line wiring change plus a config
field; the `TaskListInterpreter` trait, `open_run`, and the whole read-path are
untouched (they call `interpret(slug, source_text)` either way).

### Cost containment
The artifact-first read path in `open_run` (`orchestrator.rs:~567-604`) loads
`.makina/tasks/{slug}.json` when present and **skips interpretation entirely** —
so the model runs once per source (the fresh path at `orchestrator.rs:~653-691`),
and every subsequent re-open reuses the persisted graph. The new
`ReinterpretRun` command (workstream D) is the explicit way to force a fresh
model call after editing the source.

---

## B. Ingestion validator / linter (`crates/makina-core/src/ingestion.rs`, new)

### Today
`TaskGraph::validate()` (called inside every interpreter and `EdgeInferrer`)
returns the **first** `TaskGraphError` and stops — a fail-fast single-point error
(`InterpretError::ValidationFailed`, `interpreter.rs:~73`). The deterministic
parser surfaces one `ParseError { location, context }` at a time
(`parse_structured_text`, `interpreter.rs:~247-506`).

### Change
Add a pure, fail-soft structural checker in a new `ingestion` module:
`pub fn validate(graph: &TaskGraph) -> Vec<IngestionIssue>` that collects **all**
structural problems in one pass — dangling `depends_on`, duplicate ids, empty /
missing `done_when`, self-dependencies, and cycles (reuse the reachability walk
already in `dependency.rs`). Each becomes an `IngestionIssue` with
`source = Validator`, a stable `code`, and a `suggestion`. This does **not**
replace `TaskGraph::validate()` (the interpreters still need a hard pass/fail to
build a graph at all); it is an **additional**, richer report computed at
`OpenRun` after a graph exists. The deterministic offline parser additionally
gets multi-error diagnostics so a malformed `TASKS.md` lists every problem at
once instead of erroring on the first.

---

## C. Qualifier entry-gate (`crates/makina-core/src/ingestion.rs`)

### Today
Nothing checks task *actionability*. A structurally-valid task with a vague
`done_when` ("make it work") runs an agent and wastes the iteration.

### Change
Add a pure deterministic checker beside the validator:
`pub fn qualify(graph: &TaskGraph) -> Vec<IngestionIssue>` (`source = Qualifier`).
The heuristic set (string / structural predicates over each `Task`, no LLM):
- **non-empty, verifiable `done_when`** — present, above a minimum length, and
  not a placeholder;
- **no placeholder / `TBD` / `TODO` / `???` text** in title, description, or
  `done_when`;
- **actionable title** — starts with (or contains) an imperative verb, not a bare
  noun;
- **substantive description** — above a minimum length / not empty;
- **resolvable deps** — every `depends_on` id exists (overlaps the validator;
  emitted once, deduped by `(task_id, code)`).
Each flagged task yields a `Blocking` issue with the failing `code` and a
suggestion. Thresholds are named constants so they are tunable and test-pinned.
The Qualifier never mutates the graph and adds **no** FSM states — it only
produces issues that gate the start (workstream D).

---

## D. Review & approval gate (`crates/makina-core/src/{api.rs,orchestrator.rs}`, `crates/makina/src/{app.rs,event.rs,ui.rs}`)

### Data types (`crates/makina-core/src/ingestion.rs`)
```rust
pub enum IssueSeverity { Blocking, Warning }
pub enum IssueSource   { Interpreter, Validator, Qualifier }
pub struct IngestionIssue {
    pub task_id: Option<TaskId>,     // None = graph-level (cycle / empty graph / interpret failure)
    pub severity: IssueSeverity,
    pub source: IssueSource,
    pub code: String,                // stable kebab, e.g. "empty-done-when"
    pub message: String,
    pub suggestion: Option<String>,
}
pub struct IngestionReport { pub issues: Vec<IngestionIssue> }
// impl: is_blocked() = any Blocking; blocking(); warnings()
```
`IngestionReport` derives `Serialize`/`Deserialize` (mirrors `RunMetadata`,
plan 0003) so it can ride on `RunView` and, optionally, `run.json`.

### Threading (`orchestrator.rs`)
`open_run` (`orchestrator.rs:~560`) computes the report right after it obtains a
graph — both on the fresh interpret path (`:~672-674`) and the artifact path
(`:~567-604`) — and stores it on `RunEntry` (`orchestrator.rs`, the struct plan
0003 already extended with `run_uid` / `run_slug` / `started_at`). On a **hard
interpret failure** (today `orchestrator.rs:~675-677` returns
`ApiError::InvalidCommand` and drops the run), instead register a Pending run
with a **zero-task graph + one `Blocking` `interpreter-failed` issue**, so the
re-interpret affordance can recover a transient model failure in place.
`build_view` (`orchestrator.rs`, the `RunView` projector plan 0003 extended with
`project`) gains the report so the TUI renders it with no new query; add
`pub report: IngestionReport` to `RunView` (`api.rs`, struct `RunView`) and set
it at every `RunView { .. }` literal — the same cross-workspace ripple
`runview-project-field` handled in plan 0003.

### `StartRun` guard (`orchestrator.rs`)
`start_run` refuses while `report.is_blocked()`, returning a clear
`ApiError::InvalidCommand { reason }` naming the blocking codes; otherwise it
proceeds exactly as today. (No new `RunStatus`: the run stays `Pending`; the gate
is the refusal + the rendered report.)

### `ReinterpretRun` command (`api.rs`, `orchestrator.rs`)
Add `Command::ReinterpretRun(RunId)` (mirror `Command::CancelRun`/`PauseRun` in
the `Api` command enum and its `orchestrator` handler). It re-reads the source
file, re-runs the interpreter **bypassing** the persisted artifact, recomputes
the report, replaces the graph + report on the `RunEntry`, overwrites
`.makina/tasks/{slug}.json`, and emits an event so the TUI refreshes. Only legal
while the run is `Pending` (not yet started).

### TUI (`crates/makina/src/{app.rs,event.rs,ui.rs}`)
- **Issues panel** — render `selected_run().report` as a list grouped by
  severity, each line `[source] code — message` coloured by severity (mirror the
  plan-0003 error-pane render + `task_state_badge` colour pattern in `ui.rs`).
- **Blocked-start indicator** — when `report.is_blocked()`, show a status-bar
  notice that `StartRun` is gated and list the blocking count.
- **Keys** — a re-interpret key (e.g. `r`/`R`, currently unmapped — verify in
  `translate_key`, `event.rs`) → `Command::ReinterpretRun`; the existing start
  key is unchanged but now no-ops with a status message when blocked.

---

## Testing strategy

Everything is driven by the deterministic `NoopBackend`
(`crates/makina-core/src/backend/noop.rs`, production-available, already used by
the `ModelInterpreter` unit tests) — **no real model required**:
- `ingestion.rs` unit tests: one fixture `TaskGraph` per issue `code` →
  `validate`/`qualify` return exactly the expected issues (and dedupe overlaps).
- Orchestrator integration (mirror `tests/orchestrator_read_path.rs` + the
  plan-0003 `tests/run_metadata.rs`): `OpenRun` with a `NoopBackend` returning
  canned JSON containing a non-actionable task → assert the Pending `RunView`
  carries a `Blocking` report and `StartRun` is refused; `ReinterpretRun` with
  clean canned JSON → report clears and `StartRun` proceeds.
- `build_planner_interpreter` wiring: `Some(backend)` → model path, `None` →
  deterministic (extend the existing factory tests).
- TUI render tests (`ratatui::TestBackend`, mirror plan-0003 `ui.rs` tests): the
  issues panel renders codes/messages; the blocked-start indicator appears.
- Offline: `None` backend → deterministic parse; a malformed `TASKS.md` →
  multi-error report.

## Decisions & open questions

Locked (see SCOPE for rationale): model produces JSON directly (no normalized-
Markdown intermediate); model is the default interpreter with deterministic
offline fallback; Qualifier blocks at ingestion with **no** new FSM states; the
gate reuses `Pending → StartRun` (no `AwaitingReview` status); one
`IngestionIssue`/`IngestionReport` type for all three sources; the Qualifier is
deterministic.

- **Interpret-failure handling (decided):** a hard interpret failure registers a
  recoverable Pending run carrying a `Blocking` `interpreter-failed` issue (so
  re-interpret works), rather than the current hard `OpenRun` error that drops
  the run.
- **No "Interpreting…" status (decided):** `OpenRun` awaits the model and the run
  appears in `Pending` when it returns. A progress/`Interpreting` status is a
  FUTURE polish, not in this plan.
- **Qualifier thresholds (open, tune at review):** the minimum `done_when` /
  description lengths and the imperative-verb check are heuristics — pinned as
  named constants and reviewable; expect to calibrate them against the real
  plan-0003 `TASKS.md` so a known-good list passes cleanly.
- **`run.json` enrichment (open):** whether to also write the `IngestionReport`
  into `run.json` (plan 0003's `RunMetadata`) is left as a small optional follow;
  the gate itself only needs the in-memory report on `RunView`.
