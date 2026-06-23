# Scope — Plan 0034

> Implement keyboard-driven Tab/Shift+Tab hierarchical focus navigation where Tab moves focus sequentially through major regions (Sidebar → Main → accordion sections) with Shift+Tab for reverse traversal, visual focus indicators, and wrapping behavior at region boundaries.

## Why this plan

**1. Tab currently toggles only Sidebar ↔ Main, ignoring accordion nesting.** `crates/makina/src/event.rs:1207` maps Tab to `AppEvent::FocusNext`, and `crates/makina/src/app.rs:1684–1689` implements it as a simple binary toggle between `Panel::Sidebar` and `Panel::Main`. When a plan tab with expandable accordion sections (Plan 0032) is active, Tab has nowhere to go within the pane — users cannot Tab into SCOPE/ARCHITECTURE/TASKS/STATUS sections or traverse them. Keyboard-only users are left stranded at the Main pane boundary, forced to use mouse or hardcoded section toggles (S/A/T/Z) to navigate further.

**2. The Panel enum has no nested focus tracking for accordion sections.** `crates/makina/src/app.rs:569–572` defines `Panel` with only two variants: `Sidebar` and `Main`. There is no focus state for *which* accordion section is currently focused, or any API to move focus *within* the Main pane. The accordion expand/collapse state exists (`crates/makina/src/app.rs:1176–1179`, a `HashMap` keyed by plan slug), but no parallel "focused section" field. This makes hierarchical focus navigation structurally impossible without extending the focus model.

**3. Accordion sections are presently togglable only by hardcoded keys (S/A/T/Z), not Tab navigation.** `crates/makina/src/event.rs:1228–1251` checks for plan-tab-active and focused-Main before invoking accordion toggles; `crates/makina/src/app.rs` does not track which section Tab should focus next. Users accustomed to Tab-based navigation (browser, terminal, desktop app patterns) have no affordance to discover or use Tab as a hierarchical movement verb; they must remember or look up the S/A/T/Z keys.

**4. No visible focus indicator marks the currently focused item within regions.** When Tab moves focus from Sidebar to Main, there is no visual feedback showing which section inside Main now owns focus (if any). All four accordion sections in the plan pane remain unmarked; users cannot distinguish an "unfocused but readable" section from a "focused and interactive" one. This breaks the desktop UI convention where focus is *always* visible, allowing power-users to predict Tab behavior.

**5. Plan 0032 provides natural nested focusable targets but leaves the navigation pattern incomplete.** `crates/makina/src/app.rs:932–940` defines the `AccordionSection` enum (Scope, Architecture, Tasks, Status) and `crates/makina/src/ui.rs:1327–1410` renders them as expandable sections. The UI can display focus; there is no blocker to adding a nested focus model. Completing the feature here enables Tab to work as users expect: a unified, hierarchical navigation scheme that mirrors browser/editor/terminal patterns — the UX gold standard for keyboard-driven applications.

This plan extends the focus model with a `focused_section` field and a `FocusState` enum, implements forward/backward traversal through the Sidebar → Main → accordion-section sequence, adds visible focus styling to the focused section header, and wires Tab/Shift+Tab to the new movement functions with explicit wrapping and test coverage.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0004):

- **0001 — Focus Model Extension.** Extend the App focus tracking from a binary `Panel` enum to a hierarchical `FocusState` that represents both the current region (Sidebar/Main) and, when Main is focused, the currently focused accordion section. Add a `focused_section: Option<AccordionSection>` field to App and implement focus-movement logic that respects the nesting: only accordion sections are focusable when a plan tab is active.
- **0002 — Tab/Shift+Tab Navigation Logic.** Implement forward Tab and backward Shift+Tab traversal through regions and nested accordion sections in a defined order: Sidebar → Main → accordion sections (if a plan tab is active) → back to Sidebar. Tab from Sidebar focuses Main; Tab from Main without an active plan tab wraps back to Sidebar; Tab from Main with a plan tab enters accordion sections, cycling through them; Tab from the last accordion section wraps to Sidebar. Shift+Tab reverses direction at all levels.
- **0003 — Visual Focus Indicator.** Add visible focus markers (border highlight or underline) to `render_sidebar` and `render_plan_accordion_pane` so users see which item/section currently owns keyboard focus. Apply distinct styling (e.g., bright background or bold) to the focused section header when `focused_section` is `Some`. The sidebar is already visually distinct (`highlight_style` and `highlight_symbol` are set in the current code).
- **0004 — Keyboard Integration & Testing.** Wire Tab/Shift+Tab key events to the new focus-movement functions. Update keybinding translation in `event.rs` to emit Tab/Shift+Tab events. Update the `AppEvent::FocusNext` handler to call the new `move_focus_forward` and `move_focus_backward` methods. Add unit tests for focus traversal order, wrapping behavior, and state transitions, plus an integration test navigating through all regions with Tab and back with Shift+Tab.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Tab currently toggles only Sidebar ↔ Main, ignoring accordion nesting. | `0001` |
| The Panel enum has no nested focus tracking for accordion sections. | `0001` |
| Accordion sections are presently togglable only by hardcoded keys (S/A/T/Z), not Tab navigation. | `0002` |
| No visible focus indicator marks the currently focused item within regions. | `0003` |
| Plan 0032 provides natural nested focusable targets but leaves the navigation pattern incomplete. | `0001` |

## Locked decisions

- **Accordion section focus is independent of sidebar tree cursor.** The `focused_section: Option<AccordionSection>` field in App tracks accordion section focus only when `focused_panel == Panel::Main`. When focus is in the Sidebar, `focused_section` is ignored (the `tree_cursor` index owns focus). This orthogonality allows users to navigate the sidebar tree freely without losing accordion focus state, so returning to a plan tab preserves which section they were last viewing. On first Tab into a plan tab, `focused_section` is `None`; the next Tab focuses Scope.
- **Tab traversal order is fixed: Sidebar → Main → Scope → Architecture → Tasks → Status → Sidebar.** The accordion section cycle order is hardcoded in `App::accordion_section_order()` as `[Scope, Architecture, Tasks, Status]`. This order mirrors the plan document hierarchy (SCOPE.md is the planning overview, ARCHITECTURE.md describes edits, TASKS.md is the executable list, STATUS.md tracks progress). Tab moves through these in sequence; Shift+Tab reverses. The order is not configurable per this plan; future plans may add keybinding customization, but the default order is intentional and stable.
- **Wrapping at region boundaries is explicit and documented.** When Tab reaches the last accordion section (Status), the next Tab wraps to Sidebar (not to Main, not to Scope). When Shift+Tab is pressed in Sidebar with a plan tab active, it jumps directly to Status (not cycling up through Scope). When Tab is pressed in Main without an active plan tab, it wraps immediately to Sidebar (accordion focus is skipped). These behaviors are implemented in `move_focus_forward` and `move_focus_backward` and tested explicitly so future changes cannot accidentally alter the wrapping logic.
- **Visual focus indicator uses background color + bold for accordion section headers.** When an accordion section header is focused (checked via `app.focused_section`), its rendering applies `Color::DarkGray` background and `Modifier::BOLD` to distinguish it from unfocused sections. This follows the sidebar's existing pattern (`highlight_symbol` + `highlight_style`). The choice of DarkGray is deliberate (sufficient contrast, not jarring); future color customization is out of scope.
- **Enter toggles the focused accordion section; S/A/T/Z keybindings remain available for quick access.** When a user Tabs to focus an accordion section, pressing Enter toggles (expand/collapse) it. This follows standard UI convention where Enter activates a focused control. The hardcoded S/A/T/Z keybindings (from Plan 0032) remain active and work identically — they toggle sections without moving focus, providing a faster alternative for users who learn the keys. No conflicts; Enter and S/A/T/Z are orthogonal input methods for the same operation.

## Out of scope

- Mouse click on accordion section headers to focus them. This plan is keyboard-driven focus navigation. Mouse clicks may be added in a future plan; for now, Tab/Shift+Tab are the only ways to move focus into accordion sections.
- Custom keybinding configuration for Tab or accordion toggles. Keybindings are hardcoded in this plan. Future plans may add a settings UI for keybinding customization; this plan delivers the core Tab-based navigation feature only.
- Arrow key navigation within accordion sections (e.g., Up/Down to move through content). Arrow keys are reserved for sidebar tree navigation and pane scrolling. Content within accordion sections is scrollable but not keyboard-navigable as a structured list in this plan.
- Vim-style keybindings (j/k for down/up, h/l for collapse/expand). Navigation uses Tab/Shift+Tab and Enter; Vim-style keys are not added. A future plan may introduce modal keybindings; this plan sticks to standard desktop conventions (Tab = focus cycle).
- Persisting focused accordion section across app restart. Focus state is ephemeral (lives in App memory only). On app restart, focus resets to Sidebar. Persistence of session state is a separate feature (out of scope).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
