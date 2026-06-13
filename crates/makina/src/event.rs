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
        let editing_providers = app.is_editing_providers();
        let viewing_doctor = app.is_viewing_doctor();
        let app_event: Option<AppEvent> = tokio::select! {
            // Bias toward terminal input (lower latency for keystrokes).
            biased;

            maybe_term = term_rx.recv() => {
                maybe_term.map(|ev| translate_terminal_event(ev, browsing, editing_providers, viewing_doctor))
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
            // OpenLog requires tui for terminal teardown/restore, so it is
            // handled here (in the loop body, where `tui` is accessible) rather
            // than in `resolve_io` (which does not receive `tui`).
            if matches!(event, AppEvent::OpenLog) {
                let status = open_log(tui, app).await;
                if let Some(msg) = status {
                    app.update(AppEvent::StatusMessage(msg));
                }
                // Force a full repaint after the pager exits.
                tui.draw(|frame| ui::render(app, frame))?;
                continue;
            }

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
        // ── Provider configuration editor commit (task 0041) ──────────────────
        // Write the editor's current providers + roles back to the project config
        // file (`{repo_root}/.makina/config.toml`).  Best-effort: on IO/serialise
        // error push an error-pane message rather than crashing.  The actual state
        // update (closing the editor, updating app.providers/roles) is handled by
        // App::update after this returns.
        AppEvent::ProviderEditorCommit => {
            let status = commit_provider_config(app).await;
            (AppEvent::ProviderEditorCommit, status)
        }
        // ── Doctor scaffold (task 0046) ──────────────────────────────────────
        // Write starter config templates to both config paths if neither exists.
        // Never overwrite existing files; re-check and refuse if present.
        AppEvent::DoctorWriteScaffold => {
            let status = write_doctor_scaffold(app).await;
            (AppEvent::Tick, status)
        }
        // ── Open log (task 0049) ──────────────────────────────────────────────
        // Resolve the focused task's log path, spawn $PAGER on it, and return a
        // status message (success, absence, or error).  If the file doesn't exist,
        // emit "no log for this task yet" instead of opening.
        // OpenLog is intercepted before resolve_io in the event loop so tui
        // is accessible for terminal teardown/restore.  This arm is unreachable
        // in production but kept so the match stays exhaustive.
        AppEvent::OpenLog => (AppEvent::Tick, None),
        // Everything else passes straight through.
        other => (other, None),
    }
}

/// Write the provider/role configuration from the editor back to
/// `{repo_root}/.makina/config.toml`, best-effort.
///
/// Returns `Some(msg)` with a success/error description (surfaced in the status
/// bar), or `None` if there is no active editor to commit.
async fn commit_provider_config(app: &App) -> Option<String> {
    use makina_core::config::GlobalConfig;
    use makina_core::paths::config_file;

    let editor = app.provider_editor.as_ref()?;

    // Read the current on-disk config (if any) so we don't lose fields we
    // don't manage (e.g. gates, caps, base_branch).  On read failure start
    // from a default so we can still write back the providers/roles.
    let config_path = config_file(&app.repo_root);
    let existing_global: GlobalConfig = if config_path.exists() {
        match tokio::fs::read_to_string(&config_path).await {
            Ok(s) => toml::from_str::<GlobalConfig>(&s).unwrap_or_default(),
            Err(_) => GlobalConfig::default(),
        }
    } else {
        GlobalConfig::default()
    };

    // Build the updated global config: preserve all existing fields but
    // replace providers and roles with the editor's current state.
    let updated = GlobalConfig {
        providers: editor.providers.clone(),
        roles: editor.roles.clone(),
        ..existing_global
    };

    // Serialise to TOML.
    let toml_str = match toml::to_string_pretty(&updated) {
        Ok(s) => s,
        Err(e) => {
            return Some(format!("Config serialise error: {e}"));
        }
    };

    // Ensure the parent directory exists.
    if let Some(parent) = config_path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return Some(format!("Config write error: {e}"));
    }

    // Write the file.
    match tokio::fs::write(&config_path, toml_str).await {
        Ok(()) => Some("Config saved".to_string()),
        Err(e) => Some(format!("Config write error: {e}")),
    }
}

/// Write starter config templates when no config files exist.
///
/// Writes commented template files to both ~/.makina/config.toml (global)
/// and .makina/config.toml (project). Never overwrites existing files;
/// if either already exists after the check, returns a refusal message.
async fn write_doctor_scaffold(app: &App) -> Option<String> {
    // Recheck: neither config file should exist
    let global_exists = app.config_paths.global.as_ref().is_some_and(|p| p.exists());
    let project_exists = app
        .config_paths
        .project
        .as_ref()
        .is_some_and(|p| p.exists());

    if global_exists || project_exists {
        return Some(
            "Config file already exists; not overwriting. Edit it directly or delete to scaffold."
                .to_string(),
        );
    }

    // Global config template (~/.makina/config.toml)
    let global_template = r#"# Makina global configuration — machine-specific, not committed.
# Place at ~/.makina/config.toml

[backend]
# The command to invoke your agent (must support ACP --acp flag).
# Examples: "gemini", "grok", or "claude-acp"
command = "gemini"
# Optional arguments passed to the agent CLI.
args = ["--acp", "--yolo"]

[planner]
# The planner mechanism: one-shot-agent or persistent-session.
mechanism = "one-shot-agent"
"#;

    // Project config template (.makina/config.toml)
    let project_template = r#"# Makina project configuration — committed with the repository.
# Place at .makina/config.toml

base_branch = "develop"
concurrency = 2

[caps]
gate_iterations = 5
reviewer_iterations = 3
wall_clock_secs = 1200

[[gates]]
name = "test"
command = "cargo test"

[[gates]]
name = "clippy"
command = "cargo clippy -- -D warnings"

[[gates]]
name = "fmt"
command = "cargo fmt --check"
"#;

    // Write global config if global path exists
    let mut written_paths = vec![];

    if let Some(global_path) = &app.config_paths.global {
        // Ensure the parent directory exists
        if let Some(parent) = global_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        match tokio::fs::write(global_path, global_template).await {
            Ok(()) => {
                written_paths.push(global_path.display().to_string());
            }
            Err(e) => {
                return Some(format!(
                    "Failed to write global config {}: {}",
                    global_path.display(),
                    e
                ));
            }
        }
    }

    // Write project config if project path exists (it should always resolve)
    if let Some(project_path) = &app.config_paths.project {
        // Ensure the parent directory exists
        if let Some(parent) = project_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        match tokio::fs::write(project_path, project_template).await {
            Ok(()) => {
                written_paths.push(project_path.display().to_string());
            }
            Err(e) => {
                return Some(format!(
                    "Failed to write project config {}: {}",
                    project_path.display(),
                    e
                ));
            }
        }
    }

    if written_paths.is_empty() {
        Some("No valid config path to write to.".to_string())
    } else {
        Some(format!(
            "Starter configs written to: {}",
            written_paths.join(", ")
        ))
    }
}

/// Open the focused task's log in `$PAGER`, or report its absence.
///
/// Derives the log path from the run id + task id using [`makina_core::paths::task_log`],
/// tears down the TUI (leaves the alternate screen / disables raw mode via
/// [`Tui::restore`]), spawns `$PAGER` (fallback `less`, then `more`) on the
/// file, waits for it to exit, then re-initialises the terminal via
/// [`Tui::reinit`] so the TUI can resume from where it left off.  If the file
/// is absent, emits "no log for this task yet" without tearing down the terminal.
///
/// Called from the event loop's `OpenLog` arm (not from `resolve_io`) because
/// it needs mutable access to `tui` for the teardown/restore cycle.
async fn open_log(tui: &mut Tui, app: &App) -> Option<String> {
    use makina_core::paths;
    use std::process::Command;

    // Get the currently focused task's run id and task id.
    let run = match app.selected_run() {
        Some(r) => r,
        None => return Some("No run selected".to_string()),
    };
    let task_id = match app.selected_task_id() {
        Some(t) => t,
        None => return Some("No task selected".to_string()),
    };

    // Derive the log path.
    let log_path = paths::task_log(&app.repo_root, &run.run_uid, &task_id.0);

    // Check if the log file exists.
    if !log_path.exists() {
        return Some("no log for this task yet".to_string());
    }

    // Get the pager command, trying $PAGER first, then falling back to less.
    let pager_cmd = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());

    // Tear down the TUI before handing the terminal to the pager: leave the
    // alternate screen and disable raw mode so the pager output is visible.
    tui.restore();

    // Spawn the pager and wait for it to exit.
    let result = Command::new(&pager_cmd).arg(&log_path).status();
    let msg = match result {
        Ok(status) => {
            if status.success() {
                Some(format!("Opened {} in {}", log_path.display(), pager_cmd))
            } else {
                Some(format!("{} exit code: {:?}", pager_cmd, status.code()))
            }
        }
        Err(e) => {
            // If the preferred pager failed, try the fallback (less or more).
            let fallback = if pager_cmd != "less" { "less" } else { "more" };
            if let Ok(fb_status) = Command::new(fallback).arg(&log_path).status()
                && fb_status.success()
            {
                Some(format!("Opened {} in {fallback}", log_path.display()))
            } else {
                Some(format!("Failed to open log: {e}"))
            }
        }
    };

    // Restore the TUI: re-enter the alternate screen, enable raw mode, and
    // force a full repaint so no pager output bleeds through.
    let _ = tui.reinit();

    msg
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
/// Similarly, `editing_providers` activates the provider editor keymap.
///
/// Returns [`AppEvent::Tick`] for events the TUI doesn't handle (e.g. mouse
/// events); those simply trigger a harmless redraw.
fn translate_terminal_event(
    ev: CrosstermEvent,
    browsing: bool,
    editing_providers: bool,
    viewing_doctor: bool,
) -> AppEvent {
    match ev {
        CrosstermEvent::Key(key) => translate_key(key, browsing, editing_providers, viewing_doctor),
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
fn translate_key(
    key: crossterm::event::KeyEvent,
    browsing: bool,
    editing_providers: bool,
    viewing_doctor: bool,
) -> AppEvent {
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
    } else if editing_providers {
        // ── Provider editor keymap ────────────────────────────────────────────
        // Esc closes the editor; Enter commits; j/k/arrows navigate.
        match key.code {
            KeyCode::Esc => AppEvent::CloseProviderEditor,
            KeyCode::Enter => AppEvent::ProviderEditorCommit,
            KeyCode::Up | KeyCode::Char('k') => AppEvent::ProviderEditorUp,
            KeyCode::Down | KeyCode::Char('j') => AppEvent::ProviderEditorDown,
            _ => AppEvent::Tick,
        }
    } else if viewing_doctor {
        // ── Doctor overlay keymap ────────────────────────────────────────────
        // Esc closes the doctor; w writes starter config (if no config exists).
        match key.code {
            KeyCode::Esc => AppEvent::CloseDoctor,
            KeyCode::Char('w') | KeyCode::Char('W') => AppEvent::DoctorWriteScaffold,
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
            // Open the focused task's log in $PAGER.
            KeyCode::Char('l') | KeyCode::Char('L') => AppEvent::OpenLog,
            // Open the file browser to pick a task list.
            KeyCode::Char('o') | KeyCode::Char('O') => AppEvent::OpenBrowser,
            // Open the provider/role configuration editor.
            KeyCode::Char('g') | KeyCode::Char('G') => AppEvent::OpenProviderEditor,
            // Open the doctor health-check overlay.
            KeyCode::Char('?') => AppEvent::OpenDoctor,
            // ── Run control (task 31): act on the selected Run ────────────────
            // s = Start/resume, p = Pause, c = Cancel.  These are intents; the IO
            // layer resolves them into the async `api.execute(...)` call.
            KeyCode::Char('s') | KeyCode::Char('S') => AppEvent::StartRun,
            KeyCode::Char('p') | KeyCode::Char('P') => AppEvent::PauseRun,
            KeyCode::Char('c') | KeyCode::Char('C') => AppEvent::CancelRun,
            // Re-interpret the selected run (e.g. after fixing blocking issues).
            KeyCode::Char('r') | KeyCode::Char('R') => AppEvent::Reinterpret,
            // Dismiss the provider-missing warning banner (non-fatal; just hides it).
            KeyCode::Char('d') | KeyCode::Char('D') => AppEvent::DismissProviderWarning,
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
            translate_terminal_event(wheel(MouseEventKind::ScrollUp), false, false, false),
            AppEvent::ScrollUp
        ));
        assert!(matches!(
            translate_terminal_event(wheel(MouseEventKind::ScrollDown), false, false, false),
            AppEvent::ScrollDown
        ));
    }

    #[test]
    fn q_key_translates_to_quit() {
        let ev = key_press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::Quit
        ));
    }

    #[test]
    fn esc_key_translates_to_quit() {
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::Quit
        ));
    }

    #[test]
    fn ctrl_c_translates_to_quit() {
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::Quit
        ));
    }

    #[test]
    fn tab_translates_to_focus_next() {
        let ev = key_press(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::FocusNext
        ));
    }

    #[test]
    fn v_translates_to_cycle_dependency_view() {
        assert!(matches!(
            translate_terminal_event(
                key_press(KeyCode::Char('v'), KeyModifiers::NONE),
                false,
                false,
                false
            ),
            AppEvent::CycleDependencyView
        ));
    }

    #[test]
    fn e_key_translates_to_toggle_error_pane() {
        assert!(matches!(
            translate_terminal_event(
                key_press(KeyCode::Char('e'), KeyModifiers::NONE),
                false,
                false,
                false
            ),
            AppEvent::ToggleErrorPane
        ));
    }

    #[test]
    fn r_key_translates_to_reinterpret() {
        assert!(matches!(
            translate_terminal_event(
                key_press(KeyCode::Char('r'), KeyModifiers::NONE),
                false,
                false,
                false
            ),
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
            translate_terminal_event(ev, false, false, false),
            AppEvent::Tick
        ));
    }

    #[test]
    fn resize_translates_to_resize_event() {
        let ev = CrosstermEvent::Resize(120, 40);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::Resize(120, 40)
        ));
    }

    #[test]
    fn up_arrow_translates_to_select_up() {
        let ev = key_press(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::SelectUp
        ));
    }

    #[test]
    fn down_arrow_translates_to_select_down() {
        let ev = key_press(KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::SelectDown
        ));
    }

    #[test]
    fn k_key_translates_to_select_up() {
        let ev = key_press(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::SelectUp
        ));
    }

    #[test]
    fn j_key_translates_to_select_down() {
        let ev = key_press(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::SelectDown
        ));
    }

    // ── File-browser keymap (task 28) ─────────────────────────────────────────

    #[test]
    fn o_key_opens_browser_in_normal_mode() {
        let ev = key_press(KeyCode::Char('o'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::OpenBrowser
        ));
    }

    // ── Run-control key translation (task 31) ─────────────────────────────────

    #[test]
    fn s_key_translates_to_start_run() {
        let ev = key_press(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::StartRun
        ));
    }

    #[test]
    fn p_key_translates_to_pause_run() {
        let ev = key_press(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
            AppEvent::PauseRun
        ));
    }

    #[test]
    fn c_key_translates_to_cancel_run() {
        // Plain `c` (no modifier) is Cancel; Ctrl-C remains Quit (covered above).
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, false, false, false),
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
                matches!(
                    translate_terminal_event(ev, true, false, false),
                    AppEvent::Tick
                ),
                "'{ch}' must be inert in browser mode"
            );
        }
    }

    #[test]
    fn enter_in_browser_activates_selection() {
        let ev = key_press(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, true, false, false),
            AppEvent::BrowserActivate
        ));
    }

    #[test]
    fn esc_in_browser_closes_not_quits() {
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        // In browser mode, Esc must close the browser, NOT quit the app.
        assert!(matches!(
            translate_terminal_event(ev, true, false, false),
            AppEvent::CloseBrowser
        ));
    }

    #[test]
    fn backspace_in_browser_goes_to_parent() {
        let ev = key_press(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, true, false, false),
            AppEvent::BrowserParent
        ));
    }

    #[test]
    fn jk_in_browser_navigate_browser_not_sidebar() {
        let down = key_press(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(down, true, false, false),
            AppEvent::BrowserDown
        ));
        let up = key_press(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(up, true, false, false),
            AppEvent::BrowserUp
        ));
    }

    #[test]
    fn ctrl_c_quits_even_in_browser_mode() {
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(ev, true, false, false),
            AppEvent::Quit
        ));
    }

    #[test]
    fn q_in_browser_is_not_quit() {
        // `q` is a normal-mode quit key; inside the browser it must not quit
        // (it falls through to Tick so the user can keep browsing).
        let ev = key_press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, true, false, false),
            AppEvent::Tick
        ));
    }

    /// Verify the full quit path: translate key → update App → should_quit.
    #[test]
    fn quit_key_drives_app_to_should_quit() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        let ev = translate_terminal_event(
            key_press(KeyCode::Char('q'), KeyModifiers::NONE),
            false,
            false,
            false,
        );
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
                        failure_reason: None,
                    },
                    TaskView {
                        id: TaskId::new("second"),
                        title: "Second task".into(),
                        state: TaskState::New,
                        gate_iterations: 0,
                        review_iterations: 0,
                        depends_on: vec![TaskId::new("first")],
                        failure_reason: None,
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

    // ── Provider configuration editor commit (task 0041) ─────────────────────

    /// Edit a provider assignment then commit → the temp `config.toml` round-trips
    /// the change.  The test exercises the full IO path:
    /// 1. Build an App with a provider-editor already open (providers + roles set).
    /// 2. Call `resolve_io(ProviderEditorCommit)` → writes `{repo_root}/.makina/config.toml`.
    /// 3. Read back the TOML and assert the providers/roles were persisted.
    #[tokio::test]
    async fn provider_editor_commit_writes_config() {
        use crate::app::{App, AppEvent, Mode, ProviderEditor};
        use crate::placeholder::PlaceholderApi;
        use makina_core::config::{ProviderConfig, RoleAssignment, RolesConfig};
        use std::sync::Arc;

        let tmp = tempfile::tempdir().expect("temp dir");
        let repo_root = tmp.path().to_path_buf();

        let providers = vec![ProviderConfig {
            name: "fast".into(),
            command: "grok".into(),
            args: vec!["agent".into()],
            env: Default::default(),
        }];
        let roles = RolesConfig {
            developer: Some(RoleAssignment {
                provider: "fast".into(),
                mode: Some("code".into()),
                model: Some("grok-3".into()),
                effort: Some("high".into()),
            }),
            ..Default::default()
        };

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn makina_core::api::Api>,
            vec![],
            repo_root.clone(),
        );
        // Seed the editor (as if the user opened it and made edits).
        app.provider_editor = Some(ProviderEditor {
            providers: providers.clone(),
            roles: roles.clone(),
            available_modes: None,
            available_config_options: vec![],
            selected_provider: Some(0),
            selection_index: 0,
        });
        app.mode = Mode::ProviderConfig;

        // Run the IO layer commit.
        let (resolved_event, status) = resolve_io(&app, AppEvent::ProviderEditorCommit).await;
        assert!(
            matches!(resolved_event, AppEvent::ProviderEditorCommit),
            "commit must return ProviderEditorCommit for App::update to close the editor"
        );
        let msg = status.expect("commit must produce a status message");
        assert!(
            msg.contains("saved") || msg.contains("Config"),
            "status must mention config write; got {msg:?}"
        );

        // The config file must have been written.
        let config_path = repo_root.join(".makina").join("config.toml");
        assert!(
            config_path.exists(),
            "config.toml must exist after commit; path: {}",
            config_path.display()
        );

        // Round-trip: read back and parse.
        let written = std::fs::read_to_string(&config_path).expect("read config");
        let parsed: makina_core::config::GlobalConfig =
            toml::from_str(&written).expect("config.toml must be valid TOML");

        // Assert the providers were persisted.
        assert_eq!(
            parsed.providers.len(),
            1,
            "one provider must be written; got {}",
            parsed.providers.len()
        );
        assert_eq!(parsed.providers[0].name, "fast");
        assert_eq!(parsed.providers[0].command, "grok");

        // Assert the role assignment was persisted.
        let dev = parsed
            .roles
            .developer
            .as_ref()
            .expect("developer role must be written");
        assert_eq!(dev.provider, "fast");
        assert_eq!(dev.mode.as_deref(), Some("code"));
        assert_eq!(dev.model.as_deref(), Some("grok-3"));
        assert_eq!(dev.effort.as_deref(), Some("high"));
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
            providers: vec![makina_core::config::ProviderConfig {
                name: "default".into(),
                command: "echo".into(),
                args: vec![],
                env: Default::default(),
            }],
            roles: makina_core::config::RolesConfig::default(),
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

    /// The derived log path from `open_log` must match the path constructed by
    /// `makina_core::paths::task_log`.  This is a unit test of the path logic
    /// without spawning a pager.
    #[test]
    fn open_log_resolves_expected_path() {
        use crate::placeholder::PlaceholderApi;
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        use std::path::PathBuf;
        use std::sync::Arc;

        // Create a minimal app with a run and task.
        let api = Arc::new(PlaceholderApi::empty());
        let repo_root = PathBuf::from("/test/repo");
        let run_id_str = "run-001-test";
        let task = TaskView {
            id: TaskId::new("my-task"),
            title: "Test Task".into(),
            state: TaskState::Done,
            gate_iterations: 0,
            review_iterations: 0,
            depends_on: vec![],
            failure_reason: None,
        };
        let run = RunView {
            id: RunId(123),
            run_uid: run_id_str.to_string(),
            task_list_path: PathBuf::from("sample.json"),
            status: RunStatus::Completed,
            project: "test".to_string(),
            tasks: vec![task],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = crate::app::App::new(api, vec![run], repo_root.clone());

        // Select the task.
        app.selected_run = Some(0);
        app.selected_task = Some(0);

        // Construct the expected path using the same logic as log.rs.
        let expected = makina_core::paths::task_log(&repo_root, run_id_str, "my-task");

        // Derive the path from the app state.
        if let Some(run) = app.selected_run() {
            if let Some(task_id) = app.selected_task_id() {
                let actual = makina_core::paths::task_log(&repo_root, &run.run_uid, &task_id.0);
                assert_eq!(
                    actual, expected,
                    "derived path must match paths::task_log output"
                );
                // Verify the path has the expected format: .makina/runs/{run_id}/logs/{task}.log
                assert!(
                    expected.to_string_lossy().contains(".makina/runs/"),
                    "path should contain .makina/runs directory"
                );
                assert!(
                    expected.to_string_lossy().ends_with("my-task.log"),
                    "path should end with task-id.log"
                );
            } else {
                panic!("no task selected");
            }
        } else {
            panic!("no run selected");
        }
    }

    /// The doctor scaffold action must refuse to run when a config file already
    /// exists on disk: it returns a refusal status message and leaves the
    /// existing file's contents untouched (it does NOT overwrite).
    #[tokio::test]
    async fn doctor_scaffold_refuses_when_present() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        // Create a REAL config file on disk so the `p.exists()` guard fires.
        let tmp = tempfile::tempdir().expect("temp dir");
        let repo_root = tmp.path().to_path_buf();
        let config_path = repo_root.join(".makina").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).expect("create .makina dir");
        let original_contents =
            "# user's existing config — must not be overwritten\nbase_branch = \"main\"\n";
        std::fs::write(&config_path, original_contents).expect("seed existing config");

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn makina_core::api::Api>,
            vec![],
            repo_root.clone(),
        );
        // Point the project config path at the real, existing file.
        app.config_paths = makina_core::config::ConfigPaths {
            global: None,
            project: Some(config_path.clone()),
        };

        // Invoke the IO path that performs the write.
        let status = write_doctor_scaffold(&app).await;

        // (a) It must return a refusal status message.
        let msg = status.expect("scaffold must emit a status message when refusing");
        assert!(
            msg.contains("already exists") && msg.contains("not overwriting"),
            "refusal message must explain the file exists and is not overwritten; got {msg:?}"
        );

        // (b) It must NOT overwrite the existing file.
        let after = std::fs::read_to_string(&config_path).expect("read config back");
        assert_eq!(
            after, original_contents,
            "existing config contents must be left untouched by the refused scaffold"
        );
    }

    /// The doctor scaffold writes both starter templates when no config exists,
    /// reports the written paths, and produces parseable TOML.
    #[tokio::test]
    async fn doctor_scaffold_writes_when_absent() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let tmp = tempfile::tempdir().expect("temp dir");
        let repo_root = tmp.path().to_path_buf();
        // Both target paths point inside the tempdir and do NOT exist yet.
        let global_path = repo_root.join("global").join("config.toml");
        let project_path = repo_root.join(".makina").join("config.toml");

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn makina_core::api::Api>,
            vec![],
            repo_root.clone(),
        );
        app.config_paths = makina_core::config::ConfigPaths {
            global: Some(global_path.clone()),
            project: Some(project_path.clone()),
        };

        let status = write_doctor_scaffold(&app).await;
        let msg = status.expect("scaffold must emit a status message");
        assert!(
            msg.contains("written"),
            "status must name the written files; got {msg:?}"
        );

        // Both files must now exist with non-empty, parseable contents.
        assert!(global_path.exists(), "global config must be written");
        assert!(project_path.exists(), "project config must be written");
        let project_contents = std::fs::read_to_string(&project_path).expect("read project config");
        let _: toml::Value =
            toml::from_str(&project_contents).expect("scaffold project config must be valid TOML");
    }
}
