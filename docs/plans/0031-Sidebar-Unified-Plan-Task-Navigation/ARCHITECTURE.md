# Architecture — Plan 0031 (deltas)

> The concrete deltas. This plan touches `crates/makina/src/app.rs`,
> `crates/makina/src/ui.rs`, `crates/makina/src/event.rs`,
> `crates/makina-core/src/orchestrator.rs`,
> `crates/makina-core/src/normalizer.rs` (new),
> `crates/makina-core/src/api.rs`, `crates/makina-core/src/constants.rs`,
> and the integration suites under `crates/makina/tests/` and
> `crates/makina-core/tests/`.
> Line numbers are hints; locate by symbol.

## 0001 — Sidebar Unified Plan-Task Tree

Today the sidebar renders a flat tree of open runs + their tasks
(`visible_tree_nodes`, `crates/makina/src/app.rs:1139`). Discovered plans are
held in `App::discovered_plans` (`crates/makina-core/src/orchestrator.rs:208`,
0027) but only rendered in a modal overlay (`render_plan_picker`,
`crates/makina/src/ui.rs:2339`) when `app.is_picking_plan()` (i.e.
`app.mode == Mode::PlanPicker`, `crates/makina/src/app.rs:417`). The modal
replaces the sidebar entirely, forcing a binary choice between viewing plans and
viewing runs.

**Edits:**

**Extend `TreeNode` with a plan variant.** Add a third variant to the enum at
`crates/makina/src/app.rs:581`, alongside the existing `Run { run }` and
`Task { run, task }`, so discovered plans become first-class tree nodes:

```rust
pub enum TreeNode {
    /// The run at `runs[run]`.
    Run { run: usize },
    /// Task at `runs[run].tasks[task]`.
    Task { run: usize, task: usize },
    /// A discovered plan at `discovered_plans[plan_idx]`.
    Plan { plan_idx: usize },
}
```

**Track per-plan collapse state.** Mirror `collapsed_runs` with a plan-keyed set
on `App` (around `crates/makina/src/app.rs:930`), initialized to empty in
`App::new()`:

```rust
/// Plan indices currently collapsed in the sidebar tree.
/// Parallel to `collapsed_runs` but keyed by index into `discovered_plans`.
pub collapsed_plans: HashSet<usize>,
```

**Prepend plans in the tree builder.** `visible_tree_nodes`
(`crates/makina/src/app.rs:1139`) is the pure flattener that feeds cursor
navigation. Emit discovered plans at the top, then the existing runs/tasks pass
unchanged:

```rust
pub fn visible_tree_nodes(&self) -> Vec<TreeNode> {
    let mut nodes = Vec::new();
    // Discovered plans lead the tree, navigable alongside runs.
    for (plan_idx, _plan) in self.discovered_plans.iter().enumerate() {
        nodes.push(TreeNode::Plan { plan_idx });
        // Expanded plans nest their tasks here once expansion lands.
    }
    // Then open runs and their (un-collapsed) tasks — existing logic.
    for (run_idx, run) in self.runs.iter().enumerate() {
        nodes.push(TreeNode::Run { run: run_idx });
        // ... existing collapsed_runs / task loop
    }
    nodes
}
```

**Render the plan node.** The sidebar build loop
(`crates/makina/src/ui.rs:156–237`) gains a `TreeNode::Plan` arm that shows the
slug with a disclosure glyph and reuses the plan-picker's dim "no tasks" hint
(`crates/makina/src/ui.rs:2364`):

```rust
TreeNode::Plan { plan_idx } => {
    let plan = &app.discovered_plans[*plan_idx];
    // ▸ collapsed, ▾ expanded — same disclosure vocabulary as runs.
    let disclosure = if app.collapsed_plans.contains(plan_idx) { "▸ " } else { "▾ " };
    let mut spans = vec![Span::raw(disclosure), Span::raw(&plan.slug)];
    if !plan.has_tasks {
        spans.push(Span::styled(" (no tasks — will plan)", Style::default().fg(Color::DarkGray)));
    }
    ListItem::new(Line::from(spans))
}
```

**Retire the modal path.** Remove the `PlanPicker` variant from `Mode`
(`crates/makina/src/app.rs:417`) and the `plan_cursor` field; drop the
`is_picking_plan()` branch that swaps in `render_plan_picker`
(`crates/makina/src/ui.rs:115–117`) so the sidebar is always the unified tree.
The shared `tree_cursor` now selects runs and plans interchangeably.

**Open a plan from the tree.** When `focused_node()`
(`crates/makina/src/app.rs:1157`) returns `TreeNode::Plan { plan_idx }`, Enter
dispatches a new event that routes through the existing open path
(`CoreApi::open_run` with the plan dir's `TASKS.md`):

```rust
TreeNode::Plan { plan_idx } => {
    // Reuse the same open path the file-browser/plan-picker already drove.
    return Some(AppEvent::OpenPlan(plan_idx));
}
```

**Properties that make this safe:**

- `visible_tree_nodes` is pure (returns `Vec<TreeNode>` with no IO), so adding a
  variant is a straightforward extension; the cursor machinery is unchanged.
- Discovered plans are already loaded at startup (`discover_plans`), so threading
  them into the tree is a rendering-and-routing change, not a new data source.
- The open path already exists — both the file browser and the (removed) modal
  invoke `CoreApi::open_run`; unifying them is a routing simplification.
- Backward compat: an empty `discovered_plans` (no `docs/plans/` dir) yields a
  runs-only tree — no regression.

## 0002 — Tabbed Content Pane

Today the main content pane is fixed to one run + one task
(`crates/makina/src/ui.rs:240–392`): `app.selected_run` and `app.selected_task`
(`crates/makina/src/app.rs:936`) are singleton pointers driven by sidebar cursor
movement. The pane renders the header, ingestion report, the focused task's
exchange log, and an optional dependency view — all for a single task at a time,
so comparing two tasks means losing focus on the first.

**Edits:**

**Define tab identity and state.** Add a content-keyed tab model to
`crates/makina/src/app.rs` (before `App`). A tab is identified by *what it shows*,
not by a view slot, so opening the same task twice is a single tab:

```rust
/// Content displayed in a tab in the main pane.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TabContent {
    /// A task within a run, keyed by plan slug + task id.
    Task { plan_slug: String, task_id: TaskId },
    /// A discovered plan, keyed by plan slug.
    Plan { plan_slug: String },
}

/// State for the tabbed content pane.
#[derive(Debug, Clone)]
pub struct TabState {
    /// Open tabs in insertion order.
    pub open_tabs: Vec<TabContent>,
    /// Index into `open_tabs`; `None` when no tabs are open.
    pub active_tab: Option<usize>,
}
```

`TabState::open_tab` either switches to an already-open tab or pushes a new one
and activates it; `close_tab` removes by index and clamps `active_tab` to a still-
valid neighbor (or `None` when empty).

**Hold the state on `App`.** Add `pub tabs: TabState` after `settings`
(around `crates/makina/src/app.rs:1074`), initialized to `TabState::new()` in
`App::new()`.

**Add tab events.** Extend `AppEvent` (`crates/makina/src/app.rs:595`) with the
four interactions, handled in `App::update()` against `self.tabs`:

```rust
/// Open a tab with the given content (or switch to it if already open).
OpenTab(TabContent),
/// Close the active tab.
CloseTab,
/// Switch to the next / previous open tab (wrapping).
NextTab,
PrevTab,
```

**Render the tab bar.** Add `render_tab_bar` to `crates/makina/src/ui.rs` (a
no-op when no tabs are open) that lays the open tabs out horizontally with the
active one highlighted:

```rust
/// Render the open tabs above the main content pane.
fn render_tab_bar(app: &App, frame: &mut Frame, area: Rect) {
    if app.tabs.open_tabs.is_empty() {
        return;
    }
    // One span per tab; active tab inverted. Label = task id or plan slug.
}
```

**Reserve a row for it in the pane layout.** The main-pane vertical split
(`crates/makina/src/ui.rs:349–391`) gains a leading `Constraint::Length(1)` for
the tab bar; the active tab's content fills the rest:

```rust
let split = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
        Constraint::Length(1), // tab bar (new)
        Constraint::Length(header_height),
        // ... ingestion / exchange / error, as before
    ])
    .split(inner);
render_tab_bar(app, frame, split[0]);
```

**Route sidebar selection to tabs.** Where Enter on a task node currently updates
`selected_task` (`crates/makina/src/event.rs`), dispatch `OpenTab` instead:

```rust
if let Some(TreeNode::Task { run, task }) = app.focused_node() {
    let run_view = &app.runs[run];
    return Some(AppEvent::OpenTab(TabContent::Task {
        plan_slug: run_view.plan_slug.clone(),
        task_id: run_view.tasks[task].id.clone(),
    }));
}
```

`[←] / [→]` cycle the open tabs and `[Ctrl+W]` closes the active one; the
dependency-view (`[v]`) and exchange keybinds keep applying — now to the active
tab's content.

**Properties that make this safe:**

- Tab state lives entirely in `App` (no IO), so the keybind handlers stay
  synchronous.
- The pane's sub-renderers (exchange pane, dependency view, ingestion panel) are
  already factored into helpers; pointing them at the active tab's content is a
  composition change, not a rewrite.
- The sidebar cursor and the active tab are independent pointers — moving the
  tree cursor never closes or reorders tabs — so navigation and tab management are
  decoupled, orthogonal concerns.
- Backward compat: opening one task at a time degenerates to a single-tab pane,
  so there is no new UI to learn for the common case.

## 0003 — Model-Normalized TASKS.md Ingestion

Today `interpret_and_seed` (`crates/makina-core/src/orchestrator.rs:1024`) reads
`TASKS.md` for a plan dir. On `NotFound`, plan 0028 branches into a generate path
if the dir is plan-convention compliant (has `SCOPE.md` + `ARCHITECTURE.md`,
`docs/plans/0028-Planner-Generated-Tasks/SCOPE.md`). Any *other* read error, or a
`ParseError` during deterministic interpretation
(e.g. "task heading before section heading",
`crates/makina-core/src/interpreter.rs:326`), hard-fails the open: the user must
hand-fix the markdown and retry. Plan 0028 generates-when-missing but never
normalizes-when-malformed.

**Edits:**

**Add a `ModelNormalizer`.** New file `crates/makina-core/src/normalizer.rs`,
registered with `pub mod normalizer;` in `lib.rs`. Like `ModelInterpreter`, it
wraps an `AgentBackend`; `normalize` reads the SCOPE/ARCHITECTURE brief, feeds the
planner the brief plus any malformed `TASKS.md` as error context, and returns the
repaired markdown:

```rust
pub struct ModelNormalizer {
    backend: Arc<dyn AgentBackend>,
}

impl ModelNormalizer {
    pub fn new(backend: Arc<dyn AgentBackend>) -> Self { Self { backend } }

    /// Repair/generate `TASKS.md` from the SCOPE.md + ARCHITECTURE.md brief.
    /// Any existing (malformed) `TASKS.md` is passed as error context so the
    /// planner can preserve task ids / dependencies where possible.
    pub async fn normalize(&self, plan_dir: &Path, slug: &str)
        -> Result<String, NormalizeError> { /* read brief; prompt planner; return markdown */ }
}
```

**Add the repair system prompt.** A constant alongside `PLANNER_SYSTEM_PROMPT` /
`PLANNER_GENERATE_SYSTEM_PROMPT` (`crates/makina-core/src/constants.rs`) tells the
planner to emit canonical, convention-conformant markdown and nothing else:

```rust
/// System prompt for the normalizer: repair or regenerate a canonical TASKS.md.
pub const PLANNER_NORMALIZE_SYSTEM_PROMPT: &str = r#"
You are a Makina task-list repair expert. Given a SCOPE.md / ARCHITECTURE.md
brief and (optionally) a malformed TASKS.md, write a canonical TASKS.md that
conforms to the Makina structured-text convention. Output ONLY the file content.
"#;
```

**A convention gate.** A small helper in `orchestrator.rs` decides whether a dir
is eligible for model assistance — the same predicate 0028's generate path uses:

```rust
/// True when `dir` follows the plan convention (both spec files present).
fn is_plan_convention_dir(dir: &Path) -> bool {
    dir.join("SCOPE.md").is_file() && dir.join("ARCHITECTURE.md").is_file()
}
```

**Wire normalization into the ingestion path.** In `interpret_and_seed`
(`crates/makina-core/src/orchestrator.rs:1024`), wrap the deterministic
`interpret` call: on a `ParseError` for a plan-convention dir, normalize, write
the result back, and re-interpret the canonical text. On normalizer failure, fall
back to the original error:

```rust
let graph = match self.interpreter.interpret(&slug, &text).await {
    Ok(g) => g,
    Err(InterpretError::ParseError { location, context }) => {
        let plan_dir = task_list_path.parent().unwrap();
        if is_plan_convention_dir(plan_dir) {
            // Front-door repair, then re-validate deterministically.
            match self.normalizer.normalize(plan_dir, &slug).await {
                Ok(normalized) => {
                    tokio::fs::write(&task_list_path, &normalized).await?;
                    self.interpreter.interpret(&slug, &normalized).await?
                }
                // Graceful degradation: surface the original parse error.
                Err(_) => return Err(ApiError::InterpretError { /* original location/context */ }),
            }
        } else {
            return Err(ApiError::InterpretError { /* original */ });
        }
    }
    Err(e) => return Err(ApiError::InterpretError { /* e */ }),
};
```

**Inject the normalizer.** `CoreApi` (`crates/makina-core/src/api.rs`) gains a
`normalizer: Arc<ModelNormalizer>` field, built from the same `AgentBackend`
already injected for the planner, so `interpret_and_seed` reaches it via `self`.

**Properties that make this safe:**

- The normalizer runs **only after** the deterministic parser fails on a
  plan-convention dir; non-plan task lists never reach it, preserving existing
  error behavior for ad-hoc dirs.
- The repaired markdown is **re-validated** by the deterministic interpreter
  before the run ingests it, so the graph contract is unchanged — the model never
  produces the in-memory graph directly.
- The written `TASKS.md` is the auditable record: a user reads and edits it like
  any other file; the normalizer is a repair tool, not a black box.
- The deterministic-governance wedge holds: only the ingestion front door
  (read / parse failure) invokes the model; the scheduler, gates, and merges stay
  deterministic.
- Backward compat: a plan dir with a valid `TASKS.md` is untouched — the
  normalizer fires solely on read/parse errors.

## Test strategy

- **0001 (sidebar).** A unit test asserts `visible_tree_nodes()` places
  discovered plans before open runs (the first node is `TreeNode::Plan { .. }`
  when plans are present). An integration test navigates the `tree_cursor` to a
  plan node, simulates Enter, and asserts the plan opens via `CoreApi::open_run`
  (it transitions onto the open-runs list).
- **0002 (tabs).** Unit tests over `TabState` cover `open_tab` adding a new tab,
  re-opening switching back to the existing tab, and `close_tab` removing a tab
  while keeping `active_tab` valid. A render test opens two task tabs and asserts
  both labels appear in the drawn tab bar with the active tab distinguished.
- **0003 (normalizer).** A basic test instantiates `ModelNormalizer` with a mock
  backend. An integration test writes a plan dir with valid SCOPE/ARCHITECTURE but
  a malformed `TASKS.md` (task heading before section heading), opens it through
  the API with a mock planner returning valid markdown, and asserts the open
  succeeds and the on-disk `TASKS.md` is the normalized version.
- All tests keep `cargo test`, `cargo clippy -- -D warnings`, and
  `cargo fmt --check` green.

## Interaction with prior work

- **0027 / discovery.** Reuses `App::discovered_plans` and the startup
  `discover_plans` pass as the sidebar's plan source — no new discovery; 0001 only
  changes how the already-loaded entries are rendered and routed.
- **0028 / generate-when-missing.** Generalizes 0028's `NotFound` generate path
  into a normalize-on-ingestion pattern: 0003 reuses the same plan-convention gate
  (`SCOPE.md` + `ARCHITECTURE.md`) and the same "write canonical `TASKS.md`, then
  re-interpret deterministically" discipline, extending it from missing to
  malformed input while keeping the model at the front door only.
- **0030 / plan-branch integration.** Orthogonal: this plan is discovery /
  viewing / ingestion and never touches branch or merge behavior; pushing plan
  branches and opening PRs remains 0030's domain.
