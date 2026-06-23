# Scope — Plan 0032

> Convert the single read-only plan-detail pane into a tabbed interface where each plan tab displays SCOPE, ARCHITECTURE, TASKS, and STATUS as independently expandable accordion sections, improving UX for parallel plan comparison.

## Why this plan

**1. The current plan detail pane shows only one plan at a time, limiting multi-plan comparison.** `crates/makina/src/app.rs:1012` holds `plan_detail: Option<usize>`, a singleton pointer to the focused plan. `crates/makina/src/ui.rs:1293–1377` (`render_plan_detail`) renders the plan's slug, dir, and task list into a fixed pane. When a user wants to view two plans side-by-side — e.g., comparing task counts or status across related plans — they must toggle back and forth, losing context with each switch. Tabbed viewing (the standard browser/editor pattern) allows multiple plans to remain open and clickable, reducing context-switch cost.

**2. The plan detail pane is read-only and monolithic; users cannot focus on specific sections.** Today the pane renders everything (slug, dir, full task list with GATED counts and dependency chains) in one scrollable view. If a user wants to focus on, e.g., just the task list, or just the STATUS, they must scroll past unrelated content. Accordion sections (SCOPE, ARCHITECTURE, TASKS, STATUS) — each togglable with expand/collapse — let users focus on what matters and hide the rest, making long plans easier to navigate.

**3. SCOPE.md and ARCHITECTURE.md content is not yet exposed in the plan pane.** `crates/makina-core/src/orchestrator.rs:212–224` defines `PlanEntry` with `slug`, `dir`, `has_tasks`, and `tasks` (parsed from TASKS.md); there is no `scope_text` or `architecture_text` field yet. The plan detail pane (`render_plan_detail`, line 1301–1377) shows only the task list. Adding full SCOPE/ARCHITECTURE/STATUS views requires both extending `PlanEntry` to cache the file contents and expanding the pane's rendering logic. Accordion sections provide a clean structure: each section owns its own content and expand state, avoiding a monolithic mega-pane.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0004):

- **0001 — Tab-Based Plan Rendering.** Replace the singleton `plan_detail: Option<usize>` model with a tab-based system. Pressing Enter on a discovered plan node opens a new tab (or switches to an existing tab). Render a tab bar above the main content area showing open plan tabs with the active tab highlighted. Implement tab-close keybinding (e.g., Ctrl+W or x button) and tab-switch navigation (Alt+Left/Right or Ctrl+Tab). The sidebar focus and active-tab pointer remain independent so users can navigate the tree freely without losing tabs.
- **0002 — Accordion-Section Layout for Plan Metadata.** Extend `PlanEntry` to cache SCOPE.md and ARCHITECTURE.md content and add an optional STATUS.md field. Render each plan tab as four accordion sections: SCOPE (intro text), ARCHITECTURE (workstream list with indices), TASKS (current list with GATED/dependency markers), and STATUS (completion state). Each section has independent expand/collapse state, persisted in `App` as a nested map `plan_tab_accordion_state: HashMap<String, Set<AccordionSection>>` keyed by plan slug. Implement keybindings to toggle each section (e.g., 's' = scope, 'a' = arch, 't' = tasks, 'z' = status) when focus is on the plan pane. Render section headers as `[+]` (collapsed) or `[-]` (expanded) with a distinct color and the section name.
- **0003 — Tab Navigation and Keybindings.** Consolidate keybindings for plan-tab navigation into a consistent, documented set. Implement Alt+Left/Alt+Right (or Ctrl+Page Up/Ctrl+Page Down) to cycle between open plan tabs. Implement Ctrl+W to close the active plan tab. Display a help tooltip in the plan pane footer listing available keybindings (e.g., "[s] scope  [a] arch  [t] tasks  [z] status  [◄] [►] tabs  [Ctrl+W] close"). Ensure keybindings don't conflict with existing sidebar or exchange-pane bindings.
- **0004 — Integration and Polish.** Integrate all three workstreams (tab-based rendering, accordion sections, keybindings) into the app. Test tab state across plan discovery (ensure accordion state is not lost when plans are re-discovered; clamp the active-tab index if it goes out of bounds). Verify accordion rendering with long text (SCOPE/ARCHITECTURE text wrapping, scrolling). Ensure the tab bar and scrollbar remain visible when sections are tall. Add an integration test that opens two plan tabs, expands different sections in each, navigates between tabs, and verifies section state is preserved per-tab. Update any help text or documentation to mention the new tabbed plan interface.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Single plan detail pane prevents parallel viewing. | `0001` |
| Plan detail pane is monolithic and read-only; accordion sections would enable focus. | `0002` |
| SCOPE.md and ARCHITECTURE.md are not cached in PlanEntry; STATUS is not rendered. | `0002` |

## Locked decisions

- **One tab per plan; pressing Enter on an already-open plan focuses its existing tab — it never duplicates and never closes it.** The `TabContent::Plan { plan_slug: String }` variant identifies a plan tab by its slug (the unique discovery key), and `TabState::open_tab` (`crates/makina/src/app.rs:901`) already dedups by `TabContent` equality: if a tab with the same content is open it sets `active_tab` to that index, otherwise it pushes a new tab. Because a plan tab carries only its slug, opening the same plan again resolves to the same `TabContent` and therefore *focuses the existing tab* (opening it if it was not visible) — exactly the requested "open it if closed, focus it if open" behavior. Closing is reserved for `Ctrl+W`; Enter on a plan node never closes a tab. This reuses the tab contract established for Plan 0031 (proven by the existing `open_existing_tab_switches_to_it` test).
- **Accordion sections default to collapsed; all sections can be expanded independently.** When a plan tab is first opened, all four accordion sections (SCOPE, ARCHITECTURE, TASKS, STATUS) render collapsed (`[+]` marker). Pressing 's', 'a', 't', or 'z' toggles the corresponding section. Expand state is persisted per-tab in `App::accordion_state`, keyed by plan slug. Users can expand any combination of sections (all four, just SCOPE, etc.) without constraint.
- **Accordion state is ephemeral (not persisted to disk); it resets when tabs are closed.** Accordion expand state is held in memory only (`App::accordion_state: HashMap<String, HashSet<AccordionSection>>`). When a plan tab is closed, its accordion state is lost. When the app restarts, tabs are empty and accordion state starts fresh. This reduces persistence complexity and keeps the feature focused on session-local navigation convenience.
- **The sidebar cursor and active-tab pointer remain independent; navigation doesn't close tabs.** Pressing arrow keys in the sidebar moves the `tree_cursor` and focuses tree nodes, but does not automatically open or close tabs. Pressing Enter on a plan/task node opens/switches a tab. Pressing Ctrl+W closes the active tab but keeps the sidebar and tree cursor intact. This orthogonality allows users to navigate the tree freely while maintaining a set of open tabs for reference.
- **Plan tabs route through the existing tab infrastructure from Plan 0031; no new tab mechanism is introduced.** Plan tabs use the same `TabContent::Plan`, `TabState`, and tab-event handling (`OpenTab`, `CloseTab`, `NextTab`, `PrevTab`) already implemented in Plan 0031. This plan only adds accordion sections and SCOPE/ARCHITECTURE/STATUS rendering; it does not introduce a separate plan-tab system.
- **Missing SCOPE/ARCHITECTURE/STATUS files render as '(no <FILE>.md)' placeholders, not errors.** When a plan has no SCOPE.md, ARCHITECTURE.md, or STATUS.md file (or the file fails to read), the corresponding accordion section displays a dim placeholder message. This keeps the pane user-friendly even for incomplete plans and avoids visual errors.

## Out of scope

- Persisting accordion state to disk across app restarts. Accordion state is session-local; persistence would require a new storage format and adds complexity. Users can re-expand sections as needed when reopening a plan.
- Rendering all plan files (README, FUTURE, VISION, ROADMAP, etc.) in the accordion pane. Only SCOPE, ARCHITECTURE, TASKS, and STATUS are core plan metadata. Additional files are out of scope; users can open the plan dir in a file browser if needed.
- Editing SCOPE/ARCHITECTURE/STATUS directly in the accordion pane. The accordion pane is read-only. Editing must be done via an external editor; this plan does not add in-app editing.
- Real-time updates to SCOPE/ARCHITECTURE/STATUS when files change on disk. File caching happens at discovery time. Re-discovery would refresh the cache, but hot-reloading is not implemented in this plan.
- Custom keybindings or keybinding configuration for accordion toggle. Keybindings are hard-coded (s/a/t/z for accordion, Alt+Left/Right for tabs). Customization via config is deferred to a future plan.
- Exporting or copying plan content from the accordion pane. Copy-paste is an OS-level feature (mouse selection + Ctrl+C); the app does not provide special export logic.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
