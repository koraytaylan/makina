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

use crate::app::{App, ExchangeEntry, Panel};

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

            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(header_height),
                    Constraint::Length(table_rows),
                    Constraint::Min(3), // exchange pane — always at least 3 rows
                    Constraint::Length(error_pane_height), // error pane (0 = hidden)
                ])
                .split(inner);

            let header_area = split[0];
            let table_area = split[1];
            let exchange_area = split[2];
            let error_area = split[3];

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

            // ── Exchange pane (task 30) ────────────────────────────────────
            render_exchange_pane(app, frame, exchange_area, main_focused);

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
    let status_text =
        format!(" [o] open  [s/p/c] start/pause/cancel  [Tab] panel  [q/^C] quit{trailer}");
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

            // Auto-scroll: compute scroll offset so the last lines are visible.
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

/// Convert a single [`ExchangeEntry`] into display [`Line`]s.
///
/// Prompt entries get a role-coloured label header; response entries are
/// indented and shown in a lighter colour.  An in-progress streaming response
/// (not yet complete) gets a trailing `▌` cursor indicator.
fn exchange_entry_lines(entry: &ExchangeEntry) -> Vec<Line<'static>> {
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
            lines.push(Line::from(vec![Span::styled(
                format!("  {text_line}"),
                Style::default().fg(Color::White),
            )]));
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
    use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
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
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/beta.json"),
                status: RunStatus::Failed,
                project: String::new(),
                tasks: vec![],
            },
            RunView {
                id: RunId(3),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/gamma.json"),
                status: RunStatus::Completed,
                project: String::new(),
                tasks: vec![],
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
            },
            RunView {
                id: RunId(1),
                run_uid: String::new(),
                task_list_path: PathBuf::from("docs/plans/0002-Governance-and-Persistence/TASKS.md"),
                status: RunStatus::Running,
                project: "makina".into(),
                tasks: vec![],
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
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/running.json"),
                status: RunStatus::Running,
                project: String::new(),
                tasks: vec![],
            },
            RunView {
                id: RunId(3),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/paused.json"),
                status: RunStatus::Paused,
                project: String::new(),
                tasks: vec![],
            },
            RunView {
                id: RunId(4),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/completed.json"),
                status: RunStatus::Completed,
                project: String::new(),
                tasks: vec![],
            },
            RunView {
                id: RunId(5),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/failed.json"),
                status: RunStatus::Failed,
                project: String::new(),
                tasks: vec![],
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
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/second.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
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
}
