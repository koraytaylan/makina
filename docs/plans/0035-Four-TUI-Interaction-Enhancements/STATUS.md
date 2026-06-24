# Plan 0035 — Four TUI Interaction Enhancements (Mouse Click, Arrow Keys, Markdown Rendering, Task Tabs) — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** ✅ Complete.

_Last updated: 2026-06-24, against develop._

- **Goal:** Four complementary TUI interaction enhancements ship and pass all gates: (1) clicking accordion section headers in plan/task tabs toggles expand/collapse, (2) Right/Left arrows work as Tab/Shift+Tab equivalents in the main pane for forward/backward focus, (3) Markdown in task-entry tabs renders headings, code blocks, lists, links, and rules properly, (4) pressing Enter on a task in the sidebar opens a new tab displaying the task entry with full metadata and rendered content.
- **Root cause:** Four distinct usability gaps in the TUI: (1) accordion headers are rendered but not mouse-interactive, leaving users without a modern click-to-toggle UX; (2) keyboard navigation lacks arrow-key parity with Tab, forcing users to reach for an uncomfortable key for a common action; (3) task-entry content is not properly rendered, displaying raw Markdown instead of readable structure; (4) task entries cannot be viewed in tabs, preventing the parallel-comparison workflows users need for multi-task analysis.
- **Approach:** Research the codebase to locate accordion rendering, event translation, and markdown rendering logic. Design four independent workstreams, each with a single focused change: (1) compute accordion header bounds during render and hit-test mouse clicks, (2) extend arrow-key handling to support main-pane focus as Tab equivalents, (3) create task-entry pane using the same Markdown renderer and width-threading as plan 0020, (4) ensure task tabs render the entry view. Each workstream is testable independently; dependencies are minimal. All four are junior-executable with concrete file:line anchors and exact acceptance criteria.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Accordion Click-to-Toggle | `accordion-click-detection` | ✅ Done |
| 0002 | Arrow-Key Hierarchical Navigation | `arrow-key-navigation-parity` | ✅ Done |
| 0003 | Markdown Content Rendering | `task-entry-markdown-rendering` | ✅ Done |
| 0004 | Task Entry Opening in Tabs | `task-entry-tab-opening` | ✅ Done |
