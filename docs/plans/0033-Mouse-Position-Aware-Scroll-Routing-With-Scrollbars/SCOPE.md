# Scope — Plan 0033

> Enable mouse-wheel scroll routing to the panel under the cursor, and render vertical scrollbars on scrollable panels to show content height and scroll position.

## Why this plan

**1. Scroll events ignore mouse position; all wheel events scroll only the exchange pane.** Currently at `crates/makina/src/event.rs:1098–1099`, mouse wheel events unconditionally dispatch `ScrollUp`/`ScrollDown` regardless of cursor position. The comment explicitly states "Mouse wheel scrolls the focused exchange pane regardless of the `browsing` flag", but there is only one exchange pane and no routing logic to handle multiple scrollable areas (sidebar, accordion pane, dependency view sub-pane).

**2. No per-panel scroll state exists.** At `crates/makina/src/app.rs:569–572`, the `Panel` enum defines only `Sidebar` and `Main` top-level panels. At `crates/makina/src/app.rs:1064–1090`, only `exchange_scroll` and `last_scroll_max` track scroll state for the exchange pane; no scroll offsets exist for sidebar, dependency view, plan accordion, or any sub-pane within the main area.

**3. No panel geometry tracking.** The rendering pass in `crates/makina/src/ui.rs:58–496` computes layout rectangles for every panel (sidebar, content_area, exchange_pane_area, error_area), but these are local variables that are discarded after rendering. There is no persistent record of which `Rect` corresponds to which logical panel, so hitbox-testing a mouse coordinate has nowhere to query "what panel is at (column, row)?".

**4. No scrollbar widgets.** Ratatui 0.30 provides `widgets::Scrollbar` (available in the workspace dependency), but it is not imported or used anywhere. Every scrollable pane (exchange pane, sidebar if tall, plan accordion, dependency view) currently has no visual feedback about content height or scroll position beyond truncated text.

**5. Multi-panel layouts need mouse-aware scroll routing.** With the addition of the plan accordion pane (plan 0032), sidebar expansion (plan 0027), and the dependency-view overlay (visible when `DependencyViewMode` is not `Off`), the TUI now has multiple independently-scrollable areas. A user mousing over the sidebar expects to scroll the sidebar, not the exchange pane. The current "always scroll exchange pane" behaviour is unintuitive and cannot be fixed by "focus the sidebar" because focus is typically on the exchange for review.

This plan adds mouse-position-aware routing, per-panel scroll-state tracking, and scrollbar widgets to make scroll behaviour intuitive and provide visual feedback.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0006):

- **0001 — Hitbox and Mouse Tracking.** Add a `PanelGeometry` struct to store rendered panel rectangles. Extend `App` to record the geometry of sidebar, exchange pane, dependency view, and plan accordion at each render. Modify `event.rs` to capture mouse coordinates from wheel events and pass them to a hitbox-detection function.
- **0002 — Per-Panel Scroll State.** Extend `App` to track independent scroll offsets and maximums for each scrollable panel: sidebar, exchange pane, plan accordion, and dependency view overlay. Introduce an enum to name panels and a map to store scroll state.
- **0003 — Mouse-Position-Aware Scroll Event Dispatch.** Modify `app.rs::update` to handle the new `ScrollUpAt` / `ScrollDownAt` events, use the hitbox-detection function to identify the target panel, and dispatch to the appropriate scroll method for that panel.
- **0004 — Ratatui Scrollbar Widget Integration.** Import ratatui's `Scrollbar` widget and render it on the right edge of each scrollable panel. Wire the panel's scroll offset and content height to the widget state. Show/hide the scrollbar based on content height versus viewport height.
- **0005 — Scroll Offset Clamping and Rendering.** Update rendering paths to read per-panel scroll offsets from the map, apply clamping, and pass the offset to the `.scroll()` method of each widget. Ensure the auto-follow logic for the exchange pane still works.
- **0006 — Integration and Testing.** Wire all five workstreams together. Update the render pass to call `set_panel_geometries()` with the computed layout rectangles. Test that wheel-scroll-over-sidebar scrolls the sidebar; wheel-scroll-over-main scrolls the main content; scrollbars render only when needed; auto-follow still works; all gate commands pass.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Scroll events ignore mouse position. | `0001` |
| No per-panel scroll state exists. | `0002` |
| No panel geometry tracking. | `0001` |
| No scrollbar widgets. | `0004` |
| Multi-panel layouts need mouse-aware scroll routing. | `0003` |

## Locked decisions

- **Panel geometries are recorded every frame, not cached persistently.** The `panel_geometries` field is recalculated every render pass (via `set_panel_geometries()`) to reflect the current layout. This ensures hitbox testing is always performed against the current terminal dimensions and split layout, accounting for resize events and dynamic pane visibility. If the layout changes (e.g., a run is selected/deselected), the next render pass updates the geometries and the event loop uses the fresh values.
- **Mouse coordinates outside all panels are silently ignored, not treated as errors.** When a scroll event occurs at coordinates not matching any panel (e.g., on a border, in the status bar, or outside the rendered area), `panel_at()` returns `None` and the scroll is a no-op. This is defensive: the event is not rejected, logged, or panicked; it simply has no effect. This simplifies the event dispatch logic and avoids edge-case bugs.
- **Sidebar scroll offset is applied directly to the list content.** The `scroll_offsets[Sidebar]` map entry is tracked and updated by wheel scroll events, a scrollbar is rendered, and the offset is applied to the list itself via ratatui 0.30's `ListState::with_offset(usize)` / `offset_mut()` (`offset` = "Index of the first item to be displayed"). The list content scrolls — there is no deferral. ratatui may nudge the offset to keep the selected item visible, which is acceptable.
- **Auto-follow logic is preserved only for the exchange pane.** The `exchange_auto_follow` boolean and the `effective_offset()` method apply auto-follow logic only when the panel is `ScrollablePanel::Exchange`. Other panels (sidebar, accordion, dependency view) have no auto-follow behaviour; they simply track and render their scroll offset as-is. This keeps the logic simple and focused on the exchange pane's existing auto-follow requirement.
- **Scrollbar rendering is conditional on `scroll_max > 0`.** A panel's scrollbar is only rendered if `last_scroll_maxes[panel] > 0`, indicating content exceeds the viewport. When content fits entirely, the scrollbar is omitted (not rendered as empty). This reduces visual clutter and is consistent with typical TUI scrollbar patterns.

## Out of scope

- Keyboard-based scroll commands (arrow keys, Page Up/Down) mapped to specific panels. Currently only mouse wheel scroll is routed per-panel. Keyboard scroll commands remain as-is (affecting the focused panel or global exchange pane). This can be extended in future work.
- Scrollbar click/drag interaction to jump to a position. Scrollbars are rendered as visual indicators only; they do not respond to mouse clicks to jump scroll position. This would require additional hitbox testing for the scrollbar widget itself and is deferred to future work.
- Smooth/animated scrolling. Scroll changes are applied immediately (one line per wheel event). Smooth/animated scrolling is out of scope and can be added in future work if desired.
- Persistent scroll position across runs or sessions. Scroll state is stored in `App` during the session and is reset when the app is restarted or a run is closed. Persisting scroll to disk is out of scope.
- Rendering scrollbars in modal overlays (file browser, settings, command palette). Modal overlays currently do not support scrollable content; they are small and fit within the terminal. Scrolling in modals is out of scope.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
