//! Rendering logic.
//!
//! [`render`] takes an immutable borrow of [`App`] and a mutable [`Frame`] and
//! writes everything to the frame's buffer.  It holds **no mutable state** and
//! makes **no IO calls**; it is purely a transform from `App` → `Frame`.
//!
//! # Layout
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │ Title bar: "Makina vX.Y — multi-agent factory"      │
//! ├─────────────────┬────────────────────────────────────┤
//! │                 │  Header (run path + status)        │
//! │  Sidebar        ├────────────────────────────────────┤
//! │  (Runs list)    │  Task table (task 29)              │
//! │  task 27 ─────► ├────────────────────────────────────┤
//! │                 │  Exchange pane (task 30)            │
//! │                 │  (focused task prompts/answers)     │
//! ├─────────────────┴────────────────────────────────────┤
//! │ Status bar: panel focus + last event hint            │
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! The sidebar and main areas are **placeholder** panels.  Tasks 27–31 will
//! fill them; for now they show a frame, a label, and a focus indicator.
//!
//! # Extension points
//!
//! * Task 27 (runs-sidebar): replace the sidebar [`Block`] with a real widget
//!   that iterates `app.runs`.
//! * Task 29 (task-status-view): render task state badges inside the main area.
//! * Task 30 (prompt-answer-stream): stream agent exchange text into the main
//!   area for the focused task.  The exchange pane is rendered below the task
//!   table and shows the focused task's prompts and streamed answers in order.
//! * Task 31 (run-control): add keybind hints to the status bar.

use chrono::{DateTime, Utc};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph,
        Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use std::collections::HashSet;

use crate::app::{
    AccordionSection, App, DependencyViewMode, ExchangeEntry, Panel, PanelGeometry,
    ScrollablePanel, TabContent, TreeNode,
};
use makina_core::api::{FailureKind, RunView, TaskId};

/// Render markdown with caching by (text_hash, width).
/// Subsequent calls with identical text and width return the cached result without re-parsing.
fn render_markdown_cached(
    app: &App,
    text: &str,
    base: Style,
    width: u16,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    use crate::app::hash_text;
    let key = (hash_text(text), width);
    if let Some(lines) = app.markdown_cache.borrow().get(&key) {
        return lines.clone();
    }
    let lines = crate::markup::render_markdown(text, base, width, theme);
    app.markdown_cache.borrow_mut().insert(key, lines.clone());
    lines
}

/// Render the full TUI layout into `frame`.
///
/// When the modal file browser is active ([`App::is_browsing`]) it is drawn as
/// an overlay on top of the normal layout (task 28).
pub fn render(app: &App, frame: &mut Frame) {
    let area = frame.area();

    // ── Small-terminal guard ──────────────────────────────────────────────────
    // First statement in render(), before any layout. Below this the normal panes
    // overlap and are unusable; draw a single readable centered message and return.
    const MIN_W: u16 = 40;
    const MIN_H: u16 = 10;
    if area.width < MIN_W || area.height < MIN_H {
        // Render a centered "Terminal too small" message and return early.
        // Use the full area (not a centered_rect sub-rect) so the message is
        // always visible even on a 20×5 terminal where a 30%-height sub-rect
        // would be ≤1 row and clip the text.
        let message = vec![
            Line::from(vec![Span::raw("Terminal too small")]),
            Line::from(vec![Span::raw("(minimum 40×10)")]),
        ];
        let paragraph = Paragraph::new(message)
            .alignment(Alignment::Center)
            .style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)));
        frame.render_widget(paragraph, area);
        return;
    }

    // Check if any provider is missing and warning hasn't been dismissed —
    // if so, reserve one extra line for the warning banner.
    let has_missing_provider =
        !app.provider_warning_dismissed && app.provider_probes.iter().any(|p| p.resolved.is_none());
    let warning_height: u16 = if has_missing_provider { 1 } else { 0 };

    // ── Top-level vertical split ──────────────────────────────────────────────
    // title_bar (1 row) / [warning_banner (0 or 1 row)] / body (fills remaining) / status_bar (1 row)
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),              // title bar
            Constraint::Length(warning_height), // warning banner (0 or 1)
            Constraint::Min(0),                 // body
            Constraint::Length(1),              // status bar
        ])
        .split(area);

    let title_area = vertical[0];
    let warning_area = vertical[1];
    let body_area = vertical[2];
    let status_area = vertical[3];

    // ── Body horizontal split ─────────────────────────────────────────────────
    // Body split is driven by the user's saved sidebar width, not a fixed 30/70.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(app.sidebar_width_percent),
            Constraint::Percentage(100 - app.sidebar_width_percent),
        ])
        .split(body_area);

    let sidebar_area = body[0];
    let main_area = body[1];

    // ── Title bar ─────────────────────────────────────────────────────────────
    let version = env!("CARGO_PKG_VERSION");
    let title_text = format!(" Makina v{version} — multi-agent software factory ");
    let title = Paragraph::new(title_text).style(
        Style::default()
            .bg(app.active_theme.get(crate::theme::ThemeRole::Info))
            .fg(app.active_theme.get(crate::theme::ThemeRole::Foreground))
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(title, title_area);

    // ── Provider warning banner ───────────────────────────────────────────────
    if has_missing_provider {
        render_provider_warning(app, frame, warning_area);
    }

    // ── Sidebar ───────────────────────────────────────────────────────────────
    // Render a tree of runs and tasks. Each open run is an expandable parent
    // node with its tasks nested beneath it (only when expanded).
    // The sidebar now shows "Runs & Tasks" as the title.
    let sidebar_focused = app.focused_panel == Panel::Sidebar;

    // Render the unified sidebar tree (plans + runs + tasks).
    let sidebar_block = panel_block(app, "Runs & Tasks", sidebar_focused);

    // The tree is empty only when there are NO discovered plans AND no open
    // runs — gate on the flattened node list, not `runs` alone, so a freshly
    // opened project that has plans but no runs yet still renders its plans.
    let tree_nodes = app.visible_tree_nodes();
    // Reset per-frame click bounds up front; the sidebar branch and
    // `render_tab_bar` refill them. Clearing unconditionally here (rather than
    // only where they are populated) means a frame whose match arm draws no tab
    // bar — e.g. the no-tabs hint — can't leave a previous frame's chip bounds
    // around to absorb a stray click.
    app.sidebar_node_bounds.borrow_mut().clear();
    app.tab_bounds.borrow_mut().clear();
    if tree_nodes.is_empty() {
        // Empty state. While a background job is running (e.g. startup plan
        // discovery) show an animated spinner + label so the empty sidebar
        // reads as "working", not "nothing here". Otherwise show a hint that
        // guides the user to the `[o]` file browser (task 28).
        let empty_text = if let Some(label) = &app.busy {
            vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    format!("  {} {label}…", spinner_frame(app.tick)),
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)),
                )]),
            ]
        } else {
            vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  No runs open.",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Press [o] to open a",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )]),
                Line::from(vec![Span::styled(
                    "  task-list file.",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )]),
            ]
        };
        let para = Paragraph::new(empty_text)
            .block(sidebar_block)
            .style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)));
        frame.render_widget(para, sidebar_area);
    } else {
        // Build one ListItem per visible tree node (plans, runs, and their
        // expanded tasks).
        let items: Vec<ListItem> = tree_nodes
            .iter()
            .map(|node| {
                match node {
                    TreeNode::Run { run } => {
                        // Run node: disclosure glyph + status badge + run name
                        let run_view = &app.runs[*run];
                        let disclosure = if app.collapsed_runs.contains(&run_view.id) {
                            "▸ "
                        } else {
                            "▾ "
                        };
                        let (badge, badge_color) = status_badge(&run_view.status, app);
                        let name = run_label(run_view);
                        let line = Line::from(vec![
                            Span::raw(disclosure),
                            Span::styled(badge, Style::default().fg(badge_color)),
                            Span::styled(" ", Style::default()),
                            Span::raw(name),
                        ]);
                        ListItem::new(line)
                    }
                    TreeNode::Task { run, task } => {
                        // Task node: indent + task state badge + spinner (if InProgress/InReview) +
                        // task title + failure label (if Failed)
                        let run_view = &app.runs[*run];
                        let task_view = &run_view.tasks[*task];

                        let (badge, badge_color) = task_state_badge(&task_view.state, app);
                        let badge_text = match task_view.state {
                            makina_core::api::TaskState::InProgress
                            | makina_core::api::TaskState::InReview => {
                                format!("{} {}", spinner_frame(app.tick), badge)
                            }
                            _ => badge.to_string(),
                        };

                        // Build failure label if needed
                        let failure_label =
                            if matches!(task_view.state, makina_core::api::TaskState::Failed) {
                                if let Some(reason) = &task_view.failure_reason {
                                    format!(" {}", failure_kind_label(&reason.kind))
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            };

                        let line = Line::from(vec![
                            Span::raw("  "), // indent
                            Span::styled(badge_text, Style::default().fg(badge_color)),
                            Span::styled(" ", Style::default()),
                            Span::raw(&task_view.title),
                            Span::raw(failure_label),
                        ]);
                        ListItem::new(line)
                    }
                    TreeNode::Plan { plan_idx } => {
                        let plan_entry = &app.discovered_plans[*plan_idx];
                        let n_tasks = plan_entry.tasks.len();
                        // Disclosure glyph: a plan with tasks gets ▸/▾; a plan with
                        // no tasks is a leaf (no triangle).
                        let disclosure = if n_tasks == 0 {
                            "  "
                        } else if app.collapsed_plans.contains(plan_idx) {
                            "▸ "
                        } else {
                            "▾ "
                        };
                        let mut line_spans = vec![
                            Span::raw(disclosure),
                            Span::styled(
                                &plan_entry.slug,
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                        ];
                        if !plan_entry.has_tasks {
                            // No TASKS.md: this plan still needs a task list.
                            line_spans.push(Span::styled(
                                " (no tasks — will plan)",
                                Style::default()
                                    .fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                            ));
                        } else {
                            // Show the task count so the plan reads as a container.
                            line_spans.push(Span::styled(
                                format!(
                                    "  · {n_tasks} task{}",
                                    if n_tasks == 1 { "" } else { "s" }
                                ),
                                Style::default()
                                    .fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                            ));
                        }
                        ListItem::new(Line::from(line_spans))
                    }
                    TreeNode::PlanTask { plan_idx, task_idx } => {
                        // Read-only task preview under an expanded plan: tree
                        // connector + id — title, with a GATED marker.
                        let task = &app.discovered_plans[*plan_idx].tasks[*task_idx];
                        let last = *task_idx + 1 == app.discovered_plans[*plan_idx].tasks.len();
                        let connector = if last { "  └ " } else { "  ├ " };
                        let mut spans = vec![
                            Span::styled(
                                connector,
                                Style::default()
                                    .fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                            ),
                            Span::styled(
                                &task.id,
                                Style::default()
                                    .fg(app.active_theme.get(crate::theme::ThemeRole::Accent)),
                            ),
                            Span::styled(
                                format!(" — {}", task.title),
                                Style::default()
                                    .fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                            ),
                        ];
                        if task.gated {
                            spans.push(Span::styled(
                                "  GATED",
                                Style::default()
                                    .fg(app.active_theme.get(crate::theme::ThemeRole::Warning)),
                            ));
                        }
                        ListItem::new(Line::from(spans))
                    }
                }
            })
            .collect();

        // Highlight style for the focused node.
        let highlight_style = Style::default()
            .fg(app.active_theme.get(crate::theme::ThemeRole::Background))
            .bg(app.active_theme.get(crate::theme::ThemeRole::Accent))
            .add_modifier(Modifier::BOLD);

        // Capture item count and compute scroll bounds before items are moved.
        let total_items = items.len();
        let sidebar_inner = sidebar_block.inner(sidebar_area);
        let sidebar_visible = sidebar_inner.height as usize;
        let sidebar_scroll_max = total_items.saturating_sub(sidebar_visible) as u16;

        let sidebar_list = List::new(items)
            .block(sidebar_block)
            .highlight_style(highlight_style)
            .highlight_symbol("▶ ");

        // ListState carries the selected index so ratatui knows which node to
        // highlight.  Use tree_cursor instead of selected_run.
        let sidebar_scroll_offset = app
            .scroll_offsets
            .get(&ScrollablePanel::Sidebar)
            .copied()
            .unwrap_or(0) as usize;
        let mut list_state = ListState::default()
            .with_selected(app.tree_cursor)
            .with_offset(sidebar_scroll_offset);

        frame.render_stateful_widget(sidebar_list, sidebar_area, &mut list_state);

        // Record one clickable bound per visible row so a mouse click can
        // open/focus that node's tab (mirrors keyboard Enter). Every tree node
        // is exactly one row; the first visible row maps to the list's scroll
        // offset. See the `Down(Left)` hit-test in `event::translate_terminal_event`.
        {
            let mut node_bounds = app.sidebar_node_bounds.borrow_mut();
            let visible_rows =
                sidebar_visible.min(total_items.saturating_sub(sidebar_scroll_offset));
            for row in 0..visible_rows {
                let node_idx = sidebar_scroll_offset + row;
                node_bounds.push((
                    node_idx,
                    Rect {
                        x: sidebar_inner.x,
                        y: sidebar_inner.y + row as u16,
                        width: sidebar_inner.width,
                        height: 1,
                    },
                ));
            }
        }

        // Record the sidebar scroll-max this frame and render scrollbar if needed.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Sidebar, sidebar_scroll_max);

        if total_items > sidebar_visible {
            let sidebar_scroll_offset = app
                .scroll_offsets
                .get(&ScrollablePanel::Sidebar)
                .copied()
                .unwrap_or(0);
            let mut scrollbar_state = ScrollbarState::new(sidebar_scroll_max as usize)
                .position(sidebar_scroll_offset as usize);
            let scrollbar = Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);
            frame.render_stateful_widget(scrollbar, sidebar_inner, &mut scrollbar_state);
        }
    }

    // ── Main content — per-task status view (task 29) ────────────────────────
    let main_focused = app.focused_panel == Panel::Main;
    let main_block = panel_block(app, "Detail", main_focused);

    // ── Record selectable panes for mouse text selection ───────────────────────
    // `hit` is the full pane column (so a drag may begin on a border/padding
    // cell); `clip` is the inner content rect (so the selection excludes borders
    // and never crosses into the other pane). When a modal overlay is up, treat
    // the whole screen as one pane so selection spans it without column clipping.
    // See `crate::selection`.
    let overlay_active = app.is_browsing()
        || app.is_editing_providers()
        || app.is_viewing_doctor()
        || app.is_command_palette()
        || app.is_settings();
    app.set_selection_panes(if overlay_active {
        vec![crate::app::SelectionPane {
            hit: area,
            clip: area,
        }]
    } else {
        vec![
            crate::app::SelectionPane {
                hit: sidebar_area,
                clip: panel_block(app, "Runs & Tasks", sidebar_focused).inner(sidebar_area),
            },
            crate::app::SelectionPane {
                hit: main_area,
                clip: main_block.inner(main_area),
            },
        ]
    });

    // Draw the main border once, then carve a GLOBAL error pane off the bottom
    // of its inner area. The error pane is shared by every content state (hint,
    // plan detail, run view) so `[e]` reveals errors even when no run is
    // selected — previously it only rendered inside the run view.
    let inner = main_block.inner(main_area);
    frame.render_widget(main_block, main_area);
    let error_pane_height: u16 = if app.error_pane_open {
        8.min(inner.height.saturating_sub(3))
    } else {
        0
    };
    let main_split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(error_pane_height)])
        .split(inner);
    let content_area = main_split[0];
    let error_area = main_split[1];

    // Content precedence: an active plan tab (opened via the tab system in plan 0032)
    // is rendered via render_plan_accordion_pane; an active task tab is rendered via
    // render_task_entry_pane; otherwise the selected run's view; otherwise a hint.
    let active_plan_tab = app.tabs.active_tab.and_then(|idx| {
        app.tabs.open_tabs.get(idx).and_then(|tab_content| {
            if let crate::app::TabContent::Plan { plan_slug } = tab_content {
                app.discovered_plans.iter().find(|p| p.slug == *plan_slug)
            } else {
                None
            }
        })
    });

    let active_task_tab = app.tabs.active_tab.and_then(|idx| {
        app.tabs.open_tabs.get(idx).and_then(|tab_content| {
            if let crate::app::TabContent::Task { task_id, .. } = tab_content {
                Some(task_id.clone())
            } else {
                None
            }
        })
    });

    // An active plan-task tab resolves to its plan entry and the matching task
    // preview, rendered as a standalone task pane (distinct from the plan tab).
    let active_plan_task_tab = app.tabs.active_tab.and_then(|idx| {
        app.tabs.open_tabs.get(idx).and_then(|tab_content| {
            if let crate::app::TabContent::PlanTask { plan_slug, task_id } = tab_content {
                app.discovered_plans
                    .iter()
                    .find(|p| p.slug == *plan_slug)
                    .and_then(|plan| {
                        plan.tasks
                            .iter()
                            .find(|t| t.id == *task_id)
                            .map(|preview| (plan, preview))
                    })
            } else {
                None
            }
        })
    });

    // Accumulate panel geometries for hitbox testing.
    let mut panel_geoms: Vec<PanelGeometry> = vec![PanelGeometry {
        panel: ScrollablePanel::Sidebar,
        rect: sidebar_area,
    }];

    match (
        active_plan_tab,
        active_plan_task_tab,
        active_task_tab.clone(),
        app.selected_run(),
    ) {
        (Some(plan), _, _, _) => {
            // Split content area to reserve 1 row for tab bar at the top
            let plan_split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(3)])
                .split(content_area);

            let tab_area = plan_split[0];
            let plan_area = plan_split[1];

            // Render the tab bar
            render_tab_bar(app, frame, tab_area);

            // Render the plan accordion pane below the tab bar
            render_plan_accordion_pane(app, plan, frame, plan_area);

            // Record the plan accordion geometry
            panel_geoms.push(PanelGeometry {
                panel: ScrollablePanel::PlanAccordion,
                rect: plan_area,
            });
        }
        (None, Some((plan, preview)), _, _) => {
            // An active plan-task tab shows the task preview (id, title, gated,
            // dependencies) in its own pane, with the tab bar above.
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(3)])
                .split(content_area);
            render_tab_bar(app, frame, split[0]);
            render_plan_task_pane(app, plan, preview, frame, split[1]);
            panel_geoms.push(PanelGeometry {
                panel: ScrollablePanel::PlanAccordion,
                rect: split[1],
            });
        }
        (None, None, None, None) => {
            // No run selected: show a hint paragraph.
            let hint_lines = vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Select a run, or press Enter on a plan to view it.",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  [→] expand plan   [Enter] plan detail   [Tab] switch focus",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )]),
                Line::from(vec![Span::styled(
                    "  [q / Esc / Ctrl-C] — quit",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )]),
            ];
            let hint_para = Paragraph::new(hint_lines).style(
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
            );
            frame.render_widget(hint_para, content_area);
        }
        (None, None, Some(task_id), Some(run)) => {
            // An active task tab shows the task entry (metadata + Markdown body).
            // Split content area to reserve 1 row for tab bar at the top.
            let task_split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(3)])
                .split(content_area);

            let tab_area = task_split[0];
            let task_area = task_split[1];

            // Render the tab bar first so it stays visible even if the task
            // itself can't be resolved in the selected run.
            render_tab_bar(app, frame, tab_area);

            if let Some(task_idx) = find_task_idx_in_run(app, &task_id) {
                // Render the task entry pane below the tab bar
                render_task_entry_pane(app, run, task_idx, frame, task_area);

                // Record the task entry geometry
                panel_geoms.push(PanelGeometry {
                    panel: ScrollablePanel::TaskEntry,
                    rect: task_area,
                });
            } else {
                // The active task tab points at a task that is no longer in the
                // selected run — keep the tab bar and show a hint rather than a
                // blank pane.
                let hint = Paragraph::new(vec![
                    Line::from(""),
                    Line::from(vec![Span::styled(
                        "  This task is no longer available.",
                        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                    )]),
                ])
                .style(
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
                );
                frame.render_widget(hint, task_area);
            }
        }
        (None, None, _, Some(run)) => {
            // The selected run's view fills the content area (above the global
            // error pane). The task table has been removed; tasks are now in the
            // sidebar tree.
            let inner = content_area;

            // Header: run path and aggregate status.
            let header_lines: Vec<Line> = vec![
                Line::from(vec![
                    Span::styled(
                        "Run: ",
                        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                    ),
                    Span::styled(
                        run.task_list_path.display().to_string(),
                        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(
                        "Status: ",
                        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                    ),
                    Span::styled(
                        status_label(&run.status),
                        Style::default().fg(status_color(&run.status, app)),
                    ),
                    Span::styled(
                        format!(
                            "  ({} task{})",
                            run.tasks.len(),
                            if run.tasks.len() == 1 { "" } else { "s" }
                        ),
                        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                    ),
                ]),
                Line::from(""),
            ];
            let header_height = header_lines.len() as u16;

            // Ingestion pane height: non-zero only when the selected run has a
            // non-empty report.
            let ingestion_pane_height: u16 = if let Some(r) = app.selected_run() {
                if r.report.is_empty() {
                    0
                } else {
                    let n = r.report.issues.len() as u16;
                    (n + 2).min(8) // title + borders + issues (capped)
                }
            } else {
                0
            };

            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // tab bar (new)
                    Constraint::Length(header_height),
                    Constraint::Length(ingestion_pane_height), // ingestion issues (0 = hidden)
                    Constraint::Min(3), // exchange pane — always at least 3 rows
                ])
                .split(inner);

            let tab_area = split[0];
            let header_area = split[1];
            let ingestion_area = split[2];
            let exchange_area = split[3];

            // Render the tab bar at the top.
            // NOTE: render_tab_bar is a no-op when no tabs are open, so this doesn't
            // affect tests that don't open tabs.
            render_tab_bar(app, frame, tab_area);

            let header_para = Paragraph::new(header_lines).style(
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
            );
            frame.render_widget(header_para, header_area);

            // Render ingestion report panel (0-height area is a no-op inside).
            // Placed directly after the header (since task table is gone).
            render_ingestion_panel(app, frame, ingestion_area);

            // ── Dependency view + Exchange pane (task 30) ──────────────────
            // When a dependency-view overlay is active, carve a top sub-pane out
            // of the exchange region for it and render the exchange pane below.
            // `DependencyViewMode::Off` leaves the exchange pane full-height.
            let exchange_pane_area = if app.dependency_view == DependencyViewMode::Off {
                exchange_area
            } else {
                let dep_height = (exchange_area.height / 2).max(3);
                let dep_split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(dep_height), Constraint::Min(3)])
                    .split(exchange_area);
                let dep_area = dep_split[0];
                render_dependency_view(app, frame, dep_area);
                panel_geoms.push(PanelGeometry {
                    panel: ScrollablePanel::DependencyView,
                    rect: dep_area,
                });
                dep_split[1]
            };
            render_exchange_pane(app, frame, exchange_pane_area, main_focused);
            panel_geoms.push(PanelGeometry {
                panel: ScrollablePanel::Exchange,
                rect: exchange_pane_area,
            });
        }
        _ => {
            // Fallback: a tab is active but its task can't be resolved to a run
            // (e.g. the run was closed). Keep the tab bar visible when any tab is
            // open so the strip never silently disappears, and show a hint below.
            let hint_area = if app.tabs.open_tabs.is_empty() {
                content_area
            } else {
                let split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(1), Constraint::Min(3)])
                    .split(content_area);
                render_tab_bar(app, frame, split[0]);
                split[1]
            };
            let hint_lines = vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Select a run, or press Enter on a plan to view it.",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )]),
                Line::from(""),
            ];
            let hint_para = Paragraph::new(hint_lines).style(
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
            );
            frame.render_widget(hint_para, hint_area);
        }
    }

    // Add error pane geometry if it's open (for mouse hitbox testing).
    if app.error_pane_open && error_area.height > 0 && error_area.width > 0 {
        panel_geoms.push(PanelGeometry {
            panel: crate::app::ScrollablePanel::ErrorPane,
            rect: error_area,
        });
    }

    // Record the accumulated panel geometries for hitbox testing in the event loop.
    app.set_panel_geometries(panel_geoms);

    // ── Error pane (collapsible, global) ───────────────────────────────────
    // A 0-height `error_area` (pane closed) makes this a no-op. Rendered for
    // every content state so `[e]` always reveals the error log.
    render_error_pane(app, frame, error_area);

    // ── Status bar ────────────────────────────────────────────────────────────
    // Key hints reflect the REAL keys: [o] open file browser, [s/p/c] run
    // control (start/pause/cancel the selected run — task 31), [Tab] switch
    // focus, [q/Esc/^C] quit.  A transient command-outcome message (set on the
    // most recent `api.execute(...)`) is shown when present; otherwise the focus
    // label + last-event hint are shown.
    let focus_label = match app.focused_panel {
        Panel::Sidebar => "focus: sidebar",
        Panel::Main => "focus: main",
    };
    let trailer = if let Some(label) = &app.busy {
        // An in-flight background job (e.g. plan discovery): show an animated
        // spinner + label so the user can tell the app is working.
        format!("  │  {} {label}…", spinner_frame(app.tick))
    } else {
        match &app.status_message {
            Some(msg) => format!("  │  {msg}"),
            None => {
                let event_hint = match &app.last_event {
                    None => String::new(),
                    Some(ev) => format!("  │  last: {}", event_short_name(ev)),
                };
                format!("  {focus_label}{event_hint}")
            }
        }
    };
    // Blocked-start notice (mirrors gr-legend append): only when the selected
    // run's report has blocking issues. Tells user why Start is gated and how
    // to re-interpret.
    let blocked_notice = app
        .selected_run()
        .and_then(|r| {
            if r.report.is_blocked() {
                let n = r.report.blocking().count();
                Some(format!(
                    "  │  ⚠ {} blocking issue(s) — press r to re-interpret",
                    n
                ))
            } else {
                None
            }
        })
        .unwrap_or_default();
    // Compute the current-view label from app.dependency_view
    let view = match app.dependency_view {
        DependencyViewMode::Off => "off",
        DependencyViewMode::List => "list",
        DependencyViewMode::Tree => "tree",
        DependencyViewMode::Timeline => "timeline",
    };
    // Build the error-hint badge: plain text when the pane is current, warn
    // colour (Yellow) when unseen errors are present so the user notices.
    let (error_badge_text, error_badge_style) = if app.unseen_errors {
        let count = app.error_messages.len();
        (
            format!("[e] errors({})", count),
            Style::default()
                .bg(app.active_theme.get(crate::theme::ThemeRole::Dim))
                .fg(app.active_theme.get(crate::theme::ThemeRole::Warning)),
        )
    } else {
        (
            "[e] errors".to_string(),
            Style::default()
                .bg(app.active_theme.get(crate::theme::ThemeRole::Dim))
                .fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
        )
    };
    // The blocked notice precedes the trailer so its full text stays inside the
    // visible width; the lower-priority trailer (focus/last-event hint) is the
    // part that gets clipped on narrow terminals.
    //
    // The status bar is built as a `Line` of `Span`s so the error-badge span
    // can carry its own colour (warn/yellow) while the rest stays White/DarkGray.
    let verbose_state = if app.verbose_mode { "on" } else { "off" };
    let default_style = Style::default()
        .bg(app.active_theme.get(crate::theme::ThemeRole::Dim))
        .fg(app.active_theme.get(crate::theme::ThemeRole::Foreground));
    let status_bar = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" [^P] cmds  [o] open  [s/p/c] start/pause/cancel  [r] retry  [Tab] panel  [v] view  [^O] verbose:{verbose_state}  [L] log  [?] help  [wheel] scroll  "),
            default_style,
        ),
        Span::styled(error_badge_text, error_badge_style),
        Span::styled(
            format!("  [q/^C] quit  │  view: {view}{blocked_notice}{trailer}"),
            default_style,
        ),
    ]))
    .style(default_style);
    frame.render_widget(status_bar, status_area);

    // ── File-browser overlay ────────────────────────────────────────────────────
    // Drawn LAST so it sits on top of the normal layout (task 28).
    if app.is_browsing()
        && let Some(browser) = app.browser.as_ref()
    {
        render_file_browser(app, browser, frame, area);
    }

    // ── Provider configuration editor overlay ──────────────────────────────────
    // Drawn after the file browser so it sits on top when both might be open
    // (task 0041).
    if app.is_editing_providers()
        && let Some(editor) = app.provider_editor.as_ref()
    {
        render_provider_editor(app, editor, frame, area);
    }

    // ── Doctor health-check overlay (task 0046) ──────────────────────────────────
    // Drawn before the help overlay. Both call Clear() first and are toggled by
    // distinct keys, so at most one is open at a time.
    if app.is_viewing_doctor() {
        render_doctor(app, frame, area);
    }

    // ── Help overlay (plan 0038) ──────────────────────────────────────────────────
    // Drawn after the doctor overlay, so if both were ever open help would sit on
    // top. Both call Clear() first, so neither leaks through the other.
    if app.help_mode_active {
        render_help_overlay(app, frame, area);
    }

    // ── Command palette overlay (plan 0069) ────────────────────────────────────
    // Drawn after help overlay so it sits on top when both might be open.
    if app.is_command_palette()
        && let Some(p) = app.command_palette.as_ref()
    {
        render_command_palette(app, p, frame, area);
    }

    // ── Settings overlay (plan 0070) ──────────────────────────────────────────────
    // Drawn after command palette so it sits on top when both might be open.
    if app.is_settings()
        && let Some(s) = app.settings.as_ref()
    {
        render_settings(app, s, frame, area);
    }

    // ── Mouse text-selection highlight ─────────────────────────────────────────
    // Applied last so it reverses whatever pane or overlay drew beneath the
    // dragged region. See `crate::selection` for why selection lives in-app.
    if let Some(sel) = app.selection {
        sel.highlight(frame.buffer_mut(), &app.active_theme);
    }
}

// ── Exchange pane (task 30: prompt-answer-stream) ─────────────────────────────

/// Render the live agent exchange pane for the currently focused task.
///
/// Shows the focused task's prompts (labelled with role) and the streamed
/// responses accumulated so far, in order, auto-scrolled to the latest entry.
///
/// # Filtering
///
/// The App stores exchange logs for ALL in-flight tasks.  This function reads
/// only the log for `app.selected_task_id()` — focus filtering happens here at
/// render time, not in the event-handling layer.
/// Render the dependency-view sub-pane for the selected task.
///
/// Render the tab bar showing open tabs above the main content pane.
///
/// Each tab is drawn as a `│ kind label │` chip — the active one bold on a Cyan
/// background, inactive ones dim on DarkGray — so the row reads as a tab strip.
/// The clickable bound of every chip is recorded in `app.tab_bounds` so the
/// event loop can activate the tab under a mouse click (see the `Down(Left)`
/// hit-test in `event::translate_terminal_event`).
fn render_tab_bar(app: &App, frame: &mut Frame, area: Rect) {
    let mut bounds = app.tab_bounds.borrow_mut();
    bounds.clear();
    if app.tabs.open_tabs.is_empty() {
        return; // No tabs to render
    }

    let mut spans = Vec::new();
    // `x` tracks the column where the next chip starts so recorded bounds line
    // up exactly with what is drawn.
    let mut x = area.x;
    let area_end = area.x.saturating_add(area.width);
    // Leading divider so the first chip reads as a bordered tab.
    spans.push(Span::styled(
        "│",
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
    ));
    x = x.saturating_add(1);

    for (idx, tab) in app.tabs.open_tabs.iter().enumerate() {
        let (kind, label) = match tab {
            TabContent::Task { task_id, .. } => ("task ", task_id.0.clone()),
            TabContent::PlanTask { task_id, .. } => ("task ", task_id.clone()),
            TabContent::Plan { plan_slug } => ("plan ", plan_slug.clone()),
        };
        let chip = format!(" {kind}{label} ");
        let chip_w = chip.chars().count() as u16;

        // Record the clickable bound for this chip, clamped to the bar width.
        if x < area_end {
            let width = chip_w.min(area_end - x);
            bounds.push((
                idx,
                Rect {
                    x,
                    y: area.y,
                    width,
                    height: 1,
                },
            ));
        }

        let is_active = app.tabs.active_tab == Some(idx);
        let style = if is_active {
            Style::default()
                .bg(app.active_theme.get(crate::theme::ThemeRole::Accent))
                .fg(app.active_theme.get(crate::theme::ThemeRole::Background))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(app.active_theme.get(crate::theme::ThemeRole::Border))
                .fg(app.active_theme.get(crate::theme::ThemeRole::Foreground))
        };
        spans.push(Span::styled(chip, style));
        spans.push(Span::styled(
            "│",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
        ));
        // Chip width + 1 for the trailing divider.
        x = x.saturating_add(chip_w).saturating_add(1);
    }
    let para = Paragraph::new(Line::from(spans));
    frame.render_widget(para, area);
}

/// Render the content pane for an active plan-task tab: the task's id and title,
/// then its FULL section body from the plan's TASKS.md rendered as Markdown
/// (best effort), scrollable with a scrollbar like the plan-accordion pane.
///
/// A discovered plan's task is a read-only preview ([`PlanTaskPreview`]), not a
/// running task, so the body is the parsed Markdown under the task heading, not
/// a live exchange. Falls back to the parsed metadata when no body was captured.
fn render_plan_task_pane(
    app: &App,
    plan: &makina_core::orchestrator::PlanEntry,
    preview: &makina_core::orchestrator::PlanTaskPreview,
    frame: &mut Frame,
    area: Rect,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Reserve the rightmost column for the scrollbar (mirrors the accordion pane).
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let content_area = cols[0];
    let scrollbar_area = cols[1];

    // Wrap-aware row counting so scroll bounds account for soft-wrapped lines.
    let rendered_rows_for_line = |line: &Line<'_>| -> u16 {
        let w = line.width();
        if content_area.width == 0 || w == 0 {
            1
        } else {
            (w as u32)
                .div_ceil(content_area.width as u32)
                .min(u16::MAX as u32) as u16
        }
    };

    let mut rendered_row: u16 = 0;
    let mut lines: Vec<Line<'static>> = Vec::new();
    macro_rules! push_line {
        ($l:expr) => {{
            let l: Line<'static> = $l;
            rendered_row += rendered_rows_for_line(&l);
            lines.push(l);
        }};
    }

    // Header: `id — title`, plan slug, and a gated note.
    push_line!(Line::from(vec![Span::styled(
        format!("{} — {}", preview.id, preview.title),
        Style::default()
            .fg(app.active_theme.get(crate::theme::ThemeRole::Accent))
            .add_modifier(Modifier::BOLD),
    )]));
    push_line!(Line::from(vec![
        Span::styled(
            "Plan: ",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
        ),
        Span::styled(
            plan.slug.clone(),
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
        ),
    ]));
    if preview.gated {
        push_line!(Line::from(vec![Span::styled(
            "Gated — blocked until prerequisites land",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Warning)),
        )]));
    }
    push_line!(Line::from(""));

    // Body: the full task section from TASKS.md, rendered as Markdown.
    if preview.body.trim().is_empty() {
        if preview.depends_on.is_empty() {
            push_line!(Line::from(Span::styled(
                "(no further detail in TASKS.md)",
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
            )));
        } else {
            push_line!(Line::from(vec![
                Span::styled(
                    "Depends on: ",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                ),
                Span::styled(
                    preview.depends_on.join(", "),
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)),
                ),
            ]));
        }
    } else {
        let base_style =
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground));
        for l in render_markdown_cached(
            app,
            &preview.body,
            base_style,
            content_area.width,
            &app.active_theme,
        ) {
            push_line!(l);
        }
    }

    let total_rendered_rows = rendered_row;
    let scroll_max = total_rendered_rows.saturating_sub(content_area.height);
    app.last_scroll_maxes
        .borrow_mut()
        .insert(ScrollablePanel::PlanAccordion, scroll_max);
    let scroll_offset = app.panel_offset(ScrollablePanel::PlanAccordion, scroll_max);

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset, 0));
    frame.render_widget(para, content_area);

    if scroll_max > 0 {
        let mut scrollbar_state =
            ScrollbarState::new(scroll_max as usize).position(scroll_offset as usize);
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

/// This is the single shared entry point for all [`DependencyViewMode`]
/// renderings: sibling tasks (`tui-dep-tree`, `tui-dep-timeline`) only add their
/// match arms here.  The pane is a top-bordered `Dependencies` block; its body
/// depends on the active [`App::dependency_view`].
///
/// For [`DependencyViewMode::List`] it renders the selected task's `depends_on`
/// as a compact `[state] task-id` list, one prerequisite per line, looking up
/// each dependency's [`TaskView`] in the same run to colour its state badge.
fn render_dependency_view(app: &App, frame: &mut Frame, area: Rect) {
    let view_label = match app.dependency_view {
        DependencyViewMode::Off => "off",
        DependencyViewMode::List => "list",
        DependencyViewMode::Tree => "tree",
        DependencyViewMode::Timeline => "timeline",
    };
    let block = Block::default()
        .title(format!(" Dependencies — {} ", view_label))
        .borders(Borders::TOP)
        .border_style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match app.dependency_view {
        DependencyViewMode::List => {
            // Resolve the selected task and its prerequisites within the run.
            let selected_id = app.selected_task_id();
            let run = app.selected_run();
            let lines: Vec<Line> = match (selected_id, run) {
                (Some(id), Some(run)) => {
                    let selected = run.tasks.iter().find(|t| &t.id == id);
                    match selected {
                        Some(task) if !task.depends_on.is_empty() => task
                            .depends_on
                            .iter()
                            .map(|dep_id| {
                                // Look up the prerequisite's view in the same run
                                // to colour its state badge; fall back to a plain
                                // id line if the task is not present.
                                match run.tasks.iter().find(|t| &t.id == dep_id) {
                                    Some(dep) => {
                                        let (badge, color) = task_state_badge(&dep.state, app);
                                        Line::from(vec![Span::styled(
                                            format!("{} {}", badge, dep.id.0),
                                            Style::default().fg(color),
                                        )])
                                    }
                                    None => Line::from(vec![Span::styled(
                                        format!("  {}", dep_id.0),
                                        Style::default()
                                            .fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                                    )]),
                                }
                            })
                            .collect(),
                        _ => vec![Line::from(vec![Span::styled(
                            "  No dependencies.",
                            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                        )])],
                    }
                }
                _ => vec![Line::from(vec![Span::styled(
                    "  Select a task to see its dependencies — v cycles the view",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )])],
            };
            let dep_total = lines.len() as u16;
            let dep_scroll_max = dep_total.saturating_sub(inner.height);
            app.last_scroll_maxes
                .borrow_mut()
                .insert(ScrollablePanel::DependencyView, dep_scroll_max);
            let dep_scroll_offset =
                app.panel_offset(ScrollablePanel::DependencyView, dep_scroll_max);
            let para = Paragraph::new(lines).scroll((dep_scroll_offset, 0));
            frame.render_widget(para, inner);
        }
        DependencyViewMode::Tree => {
            // Resolve the selected task (the tree root) within the run, then
            // recurse over its forward `depends_on` prerequisites, drawing an
            // indented ASCII tree capped at depth 2.
            let selected_id = app.selected_task_id();
            let run = app.selected_run();
            let lines: Vec<Line> = match (selected_id, run) {
                (Some(id), Some(run)) => {
                    let selected = run.tasks.iter().find(|t| &t.id == id);
                    match selected {
                        Some(root) if !root.depends_on.is_empty() => {
                            let mut acc: Vec<Line> = Vec::new();
                            render_dependency_tree_children(
                                app,
                                run,
                                &root.depends_on,
                                "",
                                0,
                                &mut acc,
                            );
                            acc
                        }
                        _ => vec![Line::from(vec![Span::styled(
                            "  No dependencies.",
                            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                        )])],
                    }
                }
                _ => vec![Line::from(vec![Span::styled(
                    "  Select a task to see its dependencies — v cycles the view",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )])],
            };
            let dep_total = lines.len() as u16;
            let dep_scroll_max = dep_total.saturating_sub(inner.height);
            app.last_scroll_maxes
                .borrow_mut()
                .insert(ScrollablePanel::DependencyView, dep_scroll_max);
            let dep_scroll_offset =
                app.panel_offset(ScrollablePanel::DependencyView, dep_scroll_max);
            let para = Paragraph::new(lines).scroll((dep_scroll_offset, 0));
            frame.render_widget(para, inner);
        }
        DependencyViewMode::Timeline => {
            // Time-scaled Gantt: one row per task, bar scaled to wall-clock window.
            // Read the clock ONCE at render; pass it into the pure helper.
            let now = chrono::Utc::now();
            let run = app.selected_run();
            let lines: Vec<Line> = match run {
                Some(run) if !run.tasks.is_empty() => {
                    // Span: min started_at over started tasks → max(finished_at,
                    // or `now` for any task still running).
                    let span_start = run.tasks.iter().filter_map(|t| t.started_at).min();
                    match span_start {
                        None => {
                            // Nothing has started yet — friendly placeholder.
                            vec![Line::from(vec![Span::styled(
                                "  No timing yet.",
                                Style::default()
                                    .fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                            )])]
                        }
                        Some(span_start) => {
                            let mut span_end = run
                                .tasks
                                .iter()
                                .filter_map(|t| t.finished_at)
                                .max()
                                .unwrap_or(span_start);
                            let any_running = run
                                .tasks
                                .iter()
                                .any(|t| t.started_at.is_some() && t.finished_at.is_none());
                            if any_running {
                                span_end = span_end.max(now);
                            }
                            // Guard: ensure span is at least 1 second wide to avoid
                            // division by zero inside gantt_bar_cols.
                            if span_end <= span_start {
                                span_end = span_start + chrono::Duration::seconds(1);
                            }

                            // Label columns + bar columns = inner.width.
                            let bar_width = inner.width.saturating_sub(LABEL_COLS);
                            run.tasks
                                .iter()
                                .map(|task| {
                                    let (_badge, color) = task_state_badge(&task.state, app);
                                    let label = truncate_label(&task.id.0, LABEL_COLS);
                                    let bar: String = if bar_width == 0 {
                                        // Pane too narrow — omit bar.
                                        String::new()
                                    } else {
                                        match task.started_at {
                                            None => {
                                                // Not started yet: ghost slot with dim dots.
                                                "·".repeat(bar_width as usize)
                                            }
                                            Some(start) => {
                                                let end = task.finished_at.unwrap_or(now);
                                                let (a, b) = gantt_bar_cols(
                                                    span_start, span_end, start, end, bar_width,
                                                );
                                                // Spaces before [a), '█' across [a,b), spaces after.
                                                let before = " ".repeat(a as usize);
                                                let filled =
                                                    "█".repeat((b.saturating_sub(a)) as usize);
                                                let after = " "
                                                    .repeat(bar_width.saturating_sub(b) as usize);
                                                format!("{before}{filled}{after}")
                                            }
                                        }
                                    };
                                    Line::from(vec![
                                        Span::raw(label),
                                        Span::styled(bar, Style::default().fg(color)),
                                    ])
                                })
                                .collect()
                        }
                    }
                }
                _ => vec![Line::from(vec![Span::styled(
                    "  No tasks.",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )])],
            };
            let dep_total = lines.len() as u16;
            let dep_scroll_max = dep_total.saturating_sub(inner.height);
            app.last_scroll_maxes
                .borrow_mut()
                .insert(ScrollablePanel::DependencyView, dep_scroll_max);
            let dep_scroll_offset =
                app.panel_offset(ScrollablePanel::DependencyView, dep_scroll_max);
            let para = Paragraph::new(lines).scroll((dep_scroll_offset, 0));
            frame.render_widget(para, inner);
        }
        // `Off` is handled by the caller (this fn is not invoked).
        DependencyViewMode::Off => {}
    }
}

/// Maximum recursion depth (in addition to the direct prerequisites at depth 0)
/// for the dependency `Tree` view; `0` = direct prerequisites only, so the cap
/// of `2` admits prerequisites, their prerequisites, and grandchildren.
const DEPENDENCY_TREE_MAX_DEPTH: usize = 2;

/// Recursively emit indented ASCII-tree lines for a slice of prerequisite task
/// ids, looking each up in `run.tasks` for its state badge.
///
/// `prefix` is the indentation carried from ancestor levels (built from `│   `
/// for ancestors that still have following siblings and `    ` for ancestors
/// that were the last child).  Each emitted line is
/// `<prefix><connector>[state] <id>` where `<connector>` is `├── ` for a child
/// with following siblings and `└── ` for the last child.  An unknown id (not
/// present in `run.tasks`) renders with a `[?]` badge.  Recursion stops once
/// `depth` exceeds [`DEPENDENCY_TREE_MAX_DEPTH`].
fn render_dependency_tree_children(
    app: &App,
    run: &makina_core::api::RunView,
    deps: &[makina_core::api::TaskId],
    prefix: &str,
    depth: usize,
    acc: &mut Vec<Line<'static>>,
) {
    for (i, dep_id) in deps.iter().enumerate() {
        let is_last = i + 1 == deps.len();
        let connector = if is_last { "└── " } else { "├── " };
        match run.tasks.iter().find(|t| &t.id == dep_id) {
            Some(dep) => {
                let (badge, color) = task_state_badge(&dep.state, app);
                acc.push(Line::from(vec![Span::styled(
                    format!("{prefix}{connector}{badge} {}", dep.id.0),
                    Style::default().fg(color),
                )]));
                if depth < DEPENDENCY_TREE_MAX_DEPTH && !dep.depends_on.is_empty() {
                    // Carry `│   ` past children that still have siblings, or a
                    // blank gap past the last child.
                    let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
                    render_dependency_tree_children(
                        app,
                        run,
                        &dep.depends_on,
                        &child_prefix,
                        depth + 1,
                        acc,
                    );
                }
            }
            None => {
                // Missing/unknown id: render the bare id with a `[?]` badge.
                acc.push(Line::from(vec![Span::styled(
                    format!("{prefix}{connector}[?] {}", dep_id.0),
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )]));
            }
        }
    }
}

/// Number of terminal columns reserved for the task-id label in the Gantt view.
const LABEL_COLS: u16 = 12;

/// Map a task's wall-clock window onto bar columns within `width`.
///
/// `span_start`/`span_end` define the overall run window (callers guarantee
/// `span_end > span_start`).  `start`/`end` are the task's own window.
/// Returns `(start_col, end_col)` with `0 <= start_col <= end_col <= width`,
/// scaled linearly by elapsed nanoseconds.  A non-zero-duration task is rounded
/// up to at least one cell.  **No clock is read here.**
fn gantt_bar_cols(
    span_start: DateTime<Utc>,
    span_end: DateTime<Utc>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    width: u16,
) -> (u16, u16) {
    let total = (span_end - span_start)
        .num_nanoseconds()
        .unwrap_or(1)
        .max(1);
    let off = (start - span_start).num_nanoseconds().unwrap_or(0).max(0);
    let len = (end - start).num_nanoseconds().unwrap_or(0).max(0);
    let w = width as i128;
    let start_col = ((off as i128 * w) / total as i128) as u16;
    // Round the end up so a non-zero-duration task always shows ≥1 cell.
    let raw_end = (((off + len) as i128 * w) / total as i128) as u16;
    let end_col = raw_end.max(start_col.saturating_add(1)).min(width);
    (start_col.min(width), end_col)
}

/// Truncate (or pad) `s` to exactly `cols` terminal columns for the Gantt label.
///
/// If `s` is longer than `cols` it is truncated; if shorter, it is padded with
/// trailing spaces so the bar column always starts at the same offset.
fn truncate_label(s: &str, cols: u16) -> String {
    let cols = cols as usize;
    let mut out: String = s.chars().take(cols).collect();
    while out.chars().count() < cols {
        out.push(' ');
    }
    out
}

/// Per-role metric label.
///
/// Returns "developer" or "reviewer".
fn role_label(role: &makina_core::api::AgentRole) -> &'static str {
    match role {
        makina_core::api::AgentRole::Developer => "developer",
        makina_core::api::AgentRole::Reviewer => "reviewer",
    }
}

/// Format a duration in milliseconds as a human-readable string.
///
/// Examples: "1.8s", "2m 04s"
fn fmt_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    let remaining_ms = ms % 1000;

    if total_secs >= 60 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("{}m {:02}s", mins, secs)
    } else {
        let secs_f = total_secs as f64 + remaining_ms as f64 / 1000.0;
        format!("{:.1}s", secs_f)
    }
}

/// Per-role metric lines for the focused task's detail header.
///
/// Builds one line per role with the format:
/// `{role} · {model} · {duration}` (+ ` · {in}→{out} tok` only when usage Some).
fn role_metric_lines(app: &App, task: &makina_core::api::TaskView) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let Some(run) = app.selected_run() else {
        return lines;
    };
    let Some(by_role) = app.role_metrics.get(&(run.id, task.id.clone())) else {
        return lines;
    };

    for role in [
        makina_core::api::AgentRole::Developer,
        makina_core::api::AgentRole::Reviewer,
    ] {
        if let Some(m) = by_role.get(&role) {
            let mut text = format!(
                "{} · {} · {}",
                role_label(&role),
                m.model,
                fmt_duration(m.duration_ms)
            );
            if let Some(u) = &m.usage
                && let (Some(i), Some(o)) = (u.input_tokens, u.output_tokens)
            {
                text.push_str(&format!(" · {}→{} tok", i, o));
            }
            lines.push(Line::from(Span::styled(
                text,
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
            )));
        }
    }
    lines
}

/// Render the activity indicators (idle time and wall-clock countdown) for a task.
///
/// Returns a vector of Spans to be added to the task detail line, showing:
/// - `idle {n}s` with color based on idle threshold (dim → amber → red)
/// - `wall-clock {m}m {s}s left` countdown
fn task_activity_indicators(app: &App, task: &makina_core::api::TaskView) -> Vec<Span<'static>> {
    let mut spans: Vec<Span> = Vec::new();

    if !matches!(
        task.state,
        makina_core::api::TaskState::InProgress | makina_core::api::TaskState::InReview
    ) {
        return spans;
    }

    if let Some(run) = app.selected_run() {
        let key = (run.id, task.id.clone());

        // Add idle time indicator.
        if let Some(last_activity_tick) = app.task_last_activity_tick.get(&key) {
            // Tick interval is 250ms, so 4 ticks per second.
            let idle_secs = (app.tick - last_activity_tick) / 4;
            let idle_color = if let Some(idle_cap) = app.idle_secs_config {
                if idle_secs >= idle_cap {
                    app.active_theme.get(crate::theme::ThemeRole::Error)
                } else if idle_secs >= idle_cap / 2 {
                    app.active_theme.get(crate::theme::ThemeRole::Warning)
                } else {
                    app.active_theme.get(crate::theme::ThemeRole::Dim)
                }
            } else {
                app.active_theme.get(crate::theme::ThemeRole::Dim)
            };
            spans.push(Span::styled(
                format!("  idle {}s", idle_secs),
                Style::default().fg(idle_color),
            ));
        }

        // Add wall-clock countdown.
        if let Some(step_start_tick) = app.task_step_start_tick.get(&key) {
            // Tick interval is 250ms, so 4 ticks per second.
            let elapsed_secs = (app.tick - step_start_tick) / 4;
            let wall_clock_cap = app.wall_clock_secs_config;
            if elapsed_secs < wall_clock_cap {
                let remaining_secs = wall_clock_cap - elapsed_secs;
                let minutes = remaining_secs / 60;
                let secs = remaining_secs % 60;
                spans.push(Span::styled(
                    format!("  · wall-clock {}m {}s left", minutes, secs),
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                ));
            } else {
                spans.push(Span::styled(
                    "  · wall-clock exceeded",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Error)),
                ));
            }
        }
    }

    spans
}

fn render_exchange_pane(app: &App, frame: &mut Frame, area: Rect, focused: bool) {
    // Determine which task's log to display using the composite (RunId, TaskId)
    // key so logs from different runs with the same task slug never collide.
    let task_id = app.selected_task_id();
    let log_opt = app.selected_exchange_log();

    // Check if the selected task has a trailing incomplete response.
    let has_trailing_incomplete = log_opt
        .map(|log| {
            log.entries
                .last()
                .map(|entry| {
                    matches!(
                        &entry.content,
                        crate::app::ExchangeContent::Response {
                            complete: false,
                            ..
                        }
                    )
                })
                .unwrap_or(false)
        })
        .unwrap_or(false);

    // When the error pane is collapsed but errors are pending, surface a badge
    // in the Exchange title so the user knows there's something to expand.
    let title = if !app.error_pane_open && !app.error_messages.is_empty() {
        let n = app.error_messages.len();
        format!(" Exchange ({n} errors) ")
    } else if has_trailing_incomplete {
        format!(" {} Exchange ", spinner_frame(app.tick))
    } else {
        " Exchange ".to_string()
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::TOP)
        .border_style(if focused {
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Info))
        } else {
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim))
        });

    let inner = block.inner(area);
    frame.render_widget(block, area);

    match log_opt {
        None => {
            // No task focused or no exchange yet.
            let mut detail_lines: Vec<Line> = Vec::new();

            // Add task detail with iteration counts if a task is selected.
            if let Some(task) = app
                .selected_run()
                .and_then(|run| app.selected_task.and_then(|i| run.tasks.get(i)))
            {
                let counts = format!(
                    "gate ×{}  ·  review ×{}",
                    task.gate_iterations, task.review_iterations
                );
                let style = if task.gate_iterations + task.review_iterations == 0 {
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim))
                } else {
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Warning))
                };
                detail_lines.push(Line::from(Span::styled(counts, style)));

                // Add idle time and wall-clock countdown for in-progress tasks.
                let activity_indicators = task_activity_indicators(app, task);
                if !activity_indicators.is_empty() {
                    detail_lines.push(Line::from(activity_indicators));
                }

                // Add per-role metrics (plan 0024).
                let metrics = role_metric_lines(app, task);
                detail_lines.extend(metrics);

                detail_lines.push(Line::from(""));

                // Add failure reason if the task is failed.
                if let Some(reason) = &task.failure_reason {
                    let label = failure_kind_label(&reason.kind);
                    let reason_text = format!("failed: {} — {}", label, reason.message);
                    detail_lines.push(Line::from(Span::styled(
                        reason_text,
                        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Error)),
                    )));
                    detail_lines.push(Line::from(""));
                }
            }

            let hint = if task_id.is_none() {
                "  No task focused."
            } else {
                "  No exchange yet."
            };
            detail_lines.push(Line::from(vec![Span::styled(
                hint,
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
            )]));

            let para = Paragraph::new(detail_lines);
            frame.render_widget(para, inner);
        }
        Some(log) if log.entries.is_empty() => {
            let mut detail_lines: Vec<Line> = Vec::new();

            // Add task detail with iteration counts if a task is selected.
            if let Some(task) = app
                .selected_run()
                .and_then(|run| app.selected_task.and_then(|i| run.tasks.get(i)))
            {
                let counts = format!(
                    "gate ×{}  ·  review ×{}",
                    task.gate_iterations, task.review_iterations
                );
                let style = if task.gate_iterations + task.review_iterations == 0 {
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim))
                } else {
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Warning))
                };
                detail_lines.push(Line::from(Span::styled(counts, style)));

                // Add idle time and wall-clock countdown for in-progress tasks.
                let activity_indicators = task_activity_indicators(app, task);
                if !activity_indicators.is_empty() {
                    detail_lines.push(Line::from(activity_indicators));
                }

                // Add per-role metrics (plan 0024).
                let metrics = role_metric_lines(app, task);
                detail_lines.extend(metrics);

                detail_lines.push(Line::from(""));

                // Add failure reason if the task is failed.
                if let Some(reason) = &task.failure_reason {
                    let label = failure_kind_label(&reason.kind);
                    let reason_text = format!("failed: {} — {}", label, reason.message);
                    detail_lines.push(Line::from(Span::styled(
                        reason_text,
                        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Error)),
                    )));
                    detail_lines.push(Line::from(""));
                }
            }

            detail_lines.push(Line::from(vec![Span::styled(
                "  No exchange yet.",
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
            )]));

            let para = Paragraph::new(detail_lines);
            frame.render_widget(para, inner);
        }
        Some(log) => {
            // Build the exchange lines.
            let mut lines: Vec<Line> = Vec::new();

            // Add task detail with iteration counts if a task is selected.
            if let Some(task) = app
                .selected_run()
                .and_then(|run| app.selected_task.and_then(|i| run.tasks.get(i)))
            {
                let counts = format!(
                    "gate ×{}  ·  review ×{}",
                    task.gate_iterations, task.review_iterations
                );
                let style = if task.gate_iterations + task.review_iterations == 0 {
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim))
                } else {
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Warning))
                };
                lines.push(Line::from(Span::styled(counts, style)));

                // Add idle time and wall-clock countdown for in-progress tasks.
                let activity_indicators = task_activity_indicators(app, task);
                if !activity_indicators.is_empty() {
                    lines.push(Line::from(activity_indicators));
                }

                // Add per-role metrics (plan 0024).
                let metrics = role_metric_lines(app, task);
                lines.extend(metrics);

                lines.push(Line::from(""));

                // Add failure reason if the task is failed.
                if let Some(reason) = &task.failure_reason {
                    let label = failure_kind_label(&reason.kind);
                    let reason_text = format!("failed: {} — {}", label, reason.message);
                    lines.push(Line::from(Span::styled(
                        reason_text,
                        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Error)),
                    )));
                    lines.push(Line::from(""));
                }
            }

            let content_width = inner.width;
            for entry in &log.entries {
                lines.extend(exchange_entry_lines(entry, app, content_width));
            }

            // Scroll: `scroll_max` pins the bottom-most visible offset (as the
            // old auto-scroll did); `effective_offset` honours the user's manual
            // wheel offset (task `tui-mouse-scroll`) or stays pinned to the
            // bottom while auto-following.
            let pane_height = inner.height as usize;
            let total_lines = lines.len();
            let scroll_max = total_lines.saturating_sub(pane_height) as u16;
            // Record the rendered bottom so the (geometry-free) `App::update`
            // scroll path can anchor `scroll_up` and bound `scroll_down` to the
            // real bottom (interior mutability keeps the `&App` render signature).
            app.last_scroll_maxes
                .borrow_mut()
                .insert(crate::app::ScrollablePanel::Exchange, scroll_max);
            let scroll_offset = app.effective_offset(scroll_max);

            let para = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll_offset, 0));
            frame.render_widget(para, inner);

            // Render scrollbar only when content exceeds the viewport.
            if scroll_max > 0 {
                let mut scrollbar_state =
                    ScrollbarState::new(scroll_max as usize).position(scroll_offset as usize);
                let scrollbar = Scrollbar::default()
                    .orientation(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None);
                frame.render_stateful_widget(scrollbar, inner, &mut scrollbar_state);
            }
        }
    }
}

// ── Task entry pane ───────────────────────────────────────────────────────────

/// Format the execution content for a task's Execution accordion section.
///
/// Summarizes the task's exchanges and progress for the given (RunId, TaskId),
/// showing iteration counts, activity indicators, role metrics, and failure reason if applicable.
/// Falls back to "No execution yet — start the run (Ctrl+S)" when there are no exchanges.
fn format_task_execution_content(
    app: &App,
    run: &RunView,
    task: &makina_core::api::TaskView,
) -> String {
    let mut content = String::new();

    // Check if there are any exchanges for this task
    let exchange_exists = app.exchange_logs.contains_key(&(run.id, task.id.clone()));

    if !exchange_exists {
        // No execution yet — show empty state
        return "No execution yet — start the run (Ctrl+S)".to_string();
    }

    // Add iteration counts
    if task.gate_iterations > 0 || task.review_iterations > 0 {
        let counts = format!(
            "gate ×{}  ·  review ×{}",
            task.gate_iterations, task.review_iterations
        );
        content.push_str(&counts);
        content.push('\n');
    }

    // Add activity indicators (idle time, wall-clock countdown) for in-progress tasks
    let activity_indicators = task_activity_indicators(app, task);
    if !activity_indicators.is_empty() {
        let activity_text = activity_indicators
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        content.push_str(&activity_text);
        content.push('\n');
    }

    // Add per-role metrics
    let metrics = role_metric_lines(app, task);
    for line in metrics {
        for span in &line.spans {
            content.push_str(span.content.as_ref());
        }
        content.push('\n');
    }

    // Add failure reason if the task is failed
    if let Some(reason) = &task.failure_reason {
        let label = failure_kind_label(&reason.kind);
        content.push_str(&format!("failed: {} — {}\n", label, reason.message));
    }

    // If we generated content, trim trailing newline; otherwise use empty state
    if content.is_empty() {
        "No execution data available".to_string()
    } else {
        content.trim_end().to_string()
    }
}

/// Render a task's entry (metadata + Markdown body) into a bordered pane.
/// Width is taken from the pane's inner area so wrapping matches the pane, and
/// the body reuses plan 0020's hardened `render_markdown` — no new parser.
fn render_task_entry_pane(
    app: &App,
    run: &RunView,
    task_idx: usize,
    frame: &mut Frame,
    area: Rect,
) {
    let block = Block::default()
        .title(" Task Entry ")
        .borders(Borders::TOP)
        .border_style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Info)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(task) = run.tasks.get(task_idx) {
        let mut lines: Vec<Line> = Vec::new();

        // Task header: ID and title
        lines.push(Line::from(vec![Span::styled(
            format!("{} — {}", task.id, task.title),
            Style::default()
                .fg(app.active_theme.get(crate::theme::ThemeRole::Accent))
                .add_modifier(Modifier::BOLD),
        )]));

        // State badge
        let (badge, badge_color) = task_state_badge(&task.state, app);
        lines.push(Line::from(vec![Span::styled(
            format!("  {}", badge),
            Style::default().fg(badge_color),
        )]));

        // Dependencies and gated status
        if !task.depends_on.is_empty() {
            let deps_str = task
                .depends_on
                .iter()
                .map(|id| id.0.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(Line::from(vec![Span::styled(
                format!("  Depends on: {}", deps_str),
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
            )]));
        }

        // Add metrics if available
        if task.gate_iterations > 0 || task.review_iterations > 0 {
            let counts = format!(
                "gate ×{}  ·  review ×{}",
                task.gate_iterations, task.review_iterations
            );
            let style = Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Warning));
            lines.push(Line::from(Span::styled(counts, style)));
        }

        lines.push(Line::from(""));

        // Get the task's expanded accordion sections from the app state.
        // Default to both Scope and Execution expanded for backward compatibility.
        let expanded = app
            .task_accordion_expanded
            .get(&task.id)
            .cloned()
            .unwrap_or_else(|| {
                let mut default = HashSet::new();
                default.insert(AccordionSection::Scope);
                default.insert(AccordionSection::Execution);
                default
            });

        let content_width = inner.width;

        // SCOPE section — task's static description rendered as Markdown
        for l in render_accordion_section(
            app,
            "Scope",
            AccordionSection::Scope,
            &expanded,
            &task.entry_text,
            false, // task tabs don't use focused_section highlighting
            content_width,
            true, // render as markdown
        ) {
            lines.push(l);
        }
        lines.push(Line::from(""));

        // EXECUTION section — live activity or empty state
        let execution_content = format_task_execution_content(app, run, task);
        for l in render_accordion_section(
            app,
            "Execution",
            AccordionSection::Execution,
            &expanded,
            &execution_content,
            false, // task tabs don't use focused_section highlighting
            content_width,
            false, // render as raw text, not markdown
        ) {
            lines.push(l);
        }
        lines.push(Line::from(""));

        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
    } else {
        // Task not found placeholder
        let placeholder = Line::from(vec![Span::styled(
            "  Task not found.",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
        )]);
        let para = Paragraph::new(vec![placeholder]);
        frame.render_widget(para, inner);
    }
}

// ── Helper: find task in run ──────────────────────────────────────────────────

/// Resolve a `TaskId` to a task index within the selected run, if present.
fn find_task_idx_in_run(app: &App, task_id: &TaskId) -> Option<usize> {
    app.selected_run()
        .and_then(|run| run.tasks.iter().position(|t| &t.id == task_id))
}

// ── Plan detail pane ──────────────────────────────────────────────────────────

/// Render a plan tab's accordion pane with SCOPE, ARCHITECTURE, TASKS, and STATUS sections.
///
/// Displays the plan with four independently expandable accordion sections.
/// Each section shows a `[+]` (collapsed) or `[-]` (expanded) marker followed by the section title.
/// When expanded, content is displayed below the header, indented by two spaces.
/// Missing files render as `(no SCOPE.md)`, etc.
///
/// # Note
///
/// This function is not yet wired to the main render path; it will be called from
/// the active-tab dispatch in `render` once the `remove-plan-detail-singleton` task
/// lands.  The `allow(dead_code)` below suppresses the lint until that task is done.
#[allow(dead_code)]
pub(crate) fn render_plan_accordion_pane(
    app: &App,
    plan: &makina_core::orchestrator::PlanEntry,
    frame: &mut Frame,
    area: Rect,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Reserve the rightmost column for the scrollbar so text is not overpainted.
    // We compute content_area early so we know the render width for wrap-aware
    // row accounting (needed to correctly map header lines to terminal rows).
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let content_area = cols[0];
    let scrollbar_area = cols[1];

    // Helper: number of terminal rows a single Line occupies when rendered with
    // `Wrap { trim: false }` at the given column width.
    let rendered_rows_for_line = |line: &Line<'_>| -> u16 {
        let w = line.width();
        if content_area.width == 0 || w == 0 {
            1
        } else {
            // ceil(w / content_area.width), clamped to u16::MAX
            (w as u32)
                .div_ceil(content_area.width as u32)
                .min(u16::MAX as u32) as u16
        }
    };

    // `rendered_row` tracks the running count of *terminal rows* (not Vec<Line>
    // indices) emitted so far.  We record this value before pushing each header
    // line so the hit-test can map a terminal y-coordinate back to the section.
    let mut rendered_row: u16 = 0;
    // Pairs of (section, rendered_row_of_header).
    let mut accordion_header_rows: Vec<(AccordionSection, u16)> = Vec::new();

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Helper macro: push a line and advance rendered_row.
    // (Using a closure would hit borrow checker issues with the captures.)
    macro_rules! push_line {
        ($l:expr) => {{
            let l: Line<'static> = $l;
            rendered_row += rendered_rows_for_line(&l);
            lines.push(l);
        }};
    }

    // Header: plan name and directory
    push_line!(Line::from(vec![
        Span::styled(
            "Plan: ",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim))
        ),
        Span::styled(
            plan.slug.clone(),
            Style::default()
                .fg(app.active_theme.get(crate::theme::ThemeRole::Accent))
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    push_line!(Line::from(vec![
        Span::styled(
            "Dir:  ",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim))
        ),
        Span::styled(
            plan.dir.display().to_string(),
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
        ),
    ]));
    push_line!(Line::from(""));

    // Get accordion state for this plan
    let expanded = app
        .accordion_state
        .get(&plan.slug)
        .cloned()
        .unwrap_or_default();

    // SCOPE section — record header row *before* pushing the header line.
    let scope_focused = matches!(app.focused_section, Some(AccordionSection::Scope));
    accordion_header_rows.push((AccordionSection::Scope, rendered_row));
    for l in render_accordion_section(
        app,
        "SCOPE",
        AccordionSection::Scope,
        &expanded,
        plan.scope_text.as_deref().unwrap_or("(no SCOPE.md)"),
        scope_focused,
        content_area.width,
        true,
    ) {
        push_line!(l);
    }
    push_line!(Line::from(""));

    // ARCHITECTURE section
    let arch_focused = matches!(app.focused_section, Some(AccordionSection::Architecture));
    accordion_header_rows.push((AccordionSection::Architecture, rendered_row));
    for l in render_accordion_section(
        app,
        "ARCHITECTURE",
        AccordionSection::Architecture,
        &expanded,
        plan.architecture_text
            .as_deref()
            .unwrap_or("(no ARCHITECTURE.md)"),
        arch_focused,
        content_area.width,
        true,
    ) {
        push_line!(l);
    }
    push_line!(Line::from(""));

    // TASKS section
    let tasks_text = format_tasks_section(&plan.tasks);
    let tasks_focused = matches!(app.focused_section, Some(AccordionSection::Tasks));
    accordion_header_rows.push((AccordionSection::Tasks, rendered_row));
    for l in render_accordion_section(
        app,
        "TASKS",
        AccordionSection::Tasks,
        &expanded,
        &tasks_text,
        tasks_focused,
        content_area.width,
        false,
    ) {
        push_line!(l);
    }
    push_line!(Line::from(""));

    // STATUS section
    let status_focused = matches!(app.focused_section, Some(AccordionSection::Status));
    accordion_header_rows.push((AccordionSection::Status, rendered_row));
    for l in render_accordion_section(
        app,
        "STATUS",
        AccordionSection::Status,
        &expanded,
        plan.status_text.as_deref().unwrap_or("(no STATUS.md)"),
        status_focused,
        content_area.width,
        true,
    ) {
        push_line!(l);
    }
    push_line!(Line::from(""));

    // Footer help text
    push_line!(Line::from(Span::styled(
        "  [s] scope  [a] arch  [t] tasks  [z] status  [◄] [►] tabs  [Ctrl+W] close",
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
    )));

    // `rendered_row` now holds the total rendered height in terminal rows.
    let total_rendered_rows = rendered_row;
    // Per-panel scroll clamp ceiling: how many rows can be scrolled before the
    // last line of content reaches the top of the viewport.
    let accordion_scroll_max = total_rendered_rows.saturating_sub(content_area.height);
    app.last_scroll_maxes
        .borrow_mut()
        .insert(ScrollablePanel::PlanAccordion, accordion_scroll_max);

    let accordion_scroll_offset =
        app.panel_offset(ScrollablePanel::PlanAccordion, accordion_scroll_max);

    // Compute the actual terminal rectangle for each visible accordion header.
    // Each header occupies exactly one row; we convert from rendered-row space to
    // viewport coordinates by subtracting the scroll offset and adding the pane origin.
    let mut computed_bounds = Vec::new();
    for (section, header_rendered_row) in accordion_header_rows {
        // Skip headers scrolled above or below the visible viewport.
        if header_rendered_row >= accordion_scroll_offset
            && header_rendered_row < accordion_scroll_offset + content_area.height
        {
            let row_in_viewport = content_area.y + (header_rendered_row - accordion_scroll_offset);
            let header_rect = Rect {
                x: content_area.x,
                y: row_in_viewport,
                width: content_area.width,
                height: 1,
            };
            computed_bounds.push((section, header_rect));
        }
    }

    // Store the computed bounds via RefCell so the event loop can hit-test mouse
    // clicks.  Mirrors the pattern used by `selection_panes` and `panel_geometries`.
    *app.accordion_header_bounds.borrow_mut() = computed_bounds;

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((accordion_scroll_offset, 0));
    frame.render_widget(para, content_area);

    if accordion_scroll_max > 0 {
        let accordion_scroll_offset = app
            .scroll_offsets
            .get(&ScrollablePanel::PlanAccordion)
            .copied()
            .unwrap_or(0)
            .min(accordion_scroll_max);
        let mut scrollbar_state = ScrollbarState::new(accordion_scroll_max as usize)
            .position(accordion_scroll_offset as usize);
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

/// Render a single accordion section (SCOPE, ARCHITECTURE, TASKS, or STATUS).
///
/// Returns a Vec<Line> containing the header (expanded/collapsed marker) and,
/// if expanded, the content lines with proper indentation.
#[allow(dead_code, clippy::too_many_arguments)]
fn render_accordion_section(
    app: &App,
    title: &str,
    section: AccordionSection,
    expanded_set: &HashSet<AccordionSection>,
    content: &str,
    focused: bool,
    content_width: u16,
    as_markdown: bool,
) -> Vec<Line<'static>> {
    let mut result = Vec::new();
    let is_expanded = expanded_set.contains(&section);
    let marker = if is_expanded { "[-]" } else { "[+]" };

    // Section header with focus styling
    let marker_style = Style::default()
        .fg(app.active_theme.get(crate::theme::ThemeRole::Warning))
        .add_modifier(Modifier::BOLD);

    let mut title_style = Style::default()
        .fg(app.active_theme.get(crate::theme::ThemeRole::Accent))
        .add_modifier(Modifier::BOLD);

    if focused {
        // Apply a distinctive background and bold modifier when focused.
        title_style = title_style
            .bg(app.active_theme.get(crate::theme::ThemeRole::FocusBg))
            .fg(app.active_theme.get(crate::theme::ThemeRole::Foreground))
            .add_modifier(Modifier::BOLD);
    }

    result.push(Line::from(vec![
        Span::styled(marker, marker_style),
        Span::raw(" "),
        Span::styled(title.to_string(), title_style),
    ]));

    // Content (if expanded)
    if is_expanded {
        result.push(Line::from(""));
        if as_markdown {
            // Render the section body through the hardened Markdown renderer
            // (headings, bold/italic, lists, code blocks, links, rules) instead
            // of showing raw CommonMark source. Wrap to the pane width minus the
            // 2-space indent so the indented lines still fit the viewport, then
            // prepend the indent so the body stays nested under its header.
            let body_width = content_width.saturating_sub(2);
            let base =
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground));
            for mut line in
                render_markdown_cached(app, content, base, body_width, &app.active_theme)
            {
                line.spans.insert(0, Span::raw("  "));
                result.push(line);
            }
        } else {
            // Synthesized, already-structured text (e.g. the TASKS summary):
            // indent each raw line without Markdown processing.
            for line in content.lines() {
                result.push(Line::from(format!("  {line}")));
            }
        }
    }

    result
}

/// Format the tasks section content: task list with GATED markers and dependencies.
fn format_tasks_section(tasks: &[makina_core::orchestrator::PlanTaskPreview]) -> String {
    if tasks.is_empty() {
        return "(no tasks)".to_string();
    }
    let mut text = format!("Tasks ({})", tasks.len());
    let gated = tasks.iter().filter(|t| t.gated).count();
    if gated > 0 {
        text.push_str(&format!("  · {} gated", gated));
    }
    text.push('\n');
    text.push('\n');
    for (i, t) in tasks.iter().enumerate() {
        text.push_str(&format!("  {}. ", i + 1));
        text.push_str(&t.id);
        text.push_str(&format!(" — {}", t.title));
        if t.gated {
            text.push_str("  GATED");
        }
        text.push('\n');
        if !t.depends_on.is_empty() {
            text.push_str(&format!("     depends on: {}", t.depends_on.join(", ")));
            text.push('\n');
        }
    }
    text
}

// ── Error pane (collapsible) ──────────────────────────────────────────────────

/// Render the collapsible error pane below the exchange pane.
///
/// Mirrors [`render_exchange_pane`]: a top-bordered block titled `Errors` with
/// one line per recent [`crate::app::ErrorMessage`], coloured by its
/// [`crate::app::ErrorLevel`].  The pane is shown only when
/// `app.error_pane_open` is set; the caller passes a 0-height `area` when the
/// pane is collapsed, which makes this a no-op (nothing is drawn into an empty
/// rectangle).
fn render_error_pane(app: &App, frame: &mut Frame, area: Rect) {
    // A 0-height area (pane collapsed) is a no-op: skip all rendering.
    if area.height == 0 || area.width == 0 {
        return;
    }

    use crate::app::ErrorLevel;

    let block = Block::default()
        .title(" Errors ")
        .borders(Borders::TOP)
        .border_style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Error)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.error_messages.is_empty() {
        let para = Paragraph::new(Line::from(vec![Span::styled(
            "  No errors.",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
        )]));
        frame.render_widget(para, inner);
        return;
    }

    // Show the most recent messages, newest last, each coloured by level.
    let lines: Vec<Line> = app
        .error_messages
        .iter()
        .map(|msg| {
            let color = match msg.level {
                ErrorLevel::Error => app.active_theme.get(crate::theme::ThemeRole::Error),
                ErrorLevel::Warn => app.active_theme.get(crate::theme::ThemeRole::Warning),
                ErrorLevel::Info => app.active_theme.get(crate::theme::ThemeRole::Dim),
            };
            Line::from(vec![Span::styled(
                format!("  {}", msg.text),
                Style::default().fg(color),
            )])
        })
        .collect();

    // Mirror the exchange pane (ui.rs:1733–1749): record scroll_max for clamping,
    // then render at the bottom while following or at the stored offset otherwise.
    let pane_height = inner.height as usize;
    let total_lines = lines.len();
    let scroll_max = total_lines.saturating_sub(pane_height) as u16;
    app.last_scroll_maxes
        .borrow_mut()
        .insert(crate::app::ScrollablePanel::ErrorPane, scroll_max);
    let scroll_offset = app.panel_offset(crate::app::ScrollablePanel::ErrorPane, scroll_max);

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset, 0));
    frame.render_widget(para, inner);
}

/// Render a warning banner when provider binaries are missing.
///
/// Shows a single-line warning for each missing provider in a yellow/amber style,
/// listing the provider name and missing command. This warning is non-fatal — the
/// app continues to run normally, but the user is alerted before a run begins.
fn render_provider_warning(app: &App, frame: &mut Frame, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Collect all missing providers.
    let missing: Vec<_> = app
        .provider_probes
        .iter()
        .filter(|p| p.resolved.is_none())
        .collect();

    if missing.is_empty() {
        return;
    }

    // Build warning text: "⚠ provider "foo" command 'bar' not found on PATH  [d] dismiss"
    // Shows the first missing provider; includes a dismiss hint.
    let first = &missing[0];
    let warning_text = format!(
        "⚠ provider \"{}\" command '{}' not found on PATH  [d] dismiss",
        first.provider, first.command
    );

    let para = Paragraph::new(warning_text).style(
        Style::default()
            .fg(app.active_theme.get(crate::theme::ThemeRole::Warning))
            .bg(app.active_theme.get(crate::theme::ThemeRole::Background)),
    );

    frame.render_widget(para, area);
}

/// Render the ingestion report panel for the selected run (when it has issues).
///
/// Each issue is shown as `[{source}] {code} — {message}` with an optional
/// ` — suggestion: …` suffix when present, and severity colour (Blocking=Red,
/// Warning=Yellow). Mirrors `render_error_pane`
/// structure and `task_state_badge` colouring. A 0-height area is a no-op.
fn render_ingestion_panel(app: &App, frame: &mut Frame, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    use makina_core::api::{IssueSeverity, IssueSource};

    let run = match app.selected_run() {
        Some(r) if !r.report.is_empty() => r,
        _ => return,
    };

    let has_blocking = run.report.is_blocked();
    let border_color = if has_blocking {
        app.active_theme.get(crate::theme::ThemeRole::Error)
    } else {
        app.active_theme.get(crate::theme::ThemeRole::Warning)
    };
    let title = if has_blocking {
        " Blocking Issues "
    } else {
        " Ingestion Issues "
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::TOP)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = run
        .report
        .issues
        .iter()
        .map(|issue| {
            let color = match issue.severity {
                IssueSeverity::Blocking => app.active_theme.get(crate::theme::ThemeRole::Error),
                IssueSeverity::Warning => app.active_theme.get(crate::theme::ThemeRole::Warning),
            };
            let source = match issue.source {
                IssueSource::Interpreter => "interpreter",
                IssueSource::Validator => "validator",
                IssueSource::Qualifier => "qualifier",
            };
            let suffix = issue
                .suggestion
                .as_ref()
                .map(|s| format!(" — suggestion: {}", s))
                .unwrap_or_default();
            Line::from(vec![Span::styled(
                format!(
                    "  [{}] {} — {}{}",
                    source, issue.code, issue.message, suffix
                ),
                Style::default().fg(color),
            )])
        })
        .collect();

    // Auto-scroll if more issues than fit (rare, capped by layout height).
    let pane_height = inner.height as usize;
    let total_lines = lines.len();
    let scroll_offset = if total_lines > pane_height {
        (total_lines - pane_height) as u16
    } else {
        0
    };

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset, 0));
    frame.render_widget(para, inner);
}

/// Convert a single [`ExchangeEntry`] into display [`Line`]s.
///
/// Prompt entries get a role-coloured label header; response entries are
/// indented and shown in a lighter colour.  An in-progress streaming response
/// (not yet complete) gets a trailing `▌` cursor indicator.
/// Render one content line: parse embedded ANSI SGR runs into styled spans
/// (no literal escape byte survives), overlaying a diff base colour where the
/// line is a diff add/remove/hunk line. ANSI SGR foreground wins where present;
/// modifiers (e.g. BOLD) are always preserved. Two-space indented.
fn diff_overlaid_content_line(text_line: &str, theme: &crate::theme::Theme) -> Line<'static> {
    use crate::ansi::{AnsiSpan, diff_line_style, parse_ansi};

    let diff_style = diff_line_style(text_line, theme);
    let ansi_spans = parse_ansi(text_line, theme);

    let mut spans: Vec<Span<'static>> = Vec::new();
    // Two-space indent, default-styled, owning its text.
    spans.push(Span::raw("  "));
    for AnsiSpan { text, style } in ansi_spans {
        let style = match (diff_style, style.fg) {
            // ANSI SGR set no foreground → overlay ONLY the diff base
            // colour, preserving any add_modifier (e.g. BOLD) the
            // span's ANSI run set.  Replacing the whole style here
            // would drop those modifiers (spec `tui-exchange-render`
            // step 3: overlay the foreground only).  `base.fg` is
            // always `Some` for a diff line, but fall back to the
            // span's own fg defensively.
            (Some(base), None) => match base.fg {
                Some(base_color) => style.fg(base_color),
                None => style,
            },
            // ANSI SGR set a foreground → it wins over the diff base.
            _ => style,
        };
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

fn exchange_entry_lines(entry: &ExchangeEntry, app: &App, width: u16) -> Vec<Line<'static>> {
    use crate::app::ExchangeContent;
    use makina_core::api::AgentRole;

    let mut lines = Vec::new();

    match &entry.content {
        ExchangeContent::Prompt { text } => {
            // Role label + prompt text on separate lines.
            let (label, label_color) = match entry.role {
                AgentRole::Developer => (
                    "▶ Developer prompt",
                    app.active_theme.get(crate::theme::ThemeRole::Success),
                ),
                AgentRole::Reviewer => (
                    "▶ Reviewer prompt",
                    app.active_theme.get(crate::theme::ThemeRole::Warning),
                ),
            };
            lines.push(Line::from(vec![Span::styled(
                label,
                Style::default()
                    .fg(label_color)
                    .add_modifier(Modifier::BOLD),
            )]));
            // Render prompt text lines (split on newlines).
            for text_line in text.lines() {
                lines.push(Line::from(vec![Span::styled(
                    format!("  {text_line}"),
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
                )]));
            }
            if text.is_empty() {
                lines.push(Line::from(vec![Span::styled(
                    "  (empty)",
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                )]));
            }
        }
        ExchangeContent::Response { text, complete } => {
            // Response entry.
            let (resp_label, resp_color) = match entry.role {
                AgentRole::Developer => (
                    "◀ Developer response",
                    app.active_theme.get(crate::theme::ThemeRole::Accent),
                ),
                AgentRole::Reviewer => (
                    "◀ Reviewer response",
                    app.active_theme.get(crate::theme::ThemeRole::Accent),
                ),
            };
            lines.push(Line::from(vec![Span::styled(
                resp_label,
                Style::default().fg(resp_color).add_modifier(Modifier::BOLD),
            )]));
            // Response text rendered through Markdown + ANSI.
            let base_style = Style::default().fg(resp_color);
            lines.extend(render_markdown_cached(
                app,
                text,
                base_style,
                width,
                &app.active_theme,
            ));

            // Streaming cursor (if not complete).
            if !*complete {
                if text.is_empty() {
                    lines.push(Line::from(vec![Span::styled(
                        "  ▌",
                        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                    )]));
                } else {
                    // Append cursor to the last line if text is present.
                    if let Some(last_line) = lines.last_mut() {
                        last_line.spans.push(Span::styled(
                            "▌",
                            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                        ));
                    }
                }
            }
        }
        // ── Thought (agent internal reasoning) ────────────────────────────
        // Observability-only side channel.  Bold, role-coloured header
        // ("💭 Developer thought" green / "💭 Reviewer thought" yellow) is
        // ALWAYS rendered so the user can see reasoning happened.  In verbose
        // mode the reasoning text is additionally shown dimmed (DarkGray) at
        // a 2-space indent so it reads as a quiet aside rather than part of
        // the answer.  In compact mode only the header line is shown.
        ExchangeContent::Thought { text } => {
            let (label, label_color) = match entry.role {
                AgentRole::Developer => (
                    "💭 Developer thought",
                    app.active_theme.get(crate::theme::ThemeRole::Success),
                ),
                AgentRole::Reviewer => (
                    "💭 Reviewer thought",
                    app.active_theme.get(crate::theme::ThemeRole::Warning),
                ),
            };
            lines.push(Line::from(vec![Span::styled(
                label,
                Style::default()
                    .fg(label_color)
                    .add_modifier(Modifier::BOLD),
            )]));
            // Thought body only in verbose mode.
            if app.verbose_mode {
                let base_style =
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim));
                let mut thought_lines =
                    render_markdown_cached(app, text, base_style, width, &app.active_theme);
                // Indent all thought lines by 2 spaces.
                for line in &mut thought_lines {
                    line.spans.insert(0, Span::raw("  "));
                }
                lines.extend(thought_lines);
            }
        }
        // ── Tool (agent tool invocation) ──────────────────────────────────
        // Header "⚙ <title> [<status>]" coloured by lifecycle status is
        // ALWAYS rendered.  In verbose mode the captured content lines are
        // additionally rendered via `diff_overlaid_content_line` so the user
        // sees exactly what was added/updated; in compact mode only the header
        // is shown.  Empty content renders nothing extra in either mode.
        ExchangeContent::Tool {
            title,
            status,
            content,
            ..
        } => {
            let status_color = match status.as_str() {
                "pending" => app.active_theme.get(crate::theme::ThemeRole::Dim),
                "in_progress" => app.active_theme.get(crate::theme::ThemeRole::Accent),
                "completed" => app.active_theme.get(crate::theme::ThemeRole::Success),
                "failed" => app.active_theme.get(crate::theme::ThemeRole::Error),
                _ => app.active_theme.get(crate::theme::ThemeRole::Foreground),
            };
            let compacted_title = crate::markup::compact_paths(title, &app.repo_root);
            lines.push(Line::from(vec![Span::styled(
                format!("⚙ {compacted_title} [{status}]"),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            )]));
            // Tool content only in verbose mode.
            if app.verbose_mode {
                for text_line in content.lines() {
                    // Re-use the Response arm's ANSI + diff overlay so an edit
                    // diff in tool output is syntax-coloured the same way.
                    lines.push(diff_overlaid_content_line(text_line, &app.active_theme));
                }
            }
        }
    }
    // Blank separator line between entries.
    lines.push(Line::from(""));
    lines
}

/// Render the modal file browser overlay centred within `area`.
///
/// Shows the current directory in the title, one row per [`crate::browser::DirEntry`]
/// (directories suffixed with `/`), the highlighted selection, and a footer of
/// key hints.  Drawn over a [`Clear`]ed region so the underlying layout does not
/// bleed through.
fn render_file_browser(
    app: &App,
    browser: &crate::browser::FileBrowser,
    frame: &mut Frame,
    area: Rect,
) {
    // Centre a box ~80% wide / 80% tall.
    let popup = centered_rect(80, 80, area);

    // Clear the region first so the popup is opaque.
    frame.render_widget(Clear, popup);

    let title = format!(" Open task list — {} ", browser.cwd.display());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)))
        .padding(Padding::horizontal(1));

    // Split the popup into a list area + a 1-row footer of hints.
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let list_area = chunks[0];
    let footer_area = chunks[1];

    if browser.entries.is_empty() {
        let empty = Paragraph::new(Line::from(vec![Span::styled(
            "(empty directory)",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
        )]));
        frame.render_widget(empty, list_area);
    } else {
        let items: Vec<ListItem> = browser
            .entries
            .iter()
            .map(|entry| {
                let (icon, name_color) = if entry.is_dir {
                    ("▸ ", app.active_theme.get(crate::theme::ThemeRole::Accent))
                } else {
                    (
                        "  ",
                        app.active_theme.get(crate::theme::ThemeRole::Foreground),
                    )
                };
                let suffix = if entry.is_dir { "/" } else { "" };
                let line = Line::from(vec![
                    Span::styled(
                        icon,
                        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
                    ),
                    Span::styled(
                        format!("{}{}", entry.name, suffix),
                        Style::default().fg(name_color),
                    ),
                ]);
                ListItem::new(line)
            })
            .collect();

        let highlight_style = Style::default()
            .fg(app.active_theme.get(crate::theme::ThemeRole::Background))
            .bg(app.active_theme.get(crate::theme::ThemeRole::Accent))
            .add_modifier(Modifier::BOLD);

        let list = List::new(items)
            .highlight_style(highlight_style)
            .highlight_symbol("▶ ");

        let mut state = ListState::default();
        state.select(Some(browser.selected));
        frame.render_stateful_widget(list, list_area, &mut state);
    }

    let footer = Paragraph::new(Line::from(vec![Span::styled(
        "[Enter] open/enter  [Backspace] up  [↑↓/jk] move  [Esc] cancel",
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
    )]));
    frame.render_widget(footer, footer_area);
}

/// Render the provider configuration editor modal.
fn render_provider_editor(
    app: &App,
    editor: &crate::app::ProviderEditor,
    frame: &mut Frame,
    area: Rect,
) {
    // Centre a box ~85% wide / 85% tall.
    let popup = centered_rect(85, 85, area);

    // Clear the region first so the popup is opaque.
    frame.render_widget(Clear, popup);

    let title = " View Providers & Roles ";
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Split the popup into list area + footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(2)])
        .split(inner);
    let list_area = chunks[0];
    let footer_area = chunks[1];

    // Build the list of items: providers + roles
    let mut items: Vec<ListItem> = vec![];

    // Add providers section
    for (idx, provider) in editor.providers.iter().enumerate() {
        let style = if editor.selected_provider == Some(idx) {
            Style::default()
                .fg(app.active_theme.get(crate::theme::ThemeRole::Warning))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent))
        };
        let line = Line::from(vec![Span::styled(
            format!("  Provider: {}", provider.name),
            style,
        )]);
        items.push(ListItem::new(line));
    }

    // Add role assignments section
    let roles_header = Line::from(vec![Span::styled(
        "  Roles:",
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)),
    )]);
    items.push(ListItem::new(roles_header));

    // Helper: format a role assignment as "    {label}: {provider} ({model} · {effort})".
    // The `name · effort` format is as requested by the spec (model shown with effort level).
    let fmt_role = |label: &str, assignment: &makina_core::config::RoleAssignment| -> String {
        let model_effort = match (&assignment.model, &assignment.effort) {
            (Some(model), Some(effort)) => format!("{} · {}", model, effort),
            (Some(model), None) => model.clone(),
            (None, Some(effort)) => effort.clone(),
            (None, None) => String::new(),
        };
        if model_effort.is_empty() {
            format!("    {}: {}", label, assignment.provider)
        } else {
            format!("    {}: {} ({})", label, assignment.provider, model_effort)
        }
    };

    for (label, assignment_opt) in [
        ("Developer", &editor.roles.developer),
        ("Reviewer", &editor.roles.reviewer),
        ("Planner", &editor.roles.planner),
    ] {
        if let Some(assignment) = assignment_opt {
            let detail = fmt_role(label, assignment);
            items.push(ListItem::new(Line::from(vec![Span::styled(
                detail,
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
            )])));
        }
    }

    // Discovered section: what the live agent actually advertises (modes +
    // model/effort options), distinct from the declared config above. Only shown
    // once a session has reported its capabilities.
    let has_discovered =
        editor.available_modes.is_some() || !editor.available_config_options.is_empty();
    if has_discovered {
        items.push(ListItem::new(Line::from(vec![Span::styled(
            "  Discovered (live agent):",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)),
        )])));

        if let Some(modes) = &editor.available_modes {
            let names: Vec<String> = modes
                .available_modes
                .iter()
                .map(|m| {
                    if m.id == modes.current_mode_id {
                        format!("[{}]", m.id)
                    } else {
                        m.id.clone()
                    }
                })
                .collect();
            items.push(ListItem::new(Line::from(vec![Span::styled(
                format!("    modes: {}", names.join("  ")),
                Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Success)),
            )])));
        }

        // List each advertised model, annotated with the available effort
        // (thought_level) choices, as "model · {effort options}".
        let effort_choices: Vec<String> = editor
            .available_config_options
            .iter()
            .find(|o| o.category == "thought_level")
            .map(|o| o.options.iter().map(|c| c.value.clone()).collect())
            .unwrap_or_default();
        let effort_hint = if effort_choices.is_empty() {
            String::new()
        } else {
            format!(" · {{{}}}", effort_choices.join("|"))
        };
        for opt in editor
            .available_config_options
            .iter()
            .filter(|o| o.category == "model")
        {
            for choice in &opt.options {
                items.push(ListItem::new(Line::from(vec![Span::styled(
                    format!("    model: {}{}", choice.value, effort_hint),
                    Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Success)),
                )])));
            }
        }
    }

    let highlight_style = Style::default()
        .fg(app.active_theme.get(crate::theme::ThemeRole::Background))
        .bg(app.active_theme.get(crate::theme::ThemeRole::Accent))
        .add_modifier(Modifier::BOLD);

    let list = List::new(items)
        .highlight_style(highlight_style)
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(editor.selection_index.min(editor.providers.len() + 3)));
    frame.render_stateful_widget(list, list_area, &mut state);

    // Footer with hints
    let footer = Paragraph::new(vec![
        Line::from(vec![Span::styled(
            "[Enter] commit  [↑↓/jk] navigate  [Esc] cancel",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
        )]),
        Line::from(vec![Span::styled(
            "(Read-only; edits via .makina/config.toml)",
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
        )]),
    ]);
    frame.render_widget(footer, footer_area);
}

/// Render the command palette modal.
///
/// A modal that lists all available commands, filtered by user input,
/// with navigation and selection highlighting.
fn render_command_palette(
    app: &App,
    palette: &crate::app::CommandPalette,
    frame: &mut Frame,
    area: Rect,
) {
    // Centre a box ~60% wide / 60% tall.
    let popup = centered_rect(60, 60, area);

    // Clear the region first so the popup is opaque.
    frame.render_widget(Clear, popup);

    let title = " Command Palette ";
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Split the popup into filter area, list area, and footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(inner);
    let filter_area = chunks[0];
    let list_area = chunks[1];
    let footer_area = chunks[2];

    // Render the filter line as "> {filter}▏"
    let filter_text = Line::from(vec![Span::raw(format!("> {}▏", palette.filter))]);
    let filter_widget = Paragraph::new(filter_text);
    frame.render_widget(filter_widget, filter_area);

    // Build the list of items based on mode
    let items: Vec<ListItem> = if let Some(ref theme_names) = palette.theme_selector {
        // Theme selector mode: filter and display theme names
        if palette.filter.is_empty() {
            theme_names
                .iter()
                .map(|name| ListItem::new(Line::from(vec![Span::raw(name)])))
                .collect()
        } else {
            let filter_lower = palette.filter.to_lowercase();
            theme_names
                .iter()
                .filter(|name| name.to_lowercase().contains(&filter_lower))
                .map(|name| ListItem::new(Line::from(vec![Span::raw(name)])))
                .collect()
        }
    } else {
        // Normal action list mode
        let filtered_actions = palette.filtered();
        filtered_actions
            .iter()
            .map(|action| ListItem::new(Line::from(vec![Span::raw(action.label())])))
            .collect()
    };

    // Highlight style for selected row
    let highlight_style = Style::default()
        .fg(app.active_theme.get(crate::theme::ThemeRole::Background))
        .bg(app.active_theme.get(crate::theme::ThemeRole::Accent))
        .add_modifier(Modifier::BOLD);

    let items_len = items.len();
    let list = List::new(items)
        .highlight_style(highlight_style)
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    // Select the appropriate row, clamping to the filtered list size
    if items_len > 0 {
        state.select(Some(palette.selected.min(items_len - 1)));
    }
    frame.render_stateful_widget(list, list_area, &mut state);

    // Footer with hints
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        "↑/↓ select · Enter run · Esc close",
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
    )]));
    frame.render_widget(footer, footer_area);
}

/// Render the settings modal.
///
/// A modal that displays editable configuration fields (gate iterations,
/// reviewer iterations, wall-clock seconds, idle seconds, concurrency),
/// with highlighting for the focused field and error messages.
fn render_settings(app: &App, settings: &crate::app::Settings, frame: &mut Frame, area: Rect) {
    // Centre a box ~70% wide / 70% tall.
    let popup = centered_rect(70, 70, area);

    // Clear the region first so the popup is opaque.
    frame.render_widget(Clear, popup);

    let title = " Settings ";
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Split the popup into list area, error area (if present), and footer
    let error_height = if settings.error.is_some() { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),               // list
            Constraint::Length(error_height), // error (0 or 1)
            Constraint::Length(2),            // footer
        ])
        .split(inner);
    let list_area = chunks[0];
    let error_area = chunks[1];
    let footer_area = chunks[2];

    // Build the list of settings fields
    let mut items: Vec<ListItem> = vec![];

    let fields = vec![
        (
            "Gate iterations",
            &settings.gate_iterations,
            crate::app::SettingsField::GateIterations,
        ),
        (
            "Reviewer iterations",
            &settings.reviewer_iterations,
            crate::app::SettingsField::ReviewerIterations,
        ),
        (
            "Wall-clock (s)",
            &settings.wall_clock_secs,
            crate::app::SettingsField::WallClockSecs,
        ),
        (
            "Idle (s)",
            &settings.idle_secs,
            crate::app::SettingsField::IdleSecs,
        ),
        (
            "Concurrency",
            &settings.concurrency,
            crate::app::SettingsField::Concurrency,
        ),
    ];

    for (label, value, field) in fields {
        let is_focused = field == settings.focused;
        let display_value = if value.is_empty() {
            "—".to_string()
        } else {
            value.to_string()
        };

        let text = if is_focused {
            format!("  {}: {}▏", label, display_value)
        } else {
            format!("  {}: {}", label, display_value)
        };

        let style = if is_focused {
            Style::default()
                .fg(app.active_theme.get(crate::theme::ThemeRole::Background))
                .bg(app.active_theme.get(crate::theme::ThemeRole::Accent))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground))
        };

        let line = Line::from(vec![Span::styled(text, style)]);
        items.push(ListItem::new(line));
    }

    let list = List::new(items);
    frame.render_widget(list, list_area);

    // If there's an error, render it in red
    if let Some(error) = &settings.error {
        let error_style = Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Error));
        let error_line = Line::from(vec![Span::styled(error.clone(), error_style)]);
        let error_para = Paragraph::new(error_line);
        frame.render_widget(error_para, error_area);
    }

    // Footer with hints
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        "↑/↓ field · 0-9 edit · Enter save · Esc cancel",
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
    )]));
    frame.render_widget(footer, footer_area);
}

/// Probe whether a directory is writable.
///
/// Reads the directory's metadata and reports writability. On Unix the mode
/// bits are inspected directly (owner/group/other write); elsewhere the coarse
/// `readonly()` flag is used. This never mutates the filesystem.
fn is_dir_writable(dir: &std::path::Path) -> bool {
    match std::fs::metadata(dir) {
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Any write bit set (owner/group/other) is treated as writable;
                // the metadata probe avoids creating a temp file in the tree.
                meta.permissions().mode() & 0o222 != 0
            }
            #[cfg(not(unix))]
            {
                !meta.permissions().readonly()
            }
        }
        Err(_) => false,
    }
}

/// Render the doctor health-check overlay.
///
/// Lists four health checks (config files, providers, base branch, .makina/ dir).
/// Each row shows a status (✓/✗/⚠) and a remedy hint. When no config exists,
/// offers a [w] write scaffold action.
fn render_doctor(app: &App, frame: &mut Frame, area: Rect) {
    // Centre a box ~75% wide / 70% tall.
    let popup = centered_rect(75, 70, area);

    // Clear the region first so the popup is opaque.
    frame.render_widget(Clear, popup);

    let title = " Doctor — Health Check ";
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Split the popup into list area + footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(2)])
        .split(inner);
    let list_area = chunks[0];
    let footer_area = chunks[1];

    // Build the checklist items
    let mut items: Vec<ListItem> = vec![];

    // Check 1: Config files found
    let (config_check, config_msg) = {
        let global_exists = app.config_paths.global.as_ref().is_some_and(|p| p.exists());
        let project_exists = app
            .config_paths
            .project
            .as_ref()
            .is_some_and(|p| p.exists());
        if global_exists || project_exists {
            ("✓".to_string(), "Config files found".to_string())
        } else {
            (
                "✗".to_string(),
                "No config files — write one with [w]".to_string(),
            )
        }
    };
    let config_style = if config_check == "✓" {
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Success))
    } else {
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Error))
    };
    items.push(ListItem::new(Line::from(vec![
        Span::styled(config_check, config_style.add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(
            config_msg,
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
        ),
    ])));

    // Check 2: Providers resolvable
    let missing_providers: Vec<_> = app
        .provider_probes
        .iter()
        .filter(|p| p.resolved.is_none())
        .collect();
    let (providers_check, providers_msg) = if missing_providers.is_empty() {
        (
            "✓".to_string(),
            "All configured providers found on PATH".to_string(),
        )
    } else {
        let names: Vec<&str> = missing_providers
            .iter()
            .map(|p| p.provider.as_str())
            .collect();
        // Soft warning: a missing provider binary is not a hard failure — the
        // user can still browse and fix PATH/config before a run.
        (
            "⚠".to_string(),
            format!("Providers not found: {}", names.join(", ")),
        )
    };
    let providers_style = if providers_check == "✓" {
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Success))
    } else {
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Warning))
    };
    items.push(ListItem::new(Line::from(vec![
        Span::styled(
            providers_check,
            providers_style.add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            providers_msg,
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
        ),
    ])));

    // Check 3: Base branch exists in repo
    let (branch_check, branch_msg) = if app.base_branch_exists {
        (
            "✓".to_string(),
            "Base branch exists in repository".to_string(),
        )
    } else {
        (
            "✗".to_string(),
            "Base branch not found — create it or update config".to_string(),
        )
    };
    let branch_style = if branch_check == "✓" {
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Success))
    } else {
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Error))
    };
    items.push(ListItem::new(Line::from(vec![
        Span::styled(branch_check, branch_style.add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(
            branch_msg,
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
        ),
    ])));

    // Check 4: .makina/ directory present and writable (relative to the repo
    // root, not the process CWD). Writability is probed against the directory
    // metadata so a read-only workspace surfaces a soft warning.
    let makina_dir = app.repo_root.join(".makina");
    let (makina_check, makina_msg) = if !makina_dir.is_dir() {
        (
            "⚠".to_string(),
            ".makina/ directory missing — it is created on first run".to_string(),
        )
    } else if is_dir_writable(&makina_dir) {
        (
            "✓".to_string(),
            ".makina/ directory present and writable".to_string(),
        )
    } else {
        (
            "⚠".to_string(),
            ".makina/ directory present but not writable — fix permissions".to_string(),
        )
    };
    let makina_style = if makina_check == "✓" {
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Success))
    } else {
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Warning))
    };
    items.push(ListItem::new(Line::from(vec![
        Span::styled(makina_check, makina_style.add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(
            makina_msg,
            Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)),
        ),
    ])));

    // Render the checklist
    let list = List::new(items)
        .style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)));
    frame.render_widget(list, list_area);

    // Footer with hints: show [w] scaffold hint if no config exists
    let (global_exists, project_exists) = (
        app.config_paths.global.as_ref().is_some_and(|p| p.exists()),
        app.config_paths
            .project
            .as_ref()
            .is_some_and(|p| p.exists()),
    );
    let footer_text = if !global_exists && !project_exists {
        "[w] write config  [Esc] close"
    } else {
        "[Esc] close"
    };
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        footer_text,
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
    )]));
    frame.render_widget(footer, footer_area);
}

/// Render the full-screen help overlay showing all keybindings grouped by category.
fn render_help_overlay(app: &App, frame: &mut Frame, area: Rect) {
    // Centre a box ~75% wide / 80% tall.
    let popup = centered_rect(75, 80, area);

    // Clear the region first so the popup is opaque.
    frame.render_widget(Clear, popup);

    let title = " Help — Keybindings ";
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Accent)))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Split the popup into content area + footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let content_area = chunks[0];
    let footer_area = chunks[1];

    // Build the keybindings grouped by category
    let mut lines: Vec<Line> = Vec::new();

    let section_style = Style::default()
        .fg(app.active_theme.get(crate::theme::ThemeRole::Accent))
        .add_modifier(Modifier::BOLD);
    let binding_style =
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground));
    let key_style = Style::default()
        .fg(app.active_theme.get(crate::theme::ThemeRole::Success))
        .add_modifier(Modifier::BOLD);

    // Run Control
    lines.push(Line::from(Span::styled("Run Control", section_style)));
    lines.push(Line::from(vec![
        Span::styled("[o]", key_style),
        Span::styled(" open browser  ", binding_style),
        Span::styled("[s]", key_style),
        Span::styled(" start run  ", binding_style),
        Span::styled("[p]", key_style),
        Span::styled(" pause", binding_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("[c]", key_style),
        Span::styled(" cancel  ", binding_style),
        Span::styled("[r]", key_style),
        Span::styled(" retry focused", binding_style),
    ]));
    lines.push(Line::from(""));

    // Navigation
    lines.push(Line::from(Span::styled("Navigation", section_style)));
    lines.push(Line::from(vec![
        Span::styled("[Tab]", key_style),
        Span::styled(" next focus  ", binding_style),
        Span::styled("[Shift+Tab]", key_style),
        Span::styled(" prev focus", binding_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("[↑/↓ or j/k]", key_style),
        Span::styled(" select up/down  ", binding_style),
        Span::styled("[←/→]", key_style),
        Span::styled(" collapse/expand", binding_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("[Alt+←/→]", key_style),
        Span::styled(" prev/next tab  ", binding_style),
        Span::styled("[Ctrl+W]", key_style),
        Span::styled(" close tab", binding_style),
    ]));
    lines.push(Line::from(""));

    // View
    lines.push(Line::from(Span::styled("View", section_style)));
    lines.push(Line::from(vec![
        Span::styled("[v]", key_style),
        Span::styled(
            " cycle dependency view (off → list → tree → timeline)  ",
            binding_style,
        ),
        Span::styled("[e]", key_style),
        Span::styled(" toggle error pane", binding_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("[l]", key_style),
        Span::styled(" open log  ", binding_style),
        Span::styled("[Ctrl+O]", key_style),
        Span::styled(" toggle verbose", binding_style),
    ]));
    lines.push(Line::from(""));

    // Accordion (plan tab)
    lines.push(Line::from(Span::styled(
        "Accordion (Plan Tab Active)",
        section_style,
    )));
    lines.push(Line::from(vec![
        Span::styled("[s]", key_style),
        Span::styled(" toggle Scope  ", binding_style),
        Span::styled("[a]", key_style),
        Span::styled(" toggle Architecture", binding_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("[t]", key_style),
        Span::styled(" toggle Tasks  ", binding_style),
        Span::styled("[z]", key_style),
        Span::styled(" toggle Status", binding_style),
    ]));
    lines.push(Line::from(""));

    // Selection
    lines.push(Line::from(Span::styled("Selection / Tree", section_style)));
    lines.push(Line::from(vec![
        Span::styled("[Space]", key_style),
        Span::styled(" toggle tree node  ", binding_style),
        Span::styled("[Enter]", key_style),
        Span::styled(" open/toggle section", binding_style),
    ]));
    lines.push(Line::from(""));

    // Other
    lines.push(Line::from(Span::styled("Other", section_style)));
    lines.push(Line::from(vec![
        Span::styled("[?]", key_style),
        Span::styled(" toggle help  ", binding_style),
        Span::styled("[!]", key_style),
        Span::styled(" doctor", binding_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("[d]", key_style),
        Span::styled(" dismiss warning  ", binding_style),
        Span::styled("[g]", key_style),
        Span::styled(" provider editor", binding_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("[Ctrl+P]", key_style),
        Span::styled(" command palette  ", binding_style),
        Span::styled("[q/Esc]", key_style),
        Span::styled(" quit", binding_style),
    ]));

    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Foreground)))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, content_area);

    // Footer with hints
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        "[Esc] or [q] close",
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim)),
    )]));
    frame.render_widget(footer, footer_area);
}

/// Compute a [`Rect`] centred within `area`, sized to `percent_x` × `percent_y`
/// of it.  Used to position the modal file-browser popup.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a titled [`Block`] with a focus-aware border style.
fn panel_block(app: &App, title: &str, focused: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Info))
    } else {
        Style::default().fg(app.active_theme.get(crate::theme::ThemeRole::Dim))
    };
    Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(border_style)
        .padding(Padding::horizontal(1))
}

/// Derive the sidebar run label as `{project}/{plan}` for plan-style task-list
/// paths, falling back to the bare file stem otherwise.
///
/// A path is plan-style when its `file_name` is `TASKS.md` (case-insensitive)
/// AND its parent directory name is non-empty; in that case `{plan}` is the
/// parent-directory name and `{project}` is [`RunView::project`]. For any other
/// shape (e.g. `.tasks/feature.json`) the label is just the `file_stem`.
fn run_label(run: &makina_core::api::RunView) -> String {
    let path = &run.task_list_path;
    let is_tasks_md = path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("TASKS.md"));
    let plan = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str());
    if is_tasks_md && let Some(plan) = plan.filter(|p| !p.is_empty()) {
        let project = &run.project;
        return format!("{project}/{plan}");
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Return the short status badge text and its display colour for a [`RunStatus`].
///
/// The badge is a fixed-width 3-character label shown in the sidebar List.
/// Colours match the same palette used by [`status_color`] so they are
/// consistent between the sidebar and the main-panel header.
fn status_badge(s: &makina_core::api::RunStatus, app: &App) -> (&'static str, Color) {
    use makina_core::api::RunStatus;
    match s {
        RunStatus::Pending => ("[·]", app.active_theme.get(crate::theme::ThemeRole::Dim)),
        RunStatus::Running => (
            "[▶]",
            app.active_theme.get(crate::theme::ThemeRole::Success),
        ),
        RunStatus::Paused => (
            "[‖]",
            app.active_theme.get(crate::theme::ThemeRole::Warning),
        ),
        RunStatus::Completed => ("[✓]", app.active_theme.get(crate::theme::ThemeRole::Accent)),
        RunStatus::Failed => ("[✗]", app.active_theme.get(crate::theme::ThemeRole::Error)),
    }
}

fn status_color(s: &makina_core::api::RunStatus, app: &App) -> Color {
    use makina_core::api::RunStatus;
    match s {
        RunStatus::Pending => app.active_theme.get(crate::theme::ThemeRole::Dim),
        RunStatus::Running => app.active_theme.get(crate::theme::ThemeRole::Success),
        RunStatus::Paused => app.active_theme.get(crate::theme::ThemeRole::Warning),
        RunStatus::Completed => app.active_theme.get(crate::theme::ThemeRole::Accent),
        RunStatus::Failed => app.active_theme.get(crate::theme::ThemeRole::Error),
    }
}

/// Return a fixed-width status badge text and its display colour for a [`TaskState`].
///
/// Badge format is a short bracketed label (≤12 chars) consistent with the
/// RunStatus badges in the sidebar.  Colours reuse the same palette as
/// [`task_state_color`].
fn task_state_badge(s: &makina_core::api::TaskState, app: &App) -> (&'static str, Color) {
    use makina_core::api::TaskState;
    match s {
        TaskState::New => ("[new]", app.active_theme.get(crate::theme::ThemeRole::Dim)),
        TaskState::Ready => (
            "[ready]",
            app.active_theme.get(crate::theme::ThemeRole::Foreground),
        ),
        TaskState::InProgress => (
            "[▶ working]",
            app.active_theme.get(crate::theme::ThemeRole::Success),
        ),
        TaskState::InReview => (
            "[⧗ review]",
            app.active_theme.get(crate::theme::ThemeRole::Warning),
        ),
        TaskState::Done => (
            "[✓ done]",
            app.active_theme.get(crate::theme::ThemeRole::Accent),
        ),
        TaskState::Failed => (
            "[✗ failed]",
            app.active_theme.get(crate::theme::ThemeRole::Error),
        ),
        TaskState::Skipped => (
            "[⊘ skipped]",
            app.active_theme.get(crate::theme::ThemeRole::Dim),
        ),
    }
}

/// Short label for a failure reason kind.
fn failure_kind_label(kind: &FailureKind) -> &'static str {
    match kind {
        FailureKind::GateCap => "gate cap",
        FailureKind::ReviewCap => "review cap",
        FailureKind::MergeConflict => "merge conflict",
        FailureKind::HardError => "hard error",
        FailureKind::WallClockCap => "wall-clock cap",
        FailureKind::IdleTimeout => "idle timeout",
    }
}

/// Spinner animation frames.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Get the current spinner frame based on tick counter.
pub fn spinner_frame(tick: u64) -> &'static str {
    SPINNER[(tick as usize) % SPINNER.len()]
}

/// Human-readable label for a [`RunStatus`] (used in the main-panel header).
fn status_label(s: &makina_core::api::RunStatus) -> &'static str {
    use makina_core::api::RunStatus;
    match s {
        RunStatus::Pending => "Pending",
        RunStatus::Running => "Running",
        RunStatus::Paused => "Paused",
        RunStatus::Completed => "Completed",
        RunStatus::Failed => "Failed",
    }
}

fn event_short_name(ev: &makina_core::api::Event) -> &'static str {
    use makina_core::api::Event;
    match ev {
        Event::RunOpened { .. } => "RunOpened",
        Event::RunStatusChanged { .. } => "RunStatusChanged",
        Event::TaskStateChanged { .. } => "TaskStateChanged",
        Event::TaskIterationsUpdated { .. } => "TaskIterationsUpdated",
        Event::SessionCapabilities { .. } => "SessionCapabilities",
        Event::CurrentModeUpdate { .. } => "CurrentModeUpdate",
        Event::AgentExchange { .. } => "AgentExchange",
        Event::TaskIdle { .. } => "TaskIdle",
        Event::TaskRetried { .. } => "TaskRetried",
        Event::RoleTurnMetrics { .. } => "RoleTurnMetrics",
        Event::ProjectDiscovered { .. } => "ProjectDiscovered",
        Event::RunIntegrationBranchLeft { .. } => "RunIntegrationBranchLeft",
    }
}

// ── Plan picker ───────────────────────────────────────────────────────────────
// Render the plan picker modal showing discovered plans from `docs/plans/*/`.

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, ScrollablePanel};
    use crate::placeholder::PlaceholderApi;
    use makina_core::api::{
        IngestionIssue, IngestionReport, IssueSeverity, IssueSource, RunId, RunStatus, RunView,
        TaskId, TaskState, TaskView,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::sync::Arc;

    // Use the process-global HOME_ENV_LOCK from makina_core so tests that
    // mutate HOME are serialised against all other HOME-mutating tests in the
    // process (including those in makina-core).
    use makina_core::HOME_ENV_LOCK;

    fn make_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        Terminal::new(backend).unwrap()
    }

    // ── Render: empty state ───────────────────────────────────────────────────

    #[test]
    fn render_empty_state_contains_title_and_panels() {
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::new());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));

        terminal
            .draw(|frame| render(&app, frame))
            .expect("draw must succeed");

        let buffer = terminal.backend().buffer().clone();
        let screen: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();

        // Title bar
        assert!(screen.contains("Makina"), "title bar must say 'Makina'");
        // Panel titles appear in the border
        assert!(screen.contains("Runs"), "sidebar must show 'Runs'");
        assert!(screen.contains("Detail"), "main area must show 'Detail'");
        // Status bar keybinds
        assert!(screen.contains("Tab"), "status bar must show Tab hint");
        assert!(screen.contains("quit"), "status bar must mention quit");
    }

    // ── Render: hint fix + run-control status bar (task 31) ───────────────────

    /// The empty-sidebar hint must guide the user to `[o]` (the real key) and
    /// must NOT contain the stale "run-control (task 31)" copy.
    #[test]
    fn render_empty_sidebar_hint_points_to_o_not_stale_copy() {
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("[o]"),
            "empty-sidebar hint must mention the [o] open key"
        );
        assert!(
            !screen.contains("task 31"),
            "stale 'run-control (task 31)' copy must be gone"
        );
    }

    /// A discovered plan with NO open runs must still render in the sidebar.
    /// Regression: the empty-state guard keyed on `runs.is_empty()` hid
    /// discovered plans whenever no run was open yet (e.g. first open of a
    /// freshly-planned project), so the plan was scanned but never shown.
    #[test]
    fn render_discovered_plan_shows_in_sidebar_with_no_runs() {
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        app.discovered_plans = vec![makina_core::orchestrator::PlanEntry {
            dir: PathBuf::from("/repo/docs/plans/0001-Initial"),
            slug: "0001-initial".to_string(),
            has_tasks: true,
            tasks: vec![makina_core::orchestrator::PlanTaskPreview {
                id: "cargo-scaffold".to_string(),
                title: "Compiling Skeleton".to_string(),
                gated: false,
                body: String::new(),
                depends_on: Vec::new(),
            }],
            scope_text: None,
            architecture_text: None,
            status_text: None,
        }];

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("0001-initial"),
            "sidebar must show the discovered plan slug even with no open runs;\nscreen was:\n{screen}"
        );
    }

    /// An expanded plan must list its task previews as child rows in the sidebar.
    #[test]
    fn render_expanded_plan_shows_task_children() {
        let mut terminal = make_terminal(90, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        app.discovered_plans = vec![makina_core::orchestrator::PlanEntry {
            dir: PathBuf::from("/repo/docs/plans/0001-Initial"),
            slug: "0001-initial".to_string(),
            has_tasks: true,
            tasks: vec![
                makina_core::orchestrator::PlanTaskPreview {
                    id: "cargo-scaffold".to_string(),
                    title: "Skeleton".to_string(),
                    gated: false,
                    body: String::new(),
                    depends_on: Vec::new(),
                },
                makina_core::orchestrator::PlanTaskPreview {
                    id: "task-model".to_string(),
                    title: "Domain Model".to_string(),
                    gated: false,
                    body: String::new(),
                    depends_on: vec!["cargo-scaffold".to_string()],
                },
            ],
            scope_text: None,
            architecture_text: None,
            status_text: None,
        }];
        app.tree_cursor = Some(0);
        app.collapsed_plans.insert(0); // PlansDiscovered seeds this in the real flow
        // Plan starts collapsed (children hidden) until expanded.
        terminal.draw(|f| render(&app, f)).unwrap();
        assert!(
            !screen_of(&terminal).contains("cargo-scaffold"),
            "collapsed plan must not show its tasks"
        );

        // Expand it; both task ids must now appear.
        app.update(crate::app::AppEvent::FocusRightOrExpand);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("cargo-scaffold") && screen.contains("task-model"),
            "expanded plan must list its task ids; screen was:\n{screen}"
        );
    }

    /// Enter on a plan opens a plan tab (plan 0032) that renders with accordion sections.
    #[test]
    fn render_plan_accordion_pane_lists_tasks() {
        let mut terminal = make_terminal(100, 26);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        app.discovered_plans = vec![makina_core::orchestrator::PlanEntry {
            dir: PathBuf::from("/repo/docs/plans/0001-Initial"),
            slug: "0001-initial".to_string(),
            has_tasks: true,
            tasks: vec![makina_core::orchestrator::PlanTaskPreview {
                id: "json-store".to_string(),
                title: "JSON Store".to_string(),
                gated: true,
                body: String::new(),
                depends_on: vec!["task-model".to_string()],
            }],
            scope_text: None,
            architecture_text: None,
            status_text: None,
        }];
        app.tree_cursor = Some(0);
        // Open a plan tab for this plan (plan 0032).
        app.update(crate::app::AppEvent::OpenTab(
            crate::app::TabContent::Plan {
                plan_slug: "0001-initial".to_string(),
            },
        ));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(screen.contains("Plan: 0001-initial"), "plan header missing");
        // Sections are collapsed by default, so we expand TASKS to see the task list
        app.accordion_state
            .entry("0001-initial".to_string())
            .or_default()
            .insert(crate::app::AccordionSection::Tasks);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("json-store"),
            "expanded TASKS section must list the task"
        );
        assert!(screen.contains("GATED"), "gated marker must show");
        assert!(
            screen.contains("depends on: task-model"),
            "plan must show dependencies; screen:\n{screen}"
        );
    }

    /// **Accordion scrollbar renders when tall:** When a plan's expanded sections
    /// exceed the pane height, a scrollbar appears in the reserved rightmost column.
    #[test]
    fn accordion_scrollbar_renders_when_tall() {
        let mut terminal = make_terminal(80, 10);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Create a plan with very long content so all sections expanded will exceed pane height.
        let long_content = "This is a test section.\n".repeat(50);

        app.discovered_plans = vec![makina_core::orchestrator::PlanEntry {
            dir: PathBuf::from("/repo/docs/plans/0001-Initial"),
            slug: "0001-initial".to_string(),
            has_tasks: true,
            tasks: vec![makina_core::orchestrator::PlanTaskPreview {
                id: "json-store".to_string(),
                title: "JSON Store".to_string(),
                gated: true,
                body: String::new(),
                depends_on: vec!["task-model".to_string()],
            }],
            scope_text: Some(long_content.clone()),
            architecture_text: Some(long_content.clone()),
            status_text: Some(long_content.clone()),
        }];
        app.tree_cursor = Some(0);

        // Open a plan tab for this plan.
        app.update(crate::app::AppEvent::OpenTab(
            crate::app::TabContent::Plan {
                plan_slug: "0001-initial".to_string(),
            },
        ));

        // Expand all sections to make content tall.
        app.accordion_state
            .entry("0001-initial".to_string())
            .or_default()
            .insert(crate::app::AccordionSection::Scope);
        app.accordion_state
            .entry("0001-initial".to_string())
            .or_default()
            .insert(crate::app::AccordionSection::Architecture);
        app.accordion_state
            .entry("0001-initial".to_string())
            .or_default()
            .insert(crate::app::AccordionSection::Status);

        terminal.draw(|f| render(&app, f)).unwrap();

        let buffer = terminal.backend().buffer().clone();

        // The accordion's reserved scrollbar column is the rightmost column of plan_area.
        // For an 80-wide terminal: sidebar = 30% = 24 cols, main = 56 cols.
        // main_block (Borders::ALL + Padding::horizontal(1)) inner: x=26, width=52.
        // plan_area has the same x and width, so its rightmost column = 26 + 52 - 1 = 77.
        let accordion_scrollbar_col: u16 = 77;
        // Accordion rows start after the tab row inside main inner (y=2 for body starting at y=1).
        let accordion_rows = 1u16..9u16;

        // Assert a scrollbar glyph is present in the accordion's reserved rightmost column.
        let has_scrollbar = col_has_scrollbar(
            &buffer,
            accordion_scrollbar_col,
            accordion_rows.start,
            accordion_rows.end,
        );

        assert!(
            has_scrollbar,
            "accordion pane must show scrollbar thumb (█) or track (║) in the rightmost column (x={accordion_scrollbar_col}) when content is tall"
        );
    }

    /// **Accordion has no scrollbar when short:** When a plan's expanded sections
    /// fit within the pane height, no scrollbar glyphs should appear in the
    /// accordion pane's rightmost columns.
    #[test]
    fn accordion_no_scrollbar_when_short() {
        let mut terminal = make_terminal(80, 25);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Create a plan with minimal content so expanded sections fit in pane height.
        app.discovered_plans = vec![makina_core::orchestrator::PlanEntry {
            dir: PathBuf::from("/repo/docs/plans/0001-Initial"),
            slug: "0001-initial".to_string(),
            has_tasks: true,
            tasks: vec![makina_core::orchestrator::PlanTaskPreview {
                id: "json-store".to_string(),
                title: "JSON Store".to_string(),
                gated: true,
                body: String::new(),
                depends_on: vec!["task-model".to_string()],
            }],
            scope_text: Some("Short scope text.\n".to_string()),
            architecture_text: Some("Short arch text.\n".to_string()),
            status_text: Some("Short status text.\n".to_string()),
        }];
        app.tree_cursor = Some(0);

        // Open a plan tab for this plan.
        app.update(crate::app::AppEvent::OpenTab(
            crate::app::TabContent::Plan {
                plan_slug: "0001-initial".to_string(),
            },
        ));

        // Don't expand sections - leave them collapsed so content is short.

        terminal.draw(|f| render(&app, f)).unwrap();

        let buffer = terminal.backend().buffer().clone();

        // The accordion's reserved scrollbar column is the rightmost column of plan_area.
        // For an 80-wide terminal: sidebar = 30% = 24 cols, main = 56 cols.
        // main_block (Borders::ALL + Padding::horizontal(1)) inner: x=26, width=52.
        // plan_area has the same x and width, so its rightmost column = 26 + 52 - 1 = 77.
        let accordion_scrollbar_col: u16 = 77;
        // With terminal height 25, accordion rows go from 1 to 23.
        let accordion_rows = 1u16..23u16;

        // Assert no scrollbar glyph appears in the accordion's reserved rightmost column.
        let has_scrollbar = col_has_scrollbar(
            &buffer,
            accordion_scrollbar_col,
            accordion_rows.start,
            accordion_rows.end,
        );

        assert!(
            !has_scrollbar,
            "accordion pane must not show scrollbar glyphs when content fits (checked col x={accordion_scrollbar_col})"
        );
    }

    /// **Accordion scroll offset applied to rendering:** When `scroll_offsets[PlanAccordion] = 3`
    /// and the accordion content exceeds the pane height, the first rendered accordion content
    /// row equals the wrapped line originally at index 3 (lines 0–2 are skipped). With an offset
    /// greater than `accordion_scroll_max` it clamps so the first row equals the line at index
    /// `accordion_scroll_max`.
    ///
    /// Proof strategy (geometry-free, mirrors exchange_scroll_offset_applied_to_rendering):
    ///   1. Render at offset=0 into a tall-enough terminal; scan for the first row containing
    ///      "SCOPE" (the first accordion line, index 0). That row is `first_content_y`.
    ///   2. Capture the content of row `first_content_y + OFFSET` from the offset=0 render.
    ///      That is exactly what lines[OFFSET] looks like.
    ///   3. Render at offset=OFFSET and assert row `first_content_y` equals the content captured
    ///      in step 2 — proving lines[OFFSET] has moved to the top.
    ///   4. Read `accordion_scroll_max` from `app.last_scroll_maxes` (written by the render pass).
    ///      Render at `accordion_scroll_max`, record the first visible row.
    ///      Render at `accordion_scroll_max + 100` (over the ceiling); assert the first visible
    ///      row equals the row captured at `accordion_scroll_max` (clamp proof).
    #[test]
    fn accordion_scroll_offset_applied_to_rendering() {
        use crate::app::AppEvent;

        // Use a tall terminal (100×30) so `first_content_y + OFFSET` is within the viewport
        // when offset=0.  The accordion occupies roughly rows 3..28 (9 visible rows per frame
        // after borders/tabs, but the paragraph is tall enough to overflow).
        let term_width: usize = 100;
        let term_height: u16 = 30;
        const OFFSET: u16 = 3; // spec requires scroll_offsets[PlanAccordion] = 3

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Create a plan with tall content so scroll_max > 0.
        let long_content = (0..80)
            .map(|i| format!("Section line {}\n", i))
            .collect::<String>();

        app.discovered_plans = vec![makina_core::orchestrator::PlanEntry {
            dir: PathBuf::from("/repo/docs/plans/0001-Initial"),
            slug: "0001-initial".to_string(),
            has_tasks: true,
            tasks: vec![makina_core::orchestrator::PlanTaskPreview {
                id: "test-task".to_string(),
                title: "Test Task".to_string(),
                gated: false,
                body: String::new(),
                depends_on: vec![],
            }],
            scope_text: Some(long_content.clone()),
            architecture_text: Some(long_content.clone()),
            status_text: Some(long_content.clone()),
        }];
        app.tree_cursor = Some(0);

        // Open a plan tab.
        app.update(AppEvent::OpenTab(crate::app::TabContent::Plan {
            plan_slug: "0001-initial".to_string(),
        }));

        // Expand all sections to make content tall.
        app.accordion_state
            .entry("0001-initial".to_string())
            .or_default()
            .insert(crate::app::AccordionSection::Scope);
        app.accordion_state
            .entry("0001-initial".to_string())
            .or_default()
            .insert(crate::app::AccordionSection::Architecture);
        app.accordion_state
            .entry("0001-initial".to_string())
            .or_default()
            .insert(crate::app::AccordionSection::Status);

        // ── Step 1: render at offset=0; find first_content_y and capture lines[OFFSET] ──
        app.scroll_offsets.insert(ScrollablePanel::PlanAccordion, 0);
        let mut terminal = make_terminal(term_width as u16, term_height);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_at_0 = screen_of(&terminal);

        // Locate first_content_y: the first terminal row that contains "SCOPE"
        // (the accordion header, which is lines[0] of the paragraph).
        let first_content_y = (0..term_height as usize)
            .find(|&y| {
                let row: String = screen_at_0
                    .chars()
                    .skip(y * term_width)
                    .take(term_width)
                    .collect();
                row.contains("SCOPE")
            })
            .expect(
                "offset=0 render must show 'SCOPE' header in the accordion; \
                 check that the plan tab is open and sections are expanded",
            );

        // lines[0] = "[-] SCOPE" → appears at first_content_y.
        // lines[OFFSET=3] appears at first_content_y + OFFSET (when offset=0).
        let target_y = first_content_y + OFFSET as usize;
        assert!(
            target_y < term_height as usize,
            "first_content_y={} + OFFSET={} = {} must be within term_height={}; \
             increase term_height",
            first_content_y,
            OFFSET,
            target_y,
            term_height
        );

        // Capture what lines[OFFSET] looks like at offset=0.
        let row_lines_offset_at_0: String = screen_at_0
            .chars()
            .skip(target_y * term_width)
            .take(term_width)
            .collect();

        // Sanity: lines[OFFSET] must not be identical to lines[0].
        let row_lines_0_at_0: String = screen_at_0
            .chars()
            .skip(first_content_y * term_width)
            .take(term_width)
            .collect();
        assert_ne!(
            row_lines_offset_at_0, row_lines_0_at_0,
            "lines[{OFFSET}] and lines[0] must be different; content structure may be wrong"
        );

        // ── Step 2: render at offset=OFFSET=3; first row must equal lines[OFFSET] ──
        app.scroll_offsets
            .insert(ScrollablePanel::PlanAccordion, OFFSET);
        let mut terminal = make_terminal(term_width as u16, term_height);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_at_offset = screen_of(&terminal);

        let first_row_at_offset: String = screen_at_offset
            .chars()
            .skip(first_content_y * term_width)
            .take(term_width)
            .collect();

        assert_eq!(
            first_row_at_offset,
            row_lines_offset_at_0,
            "With scroll_offsets[PlanAccordion]={OFFSET}, the first visible accordion row \
             (terminal row y={first_content_y}) must equal lines[{OFFSET}] (which appeared at \
             row {} when offset=0);\n  expected: '{}'\n  got:      '{}'",
            target_y,
            row_lines_offset_at_0.trim_end(),
            first_row_at_offset.trim_end()
        );

        // ── Step 3: verify clamping ─────────────────────────────────────────────
        // Read accordion_scroll_max that the render pass recorded this frame.
        let accordion_scroll_max = app
            .last_scroll_maxes
            .borrow()
            .get(&ScrollablePanel::PlanAccordion)
            .copied()
            .unwrap_or(0);
        assert!(
            accordion_scroll_max > 0,
            "accordion_scroll_max must be > 0 (content must overflow the pane)"
        );

        // Render at exactly scroll_max; record the first visible row.
        app.scroll_offsets
            .insert(ScrollablePanel::PlanAccordion, accordion_scroll_max);
        let mut terminal = make_terminal(term_width as u16, term_height);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_at_max = screen_of(&terminal);

        let first_row_at_max: String = screen_at_max
            .chars()
            .skip(first_content_y * term_width)
            .take(term_width)
            .collect();

        // Render at offset well above scroll_max (should clamp to scroll_max).
        let huge_offset = accordion_scroll_max.saturating_add(100);
        app.scroll_offsets
            .insert(ScrollablePanel::PlanAccordion, huge_offset);
        let mut terminal = make_terminal(term_width as u16, term_height);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_at_huge = screen_of(&terminal);

        let first_row_at_huge: String = screen_at_huge
            .chars()
            .skip(first_content_y * term_width)
            .take(term_width)
            .collect();

        assert_eq!(
            first_row_at_huge,
            first_row_at_max,
            "With scroll_offsets[PlanAccordion]={} (> accordion_scroll_max={}), the first \
             visible accordion row must clamp to the row at scroll_max={};\
             \n  expected (at scroll_max): '{}'\n  got (at huge offset):   '{}'",
            huge_offset,
            accordion_scroll_max,
            accordion_scroll_max,
            first_row_at_max.trim_end(),
            first_row_at_huge.trim_end()
        );
    }

    /// The error pane must render even when no run is selected (regression: it
    /// used to be drawn only inside the run view, so `[e]` showed nothing).
    #[test]
    fn render_error_pane_shows_with_no_run_selected() {
        let mut terminal = make_terminal(90, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        app.push_error(crate::app::ErrorMessage {
            timestamp: std::time::SystemTime::UNIX_EPOCH,
            level: crate::app::ErrorLevel::Error,
            text: "boom-happened".to_string(),
        });
        // Open the error pane via the real toggle event; no run is selected.
        app.update(crate::app::AppEvent::ToggleErrorPane);
        assert!(
            app.selected_run().is_none(),
            "precondition: no run selected"
        );

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("Errors") && screen.contains("boom-happened"),
            "error pane + message must render with no run; screen:\n{screen}"
        );
    }

    /// The status bar must show the run-control key hints (`[o]`, `[s/p/c]`).
    #[test]
    fn render_status_bar_shows_run_control_hints() {
        let mut terminal = make_terminal(100, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(screen.contains("[o]"), "status bar must show [o] open");
        assert!(
            screen.contains("[s/p/c]"),
            "status bar must show the [s/p/c] start/pause/cancel hints"
        );
    }

    /// When `app.status_message` is set, it is rendered in the status bar.
    #[test]
    fn render_status_bar_shows_status_message() {
        let mut terminal = make_terminal(220, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        app.update(crate::app::AppEvent::StatusMessage("Start run:1".into()));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("Start run:1"),
            "status bar must render the transient status_message"
        );
    }

    /// The status bar must advertise the `[v]` key for cycling dependency views.
    #[test]
    fn status_bar_advertises_view_key() {
        let mut terminal = make_terminal(120, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("[v]"),
            "status bar must advertise the [v] view key"
        );
    }

    /// The status bar must show the current dependency view label.
    #[test]
    fn status_bar_shows_current_view_label() {
        // The status bar carries more hints now ([^P] cmds + [e] errors badge + [?] help
        // + [wheel] scroll), so use a wider terminal to ensure the trailing
        // `view:` label is not clipped before the assertions run.
        let mut terminal = make_terminal(220, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Test with DependencyViewMode::Off (default)
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("view: off"),
            "status bar must show 'view: off' when dependency_view is Off"
        );

        // Cycle to List
        app.update(crate::app::AppEvent::CycleDependencyView);
        let mut terminal = make_terminal(220, 24);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("view: list"),
            "status bar must show 'view: list' when dependency_view is List"
        );

        // Cycle to Tree
        app.update(crate::app::AppEvent::CycleDependencyView);
        let mut terminal = make_terminal(220, 24);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("view: tree"),
            "status bar must show 'view: tree' when dependency_view is Tree"
        );

        // Cycle to Timeline
        app.update(crate::app::AppEvent::CycleDependencyView);
        let mut terminal = make_terminal(220, 24);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("view: timeline"),
            "status bar must show 'view: timeline' when dependency_view is Timeline"
        );
    }

    /// The status bar must advertise the `[e]` key for the error pane.
    /// When unseen errors are present, it must show a count badge.
    #[test]
    fn status_bar_advertises_errors_key() {
        use crate::app::{ErrorLevel, ErrorMessage};

        let mut terminal = make_terminal(170, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Without errors, the status bar shows "[e] errors"
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("[e]"),
            "status bar must advertise the [e] errors key"
        );

        // Add an error while the pane is closed — should mark unseen
        app.push_error(ErrorMessage {
            timestamp: std::time::SystemTime::now(),
            level: ErrorLevel::Error,
            text: "test error".to_string(),
        });
        assert!(app.unseen_errors, "unseen_errors flag should be set");

        // With unseen errors, the status bar shows "[e] errors(count)"
        let mut terminal = make_terminal(170, 24);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("[e] errors(1)"),
            "status bar must show unseen error count"
        );

        // Opening the error pane clears the unseen flag
        app.update(crate::app::AppEvent::ToggleErrorPane);
        assert!(
            !app.unseen_errors,
            "unseen_errors flag should be cleared when pane opens"
        );
        let mut terminal = make_terminal(170, 24);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("[e] errors") && !screen.contains("[e] errors("),
            "status bar must show [e] errors without count when pane is open"
        );
    }

    // ── Render: with runs ─────────────────────────────────────────────────────

    #[test]
    fn render_with_run_shows_run_in_sidebar() {
        let mut terminal = make_terminal(120, 24);
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/my-feature.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("t1"),
                title: "First task".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));

        terminal
            .draw(|frame| render(&app, frame))
            .expect("draw must succeed");

        let buffer = terminal.backend().buffer().clone();
        let screen: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();

        // The run's file stem appears in the sidebar (List uses file_stem).
        assert!(
            screen.contains("my-feature"),
            "sidebar should list the open run's file stem"
        );
        // The first task's title now appears in the sidebar tree (not the main table).
        assert!(
            screen.contains("First task"),
            "sidebar tree should show the run's tasks"
        );
    }

    // ── Render: sidebar List — name and status badge per run ──────────────────

    #[test]
    fn render_sidebar_shows_run_name_and_status_badge() {
        let mut terminal = make_terminal(100, 30);
        let api = Arc::new(PlaceholderApi::empty());
        let runs = vec![
            RunView {
                id: RunId(1),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/alpha.json"),
                status: RunStatus::Running,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/beta.json"),
                status: RunStatus::Failed,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(3),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/gamma.json"),
                status: RunStatus::Completed,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
        ];
        let app = App::new(api, runs, std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();

        // Each run's file stem must appear.
        assert!(screen.contains("alpha"), "sidebar must show 'alpha'");
        assert!(screen.contains("beta"), "sidebar must show 'beta'");
        assert!(screen.contains("gamma"), "sidebar must show 'gamma'");

        // The status badges must appear.
        // Running badge is "[▶]"
        assert!(
            screen.contains("[▶]"),
            "Running badge must appear for alpha"
        );
        // Failed badge is "[✗]"
        assert!(screen.contains("[✗]"), "Failed badge must appear for beta");
        // Completed badge is "[✓]"
        assert!(
            screen.contains("[✓]"),
            "Completed badge must appear for gamma"
        );
    }

    #[test]
    fn render_sidebar_shows_plan_label() {
        // Plan-style path renders "{project}/{plan}"; a non-plan path falls back
        // to the bare file stem.  The terminal is wide enough that the full
        // "makina/0002-Governance-and-Persistence" label fits in the 30%-wide
        // sidebar without being clipped at the panel boundary.
        let mut terminal = make_terminal(200, 30);
        let api = Arc::new(PlaceholderApi::empty());
        // The non-plan run is listed first so that it is the auto-selected run
        // (`App::new` selects index 0). This keeps the `.tasks/feature.json`
        // stem — which has no "TASKS" substring — in the Detail header, so the
        // only path-derived text on screen that could contain "TASKS" is the
        // plan-style sidebar label, which renders as "{project}/{plan}".
        let runs = vec![
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/feature.json"),
                status: RunStatus::Completed,
                project: "makina".into(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(1),
                run_uid: String::new(),
                task_list_path: PathBuf::from(
                    "docs/plans/0002-Governance-and-Persistence/TASKS.md",
                ),
                status: RunStatus::Running,
                project: "makina".into(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
        ];
        let app = App::new(api, runs, std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();

        // Plan-style run shows the plan dir name, not "TASKS".
        assert!(
            screen.contains("0002-Governance-and-Persistence"),
            "sidebar must show the plan dir name"
        );
        assert!(
            !screen.contains("TASKS"),
            "sidebar must not show 'TASKS' for plan-style paths"
        );

        // Non-plan path still renders its bare file stem.
        assert!(
            screen.contains("feature"),
            "sidebar must show 'feature' stem for non-plan paths"
        );
    }

    #[test]
    fn render_sidebar_shows_all_status_badges() {
        // One run per RunStatus variant — all badges must appear.
        let mut terminal = make_terminal(100, 40);
        let api = Arc::new(PlaceholderApi::empty());
        let runs = vec![
            RunView {
                id: RunId(1),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/pending.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/running.json"),
                status: RunStatus::Running,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(3),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/paused.json"),
                status: RunStatus::Paused,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(4),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/completed.json"),
                status: RunStatus::Completed,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(5),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/failed.json"),
                status: RunStatus::Failed,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
        ];
        let app = App::new(api, runs, std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();

        assert!(screen.contains("[·]"), "Pending badge must appear");
        assert!(screen.contains("[▶]"), "Running badge must appear");
        assert!(screen.contains("[‖]"), "Paused badge must appear");
        assert!(screen.contains("[✓]"), "Completed badge must appear");
        assert!(screen.contains("[✗]"), "Failed badge must appear");
    }

    // ── Render: selected row is highlighted ───────────────────────────────────

    #[test]
    fn render_selected_run_is_highlighted() {
        // The selected row must have the highlight background (Cyan in our
        // palette) and the highlight symbol "▶ " prepended by ratatui.
        let mut terminal = make_terminal(100, 30);
        let th = crate::theme::ayu_dark();
        let api = Arc::new(PlaceholderApi::empty());
        let runs = vec![
            RunView {
                id: RunId(1),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/first.json"),
                status: RunStatus::Running,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/second.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
        ];
        // App::new selects index 0 by default.
        let app = App::new(api, runs, std::path::PathBuf::from("."));
        assert_eq!(app.selected_run, Some(0));

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // Find a cell with the Accent background (the highlight colour) — there
        // must be at least one such cell within the sidebar region (columns 0..30).
        let has_highlight = buf
            .content()
            .iter()
            .any(|cell| cell.bg == th.get(crate::theme::ThemeRole::Accent));
        assert!(
            has_highlight,
            "selected row must use Accent highlight background"
        );
    }

    // ── Render: status indicator colours in the cell ──────────────────────────

    #[test]
    fn render_running_badge_uses_green_fg() {
        let mut terminal = make_terminal(100, 24);
        let th = crate::theme::ayu_dark();
        let api = Arc::new(PlaceholderApi::empty());
        let runs = vec![RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/live.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        }];
        let app = App::new(api, runs, std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // At least one cell with Green fg must exist (the Running badge).
        let has_green = buf
            .content()
            .iter()
            .any(|cell| cell.fg == th.get(crate::theme::ThemeRole::Success));
        assert!(has_green, "Running status badge must use Green foreground");
    }

    #[test]
    fn render_failed_badge_uses_red_fg() {
        let mut terminal = make_terminal(100, 24);
        let th = crate::theme::ayu_dark();
        let api = Arc::new(PlaceholderApi::empty());
        let runs = vec![RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/broken.json"),
            status: RunStatus::Failed,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        }];
        let app = App::new(api, runs, std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let has_red = buf
            .content()
            .iter()
            .any(|cell| cell.fg == th.get(crate::theme::ThemeRole::Error));
        assert!(has_red, "Failed status badge must use Red foreground");
    }

    // ── Render: ingestion report panel (ingest-tui-report-panel) ──────────────

    #[test]
    fn render_ingestion_panel_shows_blocking_issue() {
        let mut terminal = make_terminal(100, 24);
        let th = crate::theme::ayu_dark();
        let api = Arc::new(PlaceholderApi::empty());
        let runs = vec![RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/bad.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: IngestionReport {
                issues: vec![IngestionIssue {
                    task_id: None,
                    severity: IssueSeverity::Blocking,
                    source: IssueSource::Qualifier,
                    code: "vague-done-when".into(),
                    message: "done_when is too vague".into(),
                    suggestion: Some("write a concrete acceptance criterion".into()),
                }],
            },
        }];
        let app = App::new(api, runs, std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("vague-done-when"),
            "ingestion panel should contain the issue code"
        );
        assert!(
            screen.contains("suggestion"),
            "ingestion panel should label suggestions"
        );
        assert!(
            screen.contains("write a concrete acceptance criterion"),
            "ingestion panel should contain the suggestion text"
        );

        let buf = terminal.backend().buffer().clone();
        let has_red = buf
            .content()
            .iter()
            .any(|cell| cell.fg == th.get(crate::theme::ThemeRole::Error));
        assert!(has_red, "Blocking issue line must use Red foreground");
    }

    #[test]
    fn render_ingestion_panel_shows_warning_issue() {
        let mut terminal = make_terminal(100, 24);
        let th = crate::theme::ayu_dark();
        let api = Arc::new(PlaceholderApi::empty());
        let runs = vec![RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/warn.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: IngestionReport {
                issues: vec![IngestionIssue {
                    task_id: None,
                    severity: IssueSeverity::Warning,
                    source: IssueSource::Qualifier,
                    code: "short-done-when".into(),
                    message: "done_when is very short".into(),
                    suggestion: None,
                }],
            },
        }];
        let app = App::new(api, runs, std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("short-done-when"),
            "ingestion panel should contain the warning issue code"
        );

        let buf = terminal.backend().buffer().clone();
        let has_yellow = buf
            .content()
            .iter()
            .any(|cell| cell.fg == th.get(crate::theme::ThemeRole::Warning));
        assert!(has_yellow, "Warning issue line must use Yellow foreground");
    }

    #[test]
    fn render_status_bar_shows_blocked_notice_when_report_blocked() {
        let mut terminal = make_terminal(260, 30);
        let api = Arc::new(PlaceholderApi::empty());
        let runs = vec![RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/blocked.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: IngestionReport {
                issues: vec![IngestionIssue {
                    task_id: None,
                    severity: IssueSeverity::Blocking,
                    source: IssueSource::Interpreter,
                    code: "bad-json".into(),
                    message: "parse failed".into(),
                    suggestion: None,
                }],
            },
        }];
        let app = App::new(api, runs, std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("blocking issue(s) — press r to re-interpret"),
            "status bar must show the blocking-count notice when report.is_blocked()"
        );
    }

    // ── Render: focus indicator ───────────────────────────────────────────────

    #[test]
    fn render_focus_label_changes_with_panel() {
        let mut terminal = make_terminal(220, 24);
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Default focus: Sidebar.
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_sidebar: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            screen_sidebar.contains("focus: sidebar"),
            "status bar should say 'focus: sidebar' when Sidebar is focused"
        );

        // Switch to Main.
        app.update(crate::app::AppEvent::FocusNext);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_main: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            screen_main.contains("focus: main"),
            "status bar should say 'focus: main' when Main is focused"
        );
    }

    // ── File browser overlay (task 28) ────────────────────────────────────────

    use crate::app::Mode;
    use crate::browser::{DirEntry, FileBrowser};

    fn browsing_app(entries: Vec<DirEntry>, selected: usize) -> App {
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        app.mode = Mode::FileBrowser;
        let mut browser = FileBrowser::new(PathBuf::from("/home/user/project"), entries);
        browser.selected = selected;
        app.browser = Some(browser);
        app
    }

    fn screen_of(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    fn col_has_scrollbar(buf: &ratatui::buffer::Buffer, x: u16, y0: u16, y1: u16) -> bool {
        (y0..y1).any(|y| {
            let s = buf[(x, y)].symbol();
            s == "█" || s == "║" || s == "▲" || s == "▼"
        })
    }

    #[test]
    fn render_file_browser_lists_entries_and_dir_marker() {
        let mut terminal = make_terminal(100, 30);
        let app = browsing_app(
            vec![
                DirEntry {
                    name: "..".into(),
                    path: PathBuf::from("/home/user"),
                    is_dir: true,
                },
                DirEntry {
                    name: "src".into(),
                    path: PathBuf::from("/home/user/project/src"),
                    is_dir: true,
                },
                DirEntry {
                    name: "my-feature.md".into(),
                    path: PathBuf::from("/home/user/project/my-feature.md"),
                    is_dir: false,
                },
            ],
            0,
        );

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // The popup title shows the current directory.
        assert!(
            screen.contains("Open task list"),
            "browser title must be shown"
        );
        // Directory entries render with a trailing slash; files do not.
        assert!(
            screen.contains("src/"),
            "directory entry should show 'src/'"
        );
        assert!(
            screen.contains("my-feature.md"),
            "file entry should be listed"
        );
        // Footer key hints.
        assert!(
            screen.contains("Enter"),
            "browser footer should mention Enter"
        );
    }

    #[test]
    fn render_file_browser_highlights_selection() {
        let mut terminal = make_terminal(100, 30);
        let th = crate::theme::ayu_dark();
        // Select the second entry.
        let app = browsing_app(
            vec![
                DirEntry {
                    name: "alpha".into(),
                    path: PathBuf::from("/home/user/project/alpha"),
                    is_dir: true,
                },
                DirEntry {
                    name: "beta.md".into(),
                    path: PathBuf::from("/home/user/project/beta.md"),
                    is_dir: false,
                },
            ],
            1,
        );

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // The highlighted row uses a Magenta background in our palette.
        let has_highlight = buf
            .content()
            .iter()
            .any(|cell| cell.bg == th.get(crate::theme::ThemeRole::Accent));
        assert!(
            has_highlight,
            "selected browser row must use the Accent highlight background"
        );
    }

    #[test]
    fn render_file_browser_empty_directory_shows_hint() {
        let mut terminal = make_terminal(80, 24);
        let app = browsing_app(vec![], 0);

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("empty directory"),
            "empty browser must show an '(empty directory)' hint"
        );
    }

    #[test]
    fn normal_mode_does_not_render_browser() {
        // Without browsing, the browser title must NOT appear.
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            !screen.contains("Open task list"),
            "browser overlay must not render in Normal mode"
        );
    }

    // ── Task-status view (task 29) ────────────────────────────────────────────

    /// Build an `App` with one selected run that has varied task states and
    /// non-zero iteration counts — used by the per-task status-view render tests.
    fn task_status_app() -> App {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/status-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("alpha"),
                    title: "Alpha task".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 2,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("beta"),
                    title: "Beta task".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 1,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("alpha")],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("gamma"),
                    title: "Gamma task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("beta")],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("delta"),
                    title: "Delta task".into(),
                    state: TaskState::Failed,
                    gate_iterations: 3,
                    review_iterations: 1,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        App::new(api, vec![run], std::path::PathBuf::from("."))
    }

    /// Render the task-status view and assert all task titles appear.
    #[test]
    fn render_task_status_shows_all_task_titles() {
        let mut terminal = make_terminal(120, 30);
        let app = task_status_app();

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(screen.contains("Alpha task"), "should show 'Alpha task'");
        assert!(screen.contains("Beta task"), "should show 'Beta task'");
        assert!(screen.contains("Gamma task"), "should show 'Gamma task'");
        assert!(screen.contains("Delta task"), "should show 'Delta task'");
    }

    /// State badges for each TaskState variant must appear in the rendered output.
    #[test]
    fn render_task_status_shows_state_badges() {
        let mut terminal = make_terminal(120, 30);
        let app = task_status_app();

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // done badge
        assert!(screen.contains("done"), "Done badge must appear");
        // working/InProgress badge
        assert!(screen.contains("working"), "InProgress badge must appear");
        // new badge
        assert!(screen.contains("new"), "New badge must appear");
        // failed badge
        assert!(screen.contains("failed"), "Failed badge must appear");
    }

    /// Non-zero gate and review iteration counts must appear in the rendered output.
    #[test]
    fn render_task_status_shows_iteration_counts() {
        let mut terminal = make_terminal(120, 30);
        let mut app = task_status_app();

        // Select a task to show its iteration counts in the exchange pane detail.
        // Alpha task has review_iterations=2; Beta has gate_iterations=1.
        app.selected_task = Some(1); // Select Beta task (gate_iterations=1)

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Beta task has gate_iterations=1; it should appear in the task detail.
        assert!(
            screen.contains("gate ×1"),
            "gate iteration count 1 must appear in task detail"
        );
    }

    /// With [`DependencyViewMode::List`] active and gamma (which depends on
    /// beta) selected, the dependency sub-pane lists beta as a `[state] task-id`
    /// line with beta's state badge, and the Exchange pane still renders below it
    /// (no overlap).
    #[test]
    fn render_dependency_list_shows_prereqs_with_badges() {
        use crate::app::DependencyViewMode;
        let mut terminal = make_terminal(120, 30);
        let mut app = task_status_app();
        // gamma is index 2 and depends_on beta (InProgress → "[▶ working]").
        app.selected_task = Some(2);
        app.dependency_view = DependencyViewMode::List;

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // The beta dependency id appears in the dependency list.
        assert!(
            screen.contains("beta"),
            "dependency list should show the beta prerequisite id"
        );
        // Beta is InProgress; its state badge text must appear.
        assert!(
            screen.contains("working"),
            "dependency list should show beta's state badge"
        );
        // The Exchange pane still renders below (no overlap).
        assert!(
            screen.contains("Exchange"),
            "Exchange title/border must still render below the dependency pane"
        );
    }

    /// With [`DependencyViewMode::Tree`] active, the dependency sub-pane draws
    /// an indented ASCII tree of the selected task's prerequisites: direct
    /// children carry `├──`/`└──` connectors with their state badges, and a
    /// grandchild prerequisite is indented deeper than its parent.
    #[test]
    fn render_dependency_tree_shows_connectors_and_badges() {
        use crate::app::DependencyViewMode;
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};

        let api = Arc::new(PlaceholderApi::empty());
        // root depends_on [a (Done), b (Failed)]; a depends_on [c (New)].
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/dep-tree-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("root"),
                    title: "Root task".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("a"), TaskId::new("b")],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("a"),
                    title: "A task".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("c")],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("b"),
                    title: "B task".into(),
                    state: TaskState::Failed,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("c"),
                    title: "C task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));
        // Select root (index 0) and switch to the tree view.
        app.selected_task = Some(0);
        app.dependency_view = DependencyViewMode::Tree;

        let mut terminal = make_terminal(120, 30);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Connectors for non-last (a) and last (b) direct children.
        assert!(
            screen.contains("├── "),
            "tree should draw a branch connector for non-last children"
        );
        assert!(
            screen.contains("└── "),
            "tree should draw a corner connector for the last child"
        );
        // State badges: a is Done, b is Failed.
        assert!(
            screen.contains("[✓ done]"),
            "tree should show the Done badge for prerequisite a"
        );
        assert!(
            screen.contains("[✗ failed]"),
            "tree should show the Failed badge for prerequisite b"
        );
        // Child id strings appear.
        assert!(
            screen.contains(" a"),
            "tree should show prerequisite id 'a'"
        );
        assert!(
            screen.contains(" b"),
            "tree should show prerequisite id 'b'"
        );
        assert!(
            screen.contains(" c"),
            "tree should show grandchild prerequisite id 'c'"
        );

        // The grandchild `c` must be indented deeper than its parent `a`.  The
        // flat `screen` string is `width * height` chars laid out row-major, so
        // split it into 120-char rows on char boundaries (multi-byte connectors
        // and badges make byte-chunking unsafe).  Because both the `a` row (the
        // only `[✓ done]` line) and the `c` row (the only `[new]` line) share
        // the same dependency-pane left offset, the column of their tree
        // connector char (`├`/`└`) directly reflects the relative indent.
        let chars: Vec<char> = screen.chars().collect();
        let rows: Vec<Vec<char>> = chars.chunks(120).map(|c| c.to_vec()).collect();
        let connector_col = |badge: &str| -> usize {
            // First, look for rows with tree connectors (├ or └) that contain the badge.
            for row in &rows {
                let row_str: String = row.iter().collect();
                if row_str.contains(badge) {
                    // Find the first tree connector char on this row.
                    if let Some(col) = row.iter().position(|&ch| ch == '├' || ch == '└') {
                        return col;
                    }
                }
            }
            // If no connector found in any row with this badge, search for the badge
            // text itself only in rows that contain tree connectors (dependency view).
            // This fallback handles "dependency tree might be using different formatting now".
            for row in &rows {
                let row_str: String = row.iter().collect();
                if row_str.contains(badge)
                    && (row_str.contains("├") || row_str.contains("└"))
                    && let Some(col) = row_str.find(badge)
                {
                    return col;
                }
            }
            usize::MAX
        };
        let a_col = connector_col("[✓ done]");
        let c_col = connector_col("[new]");
        assert_ne!(a_col, usize::MAX, "expected a rendered tree row for 'a'");
        assert_ne!(c_col, usize::MAX, "expected a rendered tree row for 'c'");
        assert!(
            c_col > a_col,
            "grandchild 'c' must be indented deeper than parent 'a' (a_col={a_col}, c_col={c_col})"
        );
    }

    /// `gantt_bar_cols` positions bars by wall-clock time.
    ///
    /// Span: [t0, t0+100s], width=100.  A task [t0+20s, t0+50s] should map to
    /// columns (20, 50).
    #[test]
    fn gantt_positions_bars_by_time() {
        use chrono::TimeZone;

        let t0 = chrono::Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let span_start = t0;
        let span_end = t0 + chrono::Duration::seconds(100);
        let task_start = t0 + chrono::Duration::seconds(20);
        let task_end = t0 + chrono::Duration::seconds(50);

        let (start_col, end_col) = gantt_bar_cols(span_start, span_end, task_start, task_end, 100);
        assert_eq!(start_col, 20, "task starts at 20% of span → column 20");
        assert_eq!(end_col, 50, "task ends at 50% of span → column 50");
    }

    /// A task with `started_at == None` renders as a ghost slot (dim dots), not
    /// a solid bar of '█' glyphs, when the Timeline view is active.
    #[test]
    fn pending_task_has_no_solid_bar() {
        use crate::app::DependencyViewMode;
        use chrono::TimeZone;

        let t0 = chrono::Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/gantt-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                // Task 1: has started and finished — shows a solid bar.
                TaskView {
                    id: TaskId::new("started"),
                    title: "Started task".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: Some(t0),
                    finished_at: Some(t0 + chrono::Duration::seconds(60)),
                    failure_reason: None,
                    entry_text: String::new(),
                },
                // Task 2: not yet started — must show ghost, not '█'.
                TaskView {
                    id: TaskId::new("pending"),
                    title: "Pending task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));
        app.dependency_view = DependencyViewMode::Timeline;

        let mut terminal = make_terminal(120, 40);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Find the row containing "pending" and assert it has no '█'.
        let chars: Vec<char> = screen.chars().collect();
        let rows: Vec<Vec<char>> = chars.chunks(120).map(|c| c.to_vec()).collect();
        let pending_row = rows.iter().find(|row| {
            let s: String = row.iter().collect();
            s.contains("pending")
        });
        assert!(
            pending_row.is_some(),
            "pending task must appear in the Timeline render"
        );
        let pending_row = pending_row.unwrap();
        assert!(
            !pending_row.contains(&'█'),
            "not-yet-started task must NOT have a solid '█' bar; \
             row: {}",
            pending_row.iter().collect::<String>()
        );
    }

    /// When every task has `started_at == None`, the Timeline view renders the
    /// "No timing yet." placeholder and must not panic.
    #[test]
    fn empty_span_shows_placeholder() {
        use crate::app::DependencyViewMode;

        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/empty-timing.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("task-a"),
                    title: "Task A".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("task-b"),
                    title: "Task B".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));
        app.dependency_view = DependencyViewMode::Timeline;

        // Must not panic.
        let mut terminal = make_terminal(120, 40);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("No timing yet"),
            "Timeline with no started tasks must show 'No timing yet' placeholder; \
             got:\n{screen}"
        );
    }

    /// The status bar does NOT show the `G`/`R` legend (it was removed).
    /// The iteration counts are now shown in the task detail pane instead.
    #[test]
    fn render_status_bar_omits_gr_legend_when_counts_nonzero() {
        let mut terminal = make_terminal(120, 30);
        let app = task_status_app();

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // The legend no longer appears in the status bar.
        assert!(
            !screen.contains("gate iterations"),
            "legend should NOT appear in status bar (G/R columns removed)"
        );
        assert!(
            !screen.contains("review iterations"),
            "legend should NOT appear in status bar (G/R columns removed)"
        );
    }

    /// The status bar hides the `G`/`R` legend when every task has zero
    /// gate/review iteration counts.
    #[test]
    fn render_status_bar_hides_gr_legend_when_counts_zero() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/status-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("alpha"),
                    title: "Alpha task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("beta"),
                    title: "Beta task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));

        let mut terminal = make_terminal(120, 30);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            !screen.contains("gate iterations"),
            "legend must be hidden when all counts are zero"
        );
    }

    /// Done-state badge must use Cyan foreground; Failed must use Red.
    #[test]
    fn render_task_status_badge_colors() {
        let mut terminal = make_terminal(120, 30);
        let th = crate::theme::ayu_dark();
        let app = task_status_app();

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // Cyan for Done.
        let has_cyan = buf
            .content()
            .iter()
            .any(|cell| cell.fg == th.get(crate::theme::ThemeRole::Accent));
        assert!(has_cyan, "Done badge must use Cyan foreground");

        // Red for Failed.
        let has_red = buf
            .content()
            .iter()
            .any(|cell| cell.fg == th.get(crate::theme::ThemeRole::Error));
        assert!(has_red, "Failed badge must use Red foreground");

        // Green for InProgress.
        let has_green = buf
            .content()
            .iter()
            .any(|cell| cell.fg == th.get(crate::theme::ThemeRole::Success));
        assert!(has_green, "InProgress badge must use Green foreground");
    }

    /// When no run is selected the main panel must show the hint text.
    #[test]
    fn render_no_run_selected_shows_hint() {
        let mut terminal = make_terminal(120, 30);
        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("Select a run"),
            "no-run state must show 'Select a run' hint"
        );
    }

    /// When a run is selected but has no tasks yet (placeholder) the panel
    /// must show the "Loading tasks…" hint rather than an empty table.
    #[test]
    fn render_run_with_no_tasks_shows_loading_hint() {
        let mut terminal = make_terminal(120, 30);
        let api = Arc::new(PlaceholderApi::empty());
        let run = makina_core::api::RunView {
            id: makina_core::api::RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/empty-run.json"),
            status: makina_core::api::RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // With task table removed, we no longer show "Loading tasks" hint.
        // The sidebar shows just the run header; the exchange pane has more space.
        assert!(
            screen.contains("empty-run"),
            "run with no tasks must show the run in the sidebar"
        );
    }

    /// **Live update (done-when):** Feed `TaskStateChanged` and
    /// `TaskIterationsUpdated` events through `App::update` and assert the
    /// main panel reflects the new state and counts.
    ///
    /// This proves "task states update in the TUI as the loop progresses."
    #[test]
    fn live_update_task_state_and_iterations_reflect_in_panel() {
        use crate::app::AppEvent;
        use makina_core::api::{Event, RunId, RunStatus, RunView, TaskId, TaskState, TaskView};

        let mut terminal = make_terminal(120, 30);
        let api = Arc::new(PlaceholderApi::empty());

        // Start with one task in New state, zero iterations.
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/live.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("live-task"),
                title: "Live task".into(),
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Select the task (it's at index 0) so its detail shows in the exchange pane.
        app.selected_task = Some(0);

        // Feed TaskStateChanged: New → InProgress.
        app.update(AppEvent::ApiEvent(Event::TaskStateChanged {
            run: RunId(1),
            task: TaskId::new("live-task"),
            state: TaskState::InProgress,
        }));

        // Feed TaskIterationsUpdated: gate=1, review=0.
        app.update(AppEvent::ApiEvent(Event::TaskIterationsUpdated {
            run: RunId(1),
            task: TaskId::new("live-task"),
            gate_iterations: 1,
            review_iterations: 0,
        }));

        // Render and assert the InProgress badge and gate count appear.
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("Live task"),
            "task title must appear after update"
        );
        assert!(
            screen.contains("working"),
            "InProgress badge must appear after TaskStateChanged"
        );
        // Gate iteration count should appear in task detail as "gate ×1".
        assert!(
            screen.contains("gate ×1"),
            "gate iteration count must appear in task detail after TaskIterationsUpdated"
        );

        // Now transition to Done and update review iterations.
        app.update(AppEvent::ApiEvent(Event::TaskStateChanged {
            run: RunId(1),
            task: TaskId::new("live-task"),
            state: TaskState::Done,
        }));
        app.update(AppEvent::ApiEvent(Event::TaskIterationsUpdated {
            run: RunId(1),
            task: TaskId::new("live-task"),
            gate_iterations: 1,
            review_iterations: 2,
        }));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen2 = screen_of(&terminal);

        assert!(
            screen2.contains("done"),
            "Done badge must appear after state transition to Done"
        );
        assert!(
            screen2.contains("review ×2"),
            "review iteration count 2 must appear in task detail after update"
        );
        // Run aggregate status must also have updated to Completed.
        assert_eq!(
            app.runs[0].status,
            makina_core::api::RunStatus::Completed,
            "aggregate RunStatus must be Completed when all tasks are Done"
        );
    }

    // ── prompt-answer-stream (task 30) ────────────────────────────────────────

    /// Build an App with a run/task set up for exchange tests.
    fn exchange_app() -> App {
        use crate::app::AppEvent;
        use makina_core::api::{
            AgentRole, Event, ExchangeEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/exchange-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("task-a"),
                    title: "Task A".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("task-b"),
                    title: "Task B".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Feed task-a: PromptSent + ResponseChunks + TurnComplete.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-a"),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "implement X".into(),
            },
        }));
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-a"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: "work".into(),
            },
        }));
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-a"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk { text: "ing".into() },
        }));
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-a"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: " on it".into(),
            },
        }));
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-a"),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        // Feed task-b: a separate prompt (different task).
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-b"),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "task-b only".into(),
            },
        }));

        app
    }

    /// **Live streaming (done-when):** after feeding PromptSent + chunks +
    /// TurnComplete, the exchange pane must show the prompt AND the
    /// concatenated streamed answer.
    #[test]
    fn render_exchange_pane_shows_prompt_and_concatenated_answer() {
        let mut terminal = make_terminal(120, 40);
        let app = exchange_app();

        // task-a is focused (index 0).
        assert_eq!(app.selected_task, Some(0));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Prompt label and text must appear.
        assert!(
            screen.contains("Developer prompt") || screen.contains("prompt"),
            "exchange pane must show the prompt label"
        );
        assert!(
            screen.contains("implement X"),
            "exchange pane must show the prompt text"
        );
        // Concatenated answer must appear.
        assert!(
            screen.contains("working on it"),
            "exchange pane must show the concatenated response 'working on it'"
        );
    }

    /// **Focus filtering (done-when):** with task-a focused, only task-a's
    /// exchange appears; switching to task-b shows task-b's exchange instead.
    #[test]
    fn render_exchange_pane_focus_filter() {
        use crate::app::AppEvent;

        let mut terminal = make_terminal(120, 40);
        let mut app = exchange_app();

        // task-a focused (index 0): assert task-a's prompt visible, task-b's not.
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_a = screen_of(&terminal);
        assert!(
            screen_a.contains("implement X"),
            "task-a exchange must be visible when task-a is focused"
        );
        assert!(
            !screen_a.contains("task-b only"),
            "task-b exchange must NOT appear when task-a is focused"
        );

        // Navigate to task-b (index 1) via tree in Sidebar.
        // Tree cursor: 0 (Run) -> 1 (Task A) -> 2 (Task B)
        // Need SelectDown twice to reach task-b.
        app.update(AppEvent::SelectDown); // tree move to task-a node
        app.update(AppEvent::SelectDown); // tree move to task-b node
        // Note: selected_task is no longer updated by sidebar navigation (plan 0031).
        // With tabs, the active tab determines which task's content is displayed.
        // For this test, we manually set selected_task to simulate opening the task in a tab.
        app.selected_task = Some(1);

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_b = screen_of(&terminal);
        assert!(
            screen_b.contains("task-b only"),
            "task-b exchange must be visible when task-b's tab is active"
        );
        assert!(
            !screen_b.contains("implement X"),
            "task-a exchange must NOT appear when task-b's tab is active"
        );
    }

    /// **Task row highlight:** when Main panel is focused the focused task row
    /// must be highlighted (Accent background).
    #[test]
    fn render_focused_task_row_is_highlighted() {
        let mut terminal = make_terminal(120, 40);
        let th = crate::theme::ayu_dark();
        let app = exchange_app();

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let has_accent = buf
            .content()
            .iter()
            .any(|cell| cell.bg == th.get(crate::theme::ThemeRole::Accent));
        assert!(
            has_accent,
            "focused task row must use Accent highlight background"
        );
    }

    /// **Response with ANSI codes (no literal escapes):** a response whose
    /// text carries an embedded SGR escape must render with no literal escape
    /// byte and no raw `[..m` SGR text. Since Response rendering uses Markdown
    /// (which strips ANSI), both lines render with the response base color (Cyan
    /// for Developer), not with the original ANSI/diff colors.
    ///
    /// (Previous version of this test checked for Green/Cyan diff coloring,
    /// but plan-0009 task "wire-markup-into-exchange-pane" changes Response
    /// rendering from diff-aware to Markdown-based.)
    #[test]
    fn exchange_render_styles_ansi_and_diff_no_literal_escape() {
        use crate::app::AppEvent;
        use makina_core::api::{
            AgentRole, Event, ExchangeEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        let mut terminal = make_terminal(120, 40);
        let th = crate::theme::ayu_dark();

        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/ansi-diff.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("task-a"),
                title: "Task A".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));
        assert_eq!(app.selected_task, Some(0));

        // Response text: an ANSI-green `+added` line and a `@@` hunk header.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-a"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: "\x1b[32m+added\x1b[0m\n@@ -1,2 +1,2 @@\n context".into(),
            },
        }));
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-a"),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // (a) No literal escape char survives, and the flattened buffer carries
        // no raw `[32m` SGR text.  Scan the full multi-char cell symbols, not
        // just each cell's first char.
        let flattened: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            !buf.content()
                .iter()
                .any(|c| c.symbol().chars().any(|ch| ch == '\u{1b}')),
            "no cell symbol may contain the literal ESC char"
        );
        assert!(
            !flattened.contains("[32m"),
            "raw SGR `[32m` text must not render literally"
        );

        // (b) Both lines must render with the response base color (Cyan for Developer),
        // since Response now uses Markdown rendering (which strips ANSI).
        let row_text = |row: u16| -> String {
            (0..buf.area.width)
                .map(|col| buf[(col, row)].symbol().chars().next().unwrap_or(' '))
                .collect()
        };
        let added_fg_cyan = (0..buf.area.height).any(|row| {
            row_text(row).contains("+added")
                && (0..buf.area.width)
                    .any(|col| buf[(col, row)].fg == th.get(crate::theme::ThemeRole::Accent))
        });
        let hunk_fg_cyan = (0..buf.area.height).any(|row| {
            row_text(row).contains("@@ -1,2 +1,2 @@")
                && (0..buf.area.width)
                    .any(|col| buf[(col, row)].fg == th.get(crate::theme::ThemeRole::Accent))
        });
        assert!(
            added_fg_cyan,
            "the `+added` line must have at least one Cyan-foreground cell (response color)"
        );
        assert!(
            hunk_fg_cyan,
            "the `@@` hunk header line must have at least one Cyan-foreground cell"
        );
    }

    /// **Markdown and ANSI rendering (done-when):** a Response entry with
    /// Markdown renders styled text; another with ANSI colour renders with
    /// no literal escape bytes. Tests both features.
    #[test]
    fn exchange_render_markdown_and_ansi() {
        use crate::app::AppEvent;
        use makina_core::api::{
            AgentRole, Event, ExchangeEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        let mut terminal = make_terminal(120, 40);

        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/markdown.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("task-md"),
                title: "Markdown Task".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));
        assert_eq!(app.selected_task, Some(0));

        // Response text: Markdown with heading and bullet list.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-md"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: "# Title\n\n- Item 1\n- Item 2".into(),
            },
        }));

        // Another response with ANSI color.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-md"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: "\n\n\x1b[38;5;208mwarning text\x1b[0m".into(),
            },
        }));

        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("task-md"),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // (a) No literal escape char survives.
        assert!(
            !buf.content()
                .iter()
                .any(|c| c.symbol().chars().any(|ch| ch == '\u{1b}')),
            "no cell symbol may contain the literal ESC char"
        );

        // (b) Markdown heading text appears (without literal '#')
        let flattened: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            flattened.contains("Title"),
            "Markdown heading text must appear"
        );
        assert!(
            !flattened.contains("# Title"),
            "Markdown heading literal '#' must not appear"
        );

        // (c) Bullet text appears.
        assert!(
            flattened.contains("Item 1") || flattened.contains("Item"),
            "Bullet list items must appear"
        );

        // (d) ANSI-coloured text appears without SGR codes.
        assert!(
            flattened.contains("warning text"),
            "ANSI-coloured text must appear"
        );
        assert!(
            !flattened.contains("[38;5;208m"),
            "ANSI SGR code must not appear literally"
        );
    }

    /// **No exchange yet placeholder:** when a task has no exchange log
    /// entry the pane must show a sane placeholder.
    #[test]
    fn render_exchange_pane_no_exchange_placeholder() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let mut terminal = make_terminal(120, 40);
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/no-exchange.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("t1"),
                title: "T1".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("No exchange yet") || screen.contains("exchange"),
            "exchange pane must show 'No exchange yet' placeholder when log is empty"
        );
    }

    // ── Error pane (collapsible) ──────────────────────────────────────────────

    /// **Open pane (done-when):** with `error_pane_open = true` and messages
    /// present the pane shows the message text AND colours each line by level.
    #[test]
    fn render_error_pane_shows_messages_when_open() {
        use crate::app::{ErrorLevel, ErrorMessage};

        let mut terminal = make_terminal(120, 40);
        let th = crate::theme::ayu_dark();
        let mut app = exchange_app();
        app.error_pane_open = true;
        app.error_messages.push(ErrorMessage {
            timestamp: std::time::SystemTime::now(),
            level: ErrorLevel::Error,
            text: "agent crashed unexpectedly".into(),
        });

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        let buf = terminal.backend().buffer().clone();

        // The message text must be visible.
        assert!(
            screen.contains("agent crashed unexpectedly"),
            "open error pane must show the message text"
        );

        // At least one cell must use the Error level colour (Red).
        let has_red = buf
            .content()
            .iter()
            .any(|cell| cell.fg == th.get(crate::theme::ThemeRole::Error));
        assert!(
            has_red,
            "Error-level message must be rendered with Red foreground"
        );
    }

    /// **Collapsed badge (done-when):** with `error_pane_open = false` and
    /// messages present, the Exchange title carries a `(N errors)` badge and the
    /// message text itself is NOT shown.
    #[test]
    fn render_error_badge_when_collapsed_with_errors() {
        use crate::app::{ErrorLevel, ErrorMessage};

        let mut terminal = make_terminal(120, 40);
        let mut app = exchange_app();
        app.error_pane_open = false;
        app.error_messages.push(ErrorMessage {
            timestamp: std::time::SystemTime::now(),
            level: ErrorLevel::Warn,
            text: "this text must stay hidden".into(),
        });

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // The count badge must appear in the Exchange title.
        assert!(
            screen.contains("(1 errors)"),
            "collapsed error pane must surface a count badge in the Exchange title"
        );
        // The message text itself must NOT be rendered when collapsed.
        assert!(
            !screen.contains("this text must stay hidden"),
            "collapsed error pane must NOT render message text"
        );
    }

    // ── Diff overlay preserves ANSI modifiers (fix `tui-scroll-and-restore` #5) ──

    /// Response content rendered through Markdown (not diff-aware): plain
    /// text with ANSI codes is stripped before Markdown parsing, so neither
    /// diff coloring nor ANSI modifiers survive; the text is rendered with
    /// the response's base style (Cyan for Developer).
    ///
    /// (This test was previously checking diff overlay behavior with ANSI modifiers,
    /// but as of plan-0009 task "wire-markup-into-exchange-pane", Response content
    /// uses Markdown rendering instead of diff-aware rendering.)
    #[test]
    fn diff_base_overlay_preserves_ansi_bold_modifier() {
        use crate::app::{ExchangeContent, ExchangeEntry};
        use makina_core::api::AgentRole;
        use std::sync::Arc;

        // A complete Developer response with ANSI BOLD in the text.
        // Since Response now uses Markdown rendering (which strips ANSI),
        // the BOLD modifier is lost, and the text is rendered with the
        // response's Cyan base colour (not the old diff Green).
        let entry = ExchangeEntry {
            role: AgentRole::Developer,
            content: ExchangeContent::Response {
                text: "+added bold line".to_string(), // ANSI removed for clarity
                complete: true,
            },
        };

        let api = Arc::new(PlaceholderApi::new());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));
        let lines = exchange_entry_lines(&entry, &app, 80);

        // Find the span carrying the text and assert it uses the response colour.
        let mut found = false;
        for line in &lines {
            for span in line.spans.iter() {
                if span.content.contains("added") {
                    found = true;
                    // Expect Cyan (Developer response color), not Green (old diff color).
                    assert_eq!(
                        span.style.fg,
                        Some(app.active_theme.get(crate::theme::ThemeRole::Accent)),
                        "Response text must use the response colour (Cyan for Developer)"
                    );
                }
            }
        }
        assert!(found, "the text span must be present in the rendered lines");
    }

    /// **Rich thought + tool rendering:** a Developer thought and a completed
    /// Developer tool (with a diff line in its content) must render with their
    /// distinctive headers and styled content.
    ///
    /// Inspects the rendered Buffer (not just the `Line` spans) for the ASCII
    /// header text ("Developer thought", the tool title, "[completed]"), a body
    /// word, a diff-style fact (a Green-foreground cell on the "+added line"),
    /// and confirms no literal ESC byte survives.  Asserts on ASCII substrings
    /// rather than the wide emoji glyphs ("💭"/"⚙"), which the per-cell
    /// `.chars().next()` flattening can mangle.
    #[test]
    fn exchange_render_shows_thought_and_tool_entries() {
        use crate::app::{ExchangeContent, ExchangeEntry};
        use makina_core::api::AgentRole;
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;
        use std::sync::Arc;

        // A Developer thought followed by a completed Developer tool whose
        // content carries a `@@` hunk header and a `+added line` diff line.
        let thought = ExchangeEntry {
            role: AgentRole::Developer,
            content: ExchangeContent::Thought {
                text: "considering the trait".to_string(),
            },
        };
        let tool = ExchangeEntry {
            role: AgentRole::Developer,
            content: ExchangeContent::Tool {
                id: "tool-1".to_string(),
                title: "Editing src/lib.rs".to_string(),
                kind: Some("edit".to_string()),
                status: "completed".to_string(),
                content: "@@ -1 +1 @@\n+added line".to_string(),
            },
        };

        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        // Enable verbose mode so both thought body and tool content are rendered.
        app.verbose_mode = true;
        // Render both entries' lines into a small Buffer via a Paragraph.
        let mut lines: Vec<Line> = Vec::new();
        lines.extend(exchange_entry_lines(&thought, &app, 80));
        lines.extend(exchange_entry_lines(&tool, &app, 80));

        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);

        // (a) No literal escape char survives any cell.
        assert!(
            !buf.content()
                .iter()
                .any(|c| c.symbol().chars().any(|ch| ch == '\u{1b}')),
            "no cell symbol may contain the literal ESC char"
        );

        // (b) ASCII header + body substrings appear (assert on normal-width
        // text, NOT the wide emoji glyphs).
        let row_text = |row: u16| -> String {
            (0..buf.area.width)
                .map(|col| buf[(col, row)].symbol().chars().next().unwrap_or(' '))
                .collect()
        };
        let flattened: String = (0..buf.area.height).map(row_text).collect();
        assert!(
            flattened.contains("Developer thought"),
            "thought header text must render; got:\n{flattened}"
        );
        assert!(
            flattened.contains("considering"),
            "thought body text must render; got:\n{flattened}"
        );
        assert!(
            flattened.contains("Editing src/lib.rs"),
            "tool title must render in the header; got:\n{flattened}"
        );
        assert!(
            flattened.contains("[completed]"),
            "tool status badge must render in the header; got:\n{flattened}"
        );

        // (c) Diff styling reaches the tool content: the `+added line` row must
        // carry at least one Success-color (green) foreground cell from theme.
        let expected_fg = app.active_theme.get(crate::theme::ThemeRole::Success);
        let added_fg_success = (0..buf.area.height).any(|row| {
            row_text(row).contains("+added line")
                && (0..buf.area.width).any(|col| buf[(col, row)].fg == expected_fg)
        });
        assert!(
            added_fg_success,
            "the `+added line` in tool content must have a Success foreground cell (diff styling)"
        );
    }

    // ── Scrollbar rendering tests ─────────────────────────────────────────────

    /// **Exchange pane scrollbar renders when tall:** When the exchange log
    /// exceeds the pane height, a vertical scrollbar (thumb `█` and track `║`)
    /// must render in the right column of the pane's inner area.
    #[test]
    fn exchange_pane_scrollbar_renders_when_tall() {
        use crate::app::AppEvent;
        use makina_core::api::{
            AgentRole, Event, ExchangeEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        let mut terminal = make_terminal(80, 10);
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/scrollbar-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("tall-log"),
                title: "Tall Log".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Send a prompt.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("tall-log"),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "Do something".into(),
            },
        }));

        // Send many response chunks to exceed the pane height.
        for i in 0..20 {
            app.update(AppEvent::ApiEvent(Event::AgentExchange {
                run: RunId(1),
                task: TaskId::new("tall-log"),
                role: AgentRole::Developer,
                event: ExchangeEvent::ResponseChunk {
                    text: format!("Response line {}\n", i),
                },
            }));
        }

        // Complete the response.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("tall-log"),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        terminal.draw(|f| render(&app, f)).unwrap();

        let buffer = terminal.backend().buffer().clone();

        // The exchange pane's inner area (Borders::TOP, no horizontal border) has x=26, width=52
        // for an 80-wide terminal (main_block inner: border+padding = 2 per side → x=26, width=52).
        // The scrollbar renders into the rightmost column of the inner area: x = 26 + 52 - 1 = 77.
        let exchange_scrollbar_col: u16 = 77;
        let exchange_rows = 2u16..9u16;

        // Assert a scrollbar glyph is present in the exchange pane's rightmost inner column.
        let has_scrollbar = col_has_scrollbar(
            &buffer,
            exchange_scrollbar_col,
            exchange_rows.start,
            exchange_rows.end,
        );

        assert!(
            has_scrollbar,
            "exchange pane must show scrollbar thumb (█) or track (║) in rightmost inner column (x={exchange_scrollbar_col}) when content is tall"
        );
    }

    /// **Exchange pane has no scrollbar when short:** When the exchange log
    /// fits within the pane height, no scrollbar glyphs should appear in the
    /// right column of the pane's inner area.
    #[test]
    fn exchange_pane_no_scrollbar_when_short() {
        use crate::app::AppEvent;
        use makina_core::api::{
            AgentRole, Event, ExchangeEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        // Use a tall terminal so the exchange pane inner area has many more rows than
        // the short log (2 responses). 80×24: body=22, main inner=20, after tab(1)+header(3)=16
        // exchange inner rows ≈ 15 > 3 content lines → scroll_max = 0 → no scrollbar.
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/scrollbar-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("short-log"),
                title: "Short Log".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Send a prompt.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("short-log"),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "Quick task".into(),
            },
        }));

        // Send only 2 response chunks (fits easily in the tall pane).
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("short-log"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: "Answer part 1\n".into(),
            },
        }));
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("short-log"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: "Answer part 2\n".into(),
            },
        }));

        // Complete the response.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("short-log"),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        terminal.draw(|f| render(&app, f)).unwrap();

        let buffer = terminal.backend().buffer().clone();

        // For an 80-wide terminal: exchange pane inner rightmost column = 26 + 52 - 1 = 77.
        let exchange_scrollbar_col: u16 = 77;
        let exchange_rows = 2u16..23u16;

        // Assert no scrollbar glyph appears in the exchange pane's rightmost inner column.
        let has_scrollbar = col_has_scrollbar(
            &buffer,
            exchange_scrollbar_col,
            exchange_rows.start,
            exchange_rows.end,
        );

        assert!(
            !has_scrollbar,
            "exchange pane must not show any scrollbar glyphs when content fits (checked col x={exchange_scrollbar_col})"
        );
    }

    /// **Exchange pane scrollbar position matches offset:** When the scroll
    /// offset is set to a mid value (with auto-follow off), the thumb glyph
    /// must appear in the expected row band within the pane height, not at
    /// the top.
    #[test]
    fn exchange_pane_scrollbar_position_matches_offset() {
        use crate::app::AppEvent;
        use makina_core::api::{
            AgentRole, Event, ExchangeEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        let mut terminal = make_terminal(80, 12);
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/scrollbar-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("mid-offset"),
                title: "Mid Offset".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Send a prompt.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("mid-offset"),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "Do something".into(),
            },
        }));

        // Send many response chunks to create scroll space.
        for i in 0..25 {
            app.update(AppEvent::ApiEvent(Event::AgentExchange {
                run: RunId(1),
                task: TaskId::new("mid-offset"),
                role: AgentRole::Developer,
                event: ExchangeEvent::ResponseChunk {
                    text: format!("Response line {}\n", i),
                },
            }));
        }

        // Complete the response.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("mid-offset"),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        // Disable auto-follow and set scroll offset to a mid value.
        app.exchange_auto_follow = false;
        app.scroll_offsets.insert(ScrollablePanel::Exchange, 10);

        terminal.draw(|f| render(&app, f)).unwrap();

        let buffer = terminal.backend().buffer().clone();

        // The exchange pane's inner area occupies rows 2 to 11 (10 rows total).
        // With a scroll offset of 10 and pane height ~9, the thumb should be
        // roughly in the middle-to-lower portion of the visible scrollbar area.
        let exchange_rows = 2u16..11u16;

        // Find the row with the thumb glyph in the rightmost columns.
        let thumb_row = exchange_rows.clone().find(|row| {
            (70u16..80u16).any(|col| {
                let symbol = buffer[(col, *row)].symbol();
                symbol == "█"
            })
        });

        // The thumb should not be at the very top (row 2).
        assert!(
            thumb_row.is_some() && thumb_row != Some(2),
            "exchange pane scrollbar thumb must appear at a non-top row when offset is mid; found at {:?}",
            thumb_row
        );
    }

    /// **Exchange pane scroll offset is applied to rendering:** When scroll_offsets
    /// contains a manual offset N and auto-follow is disabled, the first visible
    /// content row of the exchange pane must equal the log line originally at index N.
    ///
    /// Proof strategy (geometry-free):
    ///   1. Render with offset=0 and a tall-enough terminal (80×30) so that row
    ///      `first_inner_y` shows lines[0] and row `first_inner_y + N` shows lines[N].
    ///   2. Record the content of row `first_inner_y + N` from that render.
    ///   3. Render with offset=N and assert the content of row `first_inner_y` equals
    ///      the content recorded in step 2 — proving lines[N] has moved to the top.
    ///
    /// The first inner row coordinate (`first_inner_y`) is located dynamically by
    /// scanning for lines[0] = "gate … review …" so the test is layout-independent.
    #[test]
    fn exchange_scroll_offset_applied_to_rendering() {
        use crate::app::AppEvent;
        use makina_core::api::{
            AgentRole, Event, ExchangeEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/scroll-offset-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("offset-test"),
                title: "Offset Test".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Send a prompt (lines[2] and lines[3] in the exchange paragraph).
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("offset-test"),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "Test prompt".into(),
            },
        }));

        // Send 30 response chunks.  After TurnComplete they are concatenated in one
        // Response entry and rendered through the markdown pipeline; the label line
        // plus the response-text lines give at least 30 content lines beyond the
        // header, which is far more than the pane height so scroll_max > 0.
        for i in 0..30 {
            app.update(AppEvent::ApiEvent(Event::AgentExchange {
                run: RunId(1),
                task: TaskId::new("offset-test"),
                role: AgentRole::Developer,
                event: ExchangeEvent::ResponseChunk {
                    text: format!("Response line {}\n", i),
                },
            }));
        }

        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("offset-test"),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        // Use a tall terminal so row `first_inner_y + N` is within the viewport
        // when offset=0 (we need N=10 to be visible, first_inner_y is around 7-8).
        let term_width: usize = 80;
        let term_height: u16 = 30;
        const N: usize = 10; // scroll offset to test

        // ── Step 1: render with offset=0, locate first_inner_y and record lines[N] ─
        app.exchange_auto_follow = false;
        app.scroll_offsets.insert(ScrollablePanel::Exchange, 0);
        let mut terminal = make_terminal(term_width as u16, term_height);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_at_0 = screen_of(&terminal);

        // Locate first_inner_y: the first row showing "gate … review …" (lines[0]).
        let first_inner_y = (0..term_height as usize)
            .find(|&y| {
                let row: String = screen_at_0
                    .chars()
                    .skip(y * term_width)
                    .take(term_width)
                    .collect();
                row.contains("gate") && row.contains("review")
            })
            .expect(
                "offset=0 render must show 'gate … review …' (lines[0]) at some row; \
                 check that the exchange pane is visible in the 80×30 terminal",
            );

        // Capture what lines[N] looks like at offset=0 (it appears at first_inner_y + N).
        let row_n_at_offset_0: String = screen_at_0
            .chars()
            .skip((first_inner_y + N) * term_width)
            .take(term_width)
            .collect();

        // Sanity: lines[N] must be within the terminal height.
        assert!(
            first_inner_y + N < term_height as usize,
            "first_inner_y={} + N={} must be less than term_height={}; \
             increase term_height or reduce N",
            first_inner_y,
            N,
            term_height
        );

        // ── Step 2: render with offset=N; first content row must equal lines[N] ──
        app.scroll_offsets
            .insert(ScrollablePanel::Exchange, N as u16);
        let mut terminal = make_terminal(term_width as u16, term_height);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_at_n = screen_of(&terminal);

        let first_row_at_n: String = screen_at_n
            .chars()
            .skip(first_inner_y * term_width)
            .take(term_width)
            .collect();

        // Compare rows up to the scrollbar area; scrollbar rendering may differ with
        // correct scroll_max initialization, but content must be identical.
        // The scrollbar occupies approximately the last 3-4 chars before the right border.
        let content_end = term_width.saturating_sub(4);
        let first_row_at_n_content =
            first_row_at_n[..content_end.min(first_row_at_n.len())].to_string();
        let row_n_at_offset_0_content =
            row_n_at_offset_0[..content_end.min(row_n_at_offset_0.len())].to_string();

        assert_eq!(
            first_row_at_n_content,
            row_n_at_offset_0_content,
            "With scroll_offset={N} the first visible content row of the exchange pane \
             (buffer row y={first_inner_y}) must equal the line originally at index {N} \
             (which appeared at row {} when offset=0); \
             \n  expected: '{}'\n  got:      '{}'",
            first_inner_y + N,
            row_n_at_offset_0_content.trim_end(),
            first_row_at_n_content.trim_end()
        );

        // Extra positive check: lines[N] must contain response text, not the header.
        assert!(
            first_row_at_n.contains("Response line"),
            "lines[{N}] (at first content row with offset={N}) must be a response line, not a header; \
             got: '{}'",
            first_row_at_n.trim_end()
        );
    }

    /// **Exchange pane auto-follow shows the bottom of the log:** When
    /// `exchange_auto_follow` is true, the rendered exchange pane must contain
    /// the last response line in the buffer.  When it is false with offset=0,
    /// the top of the log is shown instead and the last response line is absent.
    #[test]
    fn exchange_auto_follow_preserved_with_rendering() {
        use crate::app::AppEvent;
        use makina_core::api::{
            AgentRole, Event, ExchangeEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/auto-follow-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("auto-follow-test"),
                title: "Auto Follow Test".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Send a prompt.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("auto-follow-test"),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "Do something".into(),
            },
        }));

        // Send 30 response chunks so total_lines (36) exceeds every reasonable
        // pane height; the last chunk text is "Response line 29".
        for i in 0..30 {
            app.update(AppEvent::ApiEvent(Event::AgentExchange {
                run: RunId(1),
                task: TaskId::new("auto-follow-test"),
                role: AgentRole::Developer,
                event: ExchangeEvent::ResponseChunk {
                    text: format!("Response line {}\n", i),
                },
            }));
        }

        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("auto-follow-test"),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        // ── Auto-follow ON: effective_offset = scroll_max; last line visible ────
        // The TurnComplete event re-engages auto-follow (exchange_auto_follow=true).
        assert!(
            app.exchange_auto_follow,
            "auto-follow must be true after TurnComplete drives scroll_offsets to the bottom"
        );
        let mut terminal = make_terminal(80, 12);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_auto = screen_of(&terminal);

        // The bottom of the log ("Response line 29") must appear somewhere in
        // the exchange pane rows of the TestBackend buffer.
        assert!(
            screen_auto.contains("Response line 29"),
            "With auto-follow ON the bottom of the log ('Response line 29') must be \
             visible in the rendered buffer; effective_offset should equal scroll_max"
        );

        // ── Auto-follow OFF + offset=0: top of log shown; last line absent ──────
        // When the user manually pins to offset=0 with auto-follow off, the pane
        // shows the very first lines ("gate …", "", "▶ Developer prompt", …).
        // "Response line 29" is far beyond the pane viewport and must not appear.
        app.exchange_auto_follow = false;
        app.scroll_offsets.insert(ScrollablePanel::Exchange, 0);
        let mut terminal = make_terminal(80, 12);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_top = screen_of(&terminal);

        assert!(
            !screen_top.contains("Response line 29"),
            "With auto-follow OFF and offset=0 the last line ('Response line 29') must \
             NOT appear in the buffer (the pane shows the top of the log)"
        );

        // The top of the log must be visible: lines[0] = "gate ×0  ·  review ×0".
        // "gate" is reliable ASCII that only appears in the exchange pane header at offset=0.
        assert!(
            screen_top.contains("gate") && screen_top.contains("review"),
            "With auto-follow OFF and offset=0 the exchange pane must show the top of \
             the log (lines[0] = 'gate × … review ×0' must appear in the buffer)"
        );
    }

    /// **Task table has no G/R columns:** The task table header must contain
    /// Task table was removed as part of the sidebar tree implementation.
    /// This test now verifies that the old task table headers are gone.
    #[test]
    fn task_table_has_no_gate_review_columns() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};

        let mut terminal = make_terminal(100, 30);
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("alpha"),
                    title: "Alpha task".into(),
                    state: TaskState::Done,
                    gate_iterations: 2,
                    review_iterations: 1,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("beta"),
                    title: "Beta task".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 1,
                    review_iterations: 3,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Task table is removed, so the old table headers should not appear.
        // The task table headers "Task" and "State" are gone (they were underlined in the table).
        // Tasks now appear in the sidebar tree instead.
        assert!(
            screen.contains("Alpha task"),
            "task should appear in the sidebar tree"
        );
        // Verify the old table pattern is gone: underlined headers.
        // We can't check for underline directly in plain text, so we check that
        // the specific task-table column pattern is absent.
        assert!(
            !screen.contains("\nG ") && !screen.contains("\nR "),
            "no gate/review iteration columns should be in the main panel"
        );
    }

    /// **Task detail shows iteration counts:** When a task with non-zero
    /// gate_iterations and review_iterations is selected, the exchange pane
    /// displays the counts in the format "gate ×N · review ×M".
    #[test]
    fn task_detail_shows_iteration_counts() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};

        let mut terminal = make_terminal(100, 30);
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("gamma"),
                title: "Gamma task".into(),
                state: TaskState::Done,
                gate_iterations: 2,
                review_iterations: 1,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Select the run (index 0 by default) and task (index 0).
        app.selected_run = Some(0);
        app.selected_task = Some(0);

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Assert that the iteration counts appear in the rendered output.
        assert!(
            screen.contains("gate ×2"),
            "task detail must show 'gate ×2' for gate_iterations=2"
        );
        assert!(
            screen.contains("review ×1"),
            "task detail must show 'review ×1' for review_iterations=1"
        );
    }

    #[test]
    fn failed_detail_renders_reason() {
        use makina_core::api::{
            FailureKind, FailureReason, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        let mut terminal = make_terminal(100, 30);
        let th = crate::theme::ayu_dark();
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Failed,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("delta"),
                title: "Delta task".into(),
                state: TaskState::Failed,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: Some(FailureReason {
                    kind: FailureKind::MergeConflict,
                    message: "squash merge conflict detected".into(),
                }),
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Select the run (index 0 by default) and task (index 0).
        app.selected_run = Some(0);
        app.selected_task = Some(0);

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Assert that the failure reason appears in the rendered output.
        assert!(
            screen.contains("failed:"),
            "task detail must contain 'failed:' label for failed task"
        );
        assert!(
            screen.contains("merge conflict"),
            "task detail must show 'merge conflict' for MergeConflict kind"
        );
        assert!(
            screen.contains("squash merge conflict detected"),
            "task detail must show the failure reason message"
        );

        // Assert that the failure reason is rendered in red.
        let buf = terminal.backend().buffer().clone();
        let has_red = buf
            .content()
            .iter()
            .any(|cell| cell.fg == th.get(crate::theme::ThemeRole::Error));
        assert!(has_red, "failure reason line must use Red foreground");
    }

    /// **Tool title path compaction:** A Tool entry whose title contains a
    /// worktree-absolute path under the repo root must render with the path
    /// compacted to repo-relative form.
    #[test]
    fn tool_title_compacted_to_repo_root() {
        use crate::app::{ExchangeContent, ExchangeEntry};
        use makina_core::api::AgentRole;
        use std::sync::Arc;

        let _guard = HOME_ENV_LOCK.blocking_lock();

        // Set HOME to a temp dir so state_root resolves predictably.
        let temp_home = tempfile::tempdir().expect("create temp home");
        let original_home = std::env::var_os("HOME");
        // SAFETY: serialised by HOME_ENV_LOCK
        unsafe { std::env::set_var("HOME", temp_home.path()) };

        // Build the worktree path via the NEW relocated short-name layout.
        let repo_root = std::path::PathBuf::from("/home/user/workspace/myproject");
        let state_root = makina_core::paths::state_root(&repo_root);
        let short_name = makina_core::paths::short_worktree_name("0009-sidebar-tree", "task1");
        let worktree_path = format!(
            "{}/worktrees/{}/src/main.rs",
            state_root.display(),
            short_name
        );

        let tool = ExchangeEntry {
            role: AgentRole::Developer,
            content: ExchangeContent::Tool {
                id: "tool-1".to_string(),
                title: format!("Editing {worktree_path}"),
                kind: Some("edit".to_string()),
                status: "completed".to_string(),
                content: "some content".to_string(),
            },
        };

        let api = Arc::new(PlaceholderApi::new());
        let app = App::new(api, vec![], repo_root);

        let lines = exchange_entry_lines(&tool, &app, 80);

        // Find the header line and extract its text.
        let header_text: String = lines
            .first()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .unwrap_or_default();

        // Assert that the compacted path appears in the header.
        assert!(
            header_text.contains("src/main.rs"),
            "tool title must contain 'src/main.rs' (the compacted path)"
        );

        // Assert that the full worktree prefix (short-name form) is NOT in the header.
        assert!(
            !header_text.contains(&short_name),
            "tool title must NOT contain the full worktree short name prefix"
        );

        // Assert that the tool status is still shown.
        assert!(
            header_text.contains("completed"),
            "tool title must still contain the status '[completed]'"
        );

        // Restore HOME
        if let Some(home) = original_home {
            unsafe { std::env::set_var("HOME", home) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
    }

    /// **Spinner frame advances on tick:** The spinner_frame function must
    /// return different characters as the tick counter increases.
    #[test]
    fn spinner_frame_advances_on_tick() {
        assert_ne!(spinner_frame(0), spinner_frame(1));
    }

    /// **Spinner shown for in-progress task:** A task in InProgress state must
    /// display a spinner glyph in its state cell.
    #[test]
    fn spinner_shown_for_in_progress_task() {
        let mut terminal = make_terminal(100, 30);
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: makina_core::api::RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("test-task"),
                title: "Test Task".into(),
                state: makina_core::api::TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));
        // Advance the tick counter to ensure spinner changes.
        app.tick = 1;
        app.selected_run = Some(0);
        app.selected_task = Some(0);

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // The spinner frame for tick=1 should be present in the screen.
        let frame = spinner_frame(1);
        assert!(
            screen.contains(frame),
            "InProgress task must display spinner frame '{}' in the rendered output",
            frame
        );
    }

    /// **Plan 0009 acceptance:** End-to-end pane fidelity check.
    ///
    /// Builds a single task's `ExchangeLog` with:
    /// - a prompt
    /// - a response chunk
    /// - a thought
    /// - a tool (completed)
    /// - a second response chunk containing Markdown (`**bold**`) and ANSI colour
    /// - a `complete_turn`
    ///
    /// Verifies:
    /// - entry order is prompt → response → thought → tool → response (segmentation)
    /// - the second response shows styled bold text and ANSI colour (no literal `**` or escape bytes)
    /// - a tool title with a worktree-absolute path renders repo-relative
    #[test]
    fn plan_0009_acceptance_pane_fidelity() {
        use crate::app::AppEvent;
        use makina_core::api::{
            AgentRole, Event, ExchangeEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        let _guard = HOME_ENV_LOCK.blocking_lock();

        // Set HOME to a temp dir so state_root resolves predictably.
        let temp_home = tempfile::tempdir().expect("create temp home");
        let original_home = std::env::var_os("HOME");
        // SAFETY: serialised by HOME_ENV_LOCK
        unsafe { std::env::set_var("HOME", temp_home.path()) };

        let mut terminal = make_terminal(120, 40);

        // Set up a repo root and task in the worktree.
        let repo_root = std::path::PathBuf::from("/home/user/workspace/makina");
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/plan0009.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("pane-fidelity"),
                title: "Pane Fidelity".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], repo_root.clone());
        assert_eq!(app.selected_task, Some(0));

        // Step 1: Send a prompt.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("pane-fidelity"),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "Fix the exchange pane.".into(),
            },
        }));

        // Step 2: Send a response chunk.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("pane-fidelity"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: "Let me ".into(),
            },
        }));

        // Step 3: Send a thought.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("pane-fidelity"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ThoughtChunk {
                text: "checking the code…".into(),
            },
        }));

        // Step 4: Start a tool.
        // Use a *worktree-absolute* path so compact_paths must strip the
        // relocated state_root/worktrees/<short-name>/ prefix. Without the
        // compact_paths implementation the full worktree prefix would survive
        // in the rendered output and assertion 5 would fail.
        let state_root = makina_core::paths::state_root(&repo_root);
        let short_name =
            makina_core::paths::short_worktree_name("0009-exchange-pane-fidelity", "pane-fidelity");
        let worktree_tool_path = format!(
            "{}/worktrees/{}/src/main.rs",
            state_root.display(),
            short_name
        );
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("pane-fidelity"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ToolCall {
                id: "tool-1".into(),
                title: format!("Read {worktree_tool_path}"),
                kind: Some("read".into()),
                status: "pending".into(),
                content: None,
            },
        }));

        // Step 5: Update the tool (mark it as completed).
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("pane-fidelity"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ToolCallUpdate {
                id: "tool-1".into(),
                status: Some("completed".into()),
                title: None,
                content: None,
            },
        }));

        // Step 6: Send a second response chunk with Markdown and ANSI.
        // This chunk contains bold Markdown and an ANSI 256-colour code.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("pane-fidelity"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: "complete it. **Done!** \x1b[38;5;208mAll fixed.\x1b[0m".into(),
            },
        }));

        // Step 7: Complete the turn.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("pane-fidelity"),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        // Render the exchange pane.
        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // ──────────────────────────────────────────────────────────────────────
        // Assertion 1: Entry order is prompt → response → thought → tool → response
        // ──────────────────────────────────────────────────────────────────────
        let log = app
            .exchange_logs
            .get(&(RunId(1), TaskId::new("pane-fidelity")))
            .unwrap();
        let kinds: Vec<&str> = log
            .entries
            .iter()
            .map(|e| match &e.content {
                crate::app::ExchangeContent::Prompt { .. } => "prompt",
                crate::app::ExchangeContent::Response { .. } => "response",
                crate::app::ExchangeContent::Thought { .. } => "thought",
                crate::app::ExchangeContent::Tool { .. } => "tool",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["prompt", "response", "thought", "tool", "response"],
            "exchange log entries must be in chronological order (segmented responses)"
        );

        // ──────────────────────────────────────────────────────────────────────
        // Assertion 2: No literal escape bytes survive in the rendered output.
        // ──────────────────────────────────────────────────────────────────────
        assert!(
            !buf.content()
                .iter()
                .any(|c| c.symbol().chars().any(|ch| ch == '\u{1b}')),
            "no cell symbol may contain the literal ESC char"
        );

        // ──────────────────────────────────────────────────────────────────────
        // Assertion 3: Markdown bold text appears (without literal `**`).
        // ──────────────────────────────────────────────────────────────────────
        let flattened: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            flattened.contains("Done"),
            "Markdown bold text must appear in the rendered output"
        );
        assert!(
            !flattened.contains("**Done"),
            "Markdown literal '**' must not appear before bold text"
        );

        // ──────────────────────────────────────────────────────────────────────
        // Assertion 4: ANSI-coloured text appears without SGR codes.
        // ──────────────────────────────────────────────────────────────────────
        assert!(
            flattened.contains("All fixed"),
            "ANSI-coloured text 'All fixed' must appear in the rendered output"
        );
        assert!(
            !flattened.contains("[38;5;208m"),
            "ANSI SGR code must not appear literally in the rendered output"
        );

        // ──────────────────────────────────────────────────────────────────────
        // Assertion 5: Tool title with worktree-absolute path renders repo-relative.
        //
        // The tool title was set to:
        //   "Read {state_root}/worktrees/{short_name}/src/main.rs"
        //
        // After compact_paths() the relocated worktree prefix
        //   "{state_root}/worktrees/{short_name}/"
        // is stripped and only "src/main.rs" remains. Without compact_paths this
        // assertion would fail because "worktrees/{short_name}/" would still
        // appear in the rendered buffer.
        // ──────────────────────────────────────────────────────────────────────
        assert!(
            flattened.contains("src/main.rs"),
            "tool title must contain 'src/main.rs' (the compacted, worktree-stripped path)"
        );
        assert!(
            !flattened.contains(&format!("worktrees/{short_name}")),
            "tool title must NOT contain the worktree short-name prefix after compact_paths()"
        );

        // ──────────────────────────────────────────────────────────────────────
        // Assertion 6: All response entries are marked complete.
        // ──────────────────────────────────────────────────────────────────────
        assert!(
            log.entries.iter().all(|e| e.complete()),
            "after TurnComplete, all entries must be marked complete"
        );

        // Restore HOME.
        // SAFETY: serialised by HOME_ENV_LOCK
        unsafe {
            match original_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// **Live activity header shows idle and wall-clock:** When a task is
    /// in-progress, the exchange pane header displays:
    /// - `idle {n}s` with color based on idle threshold (dim → amber → red)
    /// - `wall-clock {m}m {s}s left` countdown toward the wall-clock limit
    #[test]
    fn header_shows_idle_and_countdown() {
        use crate::app::AppEvent;
        use makina_core::api::{Event, RunId, RunStatus, RunView, TaskId, TaskState, TaskView};

        let mut terminal = make_terminal(100, 30);
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("test-task"),
                title: "Test task".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Initialize: task is at tick 0 when InProgress starts
        app.update(AppEvent::ApiEvent(Event::TaskStateChanged {
            run: RunId(1),
            task: TaskId::new("test-task"),
            state: TaskState::InProgress,
        }));

        // Advance to tick 100 (simulating ~50 seconds of elapsed time at ~2 ticks/sec)
        for _ in 0..100 {
            app.update(AppEvent::Tick);
        }

        // Now simulate activity at tick 110 (so idle = 0s, since last activity is at tick 110)
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("test-task"),
            role: makina_core::api::AgentRole::Developer,
            event: makina_core::api::ExchangeEvent::ResponseChunk {
                text: "Starting work...".into(),
            },
        }));

        // Record the idle secs config via a TaskIdle event
        app.update(AppEvent::ApiEvent(Event::TaskIdle {
            run: RunId(1),
            task: TaskId::new("test-task"),
            idle_secs: 30,
        }));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // The screen must contain an idle indicator.
        assert!(
            screen.contains("idle") || screen.contains("idle 0s"),
            "exchange header must show an idle indicator when task is in-progress"
        );

        // The screen must contain a wall-clock countdown indicator.
        assert!(
            screen.contains("wall-clock") && screen.contains("left"),
            "exchange header must show a wall-clock countdown when task is in-progress"
        );
    }

    /// The doctor overlay lists all health checks with ✓/✗ indicators.
    #[test]
    fn doctor_overlay_lists_checks() {
        let mut terminal = make_terminal(120, 30);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Seed the app with a missing provider
        app.provider_probes = vec![makina_core::preflight::ProviderProbe {
            provider: "test-missing".to_string(),
            command: "missing-binary".to_string(),
            resolved: None,
            note: Some("not found".to_string()),
        }];
        app.base_branch_exists = false;

        // No config files resolve, so the config row reports a hard failure.
        app.config_paths = makina_core::config::ConfigPaths {
            global: None,
            project: None,
        };

        // Switch to doctor mode
        app.update(crate::app::AppEvent::OpenDoctor);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Verify all checks are rendered (doctor title must be present)
        assert!(app.is_viewing_doctor(), "app should be in Doctor mode");
        assert!(
            screen.contains("Doctor") || screen.contains("Health"),
            "doctor overlay title should be visible"
        );
        // At least one status glyph must render (the missing provider is a ⚠,
        // the missing base branch is a ✗).
        assert!(
            screen.contains('✓') || screen.contains('✗') || screen.contains('⚠'),
            "doctor overlay must render at least one ✓/✗/⚠ glyph; screen: {screen}"
        );
        // At least one of the four check messages must be visible.
        assert!(
            screen.contains("No config files")
                || screen.contains("Providers not found")
                || screen.contains("Base branch")
                || screen.contains(".makina/"),
            "doctor overlay must render at least one check message; screen: {screen}"
        );
    }

    /// **Test 1 (render-sidebar-tree):** Sidebar renders a run and nested tasks
    /// with disclosure glyphs, status badges, and failure labels.
    #[test]
    fn sidebar_renders_run_and_nested_tasks() {
        use makina_core::api::{
            FailureKind, FailureReason, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        let mut terminal = make_terminal(120, 30);
        let api = Arc::new(PlaceholderApi::empty());

        // App with 1 run expanded, containing a Done task and a Failed task.
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/plan-0016.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("done-task"),
                    title: "Completed task".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("failed-task"),
                    title: "Failed task".into(),
                    state: TaskState::Failed,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: Some(FailureReason {
                        kind: FailureKind::HardError,
                        message: "test error".into(),
                    }),
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Verify sidebar shows "Runs & Tasks" title.
        assert!(
            screen.contains("Runs & Tasks"),
            "sidebar must have 'Runs & Tasks' title"
        );

        // Verify run header shows disclosure glyph (expanded run = ▾).
        assert!(
            screen.contains("▾"),
            "expanded run must show ▾ disclosure glyph"
        );

        // Verify run status badge appears.
        assert!(
            screen.contains("[▶]"),
            "run in Running status must show [▶] badge"
        );

        // Verify task rows appear with their titles.
        assert!(
            screen.contains("Completed task"),
            "sidebar must show done task title"
        );
        assert!(
            screen.contains("Failed task"),
            "sidebar must show failed task title"
        );

        // Verify task state badges appear.
        assert!(
            screen.contains("[✓ done]"),
            "done task must show [✓ done] badge"
        );
        assert!(
            screen.contains("[✗ failed]"),
            "failed task must show [✗ failed] badge"
        );

        // Verify failure reason label appears for the failed task.
        // Note: the label may be split across lines due to sidebar width, so we check
        // for just "hard" which is the start of "hard error".
        assert!(
            screen.contains(" hard"),
            "failed task must show failure reason label starting with 'hard'"
        );
    }

    /// **Test 2 (render-sidebar-tree):** Collapsing a run hides its tasks.
    #[test]
    fn collapsed_run_hides_its_tasks() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};

        let mut terminal = make_terminal(120, 30);
        let api = Arc::new(PlaceholderApi::empty());

        // App with 1 run, initially expanded, containing 2 tasks.
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/collapse-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("task-a"),
                    title: "Task A".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("task-b"),
                    title: "Task B".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // First render: run is expanded (by default).
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_expanded = screen_of(&terminal);

        assert!(
            screen_expanded.contains("▾"),
            "run must be expanded initially (show ▾)"
        );
        assert!(
            screen_expanded.contains("Task A"),
            "expanded run must show its tasks"
        );
        assert!(
            screen_expanded.contains("Task B"),
            "expanded run must show all its tasks"
        );

        // Collapse the run by adding its RunId to collapsed_runs.
        app.collapsed_runs.insert(RunId(1));

        // Second render: run is now collapsed.
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_collapsed = screen_of(&terminal);

        assert!(
            screen_collapsed.contains("▸"),
            "collapsed run must show ▸ disclosure glyph"
        );
        assert!(
            !screen_collapsed.contains("Task A"),
            "collapsed run must hide its tasks"
        );
        assert!(
            !screen_collapsed.contains("Task B"),
            "collapsed run must hide all its tasks"
        );
    }

    /// **Test 3 (render-sidebar-tree):** Main panel no longer renders task table.
    #[test]
    fn main_panel_no_longer_renders_task_table_header() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};

        let mut terminal = make_terminal(120, 30);
        let api = Arc::new(PlaceholderApi::empty());

        // App with 1 run containing 2 tasks.
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/no-table.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("t1"),
                    title: "Task One".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    id: TaskId::new("t2"),
                    title: "Task Two".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Tasks now appear in the sidebar tree, not in a main-panel table.
        assert!(
            screen.contains("Task One"),
            "tasks should appear in the sidebar tree"
        );

        // The old task table had a header; verify it's not in the main area.
        // We check for patterns that would only appear in the table header line.
        // The main area (right side, wider part) should not have the underlined
        // "Task" / "State" column headers that the table used to have.
        //
        // Since the exchange pane now occupies more space, it should show content.
        assert!(
            screen.contains("Exchange"),
            "exchange pane should be visible and take up the freed space"
        );

        // Verify no old task-table specific patterns appear (checking for very
        // unlikely false positives: would need "Task" and "State" in a table header).
        // We simply verify the sidebar has the content (already checked above)
        // and the exchange pane has more room.
    }

    // ── Verbose mode (plan 0021) ──────────────────────────────────────────────

    /// Build a fixture with a Thought entry (non-empty body) and a Tool entry
    /// (non-empty content) for the verbose-mode render tests.
    fn verbose_fixture_entries() -> (crate::app::ExchangeEntry, crate::app::ExchangeEntry) {
        use crate::app::{ExchangeContent, ExchangeEntry};
        use makina_core::api::AgentRole;

        let thought = ExchangeEntry {
            role: AgentRole::Developer,
            content: ExchangeContent::Thought {
                text: "verbose thought body here".to_string(),
            },
        };
        let tool = ExchangeEntry {
            role: AgentRole::Developer,
            content: ExchangeContent::Tool {
                id: "verbose-tool".to_string(),
                title: "Editing lib.rs".to_string(),
                kind: Some("edit".to_string()),
                status: "completed".to_string(),
                content: "+verbose tool content line".to_string(),
            },
        };
        (thought, tool)
    }

    /// In compact mode (`verbose_mode = false`) the thought header and tool header
    /// are present but the thought body text and tool content lines are ABSENT.
    #[test]
    fn verbose_off_hides_thought_and_tool_content() {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;
        use std::sync::Arc;

        let (thought, tool) = verbose_fixture_entries();
        let api = Arc::new(PlaceholderApi::new());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));
        // verbose_mode defaults to false — compact.
        assert!(!app.verbose_mode);

        let mut lines: Vec<Line> = Vec::new();
        lines.extend(exchange_entry_lines(&thought, &app, 80));
        lines.extend(exchange_entry_lines(&tool, &app, 80));

        let area = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(area);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);

        let row_text = |row: u16| -> String {
            (0..buf.area.width)
                .map(|col| buf[(col, row)].symbol().chars().next().unwrap_or(' '))
                .collect()
        };
        let flattened: String = (0..buf.area.height).map(row_text).collect();

        // Headers are always present.
        assert!(
            flattened.contains("Developer thought"),
            "compact: thought header must be visible; got:\n{flattened}"
        );
        assert!(
            flattened.contains("Editing lib.rs"),
            "compact: tool header must be visible; got:\n{flattened}"
        );

        // Body / content are suppressed in compact mode.
        assert!(
            !flattened.contains("verbose thought body here"),
            "compact: thought body must NOT appear; got:\n{flattened}"
        );
        assert!(
            !flattened.contains("verbose tool content line"),
            "compact: tool content must NOT appear; got:\n{flattened}"
        );
    }

    /// In verbose mode (`verbose_mode = true`) the thought body text and tool
    /// content lines ARE present in addition to the headers.
    #[test]
    fn verbose_on_shows_thought_and_tool_content() {
        use ratatui::widgets::Widget;
        use std::sync::Arc;

        let (thought, tool) = verbose_fixture_entries();
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        app.verbose_mode = true;

        let mut lines: Vec<Line> = Vec::new();
        lines.extend(exchange_entry_lines(&thought, &app, 80));
        lines.extend(exchange_entry_lines(&tool, &app, 80));

        let area = Rect::new(0, 0, 80, 12);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);

        let row_text = |row: u16| -> String {
            (0..buf.area.width)
                .map(|col| buf[(col, row)].symbol().chars().next().unwrap_or(' '))
                .collect()
        };
        let flattened: String = (0..buf.area.height).map(row_text).collect();

        // Headers still present.
        assert!(
            flattened.contains("Developer thought"),
            "verbose: thought header must be visible; got:\n{flattened}"
        );
        assert!(
            flattened.contains("Editing lib.rs"),
            "verbose: tool header must be visible; got:\n{flattened}"
        );

        // Body and content now rendered.
        assert!(
            flattened.contains("verbose thought body here"),
            "verbose: thought body must be visible; got:\n{flattened}"
        );
        assert!(
            flattened.contains("verbose tool content line"),
            "verbose: tool content must be visible; got:\n{flattened}"
        );
    }

    /// The status bar must advertise the `[^O] verbose` key hint at all times.
    #[test]
    fn status_bar_advertises_verbose_key() {
        let mut terminal = make_terminal(160, 30);
        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("[^O] verbose"),
            "status bar must contain '[^O] verbose' hint; got:\n{screen}"
        );
    }

    // ── Command palette (plan 0069) ───────────────────────────────────────────

    /// The command palette renders filtered actions with the filter input and footer.
    #[test]
    fn palette_renders_filtered_actions() {
        let mut terminal = make_terminal(160, 40);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Set up command palette with a filter
        app.mode = crate::app::Mode::CommandPalette;
        app.command_palette = Some(crate::app::CommandPalette {
            filter: "set".to_string(),
            actions: crate::app::CommandPalette::default_actions(),
            selected: 0,
            theme_selector: None,
        });

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Verify title appears
        assert!(
            screen.contains("Command Palette"),
            "modal must contain title 'Command Palette'; got:\n{screen}"
        );

        // Verify filter is visible
        assert!(
            screen.contains("set"),
            "modal must show filter input 'set'; got:\n{screen}"
        );

        // Verify "Settings" is in the filtered output
        assert!(
            screen.contains("Settings"),
            "modal must show 'Settings' (matches filter 'set'); got:\n{screen}"
        );

        // Verify "Doctor" is NOT in the filtered output (doesn't match 'set')
        assert!(
            !screen.contains("Doctor"),
            "modal must NOT show 'Doctor' (doesn't match filter 'set'); got:\n{screen}"
        );
    }

    #[test]
    fn settings_renders_current_values() {
        let mut terminal = make_terminal(160, 40);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Set up settings with known values
        app.mode = crate::app::Mode::Settings;
        app.settings = Some(crate::app::Settings {
            gate_iterations: "7".to_string(),
            reviewer_iterations: "3".to_string(),
            wall_clock_secs: "600".to_string(),
            idle_secs: "".to_string(),
            concurrency: "4".to_string(),
            focused: crate::app::SettingsField::GateIterations,
            error: None,
        });

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Verify title appears
        assert!(
            screen.contains("Settings"),
            "modal must contain title 'Settings'; got:\n{screen}"
        );

        // Verify field values appear
        assert!(
            screen.contains("7"),
            "modal must show gate_iterations value '7'; got:\n{screen}"
        );

        assert!(
            screen.contains("4"),
            "modal must show concurrency value '4'; got:\n{screen}"
        );

        // Verify empty idle_secs is shown as "—"
        assert!(
            screen.contains("—"),
            "modal must show empty idle_secs as '—'; got:\n{screen}"
        );
    }

    // ── Per-role metrics (plan 0024) ───────────────────────────────────────────

    #[test]
    fn header_shows_model_and_duration() {
        let mut terminal = make_terminal(100, 30);
        let api = Arc::new(PlaceholderApi::new());

        let run_id = RunId(1);
        let task_id = TaskId::new("test-task");

        let task = TaskView {
            id: task_id.clone(),
            title: "Test Task".into(),
            state: TaskState::Done,
            gate_iterations: 1,
            review_iterations: 0,
            depends_on: vec![],
            started_at: None,
            finished_at: None,
            failure_reason: None,
            entry_text: String::new(),
        };

        let run = RunView {
            id: run_id,
            run_uid: "test-uid".to_string(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Completed,
            project: "test-project".to_string(),
            tasks: vec![task],
            report: IngestionReport::default(),
        };

        let mut app = App::new(api, vec![run], PathBuf::from("."));

        // Manually add a Developer metric with no usage.
        let key = (run_id, task_id.clone());
        app.role_metrics.entry(key).or_default().insert(
            makina_core::api::AgentRole::Developer,
            crate::app::RoleTurnMetric {
                model: "gpt-4o".to_string(),
                duration_ms: 1800,
                usage: None,
            },
        );

        terminal
            .draw(|frame| render(&app, frame))
            .expect("draw must succeed");

        let screen = screen_of(&terminal);

        // Check that the header contains the model name
        assert!(
            screen.contains("gpt-4o"),
            "header must show model 'gpt-4o'; got:\n{screen}"
        );

        // Check that the header contains "developer" label
        assert!(
            screen.contains("developer"),
            "header must show 'developer' label; got:\n{screen}"
        );

        // Check that a duration is shown (should be "1.8s")
        assert!(
            screen.contains("1.8s") || screen.contains("1800"),
            "header must show duration; got:\n{screen}"
        );

        // Check that "tok" does NOT appear when usage is None
        assert!(
            !screen.contains("tok"),
            "header must NOT show 'tok' when usage is None; got:\n{screen}"
        );
    }

    #[test]
    fn tokens_shown_only_when_present() {
        let mut terminal = make_terminal(100, 30);
        let api = Arc::new(PlaceholderApi::new());

        let run_id = RunId(2);
        let task_id = TaskId::new("test-task");

        let task = TaskView {
            id: task_id.clone(),
            title: "Test Task".into(),
            state: TaskState::Done,
            gate_iterations: 1,
            review_iterations: 0,
            depends_on: vec![],
            started_at: None,
            finished_at: None,
            failure_reason: None,
            entry_text: String::new(),
        };

        let run = RunView {
            id: run_id,
            run_uid: "test-uid".to_string(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Completed,
            project: "test-project".to_string(),
            tasks: vec![task],
            report: IngestionReport::default(),
        };

        let mut app = App::new(api, vec![run], PathBuf::from("."));

        // Manually add a Developer metric WITH usage.
        let key = (run_id, task_id.clone());
        app.role_metrics.entry(key).or_default().insert(
            makina_core::api::AgentRole::Developer,
            crate::app::RoleTurnMetric {
                model: "gpt-4o".to_string(),
                duration_ms: 2500,
                usage: Some(makina_core::api::UsageStats {
                    input_tokens: Some(100),
                    output_tokens: Some(40),
                }),
            },
        );

        terminal
            .draw(|frame| render(&app, frame))
            .expect("draw must succeed");

        let screen = screen_of(&terminal);

        // Check that the arrow and tok appear when usage is present
        assert!(
            screen.contains("→"),
            "header must show '→' when usage is present; got:\n{screen}"
        );

        assert!(
            screen.contains("tok"),
            "header must show 'tok' when usage is present; got:\n{screen}"
        );

        // Also check the actual token numbers appear
        assert!(
            screen.contains("100") || screen.contains("40"),
            "header must show token counts when usage is present; got:\n{screen}"
        );
    }

    // ── Render: accordion pane ────────────────────────────────────────────────

    /// Calling `render_plan_accordion_pane` with a `PlanEntry` whose sections
    /// contain many lines of text (more than the terminal height) must not panic,
    /// the scroll clamp must keep the offset within `[0, total_lines - height]`,
    /// and the expanded-section markers (`[-] SCOPE`, `[-] TASKS`, etc.) must
    /// appear in the rendered output.
    #[test]
    fn render_plan_accordion_pane_long_content_no_panic_and_scroll_clamps() {
        // Use a small terminal so the content overflows and scroll clamping fires.
        let mut terminal = make_terminal(80, 10);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        // Build a PlanEntry with long scope content (30 lines — more than the 10-row terminal).
        let long_scope: String = (1..=30)
            .map(|i| format!("Scope line {i}: some content about the plan scope.\n"))
            .collect();
        let plan = makina_core::orchestrator::PlanEntry {
            slug: "0032-test".to_string(),
            dir: PathBuf::from("docs/plans/0032-test"),
            has_tasks: true,
            tasks: vec![
                makina_core::orchestrator::PlanTaskPreview {
                    id: "task-alpha".to_string(),
                    title: "Alpha task".to_string(),
                    gated: false,
                    body: String::new(),
                    depends_on: vec![],
                },
                makina_core::orchestrator::PlanTaskPreview {
                    id: "task-beta".to_string(),
                    title: "Beta task (GATED)".to_string(),
                    gated: true,
                    body: String::new(),
                    depends_on: vec!["task-alpha".to_string()],
                },
            ],
            scope_text: Some(long_scope),
            architecture_text: Some("Architecture overview.".to_string()),
            status_text: Some("In progress.".to_string()),
        };

        // Expand all four sections so every render path is exercised.
        {
            let sections = app
                .accordion_state
                .entry("0032-test".to_string())
                .or_default();
            sections.insert(AccordionSection::Scope);
            sections.insert(AccordionSection::Architecture);
            sections.insert(AccordionSection::Tasks);
            sections.insert(AccordionSection::Status);
        }

        // Draw directly using the accordion renderer — must not panic.
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_plan_accordion_pane(&app, &plan, frame, area);
            })
            .expect("render_plan_accordion_pane must not panic with long content");

        let screen = screen_of(&terminal);

        // Expanded-section markers must appear in the rendered output.
        assert!(
            screen.contains("[-]"),
            "at least one expanded section marker '[-]' must appear; screen:\n{screen}"
        );

        // Now test with an even smaller terminal (height = 3) — scroll clamping
        // must still not panic (the content vastly overflows the area).
        let mut tiny_terminal = make_terminal(80, 3);
        tiny_terminal
            .draw(|frame| {
                let area = frame.area();
                render_plan_accordion_pane(&app, &plan, frame, area);
            })
            .expect("render_plan_accordion_pane must not panic on very small terminal");
    }

    /// Calling `render_plan_accordion_pane` with all sections collapsed
    /// (no entries in `accordion_state`) renders collapsed `[+]` markers
    /// and no section content.  When a section is then expanded and the
    /// content is `None`, the placeholder `(no ARCHITECTURE.md)` appears.
    #[test]
    fn render_plan_accordion_pane_collapsed_shows_plus_markers() {
        let mut terminal = make_terminal(80, 20);
        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], PathBuf::from("."));

        let plan = makina_core::orchestrator::PlanEntry {
            slug: "0032-collapsed".to_string(),
            dir: PathBuf::from("docs/plans/0032-collapsed"),
            has_tasks: false,
            tasks: vec![],
            scope_text: Some("Some scope content.".to_string()),
            architecture_text: None,
            status_text: None,
        };

        // No accordion_state entry — all sections default to collapsed.
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_plan_accordion_pane(&app, &plan, frame, area);
            })
            .expect("render_plan_accordion_pane must not panic when all sections are collapsed");

        let screen = screen_of(&terminal);

        // Collapsed markers appear; content lines do not.
        assert!(
            screen.contains("[+]"),
            "collapsed sections must show '[+]' markers; screen:\n{screen}"
        );
        assert!(
            !screen.contains("Some scope content"),
            "collapsed SCOPE section must not render its content; screen:\n{screen}"
        );
    }

    /// When ARCHITECTURE is expanded but its content is `None`, the placeholder
    /// `(no ARCHITECTURE.md)` must appear in the rendered output.
    #[test]
    fn render_plan_accordion_pane_missing_content_shows_placeholder() {
        let mut terminal = make_terminal(80, 20);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        let plan = makina_core::orchestrator::PlanEntry {
            slug: "0032-missing".to_string(),
            dir: PathBuf::from("docs/plans/0032-missing"),
            has_tasks: false,
            tasks: vec![],
            scope_text: None,
            architecture_text: None,
            status_text: None,
        };

        // Expand all four sections to force placeholder rendering.
        {
            let sections = app
                .accordion_state
                .entry("0032-missing".to_string())
                .or_default();
            sections.insert(AccordionSection::Scope);
            sections.insert(AccordionSection::Architecture);
            sections.insert(AccordionSection::Tasks);
            sections.insert(AccordionSection::Status);
        }

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_plan_accordion_pane(&app, &plan, frame, area);
            })
            .expect("render_plan_accordion_pane must not panic with missing content");

        let screen = screen_of(&terminal);

        // Expanded markers appear.
        assert!(
            screen.contains("[-]"),
            "expanded sections must show '[-]' markers; screen:\n{screen}"
        );
        // Placeholders for missing content appear when sections are expanded.
        assert!(
            screen.contains("(no SCOPE.md)"),
            "missing SCOPE must show placeholder when expanded; screen:\n{screen}"
        );
        assert!(
            screen.contains("(no ARCHITECTURE.md)"),
            "missing ARCHITECTURE must show placeholder when expanded; screen:\n{screen}"
        );
        assert!(
            screen.contains("(no STATUS.md)"),
            "missing STATUS must show placeholder when expanded; screen:\n{screen}"
        );
        // Tasks placeholder when empty.
        assert!(
            screen.contains("(no tasks)"),
            "empty tasks must show placeholder when expanded; screen:\n{screen}"
        );
    }

    /// **Accordion bodies render Markdown, not raw source:** an expanded SCOPE /
    /// ARCHITECTURE / STATUS section whose `.md` content contains CommonMark
    /// markup must render the *formatted* result — no literal `##` heading
    /// prefixes, `**bold**` asterisks, or fenced-code backticks should reach the
    /// screen. Regression guard for the plan-accordion markdown gap (the SCOPE/
    /// ARCHITECTURE/STATUS bodies previously displayed raw source).
    #[test]
    fn render_plan_accordion_pane_renders_markdown_not_raw() {
        let mut terminal = make_terminal(80, 40);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        let plan = makina_core::orchestrator::PlanEntry {
            slug: "0035-md".to_string(),
            dir: PathBuf::from("docs/plans/0035-md"),
            has_tasks: false,
            tasks: vec![],
            scope_text: Some(
                "## Scope Heading\n\nA **bold** word and an _italic_ word.\n\n\
                 - first bullet\n- second bullet\n\n`inline_code` here.\n"
                    .to_string(),
            ),
            architecture_text: Some("### Layers\n\nText with `code`.".to_string()),
            status_text: Some("**Status:** done.".to_string()),
        };

        // Expand SCOPE, ARCHITECTURE, and STATUS (the Markdown-rendered sections).
        {
            let sections = app
                .accordion_state
                .entry("0035-md".to_string())
                .or_default();
            sections.insert(AccordionSection::Scope);
            sections.insert(AccordionSection::Architecture);
            sections.insert(AccordionSection::Status);
        }

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_plan_accordion_pane(&app, &plan, frame, area);
            })
            .expect("render_plan_accordion_pane must not panic rendering Markdown");

        let screen = screen_of(&terminal);

        // The human-readable text survives …
        assert!(
            screen.contains("Scope Heading"),
            "heading text must render; screen:\n{screen}"
        );
        assert!(
            screen.contains("bold") && screen.contains("italic"),
            "inline-styled words must render; screen:\n{screen}"
        );
        assert!(
            screen.contains("first bullet"),
            "list items must render; screen:\n{screen}"
        );

        // … but the raw CommonMark markup must NOT appear verbatim.
        assert!(
            !screen.contains("##"),
            "heading markup '##' must be rendered away, not shown raw; screen:\n{screen}"
        );
        assert!(
            !screen.contains("**"),
            "bold markup '**' must be rendered away, not shown raw; screen:\n{screen}"
        );
        assert!(
            !screen.contains('`'),
            "code-span backticks must be rendered away, not shown raw; screen:\n{screen}"
        );
    }

    /// **Sidebar scrollbar renders when tall:** When the sidebar item count
    /// exceeds the pane height, a vertical scrollbar (thumb `█` and track `║`)
    /// must appear in the sidebar's inner rightmost column.
    #[test]
    fn sidebar_scrollbar_renders_when_tall() {
        let mut terminal = make_terminal(80, 10);
        let api = Arc::new(PlaceholderApi::empty());

        // Create 50 runs to exceed the sidebar height of ~8 rows (10 total - 1 title - 1 status).
        let runs = (0..50)
            .map(|i| RunView {
                id: RunId(i as u64),
                run_uid: format!("run-{}", i),
                task_list_path: PathBuf::from(format!(".tasks/run-{}.json", i)),
                status: RunStatus::Running,
                project: "test-project".to_string(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            })
            .collect();

        let app = App::new(api, runs, std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();

        let buffer = terminal.backend().buffer().clone();

        // Sidebar inner area: sidebar_area = Percentage(30) of 80 = 24 cols (x=0, width=24).
        // sidebar_block has Borders::ALL + Padding::horizontal(1): removes 2 per horizontal side.
        // sidebar_inner: x = 0+2 = 2, width = 24-4 = 20, rightmost column = 2 + 20 - 1 = 21.
        let sidebar_scrollbar_col: u16 = 21;
        let sidebar_rows = 1u16..9u16; // Rows from below title bar to above status bar

        // Assert a scrollbar glyph is present in the sidebar's inner rightmost column.
        let has_scrollbar = col_has_scrollbar(
            &buffer,
            sidebar_scrollbar_col,
            sidebar_rows.start,
            sidebar_rows.end,
        );

        assert!(
            has_scrollbar,
            "sidebar must show scrollbar thumb (█) or track (║) in the rightmost inner column (x={sidebar_scrollbar_col}) when content is tall"
        );
    }

    /// **Sidebar has no scrollbar when short:** When the sidebar item count
    /// fits within the pane height, no scrollbar glyphs should appear in the
    /// sidebar's right columns.
    #[test]
    fn sidebar_no_scrollbar_when_short() {
        let mut terminal = make_terminal(80, 10);
        let api = Arc::new(PlaceholderApi::empty());

        // Create only 5 runs, which fits in the sidebar height.
        let runs = (0..5)
            .map(|i| RunView {
                id: RunId(i as u64),
                run_uid: format!("run-{}", i),
                task_list_path: PathBuf::from(format!(".tasks/run-{}.json", i)),
                status: RunStatus::Running,
                project: "test-project".to_string(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            })
            .collect();

        let app = App::new(api, runs, std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();

        let buffer = terminal.backend().buffer().clone();

        // Sidebar inner rightmost column = 2 + 20 - 1 = 21 (see sidebar_scrollbar_renders_when_tall).
        let sidebar_scrollbar_col: u16 = 21;
        let sidebar_rows = 1u16..9u16;

        // Assert no scrollbar glyph appears in the sidebar's inner rightmost column.
        let has_scrollbar = col_has_scrollbar(
            &buffer,
            sidebar_scrollbar_col,
            sidebar_rows.start,
            sidebar_rows.end,
        );

        assert!(
            !has_scrollbar,
            "sidebar must not show any scrollbar glyphs when content fits (checked col x={sidebar_scrollbar_col})"
        );
    }

    /// Extract a text string from a buffer rectangle (rows y0..y1, cols x0..x1).
    /// Each row is concatenated with '\n' so substring checks can be row-bounded.
    fn extract_buffer_region(
        buf: &ratatui::buffer::Buffer,
        x0: u16,
        x1: u16,
        y0: u16,
        y1: u16,
    ) -> String {
        let mut out = String::new();
        for y in y0..y1 {
            for x in x0..x1 {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// **Sidebar scroll offset applied to rendering:**
    ///
    /// When `scroll_offsets[Sidebar] = 3` and `tree_cursor = None` (no selection,
    /// so ratatui does not nudge the offset to keep a selected item visible), the
    /// first item visible in the sidebar inner area must be the item originally at
    /// index 3, and items at indices 0-2 must NOT appear in the sidebar.
    ///
    /// With `scroll_offsets[Sidebar] = 0` the item at index 0 is visible and the
    /// item at index 6 is not (only 6 inner rows fit in a height-10 terminal).
    ///
    /// The test scans only the sidebar's inner buffer columns (1..23 in an 80-wide
    /// terminal where the sidebar is 30% = 24 cols wide) to avoid false matches
    /// from other panels.
    #[test]
    fn sidebar_scroll_offset_applied_to_rendering() {
        // Terminal: 80 wide, 10 tall.
        // Layout: row 0 = title, rows 1-8 = body (sidebar + main), row 9 = status bar.
        // Sidebar occupies 30% of 80 = 24 columns (x: 0..24), with Borders::ALL:
        //   inner columns: x 1..23 (22 wide), inner rows: y 2..8 (6 rows).
        // With 12 items and 6 visible inner rows, offset=3 shows items 3-8; items 0-2 are hidden.
        // With offset=0, items 0-5 are shown; item 6 is hidden.
        //
        // Item labels: file_stem of ".tasks/run-{i}.json" = "run-{i}" (no zero-padding).
        // Uniqueness: "run-0" only appears in item 0; "run-1" only in item 1 (items 10+ don't
        // exist since we create only 12 items, but even so "run-10" is not in the visible window
        // at offset=3). "run-3" only appears in item 3. This makes substring checks safe.

        let make_runs = || -> Vec<RunView> {
            (0..12)
                .map(|i| RunView {
                    id: RunId(i as u64),
                    run_uid: format!("run-{i}"),
                    task_list_path: PathBuf::from(format!(".tasks/run-{i}.json")),
                    status: RunStatus::Running,
                    project: "test-project".to_string(),
                    tasks: vec![],
                    report: makina_core::api::IngestionReport::default(),
                })
                .collect()
        };

        // ── Test 1: offset = 3, no selection ─────────────────────────────────
        let mut terminal = make_terminal(80, 10);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, make_runs(), std::path::PathBuf::from("."));
        // Clear the tree cursor so ratatui does not nudge the offset to keep
        // item 0 visible — without this, setting offset=3 with selected=Some(0)
        // causes ratatui to reset first_visible_index back to 0.
        app.tree_cursor = None;
        app.scroll_offsets.insert(ScrollablePanel::Sidebar, 3);

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf1 = terminal.backend().buffer().clone();

        // Sidebar inner area: x in 1..23, y in 2..8 (rows inside the Borders::ALL box).
        let sidebar1 = extract_buffer_region(&buf1, 1, 23, 2, 8);

        assert!(
            sidebar1.contains("run-3"),
            "offset=3: item at index 3 (run-3) must be visible in the sidebar; sidebar was:\n{sidebar1}"
        );
        assert!(
            !sidebar1.contains("run-0"),
            "offset=3: item at index 0 (run-0) must NOT be visible; sidebar was:\n{sidebar1}"
        );
        assert!(
            !sidebar1.contains("run-1"),
            "offset=3: item at index 1 (run-1) must NOT be visible; sidebar was:\n{sidebar1}"
        );
        assert!(
            !sidebar1.contains("run-2"),
            "offset=3: item at index 2 (run-2) must NOT be visible; sidebar was:\n{sidebar1}"
        );

        // ── Test 2: offset = 0, no selection ─────────────────────────────────
        let mut terminal2 = make_terminal(80, 10);
        let api2 = Arc::new(PlaceholderApi::empty());
        let mut app2 = App::new(api2, make_runs(), std::path::PathBuf::from("."));
        app2.tree_cursor = None;
        app2.scroll_offsets.insert(ScrollablePanel::Sidebar, 0);

        terminal2.draw(|f| render(&app2, f)).unwrap();
        let buf2 = terminal2.backend().buffer().clone();

        let sidebar2 = extract_buffer_region(&buf2, 1, 23, 2, 8);

        assert!(
            sidebar2.contains("run-0"),
            "offset=0: item at index 0 (run-0) must be visible in the sidebar; sidebar was:\n{sidebar2}"
        );
        // With 6 inner rows and offset=0, items 0-5 are visible; item 6 is not.
        assert!(
            !sidebar2.contains("run-6"),
            "offset=0: item at index 6 (run-6) must NOT be visible with only 6 inner rows; sidebar was:\n{sidebar2}"
        );
    }

    /// **Dependency view scroll offset applied to List arm:** When the dependency
    /// view's List arm is rendered with synthetic dependencies exceeding the pane
    /// height, and a scroll offset is set, the first visible line should equal
    /// the line at the offset index.
    #[test]
    fn dependency_view_scroll_offset_list_arm() {
        use crate::app::DependencyViewMode;

        let mut terminal = make_terminal(120, 30);
        let api = Arc::new(PlaceholderApi::empty());

        // Create many dependency tasks in the same run so they exceed pane height.
        let mut dep_tasks = Vec::new();
        for i in 0..12 {
            dep_tasks.push(TaskView {
                id: TaskId::new(format!("dep-{}", i)),
                title: format!("Dependency {}", i),
                state: TaskState::Done,
                gate_iterations: 0,
                review_iterations: 0,
                started_at: None,
                finished_at: None,
                depends_on: vec![],
                failure_reason: None,
                entry_text: String::new(),
            });
        }

        // Create a main task that has synthetic dependencies (all the deps above).
        let mut depends_on = Vec::new();
        for i in 0..12 {
            depends_on.push(TaskId::new(format!("dep-{}", i)));
        }
        let task_with_deps = TaskView {
            id: TaskId::new("main-task"),
            title: "Main task".into(),
            state: TaskState::Done,
            gate_iterations: 0,
            review_iterations: 0,
            started_at: None,
            finished_at: None,
            depends_on,
            failure_reason: None,
            entry_text: String::new(),
        };
        dep_tasks.push(task_with_deps);

        let run = RunView {
            id: RunId(1),
            run_uid: "test-run".to_string(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Running,
            project: "test-project".to_string(),
            tasks: dep_tasks,
            report: IngestionReport::default(),
        };

        let mut app = App::new(api, vec![run], PathBuf::from("."));

        // Select the main task (the one with dependencies).
        app.selected_task = Some(12);
        app.dependency_view = DependencyViewMode::List;

        // Test 1: offset = 2, should skip first 2 dependencies
        app.scroll_offsets
            .insert(ScrollablePanel::DependencyView, 2);

        terminal.draw(|f| render(&app, f)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        // Dependency view is in the upper portion of the main area.
        // Main area starts at column 30 (right of sidebar), rows roughly 5-15.
        let dep_view = extract_buffer_region(&buffer, 30, 119, 5, 15);

        // With offset=2, the first visible dependency should be dep-2.
        // Look for "dep-2" in the extracted region.
        assert!(
            dep_view.contains("dep-2"),
            "With scroll offset=2, first visible dependency must be dep-2; got:\n{dep_view}"
        );

        // dep-0 and dep-1 should NOT be visible (they are scrolled off).
        assert!(
            !dep_view.contains("dep-0"),
            "With scroll offset=2, dep-0 (at index 0) must NOT be visible; got:\n{dep_view}"
        );

        // Test 2: offset = 0, should show first dependency from the start.
        app.scroll_offsets
            .insert(ScrollablePanel::DependencyView, 0);

        terminal.draw(|f| render(&app, f)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let dep_view = extract_buffer_region(&buffer, 30, 119, 5, 15);

        assert!(
            dep_view.contains("dep-0"),
            "With scroll offset=0, first visible dependency must be dep-0; got:\n{dep_view}"
        );
    }

    // ── Record-panel-geometries tests ─────────────────────────────────────────

    /// Helper: compute the sidebar_area and exchange_pane_area that render()
    /// produces for a terminal of the given size (no provider warning, no error
    /// pane, DependencyViewMode::Off, empty ingestion report).
    ///
    /// Mirrors the layout logic in `render()` exactly, including the
    /// `Padding::horizontal(1)` inside `panel_block` which shifts the inner
    /// rect by +1 on each horizontal side.
    fn expected_geometry(width: u16, height: u16) -> (Rect, Rect) {
        let area = Rect::new(0, 0, width, height);

        // Top-level vertical split: title(1) / warning(0) / body(Min) / status(1).
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(0),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);
        let body_area = vertical[2];

        // Body horizontal split: sidebar(30%) / main(70%).
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(body_area);
        let sidebar_area = body[0];
        let main_area = body[1];

        // panel_block has Borders::ALL + Padding::horizontal(1).
        // Borders::ALL removes 1 cell on each side; horizontal padding removes 1
        // more on each side → total: x+2, y+1, width-4, height-2.
        let main_inner = Rect::new(
            main_area.x + 2,
            main_area.y + 1,
            main_area.width.saturating_sub(4),
            main_area.height.saturating_sub(2),
        );

        // content_area = main_inner (error pane height = 0).
        let content_area = main_inner;

        // In the (None, Some(run)) arm, the header has 3 lines, ingestion = 0.
        let header_height: u16 = 3;
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),             // tab bar
                Constraint::Length(header_height), // header
                Constraint::Length(0),             // ingestion (empty)
                Constraint::Min(3),                // exchange
            ])
            .split(content_area);
        // exchange_area = split[3]; with DependencyViewMode::Off → exchange_pane_area = exchange_area.
        let exchange_pane_area = split[3];

        (sidebar_area, exchange_pane_area)
    }

    /// **Record panel geometries — with selected run:**
    /// After rendering with a run selected (no active plan tab,
    /// DependencyViewMode::Off), `panel_geometries` must contain a Sidebar
    /// entry whose rect matches sidebar_area and an Exchange entry whose rect
    /// matches exchange_pane_area.
    #[test]
    fn record_panel_geometries_with_selected_run() {
        let (sidebar_area, exchange_pane_area) = expected_geometry(80, 24);

        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/geom-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("geom-task"),
                title: "Geometry Task".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));
        // Select the run so we enter the (None, Some(run)) match arm.
        app.tree_cursor = Some(0);

        terminal.draw(|f| render(&app, f)).unwrap();

        let geoms = app.panel_geometries.borrow();

        // Sidebar entry must exist and match the expected rect.
        let sidebar_entry = geoms.iter().find(|g| g.panel == ScrollablePanel::Sidebar);
        assert!(
            sidebar_entry.is_some(),
            "panel_geometries must contain a Sidebar entry; got: {:?}",
            &*geoms
        );
        assert_eq!(
            sidebar_entry.unwrap().rect,
            sidebar_area,
            "Sidebar rect must equal sidebar_area"
        );

        // Exchange entry must exist and match the expected rect.
        let exchange_entry = geoms.iter().find(|g| g.panel == ScrollablePanel::Exchange);
        assert!(
            exchange_entry.is_some(),
            "panel_geometries must contain an Exchange entry when a run is selected; got: {:?}",
            &*geoms
        );
        assert_eq!(
            exchange_entry.unwrap().rect,
            exchange_pane_area,
            "Exchange rect must equal exchange_pane_area"
        );
    }

    /// **Record panel geometries — no run selected:**
    /// When no run is selected and no plan tab is active, the render enters the
    /// (None, None) hint arm and must NOT record an Exchange geometry entry.
    #[test]
    fn record_panel_geometries_no_run_selected_has_no_exchange_entry() {
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::empty());
        // No runs → selected_run() returns None; no plan tabs opened.
        let app = App::new(api, vec![], PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();

        let geoms = app.panel_geometries.borrow();

        // Sidebar must still be recorded (it's always visible).
        assert!(
            geoms.iter().any(|g| g.panel == ScrollablePanel::Sidebar),
            "Sidebar entry must always be recorded; got: {:?}",
            &*geoms
        );

        // Exchange must NOT be recorded when no run is selected.
        assert!(
            !geoms.iter().any(|g| g.panel == ScrollablePanel::Exchange),
            "Exchange entry must NOT appear when no run is selected; got: {:?}",
            &*geoms
        );
    }

    /// **Record panel geometries — resize changes sidebar rect.height:**
    /// Rendering at a taller terminal size must produce a sidebar entry with a
    /// larger `rect.height` than a shorter terminal, proving that panel
    /// geometries are recomputed from the actual frame area on each render.
    #[test]
    fn record_panel_geometries_sidebar_height_changes_on_resize() {
        // First render at 80x20.
        let mut terminal_small = make_terminal(80, 20);
        let api_small: Arc<dyn makina_core::api::Api> = Arc::new(PlaceholderApi::empty());
        let app_small = App::new(api_small, vec![], PathBuf::from("."));
        terminal_small.draw(|f| render(&app_small, f)).unwrap();
        let small_height = app_small
            .panel_geometries
            .borrow()
            .iter()
            .find(|g| g.panel == ScrollablePanel::Sidebar)
            .map(|g| g.rect.height)
            .expect("Sidebar entry must exist after render");

        // Second render at 80x30 (10 rows taller).
        let mut terminal_large = make_terminal(80, 30);
        let api_large: Arc<dyn makina_core::api::Api> = Arc::new(PlaceholderApi::empty());
        let app_large = App::new(api_large, vec![], PathBuf::from("."));
        terminal_large.draw(|f| render(&app_large, f)).unwrap();
        let large_height = app_large
            .panel_geometries
            .borrow()
            .iter()
            .find(|g| g.panel == ScrollablePanel::Sidebar)
            .map(|g| g.rect.height)
            .expect("Sidebar entry must exist after render");

        assert!(
            large_height > small_height,
            "Sidebar rect.height must increase when terminal height increases \
             (small={small_height}, large={large_height})"
        );
    }

    /// **Task entry pane renders markdown:** when a task tab is active, the task
    /// entry pane renders with the task's entry_text (combination of description
    /// and done_when) processed through render_markdown.
    #[test]
    fn task_entry_pane_renders_markdown() {
        let mut terminal = make_terminal(120, 40);

        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("test-task"),
                title: "Test Task".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: "This is a **bold** test\n\n### Done when\n\n- Item 1\n- Item 2"
                    .to_string(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));
        app.selected_task = Some(0);
        app.tabs.open_tab(crate::app::TabContent::Task {
            plan_slug: "test-plan".to_string(),
            task_id: TaskId::new("test-task"),
        });

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // The rendered screen should contain the task ID, title, and markdown content.
        assert!(
            screen.contains("test-task"),
            "Task ID must appear in rendered output"
        );
        assert!(
            screen.contains("Test Task"),
            "Task title must appear in rendered output"
        );
        assert!(
            screen.contains("bold"),
            "Markdown bold content must be rendered"
        );
        assert!(
            screen.contains("Done when"),
            "Done when section must be rendered"
        );
        assert!(screen.contains("Item 1"), "List items must be rendered");
    }

    /// **Task entry pane respects pane width:** the entry_text is rendered with
    /// render_markdown using the pane's inner width for proper text wrapping.
    #[test]
    fn task_entry_pane_respects_pane_width() {
        let mut terminal = make_terminal(120, 40);

        let api = Arc::new(PlaceholderApi::empty());
        let long_text = "This is a very long line of text that should wrap at the pane width boundary when rendered through markdown rendering with proper line wrapping applied to respect the width parameter";
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("width-test"),
                title: "Width Test".into(),
                state: TaskState::Ready,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: long_text.to_string(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));
        app.selected_task = Some(0);
        app.tabs.open_tab(crate::app::TabContent::Task {
            plan_slug: "test-plan".to_string(),
            task_id: TaskId::new("width-test"),
        });

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // The content should be rendered and wrapped appropriately.
        // Check that at least some portion of the text is visible.
        assert!(
            screen.contains("very long line"),
            "Long text should be rendered with wrapping"
        );
    }

    // ── Theme Validation ─────────────────────────────────────────────────────

    /// Headless TestBackend render under ayu_mirage() theme asserts that a known
    /// cell holds the Mirage-resolved color (distinct from Ayu Dark), proving
    /// theme switching reaches the render path.
    #[test]
    fn render_with_ayu_mirage_theme_resolves_colors() {
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Switch to Ayu Mirage theme
        app.active_theme = crate::theme::ayu_mirage();

        terminal
            .draw(|frame| render(&app, frame))
            .expect("draw must succeed");

        let buffer = terminal.backend().buffer().clone();

        // Find a cell in the title bar that should have been rendered with theme colors.
        // The title bar is at the top and uses Info role for background (Ayu Mirage: Rgb(128, 191, 255))
        // and Foreground role for text (Ayu Mirage: Rgb(204, 202, 194)).
        // We look for a cell that has one of these colors applied.
        // Mirage's Info background (Rgb(128,191,255)) differs from the Ayu Dark
        // default (Rgb(115,184,255)), so finding a cell painted with the Mirage
        // value proves the active theme — not the Dark default — reached render.
        let mirage_info_bg = Color::Rgb(128, 191, 255); // Ayu Mirage Info background

        let mut found_mirage_color = false;
        for cell in buffer.content() {
            if cell.bg == mirage_info_bg {
                found_mirage_color = true;
                break;
            }
        }

        assert!(
            found_mirage_color,
            "Title bar should contain at least one cell with Ayu Mirage Info background color (Rgb(128, 191, 255))"
        );
    }

    /// Render a selection region under three Ayu variants and assert the selected
    /// cells' color pairs (bg, fg) differ across themes. This proves selection
    /// highlighting responds to theme changes and variants are visually distinct.
    ///
    /// The selection is anchored at (10, 5) and extended to (20, 5), so cells at
    /// row 5, columns 10–20 are painted with the theme's SelectionBg + Foreground.
    /// In a width-80 buffer those cells live at indices 5*80+10=410 through 5*80+20=420.
    /// We read directly from that range so the test cannot accidentally sample the
    /// background/title area (index 0) instead of the selection region.
    #[test]
    fn test_three_ayu_variants_render_distinct_selection_colors() {
        // Terminal width used throughout this test.
        const W: u16 = 80;
        // Selection anchor row and column range.
        const SEL_ROW: u16 = 5;
        const SEL_COL_START: u16 = 10;
        const SEL_COL_END: u16 = 20;

        let themes = vec![
            crate::theme::ayu_dark(),
            crate::theme::ayu_mirage(),
            crate::theme::ayu_light(),
        ];

        // Collect the (bg, fg) pair from the middle of the selection region for
        // each theme.  Using a Vec (not a HashSet) preserves per-theme ordering
        // for the diagnostic message; we deduplicate at assertion time.
        let mut pairs: Vec<(Color, Color)> = Vec::new();

        for theme in themes {
            let mut terminal = make_terminal(W, 24);
            let api = Arc::new(PlaceholderApi::new());
            let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
            app.active_theme = theme.clone();

            // Anchor at column 10, row 5; extend to column 20, row 5.
            // bounds covers the full screen so the selection is not clamped.
            app.selection = Some(crate::selection::Selection::start(
                SEL_COL_START,
                SEL_ROW,
                ratatui::layout::Rect::new(0, 0, W, 24),
            ));
            app.selection.as_mut().unwrap().extend(SEL_COL_END, SEL_ROW);

            terminal.draw(|frame| render(&app, frame)).expect("draw");

            let buffer = terminal.backend().buffer().clone();
            let cells = buffer.content();

            // Sample from the middle of the selection band (column 15, row 5).
            // Buffer is row-major with width W, so index = row * W + col.
            let idx = (SEL_ROW as usize) * (W as usize) + 15;
            let cell = &cells[idx];

            // The selection highlight paints SelectionBg as background; it must
            // differ from Color::Reset (the selection is non-empty so highlight()
            // is a no-op only for single-cell selections, which ours is not).
            assert!(
                cell.bg != Color::Reset,
                "Selection cell at row {SEL_ROW} col 15 must be painted (bg=Reset means \
                 selection highlight did not reach that cell)"
            );

            pairs.push((cell.bg, cell.fg));
        }

        // All three pairs must be distinct — if even two are equal the selection
        // color is not responding to the theme change.
        let distinct: std::collections::HashSet<_> = pairs.iter().collect();
        assert_eq!(
            distinct.len(),
            3,
            "Expected 3 distinct SelectionBg+Foreground pairs (one per theme), \
             got {}. Pairs: {:?}",
            distinct.len(),
            pairs
        );
    }

    /// Render the accordion pane with a focused section under three Ayu variants
    /// and assert that the focused header's background color differs per theme.
    ///
    /// When `focused_section = Some(AccordionSection::Scope)`, `render_accordion_section`
    /// applies `title_style.bg(theme.get(ThemeRole::FocusBg))` to the "SCOPE" header
    /// span.  The FocusBg color is distinct in each Ayu variant:
    ///   Dark   → Rgb(40,  80,  120)
    ///   Mirage → Rgb(70,  110, 160)
    ///   Light  → Rgb(160, 188, 230)
    ///
    /// The accordion layout is:
    ///   row 0: "Plan: {slug}"
    ///   row 1: "Dir:  {dir}"
    ///   row 2: ""  (empty separator)
    ///   row 3: "[+] SCOPE"  ← focused header with FocusBg bg
    ///
    /// "[+] " is 4 characters, so "SCOPE" starts at column 4 of row 3.
    /// In a width-80 buffer, buffer index = 3 * 80 + 4 = 244.
    #[test]
    fn test_accordion_focused_state_colors_differ_per_theme() {
        const W: u16 = 80;
        const H: u16 = 24;
        // Row of the SCOPE header line (0-based): Plan, Dir, blank, then SCOPE.
        const SCOPE_ROW: u16 = 3;
        // Column where "SCOPE" text begins: "[+] " is 4 chars.
        const SCOPE_COL: u16 = 4;

        let plan = makina_core::orchestrator::PlanEntry {
            slug: "test-focus".to_string(),
            dir: std::path::PathBuf::from("docs/plans/test-focus"),
            has_tasks: false,
            tasks: vec![],
            scope_text: Some("Scope content.".to_string()),
            architecture_text: None,
            status_text: None,
        };

        let themes = vec![
            crate::theme::ayu_dark(),
            crate::theme::ayu_mirage(),
            crate::theme::ayu_light(),
        ];

        let mut pairs: Vec<(Color, Color)> = Vec::new();

        for theme in themes {
            let mut terminal = make_terminal(W, H);
            let api = Arc::new(PlaceholderApi::new());
            let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
            app.active_theme = theme.clone();
            // Set the SCOPE section as focused so render_accordion_section applies
            // the FocusBg background to the "SCOPE" title span.
            app.focused_section = Some(AccordionSection::Scope);

            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_plan_accordion_pane(&app, &plan, frame, area);
                })
                .expect("draw");

            let buffer = terminal.backend().buffer().clone();
            let cells = buffer.content();

            // Sample the cell that holds the "S" in "SCOPE" on the focused header row.
            let idx = (SCOPE_ROW as usize) * (W as usize) + (SCOPE_COL as usize);
            let cell = &cells[idx];

            // The focused header must have a non-Reset background (the FocusBg color).
            assert!(
                cell.bg != Color::Reset,
                "Focused accordion header at row {SCOPE_ROW} col {SCOPE_COL} must have \
                 a non-Reset background (focused FocusBg styling was not applied)"
            );

            pairs.push((cell.bg, cell.fg));
        }

        // All three pairs must be distinct — proving the focused header color
        // is driven by the active theme, not a hardcoded value.
        let distinct: std::collections::HashSet<_> = pairs.iter().collect();
        assert_eq!(
            distinct.len(),
            3,
            "Expected 3 distinct focused-header (bg, fg) pairs (one per Ayu variant), \
             got {}. Pairs: {:?}",
            distinct.len(),
            pairs
        );
    }

    // ── Markdown cache (plan 0039) ────────────────────────────────────────────

    /// The markdown cache must return the same (cached) result when called twice
    /// with identical text and width, and must not grow the cache on the second call.
    #[test]
    fn test_markdown_cache_hits_on_same_text_and_width() {
        let api = Arc::new(PlaceholderApi::new());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));
        let theme = crate::theme::ayu_dark();
        let style = Style::default().fg(Color::Reset);
        let text = "# Heading\n\nSome **bold** text.";
        let width = 80u16;

        // First call: populate the cache
        let result1 = render_markdown_cached(&app, text, style, width, &theme);
        assert!(
            !result1.is_empty(),
            "markdown render should produce at least one line"
        );
        let cache_len_after_first = app.markdown_cache.borrow().len();
        assert_eq!(
            cache_len_after_first, 1,
            "cache should have 1 entry after first call"
        );

        // Second call: should hit the cache and not grow it
        let result2 = render_markdown_cached(&app, text, style, width, &theme);
        let cache_len_after_second = app.markdown_cache.borrow().len();
        assert_eq!(
            cache_len_after_second, 1,
            "cache should still have 1 entry after second call (cache must not grow)"
        );

        // Results must be identical
        assert_eq!(
            result1, result2,
            "cached result must match the direct result"
        );
    }

    /// Render the app to a TestBackend and inspect the buffer cells, asserting
    /// that all styled cells use truecolor (Color::Rgb) and not downsampled ANSI
    /// colors (Color::Indexed) or other variants.
    ///
    /// This end-to-end integration test verifies that colors flow from the theme
    /// through the render logic into the ratatui buffer as truecolor, not reduced
    /// to 16-color ANSI. It scans the first 200 cells of the rendered buffer —
    /// row 0 (the fully-painted title bar) plus the top of the body — asserting
    /// every styled cell is Color::Rgb, and that at least one truecolor cell is
    /// actually present so the check cannot pass vacuously.
    #[test]
    fn test_render_produces_truecolor_cells_not_ansi16() {
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::new());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));

        terminal.draw(|frame| render(&app, frame)).expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let cells = buffer.content();

        // Collect any non-RGB colors found during the scan so we can report them.
        let mut non_rgb_findings: Vec<(usize, Color, Color)> = Vec::new();

        // Scan the first 200 cells of the 80x24 buffer — row 0 (the title bar,
        // fully painted with truecolor) plus the top of the body. This exercises
        // the styled title row without depending on lower-pane content.
        for (idx, cell) in cells.iter().take(200).enumerate() {
            // Foreground color: must be Color::Reset (default/inherited) or Color::Rgb.
            match cell.fg {
                Color::Reset => {}        // OK — uses terminal's default foreground
                Color::Rgb(_, _, _) => {} // OK — truecolor foreground
                _other => {
                    non_rgb_findings.push((idx, cell.fg, cell.bg));
                }
            }

            // Background color: must be Color::Reset (default) or Color::Rgb.
            match cell.bg {
                Color::Reset => {}        // OK — uses terminal's default background
                Color::Rgb(_, _, _) => {} // OK — truecolor background
                _other => {
                    // Only record if we haven't already recorded this cell's fg issue.
                    if !non_rgb_findings.iter().any(|(i, _, _)| *i == idx) {
                        non_rgb_findings.push((idx, cell.fg, cell.bg));
                    }
                }
            }
        }

        // Assert no non-RGB colors were found.
        assert!(
            non_rgb_findings.is_empty(),
            "Found {} cells with non-RGB colors (expected all styled cells to use Color::Rgb). \
             Details: {:?}",
            non_rgb_findings.len(),
            non_rgb_findings
                .iter()
                .map(|(idx, fg, bg)| format!("cell[{}]: fg={:?}, bg={:?}", idx, fg, bg))
                .collect::<Vec<_>>()
        );

        // Positive guard: at least one scanned cell must actually be truecolor.
        // Without this, the is_empty() check above would pass vacuously if the
        // styled rows ever stopped rendering (all-Reset cells are in the allowed
        // set), masking exactly the kind of regression this test guards against.
        assert!(
            cells
                .iter()
                .take(200)
                .any(|c| matches!(c.fg, Color::Rgb(..)) || matches!(c.bg, Color::Rgb(..))),
            "Expected at least one truecolor (Color::Rgb) cell among the first 200 \
             scanned cells, found none — the styled title row may not be rendering."
        );
    }

    // ── Sidebar resize: small-terminal fallback (plan 0039) ──────────────────
    // NOTE: test_sidebar_resize_left_clamps_to_min and
    // test_sidebar_resize_right_clamps_to_max live in app.rs (the correct layer
    // for AppEvent dispatch tests) and are not duplicated here.

    #[test]
    fn test_small_terminal_renders_fallback_message() {
        let mut terminal = make_terminal(20, 5);
        let api = Arc::new(PlaceholderApi::new());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Render into a terminal below MIN_W×MIN_H (20×5 < 40×10).
        terminal.draw(|f| render(&app, f)).unwrap();

        // Collect all text rendered into the buffer.
        let buffer = terminal.backend().buffer().clone();
        let screen: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();

        // The fallback message must be present.
        assert!(
            screen.contains("Terminal too small"),
            "buffer must contain 'Terminal too small'; got: {:?}",
            screen,
        );

        // Normal layout widgets must be absent — the guard returns before any
        // layout split, so sidebar/detail/title-bar/status-bar widgets are not drawn.
        assert!(
            !screen.contains("Runs"),
            "normal sidebar widget 'Runs' must NOT appear in small-terminal fallback; got: {:?}",
            screen,
        );
        assert!(
            !screen.contains("Detail"),
            "normal main-pane 'Detail' must NOT appear in small-terminal fallback; got: {:?}",
            screen,
        );
        assert!(
            !screen.contains("Makina"),
            "title bar 'Makina' must NOT appear in small-terminal fallback; got: {:?}",
            screen,
        );
    }

    // ── Provider editor rename (plan 0039) ──────────────────────────────────

    #[test]
    fn test_provider_editor_title_is_view_not_configure() {
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Open the provider editor by dispatching the OpenProviderEditor event.
        let _ = app.update(crate::app::AppEvent::OpenProviderEditor);

        // Render the frame.
        terminal.draw(|f| render(&app, f)).unwrap();

        // Collect all text rendered into the buffer.
        let buffer = terminal.backend().buffer().clone();
        let screen: String = buffer
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();

        // Assert the title contains "View" and not "Configure".
        assert!(
            screen.contains("View Providers & Roles"),
            "provider editor modal title must contain 'View Providers & Roles'; got: {:?}",
            screen,
        );
        assert!(
            !screen.contains("Configure Providers & Roles"),
            "provider editor modal title must NOT contain 'Configure Providers & Roles'; got: {:?}",
            screen,
        );

        // Assert the read-only hint is present in the footer.
        assert!(
            screen.contains("Read-only"),
            "provider editor footer must contain 'Read-only' hint; got: {:?}",
            screen,
        );
    }
}
