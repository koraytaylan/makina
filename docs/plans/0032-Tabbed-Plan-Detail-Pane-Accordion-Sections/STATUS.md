# Plan 0032 — Tabbed Plan-Detail Pane with Accordion Sections — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** ✅ Complete.

_Last updated: 2026-06-23, against develop._

- **Goal:** Multiple plan tabs are open and viewable in parallel; each tab displays SCOPE, ARCHITECTURE, TASKS, and STATUS as independent accordion sections. Users can expand/collapse sections per-tab, switch between tabs with keybindings, and close tabs without losing other work. The single-plan-view bottleneck is eliminated, improving UX for comparing multiple plans.
- **Root cause:** The current plan-detail pane (`plan_detail: Option<usize>`) shows one plan at a time, forcing users to toggle back and forth when comparing plans. The pane is monolithic (renders everything at once) rather than sectioned, making it hard to focus on specific content. SCOPE and ARCHITECTURE files are not cached in `PlanEntry`, so they cannot be rendered without significant refactoring.
- **Approach:** Extend `PlanEntry` to cache SCOPE/ARCHITECTURE/STATUS file contents at discovery time. Replace the singleton `plan_detail` model with tab-based plan viewing (reusing the tab infrastructure from Plan 0031). Add accordion sections to the plan-tab renderer, each with independent expand/collapse state tracked in `App::accordion_state`. Wire keybindings (s/a/t/z for accordion, Alt+Left/Right for tab navigation) to events. Test accordion state persistence, rendering, and end-to-end workflow.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Tab-Based Plan Rendering | `remove-plan-detail-singleton` ✅, `integrate-tab-bar-into-plan-pane` ✅ | ✅ Done |
| 0002 | Accordion-Section Layout for Plan Metadata | `extend-plan-entry-with-spec-content` ✅, `add-accordion-state-to-app` ✅, `add-toggle-accordion-event` ✅, `handle-accordion-toggle-in-update` ✅, `create-accordion-renderer` ✅ | ✅ Done |
| 0003 | Tab Navigation and Keybindings | `wire-accordion-keybindings` ✅ | ✅ Done |
| 0004 | Integration and Polish | `clamp-plan-tabs-on-discovery` ✅, `test-accordion-section-state` ✅, `integration-plan-tabs-rendering` ✅, `verification-plan-tab-workflow` ✅ | ✅ Done |
