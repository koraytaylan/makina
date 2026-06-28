# Scope — Plan 0042

> Fix six detail-pane rendering and interaction defects plus add an accordion structure for task details: tab visibility, code-block rendering (syntect highlighting + full-width band + blank-line fix), scrollbar accuracy, the 'v' cycle-views feedback, run controls (start/pause/cancel), and task-detail UX.

## Why this plan

**1. Unfocused tab labels are barely legible due to a color collision.** The unfocused tab style at `crates/makina/src/ui.rs:994–997` applies `Dim` background with `Foreground` text. In ayu_light, `Dim` (#828E9F) used as a *background* sits too close in value to the foreground text (#5C6166), so the label is hard to read. `Dim` is a text-color semantic, not a surface — the unfocused tab needs a true secondary surface (`Border`) for legibility, distinct from the active tab's inverted Accent style.

**2. Code blocks render without syntax highlighting, with a blank line after every source line, and no visible band.** In `crates/makina/src/markup.rs:127–182` the `in_code_block` branch of `Event::Text` (lines 150–159) splits each text event on `'\n'` and pushes a `Line` per element. pulldown-cmark emits one `Event::Text` per source line *including* the trailing `'\n'`, so the trailing empty split element becomes a blank line after every code line (the user's "blank lines between every line"). Every character also uses the single `ThemeRole::CodeBlock` color (no per-token highlighting), and the background is `ThemeRole::Background` — identical to the pane — spanning only the glyph cells, so the block neither stands out nor fills the width.

**3. Scrollbar thumb position is wrong at the end of content.** The scrollbar state at `crates/makina/src/ui.rs:2148` (and the sibling at line 1131) initializes `ScrollbarState::new(total_rendered_rows)` and sets `.position(scroll_offset)`. ratatui's `ScrollbarState::new()` expects the *maximum* scroll offset (`scroll_max = total_rendered_rows - viewport_height`, computed at line 2147), not the total content height. So when `scroll_offset == scroll_max` the thumb floats mid-track and the bar looks as if more content remains below.

**4. The 'v' cycle-dependency-view key appears to do nothing.** `'v'` IS wired: `crates/makina/src/event.rs:1311` maps it to `AppEvent::CycleDependencyView`, the handler at `crates/makina/src/app.rs:2128–2135` cycles `Off → List → Tree → Timeline → Off` (tested at `app.rs:4128–4136`), and the status bar shows `view: …` (`ui.rs:821–826`). The problem is *visibility*: the dependency sub-pane is only carved and drawn in the run/exchange render branch (`ui.rs:711–720`) and only shows content for a focused task with dependencies (`render_dependency_view`, `ui.rs:1148–1205`). From a plan tab, a task tab, or with no task focused, pressing 'v' produces no obvious change, and the binding is undocumented in the help overlay (`ui.rs:3334`).

**5. Start/pause/cancel are unreachable from an opened plan.** Two defects: (a) when a plan tab is focused in `Panel::Main`, `crates/makina/src/event.rs:1332–1338` binds `'s'` to `ToggleAccordionSection(Scope)`, with `StartRun` only in the sidebar `else` branch — so a focused plan tab has NO start key; and (b) `sync_selected_run_to_active_tab` (`crates/makina/src/app.rs:1483–1502`) only syncs `selected_run` for `TabContent::Task` tabs and early-returns for a `TabContent::Plan` tab, so `run_control` (`event.rs:947–964`) finds `selected_run() == None` and returns "No run selected". The user cannot start work for an opened plan, and there is no always-available control affordance (e.g. Ctrl+S).

**6. Task detail tabs lack an accordion with collapsible Scope and Execution sections.** `render_task_entry_pane()` at `crates/makina/src/ui.rs:1831–1913` renders task metadata followed by the markdown entry text in one flat block. The plan accordion (`render_accordion_section()`, `ui.rs:2163`) is a proven pattern (collapsible sections, focus highlighting, keyboard nav). A task should adopt it: "Scope" (static description from TASKS.md) and "Execution" (live activity once work starts — exchanges, progress), mirroring the plan accordion and leveraging the established keybinds.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0006):

- **0001 — Tab-Styling-And-Visibility.** Give unfocused tabs a true secondary-surface background (`Border`) with `Foreground` text, legible across all three themes and visually distinct from the active (inverted Accent) tab.
- **0002 — Code-Block-Rendering-With-Syntax.** Add a dedicated `CodeBlockBg` theme role; add a cached `syntect`/`two-face` highlighter with fence-language detection; render exactly one `Line` per source line (eliminating the spurious blank lines); and paint a full-width `CodeBlockBg` band per code line. Unknown languages degrade to a monochrome `CodeBlock`-colored span.
- **0003 — Scrollbar-Position-Accuracy.** Initialize `ScrollbarState` with `scroll_max` (not `total_rendered_rows`) at every scrollable-pane site so the thumb tracks position accurately, especially at the end of content.
- **0004 — Cycle-Views-Key-Binding.** Make 'v' give immediate, context-independent feedback (a "Dependency view: …" status message), clarify the empty dependency-view state, and document the binding in the help overlay. (The binding and cycle already work; this fixes their *visibility*.)
- **0005 — Plan-Execution-Controls.** Add always-available `Ctrl+S`/`Ctrl+P`/`Ctrl+C` run-control aliases that dispatch from any focus context, and extend `sync_selected_run_to_active_tab` to select a plan tab's run, with an actionable message when no run exists.
- **0006 — Task-Detail-Accordion-Structure.** Adopt an accordion for task detail tabs with collapsible Scope (static description) and Execution (live activity — exchanges/progress, with a clear empty state) sections, reusing `render_accordion_section()` and routing plain `s`/`z` to toggle them (run-control having moved to Ctrl-aliases).

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Unfocused tab labels barely legible (Dim used as a background collides with Foreground text) | `0001` |
| Code blocks: blank line after every source line, no per-token coloring, no visible/full-width band | `0002` |
| Scrollbar thumb wrong at end of content (`ScrollbarState::new` given total rows, not `scroll_max`) | `0003` |
| 'v' cycles state but the effect is invisible outside the run pane and the binding is undocumented | `0004` |
| Start/pause/cancel unreachable from an opened plan ('s' shadowed by accordion; plan tab never selects its run) | `0005` |
| Task detail tabs lack a collapsible Scope/Execution accordion | `0006` |

## Locked decisions

- **Unfocused tab background uses `Border`, not `Dim`.** `Dim` is semantically a text color; using it as a surface conflates two concerns. `Border` is an existing UI-line surface defined in all three themes, giving legible contrast against `Foreground` text while staying subordinate to the active tab's inverted Accent style.
- **Code-block highlighting uses `syntect` via `two-face`; unknown languages fall back to monochrome.** Real per-token coloring is in scope and implemented with the bundled syntect assets (no runtime asset files). The fence info string selects the syntax; a light/dark bundled theme is chosen from the app `Background` luminance. If the language is unknown or highlighting errors, the line renders as a single `ThemeRole::CodeBlock`-colored span. The expensive `SyntaxSet`/`ThemeSet` are cached in `OnceLock`s so the load happens at most once.
- **Code blocks render exactly one `Line` per source line, on a full-width `CodeBlockBg` band.** The blank-line defect is fixed by not emitting a `Line` for the trailing empty `split('\n')` element. A new `CodeBlockBg` theme role (distinct from `Background`) provides the band color, padded to the viewport width.
- **Scrollbar state is initialized with `scroll_max`, not `total_rendered_rows`.** This matches ratatui's `ScrollbarState::new()` contract (argument = maximum scroll offset). `scroll_max = total_rendered_rows.saturating_sub(content_area.height)` is already computed at each site.
- **Run-control is canonically `Ctrl+S`/`Ctrl+P`/`Ctrl+C`, always available.** Decoupling start/pause/cancel from the overloaded plain letters resolves the conflict with the plan and task accordion toggles (which keep plain `s`/`z`). The plain-`'s'` → `StartRun` sidebar fallback may remain, but Ctrl+S is the reliable path from any tab. When `selected_run` is `None`, an actionable status message is shown rather than a silent no-op.
- **The 'v' fix is feedback + documentation, not new dispatch.** The binding and cycle already work and are tested; the plan only adds visible feedback (status message), a clearer empty state, and a help-overlay entry — it does not rewrite the dependency-view dispatch.
- **Task-detail accordion reuses `render_accordion_section()` and adds an `Execution` `AccordionSection` variant.** Task accordion state lives in a new `App::task_accordion_expanded` map; the Execution section is populated from the task's existing `exchange_logs`/progress (with an empty-state hint) rather than introducing a new data source.

## Out of scope

- **User-configurable / custom highlight color schemes.** This plan ships syntect highlighting with bundled themes selected by light/dark. Letting users pick or define their own highlight theme (or mapping syntect scopes onto the app's `ThemeRole` palette token-by-token) is deferred.
- **Languages beyond the bundled syntect/two-face set.** Languages not present in the bundled `SyntaxSet` degrade to monochrome; adding extra syntax definitions is deferred.
- **Streaming/auto-scrolling Execution telemetry.** The Execution section summarizes the task's existing exchanges/progress on render. Live auto-follow of streaming output, per-section independent scrolling, and richer execution timelines are deferred.
- **Persisting accordion expansion state across restarts.** Task accordion state is session-local in-memory state; disk persistence is deferred to future settings work.
- **Advanced tab operations.** Open/switch/close-single tabs remain in scope; batch close, reorder, and saved tab sets are out of scope.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
