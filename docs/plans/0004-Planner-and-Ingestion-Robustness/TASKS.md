# Makina Plan 0004 — Planner & Ingestion Robustness

Structured-text task list for the third post-MVP plan. Makes the model the
default task-list interpreter in the TUI (deterministic parser as the offline
fallback), adds a deterministic structural **validator** and semantic
**Qualifier** over the interpreted graph, and puts a **review & approval gate**
in front of every run — surfacing all issues and refusing `StartRun` until the
graph is clean. See [SCOPE.md](SCOPE.md) for what is in and out, and
[ARCHITECTURE.md](ARCHITECTURE.md) for the design and file-level seams.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch
  `task/{id}` and worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only; the Planner adds
  further dependency edges automatically for tasks that touch the same files or
  areas.
- **Done when** is the verifiable acceptance check used by gates and the
  Reviewer. Every task must also keep `cargo test`, `cargo clippy --all-targets
  -- -D warnings`, and `cargo fmt --check` green.
- Line numbers below are grounded against `develop @ b432c18` and are hints
  only; locate every site by the named symbol (grep), since earlier tasks shift
  lines.

---

## 0017 — Model-Backed Ingestion

### ingest-compose-helper — Add a testable interpreter-composition helper
Add a single composition helper so the TUI's interpreter choice is unit-testable
instead of hand-wired in `main.rs`. In `crates/makina-core/src/interpreter.rs`,
beside `build_planner_interpreter` (`interpreter.rs:822`), add
`pub fn build_ingestion_interpreter(mechanism: &crate::config::PlannerMechanism,
backend: Option<std::sync::Arc<dyn crate::backend::AgentBackend>>) ->
Result<std::sync::Arc<dyn TaskListInterpreter>, InterpretError>` whose body is
`Ok(std::sync::Arc::new(crate::dependency::EdgeInferrer::new(build_planner_interpreter(mechanism,
backend)?)))`. This is exactly the decorator the TUI hand-builds today
(`crates/makina/src/main.rs:112-114`, `EdgeInferrer::new(StructuredTextInterpreter::new())`),
but routed through the factory so `OneShotAgent + Some(backend)` yields the model
path, `OneShotAgent + None` yields the deterministic path, and `DirectApi`
propagates `InterpretError::MechanismNotSupported`. Do **not** change
`build_planner_interpreter` itself. Add a rustdoc `# Example`-free doc-comment
describing the three cases (mirror the table on `build_planner_interpreter`,
`interpreter.rs:796-798`).
- **Depends on:** —
- **Done when:** three `#[tokio::test]`s in `interpreter.rs` `mod tests`
  (alongside `build_planner_interpreter_*`, `interpreter.rs:1327-1368`):
  `build_ingestion_interpreter_model_path_interprets_canned_json` — pass
  `&PlannerMechanism::OneShotAgent` + `Some(Arc::new(NoopBackend::...))` whose
  canned response is a valid one-task `{"slug":...,"tasks":[...]}` JSON, call
  `.interpret("s", "ignored")`, and assert the returned graph has that task;
  `build_ingestion_interpreter_offline_path_parses_structured_text` — pass
  `OneShotAgent` + `None`, `.interpret` a 1-task convention snippet, assert it
  parses; `build_ingestion_interpreter_direct_api_errors` — pass
  `&PlannerMechanism::DirectApi` + any backend, assert
  `Err(InterpretError::MechanismNotSupported { .. })`. Use the `NoopBackend`
  construction already used by the existing `ModelInterpreter` tests in this
  file. `cargo test -p makina-core build_ingestion_interpreter`, clippy, and fmt
  pass.

### ingest-wire-main — Make the model the default TUI interpreter
In `crates/makina/src/main.rs`, replace the hard-coded interpreter construction
(`main.rs:112-114`, currently `let interpreter = Arc::new(EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new())));`)
with a call to `makina_core::interpreter::build_ingestion_interpreter(&config.planner.mechanism,
Some(Arc::clone(&backend)))` (the `backend: Arc<dyn AgentBackend>` already built
at `main.rs:94-100`; `config.planner.mechanism` already exists,
`config.rs:124`/`config.rs:365`). On `Err(e)` — only reachable today via
`DirectApi` — print a one-line notice with `eprintln!` (this site runs **before**
`Tui::init()`, so `eprintln!` is the correct frame-bypass-exempt channel, per
plan-0003 `tui-error-pane-no-frame-bypass`) and fall back to the deterministic
interpreter: `Arc::new(EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new())))
as Arc<dyn makina_core::api::TaskListInterpreter>`. Keep passing the resulting
`interpreter` to `CoreApi::with_audit_registry(...)` unchanged (`main.rs:115`).
Update the seam doc-comment (`main.rs:103-111`) to state that the **model**
interpreter is now the default (deterministic = offline/`None`-backend fallback),
removing the stale "swap … for a `ModelInterpreter` … (task 33)" note. The two
imports `EdgeInferrer` / `StructuredTextInterpreter` (`main.rs:28-29`) stay (used
by the fallback).
- **Depends on:** ingest-compose-helper
- **Done when:** `cargo build -p makina` succeeds; `grep -n
  "build_ingestion_interpreter" crates/makina/src/main.rs` matches and `grep -n
  "EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new()))"
  crates/makina/src/main.rs` matches **only** inside the `Err` fallback arm (the
  unconditional hard-wire is gone); `cargo test -p makina`, clippy
  (`--all-targets -- -D warnings`), and fmt pass.

---

## 0018 — Ingestion Validator / Linter

### ingest-types — Add the `ingestion` module with the issue/report types
Create `crates/makina-core/src/ingestion.rs` and register `pub mod ingestion;` in
`crates/makina-core/src/lib.rs` between `pub mod governance;` and `pub mod
interpreter;` (keep alphabetical order). Define, with
`#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]` and
`#[serde(rename_all = "snake_case")]` where it carries an enum:
`pub enum IssueSeverity { Blocking, Warning }`;
`pub enum IssueSource { Interpreter, Validator, Qualifier }`;
`pub struct IngestionIssue { pub task_id: Option<crate::task::TaskId>, pub
severity: IssueSeverity, pub source: IssueSource, pub code: String, pub message:
String, pub suggestion: Option<String> }`;
`pub struct IngestionReport { pub issues: Vec<IngestionIssue> }`. Add an
`impl IngestionReport` with `pub fn is_blocked(&self) -> bool` (any issue whose
`severity == IssueSeverity::Blocking`), `pub fn blocking(&self) -> impl
Iterator<Item = &IngestionIssue>`, `pub fn warnings(&self) -> impl Iterator<Item
= &IngestionIssue>`, and `pub fn is_empty(&self) -> bool`. Add
`impl Default for IngestionReport` returning an empty `issues` vec (used by
`RunView` literals). `serde` and `serde_json` are already deps; `TaskId` lives at
`crate::task::TaskId`.
- **Depends on:** —
- **Done when:** a `#[cfg(test)] mod tests` in `ingestion.rs` has
  `is_blocked_is_true_iff_a_blocking_issue_present` (empty report → `false`; one
  `Warning` → `false`; one `Blocking` → `true`) and
  `blocking_and_warnings_partition_issues` (a report with one `Blocking` + one
  `Warning` → `blocking().count() == 1` and `warnings().count() == 1`).
  `cargo test -p makina-core ingestion`, clippy, and fmt pass.

### ingest-validate — Structural validator that collects all issues
In `crates/makina-core/src/ingestion.rs`, add `pub fn validate(graph:
&crate::task::TaskGraph) -> Vec<IngestionIssue>` (`source = IssueSource::Validator`,
all `IssueSeverity::Blocking`) that scans the **whole** graph and returns **every**
structural problem (not fail-fast like `TaskGraph::validate`). Emit one issue per
finding with a stable kebab `code`, the offending `task_id` where applicable, and
a `suggestion`: (1) `code = "dangling-dependency"` — a `depends_on` id that names
no task in the graph (`task_id = Some(the task)`); (2) `code = "duplicate-task-id"`
— two tasks share an id (`task_id = Some(id)`, emitted once per duplicate id);
(3) `code = "empty-done-when"` — `task.done_when` is empty or whitespace-only;
(4) `code = "self-dependency"` — a task lists its own id in `depends_on`;
(5) `code = "dependency-cycle"` — the graph contains a cycle (`task_id = None`,
graph-level). For cycle detection reuse the reachability primitive already in
`crates/makina-core/src/dependency.rs` (`transitive_depends_on` /
`infer_edges`’ guard); do not write a new graph traversal if an existing helper
suffices. This is **additive** — it does not replace `TaskGraph::validate`
(`task.rs`), which the interpreters still call to build a graph at all.
- **Depends on:** ingest-types
- **Done when:** unit tests in `ingestion.rs` `mod tests`, one per code, each
  building a small `TaskGraph` fixture exhibiting exactly that defect and
  asserting `validate(&g)` contains an issue with that `code` and the right
  `task_id`: `validate_flags_dangling_dependency`,
  `validate_flags_duplicate_task_id`, `validate_flags_empty_done_when`,
  `validate_flags_self_dependency`, `validate_flags_dependency_cycle`; plus
  `validate_clean_graph_has_no_issues` (a well-formed 2-task graph → empty vec).
  Build fixtures with the same `Task`/`TaskGraph` constructors the existing
  `dependency.rs` tests use. `cargo test -p makina-core ingestion::tests::validate`,
  clippy, and fmt pass.

### ingest-lint-source — Multi-error convention lint for the offline parse path
Add `pub fn lint_source(source_text: &str) -> Vec<IngestionIssue>` to
`crates/makina-core/src/ingestion.rs` (`source = IssueSource::Interpreter`,
`IssueSeverity::Blocking`, `task_id = None`) that scans a raw task-list document
line-by-line and reports **all** convention violations at once, rather than the
single `InterpretError::ParseError` the deterministic
`parse_structured_text` stops at (`interpreter.rs:247-506`). Detect, with a
stable `code` and the 1-based line number in the `message`: (1)
`code = "task-before-section"` — a `### id — title` heading appearing before any
`## NNNN — …` section heading; (2) `code = "heading-missing-em-dash"` — a `## ` or
`### ` heading lacking the ` — ` separator; (3) `code = "task-missing-depends-on"`
and (4) `code = "task-missing-done-when"` — a `### ` task block with no
`- **Depends on:**` / `- **Done when:**` line before the next heading. Keep it a
**pure line scanner** (no graph build, no I/O); it mirrors the failure cases
`parse_structured_text` already detects but collects them. This is surfaced only
on the deterministic offline path (wired in `ingest-report-on-run`).
- **Depends on:** ingest-types
- **Done when:** unit tests in `ingestion.rs` `mod tests`:
  `lint_source_reports_all_problems_at_once` — a fixture document with a
  dash-less heading AND a task missing `Done when` yields **two** issues with the
  two expected codes (proving it does not stop at the first); a per-code test
  for each of the four codes; and `lint_source_clean_document_has_no_issues`
  (the worked example from `docs/spec/structured-text-convention.md` §7 → empty
  vec). `cargo test -p makina-core ingestion::tests::lint_source`, clippy, and
  fmt pass.

---

## 0019 — Qualifier Entry-Gate

### ingest-qualify — Deterministic actionability checks over the graph
In `crates/makina-core/src/ingestion.rs`, add `pub fn qualify(graph:
&crate::task::TaskGraph) -> Vec<IngestionIssue>` (`source = IssueSource::Qualifier`,
`IssueSeverity::Blocking`) of deterministic, string/structural heuristics over
each `Task` — no model call. Define named threshold constants at module top so
they are tunable and test-pinned: `const MIN_DONE_WHEN_LEN: usize = 12;`,
`const MIN_DESCRIPTION_LEN: usize = 12;`, and `const PLACEHOLDER_MARKERS: &[&str]
= &["tbd", "todo", "???", "fixme", "xxx", "fill in", "to be defined"];`. Emit one
issue per failing task with a stable `code`, `task_id = Some(task.id)`, and a
`suggestion`: (1) `code = "vague-done-when"` — `task.done_when` (trimmed) is
shorter than `MIN_DONE_WHEN_LEN`; (2) `code = "placeholder-text"` — the
lowercased `title`, `description`, or `done_when` contains any
`PLACEHOLDER_MARKERS` entry as a whole-word/substring match; (3) `code =
"non-actionable-title"` — `task.title` (trimmed, lowercased, first word) is not
an imperative-style verb: implement this as "the first word is **not** in a small
curated stop-list of non-actionable openers" (e.g. `["the", "a", "an", "some",
"stuff", "things", "misc"]`) **and** the title has at least two words — keep the
heuristic permissive (favour false-negatives over false-positives so real plans
pass); (4) `code = "thin-description"` — `task.description` (trimmed) is shorter
than `MIN_DESCRIPTION_LEN`. Dedupe against the validator by code: do **not**
re-emit dangling/resolvable-dependency issues here (that is
`ingest-validate`’s `dangling-dependency`). The function is pure and never
mutates the graph.
- **Depends on:** ingest-types
- **Done when:** unit tests in `ingestion.rs` `mod tests`, one per code
  (`qualify_flags_vague_done_when`, `qualify_flags_placeholder_text`,
  `qualify_flags_non_actionable_title`, `qualify_flags_thin_description`), each
  with a minimal `TaskGraph` fixture; plus a **calibration** test
  `qualify_accepts_a_known_good_plan` that builds a 2–3 task graph whose tasks
  resemble real plan-0003 tasks (substantive imperative titles, full
  descriptions, concrete `done_when`) and asserts `qualify(&g)` is **empty** — so
  the thresholds do not reject good input. `cargo test -p makina-core
  ingestion::tests::qualify`, clippy, and fmt pass.

---

## 0020 — Review & Approval Gate

### ingest-report-on-run — Compute and thread the report onto every run
Compute an `IngestionReport` at open time and carry it on the run so the TUI can
render it. In `crates/makina-core/src/orchestrator.rs`: (1) add `report:
crate::ingestion::IngestionReport` to `RunEntry` (`orchestrator.rs:237`); (2) in
`open_run` (`orchestrator.rs:560`), after the `let graph = match … { … };` block
resolves a graph (`:604`, both the artifact and fresh paths converge there) and
**before** the registry insert (`:617-629`), compute `let report = {
let mut issues = crate::ingestion::validate(&graph); issues.extend(crate::ingestion::qualify(&graph));
crate::ingestion::IngestionReport { issues } };` and set it on the new `RunEntry`;
(3) add `report: crate::ingestion::IngestionReport` to `RunView` (`api.rs:227`,
after `tasks`) and to `build_view` (`orchestrator.rs:276`) as a parameter, set on
the returned `RunView`; (4) pass `entry.report.clone()` at every `build_view`
call site (the `run`/`runs` query methods — the compiler lists them). Adding the
`RunView` field breaks every `RunView { .. }` literal in the workspace (api.rs,
orchestrator.rs, and the `makina` crate `app.rs`/`ui.rs` tests) — set
`report: IngestionReport::default()` on each, exactly the cross-workspace ripple
`runview-project-field` handled in plan 0003. Re-export the ingestion types from
`api.rs` (`pub use crate::ingestion::{IngestionIssue, IngestionReport,
IssueSeverity, IssueSource};`) so the TUI imports them from `makina_core::api`
like the other view types.
- **Depends on:** ingest-validate, ingest-qualify
- **Done when:** a `#[tokio::test] async fn open_run_attaches_ingestion_report()`
  in `orchestrator.rs` `mod tests` (modelled on
  `open_run_interprets_file_and_creates_run`, `orchestrator.rs:1227`, using the
  `execution_core_api()`-style helper + a `NoopBackend` whose canned JSON
  contains a task with an empty `done_when`) `OpenRun`s, snapshots `api.runs()`,
  and asserts the run's `RunView.report` contains a `Blocking` issue with code
  `"empty-done-when"` (or `"vague-done-when"`); and that a clean canned graph
  yields `report.is_blocked() == false`. `cargo test -p makina-core open_run`,
  the broad `cargo test -p makina-core` (RunView literals updated), clippy, and
  fmt pass.

### ingest-interpret-failure-recoverable — Turn an interpret failure into a reviewable run
Make a model/parse interpretation failure produce a **reviewable Pending run**
(carrying a blocking issue) instead of dropping the run with a hard `OpenRun`
error — so a transient model failure is recoverable via re-interpret. In
`crates/makina-core/src/orchestrator.rs`, change `interpret_and_seed`
(`orchestrator.rs:653`) so a **read** failure still returns
`Err(ApiError::InvalidCommand)` (a bad path is not reviewable), but an
**interpret** failure (the `.interpret(...).map_err(...)?` at `:670-677`) instead
returns `Ok` of an empty graph plus a carried issue. Concretely change its return
type to `Result<(TaskGraph, Option<crate::ingestion::IngestionIssue>), ApiError>`:
on interpret `Err(e)`, build `TaskGraph { slug: slug.into(), tasks: vec![] }`
(use the graph constructor the codebase uses) and `Some(IngestionIssue { task_id:
None, severity: IssueSeverity::Blocking, source: IssueSource::Interpreter, code:
"interpreter-failed".into(), message: format!("could not interpret task list
`{slug}`: {e}"), suggestion: Some("fix the task list and re-interpret".into()) })`,
skipping the seed-persist (do not persist an empty graph). Update `open_run`’s two
fresh-path call sites (`:584-585`, `:591-592`, `:601-602`) to bind the tuple, and
fold the carried `Option` into the computed `report` from `ingest-report-on-run`.
Update the two existing tests that assumed a hard error: keep
`open_run_with_missing_file_returns_error` (`orchestrator.rs:1525`) expecting
`Err` (read failure), but change `open_run_with_invalid_content_returns_error`
(`orchestrator.rs:1546`) to instead assert `OpenRun` returns `Ok(RunOpened)` and
the run’s `RunView.report` carries a `Blocking` `"interpreter-failed"` issue (and
rename it `open_run_with_invalid_content_is_reviewable`).
- **Depends on:** ingest-report-on-run
- **Done when:** `open_run_with_invalid_content_is_reviewable` asserts a Pending
  run with a blocking `interpreter-failed` report (no `Err`);
  `open_run_with_missing_file_returns_error` still asserts `Err`;
  `cargo test -p makina-core open_run`, clippy, and fmt pass.

### ingest-startrun-guard — Refuse `StartRun` while the report is blocked
In `crates/makina-core/src/orchestrator.rs`, at the top of `start_run`
(`orchestrator.rs:704`), inside the registry lock after looking up the `entry`
(`:713`) and **before** mutating any handle/status, check
`if entry.report.is_blocked() { return Err(ApiError::InvalidCommand { reason:
format!("cannot start: {} blocking ingestion issue(s) — {}", n, codes) }); }`
where `n` is the blocking count and `codes` is a comma-joined list of the
blocking issues’ `code`s (drop the guard early by returning before any
mutation — do not leave `status` changed). A clean run (`!is_blocked()`) proceeds
exactly as today. Leave `PauseRun`/`CancelRun` unaffected.
- **Depends on:** ingest-report-on-run
- **Done when:** a `#[tokio::test] async fn start_run_refused_while_report_blocked()`
  in `orchestrator.rs` `mod tests` opens a run whose `NoopBackend` canned JSON has
  a non-actionable task (blocking report), asserts `execute(Command::StartRun {
  run })` returns `Err(ApiError::InvalidCommand { .. })` and the run’s status is
  still `Pending`; and a sibling `start_run_proceeds_when_report_clean` opens a
  clean run and asserts `StartRun` returns `Ok` and the status becomes `Running`.
  `cargo test -p makina-core start_run`, clippy, and fmt pass.

### ingest-reinterpret-command — Add the `ReinterpretRun` command
Add a command that re-interprets a Pending run from its source file, bypassing the
persisted artifact. In `crates/makina-core/src/api.rs`: add `ReinterpretRun { run:
RunId }` to `Command` (`api.rs:270`, mirror `CancelRun`’s shape + doc-comment).
In `crates/makina-core/src/orchestrator.rs`: add the dispatch arm to `execute`
(`orchestrator.rs:914-922`) — `Command::ReinterpretRun { run } =>
self.reinterpret_run(run).await` — and implement `async fn reinterpret_run(&self,
run: RunId) -> Result<CommandOutcome, ApiError>`: look up the entry’s
`task_list_path` + `run_slug` (reject with `ApiError::UnknownRun` if absent, and
`ApiError::InvalidCommand` if the run’s `status != RunStatus::Pending`); call
`self.interpret_and_seed(&slug, &path, repo_root).await?` (the
`ingest-interpret-failure-recoverable` tuple form) to get a fresh graph + carried
issue **bypassing** `load_graph`; recompute the report
(`validate` + `qualify` + carried issue); then under the registry lock replace the
entry’s `graph` (`Arc::new(AsyncMutex::new(new_graph))`) and `report`; emit an
event so the TUI refreshes (reuse an existing event such as `Event::RunOpened` or
add a minimal `Event::RunReinterpreted { run }` next to it — pick the smaller
change and update its match sites); return `CommandOutcome::Acknowledged`.
- **Depends on:** ingest-interpret-failure-recoverable, ingest-startrun-guard
- **Done when:** a `#[tokio::test] async fn reinterpret_clears_block_and_allows_start()`
  in `orchestrator.rs` `mod tests` opens a run with a backend whose **first**
  canned response is a blocking (non-actionable) graph and whose **second** is a
  clean graph (use a `NoopBackend` that cycles responses), asserts the initial
  `StartRun` is refused, issues `Command::ReinterpretRun { run }`, then asserts
  the run’s `RunView.report.is_blocked() == false` and a subsequent `StartRun`
  returns `Ok`; plus `reinterpret_rejected_when_not_pending` (a `Running` run →
  `Err(InvalidCommand)`). `cargo test -p makina-core reinterpret`, clippy, and
  fmt pass.

### ingest-tui-report-panel — Render the report and gate the start key in the TUI
Surface the report in the TUI and wire the re-interpret key. In
`crates/makina/src/ui.rs`, add `render_ingestion_panel(app: &App, frame: &mut
Frame, area: Rect)` that, when `app.selected_run()` has a non-empty `report`,
renders each `IngestionIssue` as a line `[{source}] {code} — {message}` coloured
by severity (`IssueSeverity::Blocking => Color::Red`, `Warning => Color::Yellow`),
mirroring the plan-0003 error-pane render (`render_error_pane`) and the
`task_state_badge` colour pattern; place it on a layout chunk near the task
detail (mirror an existing `render_*_pane` call site). When
`report.is_blocked()`, append a blocked-start notice to the status bar (mirror the
plan-0003 `tui-gr-legend` status-bar append): e.g. `"  │  ⚠ {n} blocking issue(s)
— press r to re-interpret"`. In `crates/makina/src/app.rs` add `AppEvent::Reinterpret`
(near the other run-control variants) and an `App::update` arm that returns an
intent the event loop maps to `Command::ReinterpretRun { run: selected }` —
mirror exactly how an existing run-control key (e.g. the start key) turns an
`AppEvent` into an `Api` command in `crates/makina/src/event.rs`. Map `KeyCode::Char('r')
| KeyCode::Char('R')` to `AppEvent::Reinterpret` in `translate_key`’s normal-mode
match (`event.rs`; verify `r`/`R` is currently unmapped). `IngestionReport` /
`IngestionIssue` / `IssueSeverity` / `IssueSource` are imported from
`makina_core::api` (re-exported by `ingest-report-on-run`); `App.selected_run() ->
Option<&RunView>` already exists.
- **Depends on:** ingest-report-on-run, ingest-reinterpret-command
- **Done when:** two `ratatui::TestBackend` render tests in `ui.rs` `mod tests`
  (using the `make_terminal` + `screen_of` helpers): `render_ingestion_panel_shows_blocking_issue`
  builds an `App` whose selected `RunView.report` has a `Blocking` issue and
  asserts `screen_of` contains the issue `code` AND that some `cell.fg ==
  Color::Red` (scan `buf.content()`, mirroring `render_failed_badge_uses_red_fg`);
  `render_status_bar_shows_blocked_notice_when_report_blocked` asserts the status
  bar shows the blocking-count notice. Plus an `event.rs` test
  `r_key_translates_to_reinterpret` asserting `matches!(translate_terminal_event(key_press(KeyCode::Char('r'),
  KeyModifiers::NONE), false), AppEvent::Reinterpret)`. `cargo test -p makina`,
  clippy, and fmt pass.
