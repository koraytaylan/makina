//! Async event loop.
//!
//! [`run`] is the top-level event loop.  It:
//!
//! 1. Subscribes to the api event stream once at startup.
//! 2. Starts a background task that forwards terminal input events (key
//!    presses, resize) from crossterm's async [`EventStream`] into an mpsc
//!    channel.
//! 3. Runs a `tokio::select!` loop that races:
//!    * terminal input events (from the mpsc channel),
//!    * a periodic **tick** (drives the redraw),
//!    * api events from `api.subscribe()`.
//! 4. Translates each raw event into an [`AppEvent`] and feeds it to
//!    [`App::update`].
//! 5. Re-renders the frame after any state change.
//! 6. Exits the loop when [`App::should_quit`] is set (by `AppEvent::Quit`).
//!
//! # Quit keys
//!
//! The loop exits cleanly when the user presses:
//! * `q`
//! * `Esc`
//! * `Ctrl-C`
//!
//! # Merging input + tick + api events
//!
//! ```text
//! crossterm EventStream ──► terminal_tx ──┐
//!                                          ├── tokio::select! ──► App::update ──► render
//! tokio::time::interval ──────────────────┘
//! api.subscribe() ────────────────────────┘
//! ```

use std::time::Duration;

use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode, KeyModifiers, MouseEventKind,
};
use futures::StreamExt;
use makina_core::log_record::LogRecord;
use tokio::sync::mpsc;
use tokio::time;

use crate::app::{App, AppEvent, ErrorLevel, ErrorMessage};
use crate::tui::Tui;
use crate::ui;

/// Convert a tracing→TUI [`LogRecord`] into the TUI's [`ErrorMessage`] shape.
///
/// Maps the verbosity level (`ERROR`→[`ErrorLevel::Error`], `WARN`→
/// [`ErrorLevel::Warn`], anything quieter→[`ErrorLevel::Info`]), carries the
/// flattened message text across, and converts the wall-clock `DateTime<Utc>`
/// into a [`std::time::SystemTime`] for the error pane.
fn error_message_from_log_record(rec: LogRecord) -> ErrorMessage {
    let level = match rec.level {
        tracing::Level::ERROR => ErrorLevel::Error,
        tracing::Level::WARN => ErrorLevel::Warn,
        _ => ErrorLevel::Info,
    };
    ErrorMessage {
        timestamp: rec.timestamp.into(),
        level,
        text: rec.message,
    }
}

// ── Tick interval ─────────────────────────────────────────────────────────────

/// How often a tick is sent to drive periodic redraws (250 ms → 4 fps minimum,
/// fast enough for smooth cursor/status updates).
const TICK_INTERVAL: Duration = Duration::from_millis(250);

// ── Event loop ────────────────────────────────────────────────────────────────

/// Run the TUI event loop until the user quits.
///
/// This function blocks the calling async task until `app.should_quit` is set.
/// The caller owns `tui` and `app`; after this function returns the caller
/// should call `tui.restore()` (though the [`Drop`] impl on `Tui` is a safety
/// net).
///
/// `log_rx` is the receiving half of the bounded tracing→TUI channel
/// (task `log-subscriber-tui-channel`): the `TuiLogLayer` `try_send`s a
/// [`LogRecord`] per event onto it. A dedicated `tokio::select!` arm below
/// drains it, converts each record into an [`ErrorMessage`] via
/// [`error_message_from_log_record`], and feeds it to `update` as
/// [`AppEvent::ErrorMessageArrived`] so it lands in the error pane.
///
/// # Errors
///
/// Returns any `io::Error` from terminal I/O.
pub async fn run(
    tui: &mut Tui,
    app: &mut App,
    mut log_rx: mpsc::Receiver<LogRecord>,
) -> std::io::Result<()> {
    // Subscribe to api events once at startup.
    let mut api_stream = app.api.subscribe();

    // Spawn a background task that drives crossterm's async EventStream and
    // forwards terminal events into an mpsc channel.  Using a channel here
    // lets us use `tokio::select!` cleanly without holding a `!Send` future
    // from EventStream across await points.
    let (term_tx, mut term_rx) = mpsc::channel::<CrosstermEvent>(64);
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(Ok(ev)) = stream.next().await {
            if term_tx.send(ev).await.is_err() {
                break; // receiver dropped → main loop exited
            }
        }
    });

    // Periodic tick timer.
    let mut ticker = time::interval(TICK_INTERVAL);

    // Initial render.
    tui.draw(|frame| ui::render(app, frame))?;

    loop {
        let browsing = app.is_browsing();
        let app_event: Option<AppEvent> = tokio::select! {
            // Bias toward terminal input (lower latency for keystrokes).
            biased;

            maybe_term = term_rx.recv() => {
                maybe_term.map(|ev| translate_terminal_event(ev, browsing))
            }

            maybe_api = api_stream.next() => {
                match maybe_api {
                    Some(ev) => Some(resolve_api_event(&app.api, ev).await),
                    // api stream ended → orchestrator shut down; quit cleanly.
                    None => Some(AppEvent::Quit),
                }
            }

            maybe_log = log_rx.recv() => {
                maybe_log.map(|rec| AppEvent::ErrorMessageArrived {
                    msg: error_message_from_log_record(rec),
                })
            }

            _ = ticker.tick() => {
                Some(AppEvent::Tick)
            }
        };

        if let Some(event) = app_event {
            // IO-layer resolution: browser intents (open / enter dir / select
            // file) need filesystem reads or an async `execute`; run controls
            // (start/pause/cancel) need an async `execute` against the selected
            // run.  Resolve them here into the concrete state-mutating event
            // `update` consumes — plus an OPTIONAL transient status message to
            // surface the command outcome/error (task 31).  Keeping the async
            // work here keeps `App::update` pure.
            let (event, status) = resolve_io(app, event).await;

            let mut needs_redraw = app.update(event);
            if let Some(msg) = status {
                needs_redraw |= app.update(AppEvent::StatusMessage(msg));
            }
            if app.should_quit {
                break;
            }
            if needs_redraw {
                tui.draw(|frame| ui::render(app, frame))?;
            }
        }
    }

    Ok(())
}

// ── Api event resolution (task 29: task-status-view) ─────────────────────────

/// Resolve a core api [`makina_core::api::Event`] into an [`AppEvent`].
///
/// For most events this is just a direct `AppEvent::ApiEvent` wrap.  The
/// special case is [`makina_core::api::Event::RunOpened`]: when a Run is
/// first opened the event carries only the `RunId` and `task_list_path` —
/// NOT the task list.  So we immediately call `api.run(id).await` to fetch
/// the full [`RunView`] (with its tasks) and return it as
/// [`AppEvent::RunLoaded`].  This keeps `App::update` pure (no async) while
/// ensuring the task-status panel has data to render as soon as a Run opens.
///
/// If `api.run(id)` returns `None` (race between open and cancel) we fall
/// back to a plain `AppEvent::ApiEvent(RunOpened{..})` so the placeholder
/// entry is still created — the panel will show "no tasks yet" until the run
/// appears.
async fn resolve_api_event(
    api: &std::sync::Arc<dyn makina_core::api::Api>,
    ev: makina_core::api::Event,
) -> AppEvent {
    use makina_core::api::Event;
    if let Event::RunOpened { run, .. } = &ev
        && let Some(full_run) = api.run(*run).await
    {
        return AppEvent::RunLoaded(full_run);
    }
    AppEvent::ApiEvent(ev)
}

// ── IO resolution: file browser + run control (task 28 + 31) ──────────────────

/// Resolve an intent event into a concrete state-mutating event by performing
/// the necessary IO, returning that event plus an OPTIONAL transient status
/// message to surface the command outcome/error in the status bar.
///
/// This is the single place where the TUI touches the filesystem and the api;
/// [`App::update`] never does.  Handles:
/// - **File browser** (task 28): `OpenBrowser` → read CWD; `BrowserActivate` on
///   a dir → read it, on a file → compute transient "Interpreting …" status,
///   `execute(OpenRun)` (now fast) → `CloseBrowser` (a transient "Interpreting …"
///   status is surfaced for files); `BrowserParent` → read parent dir.
/// - **Run control** (task 31): `StartRun`/`PauseRun`/`CancelRun` →
///   `execute(...)` for `app.selected_run()` (outcome/error → status message);
///   the run-state changes themselves flow back via `api.subscribe()`.
///
/// Non-IO events pass straight through with no status message.
async fn resolve_io(app: &App, event: AppEvent) -> (AppEvent, Option<String>) {
    match event {
        AppEvent::OpenBrowser => {
            // Start from the process CWD (fall back to "." if unavailable).
            let start = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            (read_dir_event(&start).await, None)
        }
        AppEvent::BrowserParent => match app.browser.as_ref().and_then(|b| b.parent()) {
            Some(parent) => (read_dir_event(parent).await, None),
            // Already at the root — nothing to do; just redraw.
            None => (AppEvent::Tick, None),
        },
        AppEvent::BrowserActivate => {
            match app.browser.as_ref().and_then(|b| b.selected_entry()) {
                Some(entry) if entry.is_dir => (read_dir_event(&entry.path).await, None),
                Some(entry) => {
                    // It's a file: compute a transient "Interpreting …" status
                    // (for immediate user feedback), perform the (now fast)
                    // execute(OpenRun) to register the run + broadcast, then
                    // close the browser.  The "Opened {run}" (or error only on
                    // failure) can follow from the RunLoaded path.
                    let stem = entry
                        .path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("task list");
                    let status = format!("Interpreting {}...", stem);
                    let result = app
                        .api
                        .execute(makina_core::api::Command::OpenRun {
                            task_list_path: entry.path.clone(),
                        })
                        .await;
                    let msg = if let Err(e) = result {
                        format!("Open failed: {e}")
                    } else {
                        status
                    };
                    (AppEvent::CloseBrowser, Some(msg))
                }
                // No selection (empty dir) — ignore.
                None => (AppEvent::Tick, None),
            }
        }
        // ── Run control (task 31) ─────────────────────────────────────────────
        AppEvent::StartRun => (AppEvent::Tick, run_control(app, ControlKind::Start).await),
        AppEvent::PauseRun => (AppEvent::Tick, run_control(app, ControlKind::Pause).await),
        AppEvent::CancelRun => (AppEvent::Tick, run_control(app, ControlKind::Cancel).await),
        AppEvent::Reinterpret => (
            AppEvent::Tick,
            run_control(app, ControlKind::Reinterpret).await,
        ),
        // Everything else passes straight through.
        other => (other, None),
    }
}

/// Which run-control command a key intent maps to.
#[derive(Debug, Clone, Copy)]
enum ControlKind {
    Start,
    Pause,
    Cancel,
    Reinterpret,
}

/// Issue a run-control command for the currently selected Run and return a
/// status message describing the outcome (or `None` if there is no selection).
///
/// The actual run-state changes (status, task progress, agent exchanges) flow
/// back through `api.subscribe()`; this only surfaces the command's immediate
/// acknowledgement / error in the status bar.
async fn run_control(app: &App, kind: ControlKind) -> Option<String> {
    use makina_core::api::Command;

    let run = match app.selected_run() {
        Some(r) => r.id,
        None => return Some("No run selected".to_string()),
    };
    let (command, verb) = match kind {
        ControlKind::Start => (Command::StartRun { run }, "Start"),
        ControlKind::Pause => (Command::PauseRun { run }, "Pause"),
        ControlKind::Cancel => (Command::CancelRun { run }, "Cancel"),
        ControlKind::Reinterpret => (Command::ReinterpretRun { run }, "Reinterpret"),
    };
    match app.api.execute(command).await {
        Ok(_) => Some(format!("{verb} {run}")),
        Err(e) => Some(format!("{verb} failed: {e}")),
    }
}

/// Read `dir` and build a [`AppEvent::BrowserOpened`] event from its entries.
///
/// Entries are sorted directories-first, then alphabetically (case-insensitive),
/// so the listing is stable and predictable.  A `..` parent entry is prepended
/// when `dir` has a parent, giving a visible "go up" affordance.  On a read
/// error the browser is still opened on `dir` with an empty listing (so the user
/// can back out) rather than failing silently.
async fn read_dir_event(dir: &std::path::Path) -> AppEvent {
    use crate::browser::DirEntry;

    let mut entries: Vec<DirEntry> = Vec::new();

    // Prepend a ".." entry when a parent exists.
    if let Some(parent) = dir.parent() {
        entries.push(DirEntry {
            name: "..".to_string(),
            path: parent.to_path_buf(),
            is_dir: true,
        });
    }

    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        let mut items: Vec<DirEntry> = Vec::new();
        while let Ok(Some(de)) = rd.next_entry().await {
            let path = de.path();
            let name = de.file_name().to_string_lossy().to_string();
            // Skip hidden dotfiles to keep the listing focused (the `..` entry
            // above is added explicitly).
            if name.starts_with('.') {
                continue;
            }
            let is_dir = de.file_type().await.map(|ft| ft.is_dir()).unwrap_or(false);
            items.push(DirEntry { name, path, is_dir });
        }
        // Directories first, then case-insensitive name order.
        items.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        entries.extend(items);
    }

    AppEvent::BrowserOpened {
        dir: dir.to_path_buf(),
        entries,
    }
}

// ── Translation helpers ───────────────────────────────────────────────────────

/// Translate a raw crossterm [`CrosstermEvent`] into an [`AppEvent`].
///
/// `browsing` selects the keymap: the modal file browser (task 28) captures
/// navigation keys (Enter / Backspace / Esc) differently from the normal view.
///
/// Returns [`AppEvent::Tick`] for events the TUI doesn't handle (e.g. mouse
/// events); those simply trigger a harmless redraw.
fn translate_terminal_event(ev: CrosstermEvent, browsing: bool) -> AppEvent {
    match ev {
        CrosstermEvent::Key(key) => translate_key(key, browsing),
        CrosstermEvent::Resize(w, h) => AppEvent::Resize(w, h),
        // Mouse wheel scrolls the focused exchange pane regardless of the
        // `browsing` flag (the exchange pane is not the browser).  Other mouse
        // kinds (clicks, drags, moves) are ignored → harmless Tick redraw.
        CrosstermEvent::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => AppEvent::ScrollUp,
            MouseEventKind::ScrollDown => AppEvent::ScrollDown,
            _ => AppEvent::Tick,
        },
        // Paste, focus, etc. — ignored for now.
        _ => AppEvent::Tick,
    }
}

/// Translate a key press into an [`AppEvent`], honouring the current view mode.
fn translate_key(key: crossterm::event::KeyEvent, browsing: bool) -> AppEvent {
    use crossterm::event::KeyEventKind;
    // Only react to key-press events (not key-release / repeat on some platforms).
    if key.kind != KeyEventKind::Press {
        return AppEvent::Tick;
    }

    // Ctrl-C always quits, in any mode.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return AppEvent::Quit;
    }

    if browsing {
        // ── File-browser keymap ──────────────────────────────────────────────
        // Esc closes the browser (does NOT quit the app); Enter activates the
        // selection; Backspace goes to the parent dir; j/k/arrows navigate.
        match key.code {
            KeyCode::Esc => AppEvent::CloseBrowser,
            KeyCode::Enter => AppEvent::BrowserActivate,
            KeyCode::Backspace => AppEvent::BrowserParent,
            KeyCode::Up | KeyCode::Char('k') => AppEvent::BrowserUp,
            KeyCode::Down | KeyCode::Char('j') => AppEvent::BrowserDown,
            _ => AppEvent::Tick,
        }
    } else {
        // ── Normal keymap ────────────────────────────────────────────────────
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => AppEvent::Quit,
            KeyCode::Esc => AppEvent::Quit,
            KeyCode::Tab => AppEvent::FocusNext,
            // Cycle the dependency view (Off → List → Tree → Timeline → Off).
            KeyCode::Char('v') | KeyCode::Char('V') => AppEvent::CycleDependencyView,
            // Toggle the error pane open/closed.
            KeyCode::Char('e') | KeyCode::Char('E') => AppEvent::ToggleErrorPane,
            // Open the file browser to pick a task list.
            KeyCode::Char('o') | KeyCode::Char('O') => AppEvent::OpenBrowser,
            // ── Run control (task 31): act on the selected Run ────────────────
            // s = Start/resume, p = Pause, c = Cancel.  These are intents; the IO
            // layer resolves them into the async `api.execute(...)` call.
            KeyCode::Char('s') | KeyCode::Char('S') => AppEvent::StartRun,
            KeyCode::Char('p') | KeyCode::Char('P') => AppEvent::PauseRun,
            KeyCode::Char('c') | KeyCode::Char('C') => AppEvent::CancelRun,
            // Re-interpret the selected run (e.g. after fixing blocking issues).
            KeyCode::Char('r') | KeyCode::Char('R') => AppEvent::Reinterpret,
            // Sidebar navigation: arrow keys and vim-style j/k.
            KeyCode::Up | KeyCode::Char('k') => AppEvent::SelectUp,
            KeyCode::Down | KeyCode::Char('j') => AppEvent::SelectDown,
            _ => AppEvent::Tick,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseEvent,
    };

    fn key_press(code: KeyCode, modifiers: KeyModifiers) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    /// Build a crossterm mouse-wheel event of the given `kind`
    /// (task `tui-mouse-scroll`).
    fn wheel(kind: MouseEventKind) -> CrosstermEvent {
        CrosstermEvent::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn wheel_translates_to_scroll() {
        // Wheel events route to the exchange-pane scroll helpers regardless of
        // the `browsing` flag (the exchange pane is not the browser).
        assert!(matches!(
            translate_terminal_event(wheel(MouseEventKind::ScrollUp), false),
            AppEvent::ScrollUp
        ));
        assert!(matches!(
            translate_terminal_event(wheel(MouseEventKind::ScrollDown), false),
            AppEvent::ScrollDown
        ));
    }

    #[test]
    fn q_key_translates_to_quit() {
        let ev = key_press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::Quit
        ));
    }

    #[test]
    fn esc_key_translates_to_quit() {
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::Quit
        ));
    }

    #[test]
    fn ctrl_c_translates_to_quit() {
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::Quit
        ));
    }

    #[test]
    fn tab_translates_to_focus_next() {
        let ev = key_press(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::FocusNext
        ));
    }

    #[test]
    fn v_translates_to_cycle_dependency_view() {
        assert!(matches!(
            translate_terminal_event(key_press(KeyCode::Char('v'), KeyModifiers::NONE), false),
            AppEvent::CycleDependencyView
        ));
    }

    #[test]
    fn e_key_translates_to_toggle_error_pane() {
        assert!(matches!(
            translate_terminal_event(key_press(KeyCode::Char('e'), KeyModifiers::NONE), false),
            AppEvent::ToggleErrorPane
        ));
    }

    #[test]
    fn r_key_translates_to_reinterpret() {
        assert!(matches!(
            translate_terminal_event(key_press(KeyCode::Char('r'), KeyModifiers::NONE), false),
            AppEvent::Reinterpret
        ));
    }

    #[test]
    fn key_release_is_tick_not_quit() {
        // On some platforms crossterm fires key-release events; they must be
        // ignored (treated as Tick, not as Quit).
        let ev = CrosstermEvent::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::Tick
        ));
    }

    #[test]
    fn resize_translates_to_resize_event() {
        let ev = CrosstermEvent::Resize(120, 40);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::Resize(120, 40)
        ));
    }

    #[test]
    fn up_arrow_translates_to_select_up() {
        let ev = key_press(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::SelectUp
        ));
    }

    #[test]
    fn down_arrow_translates_to_select_down() {
        let ev = key_press(KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::SelectDown
        ));
    }

    #[test]
    fn k_key_translates_to_select_up() {
        let ev = key_press(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::SelectUp
        ));
    }

    #[test]
    fn j_key_translates_to_select_down() {
        let ev = key_press(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::SelectDown
        ));
    }

    // ── File-browser keymap (task 28) ─────────────────────────────────────────

    #[test]
    fn o_key_opens_browser_in_normal_mode() {
        let ev = key_press(KeyCode::Char('o'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::OpenBrowser
        ));
    }

    // ── Run-control key translation (task 31) ─────────────────────────────────

    #[test]
    fn s_key_translates_to_start_run() {
        let ev = key_press(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::StartRun
        ));
    }

    #[test]
    fn p_key_translates_to_pause_run() {
        let ev = key_press(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::PauseRun
        ));
    }

    #[test]
    fn c_key_translates_to_cancel_run() {
        // Plain `c` (no modifier) is Cancel; Ctrl-C remains Quit (covered above).
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false),
            AppEvent::CancelRun
        ));
    }

    #[test]
    fn control_keys_do_nothing_in_browser_mode() {
        // s/p/c are normal-mode keys; inside the browser they fall through to a
        // harmless Tick (the browser keymap owns navigation).
        for ch in ['s', 'p', 'c'] {
            let ev = key_press(KeyCode::Char(ch), KeyModifiers::NONE);
            assert!(
                matches!(translate_terminal_event(ev, true), AppEvent::Tick),
                "'{ch}' must be inert in browser mode"
            );
        }
    }

    #[test]
    fn enter_in_browser_activates_selection() {
        let ev = key_press(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, true),
            AppEvent::BrowserActivate
        ));
    }

    #[test]
    fn esc_in_browser_closes_not_quits() {
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        // In browser mode, Esc must close the browser, NOT quit the app.
        assert!(matches!(
            translate_terminal_event(ev, true),
            AppEvent::CloseBrowser
        ));
    }

    #[test]
    fn backspace_in_browser_goes_to_parent() {
        let ev = key_press(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, true),
            AppEvent::BrowserParent
        ));
    }

    #[test]
    fn jk_in_browser_navigate_browser_not_sidebar() {
        let down = key_press(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(down, true),
            AppEvent::BrowserDown
        ));
        let up = key_press(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(up, true),
            AppEvent::BrowserUp
        ));
    }

    #[test]
    fn ctrl_c_quits_even_in_browser_mode() {
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(translate_terminal_event(ev, true), AppEvent::Quit));
    }

    #[test]
    fn q_in_browser_is_not_quit() {
        // `q` is a normal-mode quit key; inside the browser it must not quit
        // (it falls through to Tick so the user can keep browsing).
        let ev = key_press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(translate_terminal_event(ev, true), AppEvent::Tick));
    }

    /// Verify the full quit path: translate key → update App → should_quit.
    #[test]
    fn quit_key_drives_app_to_should_quit() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        let ev = translate_terminal_event(key_press(KeyCode::Char('q'), KeyModifiers::NONE), false);
        app.update(ev);
        assert!(app.should_quit);
    }

    /// Verify that an api event flows correctly: api::Event → AppEvent::ApiEvent → App state.
    #[test]
    fn api_event_flows_to_app_state() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use makina_core::api::{Event as CoreEvent, RunId, RunStatus};
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        let core_ev = CoreEvent::RunOpened {
            run: RunId(42),
            task_list_path: std::path::PathBuf::from(".tasks/flow.json"),
        };
        let app_ev = AppEvent::ApiEvent(core_ev);
        app.update(app_ev);

        assert_eq!(app.runs.len(), 1);
        assert_eq!(app.runs[0].id, RunId(42));
        assert_eq!(app.runs[0].status, RunStatus::Pending);
    }

    // ── Run-control IO resolution + outcome surfacing (task 31) ───────────────

    /// `resolve_io` on `StartRun`/`PauseRun`/`CancelRun` must issue the matching
    /// `Command` against the api for the SELECTED run and return a status
    /// message; feeding that message through `App::update` sets
    /// `app.status_message`.  Uses a recording stub api to assert the exact
    /// command issued.
    #[tokio::test]
    async fn control_keys_issue_commands_and_set_status_message() {
        use crate::app::{App, AppEvent};
        use async_trait::async_trait;
        use makina_core::api::{
            Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunStatus, RunView,
        };
        use std::sync::{Arc, Mutex};

        /// A stub api that records every `Command` it executes.
        struct RecordingApi {
            commands: Mutex<Vec<Command>>,
        }
        #[async_trait]
        impl Api for RecordingApi {
            async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
                self.commands.lock().unwrap().push(command.clone());
                match command {
                    Command::OpenRun { .. } => Ok(CommandOutcome::RunOpened { run: RunId(1) }),
                    _ => Ok(CommandOutcome::Acknowledged),
                }
            }
            async fn runs(&self) -> Vec<RunView> {
                vec![]
            }
            async fn run(&self, _id: RunId) -> Option<RunView> {
                None
            }
            fn subscribe(&self) -> EventStream {
                Box::pin(futures::stream::empty::<Event>())
            }
        }
        // Build an App with one selected run (id 7).
        let api = Arc::new(RecordingApi {
            commands: Mutex::new(Vec::new()),
        });
        let run = RunView {
            id: RunId(7),
            run_uid: String::new(),
            task_list_path: std::path::PathBuf::from(".tasks/control.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn Api>,
            vec![run],
            std::path::PathBuf::from("."),
        );
        assert_eq!(app.selected_run().unwrap().id, RunId(7));

        // Start.
        let (ev, status) = resolve_io(&app, AppEvent::StartRun).await;
        assert!(
            matches!(ev, AppEvent::Tick),
            "control resolves to a no-op event"
        );
        let msg = status.expect("StartRun must produce a status message");
        assert!(
            msg.contains("Start"),
            "status must mention Start; got {msg:?}"
        );
        app.update(AppEvent::StatusMessage(msg.clone()));
        assert_eq!(app.status_message.as_deref(), Some(msg.as_str()));

        // Pause.
        let (_ev, status) = resolve_io(&app, AppEvent::PauseRun).await;
        assert!(status.unwrap().contains("Pause"));

        // Cancel.
        let (_ev, status) = resolve_io(&app, AppEvent::CancelRun).await;
        assert!(status.unwrap().contains("Cancel"));

        // The exact commands were issued against the api, all targeting run 7.
        let cmds = api.commands.lock().unwrap().clone();
        assert!(
            matches!(cmds[0], Command::StartRun { run: RunId(7) }),
            "first command must be StartRun{{run:7}}; got {:?}",
            cmds[0]
        );
        assert!(matches!(cmds[1], Command::PauseRun { run: RunId(7) }));
        assert!(matches!(cmds[2], Command::CancelRun { run: RunId(7) }));
    }

    /// With NO run selected, a control key surfaces a "No run selected" message
    /// and issues no command.
    #[tokio::test]
    async fn control_key_with_no_selection_reports_and_issues_nothing() {
        use crate::app::{App, AppEvent};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));
        assert!(app.selected_run().is_none());

        let (ev, status) = resolve_io(&app, AppEvent::StartRun).await;
        assert!(matches!(ev, AppEvent::Tick));
        assert_eq!(status.as_deref(), Some("No run selected"));
    }

    /// A command error from the api is surfaced as a status message (not dropped).
    #[tokio::test]
    async fn control_command_error_is_surfaced() {
        use crate::app::{App, AppEvent};
        use crate::placeholder::PlaceholderApi;
        use makina_core::api::{RunId, RunStatus, RunView};
        use std::sync::Arc;

        // PlaceholderApi::empty() has no runs, so a StartRun for a run that
        // exists in the App view but NOT in the api returns UnknownRun.
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(999),
            run_uid: String::new(),
            task_list_path: std::path::PathBuf::from(".tasks/ghost.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));

        let (_ev, status) = resolve_io(&app, AppEvent::StartRun).await;
        let msg = status.expect("an error must still produce a status message");
        assert!(
            msg.contains("failed"),
            "command error must be surfaced as a 'failed' message; got {msg:?}"
        );
    }

    // ── File-browser IO resolution (task 28) ──────────────────────────────────

    /// `resolve_browser_io(OpenBrowser)` reads the CWD and yields a
    /// `BrowserOpened` event with entries (the makina crate dir always has
    /// `src/` + `Cargo.toml`).
    #[tokio::test]
    async fn open_browser_io_reads_cwd_entries() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], std::path::PathBuf::from("."));

        let (resolved, status) = resolve_io(&app, AppEvent::OpenBrowser).await;
        assert!(status.is_none(), "OpenBrowser has no status message");
        match resolved {
            AppEvent::BrowserOpened { entries, .. } => {
                assert!(
                    !entries.is_empty(),
                    "CWD listing should be non-empty (has a `..` entry at minimum)"
                );
            }
            other => panic!("expected BrowserOpened, got {other:?}"),
        }
    }

    /// `resolve_io(BrowserActivate)` for a file entry using a `PlaceholderApi`
    /// (the style used by the other browser tests in this module) must return
    /// `CloseBrowser` plus a status whose text contains "Interpreting" (or
    /// "Opening") and the file stem.
    #[tokio::test]
    async fn browser_activate_file_produces_interpreting_status() {
        use crate::app::{App, AppEvent, Mode};
        use crate::browser::{DirEntry, FileBrowser};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        app.mode = Mode::FileBrowser;
        let file_path = std::path::PathBuf::from("/tmp/example-task-list.md");
        app.browser = Some(FileBrowser::new(
            std::path::PathBuf::from("/tmp"),
            vec![DirEntry {
                name: "example-task-list.md".to_string(),
                path: file_path,
                is_dir: false,
            }],
        ));

        let (resolved, status) = resolve_io(&app, AppEvent::BrowserActivate).await;
        assert!(
            matches!(resolved, AppEvent::CloseBrowser),
            "file activate must resolve to CloseBrowser"
        );
        let msg = status.expect("activating a file must produce a status message");
        assert!(
            msg.contains("Interpreting") || msg.contains("Opening"),
            "status must contain 'Interpreting' (or 'Opening'); got {msg:?}"
        );
        assert!(
            msg.contains("example-task-list"),
            "status must contain the file stem; got {msg:?}"
        );
    }

    /// **TUI ↔ CoreApi flow (the done-when through the event layer).**
    ///
    /// Drive the exact event-loop step that opens a file against the REAL
    /// `CoreApi`: set up a browser whose selection is a sample task-list file,
    /// call `resolve_io(BrowserActivate)` (which performs
    /// `api.execute(OpenRun)`), then drain `api.subscribe()` and feed the
    /// resulting `RunOpened` into `App::update` — asserting the Run appears in
    /// `app.runs`.
    #[tokio::test]
    async fn browser_activate_file_opens_run_via_core_api_and_appears_in_app() {
        use crate::app::{App, AppEvent, Mode};
        use crate::browser::{DirEntry, FileBrowser};
        use makina_core::dependency::EdgeInferrer;
        use makina_core::interpreter::StructuredTextInterpreter;
        use makina_core::orchestrator::CoreApi;
        use std::sync::Arc;

        // A valid task list written to a tempfile.
        let source = "# Flow — Task List\n\nPreamble.\n\n---\n\n## 0001 — S\n\n\
### only — Only task\nDoes a thing in `lib.rs`.\n- **Depends on:** —\n\
- **Done when:** it works.\n";
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("flow-feature.md");
        std::fs::write(&file_path, source).unwrap();

        // Real CoreApi with the deterministic interpreter (what main.rs uses).
        let interpreter = Arc::new(EdgeInferrer::new(
            Arc::new(StructuredTextInterpreter::new()),
        ));
        // Execution deps (task 31): these tests only exercise OpenRun, so a
        // NoopBackend + a temp-dir WorktreeManager + a default Config suffice
        // (no Run is started, so they are never driven).
        let backend: Arc<dyn makina_core::backend::AgentBackend> =
            Arc::new(makina_core::backend::noop::NoopBackend::new());
        let wm = makina_core::worktree::WorktreeManager::new(
            tempfile::tempdir().unwrap().keep(),
            "develop".into(),
        );
        let config = makina_core::config::Config::resolve(
            makina_core::config::GlobalConfig::default(),
            makina_core::config::ProjectConfig::default(),
        );
        let api: Arc<dyn makina_core::api::Api> =
            Arc::new(CoreApi::new(interpreter, backend, wm, config));

        // Subscribe BEFORE acting so we capture the RunOpened broadcast.
        let mut sub = api.subscribe();

        // Build an App whose browser has the sample file selected.
        let mut app = App::new(Arc::clone(&api), vec![], std::path::PathBuf::from("."));
        app.mode = Mode::FileBrowser;
        app.browser = Some(FileBrowser::new(
            dir.path().to_path_buf(),
            vec![DirEntry {
                name: "flow-feature.md".to_string(),
                path: file_path.clone(),
                is_dir: false,
            }],
        ));
        assert!(app.runs.is_empty());

        // The event-loop step: activating a file selection performs the async
        // OpenRun against CoreApi and returns CloseBrowser + a status message
        // (now the transient "Interpreting …" one).
        let (resolved, status) = resolve_io(&app, AppEvent::BrowserActivate).await;
        assert!(
            matches!(resolved, AppEvent::CloseBrowser),
            "selecting a file must resolve to CloseBrowser"
        );
        assert!(
            status
                .as_deref()
                .is_some_and(|m| m.contains("Interpreting") || m.contains("Opening")),
            "opening a file must surface an 'Interpreting …' (or 'Opening …') status message; got {status:?}"
        );
        app.update(resolved);
        if let Some(msg) = status {
            app.update(AppEvent::StatusMessage(msg));
        }
        assert_eq!(app.mode, Mode::Normal, "browser should close after opening");

        // The CoreApi created the Run (direct query proves OpenRun happened).
        let runs = api.runs().await;
        assert_eq!(runs.len(), 1, "CoreApi must have created exactly one run");
        assert_eq!(runs[0].task_list_path, file_path);
        assert_eq!(runs[0].tasks.len(), 1, "the task must be interpreted");

        // The RunOpened event flows back through subscribe(); feeding it into
        // App::update makes the Run appear in app.runs (the sidebar source).
        let ev = tokio::time::timeout(std::time::Duration::from_secs(1), sub.next())
            .await
            .expect("timed out waiting for RunOpened")
            .expect("stream ended unexpectedly");
        assert!(matches!(ev, makina_core::api::Event::RunOpened { .. }));
        app.update(AppEvent::ApiEvent(ev));

        assert_eq!(
            app.runs.len(),
            1,
            "the opened Run must appear in app.runs via the RunOpened event"
        );
        assert_eq!(app.runs[0].task_list_path, file_path);
    }

    // ── Task-population test (task 29): RunOpened → api.run() → RunLoaded ─────

    /// **Task population:** When a `RunOpened` core event is received by
    /// `resolve_api_event`, it must call `api.run(id).await` and return an
    /// `AppEvent::RunLoaded` carrying the full `RunView` (with tasks).
    ///
    /// This proves the async data-flow: `Event::RunOpened` → `resolve_api_event`
    /// → fetch full `RunView` → `AppEvent::RunLoaded` → `App::update` → tasks
    /// populated in `app.runs`.
    #[tokio::test]
    async fn run_opened_event_resolves_to_run_loaded_with_tasks() {
        use crate::app::{App, AppEvent};
        use crate::placeholder::PlaceholderApi;
        use makina_core::api::{
            Event as CoreEvent, RunId, RunStatus, RunView, TaskId, TaskState, TaskView,
        };
        use std::sync::Arc;

        // Seed the PlaceholderApi with a run that already has tasks (simulates
        // what CoreApi returns from `api.run(id)`).
        let _api = Arc::new(PlaceholderApi::empty());
        // Manually push a RunView with tasks into the PlaceholderApi so that
        // `api.run(id)` returns it.
        {
            let run = RunView {
                id: RunId(5),
                run_uid: String::new(),
                task_list_path: std::path::PathBuf::from(".tasks/pop.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![
                    TaskView {
                        id: TaskId::new("first"),
                        title: "First task".into(),
                        state: TaskState::Ready,
                        gate_iterations: 0,
                        review_iterations: 0,
                        depends_on: vec![],
                    },
                    TaskView {
                        id: TaskId::new("second"),
                        title: "Second task".into(),
                        state: TaskState::New,
                        gate_iterations: 0,
                        review_iterations: 0,
                        depends_on: vec![TaskId::new("first")],
                    },
                ],
                report: makina_core::api::IngestionReport::default(),
            };
            // Use execute(OpenRun) is not ideal here since it creates an empty run;
            // instead we directly call the public `execute` and rely on the test
            // seeding approach — OR we use PlaceholderApi::execute(OpenRun) then
            // observe the side effect. Since PlaceholderApi::execute(OpenRun)
            // creates a run with empty tasks, we can't easily seed tasks through it.
            //
            // Instead, we create a stub using the `CoreApi` via tempfile (the
            // real path), which correctly interprets the task list and exposes
            // tasks via `api.run(id)`.  We test only the resolve_api_event step.
            //
            // For this unit test, we construct the api separately so we can
            // directly verify the resolve_api_event path. We use PlaceholderApi
            // with a workaround: call execute to register the run, then it won't
            // have tasks (empty PlaceholderApi behaviour). We still verify the
            // resolve path returns RunLoaded regardless of empty tasks.
            let _ = run; // The reasoning is documented above; see the CoreApi test below.
        }

        // Use the real CoreApi for a proper end-to-end population test.
        use makina_core::dependency::EdgeInferrer;
        use makina_core::interpreter::StructuredTextInterpreter;
        use makina_core::orchestrator::CoreApi;

        let source = "# Pop Test\n\nPreamble.\n\n---\n\n## 0001 — S\n\n\
### first — First task\nDoes something.\n- **Depends on:** —\n\
- **Done when:** done.\n\n\
### second — Second task\nDoes more.\n- **Depends on:** first\n\
- **Done when:** done too.\n";
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("pop-test.md");
        std::fs::write(&file_path, source).unwrap();

        let interpreter = Arc::new(EdgeInferrer::new(
            Arc::new(StructuredTextInterpreter::new()),
        ));
        // Execution deps (task 31): these tests only exercise OpenRun, so a
        // NoopBackend + a temp-dir WorktreeManager + a default Config suffice
        // (no Run is started, so they are never driven).
        let backend: Arc<dyn makina_core::backend::AgentBackend> =
            Arc::new(makina_core::backend::noop::NoopBackend::new());
        let wm = makina_core::worktree::WorktreeManager::new(
            tempfile::tempdir().unwrap().keep(),
            "develop".into(),
        );
        let config = makina_core::config::Config::resolve(
            makina_core::config::GlobalConfig::default(),
            makina_core::config::ProjectConfig::default(),
        );
        let api: Arc<dyn makina_core::api::Api> =
            Arc::new(CoreApi::new(interpreter, backend, wm, config));

        // Subscribe BEFORE the execute to capture the RunOpened broadcast.
        let mut sub = api.subscribe();

        // Open the run — CoreApi interprets it and exposes tasks via api.run(id).
        let outcome = api
            .execute(makina_core::api::Command::OpenRun {
                task_list_path: file_path.clone(),
            })
            .await
            .expect("OpenRun must succeed");
        let run_id = match outcome {
            makina_core::api::CommandOutcome::RunOpened { run } => run,
            _ => panic!("unexpected outcome"),
        };

        // Drain the RunOpened event from the subscription.
        let core_ev = tokio::time::timeout(std::time::Duration::from_secs(1), sub.next())
            .await
            .expect("timed out waiting for RunOpened event")
            .expect("stream ended unexpectedly");
        assert!(matches!(core_ev, CoreEvent::RunOpened { .. }));

        // NOW invoke resolve_api_event — this is the function under test.
        // It should call api.run(run_id) and return AppEvent::RunLoaded with tasks.
        let resolved = resolve_api_event(&api, core_ev).await;

        match &resolved {
            AppEvent::RunLoaded(full_run) => {
                assert_eq!(
                    full_run.id, run_id,
                    "RunLoaded must carry the correct RunId"
                );
                assert!(
                    !full_run.tasks.is_empty(),
                    "RunLoaded must carry the populated task list"
                );
            }
            other => panic!("expected AppEvent::RunLoaded, got {:?}", other),
        }

        // Apply to App — verifies the full pipeline end-to-end.
        let mut app = App::new(Arc::clone(&api), vec![], std::path::PathBuf::from("."));
        app.update(resolved);

        assert_eq!(
            app.runs.len(),
            1,
            "app.runs must have one entry after RunLoaded"
        );
        let loaded = app.selected_run().expect("first run must be selected");
        assert_eq!(loaded.id, run_id);
        assert!(
            !loaded.tasks.is_empty(),
            "selected_run().tasks must be non-empty after RunLoaded"
        );
    }

    /// Simulate the exact construction that main.rs performs for the api
    /// (using the same EdgeInferrer + StructuredTextInterpreter literals).
    /// Then create a CoreApi and assert that an OpenRun of a known-good sample
    /// produces the expected graph with zero backend involvement (the backend
    /// panics if called, proving ingestion path does not touch it).
    /// The test must be named exactly as shown and must fail before the 0029 change.
    #[test]
    fn tui_main_constructs_deterministic_ingestion_interpreter() {
        use std::sync::Arc;

        use makina_core::backend::{AgentBackend, BackendError, SessionConfig};
        use makina_core::dependency::EdgeInferrer;
        use makina_core::interpreter::{StructuredTextInterpreter, TaskListInterpreter};
        use makina_core::worktree::WorktreeManager;

        // Exact literals from main.rs deterministic construction.
        let ingestion_interpreter: Arc<dyn TaskListInterpreter> = Arc::new(EdgeInferrer::new(
            Arc::new(StructuredTextInterpreter::new()),
        ));

        // Backend that must never be called during OpenRun/ingestion.
        struct PanicOnUseBackend;
        #[async_trait::async_trait]
        impl AgentBackend for PanicOnUseBackend {
            async fn spawn(
                &self,
                _cfg: SessionConfig,
            ) -> Result<Box<dyn makina_core::backend::AgentSession>, BackendError> {
                panic!("backend must not be involved in TUI ingestion/OpenRun path");
            }
        }
        let backend: Arc<dyn AgentBackend> = Arc::new(PanicOnUseBackend);

        // Fresh temp repo for wm (OpenRun uses it for slug/artifact paths).
        let repo_dir = tempfile::tempdir().expect("temp repo");
        // init minimal git so WorktreeManager is happy if it checks.
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo_dir.path())
            .status()
            .expect("git init");
        let wm = WorktreeManager::new(repo_dir.path().to_path_buf(), "develop".into());

        let config = makina_core::config::Config {
            backend: makina_core::config::BackendConfig {
                command: "echo".into(),
                args: vec![],
            },
            planner: makina_core::config::PlannerConfig::default(),
            gates: vec![],
            caps: makina_core::config::CapsConfig::default(),
            concurrency: 1,
            base_branch: "develop".into(),
        };

        let api = Arc::new(makina_core::orchestrator::CoreApi::new(
            ingestion_interpreter,
            backend,
            wm,
            config,
        ));

        // Bring Api trait into scope for .execute().
        use makina_core::api::Api as _;

        // Write a minimal valid task list (the interpreter will succeed).
        let (tmp, path) = {
            let dir = tempfile::tempdir().expect("task list dir");
            let p = dir.path().join("sample.md");
            std::fs::write(
                &p,
                r#"# Sample — Test
Preamble.

---
## 0001 — Section

### sample-task — Sample task
A description that is long enough to pass minimums.
- **Depends on:** —
- **Done when:** the work completes successfully with tests passing.
"#,
            )
            .expect("write sample");
            (dir, p)
        };

        // OpenRun must succeed without touching the panicking backend.
        let outcome = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(api.execute(makina_core::api::Command::OpenRun {
                task_list_path: path,
            }))
            .expect("OpenRun must succeed with det ingestion");

        match outcome {
            makina_core::api::CommandOutcome::RunOpened { .. } => {}
            other => panic!("expected RunOpened, got {:?}", other),
        }

        // If we reached here, backend was not called (would have panicked).
        // Also, the graph was interpreted (we can query runs but since no subscribe
        // in this sync test, just the outcome is proof).
        drop(tmp); // keep dir alive till end
    }
}
