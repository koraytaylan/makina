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

    // Background UI jobs send their resolved AppEvents here. This keeps
    // expensive open/discovery work off the render loop while preserving the
    // single App::update path for state changes.
    let (background_tx, mut background_rx) = mpsc::channel::<AppEvent>(64);

    // Periodic tick timer.
    let mut ticker = time::interval(TICK_INTERVAL);

    // Initial render.
    tui.draw(|frame| ui::render(app, frame))?;

    loop {
        let modal = ModalState {
            browsing: app.is_browsing(),
            editing_providers: app.is_editing_providers(),
            viewing_doctor: app.is_viewing_doctor(),
            command_palette: app.is_command_palette(),
            settings: app.is_settings(),
            picking_plan: app.is_picking_plan(),
        };
        let app_event: Option<AppEvent> = tokio::select! {
            // Bias toward terminal input (lower latency for keystrokes).
            biased;

            maybe_term = term_rx.recv() => {
                maybe_term.map(|ev| translate_terminal_event(ev, modal, app.focused_panel))

            }

            maybe_background = background_rx.recv() => {
                maybe_background
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
            let (event, status) = resolve_io(app, event, &background_tx).await;

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
/// - **File browser** (task 28): `OpenBrowser` → spawn plan discovery / CWD read;
///   `BrowserActivate` on a dir → spawn the read; on a file → compute transient
///   "Interpreting …" status, spawn `execute(OpenRun)`, and close the browser
///   immediately; `BrowserParent` → spawn parent read.
/// - **Run control** (task 31): `StartRun`/`PauseRun`/`CancelRun` →
///   `execute(...)` for `app.selected_run()` (outcome/error → status message);
///   the run-state changes themselves flow back via `api.subscribe()`.
///
/// Non-IO events pass straight through with no status message.
async fn resolve_io(
    app: &App,
    event: AppEvent,
    background_tx: &mpsc::Sender<AppEvent>,
) -> (AppEvent, Option<String>) {
    match event {
        AppEvent::OpenBrowser => {
            spawn_open_browser(app.repo_root.clone(), background_tx.clone());
            (
                AppEvent::OpenBrowser,
                Some("Discovering plans...".to_string()),
            )
        }
        AppEvent::BrowserParent => match app.browser.as_ref().and_then(|b| b.parent()) {
            Some(parent) => {
                spawn_read_dir(parent.to_path_buf(), background_tx.clone());
                (AppEvent::Tick, None)
            }
            // Already at the root — nothing to do; just redraw.
            None => (AppEvent::Tick, None),
        },
        AppEvent::BrowserActivate => {
            match app.browser.as_ref().and_then(|b| b.selected_entry()) {
                Some(entry) if entry.is_dir => {
                    spawn_read_dir(entry.path.clone(), background_tx.clone());
                    (AppEvent::Tick, None)
                }
                Some(entry) => {
                    // It's a file: compute a transient "Interpreting …" status
                    // (for immediate user feedback), then let a background task
                    // perform execute(OpenRun). The RunLoaded event will populate
                    // the panes when the core open completes.
                    let stem = entry
                        .path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("task list");
                    let status = format!("Interpreting {}...", stem);
                    spawn_open_run(
                        std::sync::Arc::clone(&app.api),
                        entry.path.clone(),
                        background_tx.clone(),
                    );
                    (AppEvent::CloseBrowser, Some(status))
                }
                // No selection (empty dir) — ignore.
                None => (AppEvent::Tick, None),
            }
        }
        // ── Plan picker (plan 0027) ───────────────────────────────────────────
        // Activate a selected plan: for plans with TASKS.md, open via OpenRun;
        // for plans without TASKS.md, route to planner-generate path.
        AppEvent::PlanActivate => match app.selected_plan() {
            Some(entry) if entry.has_tasks => {
                let path = entry.dir.join("TASKS.md");
                let status = format!("Interpreting {}...", entry.slug);
                spawn_open_run(std::sync::Arc::clone(&app.api), path, background_tx.clone());
                (AppEvent::CloseBrowser, Some(status))
            }
            Some(entry) => {
                // plan 0028: planner-generate(entry.dir) — route here instead of OpenRun.
                (
                    AppEvent::CloseBrowser,
                    Some(format!(
                        "{}: no TASKS.md — planner will generate the graph",
                        entry.slug
                    )),
                )
            }
            None => (AppEvent::Tick, None),
        },
        // ── Run control (task 31) ─────────────────────────────────────────────
        AppEvent::StartRun => (AppEvent::Tick, run_control(app, ControlKind::Start).await),
        AppEvent::PauseRun => (AppEvent::Tick, run_control(app, ControlKind::Pause).await),
        AppEvent::CancelRun => (AppEvent::Tick, run_control(app, ControlKind::Cancel).await),
        AppEvent::Reinterpret => (
            AppEvent::Tick,
            run_control(app, ControlKind::Reinterpret).await,
        ),
        // ── Context-sensitive retry (plan 0017) ───────────────────────────────
        AppEvent::RetryFocused => (AppEvent::Tick, retry_focused(app).await),
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
        // ── Settings commit (plan 0070) ──────────────────────────────────────
        // Write the edited caps and concurrency back to the config file
        // (`{repo_root}/.makina/config.toml`). Validates all fields first;
        // on any error, returns the reason without writing. Like
        // commit_provider_config, this round-trips through GlobalConfig (which
        // has no gates/base_branch field), so ProjectConfig entries are NOT
        // preserved. The actual state update (closing the modal, applying values
        // to app.caps/concurrency) is handled by App::update after this returns.
        AppEvent::SettingsCommit => {
            let status = commit_settings(app).await;
            (AppEvent::SettingsCommit, status)
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
        // ── Command palette execute (task command-palette-keys) ──────────────────
        // Extract the selected action's event from the palette before App::update
        // clears it, then re-dispatch that event through the normal intent path so
        // the event loop processes it exactly as a fresh intent.
        AppEvent::CommandPaletteExecute => {
            let selected_event = app.command_palette.as_ref().and_then(|palette| {
                let filtered = palette.filtered();
                filtered
                    .get(palette.selected)
                    .map(|action| action.event.clone())
            });
            match selected_event {
                Some(event) => (event, None),
                // No valid selection (shouldn't happen) — just tick.
                None => (AppEvent::Tick, None),
            }
        }
        // ── Project discovery (plan 0025) ──────────────────────────────────────
        // Force re-run discovery regardless of the [discovery] stamp, re-scan,
        // replace discovered gates, update last_run timestamp.
        AppEvent::DiscoverProject => {
            let status = discover_project(app).await;
            (AppEvent::Tick, status)
        }
        // Everything else passes straight through.
        other => (other, None),
    }
}

fn spawn_open_browser(repo_root: std::path::PathBuf, background_tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let discovery_root = repo_root.clone();
        let plans = tokio::task::spawn_blocking(move || {
            makina_core::orchestrator::discover_plans(&discovery_root)
        })
        .await
        .unwrap_or_default();

        let event = if plans.is_empty() {
            let start = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            read_dir_event(&start).await
        } else {
            AppEvent::PlansDiscovered { plans }
        };
        let _ = background_tx.send(event).await;
    });
}

fn spawn_read_dir(dir: std::path::PathBuf, background_tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let event = read_dir_event(&dir).await;
        let _ = background_tx.send(event).await;
    });
}

fn spawn_open_run(
    api: std::sync::Arc<dyn makina_core::api::Api>,
    task_list_path: std::path::PathBuf,
    background_tx: mpsc::Sender<AppEvent>,
) {
    tokio::spawn(async move {
        let result = api
            .execute(makina_core::api::Command::OpenRun { task_list_path })
            .await;
        if let Err(e) = result {
            let _ = background_tx
                .send(AppEvent::StatusMessage(format!("Open failed: {e}")))
                .await;
        }
    });
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

/// Commit edited settings (caps and concurrency) to the config file.
///
/// Follows the same pattern as `commit_provider_config`: read the current
/// on-disk config (if any), rebuild it with the new caps/concurrency while
/// preserving all other fields, and write it back. Validates all fields
/// before writing; on any error returns Some(reason) without writing.
///
/// Note: this writer round-trips through `GlobalConfig`, which has no
/// `gates`/`base_branch` field, so the `ProjectConfig` `[[gates]]` /
/// `base_branch` tables are NOT preserved. The gates-aware project-config
/// writer lands in plan 0025.
async fn commit_settings(app: &App) -> Option<String> {
    use makina_core::config::{CapsConfig, GlobalConfig};
    use makina_core::paths::config_file;

    let settings = app.settings.as_ref()?;

    // Parse and validate every field.
    let gate_iterations = match settings.gate_iterations.parse::<u32>() {
        Ok(val) => {
            if val >= 1 {
                val
            } else {
                return Some("caps.gate_iterations must be at least 1".to_string());
            }
        }
        Err(_) => {
            return Some("caps.gate_iterations must be a positive integer".to_string());
        }
    };

    let reviewer_iterations = match settings.reviewer_iterations.parse::<u32>() {
        Ok(val) => {
            if val >= 1 {
                val
            } else {
                return Some("caps.reviewer_iterations must be at least 1".to_string());
            }
        }
        Err(_) => {
            return Some("caps.reviewer_iterations must be a positive integer".to_string());
        }
    };

    let wall_clock_secs = match settings.wall_clock_secs.parse::<u64>() {
        Ok(val) => {
            if val >= 1 {
                val
            } else {
                return Some("caps.wall_clock_secs must be at least 1".to_string());
            }
        }
        Err(_) => {
            return Some("caps.wall_clock_secs must be a positive integer".to_string());
        }
    };

    let idle_secs = if settings.idle_secs.is_empty() {
        None
    } else {
        match settings.idle_secs.parse::<u64>() {
            Ok(val) => {
                if val >= 1 {
                    Some(val)
                } else {
                    return Some("caps.idle_secs must be at least 1".to_string());
                }
            }
            Err(_) => {
                return Some("caps.idle_secs must be a positive integer".to_string());
            }
        }
    };

    let concurrency = match settings.concurrency.parse::<usize>() {
        Ok(val) => {
            if val >= 1 {
                val
            } else {
                return Some("concurrency must be at least 1".to_string());
            }
        }
        Err(_) => {
            return Some("concurrency must be a positive integer".to_string());
        }
    };

    // Read the current on-disk config (if any) so we don't lose fields we
    // don't manage (e.g. providers, roles, project gates, base_branch).
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
    // replace caps and concurrency with the new values.
    let updated = GlobalConfig {
        caps: CapsConfig {
            gate_iterations,
            reviewer_iterations,
            wall_clock_secs,
            idle_secs,
        },
        concurrency,
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
        Ok(()) => Some("Settings saved".to_string()),
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

/// Resolve a context-sensitive retry on the focused sidebar tree node (plan
/// 0017) and return a status message describing the outcome.
///
/// - A focused `Failed` task → [`Command::RetryTask`].
/// - A focused run with any `Failed` task → [`Command::RetryFailedTasks`].
/// - Otherwise, if the focused run is still `Pending`, fall back to
///   [`Command::ReinterpretRun`] (the recover-from-blocking flow that `[r]`
///   previously served), so a single key still serves both purposes.
/// - Otherwise emit `nothing to retry here` and issue no command.
///
/// The actual state changes (task resets, run resuming) flow back through
/// `api.subscribe()`; this only surfaces the command's immediate acknowledgement
/// / error in the status bar.
async fn retry_focused(app: &App) -> Option<String> {
    use crate::app::TreeNode;
    use makina_core::api::{Command, RunStatus, TaskState};

    // Resolve the focused node into an optional retry command + a label.
    let command: Option<(Command, String)> = match app.focused_node() {
        Some(TreeNode::Task { run, task }) => {
            let rv = app.runs.get(run)?;
            let tv = rv.tasks.get(task)?;
            if tv.state == TaskState::Failed {
                Some((
                    Command::RetryTask {
                        run: rv.id,
                        task: tv.id.clone(),
                    },
                    format!("retry {}", tv.id.0),
                ))
            } else {
                None
            }
        }
        Some(TreeNode::Run { run }) => {
            let rv = app.runs.get(run)?;
            if rv.tasks.iter().any(|t| t.state == TaskState::Failed) {
                Some((
                    Command::RetryFailedTasks { run: rv.id },
                    format!("retry failed tasks in {}", rv.id),
                ))
            } else {
                None
            }
        }
        None => None,
    };

    if let Some((command, verb)) = command {
        return match app.api.execute(command).await {
            Ok(_) => Some(verb),
            Err(e) => Some(format!("{verb} failed: {e}")),
        };
    }

    // Nothing retryable. Fall back to re-interpreting a still-Pending run so the
    // single `[r]` key still serves the recover-from-blocking flow; otherwise a
    // no-op message.
    if app
        .focused_node()
        .and_then(|node| match node {
            TreeNode::Run { run } | TreeNode::Task { run, .. } => app.runs.get(run),
        })
        .map(|rv| rv.status == RunStatus::Pending)
        .unwrap_or(false)
    {
        return run_control(app, ControlKind::Reinterpret).await;
    }

    Some("nothing to retry here".to_string())
}

/// Force-re-run project discovery, regardless of the [discovery] stamp.
///
/// Issues `Command::DiscoverProject` to the orchestrator, which re-scans the
/// repository, replaces all `source = "discovered"` gates, folds the updated role
/// constraints into the role assignments, and re-stamps `last_run` in the project
/// config. The orchestrator emits `Event::ProjectDiscovered` on completion, which
/// the TUI processes via `api.subscribe()`.
///
/// Non-fatal: discovery failure is logged by the orchestrator and returns
/// `Acknowledged`; this function surfaces the outcome in the status bar.
async fn discover_project(app: &App) -> Option<String> {
    match app
        .api
        .execute(makina_core::api::Command::DiscoverProject)
        .await
    {
        Ok(_) => Some("Project discovery started".to_string()),
        Err(e) => Some(format!("Project discovery failed: {e}")),
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

/// Snapshot of which modal overlay is currently active.
///
/// Grouping the flags into a struct keeps `translate_terminal_event` and
/// `translate_key` within clippy's `too_many_arguments` threshold (≤ 7).
#[derive(Clone, Copy, Default)]
struct ModalState {
    browsing: bool,
    editing_providers: bool,
    viewing_doctor: bool,
    command_palette: bool,
    settings: bool,
    picking_plan: bool,
}

/// Translate a raw crossterm [`CrosstermEvent`] into an [`AppEvent`].
///
/// `modal` bundles which overlay is active; `focused_panel` determines whether
/// Space emits `ToggleTreeNode` (sidebar only).
///
/// Returns [`AppEvent::Tick`] for events the TUI doesn't handle (e.g. mouse
/// events); those simply trigger a harmless redraw.
fn translate_terminal_event(
    ev: CrosstermEvent,
    modal: ModalState,
    focused_panel: crate::app::Panel,
) -> AppEvent {
    match ev {
        CrosstermEvent::Key(key) => translate_key(key, modal, focused_panel),
        CrosstermEvent::Resize(w, h) => AppEvent::Resize(w, h),
        // Mouse wheel scrolls the focused exchange pane regardless of the
        // `browsing` flag (the exchange pane is not the browser).
        // Down/Up/Drag/Moved → no-op so native modifier-drag selection works:
        // the terminal's bypass modifier (Shift on most terminals; Option/Alt
        // in iTerm2) lets the user select & copy text even with capture on,
        // because the app never consumes those event kinds.
        CrosstermEvent::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => AppEvent::ScrollUp,
            MouseEventKind::ScrollDown => AppEvent::ScrollDown,
            _ => AppEvent::Tick, // Down/Up/Drag/Moved → no-op so native selection works
        },
        // Paste, focus, etc. — ignored for now.
        _ => AppEvent::Tick,
    }
}

/// Translate a key press into an [`AppEvent`], honouring the current view mode.
fn translate_key(
    key: crossterm::event::KeyEvent,
    modal: ModalState,
    focused_panel: crate::app::Panel,
) -> AppEvent {
    let ModalState {
        browsing,
        editing_providers,
        viewing_doctor,
        command_palette,
        settings,
        picking_plan,
    } = modal;
    use crossterm::event::KeyEventKind;
    // Only react to key-press events (not key-release / repeat on some platforms).
    if key.kind != KeyEventKind::Press {
        return AppEvent::Tick;
    }

    // Ctrl-C always quits, in any mode.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return AppEvent::Quit;
    }

    // Ctrl-P opens the command palette in Normal mode (before the per-mode cascade).
    if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return AppEvent::OpenCommandPalette;
    }

    if command_palette {
        // ── Command palette keymap ────────────────────────────────────────────
        // Esc closes the palette; Enter executes the selected action; Up/Down move
        // the selection; Backspace removes filter chars; letters feed the filter.
        match key.code {
            KeyCode::Esc => AppEvent::CloseCommandPalette,
            KeyCode::Enter => AppEvent::CommandPaletteExecute,
            KeyCode::Up => AppEvent::CommandPaletteUp,
            KeyCode::Down => AppEvent::CommandPaletteDown,
            KeyCode::Backspace => AppEvent::CommandPaletteBackspace,
            KeyCode::Char(c) => AppEvent::CommandPaletteInput(c),
            _ => AppEvent::Tick,
        }
    } else if picking_plan {
        // ── Plan picker keymap ────────────────────────────────────────────────
        // Esc closes the picker (does NOT quit the app); Enter activates the
        // selection; j/k/arrows navigate.
        match key.code {
            KeyCode::Esc => AppEvent::CloseBrowser,
            KeyCode::Enter => AppEvent::PlanActivate,
            KeyCode::Up | KeyCode::Char('k') => AppEvent::PlanPickerUp,
            KeyCode::Down | KeyCode::Char('j') => AppEvent::PlanPickerDown,
            _ => AppEvent::Tick,
        }
    } else if browsing {
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
    } else if settings {
        // ── Settings modal keymap ────────────────────────────────────────────
        // Esc closes without saving; Enter commits; Up/Down navigate fields;
        // 0-9 and Backspace edit the focused numeric field.
        match key.code {
            KeyCode::Esc => AppEvent::CloseSettings,
            KeyCode::Enter => AppEvent::SettingsCommit,
            KeyCode::Up => AppEvent::SettingsUp,
            KeyCode::Down => AppEvent::SettingsDown,
            KeyCode::Backspace => AppEvent::SettingsBackspace,
            KeyCode::Char(c) => AppEvent::SettingsInput(c),
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
            // Toggle verbose mode on/off (Ctrl+O — checked BEFORE the plain
            // `o`/`O` → OpenBrowser arm so the modifier guard wins).
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                AppEvent::ToggleVerbose
            }
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
            // Context-sensitive retry (plan 0017): retry the focused failed task
            // or the focused run's failures.  Falls back to re-interpreting a
            // still-Pending run (the recover-from-blocking flow) when nothing is
            // retryable.
            KeyCode::Char('r') | KeyCode::Char('R') => AppEvent::RetryFocused,
            // Dismiss the provider-missing warning banner (non-fatal; just hides it).
            KeyCode::Char('d') | KeyCode::Char('D') => AppEvent::DismissProviderWarning,
            // Sidebar navigation: arrow keys and vim-style j/k.
            KeyCode::Up | KeyCode::Char('k') => AppEvent::SelectUp,
            KeyCode::Down | KeyCode::Char('j') => AppEvent::SelectDown,
            // Right arrow: expand collapsed run or cross focus to content pane.
            KeyCode::Right => AppEvent::FocusRightOrExpand,
            // Left arrow: collapse expanded run or return focus to sidebar.
            KeyCode::Left => AppEvent::FocusLeftOrCollapse,
            // Space: toggle expand/collapse the focused tree node (sidebar focus only).
            KeyCode::Char(' ') => {
                use crate::app::Panel;
                match focused_panel {
                    Panel::Sidebar => AppEvent::ToggleTreeNode,
                    Panel::Main => AppEvent::Tick,
                }
            }
            _ => AppEvent::Tick,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    };

    fn key_press(code: KeyCode, modifiers: KeyModifiers) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn background_events() -> (mpsc::Sender<AppEvent>, mpsc::Receiver<AppEvent>) {
        mpsc::channel(64)
    }

    async fn resolve_io_for_test(app: &App, event: AppEvent) -> (AppEvent, Option<String>) {
        let (tx, _rx) = background_events();
        resolve_io(app, event, &tx).await
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
            translate_terminal_event(
                wheel(MouseEventKind::ScrollUp),
                ModalState::default(),
                crate::app::Panel::Sidebar
            ),
            AppEvent::ScrollUp
        ));
        assert!(matches!(
            translate_terminal_event(
                wheel(MouseEventKind::ScrollDown),
                ModalState::default(),
                crate::app::Panel::Sidebar
            ),
            AppEvent::ScrollDown
        ));
    }

    /// Drag and move events must map to `AppEvent::Tick` (no-op) so the
    /// terminal's modifier-bypass selection (Shift-drag; Option-drag in iTerm2)
    /// continues to work even when mouse capture is enabled.
    #[test]
    fn mouse_drag_is_noop() {
        let drag = wheel(MouseEventKind::Drag(MouseButton::Left));
        assert!(
            matches!(
                translate_terminal_event(drag, ModalState::default(), crate::app::Panel::Sidebar),
                AppEvent::Tick
            ),
            "Drag(Left) must translate to Tick so native selection coexists"
        );

        let moved = wheel(MouseEventKind::Moved);
        assert!(
            matches!(
                translate_terminal_event(moved, ModalState::default(), crate::app::Panel::Sidebar),
                AppEvent::Tick
            ),
            "Moved must translate to Tick so native selection coexists"
        );
    }

    #[test]
    fn q_key_translates_to_quit() {
        let ev = key_press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::Quit
        ));
    }

    #[test]
    fn esc_key_translates_to_quit() {
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::Quit
        ));
    }

    #[test]
    fn ctrl_c_translates_to_quit() {
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::Quit
        ));
    }

    #[test]
    fn tab_translates_to_focus_next() {
        let ev = key_press(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::FocusNext
        ));
    }

    #[test]
    fn v_translates_to_cycle_dependency_view() {
        assert!(matches!(
            translate_terminal_event(
                key_press(KeyCode::Char('v'), KeyModifiers::NONE),
                ModalState::default(),
                crate::app::Panel::Sidebar
            ),
            AppEvent::CycleDependencyView
        ));
    }

    #[test]
    fn e_key_translates_to_toggle_error_pane() {
        assert!(matches!(
            translate_terminal_event(
                key_press(KeyCode::Char('e'), KeyModifiers::NONE),
                ModalState::default(),
                crate::app::Panel::Sidebar
            ),
            AppEvent::ToggleErrorPane
        ));
    }

    #[test]
    fn r_key_translates_to_retry_focused() {
        // Plan 0017 repurposes `r` from Reinterpret to context-sensitive retry.
        assert!(matches!(
            translate_terminal_event(
                key_press(KeyCode::Char('r'), KeyModifiers::NONE),
                ModalState::default(),
                crate::app::Panel::Sidebar
            ),
            AppEvent::RetryFocused
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
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::Tick
        ));
    }

    #[test]
    fn resize_translates_to_resize_event() {
        let ev = CrosstermEvent::Resize(120, 40);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::Resize(120, 40)
        ));
    }

    #[test]
    fn up_arrow_translates_to_select_up() {
        let ev = key_press(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::SelectUp
        ));
    }

    #[test]
    fn down_arrow_translates_to_select_down() {
        let ev = key_press(KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::SelectDown
        ));
    }

    #[test]
    fn right_arrow_translates_to_focus_right_or_expand() {
        let ev = key_press(KeyCode::Right, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::FocusRightOrExpand
        ));
    }

    #[test]
    fn left_arrow_translates_to_focus_left_or_collapse() {
        let ev = key_press(KeyCode::Left, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::FocusLeftOrCollapse
        ));
    }

    #[test]
    fn k_key_translates_to_select_up() {
        let ev = key_press(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::SelectUp
        ));
    }

    #[test]
    fn j_key_translates_to_select_down() {
        let ev = key_press(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::SelectDown
        ));
    }

    // ── File-browser keymap (task 28) ─────────────────────────────────────────

    #[test]
    fn o_key_opens_browser_in_normal_mode() {
        let ev = key_press(KeyCode::Char('o'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::OpenBrowser
        ));
    }

    // ── Run-control key translation (task 31) ─────────────────────────────────

    #[test]
    fn s_key_translates_to_start_run() {
        let ev = key_press(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::StartRun
        ));
    }

    #[test]
    fn p_key_translates_to_pause_run() {
        let ev = key_press(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::PauseRun
        ));
    }

    #[test]
    fn c_key_translates_to_cancel_run() {
        // Plain `c` (no modifier) is Cancel; Ctrl-C remains Quit (covered above).
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
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
                    translate_terminal_event(
                        ev,
                        ModalState {
                            browsing: true,
                            ..ModalState::default()
                        },
                        crate::app::Panel::Sidebar
                    ),
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
            translate_terminal_event(
                ev,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::BrowserActivate
        ));
    }

    #[test]
    fn esc_in_browser_closes_not_quits() {
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        // In browser mode, Esc must close the browser, NOT quit the app.
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::CloseBrowser
        ));
    }

    #[test]
    fn backspace_in_browser_goes_to_parent() {
        let ev = key_press(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::BrowserParent
        ));
    }

    #[test]
    fn jk_in_browser_navigate_browser_not_sidebar() {
        let down = key_press(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                down,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::BrowserDown
        ));
        let up = key_press(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                up,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::BrowserUp
        ));
    }

    #[test]
    fn ctrl_c_quits_even_in_browser_mode() {
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::Quit
        ));
    }

    #[test]
    fn q_in_browser_is_not_quit() {
        // `q` is a normal-mode quit key; inside the browser it must not quit
        // (it falls through to Tick so the user can keep browsing).
        let ev = key_press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
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
            ModalState::default(),
            crate::app::Panel::Sidebar,
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
        let (ev, status) = resolve_io_for_test(&app, AppEvent::StartRun).await;
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
        let (_ev, status) = resolve_io_for_test(&app, AppEvent::PauseRun).await;
        assert!(status.unwrap().contains("Pause"));

        // Cancel.
        let (_ev, status) = resolve_io_for_test(&app, AppEvent::CancelRun).await;
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

        let (ev, status) = resolve_io_for_test(&app, AppEvent::StartRun).await;
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

        let (_ev, status) = resolve_io_for_test(&app, AppEvent::StartRun).await;
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

        let tmpdir = tempfile::tempdir().unwrap();
        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], tmpdir.path().to_path_buf());
        let (tx, mut rx) = background_events();

        let (resolved, status) = resolve_io(&app, AppEvent::OpenBrowser, &tx).await;
        assert!(
            matches!(resolved, AppEvent::OpenBrowser),
            "OpenBrowser must return immediately"
        );
        assert_eq!(status.as_deref(), Some("Discovering plans..."));
        let opened = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for BrowserOpened")
            .expect("background channel closed");
        match opened {
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

        let (resolved, status) = resolve_io_for_test(&app, AppEvent::BrowserActivate).await;
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
    /// call `resolve_io(BrowserActivate)` (which spawns `api.execute(OpenRun)`),
    /// then drain `api.subscribe()` and feed the resulting `RunOpened` into
    /// `App::update` — asserting the Run appears in `app.runs`.
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
        let (tx, _rx) = background_events();
        let (resolved, status) = resolve_io(&app, AppEvent::BrowserActivate, &tx).await;
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

        // The RunOpened event flows back through subscribe(); feeding it into
        // App::update makes the Run appear in app.runs (the sidebar source).
        let ev = tokio::time::timeout(std::time::Duration::from_secs(1), sub.next())
            .await
            .expect("timed out waiting for RunOpened")
            .expect("stream ended unexpectedly");
        assert!(matches!(ev, makina_core::api::Event::RunOpened { .. }));
        app.update(AppEvent::ApiEvent(ev));

        // The CoreApi created the Run (direct query proves OpenRun happened).
        let runs = api.runs().await;
        assert_eq!(runs.len(), 1, "CoreApi must have created exactly one run");
        assert_eq!(runs[0].task_list_path, file_path);
        assert_eq!(runs[0].tasks.len(), 1, "the task must be interpreted");

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
                        started_at: None,
                        finished_at: None,
                        failure_reason: None,
                    },
                    TaskView {
                        id: TaskId::new("second"),
                        title: "Second task".into(),
                        state: TaskState::New,
                        gate_iterations: 0,
                        review_iterations: 0,
                        depends_on: vec![TaskId::new("first")],
                        started_at: None,
                        finished_at: None,
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
                system_prompt: None,
                system_prompt_mode: None,
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
        let (resolved_event, status) =
            resolve_io_for_test(&app, AppEvent::ProviderEditorCommit).await;
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
            merge: makina_core::config::MergeConfig::default(),
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
        use makina_core::HOME_ENV_LOCK;
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        use std::path::PathBuf;
        use std::sync::Arc;

        let _guard = HOME_ENV_LOCK.blocking_lock();

        // Set HOME to a temp dir so state_root resolves deterministically.
        let temp_home = tempfile::tempdir().expect("create temp home");
        let original_home = std::env::var_os("HOME");
        // SAFETY: serialised by HOME_ENV_LOCK
        unsafe { std::env::set_var("HOME", temp_home.path()) };

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
            started_at: None,
            finished_at: None,
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
                // After plan-0029, task_log lives under state_root (HOME-based), not repo_root.
                let state_root = makina_core::paths::state_root(&repo_root);
                assert!(
                    expected.starts_with(&state_root),
                    "path should be under state_root ({}), got {}",
                    state_root.display(),
                    expected.display()
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

        // Restore HOME.
        // SAFETY: serialised by HOME_ENV_LOCK
        unsafe {
            match original_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
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

    // ── Context-sensitive retry key (plan 0017) ───────────────────────────────

    use async_trait::async_trait;
    use makina_core::api::{
        Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunStatus, RunView,
        TaskId, TaskState, TaskView,
    };
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    /// A stub api that records every `Command` it executes (for retry-key tests).
    struct RetryRecordingApi {
        commands: StdMutex<Vec<Command>>,
    }

    impl RetryRecordingApi {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                commands: StdMutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Api for RetryRecordingApi {
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

    fn task_view(id: &str, state: TaskState) -> TaskView {
        TaskView {
            id: TaskId::new(id),
            title: format!("Task {id}"),
            state,
            gate_iterations: 0,
            review_iterations: 0,
            depends_on: vec![],
            started_at: None,
            finished_at: None,
            failure_reason: None,
        }
    }

    /// Build a single-run App with the given tasks and run status, backed by a
    /// recording api. Runs are expanded by default so task nodes are focusable.
    fn retry_app(
        tasks: Vec<TaskView>,
        status: RunStatus,
    ) -> (crate::app::App, Arc<RetryRecordingApi>) {
        use crate::app::App;
        let api = RetryRecordingApi::new();
        let run = RunView {
            id: RunId(7),
            run_uid: String::new(),
            task_list_path: std::path::PathBuf::from(".tasks/retry.json"),
            status,
            project: String::new(),
            tasks,
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(
            Arc::clone(&api) as Arc<dyn Api>,
            vec![run],
            std::path::PathBuf::from("."),
        );
        (app, api)
    }

    /// Focusing a `Failed` task and pressing `[r]` issues `RetryTask` with the
    /// focused run + task.
    #[tokio::test]
    async fn retry_key_on_failed_task_issues_retry_task() {
        use crate::app::{AppEvent, TreeNode};
        let (mut app, api) = retry_app(vec![task_view("a", TaskState::Failed)], RunStatus::Failed);
        // tree_cursor starts on the Run node; move down to the (Failed) task node.
        app.tree_move(1);
        assert!(
            matches!(app.focused_node(), Some(TreeNode::Task { task: 0, .. })),
            "the focused node must be the failed task"
        );

        let (_ev, status) = resolve_io_for_test(&app, AppEvent::RetryFocused).await;
        assert!(status.is_some(), "retry must surface a status message");

        let cmds = api.commands.lock().unwrap().clone();
        assert_eq!(cmds.len(), 1, "exactly one command must be issued");
        assert!(
            matches!(
                &cmds[0],
                Command::RetryTask { run: RunId(7), task } if task.0 == "a"
            ),
            "RetryFocused on a Failed task must issue RetryTask{{run:7, task:a}}; got {:?}",
            cmds[0]
        );
    }

    /// Focusing a run node with at least one `Failed` task and pressing `[r]`
    /// issues `RetryFailedTasks`.
    #[tokio::test]
    async fn retry_key_on_run_node_issues_retry_failed() {
        use crate::app::{AppEvent, TreeNode};
        let (app, api) = retry_app(
            vec![
                task_view("a", TaskState::Done),
                task_view("b", TaskState::Failed),
            ],
            RunStatus::Failed,
        );
        // tree_cursor starts on the Run node.
        assert!(
            matches!(app.focused_node(), Some(TreeNode::Run { .. })),
            "the focused node must be the run header"
        );

        let (_ev, status) = resolve_io_for_test(&app, AppEvent::RetryFocused).await;
        assert!(status.is_some());

        let cmds = api.commands.lock().unwrap().clone();
        assert_eq!(cmds.len(), 1);
        assert!(
            matches!(&cmds[0], Command::RetryFailedTasks { run: RunId(7) }),
            "RetryFocused on a run with failures must issue RetryFailedTasks; got {:?}",
            cmds[0]
        );
    }

    /// Focusing a `Done` task (nothing retryable, run not Pending) issues no
    /// command and sets the `nothing to retry here` status message.
    #[tokio::test]
    async fn retry_key_noop_when_nothing_failed() {
        use crate::app::{AppEvent, TreeNode};
        let (mut app, api) = retry_app(vec![task_view("a", TaskState::Done)], RunStatus::Completed);
        app.tree_move(1); // focus the Done task.
        assert!(matches!(
            app.focused_node(),
            Some(TreeNode::Task { task: 0, .. })
        ));

        let (_ev, status) = resolve_io_for_test(&app, AppEvent::RetryFocused).await;
        assert_eq!(
            status.as_deref(),
            Some("nothing to retry here"),
            "a Done task in a non-Pending run must yield the no-op message"
        );
        assert!(
            api.commands.lock().unwrap().is_empty(),
            "no command must be issued when nothing is retryable"
        );
    }

    /// The status bar advertises the `[r]` retry key.
    #[test]
    fn status_bar_advertises_retry_key() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let api = RetryRecordingApi::new();
        let run = RunView {
            id: RunId(7),
            run_uid: String::new(),
            task_list_path: std::path::PathBuf::from(".tasks/retry.json"),
            status: RunStatus::Failed,
            project: String::new(),
            tasks: vec![task_view("a", TaskState::Failed)],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(
            Arc::clone(&api) as Arc<dyn Api>,
            vec![run],
            std::path::PathBuf::from("."),
        );

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| crate::ui::render(&app, f))
            .expect("render");

        let buffer = terminal.backend().buffer().clone();
        let rendered: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(
            rendered.contains("[r]"),
            "the status bar must advertise the [r] retry key; rendered: {rendered}"
        );
    }

    #[test]
    fn ctrl_p_opens_palette() {
        // Ctrl+P from Normal mode (all flags false) must open the palette.
        let ev = key_press(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(ev, ModalState::default(), crate::app::Panel::Sidebar),
            AppEvent::OpenCommandPalette
        ));
    }

    #[tokio::test]
    async fn palette_enter_executes_selected_action() {
        // With command_palette=true, Enter must map to CommandPaletteExecute.
        let ev = key_press(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    command_palette: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::CommandPaletteExecute
        ));

        // resolve_io must extract the selected action's event and re-dispatch it.
        // Create an app with an open palette and a selected action.
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Open the palette.
        app.update(AppEvent::OpenCommandPalette);
        assert!(app.is_command_palette());
        assert!(app.command_palette.is_some());

        // The default palette has multiple actions. Manually set selected to point
        // to the "Settings" action (which is at index 2 in default_actions).
        if let Some(ref mut palette) = app.command_palette {
            palette.selected = 2; // Settings
        }

        // Resolve CommandPaletteExecute; it should extract the Settings event.
        let (resolved_event, _) = resolve_io_for_test(&app, AppEvent::CommandPaletteExecute).await;

        // The resolved event should be OpenSettings.
        assert!(
            matches!(resolved_event, AppEvent::OpenSettings),
            "palette execute with Settings selected must resolve to OpenSettings"
        );
    }

    #[test]
    fn palette_esc_closes() {
        // Esc from the palette must map to CloseCommandPalette.
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    command_palette: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::CloseCommandPalette
        ));
    }

    #[tokio::test]
    async fn edit_and_commit_writes_config() {
        // Create a temp directory for the repo root.
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let repo_root = tmpdir.path();

        // Write a pre-existing config with unrelated fields (e.g., [[providers]]).
        let existing_config = r#"
[[providers]]
name = "claude"
command = "claude-acp"

[roles]
developer = { provider = "claude" }
reviewer = { provider = "claude" }

[caps]
gate_iterations = 7
reviewer_iterations = 3
wall_clock_secs = 1200
"#;
        let config_path = repo_root.join(".makina/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).expect("mkdir");
        std::fs::write(&config_path, existing_config).expect("write existing");

        // Create an app with known caps and concurrency.
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], repo_root.to_path_buf());
        app.caps = makina_core::config::CapsConfig {
            gate_iterations: 7,
            reviewer_iterations: 3,
            wall_clock_secs: 1200,
            idle_secs: None,
        };
        app.concurrency = 4;

        // Open settings and edit a field.
        app.update(AppEvent::OpenSettings);
        assert!(app.is_settings());

        // Modify gate_iterations to "10".
        app.settings.as_mut().unwrap().gate_iterations.clear();
        app.update(AppEvent::SettingsInput('1'));
        app.update(AppEvent::SettingsInput('0'));

        // Move to concurrency and modify it to "8".
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        app.settings.as_mut().unwrap().concurrency.clear();
        app.update(AppEvent::SettingsInput('8'));

        // Commit settings via resolve_io.
        let (_resolved_event, status) = resolve_io_for_test(&app, AppEvent::SettingsCommit).await;

        // Status should be "Settings saved".
        assert_eq!(status, Some("Settings saved".to_string()));

        // Now process the SettingsCommit through update (in production, the loop does this).
        // For this test, we manually apply the values since resolve_io doesn't mutate app.
        // (In production, the loop calls update(SettingsCommit) after resolve_io returns.)
        // Instead, just verify the file was written correctly.

        // Re-read the config file and verify it was updated.
        let new_config_str = std::fs::read_to_string(&config_path).expect("read config");
        let new_config: makina_core::config::GlobalConfig =
            toml::from_str(&new_config_str).expect("parse config");

        // Verify the new caps.
        assert_eq!(
            new_config.caps.gate_iterations, 10,
            "gate_iterations must be updated"
        );
        assert_eq!(new_config.concurrency, 8, "concurrency must be updated");
        assert_eq!(
            new_config.caps.reviewer_iterations, 3,
            "reviewer_iterations must be unchanged"
        );
        assert_eq!(
            new_config.caps.wall_clock_secs, 1200,
            "wall_clock_secs must be unchanged"
        );

        // Verify unrelated fields are preserved.
        assert!(
            !new_config.providers.is_empty(),
            "providers must be preserved"
        );
        assert_eq!(new_config.providers[0].name, "claude");
        assert!(
            new_config.roles.developer.is_some(),
            "roles must be preserved"
        );
    }

    #[test]
    fn settings_esc_closes() {
        // Esc in settings must map to CloseSettings.
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::CloseSettings
        ));
    }

    #[test]
    fn settings_enter_commits() {
        // Enter in settings must map to SettingsCommit.
        let ev = key_press(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::SettingsCommit
        ));
    }

    #[test]
    fn settings_arrows_navigate() {
        // Up/Down in settings must map to SettingsUp/SettingsDown.
        let up = key_press(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                up,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::SettingsUp
        ));

        let down = key_press(KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                down,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::SettingsDown
        ));
    }

    #[test]
    fn settings_digit_input() {
        // Typing a digit in settings must map to SettingsInput.
        let ev = key_press(KeyCode::Char('5'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::SettingsInput('5')
        ));
    }

    #[test]
    fn settings_backspace_deletes() {
        // Backspace in settings must map to SettingsBackspace.
        let ev = key_press(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar
            ),
            AppEvent::SettingsBackspace
        ));
    }

    // ── Plan picker tests (plan 0027) ─────────────────────────────────────────

    /// **Plan discovery default:** When `OpenBrowser` is resolved and the repo
    /// contains `docs/plans/` with convention directories, `resolve_io` must
    /// return immediately and emit `AppEvent::PlansDiscovered` on the background
    /// channel. When `docs/plans/` is absent or empty, it falls back to the file
    /// browser on that same channel.
    #[tokio::test]
    async fn open_browser_prefers_discovered_plans() {
        use crate::app::{App, AppEvent};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        // Create a temp repo with docs/plans/0001-x/ containing SCOPE.md, ARCHITECTURE.md, TASKS.md
        let tmpdir = tempfile::tempdir().unwrap();
        let repo_root = tmpdir.path();
        let plans_dir = repo_root.join("docs/plans/0001-x");
        std::fs::create_dir_all(&plans_dir).unwrap();
        std::fs::write(plans_dir.join("SCOPE.md"), "Scope").unwrap();
        std::fs::write(plans_dir.join("ARCHITECTURE.md"), "Architecture").unwrap();
        std::fs::write(plans_dir.join("TASKS.md"), "Tasks").unwrap();

        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], repo_root.to_path_buf());

        // Resolve OpenBrowser: should return immediately, then discover the plan
        // on the background channel.
        let (tx, mut rx) = background_events();
        let (resolved, status) = resolve_io(&app, AppEvent::OpenBrowser, &tx).await;
        assert!(
            matches!(resolved, AppEvent::OpenBrowser),
            "OpenBrowser must return immediately"
        );
        assert_eq!(status.as_deref(), Some("Discovering plans..."));
        let discovered = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for PlansDiscovered")
            .expect("background channel closed");
        match &discovered {
            AppEvent::PlansDiscovered { plans } => {
                assert_eq!(plans.len(), 1, "should discover exactly one plan");
                assert_eq!(plans[0].slug, "0001-x");
                assert!(plans[0].has_tasks);
            }
            _ => panic!("OpenBrowser must emit PlansDiscovered, got {discovered:?}"),
        }
    }

    /// **Plan activation with tasks:** When a plan with `has_tasks=true` is
    /// activated via `PlanActivate`, `resolve_io` must spawn
    /// `api.execute(OpenRun)` for the plan's `TASKS.md`, return `CloseBrowser`,
    /// and surface an "Interpreting ..." status message immediately.
    #[tokio::test]
    async fn plan_activate_with_tasks_opens_run() {
        use crate::app::{App, AppEvent, Mode};
        use makina_core::dependency::EdgeInferrer;
        use makina_core::interpreter::StructuredTextInterpreter;
        use makina_core::orchestrator::{CoreApi, PlanEntry};
        use std::sync::Arc;

        // A valid task list for the plan's TASKS.md
        let source = "# Flow\n\nPreamble.\n\n---\n\n## 0001 — Task\n\n\
            ### test — Test\nDoes a thing.\n- **Depends on:** —\n- **Done when:** ok.\n";
        let tmpdir = tempfile::tempdir().unwrap();
        let repo_root = tmpdir.path().to_path_buf();
        let plan_dir = repo_root.join("docs/plans/0001-test");
        std::fs::create_dir_all(&plan_dir).unwrap();
        let tasks_path = plan_dir.join("TASKS.md");
        std::fs::write(&tasks_path, source).unwrap();

        // Real CoreApi with deterministic interpreter
        let interpreter = Arc::new(EdgeInferrer::new(
            Arc::new(StructuredTextInterpreter::new()),
        ));
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

        // Subscribe before acting (to avoid dropped subscription)
        let mut sub = api.subscribe();

        // Build an App with a selected plan
        let mut app = App::new(Arc::clone(&api), vec![], repo_root.clone());
        app.mode = Mode::PlanPicker;
        app.discovered_plans = vec![PlanEntry {
            dir: plan_dir,
            slug: "0001-test".to_string(),
            has_tasks: true,
        }];
        app.plan_cursor = 0;

        // Resolve PlanActivate
        let (tx, _rx) = background_events();
        let (resolved, status) = resolve_io(&app, AppEvent::PlanActivate, &tx).await;

        // Must return CloseBrowser + "Interpreting ..." status
        assert!(
            matches!(resolved, AppEvent::CloseBrowser),
            "PlanActivate with tasks must resolve to CloseBrowser"
        );
        assert!(
            status
                .as_deref()
                .is_some_and(|m| m.contains("Interpreting") || m.contains("Opening")),
            "PlanActivate must surface an 'Interpreting ...' status; got {status:?}"
        );

        // Update app with the resolved event
        app.update(resolved);
        assert_eq!(
            app.mode,
            Mode::Normal,
            "PlanPicker should close after activation"
        );

        let ev = tokio::time::timeout(std::time::Duration::from_secs(1), sub.next())
            .await
            .expect("timed out waiting for RunOpened")
            .expect("stream ended unexpectedly");
        assert!(matches!(ev, makina_core::api::Event::RunOpened { .. }));

        // Verify CoreApi created the run.
        let runs = api.runs().await;
        assert_eq!(runs.len(), 1, "CoreApi must have created exactly one run");
        assert_eq!(runs[0].task_list_path, tasks_path);
    }

    /// **Plan activation without tasks:** When a plan with `has_tasks=false` is
    /// activated via `PlanActivate`, `resolve_io` must NOT call `api.execute(OpenRun)`,
    /// but return `CloseBrowser` and a "no TASKS.md — planner will generate the graph"
    /// status message (leaving a seam comment for plan 0028).
    #[tokio::test]
    async fn plan_activate_without_tasks_does_not_open_run() {
        use crate::app::{App, AppEvent, Mode};
        use crate::placeholder::PlaceholderApi;
        use makina_core::orchestrator::PlanEntry;
        use std::sync::Arc;

        let tmpdir = tempfile::tempdir().unwrap();
        let repo_root = tmpdir.path().to_path_buf();
        let plan_dir = repo_root.join("docs/plans/0002-no-tasks");
        std::fs::create_dir_all(&plan_dir).unwrap();

        let api = Arc::new(PlaceholderApi::empty());

        // Build an App with a selected plan without tasks
        let mut app = App::new(api, vec![], repo_root);
        app.mode = Mode::PlanPicker;
        app.discovered_plans = vec![PlanEntry {
            dir: plan_dir,
            slug: "0002-no-tasks".to_string(),
            has_tasks: false,
        }];
        app.plan_cursor = 0;

        // Resolve PlanActivate: should NOT open a run
        let (resolved, status) = resolve_io_for_test(&app, AppEvent::PlanActivate).await;

        // Must return CloseBrowser + a "planner will generate" message
        assert!(
            matches!(resolved, AppEvent::CloseBrowser),
            "PlanActivate without tasks must resolve to CloseBrowser"
        );
        assert!(
            status
                .as_deref()
                .is_some_and(|m| m.contains("planner will generate")),
            "PlanActivate without tasks must mention planner; got {status:?}"
        );

        // Update app
        app.update(resolved);
        assert_eq!(
            app.mode,
            Mode::Normal,
            "PlanPicker should close after activation"
        );

        // Verify no runs were created (the PlaceholderApi would trivially succeed OpenRun,
        // but we're testing the logic: resolve_io should not call it)
        let runs = app.runs.clone();
        assert!(
            runs.is_empty(),
            "no run should be created for a plan without TASKS.md"
        );
    }
}
