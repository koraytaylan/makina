# Scope — Plan 0031

> Unify plan discovery, task viewing, and ingestion robustness by folding the PlanPicker modal into the sidebar tree, replacing the single detail pane with tabbed content navigation, and normalizing malformed TASKS.md at ingestion.

## Why this plan

**1. Plan discovery today requires mode-switching.** `Mode::PlanPicker`
(`crates/makina/src/app.rs:417`) is a modal overlay that **replaces** the
sidebar (`render_plan_picker`, `crates/makina/src/ui.rs:2339`); when activated,
the user cannot see the open runs/tasks tree until the modal closes. Switching
between browsing available plans and managing active runs requires toggling
between two exclusive views, fragmenting the user's workflow. The
`discovered_plans` list is computed once on startup
(`crates/makina-core/src/orchestrator.rs:208`) but never integrated into the
persistent sidebar tree. By folding plan discovery into the sidebar as
always-present top-level nodes (expandable to show their tasks), users can switch
contexts without mode loss.

**2. The single detail pane is a bottleneck for parallel viewing.**
`crates/makina/src/ui.rs:240–392` renders one fixed detail area per run: when a
user focuses task A to review its exchange, they must lose focus on task B's
timeline/state if they want to compare them. The `selected_task` state
(`crates/makina/src/app.rs:936`) points to a single active task, so only one
task's exchange log is visible at a time. By replacing the fixed pane with a tab
system — each opened task/plan in its own tab — users can keep multiple views
in-flight and switch freely (common in code editors, browsers, terminals).
Requires new App state for an open-tabs set + active-tab pointer, and tab-bar
renderer / switch/close keybinds.

**3. Ingestion brittleness blocks opening well-scoped plans.**
`interpret_and_seed` (`crates/makina-core/src/orchestrator.rs:1024`) hard-fails
on a missing or malformed `TASKS.md`: the read fails at `read_to_string`, and
`ParseError::ParseError` at location 0 or a specific line (e.g., "task heading
before section heading", `crates/makina-core/src/interpreter.rs:326`) prevents
the run from registering. Plan 0028 introduced generation-when-missing for
spec-only dirs, but the ingestion path still rejects malformed markdown
(inconsistent section/task ordering, missing fields, etc.) without a repair
attempt. A user with a valid SCOPE/ARCHITECTURE but a typo in TASKS.md has to
fix it manually or regenerate — no automatic normalization. By adding a
model-driven normalizer **upstream** of the deterministic interpreter, the
plan-opening path becomes robust: missing or malformed TASKS.md is auto-repaired
using the planner's understanding of the SCOPE/ARCHITECTURE, then the
deterministic path ingests the normalized version. This keeps the auditable
record (the written TASKS.md) and the execution model (deterministic
interpreter) aligned, preserving the governance wedge.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0003):

- **0001 — Sidebar Unified Plan-Task Tree.** Integrate discovered plans into the
  always-present sidebar tree as top-level nodes that expand to reveal their
  tasks. Remove the `Mode::PlanPicker` modal and fold plan discovery into the
  persistent sidebar navigation, eliminating mode-switching between plan browsing
  and run management.
- **0002 — Tabbed Content Pane.** Replace the single fixed detail/exchange pane
  with a tabbed interface where opening a task or plan from the sidebar opens a
  new tab for that content. Tabs show their title in a bar above the pane and can
  be switched/closed independently, enabling parallel viewing of multiple
  tasks/plans.
- **0003 — Model-Normalized TASKS.md Ingestion.** Add a model-driven normalizer
  that validates and repairs malformed or missing TASKS.md before the
  deterministic interpreter runs. On any read failure or parse error for a
  plan-style path (one with SCOPE.md + ARCHITECTURE.md), invoke the normalizer to
  repair/generate TASKS.md, write it back to the plan dir, and re-ingest through
  the deterministic path. Preserves the deterministic-governance wedge (model
  assistance only at the ingestion front door) and generalizes plan 0028's
  generate-when-missing.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Plan discovery requires modal overlay; mode-switching fragments the sidebar/runs view | `0001` |
| Single fixed detail pane prevents parallel task/plan viewing; `selected_task` is a singleton pointer | `0002` |
| Missing or malformed TASKS.md hard-fails ingestion; plan 0028 generates-when-missing but doesn't normalize-when-malformed | `0003` |

## Locked decisions

- **Discovered plans integrate into the persistent sidebar tree, not a separate
  modal.** The plan picker is folded into the unified sidebar
  (`visible_tree_nodes`) as first-class `TreeNode::Plan` entries, always visible
  and navigable alongside open runs. `Mode::PlanPicker` is removed. This
  eliminates mode-switching and keeps the navigation surface consistent. If
  discovered plans are empty, the tree shows only runs — no regression.
- **Tabs are content-identified, not view-identified; multiple tabs can show the
  same task.** The `TabContent` enum identifies a tab by (plan_slug, task_id) or
  (plan_slug), so opening the same task twice creates two separate tabs that can
  be scrolled to different positions independently. Switching between the two tabs
  of the same content preserves scroll position per-tab. This is the standard
  browser/editor tab behavior.
- **The normalizer only runs on ingestion failures within plan-convention dirs;
  non-plan dirs are unchanged.** The `is_plan_convention_dir()` check gates
  normalizer invocation: only dirs with both SCOPE.md + ARCHITECTURE.md trigger
  normalization on `read_to_string` failure or `ParseError`. Non-plan dirs get
  the existing error behavior, preserving backward compat. This also keeps the
  normalizer from interfering with ad-hoc task lists.
- **The normalized TASKS.md is written to disk and re-ingested through the
  deterministic interpreter.** The normalizer output is not directly used as the
  graph; instead, it is written to the plan dir's TASKS.md file, then the existing
  `StructuredTextInterpreter` re-reads and validates it. This ensures the
  in-memory graph and the on-disk artifact are consistent, and the written
  TASKS.md is the auditable record that users can read/edit.
- **Backward compatibility: sidebar cursor and tab state are decoupled.** Sidebar
  navigation (arrow keys, Enter) moves the `tree_cursor` and can open tabs, but
  does not automatically close other tabs. The tree cursor and active tab are
  independent pointers. This allows users to navigate the tree freely while
  maintaining open tabs, avoiding the frustration of losing tabs during
  navigation.

## Out of scope

- Pushing plan branches or opening PRs for the integration branch — covered by
  plan 0030 (plan-branch integration); this plan is purely about
  discovery/viewing/ingestion.
- Per-role `system_prompt` / `system_prompt_mode` config plumbing for the
  normalizer — the normalizer uses a hard-coded `PLANNER_NORMALIZE_SYSTEM_PROMPT`;
  per-role prompts are deferred to plan 0025.
- Deleting discovered plan entries from the sidebar (only collapse/expand) —
  removing discovered plans requires project discovery/config changes; out of
  scope here. Users can ignore collapsed plans.
- Persisting tab state across app restarts — tab history is session-local; no
  persistence to disk. Tabs are ephemeral UI state, not long-lived artifacts.
- Fine-grained normalizer tuning or multi-shot agent interaction for repair — the
  normalizer makes one planner call; iterative repair loops (if the first attempt
  fails) are a follow-up (plan 0032+).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
