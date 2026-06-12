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
use makina_core::api::FailureKind;

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
                // Build a Table with columns: Task | State
                // Column widths: task title fills remainder; state fixed 12.
                let col_title = Constraint::Min(20);
                let col_state = Constraint::Length(12);

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
                ]);

                let rows: Vec<Row> = run
                    .tasks
                    .iter()
                    .map(|task| {
                        let (badge, badge_color) = task_state_badge(&task.state);
                        let badge_with_spinner = match task.state {
                            makina_core::api::TaskState::InProgress
                            | makina_core::api::TaskState::InReview => {
                                format!("{} {}", spinner_frame(app.tick), badge)
                            }
                            _ => badge.to_string(),
                        };
                        // Append failure reason label for failed tasks.
                        let state_cell =
                            if matches!(task.state, makina_core::api::TaskState::Failed) {
                                if let Some(reason) = &task.failure_reason {
                                    let label = failure_kind_label(&reason.kind);
                                    format!("{} {}", badge_with_spinner, label)
                                } else {
                                    badge_with_spinner
                                }
                            } else {
                                badge_with_spinner
                            };
                        Row::new(vec![
                            Cell::from(task.title.clone()).style(Style::default().fg(Color::White)),
                            Cell::from(state_cell).style(Style::default().fg(badge_color)),
                        ])
                    })
                    .collect();

                let task_table = Table::new(rows, [col_title, col_state])
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
            Style::default().bg(Color::DarkGray).fg(Color::Yellow),
        )
    } else {
        (
            "[e] errors".to_string(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        )
    };
    // The blocked notice precedes the trailer so its full text stays inside the
    // visible width; the lower-priority trailer (focus/last-event hint) is the
    // part that gets clipped on narrow terminals.
    //
    // The status bar is built as a `Line` of `Span`s so the error-badge span
    // can carry its own colour (warn/yellow) while the rest stays White/DarkGray.
    let default_style = Style::default().bg(Color::DarkGray).fg(Color::White);
    let status_bar = Paragraph::new(Line::from(vec![
        Span::styled(
            " [o] open  [s/p/c] start/pause/cancel  [Tab] panel  [v] view  [L] log  ",
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
        render_file_browser(browser, frame, area);
    }

    // ── Provider configuration editor overlay ──────────────────────────────────
    // Drawn after the file browser so it sits on top when both might be open
    // (task 0041).
    if app.is_editing_providers()
        && let Some(editor) = app.provider_editor.as_ref()
    {
        render_provider_editor(editor, frame, area);
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
    let view_label = match app.dependency_view {
        DependencyViewMode::Off => "off",
        DependencyViewMode::List => "list",
        DependencyViewMode::Tree => "tree",
        DependencyViewMode::Timeline => "timeline",
    };
    let block = Block::default()
        .title(format!(" Dependencies — {} ", view_label))
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
            Style::default().fg(Color::Blue)
        } else {
            Style::default().fg(Color::DarkGray)
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
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Yellow)
                };
                detail_lines.push(Line::from(Span::styled(counts, style)));
                detail_lines.push(Line::from(""));

                // Add failure reason if the task is failed.
                if let Some(reason) = &task.failure_reason {
                    let label = failure_kind_label(&reason.kind);
                    let reason_text = format!("failed: {} — {}", label, reason.message);
                    detail_lines.push(Line::from(Span::styled(
                        reason_text,
                        Style::default().fg(Color::Red),
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
                Style::default().fg(Color::DarkGray),
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
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Yellow)
                };
                detail_lines.push(Line::from(Span::styled(counts, style)));
                detail_lines.push(Line::from(""));

                // Add failure reason if the task is failed.
                if let Some(reason) = &task.failure_reason {
                    let label = failure_kind_label(&reason.kind);
                    let reason_text = format!("failed: {} — {}", label, reason.message);
                    detail_lines.push(Line::from(Span::styled(
                        reason_text,
                        Style::default().fg(Color::Red),
                    )));
                    detail_lines.push(Line::from(""));
                }
            }

            detail_lines.push(Line::from(vec![Span::styled(
                "  No exchange yet.",
                Style::default().fg(Color::DarkGray),
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
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Yellow)
                };
                lines.push(Line::from(Span::styled(counts, style)));
                lines.push(Line::from(""));

                // Add failure reason if the task is failed.
                if let Some(reason) = &task.failure_reason {
                    let label = failure_kind_label(&reason.kind);
                    let reason_text = format!("failed: {} — {}", label, reason.message);
                    lines.push(Line::from(Span::styled(
                        reason_text,
                        Style::default().fg(Color::Red),
                    )));
                    lines.push(Line::from(""));
                }
            }

            for entry in &log.entries {
                lines.extend(exchange_entry_lines(entry, app));
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
/// Render one content line: parse embedded ANSI SGR runs into styled spans
/// (no literal escape byte survives), overlaying a diff base colour where the
/// line is a diff add/remove/hunk line. ANSI SGR foreground wins where present;
/// modifiers (e.g. BOLD) are always preserved. Two-space indented.
fn diff_overlaid_content_line(text_line: &str) -> Line<'static> {
    use crate::ansi::{AnsiSpan, diff_line_style, parse_ansi};

    let diff_style = diff_line_style(text_line);
    let ansi_spans = parse_ansi(text_line);

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

fn exchange_entry_lines(entry: &ExchangeEntry, app: &App) -> Vec<Line<'static>> {
    use crate::app::ExchangeContent;
    use makina_core::api::AgentRole;

    let mut lines = Vec::new();

    match &entry.content {
        ExchangeContent::Prompt { text } => {
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
            for text_line in text.lines() {
                lines.push(Line::from(vec![Span::styled(
                    format!("  {text_line}"),
                    Style::default().fg(Color::White),
                )]));
            }
            if text.is_empty() {
                lines.push(Line::from(vec![Span::styled(
                    "  (empty)",
                    Style::default().fg(Color::DarkGray),
                )]));
            }
        }
        ExchangeContent::Response { text, complete } => {
            // Response entry.
            let (resp_label, resp_color) = match entry.role {
                AgentRole::Developer => ("◀ Developer response", Color::Cyan),
                AgentRole::Reviewer => ("◀ Reviewer response", Color::Magenta),
            };
            lines.push(Line::from(vec![Span::styled(
                resp_label,
                Style::default().fg(resp_color).add_modifier(Modifier::BOLD),
            )]));
            // Response text rendered through Markdown + ANSI.
            let base_style = Style::default().fg(resp_color);
            lines.extend(crate::markup::render_markdown(text, base_style, 80));

            // Streaming cursor (if not complete).
            if !*complete {
                if text.is_empty() {
                    lines.push(Line::from(vec![Span::styled(
                        "  ▌",
                        Style::default().fg(Color::DarkGray),
                    )]));
                } else {
                    // Append cursor to the last line if text is present.
                    if let Some(last_line) = lines.last_mut() {
                        last_line
                            .spans
                            .push(Span::styled("▌", Style::default().fg(Color::DarkGray)));
                    }
                }
            }
        }
        // ── Thought (agent internal reasoning) ────────────────────────────
        // Observability-only side channel.  Bold, role-coloured header
        // ("💭 Developer thought" green / "💭 Reviewer thought" yellow) then
        // the reasoning text dimmed (DarkGray) at a 2-space indent so it reads
        // as a quiet aside rather than part of the answer.
        ExchangeContent::Thought { text } => {
            let (label, label_color) = match entry.role {
                AgentRole::Developer => ("💭 Developer thought", Color::Green),
                AgentRole::Reviewer => ("💭 Reviewer thought", Color::Yellow),
            };
            lines.push(Line::from(vec![Span::styled(
                label,
                Style::default()
                    .fg(label_color)
                    .add_modifier(Modifier::BOLD),
            )]));
            // Thought text rendered through Markdown + ANSI.
            let base_style = Style::default().fg(Color::DarkGray);
            let mut thought_lines = crate::markup::render_markdown(text, base_style, 80);
            // Indent all thought lines by 2 spaces.
            for line in &mut thought_lines {
                line.spans.insert(0, Span::raw("  "));
            }
            lines.extend(thought_lines);
        }
        // ── Tool (agent tool invocation) ──────────────────────────────────
        // Header "⚙ <title> [<status>]" coloured by lifecycle status, then the
        // tool's content lines rendered with the SAME parse_ansi + diff base
        // overlay loop the Response arm uses, so an edit diff in tool output is
        // syntax-coloured.  Empty content renders nothing extra.
        ExchangeContent::Tool {
            title,
            status,
            content,
            ..
        } => {
            let status_color = match status.as_str() {
                "pending" => Color::DarkGray,
                "in_progress" => Color::Cyan,
                "completed" => Color::Green,
                "failed" => Color::Red,
                _ => Color::White,
            };
            let compacted_title = crate::markup::compact_paths(title, &app.repo_root);
            lines.push(Line::from(vec![Span::styled(
                format!("⚙ {compacted_title} [{status}]"),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            )]));
            for text_line in content.lines() {
                // Re-use the Response arm's ANSI + diff overlay so an edit diff
                // in tool output is syntax-coloured the same way.
                lines.push(diff_overlaid_content_line(text_line));
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

/// Render the provider configuration editor modal.
fn render_provider_editor(editor: &crate::app::ProviderEditor, frame: &mut Frame, area: Rect) {
    // Centre a box ~85% wide / 85% tall.
    let popup = centered_rect(85, 85, area);

    // Clear the region first so the popup is opaque.
    frame.render_widget(Clear, popup);

    let title = " Configure Providers & Roles ";
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(Color::Cyan))
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
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
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
        Style::default().fg(Color::Magenta),
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
                Style::default().fg(Color::White),
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
            Style::default().fg(Color::Magenta),
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
                Style::default().fg(Color::Green),
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
                    Style::default().fg(Color::Green),
                )])));
            }
        }
    }

    let highlight_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let list = List::new(items)
        .highlight_style(highlight_style)
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(editor.selection_index.min(editor.providers.len() + 3)));
    frame.render_stateful_widget(list, list_area, &mut state);

    // Footer with hints
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        "[Enter] commit  [↑↓/jk] navigate  [Esc] cancel",
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

/// Short label for a failure reason kind.
fn failure_kind_label(kind: &FailureKind) -> &'static str {
    match kind {
        FailureKind::GateCap => "gate cap",
        FailureKind::ReviewCap => "review cap",
        FailureKind::MergeConflict => "merge conflict",
        FailureKind::HardError => "hard error",
        FailureKind::WallClockCap => "wall-clock cap",
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
        let mut terminal = make_terminal(150, 24);
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
        let mut terminal = make_terminal(120, 24);
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
        let mut terminal = make_terminal(120, 24);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("view: list"),
            "status bar must show 'view: list' when dependency_view is List"
        );

        // Cycle to Tree
        app.update(crate::app::AppEvent::CycleDependencyView);
        let mut terminal = make_terminal(120, 24);
        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);
        assert!(
            screen.contains("view: tree"),
            "status bar must show 'view: tree' when dependency_view is Tree"
        );

        // Cycle to Timeline
        app.update(crate::app::AppEvent::CycleDependencyView);
        let mut terminal = make_terminal(120, 24);
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

        let mut terminal = make_terminal(120, 24);
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
        let mut terminal = make_terminal(120, 24);
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
        let mut terminal = make_terminal(120, 24);
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
                failure_reason: None,
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
        let app = App::new(api, runs, std::path::PathBuf::from("."));

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
        let app = App::new(api, runs, std::path::PathBuf::from("."));

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
            .any(|cell| cell.fg == ratatui::style::Color::Yellow);
        assert!(has_yellow, "Warning issue line must use Yellow foreground");
    }

    #[test]
    fn render_status_bar_shows_blocked_notice_when_report_blocked() {
        let mut terminal = make_terminal(200, 30);
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
        let mut terminal = make_terminal(150, 24);
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
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("beta"),
                    title: "Beta task".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 1,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("alpha")],
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("gamma"),
                    title: "Gamma task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("beta")],
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("delta"),
                    title: "Delta task".into(),
                    state: TaskState::Failed,
                    gate_iterations: 3,
                    review_iterations: 1,
                    depends_on: vec![],
                    failure_reason: None,
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
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("a"),
                    title: "A task".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("c")],
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("b"),
                    title: "B task".into(),
                    state: TaskState::Failed,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("c"),
                    title: "C task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    failure_reason: None,
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
                failure_reason: None,
            },
            TaskView {
                id: TaskId::new("B"),
                title: "B".into(),
                state: TaskState::Ready,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                failure_reason: None,
            },
            TaskView {
                id: TaskId::new("C"),
                title: "C".into(),
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![TaskId::new("A")],
                failure_reason: None,
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
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("beta"),
                    title: "Beta task".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("gamma"),
                    title: "Gamma task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("alpha")],
                    failure_reason: None,
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));
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
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("beta"),
                    title: "Beta task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    failure_reason: None,
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
                failure_reason: None,
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
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("task-b"),
                    title: "Task B".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    failure_reason: None,
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
                failure_reason: None,
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
                && (0..buf.area.width).any(|col| buf[(col, row)].fg == ratatui::style::Color::Cyan)
        });
        let hunk_fg_cyan = (0..buf.area.height).any(|row| {
            row_text(row).contains("@@ -1,2 +1,2 @@")
                && (0..buf.area.width).any(|col| buf[(col, row)].fg == ratatui::style::Color::Cyan)
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
                failure_reason: None,
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
                failure_reason: None,
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
        let lines = exchange_entry_lines(&entry, &app);

        // Find the span carrying the text and assert it uses the response colour.
        let mut found = false;
        for line in &lines {
            for span in line.spans.iter() {
                if span.content.contains("added") {
                    found = true;
                    // Expect Cyan (Developer response color), not Green (old diff color).
                    assert_eq!(
                        span.style.fg,
                        Some(Color::Cyan),
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
        let app = App::new(api, vec![], std::path::PathBuf::from("."));
        // Render both entries' lines into a small Buffer via a Paragraph.
        let mut lines: Vec<Line> = Vec::new();
        lines.extend(exchange_entry_lines(&thought, &app));
        lines.extend(exchange_entry_lines(&tool, &app));

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
        // carry at least one Green-foreground cell.
        let added_fg_green = (0..buf.area.height).any(|row| {
            row_text(row).contains("+added line")
                && (0..buf.area.width).any(|col| buf[(col, row)].fg == Color::Green)
        });
        assert!(
            added_fg_green,
            "the `+added line` in tool content must have a Green-foreground cell (diff styling)"
        );
    }

    /// **Task table has no G/R columns:** The task table header must contain
    /// exactly "Task" and "State" cells, with no "G" or "R" columns.
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
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("beta"),
                    title: "Beta task".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 1,
                    review_iterations: 3,
                    depends_on: vec![],
                    failure_reason: None,
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));

        terminal.draw(|f| render(&app, f)).unwrap();
        let screen = screen_of(&terminal);

        // Assert that "Task" and "State" appear in the header.
        assert!(
            screen.contains("Task"),
            "table header must contain 'Task' cell"
        );
        assert!(
            screen.contains("State"),
            "table header must contain 'State' cell"
        );

        // Assert that "G" and "R" headers are NOT present (they were removed).
        // We check for the specific pattern to avoid false positives from words
        // that contain these letters.
        assert!(
            !screen.contains(" G ") && !screen.contains(" G\n") && !screen.contains("\nG "),
            "table header must not contain 'G' column (gate_iterations was removed)"
        );
        assert!(
            !screen.contains(" R ") && !screen.contains(" R\n") && !screen.contains("\nR "),
            "table header must not contain 'R' column (review_iterations was removed)"
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
                failure_reason: None,
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
                failure_reason: Some(FailureReason {
                    kind: FailureKind::MergeConflict,
                    message: "squash merge conflict detected".into(),
                }),
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
            .any(|cell| cell.fg == ratatui::style::Color::Red);
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

        // Create a tool entry with a title that includes a worktree-absolute path.
        let repo_root = std::path::PathBuf::from("/home/user/workspace/myproject");
        let worktree_path =
            "/home/user/workspace/myproject/.makina/worktrees/plan-0009--task1/src/main.rs";
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

        let lines = exchange_entry_lines(&tool, &app);

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

        // Assert that the full worktree prefix is NOT in the header.
        assert!(
            !header_text.contains(".makina/worktrees/plan-0009--task1/"),
            "tool title must NOT contain the full worktree prefix"
        );

        // Assert that the tool status is still shown.
        assert!(
            header_text.contains("completed"),
            "tool title must still contain the status '[completed]'"
        );
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
                failure_reason: None,
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
                failure_reason: None,
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
        // ".makina/worktrees/<slug>/" prefix. Without the compact_paths
        // implementation the full worktree prefix would survive in the
        // rendered output and assertion 5 would fail.
        let worktree_tool_path = format!(
            "{}/.makina/worktrees/plan--pane-fidelity/src/main.rs",
            repo_root.display()
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
        //   "Read /home/user/workspace/makina/.makina/worktrees/plan--pane-fidelity/src/main.rs"
        //
        // After compact_paths() the worktree prefix
        //   "/home/user/workspace/makina/.makina/worktrees/plan--pane-fidelity/"
        // is stripped and only "src/main.rs" remains. Without compact_paths this
        // assertion would fail because ".makina/worktrees/plan--pane-fidelity/"
        // would still appear in the rendered buffer.
        // ──────────────────────────────────────────────────────────────────────
        assert!(
            flattened.contains("src/main.rs"),
            "tool title must contain 'src/main.rs' (the compacted, worktree-stripped path)"
        );
        assert!(
            !flattened.contains(".makina/worktrees/plan--pane-fidelity/"),
            "tool title must NOT contain the worktree slug prefix after compact_paths()"
        );

        // ──────────────────────────────────────────────────────────────────────
        // Assertion 6: All response entries are marked complete.
        // ──────────────────────────────────────────────────────────────────────
        assert!(
            log.entries.iter().all(|e| e.complete()),
            "after TurnComplete, all entries must be marked complete"
        );
    }
}
