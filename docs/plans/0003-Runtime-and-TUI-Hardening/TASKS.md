# Makina Plan 0003 — Runtime & TUI Hardening

Structured-text task list for the second post-MVP plan, derived from the
plan-0002 dogfood findings. Unifies runtime state under `.makina/`, adds a
logging subsystem, makes the scheduler continue past a failed task, and
polishes the TUI. See [SCOPE.md](SCOPE.md) for what is in and out, and
[ARCHITECTURE.md](ARCHITECTURE.md) for the design and file-level seams.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch
  `task/{id}` and worktree `.makina/worktrees/{id}/`).
- **Depends on** lists *direct* structural prerequisites only; the Planner
  adds further dependency edges automatically for tasks that touch the same
  files or areas.
- **Done when** is the verifiable acceptance check used by gates and the
  Reviewer. Every task must also keep `cargo test`, `cargo clippy --all-targets
  -- -D warnings`, and `cargo fmt --check` green.
- Tasks are intentionally small and self-contained: each names the exact
  file(s) and function/type to touch and the approach, so it can be picked up
  without further context.

---

## 0012 — Unified `.makina/` Workspace & Run Identity

### mk-paths-module — Add the `.makina/` paths module
Create `crates/makina-core/src/paths.rs` with pure path-building helpers (no
I/O, no logic), each taking `repo_root: &Path` and returning a `PathBuf`:
`config_file` → `.makina/config.toml`, `task_graph(slug)` → `.makina/tasks/{slug}.json`,
`run_dir(run_id)` → `.makina/runs/{run_id}`, `audit_log(run_id)` →
`.makina/runs/{run_id}/audit.jsonl`, `task_log(run_id, task_slug)` →
`.makina/runs/{run_id}/logs/{task_slug}.log`, `worktree(task_id)` →
`.makina/worktrees/{task_id}`. Register `pub mod paths;` in `lib.rs`. Write a
rustdoc example per helper.
- **Depends on:** —
- **Done when:** unit tests assert each helper returns the exact expected path
  for a sample `repo_root` (e.g. `config_file` → `{repo}/.makina/config.toml`);
  `cargo test -p makina-core paths` passes.

### mk-run-id — Persistent ULID run identity
Add the `ulid` crate. Allocate a persistent, sortable run id (26-char ULID
string) when a Run is opened, in `CoreApi`'s id allocation (`orchestrator.rs`),
stored on the `RunEntry` and exposed read-only on `RunView` (e.g. `run_uid:
String`). Keep `RunId(u64)` as the in-memory session handle. Thread the ULID to
the Supervisor alongside the existing `run_slug` (the registration site that
already carries `run_slug`).
- **Depends on:** mk-paths-module
- **Done when:** an integration test opens two runs and asserts each `RunView`
  carries a distinct 26-char ULID and that the two ids sort chronologically;
  `RunId(u64)` still works as the handle; tests pass.

### mk-run-slug — Derive a collision-free, plan-scoped run slug
Change the run-slug derivation in `open_run` (`orchestrator.rs`) from
`task_list_path.file_stem()` to a plan-scoped slug that combines the task
list's **parent directory name** and its **file stem**, lowercased and
kebab-sanitized — e.g. `docs/plans/0003-Runtime-and-TUI-Hardening/TASKS.md` →
`0003-runtime-and-tui-hardening-tasks`. Build it as
`format!("{parent_dir_name}-{file_stem}").to_lowercase()`, then sanitize to a
valid kebab id (replace any non-`[a-z0-9]` run with a single `-`, collapse
repeats, trim leading/trailing `-`). Fall back to the lowercased stem alone
when there is no usable parent directory. This makes `.makina/tasks/{slug}.json`
(and the read-path's `load_graph(slug)`) **unique per plan**, so opening
different `TASKS.md` files no longer collide on the slug `TASKS` and shadow each
other's persisted graphs. (With unique slugs the existing read-path resumes only
from the matching plan's artifact.)
- **Depends on:** —
- **Done when:** a unit test asserts the derivation maps
  `…/0003-Runtime-and-TUI-Hardening/TASKS.md` → `0003-runtime-and-tui-hardening-tasks`,
  that two different plan directories each containing a `TASKS.md` yield
  **distinct** slugs, and that the result is a valid kebab slug; an integration
  test confirms opening a `TASKS.md` whose plan-scoped artifact does not exist
  interprets the `.md` (rather than loading an unrelated `.tasks/TASKS.json`).

### mk-config-path — Load project config from `.makina/config.toml` (+ legacy fallback)
In `config.rs` `load_defaults` (the `PathBuf::from("makina.toml")` site), build
the project-config path via `paths::config_file(repo_root)`. Add back-compat:
if `.makina/config.toml` is absent but a legacy `./makina.toml` exists, load the
legacy file and emit a single `tracing::warn!` deprecation note. Update the
load-path doc-comment.
- **Depends on:** mk-paths-module
- **Done when:** an integration test in a temp repo loads `.makina/config.toml`;
  a second temp repo with only `./makina.toml` still loads and the deprecation
  path is exercised by a test; `cargo test -p makina-core config` passes.

### mk-task-graph-path — Route persistence paths through the paths module
In `persist.rs`, change `tasks_path` and `temp_path` (and the `.tasks` dir
construction) to delegate to `paths::task_graph` / a temp path under
`.makina/tasks/`. Behavior is otherwise unchanged (atomic temp-then-rename).
- **Depends on:** mk-paths-module
- **Done when:** the existing persist unit tests pass against the new location
  (the artifact and temp file live under `.makina/tasks/`); `cargo test -p
  makina-core persist` and clippy pass.

### mk-worktree-path — Route worktree paths through the paths module
In `worktree.rs`, change `WorktreeManager::worktree_path` to delegate to
`paths::worktree(repo_root, task_id)` so worktrees live at
`.makina/worktrees/{task-id}/`. Signature unchanged.
- **Depends on:** mk-paths-module
- **Done when:** the worktree tests pass with worktrees created/removed under
  `.makina/worktrees/`; `cargo test -p makina-core worktree` passes.

### mk-audit-relocate — Relocate the audit ledger under the per-run dir
In `audit.rs`, change `JsonlAuditSink::record` to write
`.makina/runs/{run_id}/audit.jsonl` via `paths::audit_log`, keyed by the ULID
run id. Add `run_uid: String` to the registry's `AuditContext`; extend
`AuditRegistry::register` (and `JsonlAuditSink`) to accept it, and update the
Supervisor registration call site to pass the ULID (from `mk-run-id`).
- **Depends on:** mk-paths-module, mk-run-id
- **Done when:** an integration test that triggers a permission decision asserts
  the enriched line lands in `.makina/runs/{run_id}/audit.jsonl` (run/task ids
  populated) and re-running appends; `cargo test` passes workspace-wide.

### mk-gitignore — Commit/ignore split for `.makina/` + migrate the repo's own config
Add `.makina/.gitignore` ignoring `runs/` and `worktrees/`. Update the root
`.gitignore` (remove `/.worktrees/`; do not ignore `.makina/`). Move this repo's
own `makina.toml` → `.makina/config.toml`. Update the gitignore-invariant test
(from plan-0002) for the new layout.
- **Depends on:** mk-config-path, mk-worktree-path, mk-audit-relocate
- **Done when:** `git check-ignore` confirms `.makina/runs/x` and
  `.makina/worktrees/x` are ignored while `.makina/config.toml` and
  `.makina/tasks/x.json` are not; the invariant test asserts this; the repo's
  config now lives at `.makina/config.toml`.

### mk-doc-refs — Update docs/comment references to the new layout
Update the remaining documentation and code-comment references to the old paths
(`makina.toml`, `.tasks/`, `.worktrees/`) to the new `.makina/` layout —
`docs/spec/runtime-artifact-schema.md`, `docs/trial/e2e-run.md`, `README.md`, and
stray doc-comment examples. Documentation-only; group edits by file.
- **Depends on:** mk-gitignore
- **Done when:** `grep -rn` for the old path literals across `docs/` + crate
  doc-comments returns only intentional/historical mentions; `cargo doc` builds
  clean.

---

## 0013 — Logging & Diagnostics

### log-run-dir — Create the per-run log directory on StartRun
Add a helper (in `paths.rs` or a small runtime module) that creates
`.makina/runs/{run_id}/logs/` and returns it. Call it during `start_run`
(`orchestrator.rs`), before the background scheduler spawns, so the directory
exists for the file layer. Best-effort: `tracing::warn!` on failure, never abort
the run.
- **Depends on:** mk-paths-module, mk-run-id
- **Done when:** a unit test asserts the helper creates
  `.makina/runs/{run_id}/logs/` and is idempotent; `cargo test -p makina-core`
  passes.

### log-subscriber — Install a tracing subscriber with file + TUI layers
Add `tracing_subscriber` to the `makina` binary and initialize it in `main.rs`
(after the audit-sink setup) composing two layers via `SubscriberExt`/`SubscriberInitExt`:
(1) a **file layer** writing under `.makina/runs/{run_id}/logs/`, and (2) a
**TUI channel layer** that forwards records onto an `mpsc` channel for the
error/log pane (consumed in plan-0015). Existing `tracing::warn!`/`error!` calls
now reach both.
- **Depends on:** mk-run-id, mk-paths-module, log-run-dir
- **Done when:** a unit test confirms the two-layer subscriber composes without
  panicking and a `tracing::warn!` emitted during a run is written to a file
  under `.makina/runs/{run_id}/logs/` and offered to the channel; `cargo test -p
  makina` + clippy + fmt pass.

### log-per-task-files — Route each task's transcript to its log file
Tag the Supervisor's per-task work with a `tracing::Span` carrying the task slug
(at the dispatch / `develop_until_gates_pass` sites in `supervisor.rs`) so that
state transitions, gate output, and agent-exchange records route to
`.makina/runs/{run_id}/logs/{task_slug}.log` via the file layer's span-aware
routing.
- **Depends on:** log-subscriber
- **Done when:** an integration test drives a task and asserts its
  `.makina/runs/{run_id}/logs/{task_slug}.log` exists and contains the task's
  state transitions (and gate output where applicable); tests pass.

### log-run-metadata — Write run.json per run
Add a `RunMetadata` type (run id, slug, status, started/ended timestamps,
task→worktree map) and a writer that serializes it to
`.makina/runs/{run_id}/run.json`. Call the writer when a run reaches a terminal
status (the orchestrator's run-status finalization). Best-effort (warn on
failure, never abort).
- **Depends on:** mk-paths-module, mk-run-id, log-run-dir
- **Done when:** an integration test opens + drives a run to terminal and
  asserts `.makina/runs/{run_id}/run.json` exists with the run id, slug, a
  terminal status, and timestamps; tests pass.

### audit-async-write — Offload the audit sink's file I/O to a background writer
Refactor `JsonlAuditSink` so the sync `record` no longer does blocking
`std::fs` on the caller's (ACP reader) thread: spawn one background writer task
on construction and have `record` enqueue the serialized line onto a bounded
`tokio::sync::mpsc` channel (drop + `tracing::warn!` if full). The writer task
appends to the per-run audit file. Keep the `AuditSink` trait sync + unchanged.
- **Depends on:** mk-audit-relocate
- **Done when:** a test confirms `record` enqueues without blocking (e.g. many
  rapid calls return promptly) and the writer flushes all entries to the ledger
  in order; tests pass.

### audit-registry-evict — Evict registry entries when a run completes
Add `evict_run(&self, run_id: &str)` to `AuditRegistry` and implement it in
`JsonlAuditSink` (remove that run's entries from the registry map). Call it from
the orchestrator after a run reaches terminal state (after `run.json` is
written), so the registry doesn't grow unbounded in a long-lived process.
- **Depends on:** mk-audit-relocate, log-run-metadata
- **Done when:** a test registers entries for a run, completes the run, and
  asserts the registry no longer retains that run's entries while other runs are
  unaffected; tests pass.

---

## 0014 — Scheduler Robustness

### fsm-skipped-state — Add the `Skipped` terminal state + `DependencyFailed` event
Add `TaskState::Skipped` (terminal) in `task.rs` and `TaskEvent::DependencyFailed`
in `state_machine.rs`; add the four transitions `(New|Ready|InProgress|InReview,
DependencyFailed) → Skipped` to `transition()`; include `Skipped` in
`is_terminal()`; add `DependencyFailed` to `legal_events()` for the four
non-terminal states. Keep the FSM total (illegal from all other states). Mirror
the plan-0002 `MergeConflict` addition.
- **Depends on:** —
- **Done when:** unit tests assert each of the four `→ Skipped` transitions,
  that `DependencyFailed` is rejected from `Done/Failed/Skipped`, and that
  `Skipped` is terminal; `cargo test -p makina-core state_machine` passes.

### fsm-skipped-tests — Re-prove FSM totality with the new state/event
Extend the exhaustive transition-table / totality test in `state_machine.rs`
(the one that enumerates every (state, event) pair) to include `Skipped` (7
states) and `DependencyFailed` (12 events), updating the legal/illegal counts
(the 4 new legal edges). 
- **Depends on:** fsm-skipped-state
- **Done when:** the exhaustive test passes with updated counts (7 states × 12
  events; original 15 legal + 4 new `DependencyFailed` edges), proving the FSM
  is still total.

### sched-skip-dependents — Mark a failed task's transitive dependents `Skipped`
Add `mark_dependents_skipped(graph, failed_task_id)` in `supervisor.rs`: a BFS
over `depends_on` edges that applies `DependencyFailed` to every task
transitively depending on the failed task (holding the graph lock). Call it
whenever a task reaches a non-`Done` terminal (the driver-outcome handling for
`Failed`, including the hard-error and wall-clock-cap paths).
- **Depends on:** fsm-skipped-state
- **Done when:** an integration test where A fails and B,C depend on A (and
  D depends on B) asserts B, C, and D all reach `Skipped` after A fails; the
  graph records the skip; tests pass.

### sched-continue-on-failure — Keep launching independents after a task fails
Change the scheduler's failure handling so a task failure — including the
hard-error driver-`Err` path that currently sets `stop_launching = true` — does
**not** stop launching independent ready tasks; the fill loop continues after
draining. Only a genuine driver **panic** stays fatal (`stop_launching`).
- **Depends on:** sched-skip-dependents
- **Done when:** an integration test with three independent ready tasks where
  one fails asserts the other two still reach `Done` and the run is not halted;
  a panic still aborts the run; tests pass.

### sched-run-status-failed — Report a `Failed` run status without halting
Make the scheduler's result/`RunReport` reflect that the run is `Failed` if
**any** task ended `Failed` (after all independent work completed) — non-
catastrophic. Record the failed task ids + reasons (add a `failed_tasks` field
to `RunReport`).
- **Depends on:** sched-continue-on-failure
- **Done when:** the continue-on-failure integration test asserts the run ends
  `Failed` with the failed task id/reason recorded in `RunReport`, while
  completed and skipped tasks are also reflected; tests pass.

### sched-parallelism-instrument — Record driver start/end for overlap detection
Instrument `task_driver` to record (via an `api::Event` or a test hook) each
driver's start and end timestamp with its task id, so overlap is observable.
- **Depends on:** —
- **Done when:** a test can read the start/end timestamps for each dispatched
  driver; `cargo test -p makina-core` passes.

### sched-parallelism-verify — Verify overlap under concurrency=2 and root-cause
Add an integration test running two independent ready tasks with a slow gate at
`concurrency = 2`, asserting the two drivers' intervals overlap (proving real
parallelism). If they do not overlap, root-cause the sequential appearance
(prime suspects: shared `cargo` target dir across worktrees; gate
serialization) and apply the fix (e.g. isolated target dirs) or document the
accepted limitation in the architecture notes.
- **Depends on:** sched-parallelism-instrument
- **Done when:** the test demonstrates ≥2 drivers overlapping under
  `concurrency = 2`; the root cause + fix (or accepted limitation) is recorded.

---

## 0015 — TUI Presentation & Views

### tui-error-pane-state — Error-pane state on `App`
In `app.rs`, add `error_pane_open: bool` and a bounded `error_messages` buffer
(cap ~50, oldest evicted) of a small `ErrorMessage { timestamp, level, text }`
type. Pure state — no rendering or events yet.
- **Depends on:** —
- **Done when:** a unit test asserts pushing past the cap evicts the oldest
  message; `cargo test -p makina` passes.

### tui-error-pane-toggle — Toggle key + event
Add `AppEvent::ToggleErrorPane`; map a key (e.g. `e`) to it in `event.rs`
`translate_key`; add an `App::update` arm that flips `error_pane_open` and
requests a redraw.
- **Depends on:** tui-error-pane-state
- **Done when:** a unit test asserts the toggle flips the flag, and the key
  translation test maps the key to `ToggleErrorPane`; tests pass.

### tui-error-pane-render — Render the collapsible pane + error badge
In `ui.rs`, add `render_error_pane`: when `error_pane_open`, show the recent
messages (colored by level) in a few rows below the exchange pane; when closed
but messages exist, show an error-count badge in the panel title. Adjust the
layout split.
- **Depends on:** tui-error-pane-toggle
- **Done when:** a `ratatui::TestBackend` render test asserts the pane shows
  messages when open and the badge when collapsed-with-errors; tests pass.

### tui-error-pane-wire — Feed tracing records into the pane
Consume the TUI channel from plan-0013's `log-subscriber`: in `event.rs`'s loop,
drain the channel (in the `tokio::select!`) and deliver records as
`AppEvent::ErrorMessageArrived { msg }`, whose `update` arm appends to
`error_messages`. System errors now surface in the pane instead of via
`eprintln!`; remove/redirect the `main.rs` `eprintln!` error sites accordingly.
- **Depends on:** log-subscriber, tui-error-pane-state
- **Done when:** an integration test feeds a record through a mock channel and
  asserts it appears in `error_messages` (and renders when the pane is open);
  no system error bypasses the frame; tests pass.

### tui-ansi-parser — ANSI SGR → ratatui style parser
Add `crates/makina/src/ansi.rs` with `parse_ansi(input: &str) -> Vec<AnsiSpan>`
(`AnsiSpan { text, style }`) that converts SGR sequences (colors, bold, reset)
to `ratatui::style::Style` and strips non-SGR control codes. (A minimal existing
crate is acceptable if cleaner.)
- **Depends on:** —
- **Done when:** unit tests assert green/red/bold/reset sequences produce the
  right styles and that cursor/other control codes are stripped (no literal
  escapes remain); clippy clean on the new module.

### tui-diff-coloring — Unified-diff line coloring
Add a helper that detects unified-diff line prefixes (`+`, `-`, `@@`) and
returns the appropriate style (green/red/cyan), preserving indentation/
alignment.
- **Depends on:** tui-ansi-parser
- **Done when:** a unit test asserts a `+` line is green, a `-` line is red, a
  `@@` hunk header is cyan, and leading whitespace is preserved; tests pass.

### tui-exchange-render — Apply ANSI + diff styling in the exchange pane
Refactor `exchange_entry_lines` (`ui.rs`) to run response text through
`parse_ansi`, then apply diff coloring per line, producing styled `Span`s —
while preserving existing behavior (role labels, the streaming cursor). No raw
`\x1b[..m` renders literally anymore.
- **Depends on:** tui-ansi-parser, tui-diff-coloring
- **Done when:** a render test of an entry containing both ANSI codes and a
  unified diff shows correct styles with no literal escape sequences, and prompt/
  label rendering is unchanged; tests pass.

### tui-scroll-state — Exchange-pane scroll offset on `App`
In `app.rs`, add a `scroll_offset` (and a computed `scroll_max`) for the focused
(exchange) pane, with `scroll_up`/`scroll_down` helpers that clamp to
`[0, scroll_max]` and an auto-follow flag (stick to bottom until the user
scrolls up).
- **Depends on:** —
- **Done when:** unit tests assert the offset clamps within bounds and
  auto-follow re-engages at the bottom; tests pass.

### tui-mouse-scroll — Mouse wheel scrolls the focused pane
In `event.rs`, handle `CrosstermEvent::Mouse` wheel events → `AppEvent::ScrollUp`/
`ScrollDown`; their `update` arms call the scroll helpers; `render_exchange_pane`
applies the stored offset instead of always auto-scrolling. Task switching stays
on keys/sidebar.
- **Depends on:** tui-scroll-state
- **Done when:** a unit test asserts wheel-up/down change the exchange scroll
  offset (and do NOT change the selected task), with auto-follow at the bottom;
  the TUI scrolls the exchange pane on wheel input.

### tui-sidebar-label — Show `{project}/{plan}` as the run label
In `ui.rs` sidebar rendering, derive the run label as `{project}/{plan}` — the
repo directory name + the task-list's plan folder (e.g. `makina /
0002-Governance-and-Persistence`), falling back to the file stem when the path
doesn't fit that shape.
- **Depends on:** —
- **Done when:** a unit test asserts the label is `{project}/{plan}` for a
  plan-style path and the bare stem otherwise; the sidebar shows the meaningful
  name instead of `TASKS`.

### tui-gr-legend — Clarify the `G`/`R` columns with a legend
Add a legend line near the status bar in `ui.rs`: `G = gate iterations · R =
review iterations` (optionally widen the headers to `Gate`/`Rev`). Show it when
the focused run has any non-zero gate/review counts.
- **Depends on:** —
- **Done when:** a render/snapshot test asserts the legend text appears for a
  run with non-zero counts; the meaning of `G`/`R` is visible in the frame.

### tui-dep-list — Dependency view mode + list rendering
Add `DependencyViewMode { Off, List, Tree, Timeline }` and a
`dependency_view` field on `App`. Render the selected task's `depends_on` as a
compact `[state] task-id` list (in the main panel, respecting pane height) when
the mode is `List`.
- **Depends on:** —
- **Done when:** a render test asserts the list shows the selected task's
  prerequisites with state badges and does not overlap the exchange pane; tests
  pass.

### tui-dep-toggle — Cycle the dependency view with a key
Add `AppEvent::CycleDependencyView`; map a key (e.g. `v`) in `event.rs`; the
`update` arm cycles `Off → List → Tree → Timeline → Off`.
- **Depends on:** tui-dep-list
- **Done when:** a unit test asserts the cycle order and that the key maps to
  the event; tests pass.

### tui-dep-tree — Dependency tree view
Render `DependencyViewMode::Tree`: an indented ASCII tree (├──/└── connectors,
`[state] id` per node) of the selected task's prerequisites/dependents, walking
`depends_on` up to a couple of levels, in a framed "Dependencies" box.
- **Depends on:** tui-dep-list
- **Done when:** a `TestBackend` render test asserts the tree shows `depends_on`
  edges with correct indentation/connectors and state badges; tests pass.

### tui-dep-timeline — Dependency timeline (lane) view
Render `DependencyViewMode::Timeline`: a lane view over scheduling order where
tasks that can run in parallel appear side-by-side and dependents appear after
their prerequisites (respecting the DAG) — the parallelism observability view.
- **Depends on:** tui-dep-list
- **Done when:** a render test asserts independent tasks share a lane/row
  (parallel) while dependents are placed after their prerequisites; tests pass.
