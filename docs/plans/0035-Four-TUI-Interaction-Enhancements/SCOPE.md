# Scope — Plan 0035

> Four complementary TUI interaction enhancements (mouse-click accordion toggle, arrow-key navigation parity, markdown rendering in task tabs, task-tab opening) that improve keyboard and mouse usability in the Makina viewer.

## Why this plan

**1. Mouse clicks on accordion headers do not toggle expand/collapse state.** The plan-tab accordion pane (plan 0032) renders section headers with `[+]` (collapsed) or `[-]` (expanded) markers (`ui.rs:1432–1461`), and pressing 's'/'a'/'t'/'z' toggles each section (`event.rs:1238–1265`). However, the app translates mouse clicks into text-selection events (`event.rs:1100–1102`, creating `AppEvent::SelectionStart/Extend/End`) and never dispatches a click as a toggle intent. Users expect to click the header to toggle, like any web accordion — the keyboard shortcut alone is insufficient for modern UX.

**2. Arrow keys are not navigation-parity equivalents to Tab/Shift+Tab.** Today, Tab switches panel focus (`Panel::Sidebar ↔ Panel::Main`), and Shift+Tab reverses the focus (via `BackTab` or Tab+SHIFT, `event.rs:1210–1217`). Right/Left arrows are *sidebar-only* (expand/collapse runs when focused on the sidebar; move focus right/left from main pane, `event.rs:1288–1293`, `app.rs:1917–1965`). Users familiar with desktop apps expect Right arrow to move focus forward (like Tab) and Left to move focus backward (like Shift+Tab) when in the main pane, matching conventions from browsers and text editors. This eliminates the need to reach for Tab and makes keyboard navigation more intuitive.

**3. Markdown is not rendered in task-entry tabs; raw source is displayed.** Task tabs (`TabContent::Task { plan_slug, task_id }`, `app.rs:896`) open when pressing Enter on a task in the sidebar (`event.rs:423–435`). Today, task tabs render the exchange pane (the conversation log) but there is no separate "task metadata" view showing the task entry (from the TASKS.md file) with its title, steps, etc. If a task tab displays the task entry at all, the entry text is raw Markdown. Plan 0020 hardened markdown rendering for exchange responses; task entries should receive the same treatment — code blocks, lists, links, and rules rendered as readable structure, not raw text.

**4. Task entries cannot be opened and compared side-by-side; the sidebar tree is the only reference.** The sidebar shows task-list tree nodes (`TreeNode::Task { run, task }`, `ui.rs:183–217`), and pressing Enter on a task opens a tab showing the task's exchange log. But there is no way to open a tab that shows the *task entry itself* (the task's role, title, steps, dependencies, description from the TASKS.md file), separate from the exchange log. To view two task entries side-by-side (e.g., comparing task steps or dependencies) users must toggle back and forth in the sidebar, losing context. Tab-based task viewing — one tab per task entry — follows the plan-tab model (plan 0032) and enables the parallel-reference workflows users ask for.

This plan adds four focused TUI (ratatui) interaction improvements to the Makina viewer: hit-testing accordion header clicks to toggle sections, extending Right/Left arrow handling to act as Tab/Shift+Tab equivalents in the main pane, rendering task-entry content through the hardened `render_markdown`, and rendering the task-entry view when a task tab is active so entries can be opened and compared.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0004):

- **0001 — Accordion Click-to-Toggle.** Detect mouse clicks on accordion section headers in plan tabs and toggle their expand/collapse state, matching keyboard toggle behavior (s/a/t/z keys). Clicks on the section title or the `[+]`/`[-]` marker both toggle. Drag/move/up events are no-ops so text selection in the content area remains unaffected.
- **0002 — Arrow-Key Hierarchical Navigation.** Implement Right arrow as a Tab-equivalent (forward focus) and Left arrow as Shift+Tab-equivalent (backward focus) when the main pane is focused. In the sidebar, Right/Left continue to expand/collapse runs and move focus as they do today. This provides keyboard navigation parity with Tab/Shift+Tab and aligns with desktop-app conventions, reducing the reach to the Tab key.
- **0003 — Markdown Content Rendering.** Apply Markdown rendering (from plan 0020) to task-entry tabs so code blocks, lists, links, headings, and rules render as readable structure. When a task tab is active and shows a task entry, render the entry's text through `render_markdown` instead of displaying raw text. Extend the width-threading and hard-wrapping logic from plan 0020 to task panes.
- **0004 — Task Entry Opening in Tabs.** When pressing Enter on a task node in the sidebar, open a new tab showing that task's entry (metadata and content from the task-list file), in addition to (or instead of) the exchange log. This extends plan 0032's plan-tab opening to task entries, enabling side-by-side task comparison and a parallel reference workflow.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Accordion section headers render but mouse clicks do not toggle expand/collapse. | `0001` |
| Right/Left arrows are sidebar-only; users expect arrow parity with Tab/Shift+Tab for forward/backward focus. | `0002` |
| Markdown in task-entry tabs is not rendered (raw text); plan 0020 hardening is not applied. | `0003` |
| Task entries cannot be opened in tabs for side-by-side viewing; sidebar-tree-only workflow limits multi-task comparison. | `0004` |

## Locked decisions

- **Accordion click toggle uses hit-test bounds computed fresh per render.** Accordion section header bounding boxes are computed during each render of the plan/task accordion pane. The event loop hit-tests incoming mouse clicks against these bounds. This approach is robust to terminal resizes and pane reflows: bounds automatically adjust when the layout changes.
- **Right/Left arrow behavior changes only in the main pane; sidebar behavior is unchanged.** When the main pane is focused, Right→FocusNext (Tab-like), Left→FocusPrev (Shift+Tab-like). When the sidebar is focused, Right/Left preserve their existing behavior. This is backward-compatible and makes keyboard navigation intuitive in both panes.
- **Markdown rendering in task tabs uses the same render_markdown function as plan 0020.** The `markup::render_markdown` function is already hardened and tested in plan 0020. Task-entry panes call it with the pane's inner width. No new markdown parser or renderer is introduced.
- **Task entries are opened in tabs; the sidebar remains the selector.** Pressing Enter on a task node in the sidebar opens a tab displaying that task's entry. The sidebar remains the primary navigation surface; the tab is a view onto the selected task.
- **Task-entry tabs show the entry definition, not the exchange log.** A task tab displays the task entry (role, title, steps, dependencies, notes from the task-list file) rendered with Markdown support. The exchange log is not shown in the entry view.

## Out of scope

- Syntax highlighting of code blocks in Markdown (language-specific tokenization). Plan 0020 already defers this; plan 0035 reuses the same renderer without adding language detection.
- Clickable/openable links in the rendered Markdown. Plan 0020 renders links as text + dim URL suffix; no click handling. Task-entry rendering reuses this; no new hyperlink behavior.
- Persisting accordion state across app restarts or switching tabs. Plan 0032 already specifies accordion state is ephemeral (session-local); plan 0035 does not extend persistence.
- Hot-reloading or live updates when TASKS.md files change on disk. Task entries are parsed at discovery time; runtime file watching is out of scope.
- Rendering all task-list file formats (e.g. JSON, YAML); only Markdown TASKS.md. Scope is limited to the TASKS.md format as defined in plan 0001.
- Customizable keybindings for accordion toggle or arrow-key behavior. Keybindings are hard-coded (s/a/t/z for accordion, Right/Left for navigation). Keybinding config is a separate future plan.
- Mouse dragging to resize panes or click-to-focus on accordion content. Scope covers accordion header clicks only; pane resizing and content-area clicks are out of scope.
- Task-entry tabs for task nodes under plans (PlanTask nodes in the sidebar tree). Task tabs open only when pressing Enter on run-bound task nodes (TreeNode::Task).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
