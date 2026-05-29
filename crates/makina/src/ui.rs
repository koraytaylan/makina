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
//! │                 │                                    │
//! │  Sidebar        │  Main content                      │
//! │  (Runs list)    │  (task-status / prompt-answer)     │
//! │  task 27 ─────► │  tasks 29–31 ──────────────────►  │
//! │                 │                                    │
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
//!   area for the focused task.
//! * Task 31 (run-control): add keybind hints to the status bar.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph},
};

use crate::app::{App, Panel};

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
        // Empty state: show a hint instead of an empty list.
        let empty_text = vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                "  No runs open.",
                Style::default().fg(Color::DarkGray),
            )]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "  Open a run via",
                Style::default().fg(Color::DarkGray),
            )]),
            Line::from(vec![Span::styled(
                "  run-control (task 31).",
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
                let name = run
                    .task_list_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
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

    // ── Main content ──────────────────────────────────────────────────────────
    let main_focused = app.focused_panel == Panel::Main;
    let main_block = panel_block("Detail", main_focused);

    // Placeholder: show selected run info, or a hint.
    // Tasks 29–31 replace this with real task-status / prompt-answer widgets.
    let main_content = match app.selected_run.and_then(|i| app.runs.get(i)) {
        None => vec![
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
        ],
        Some(run) => {
            let mut lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("  Run:  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        run.task_list_path.display().to_string(),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Status: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{:?}", run.status),
                        Style::default().fg(status_color(&run.status)),
                    ),
                ]),
                Line::from(vec![Span::styled(
                    format!("  Tasks:  {}", run.tasks.len()),
                    Style::default().fg(Color::DarkGray),
                )]),
                Line::from(""),
            ];
            // Task-list placeholder — task 29 (task-status-view) fills this in.
            for task in &run.tasks {
                lines.push(Line::from(vec![
                    Span::styled("  • ", Style::default().fg(Color::DarkGray)),
                    Span::styled(task.title.clone(), Style::default().fg(Color::White)),
                    Span::styled(
                        format!("  [{:?}]", task.state),
                        Style::default().fg(task_state_color(&task.state)),
                    ),
                ]));
            }
            lines
        }
    };

    let main_para = Paragraph::new(main_content)
        .block(main_block)
        .style(Style::default().fg(Color::White));
    frame.render_widget(main_para, main_area);

    // ── Status bar ────────────────────────────────────────────────────────────
    let focus_label = match app.focused_panel {
        Panel::Sidebar => "focus: sidebar",
        Panel::Main => "focus: main",
    };
    let event_hint = match &app.last_event {
        None => String::new(),
        Some(ev) => format!("  │  last: {}", event_short_name(ev)),
    };
    let status_text =
        format!(" [o] open  [Tab] switch panel  [q/Esc/^C] quit  {focus_label}{event_hint}");
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

fn task_state_color(s: &makina_core::api::TaskState) -> Color {
    use makina_core::api::TaskState;
    match s {
        TaskState::New => Color::DarkGray,
        TaskState::Ready => Color::White,
        TaskState::InProgress => Color::Green,
        TaskState::InReview => Color::Yellow,
        TaskState::Done => Color::Cyan,
        TaskState::Failed => Color::Red,
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

    // ── Render: with runs ─────────────────────────────────────────────────────

    #[test]
    fn render_with_run_shows_run_in_sidebar() {
        let mut terminal = make_terminal(80, 24);
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            task_list_path: PathBuf::from(".tasks/my-feature.json"),
            status: RunStatus::Running,
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
                task_list_path: PathBuf::from(".tasks/alpha.json"),
                status: RunStatus::Running,
                tasks: vec![],
            },
            RunView {
                id: RunId(2),
                task_list_path: PathBuf::from(".tasks/beta.json"),
                status: RunStatus::Failed,
                tasks: vec![],
            },
            RunView {
                id: RunId(3),
                task_list_path: PathBuf::from(".tasks/gamma.json"),
                status: RunStatus::Completed,
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
    fn render_sidebar_shows_all_status_badges() {
        // One run per RunStatus variant — all badges must appear.
        let mut terminal = make_terminal(100, 40);
        let api = Arc::new(PlaceholderApi::empty());
        let runs = vec![
            RunView {
                id: RunId(1),
                task_list_path: PathBuf::from(".tasks/pending.json"),
                status: RunStatus::Pending,
                tasks: vec![],
            },
            RunView {
                id: RunId(2),
                task_list_path: PathBuf::from(".tasks/running.json"),
                status: RunStatus::Running,
                tasks: vec![],
            },
            RunView {
                id: RunId(3),
                task_list_path: PathBuf::from(".tasks/paused.json"),
                status: RunStatus::Paused,
                tasks: vec![],
            },
            RunView {
                id: RunId(4),
                task_list_path: PathBuf::from(".tasks/completed.json"),
                status: RunStatus::Completed,
                tasks: vec![],
            },
            RunView {
                id: RunId(5),
                task_list_path: PathBuf::from(".tasks/failed.json"),
                status: RunStatus::Failed,
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
                task_list_path: PathBuf::from(".tasks/first.json"),
                status: RunStatus::Running,
                tasks: vec![],
            },
            RunView {
                id: RunId(2),
                task_list_path: PathBuf::from(".tasks/second.json"),
                status: RunStatus::Pending,
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
            task_list_path: PathBuf::from(".tasks/live.json"),
            status: RunStatus::Running,
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
            task_list_path: PathBuf::from(".tasks/broken.json"),
            status: RunStatus::Failed,
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
}
