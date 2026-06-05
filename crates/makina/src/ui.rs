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

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Padding, Paragraph,
        Row, Table, Wrap,
    },
};

use crate::app::{App, DependencyViewMode, ExchangeEntry, Panel};

/// Render the full TUI layout into `frame`.
///
/// When the modal file browser is active ([`App::is_browsing`]) it is drawn as
/// an overlay on top of the normal layout (task 28).
pub fn render(app: &App, frame: &mut Frame) {
    let area = frame.area();

    // ── Top-level vertical split ──────────────────────────────────────────────
    // title_bar (1 row) / body (fills remaining) / status_bar (1 row)
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(0),    // body
            Constraint::Length(1), // status bar
        ])
        .split(area);

    let title_area = vertical[0];
    let body_area = vertical[1];
    let status_area = vertical[2];

    // ── Body horizontal split ─────────────────────────────────────────────────
    // sidebar (30%) / main (70%)
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(body_area);

    let sidebar_area = body[0];
    let main_area = body[1];

    // ── Title bar ─────────────────────────────────────────────────────────────
    let version = env!("CARGO_PKG_VERSION");
    let title_text = format!(" Makina v{version} — multi-agent software factory ");
    let title = Paragraph::new(title_text).style(
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(title, title_area);

    // ── Sidebar ───────────────────────────────────────────────────────────────
    // Render a real ratatui List with one row per Run.  Each row shows the
    // run's file-stem (readable name) and its aggregate RunStatus with a
    // colour-coded badge.  The selected row is highlighted with a contrasting
    // style so the user can see which Run the main panel is detailing.
    let sidebar_focused = app.focused_panel == Panel::Sidebar;
    let sidebar_block = panel_block("Runs", sidebar_focused);

    if app.runs.is_empty() {
        // Empty state: show a hint instead of an empty list.  The `[o]` file
        // browser exists (task 28); guide the user to it.
        let empty_text = vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                "  No runs open.",
                Style::default().fg(Color::DarkGray),
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "  Press [o] to open a",
                Style::default().fg(Color::DarkGray),
            )]),
            Line::from(vec![Span::styled(
                "  task-list file.",
                Style::default().fg(Color::DarkGray),
            )]),
        ];
        let para = Paragraph::new(empty_text)
            .block(sidebar_block)
            .style(Style::default().fg(Color::White));
        frame.render_widget(para, sidebar_area);
    } else {
        // Build one ListItem per Run: "<status-badge> <name>".
        let items: Vec<ListItem> = app
            .runs
            .iter()
            .map(|run| {
                let name = run_label(run);
                let (badge, badge_color) = status_badge(&run.status);
                let line = Line::from(vec![
                    Span::styled(badge, Style::default().fg(badge_color)),
                    Span::styled(" ", Style::default()),
                    Span::raw(name),
                ]);
                ListItem::new(line)
            })
            .collect();

        // Highlight style for the selected row.
        let highlight_style = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);

        let sidebar_list = List::new(items)
            .block(sidebar_block)
            .highlight_style(highlight_style)
            .highlight_symbol("▶ ");

        // ListState carries the selected index so ratatui knows which row to
        // highlight.  It must be passed through render_stateful_widget.
        let mut list_state = ListState::default();
        list_state.select(app.selected_run);

        frame.render_stateful_widget(sidebar_list, sidebar_area, &mut list_state);
    }

    // ── Main content — per-task status view (task 29) ────────────────────────
    let main_focused = app.focused_panel == Panel::Main;
    let main_block = panel_block("Detail", main_focused);

    match app.selected_run() {
        None => {
            // No run selected: show a hint paragraph.
            let hint_lines = vec![
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  Select a run from the sidebar.",
                    Style::default().fg(Color::DarkGray),
                )]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "  [Tab] — switch focus",
                    Style::default().fg(Color::DarkGray),
                )]),
                Line::from(vec![Span::styled(
                    "  [q / Esc / Ctrl-C] — quit",
                    Style::default().fg(Color::DarkGray),
                )]),
            ];
            let hint_para = Paragraph::new(hint_lines)
                .block(main_block)
                .style(Style::default().fg(Color::White));
            frame.render_widget(hint_para, main_area);
        }
        Some(run) => {
            // Split main_area inside the block: header lines + task table + exchange pane.
            let inner = main_block.inner(main_area);
            frame.render_widget(main_block, main_area);

            // Header: run path and aggregate status.
            let header_lines: Vec<Line> = vec![
                Line::from(vec![
                    Span::styled("Run: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        run.task_list_path.display().to_string(),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        status_label(&run.status),
                        Style::default().fg(status_color(&run.status)),
                    ),
                    Span::styled(
                        format!(
                            "  ({} task{})",
                            run.tasks.len(),
                            if run.tasks.len() == 1 { "" } else { "s" }
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(""),
            ];
            let header_height = header_lines.len() as u16;

            // Reserve space: header + task table (up to ~40% of remaining) +
            // exchange pane (rest).  We cap the task table at a sensible height
            // so the exchange pane always has room.
            let task_count = run.tasks.len() as u16;
            // Table header (1 row) + task rows + 1 spare row.
            let table_rows = if task_count == 0 {
                1 // "Loading…" hint
            } else {
                (task_count + 1).min(10) // cap at 10 visible rows + header
            };

            // Error pane height: a few rows when open, 0 (a no-op area) when
            // closed.  Placed AFTER the exchange pane in the vertical split.
            let error_pane_height: u16 = if app.error_pane_open { 5 } else { 0 };

            // Ingestion pane height (near task table): non-zero only when the
            // selected run has a non-empty report. Mirrors error pane allocation.
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
                    Constraint::Length(header_height),
                    Constraint::Length(table_rows),
                    Constraint::Length(ingestion_pane_height), // ingestion issues (0 = hidden)
                    Constraint::Min(3), // exchange pane — always at least 3 rows
                    Constraint::Length(error_pane_height), // error pane (0 = hidden)
                ])
                .split(inner);

            let header_area = split[0];
            let table_area = split[1];
            let ingestion_area = split[2];
            let exchange_area = split[3];
            let error_area = split[4];

            let header_para = Paragraph::new(header_lines).style(Style::default().fg(Color::White));
            frame.render_widget(header_para, header_area);

            if run.tasks.is_empty() {
                // Run opened but tasks not yet loaded (fetching in progress).
                let waiting = Paragraph::new(Line::from(vec![Span::styled(
                    "  Loading tasks…",
                    Style::default().fg(Color::DarkGray),
                )]));
                frame.render_widget(waiting, table_area);
            } else {
                // Build a Table with columns: Task | State | G: | R:
                // Column widths: task title fills remainder; state fixed 12;
                // gate and review counters fixed 6 each.
                let col_title = Constraint::Min(10);
                let col_state = Constraint::Length(12);
                let col_gates = Constraint::Length(6);
                let col_reviews = Constraint::Length(6);

                let table_header = Row::new(vec![
                    Cell::from("Task").style(
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                    Cell::from("State").style(
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                    Cell::from("G").style(
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                    Cell::from("R").style(
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                ]);

                let rows: Vec<Row> = run
                    .tasks
                    .iter()
                    .map(|task| {
                        let (badge, badge_color) = task_state_badge(&task.state);
                        Row::new(vec![
                            Cell::from(task.title.clone()).style(Style::default().fg(Color::White)),
                            Cell::from(badge).style(Style::default().fg(badge_color)),
                            Cell::from(task.gate_iterations.to_string()).style(
                                Style::default().fg(if task.gate_iterations > 0 {
                                    Color::Yellow
                                } else {
                                    Color::DarkGray
                                }),
                            ),
                            Cell::from(task.review_iterations.to_string()).style(
                                Style::default().fg(if task.review_iterations > 0 {
                                    Color::Yellow
                                } else {
                                    Color::DarkGray
                                }),
                            ),
                        ])
                    })
                    .collect();

                let task_table = Table::new(rows, [col_title, col_state, col_gates, col_reviews])
                    .header(table_header)
                    .row_highlight_style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                    .column_spacing(1);

                // Use stateful rendering to highlight the focused task row.
                let mut table_state = ratatui::widgets::TableState::default();
                table_state.select(app.selected_task);
                frame.render_stateful_widget(task_table, table_area, &mut table_state);
            }

            // Render ingestion report panel (0-height area is a no-op inside).
            // Placed directly after the task table (near the task detail).
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
                render_dependency_view(app, frame, dep_split[0]);
                dep_split[1]
            };
            render_exchange_pane(app, frame, exchange_pane_area, main_focused);

            // ── Error pane (collapsible) ───────────────────────────────────
            // A 0-height `error_area` (pane closed) makes this a no-op.
            render_error_pane(app, frame, error_area);
        }
    }

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
    let trailer = match &app.status_message {
        Some(msg) => format!("  │  {msg}"),
        None => {
            let event_hint = match &app.last_event {
                None => String::new(),
                Some(ev) => format!("  │  last: {}", event_short_name(ev)),
            };
            format!("  {focus_label}{event_hint}")
        }
    };
    // Legend for the `G`/`R` task columns — only worth the screen real estate
    // when the selected run actually has non-zero iteration counts.  Appended to
    // the status bar (the top-level layout has no spare body row) using the same
    // ASCII `│` separator as the trailer (the non-ASCII `·` would collapse under
    // `screen_of`, which flattens each cell to its first char).
    let show_legend = app.selected_run().is_some_and(|r| {
        r.tasks
            .iter()
            .any(|t| t.gate_iterations > 0 || t.review_iterations > 0)
    });
    let legend = if show_legend {
        "  │  G = gate iterations  R = review iterations"
    } else {
        ""
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
    // The legend precedes the trailer so its full text (notably "review
    // iterations") stays inside the visible width; the lower-priority trailer
    // (focus/last-event hint) is the part that gets clipped on narrow terminals.
    let status_text = format!(
        " [o] open  [s/p/c] start/pause/cancel  [Tab] panel  [q/^C] quit{legend}{blocked_notice}{trailer}"
    );
    let status_bar =
        Paragraph::new(status_text).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    frame.render_widget(status_bar, status_area);

    // ── File-browser overlay ────────────────────────────────────────────────────
    // Drawn LAST so it sits on top of the normal layout (task 28).
    if app.is_browsing()
        && let Some(browser) = app.browser.as_ref()
    {
        render_file_browser(browser, frame, area);
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
/// This is the single shared entry point for all [`DependencyViewMode`]
/// renderings: sibling tasks (`tui-dep-tree`, `tui-dep-timeline`) only add their
/// match arms here.  The pane is a top-bordered `Dependencies` block; its body
/// depends on the active [`App::dependency_view`].
///
/// For [`DependencyViewMode::List`] it renders the selected task's `depends_on`
/// as a compact `[state] task-id` list, one prerequisite per line, looking up
/// each dependency's [`TaskView`] in the same run to colour its state badge.
fn render_dependency_view(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" Dependencies ")
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));
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
                                        let (badge, color) = task_state_badge(&dep.state);
                                        Line::from(vec![Span::styled(
                                            format!("{} {}", badge, dep.id.0),
                                            Style::default().fg(color),
                                        )])
                                    }
                                    None => Line::from(vec![Span::styled(
                                        format!("  {}", dep_id.0),
                                        Style::default().fg(Color::DarkGray),
                                    )]),
                                }
                            })
                            .collect(),
                        _ => vec![Line::from(vec![Span::styled(
                            "  No dependencies.",
                            Style::default().fg(Color::DarkGray),
                        )])],
                    }
                }
                _ => vec![Line::from(vec![Span::styled(
                    "  No task focused.",
                    Style::default().fg(Color::DarkGray),
                )])],
            };
            let para = Paragraph::new(lines);
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
                            render_dependency_tree_children(run, &root.depends_on, "", 0, &mut acc);
                            acc
                        }
                        _ => vec![Line::from(vec![Span::styled(
                            "  No dependencies.",
                            Style::default().fg(Color::DarkGray),
                        )])],
                    }
                }
                _ => vec![Line::from(vec![Span::styled(
                    "  No task focused.",
                    Style::default().fg(Color::DarkGray),
                )])],
            };
            let para = Paragraph::new(lines);
            frame.render_widget(para, inner);
        }
        DependencyViewMode::Timeline => {
            // Lane view over scheduling order: one ROW per longest-path level,
            // with same-level (parallelisable) tasks side-by-side and every
            // dependent in a strictly later row than its prerequisites.
            let run = app.selected_run();
            let lines: Vec<Line> = match run {
                Some(run) if !run.tasks.is_empty() => {
                    let levels = dependency_levels(&run.tasks);
                    levels
                        .iter()
                        .map(|lane| {
                            // Reuse the `[state] id` badge format, side-by-side.
                            let mut spans: Vec<Span> = Vec::new();
                            for (i, task) in lane.iter().enumerate() {
                                if i > 0 {
                                    spans.push(Span::raw("  "));
                                }
                                let (badge, color) = task_state_badge(&task.state);
                                spans.push(Span::styled(
                                    format!("{} {}", badge, task.id.0),
                                    Style::default().fg(color),
                                ));
                            }
                            Line::from(spans)
                        })
                        .collect()
                }
                _ => vec![Line::from(vec![Span::styled(
                    "  No tasks.",
                    Style::default().fg(Color::DarkGray),
                )])],
            };
            // Respect the pane height: only as many lanes as fit are drawn.
            let para = Paragraph::new(lines);
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
                let (badge, color) = task_state_badge(&dep.state);
                acc.push(Line::from(vec![Span::styled(
                    format!("{prefix}{connector}{badge} {}", dep.id.0),
                    Style::default().fg(color),
                )]));
                if depth < DEPENDENCY_TREE_MAX_DEPTH && !dep.depends_on.is_empty() {
                    // Carry `│   ` past children that still have siblings, or a
                    // blank gap past the last child.
                    let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
                    render_dependency_tree_children(
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
                    Style::default().fg(Color::DarkGray),
                )]));
            }
        }
    }
}

/// Assign each task a static *longest-path* dependency level and group the
/// tasks into lanes by that level for the `Timeline` view.
///
/// `level(t) = 0` when `t.depends_on` is empty, otherwise
/// `1 + max(level(d) for d in t.depends_on)` over the run's `tasks`.  A
/// `depends_on` id that is not present in `tasks` is treated as level `0`
/// (an unknown-id / cycle guard so the computation always terminates).
///
/// The returned `Vec` has one inner `Vec` per level: index `i` holds every
/// task whose level is `i`, in the input order.  Trailing empty levels do not
/// occur because a level is only created when at least one task occupies it.
/// Because a dependent's level is strictly greater than each of its
/// prerequisites' levels, a task always lands in a lane *after* all of its
/// prerequisites — i.e. tasks in the same inner `Vec` can run in parallel and
/// dependents appear in strictly later lanes.
fn dependency_levels(
    tasks: &[makina_core::api::TaskView],
) -> Vec<Vec<&makina_core::api::TaskView>> {
    use std::collections::HashMap;

    // Index tasks by id for O(1) prerequisite lookup.
    let by_id: HashMap<&makina_core::api::TaskId, &makina_core::api::TaskView> =
        tasks.iter().map(|t| (&t.id, t)).collect();

    // Memoised longest-path level per task id.  `in_progress` tracks ids on the
    // current DFS stack so a dependency cycle is broken (treated as level 0)
    // rather than recursing forever.
    fn level_of<'a>(
        id: &'a makina_core::api::TaskId,
        by_id: &HashMap<&'a makina_core::api::TaskId, &'a makina_core::api::TaskView>,
        memo: &mut HashMap<&'a makina_core::api::TaskId, usize>,
        in_progress: &mut std::collections::HashSet<&'a makina_core::api::TaskId>,
    ) -> usize {
        if let Some(&lvl) = memo.get(id) {
            return lvl;
        }
        // Unknown id (not in this run) or a cycle back-edge → level 0 guard.
        let Some(task) = by_id.get(id) else {
            return 0;
        };
        if !in_progress.insert(id) {
            return 0;
        }
        let lvl = if task.depends_on.is_empty() {
            0
        } else {
            task.depends_on
                .iter()
                .map(|dep| 1 + level_of(dep, by_id, memo, in_progress))
                .max()
                .unwrap_or(0)
        };
        in_progress.remove(id);
        memo.insert(id, lvl);
        lvl
    }

    let mut memo: HashMap<&makina_core::api::TaskId, usize> = HashMap::new();
    let mut levels: Vec<Vec<&makina_core::api::TaskView>> = Vec::new();
    for task in tasks {
        let mut in_progress = std::collections::HashSet::new();
        let lvl = level_of(&task.id, &by_id, &mut memo, &mut in_progress);
        if lvl >= levels.len() {
            levels.resize_with(lvl + 1, Vec::new);
        }
        levels[lvl].push(task);
    }
    levels
}

fn render_exchange_pane(app: &App, frame: &mut Frame, area: Rect, focused: bool) {
    // When the error pane is collapsed but errors are pending, surface a badge
    // in the Exchange title so the user knows there's something to expand.
    let title = if !app.error_pane_open && !app.error_messages.is_empty() {
        let n = app.error_messages.len();
        format!(" Exchange ({n} errors) ")
    } else {
        " Exchange ".to_string()
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::TOP)
        .border_style(if focused {
            Style::default().fg(Color::Blue)
        } else {
            Style::default().fg(Color::DarkGray)
        });

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Determine which task's log to display.
    let task_id = app.selected_task_id();

    let log_opt = task_id.and_then(|id| app.exchange_logs.get(id));

    match log_opt {
        None => {
            // No task focused or no exchange yet.
            let hint = if task_id.is_none() {
                "  No task focused."
            } else {
                "  No exchange yet."
            };
            let para = Paragraph::new(Line::from(vec![Span::styled(
                hint,
                Style::default().fg(Color::DarkGray),
            )]));
            frame.render_widget(para, inner);
        }
        Some(log) if log.entries.is_empty() => {
            let para = Paragraph::new(Line::from(vec![Span::styled(
                "  No exchange yet.",
                Style::default().fg(Color::DarkGray),
            )]));
            frame.render_widget(para, inner);
        }
        Some(log) => {
            // Build the exchange lines.
            let mut lines: Vec<Line> = Vec::new();
            for entry in &log.entries {
                lines.extend(exchange_entry_lines(entry));
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
            app.last_scroll_max.set(scroll_max);
            let scroll_offset = app.effective_offset(scroll_max);

            let para = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll_offset, 0));
            frame.render_widget(para, inner);
        }
    }
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
        .border_style(Style::default().fg(Color::Red));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.error_messages.is_empty() {
        let para = Paragraph::new(Line::from(vec![Span::styled(
            "  No errors.",
            Style::default().fg(Color::DarkGray),
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
                ErrorLevel::Error => Color::Red,
                ErrorLevel::Warn => Color::Yellow,
                ErrorLevel::Info => Color::DarkGray,
            };
            Line::from(vec![Span::styled(
                format!("  {}", msg.text),
                Style::default().fg(color),
            )])
        })
        .collect();

    // Auto-scroll so the latest messages stay visible.
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
        Color::Red
    } else {
        Color::Yellow
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
                IssueSeverity::Blocking => Color::Red,
                IssueSeverity::Warning => Color::Yellow,
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
fn exchange_entry_lines(entry: &ExchangeEntry) -> Vec<Line<'static>> {
    use crate::ansi::{AnsiSpan, diff_line_style, parse_ansi};
    use makina_core::api::AgentRole;

    let mut lines = Vec::new();

    if entry.is_prompt {
        // Role label + prompt text on separate lines.
        let (label, label_color) = match entry.role {
            AgentRole::Developer => ("▶ Developer prompt", Color::Green),
            AgentRole::Reviewer => ("▶ Reviewer prompt", Color::Yellow),
        };
        lines.push(Line::from(vec![Span::styled(
            label,
            Style::default()
                .fg(label_color)
                .add_modifier(Modifier::BOLD),
        )]));
        // Render prompt text lines (split on newlines).
        for text_line in entry.text.lines() {
            lines.push(Line::from(vec![Span::styled(
                format!("  {text_line}"),
                Style::default().fg(Color::White),
            )]));
        }
        if entry.text.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                "  (empty)",
                Style::default().fg(Color::DarkGray),
            )]));
        }
    } else {
        // Response entry.
        let (resp_label, resp_color) = match entry.role {
            AgentRole::Developer => ("◀ Developer response", Color::Cyan),
            AgentRole::Reviewer => ("◀ Reviewer response", Color::Magenta),
        };
        lines.push(Line::from(vec![Span::styled(
            resp_label,
            Style::default().fg(resp_color).add_modifier(Modifier::BOLD),
        )]));
        // Response text.
        let text_to_show = if entry.complete {
            entry.text.clone()
        } else {
            // Still streaming — append cursor.
            format!("{}▌", entry.text)
        };
        for text_line in text_to_show.lines() {
            // Parse embedded ANSI SGR runs into styled spans (no literal escape
            // byte survives), then overlay a diff base colour where the line is a
            // diff add/remove/hunk line — ANSI SGR wins where present.
            let diff_style = diff_line_style(text_line);
            let ansi_spans = parse_ansi(text_line);

            let mut spans: Vec<Span<'static>> = Vec::new();
            // Two-space indent, default-styled, owning its text.
            spans.push(Span::raw("  "));
            for AnsiSpan { text, style } in ansi_spans {
                let style = match (diff_style, style.fg) {
                    // ANSI SGR set no foreground → overlay ONLY the diff base
                    // colour, preserving any add_modifier (e.g. BOLD) the span's
                    // ANSI run set.  Replacing the whole style here would drop
                    // those modifiers (spec `tui-exchange-render` step 3: overlay
                    // the foreground only).  `base.fg` is always `Some` for a
                    // diff line, but fall back to the span's own fg defensively.
                    (Some(base), None) => match base.fg {
                        Some(base_color) => style.fg(base_color),
                        None => style,
                    },
                    // ANSI SGR set a foreground → it wins over the diff base.
                    _ => style,
                };
                spans.push(Span::styled(text, style));
            }
            lines.push(Line::from(spans));
        }
        if entry.text.is_empty() && !entry.complete {
            lines.push(Line::from(vec![Span::styled(
                "  ▌",
                Style::default().fg(Color::DarkGray),
            )]));
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
fn render_file_browser(browser: &crate::browser::FileBrowser, frame: &mut Frame, area: Rect) {
    // Centre a box ~80% wide / 80% tall.
    let popup = centered_rect(80, 80, area);

    // Clear the region first so the popup is opaque.
    frame.render_widget(Clear, popup);

    let title = format!(" Open task list — {} ", browser.cwd.display());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(Color::Magenta))
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
            Style::default().fg(Color::DarkGray),
        )]));
        frame.render_widget(empty, list_area);
    } else {
        let items: Vec<ListItem> = browser
            .entries
            .iter()
            .map(|entry| {
                let (icon, name_color) = if entry.is_dir {
                    ("▸ ", Color::Cyan)
                } else {
                    ("  ", Color::White)
                };
                let suffix = if entry.is_dir { "/" } else { "" };
                let line = Line::from(vec![
                    Span::styled(icon, Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{}{}", entry.name, suffix),
                        Style::default().fg(name_color),
                    ),
                ]);
                ListItem::new(line)
            })
            .collect();

        let highlight_style = Style::default()
            .fg(Color::Black)
            .bg(Color::Magenta)
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
        Style::default().fg(Color::DarkGray),
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
fn panel_block(title: &str, focused: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(Color::Blue)
    } else {
        Style::default().fg(Color::DarkGray)
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
fn status_badge(s: &makina_core::api::RunStatus) -> (&'static str, Color) {
    use makina_core::api::RunStatus;
    match s {
        RunStatus::Pending => ("[·]", Color::DarkGray),
        RunStatus::Running => ("[▶]", Color::Green),
        RunStatus::Paused => ("[‖]", Color::Yellow),
        RunStatus::Completed => ("[✓]", Color::Cyan),
        RunStatus::Failed => ("[✗]", Color::Red),
    }
}

fn status_color(s: &makina_core::api::RunStatus) -> Color {
    use makina_core::api::RunStatus;
    match s {
        RunStatus::Pending => Color::DarkGray,
        RunStatus::Running => Color::Green,
        RunStatus::Paused => Color::Yellow,
        RunStatus::Completed => Color::Cyan,
        RunStatus::Failed => Color::Red,
    }
}

/// Return a fixed-width status badge text and its display colour for a [`TaskState`].
///
/// Badge format is a short bracketed label (≤12 chars) consistent with the
/// RunStatus badges in the sidebar.  Colours reuse the same palette as
/// [`task_state_color`].
fn task_state_badge(s: &makina_core::api::TaskState) -> (&'static str, Color) {
    use makina_core::api::TaskState;
    match s {
        TaskState::New => ("[new]", Color::DarkGray),
        TaskState::Ready => ("[ready]", Color::White),
        TaskState::InProgress => ("[▶ working]", Color::Green),
        TaskState::InReview => ("[⧗ review]", Color::Yellow),
        TaskState::Done => ("[✓ done]", Color::Cyan),
        TaskState::Failed => ("[✗ failed]", Color::Red),
        TaskState::Skipped => ("[⊘ skipped]", Color::DarkGray),
    }
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
        Event::AgentExchange { .. } => "AgentExchange",
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::placeholder::PlaceholderApi;
    use makina_core::api::{
        IngestionIssue, IngestionReport, IssueSeverity, IssueSource, RunId, RunStatus, RunView,
        TaskId, TaskState, TaskView,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn make_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        Terminal::new(backend).unwrap()
    }

    // ── Render: empty state ───────────────────────────────────────────────────

    #[test]
    fn render_empty_state_contains_title_and_panels() {
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::new());
        let app = App::new(api, vec![]);

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
        let app = App::new(api, vec![]);

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

    /// The status bar must show the run-control key hints (`[o]`, `[s/p/c]`).
    #[test]
    fn render_status_bar_shows_run_control_hints() {
        let mut terminal = make_terminal(100, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![]);

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
        let mut terminal = make_terminal(100, 24);
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![]);
        app.update(crate::app::AppEvent::StatusMessage("Start run:1".into()));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("Start run:1"),
            "status bar must render the transient status_message"
        );
    }

    // ── Render: with runs ─────────────────────────────────────────────────────

    #[test]
    fn render_with_run_shows_run_in_sidebar() {
        let mut terminal = make_terminal(80, 24);
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
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run]);

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
        // The main area shows the first task's title.
        assert!(
            screen.contains("First task"),
            "main area should list the run's tasks"
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
        let app = App::new(api, runs);

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
                task_list_path: PathBuf::from("docs/plans/0002-Governance-and-Persistence/TASKS.md"),
                status: RunStatus::Running,
                project: "makina".into(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
        ];
        let app = App::new(api, runs);

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
        let app = App::new(api, runs);

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
        let app = App::new(api, runs);
        assert_eq!(app.selected_run, Some(0));

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // Find a cell with the Cyan background (the highlight colour) — there
        // must be at least one such cell within the sidebar region (columns 0..30).
        let has_highlight = buf
            .content()
            .iter()
            .any(|cell| cell.bg == ratatui::style::Color::Cyan);
        assert!(
            has_highlight,
            "selected row must use Cyan highlight background"
        );
    }

    // ── Render: status indicator colours in the cell ──────────────────────────

    #[test]
    fn render_running_badge_uses_green_fg() {
        let mut terminal = make_terminal(100, 24);
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
        let app = App::new(api, runs);

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // At least one cell with Green fg must exist (the Running badge).
        let has_green = buf
            .content()
            .iter()
            .any(|cell| cell.fg == ratatui::style::Color::Green);
        assert!(has_green, "Running status badge must use Green foreground");
    }

    #[test]
    fn render_failed_badge_uses_red_fg() {
        let mut terminal = make_terminal(100, 24);
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
        let app = App::new(api, runs);

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let has_red = buf
            .content()
            .iter()
            .any(|cell| cell.fg == ratatui::style::Color::Red);
        assert!(has_red, "Failed status badge must use Red foreground");
    }

    // ── Render: ingestion report panel (ingest-tui-report-panel) ──────────────

    #[test]
    fn render_ingestion_panel_shows_blocking_issue() {
        let mut terminal = make_terminal(100, 24);
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
        let app = App::new(api, runs);

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("vague-done-when"),
            "ingestion panel should contain the issue code"
        );
        assert!(
            screen.contains("write a concrete acceptance criterion"),
            "ingestion panel should contain the suggestion text"
        );

        let buf = terminal.backend().buffer().clone();
        let has_red = buf
            .content()
            .iter()
            .any(|cell| cell.fg == ratatui::style::Color::Red);
        assert!(has_red, "Blocking issue line must use Red foreground");
    }

    #[test]
    fn render_ingestion_panel_shows_warning_issue() {
        let mut terminal = make_terminal(100, 24);
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
        let app = App::new(api, runs);

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
            .any(|cell| cell.fg == ratatui::style::Color::Yellow);
        assert!(has_yellow, "Warning issue line must use Yellow foreground");
    }

    #[test]
    fn render_status_bar_shows_blocked_notice_when_report_blocked() {
        let mut terminal = make_terminal(120, 30);
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
        let app = App::new(api, runs);

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
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![]);

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
        let mut app = App::new(api, vec![]);
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
            .any(|cell| cell.bg == ratatui::style::Color::Magenta);
        assert!(
            has_highlight,
            "selected browser row must use the Magenta highlight background"
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
        let app = App::new(api, vec![]);

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
                },
                TaskView {
                    id: TaskId::new("beta"),
                    title: "Beta task".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 1,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("alpha")],
                },
                TaskView {
                    id: TaskId::new("gamma"),
                    title: "Gamma task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("beta")],
                },
                TaskView {
                    id: TaskId::new("delta"),
                    title: "Delta task".into(),
                    state: TaskState::Failed,
                    gate_iterations: 3,
                    review_iterations: 1,
                    depends_on: vec![],
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        App::new(api, vec![run])
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
        let app = task_status_app();

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Beta task has gate_iterations=1; Delta has gate_iterations=3.
        assert!(screen.contains('1'), "gate iteration count 1 must appear");
        assert!(screen.contains('3'), "gate iteration count 3 must appear");
        // Alpha task has review_iterations=2.
        assert!(screen.contains('2'), "review iteration count 2 must appear");
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
                },
                TaskView {
                    id: TaskId::new("a"),
                    title: "A task".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("c")],
                },
                TaskView {
                    id: TaskId::new("b"),
                    title: "B task".into(),
                    state: TaskState::Failed,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
                TaskView {
                    id: TaskId::new("c"),
                    title: "C task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run]);
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
            for row in &rows {
                let row_str: String = row.iter().collect();
                if row_str.contains(badge) {
                    // Find the first tree connector char on this row.
                    if let Some(col) = row.iter().position(|&ch| ch == '├' || ch == '└') {
                        return col;
                    }
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

    /// `dependency_levels` assigns parallelisable siblings (no deps) the same
    /// level 0 and a dependent the next level up.  Fixture: A (no deps),
    /// B (no deps), C (`depends_on A`) ⇒ level(A) == level(B) == 0, level(C) == 1.
    #[test]
    fn dependency_levels_assigns_parallel_siblings_same_level() {
        use makina_core::api::{TaskId, TaskState, TaskView};

        let tasks = vec![
            TaskView {
                id: TaskId::new("A"),
                title: "A".into(),
                state: TaskState::Ready,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
            },
            TaskView {
                id: TaskId::new("B"),
                title: "B".into(),
                state: TaskState::Ready,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
            },
            TaskView {
                id: TaskId::new("C"),
                title: "C".into(),
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![TaskId::new("A")],
            },
        ];

        let levels = dependency_levels(&tasks);
        // Helper: the level index a given task id landed in.
        let level_of_id = |id: &str| -> usize {
            levels
                .iter()
                .position(|lane| lane.iter().any(|t| t.id.0 == id))
                .expect("task should appear in some level")
        };

        assert_eq!(level_of_id("A"), 0, "A has no deps → level 0");
        assert_eq!(level_of_id("B"), 0, "B has no deps → level 0");
        assert_eq!(
            level_of_id("A"),
            level_of_id("B"),
            "A and B are parallel siblings"
        );
        assert_eq!(level_of_id("C"), 1, "C depends on A → level 1");
    }

    /// With [`DependencyViewMode::Timeline`] active, the dependency sub-pane
    /// renders a lane view: independent tasks A and B (level 0) share the same
    /// terminal row while dependent C (`depends_on A`, level 1) renders on a
    /// strictly later row.
    #[test]
    fn timeline_groups_independent_tasks_and_orders_dependents() {
        use crate::app::DependencyViewMode;
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};

        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/timeline-test.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("alpha"),
                    title: "Alpha task".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
                TaskView {
                    id: TaskId::new("beta"),
                    title: "Beta task".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
                TaskView {
                    id: TaskId::new("gamma"),
                    title: "Gamma task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("alpha")],
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run]);
        app.dependency_view = DependencyViewMode::Timeline;

        let mut terminal = make_terminal(120, 40);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // `screen` is a flat `width * height` char string laid out row-major;
        // split it into 120-char rows on char boundaries.
        let chars: Vec<char> = screen.chars().collect();
        let rows: Vec<Vec<char>> = chars.chunks(120).map(|c| c.to_vec()).collect();
        // Locate each id's row by scanning the rendered lines.
        let row_of = |id: &str| -> usize {
            rows.iter()
                .position(|row| {
                    let s: String = row.iter().collect();
                    s.contains(id)
                })
                .unwrap_or(usize::MAX)
        };
        let alpha_row = row_of("alpha");
        let beta_row = row_of("beta");
        let gamma_row = row_of("gamma");

        assert_ne!(alpha_row, usize::MAX, "alpha must render somewhere");
        assert_ne!(beta_row, usize::MAX, "beta must render somewhere");
        assert_ne!(gamma_row, usize::MAX, "gamma must render somewhere");

        // alpha and beta (level 0) share the same terminal row.
        assert_eq!(
            alpha_row, beta_row,
            "independent tasks alpha and beta must appear on the SAME row"
        );
        // gamma (level 1, depends_on alpha) is on a strictly later row.
        assert!(
            gamma_row > alpha_row,
            "dependent gamma must appear on a strictly LATER row than alpha \
             (alpha_row={alpha_row}, gamma_row={gamma_row})"
        );
    }

    /// The status bar shows the `G`/`R` legend when the selected run carries
    /// non-zero gate/review iteration counts.
    #[test]
    fn render_status_bar_shows_gr_legend_when_counts_nonzero() {
        let mut terminal = make_terminal(120, 30);
        let app = task_status_app();

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("gate iterations"),
            "legend should explain the G column"
        );
        assert!(
            screen.contains("review iterations"),
            "legend should explain the R column"
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
                },
                TaskView {
                    id: TaskId::new("beta"),
                    title: "Beta task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run]);

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
        let app = task_status_app();

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // Cyan for Done.
        let has_cyan = buf
            .content()
            .iter()
            .any(|cell| cell.fg == ratatui::style::Color::Cyan);
        assert!(has_cyan, "Done badge must use Cyan foreground");

        // Red for Failed.
        let has_red = buf
            .content()
            .iter()
            .any(|cell| cell.fg == ratatui::style::Color::Red);
        assert!(has_red, "Failed badge must use Red foreground");

        // Green for InProgress.
        let has_green = buf
            .content()
            .iter()
            .any(|cell| cell.fg == ratatui::style::Color::Green);
        assert!(has_green, "InProgress badge must use Green foreground");
    }

    /// When no run is selected the main panel must show the hint text.
    #[test]
    fn render_no_run_selected_shows_hint() {
        let mut terminal = make_terminal(120, 30);
        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![]);

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
        let app = App::new(api, vec![run]);

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        assert!(
            screen.contains("Loading tasks"),
            "run with no tasks must show 'Loading tasks' hint"
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
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run]);

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
        // '1' is the gate iteration count.
        assert!(
            screen.contains('1'),
            "gate iteration count must appear after TaskIterationsUpdated"
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
            screen2.contains('2'),
            "review iteration count 2 must appear after update"
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
                },
                TaskView {
                    id: TaskId::new("task-b"),
                    title: "Task B".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run]);

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

        // Switch Main panel focus and navigate to task-b (index 1).
        app.update(AppEvent::FocusNext); // focus → Main
        app.update(AppEvent::SelectDown); // task selection → index 1
        assert_eq!(app.selected_task, Some(1));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen_b = screen_of(&terminal);
        assert!(
            screen_b.contains("task-b only"),
            "task-b exchange must be visible when task-b is focused"
        );
        assert!(
            !screen_b.contains("implement X"),
            "task-a exchange must NOT appear when task-b is focused"
        );
    }

    /// **Task row highlight:** when Main panel is focused the focused task row
    /// must be highlighted (Cyan background).
    #[test]
    fn render_focused_task_row_is_highlighted() {
        let mut terminal = make_terminal(120, 40);
        let app = exchange_app();

        terminal.draw(|f| render(&app, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let has_cyan = buf
            .content()
            .iter()
            .any(|cell| cell.bg == ratatui::style::Color::Cyan);
        assert!(
            has_cyan,
            "focused task row must use Cyan highlight background"
        );
    }

    /// **ANSI + diff styling (done-when):** a response whose text carries an
    /// embedded SGR escape and a `@@` hunk header must render with no literal
    /// escape byte and no raw `[..m` SGR text, with the `+added` line green and
    /// the `@@` line cyan.
    #[test]
    fn exchange_render_styles_ansi_and_diff_no_literal_escape() {
        use crate::app::AppEvent;
        use makina_core::api::{
            AgentRole, Event, ExchangeEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };

        let mut terminal = make_terminal(120, 40);

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
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run]);
        assert_eq!(app.selected_task, Some(0));

        // Response text: an ANSI-green `+added` diff line and a `@@` hunk header.
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

        // (b) The `+added` line must have at least one Green cell and the `@@`
        // line must have at least one Cyan cell.
        let row_text = |row: u16| -> String {
            (0..buf.area.width)
                .map(|col| buf[(col, row)].symbol().chars().next().unwrap_or(' '))
                .collect()
        };
        let added_fg_green = (0..buf.area.height).any(|row| {
            row_text(row).contains("+added")
                && (0..buf.area.width).any(|col| buf[(col, row)].fg == ratatui::style::Color::Green)
        });
        let hunk_fg_cyan = (0..buf.area.height).any(|row| {
            row_text(row).contains("@@ -1,2 +1,2 @@")
                && (0..buf.area.width).any(|col| buf[(col, row)].fg == ratatui::style::Color::Cyan)
        });
        assert!(
            added_fg_green,
            "the `+added` line must have at least one Green-foreground cell"
        );
        assert!(
            hunk_fg_cyan,
            "the `@@` hunk header line must have at least one Cyan-foreground cell"
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
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run]);
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
            .any(|cell| cell.fg == ratatui::style::Color::Red);
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

    /// Regression: when a diff `+`/`-`/`@@` line carries an ANSI run that set a
    /// modifier (e.g. `\x1b[1m` → BOLD) but NO foreground, overlaying the diff
    /// base colour must set ONLY the foreground and PRESERVE the modifier.
    ///
    /// Before the fix, `(Some(base), None) => base` replaced the whole `Style`,
    /// dropping `add_modifier` (the BOLD) on such a span.
    #[test]
    fn diff_base_overlay_preserves_ansi_bold_modifier() {
        use crate::app::ExchangeEntry;
        use makina_core::api::AgentRole;

        // A complete Developer response whose single text line is a diff-add
        // line (`+`) that opens BOLD via ANSI but sets no foreground colour.
        let entry = ExchangeEntry {
            role: AgentRole::Developer,
            is_prompt: false,
            text: "+\x1b[1madded bold line".to_string(),
            complete: true,
        };

        let lines = exchange_entry_lines(&entry);

        // Find the span carrying the diff text and assert BOTH the diff base
        // foreground (green) AND the BOLD modifier survive.
        let mut found = false;
        for line in &lines {
            for span in line.spans.iter() {
                if span.content.contains("added bold line") {
                    found = true;
                    assert_eq!(
                        span.style.fg,
                        Some(Color::Green),
                        "diff '+' line must overlay the green diff base colour"
                    );
                    assert!(
                        span.style.add_modifier.contains(Modifier::BOLD),
                        "ANSI BOLD modifier must be preserved when overlaying the diff base"
                    );
                }
            }
        }
        assert!(
            found,
            "the diff text span must be present in the rendered lines"
        );
    }
}
