# Plan 0033 — Mouse-Position-Aware Scroll Routing with Scrollbars — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** 📋 Planned.

_Last updated: 2026-06-23, against develop._

- **Goal:** Multiple independent panels (sidebar, exchange pane, plan accordion, dependency view) each scroll independently in response to mouse-wheel events positioned over them; vertical scrollbars render on the right edge of each scrollable panel to indicate content height and current scroll position; exchange-pane auto-follow logic is preserved; all gate commands pass.
- **Root cause:** Scroll events currently ignore mouse position and always scroll the exchange pane. No per-panel scroll state exists, no panel geometry tracking for hitbox testing, and no scrollbar widgets are rendered, so users cannot intuitively scroll the correct panel or see scroll feedback.
- **Approach:** Layer-by-layer: (0001) add geometry tracking and capture mouse coordinates; (0002) add per-panel scroll-state maps; (0003) implement coordinate-based dispatch; (0004) integrate Scrollbar widgets; (0005) apply offsets during rendering; (0006) wire everything together and test end-to-end.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Hitbox and Mouse Tracking | `panel-geometry-struct`, `scroll-events-with-coords` | 📋 Planned |
| 0002 | Per-Panel Scroll State | `scrollable-panel-enum`, `per-panel-scroll-methods` | 📋 Planned |
| 0003 | Mouse-Position-Aware Scroll Event Dispatch | `scroll-event-dispatch` | 📋 Planned |
| 0004 | Ratatui Scrollbar Widget Integration | `scrollbar-widget-exchange`, `scrollbar-widget-sidebar`, `scrollbar-widget-accordion` | 📋 Planned |
| 0005 | Scroll Offset Clamping and Rendering | `apply-scroll-offsets-exchange`, `apply-scroll-offsets-accordion`, `apply-scroll-offsets-sidebar`, `apply-scroll-offsets-dependency` | 📋 Planned |
| 0006 | Integration and Testing | `record-panel-geometries`, `integration-test-scroll-routing`, `scrollbar-rendering-test`, `verify-gate-commands` | 📋 Planned |
