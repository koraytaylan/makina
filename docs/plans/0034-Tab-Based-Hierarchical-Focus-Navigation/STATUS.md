# Plan 0034 — Tab-Based Hierarchical Focus Navigation — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-06-23, against develop._

- **Goal:** Users can navigate the TUI entirely by keyboard: Tab moves through Sidebar, Main content pane, and nested accordion sections with a visible focus indicator; Shift+Tab reverses; focused accordion sections are expandable/collapsible via Enter. Power-users achieve mouse-free navigation of complex plans and task hierarchies.
- **Root cause:** The Tab key currently implements a binary toggle (Sidebar ↔ Main) without any nested focus tracking for accordion sections introduced in Plan 0032. Users cannot Tab into or navigate within accordion sections; the focus model has no field to track which section is currently focused. Additionally, there is no visual indicator showing which region/section owns focus, breaking the standard desktop UI pattern where focus is always visible. This strands keyboard-only users at the Main pane boundary and forces reliance on hardcoded S/A/T/Z keys or mouse clicks to navigate the accordion.
- **Approach:** Extend the App focus model with a `focused_section: Option<AccordionSection>` field and a FocusState enum to represent hierarchical focus. Implement `move_focus_forward()` and `move_focus_backward()` methods that traverse Sidebar → Main → accordion sections → back to Sidebar in a defined sequence, checking whether a plan tab is active before entering accordion cycling. Wire Tab/Shift+Tab key events to these methods and add visual focus styling (background + bold) to the focused accordion section header in the render path. Test the feature with integration tests covering forward/backward traversal, wrapping, and section toggling via Enter.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Focus Model Extension | `extend-focus-model` | 📋 Planned |
| 0002 | Tab/Shift+Tab Navigation Logic | `move-focus-forward`, `move-focus-backward` | 📋 Planned |
| 0003 | Visual Focus Indicator | `add-visual-focus-indicator` | 📋 Planned |
| 0004 | Keyboard Integration & Testing | `wire-tab-shift-tab-events`, `integration-test-tab-traversal`, `verify-accordion-entry-toggle` | 📋 Planned |
