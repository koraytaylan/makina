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
    Event as CrosstermEvent, EventStream, KeyCode, KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use makina_core::api::CommandOutcome;
use makina_core::log_record::LogRecord;
use tokio::sync::mpsc;
use tokio::time;

use crate::app::{App, AppEvent, ErrorLevel, ErrorMessage, PlanIdentity, ResetConfirmation};
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

    // Auto-discover plans under the repo on startup so the plan picker populates
    // without the user pressing `[o]`. The scan runs in the background; the
    // OpenBrowser arm sets `app.busy`, which renders a spinner until
    // PlansDiscoveredPerFolder arrives. `fallback_to_browser = false` keeps startup
    // unobtrusive: a repo with no `docs/plans/` just stays on the normal view.
    app.update(AppEvent::OpenBrowser);
    spawn_discover_plans(app.opened_folders.clone(), background_tx.clone(), false);

    // Initial render.
    tui.draw(|frame| ui::render(app, frame))?;

    loop {
        let modal = ModalState {
            browsing: app.is_browsing(),
            viewing_doctor: app.is_viewing_doctor(),
            help_mode_active: app.help_mode_active,
            command_palette: app.is_command_palette(),
            settings: app.is_settings(),
            reset_confirm: app.is_confirming_reset(),
            operation_notice: app.is_operation_notice(),
        };
        let app_event: Option<AppEvent> = tokio::select! {
            // Bias toward terminal input (lower latency for keystrokes).
            biased;

            maybe_term = term_rx.recv() => {
                maybe_term.map(|ev| {
                    // Check if a plan tab is currently active
                    let plan_tab_active = app.tabs.active_tab
                        .and_then(|idx| app.tabs.open_tabs.get(idx))
                        .is_some_and(|content| matches!(content, crate::app::TabContent::Plan { .. }));
                    translate_terminal_event(ev, modal, app.focused_panel, plan_tab_active, app)
                })

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
            // A finalised mouse selection copies the highlighted screen text to
            // the system clipboard. Handled here because it needs the rendered
            // buffer, which lives in `tui` (and which `App::update` cannot see).
            if matches!(event, AppEvent::SelectionEnd(_, _)) {
                app.update(event); // finalise selection (clears on a plain click)
                // Render the finalised frame (with highlight) and read its
                // buffer. The block scopes the `&tui` borrow so the clipboard
                // write below can take `&mut tui`.
                let copied = {
                    let frame = tui.draw(|frame| ui::render(app, frame))?;
                    app.selection.and_then(|sel| sel.extract(frame.buffer))
                };
                if let Some(text) = copied {
                    let n = text.chars().count();
                    tui.copy_to_clipboard(&text)?;
                    app.update(AppEvent::StatusMessage(format!(
                        "Copied {n} char{} to clipboard",
                        if n == 1 { "" } else { "s" }
                    )));
                    tui.draw(|frame| ui::render(app, frame))?;
                }
                continue;
            }

            // IO-layer resolution: browser intents (open / enter dir / select
            // file) need filesystem reads or an async `execute`; run controls
            // (start/pause/cancel/reset) need an async `execute` against the selected
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
/// first opened the event carries only the `RunId` and `plan_dir` —
/// not the authored task documents. So we immediately call `api.run(id).await` to fetch
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
///   "Interpreting …" status, spawn `execute(OpenPlan)`, and close the browser
///   immediately; `BrowserParent` → spawn parent read.
/// - **Run control** (task 31): `StartRun`/`PauseRun`/`CancelRun`/`Reinterpret` →
///   `execute(...)` for `app.selected_run()` (outcome/error → status message);
///   the run-state changes themselves flow back via `api.subscribe()`.
///
/// Non-IO events pass straight through with no status message.
async fn resolve_io(
    app: &mut App,
    event: AppEvent,
    background_tx: &mpsc::Sender<AppEvent>,
) -> (AppEvent, Option<String>) {
    match event {
        AppEvent::OpenBrowser => {
            // Manual `[o]`: discover plans, falling back to the file browser when
            // none are found. The OpenBrowser update arm sets the busy spinner;
            // no separate status message is needed.
            spawn_discover_plans(app.opened_folders.clone(), background_tx.clone(), true);
            (AppEvent::OpenBrowser, None)
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
                    // perform execute(OpenPlan). The RunLoaded event will populate
                    // the panes when the core open completes.
                    let stem = entry
                        .path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("plan");
                    let status = format!("Interpreting {}...", stem);
                    let project_root = app.canonical_repo_root();
                    let plan_path = entry.path.parent().unwrap_or(&entry.path);
                    if let Ok(relative) = plan_path.strip_prefix(&project_root)
                        && let Ok(plan_dir) = makina_core::plan::PlanKey::parse(relative)
                    {
                        spawn_open_run(
                            std::sync::Arc::clone(&app.api),
                            app.project_api.clone(),
                            project_root,
                            plan_dir,
                            background_tx.clone(),
                            false,
                            None,
                        );
                    }
                    (AppEvent::CloseBrowser, Some(status))
                }
                // No selection (empty dir) — ignore.
                None => (AppEvent::Tick, None),
            }
        }
        // ── Folder browser (plan 0043) ──────────────────────────────────────────
        // Open the folder browser modal at $HOME to let the user pick a folder.
        AppEvent::OpenFolder => {
            let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            spawn_read_dir_folders_only(home, background_tx.clone());
            (AppEvent::OpenFolder, None)
        }
        // Same directory-read spawn as `OpenFolder`, but for the "Initialize
        // Folder" palette action; `App::update`'s `InitializeFolderRequested`
        // arm sets `folder_browser_purpose` so `BrowserOpened` (below) lands in
        // `Mode::FolderBrowser { purpose: InitializeFolder }` instead of `OpenFolder`.
        AppEvent::InitializeFolderRequested => {
            let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
            spawn_read_dir_folders_only(home, background_tx.clone());
            (AppEvent::InitializeFolderRequested, None)
        }
        // ── Folder browser activation (plan 0043) ────────────────────────────────
        // When the user presses Enter on a folder in the FolderBrowser modal,
        // emit the appropriate event based on the browser's purpose.
        AppEvent::FolderBrowserActivate { purpose } => {
            use crate::app::FolderBrowserPurpose;
            match app.browser.as_ref().and_then(|b| b.selected_entry()) {
                Some(entry) if entry.is_dir => {
                    let event = match purpose {
                        FolderBrowserPurpose::OpenFolder => AppEvent::OpenFolderSelected {
                            path: entry.path.clone(),
                        },
                        FolderBrowserPurpose::InitializeFolder => {
                            AppEvent::InitializeFolderSelected {
                                path: entry.path.clone(),
                            }
                        }
                        FolderBrowserPurpose::CloseFolders => AppEvent::CloseFolderConfirmed {
                            path: entry.path.clone(),
                        },
                    };
                    let _ = background_tx.send(event).await;
                    (AppEvent::CloseBrowser, None)
                }
                // Entry is not a directory, or no selection — ignore.
                _ => (AppEvent::Tick, None),
            }
        }
        // ── Folder browser navigation (plan 0043) ────────────────────────────────
        // These are handled in app.update (for navigation) or spawned in resolve_io
        // (for parent reads). They pass through here without special handling.
        AppEvent::FolderBrowserUp | AppEvent::FolderBrowserDown => (event, None),
        AppEvent::FolderBrowserParent => match app.browser.as_ref().and_then(|b| b.parent()) {
            Some(parent) => {
                spawn_read_dir_folders_only(parent.to_path_buf(), background_tx.clone());
                (AppEvent::Tick, None)
            }
            None => (AppEvent::Tick, None),
        },
        // ── Folder selection (plan 0043) ────────────────────────────────────────
        // `FolderBrowserActivate` above already closes the modal (→ Mode::Normal)
        // in the SAME resolve_io pass and re-dispatches the selected path via
        // `background_tx`, so these arrive here on the NEXT loop iteration.
        AppEvent::OpenFolderSelected { path } => {
            // Verify it's a valid directory.
            if !path.is_dir() {
                app.push_error(ErrorMessage {
                    timestamp: std::time::SystemTime::now(),
                    level: ErrorLevel::Error,
                    text: format!("Selected path is not a directory: {}", path.display()),
                });
                app.mode = crate::app::Mode::Normal;
                return (AppEvent::Tick, None);
            }

            let path = match register_project(app, &path).await {
                Ok(path) => path,
                Err(message) => {
                    app.push_error(ErrorMessage {
                        timestamp: std::time::SystemTime::now(),
                        level: ErrorLevel::Error,
                        text: message.clone(),
                    });
                    app.mode = crate::app::Mode::Normal;
                    return (AppEvent::Tick, Some(message));
                }
            };

            // Add to opened_folders if not already present.
            if !app.opened_folders.contains(&path) {
                app.opened_folders.push(path.clone());
            }

            // Update workspace and save.
            app.workspace.add_folder(path.clone());
            if let Err(e) = app.save_workspace() {
                app.push_error(ErrorMessage {
                    timestamp: std::time::SystemTime::now(),
                    level: ErrorLevel::Error,
                    text: format!("Failed to save workspace: {e}"),
                });
            }

            // Trigger plan discovery for all opened folders.
            spawn_discover_plans(app.opened_folders.clone(), background_tx.clone(), false);

            // Return to Normal mode and set status message.
            app.mode = crate::app::Mode::Normal;
            (
                AppEvent::Tick,
                Some(format!("Folder opened: {}", path.display())),
            )
        }
        AppEvent::InitializeFolderSelected { path } => {
            // Call folder_init::initialize_folder() to set up git, branches, and docs/plans.
            match crate::folder_init::initialize_folder(&path) {
                Ok(_) => {
                    let path = match register_project(app, &path).await {
                        Ok(path) => path,
                        Err(message) => {
                            app.push_error(ErrorMessage {
                                timestamp: std::time::SystemTime::now(),
                                level: ErrorLevel::Error,
                                text: message.clone(),
                            });
                            app.mode = crate::app::Mode::Normal;
                            return (AppEvent::Tick, Some(message));
                        }
                    };
                    // Add to opened_folders if not already present.
                    if !app.opened_folders.contains(&path) {
                        app.opened_folders.push(path.clone());
                    }

                    // Update workspace and save.
                    app.workspace.add_folder(path.clone());
                    if let Err(e) = app.save_workspace() {
                        app.push_error(ErrorMessage {
                            timestamp: std::time::SystemTime::now(),
                            level: ErrorLevel::Error,
                            text: format!("Failed to save workspace: {e}"),
                        });
                    }

                    // Trigger plan discovery for all opened folders.
                    spawn_discover_plans(app.opened_folders.clone(), background_tx.clone(), false);

                    // Return to Normal mode and set status message.
                    app.mode = crate::app::Mode::Normal;
                    (
                        AppEvent::Tick,
                        Some(format!("Folder initialized: {}", path.display())),
                    )
                }
                Err(e) => {
                    // Return to Normal mode and push an error message.
                    app.push_error(ErrorMessage {
                        timestamp: std::time::SystemTime::now(),
                        level: ErrorLevel::Error,
                        text: format!("Failed to initialize folder: {e}"),
                    });
                    app.mode = crate::app::Mode::Normal;
                    (AppEvent::Tick, None)
                }
            }
        }
        // Close the folder highlighted in the sidebar tree. `focused_node()`
        // resolves the folder for a folder header or any plan/task under one,
        // so "Close Folder" acts on wherever the cursor is without a prompt.
        //
        // Only when the cursor isn't on a folder-scoped node do we fall back to
        // the selectable list: the opened folders are shown in the folder-browser
        // modal (purpose `CloseFolders`) so the user can pick one. Unlike
        // `OpenFolder` / `InitializeFolderRequested` there is no directory read
        // here — the list IS `app.opened_folders`, already in memory — so we
        // build the `FileBrowser` and flip the mode directly instead of
        // round-tripping through `BrowserOpened`.
        AppEvent::CloseFolderRequested => {
            if let Some(folder_idx) = app.focused_node().and_then(|n| n.folder_idx())
                && let Some(path) = app.opened_folders.get(folder_idx).cloned()
            {
                return Box::pin(resolve_io(
                    app,
                    AppEvent::CloseFolderConfirmed { path },
                    background_tx,
                ))
                .await;
            }
            let entries: Vec<crate::browser::DirEntry> = app
                .opened_folders
                .iter()
                .map(|p| crate::browser::DirEntry {
                    name: p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| p.display().to_string()),
                    path: p.clone(),
                    is_dir: true,
                })
                .collect();
            app.browser = Some(crate::browser::FileBrowser::new(
                app.repo_root.clone(),
                entries,
            ));
            app.folder_browser_purpose = Some(crate::app::FolderBrowserPurpose::CloseFolders);
            app.mode = crate::app::Mode::FolderBrowser {
                purpose: crate::app::FolderBrowserPurpose::CloseFolders,
            };
            (AppEvent::Tick, None)
        }
        AppEvent::CloseFolderConfirmed { path } => {
            if let Err(error) = app
                .api
                .execute(makina_core::api::Command::UnregisterProject {
                    project_root: path.clone(),
                })
                .await
            {
                let message = format!("Could not close project {}: {error}", path.display());
                app.push_error(ErrorMessage {
                    timestamp: std::time::SystemTime::now(),
                    level: ErrorLevel::Error,
                    text: message.clone(),
                });
                app.mode = crate::app::Mode::Normal;
                return (AppEvent::Tick, Some(message));
            }

            // Remove and rekey all folder-indexed state synchronously, before
            // rediscovery can leave a frame where B is indexed as A.
            app.remove_opened_folder(&path);

            // Update workspace and save.
            app.workspace.remove_folder(&path);
            if let Err(e) = app.save_workspace() {
                app.push_error(ErrorMessage {
                    timestamp: std::time::SystemTime::now(),
                    level: ErrorLevel::Error,
                    text: format!("Failed to save workspace: {e}"),
                });
            }

            // Trigger plan discovery to refresh the sidebar.
            spawn_discover_plans(app.opened_folders.clone(), background_tx.clone(), false);

            // Return to Normal mode and set status message.
            app.mode = crate::app::Mode::Normal;
            (
                AppEvent::Tick,
                Some(format!("Folder closed: {}", path.display())),
            )
        }
        // ── Run control (task 31) ─────────────────────────────────────────────
        // Start has an extra responsibility (plan 0042 follow-up): when there is
        // no open Run for the current context but the user is in a discovered
        // plan, `Start` opens that plan directory as a new Run and auto-starts
        // it — so a plan can be launched from its tab/node without the [o] file
        // browser. When a Run already exists it just starts/resumes it.
        //
        // Historical snapshots are queryable through `run(id)` too, so liveness
        // is determined by the mutating command: only UnknownRun falls back to
        // opening the snapshot's immutable plan-directory target.
        AppEvent::GeneratePlanBundle {
            project_root,
            blueprint,
        } => {
            let label = blueprint.slug.clone();
            spawn_generate_plan_bundle(
                std::sync::Arc::clone(&app.api),
                app.project_api.clone(),
                project_root,
                blueprint,
                app.opened_folders.clone(),
                background_tx.clone(),
            );
            (AppEvent::Tick, Some(format!("Generating {label}…")))
        }
        AppEvent::StartRun => {
            if let Some(event) = operation_blocked_event(app, "Start run") {
                return (event, None);
            }
            let active_run = app.active_run_id();
            if let Some(run) = active_run {
                match app
                    .api
                    .execute(makina_core::api::Command::StartRun { run })
                    .await
                {
                    Ok(_) => {
                        // Mark the run as "starting" so the sidebar renders an
                        // animated badge while the run is still Pending (before
                        // RepositoryLeaseWaiting/Running arrives). Cleared when
                        // the run transitions out of Pending.
                        app.starting_runs.insert(run);
                        return (AppEvent::Tick, Some(format!("Start {run}")));
                    }
                    Err(makina_core::api::ApiError::UnknownRun { .. })
                        if app
                            .runs
                            .iter()
                            .find(|view| view.id == run)
                            .is_some_and(|view| is_plan_plan_dir(&view.plan_dir.relative_dir)) => {}
                    Err(error) => {
                        return (AppEvent::Tick, Some(format!("Start failed: {error}")));
                    }
                }
            }

            match resolve_plan_to_open(app) {
                Some(PlanOpen { target, label }) => {
                    spawn_open_run(
                        std::sync::Arc::clone(&app.api),
                        app.project_api.clone(),
                        target.project_root.clone(),
                        target.plan_dir.clone(),
                        background_tx.clone(),
                        true,
                        Some(target.clone()),
                    );
                    (
                        AppEvent::PlanOpenStarted { target },
                        Some(format!("Starting {label}…")),
                    )
                }
                // No run and no plan context: surface the standard hint.
                None => (AppEvent::Tick, run_control(app, ControlKind::Start).await),
            }
        }
        AppEvent::PauseRun => match operation_blocked_event(app, "Pause run") {
            Some(event) => (event, None),
            None => (AppEvent::Tick, run_control(app, ControlKind::Pause).await),
        },
        AppEvent::CancelRun => match operation_blocked_event(app, "Stop run") {
            Some(event) => (event, None),
            None => (AppEvent::Tick, run_control(app, ControlKind::Cancel).await),
        },
        AppEvent::Reinterpret => match operation_blocked_event(app, "Reinterpret run") {
            Some(event) => (event, None),
            None => (
                AppEvent::Tick,
                run_control(app, ControlKind::Reinterpret).await,
            ),
        },
        // ── Context-sensitive retry (plan 0017) ───────────────────────────────
        AppEvent::RetryFocused => match operation_blocked_event(app, "Reset/retry focused task") {
            Some(event) => (event, None),
            None => (AppEvent::Tick, retry_focused(app).await),
        },
        AppEvent::ResetRun { confirmation } => {
            start_reset_confirmed(app, confirmation, background_tx).await
        }
        AppEvent::PurgeWorktrees => (AppEvent::Tick, purge_worktrees(app).await),

        // ── Settings commit (plan 0070) ──────────────────────────────────────
        // Write the edited caps, concurrency, and finalization mode back to the config file
        // (`{repo_root}/.makina/config.toml`). Validates all fields first;
        // on any error, returns the reason without writing. The writer preserves
        // project fields and updates CoreApi's live runtime settings; App::update
        // then closes the modal and applies the same values to local TUI state.
        AppEvent::SettingsCommit => commit_settings(app).await,
        // ── Agent model probe ─────────────────────────────────────────────────
        // Spawn a throwaway session to discover available models, then send
        // ModelsDiscovered back so the Settings modal can populate model fields.
        AppEvent::OpenSettings => {
            if let Some(backend) = app.developer_backend.clone() {
                let tx = background_tx.clone();
                let agent_name = app
                    .providers
                    .first()
                    .map(|p| p.command.clone())
                    .unwrap_or_default();
                tokio::spawn(async move {
                    use makina_core::backend::{AgentBackend, SessionConfig};
                    let config = SessionConfig {
                        working_dir: std::path::PathBuf::from("/"),
                        system_prompt: String::new(),
                        mode: None,
                        model: None,
                        effort: None,
                        extra: None,
                        task_id: None,
                        run_id: String::new(),
                    };
                    match backend.spawn(config).await {
                        Ok(mut session) => {
                            let models: Vec<String> = session
                                .capabilities()
                                .and_then(|c| {
                                    c.config_options.iter().find(|o| o.category == "model").map(
                                        |o| {
                                            o.options
                                                .iter()
                                                .map(|c| format!("{agent_name}/{}", c.value))
                                                .collect()
                                        },
                                    )
                                })
                                .unwrap_or_default();
                            let _ = session.terminate().await;
                            let _ = tx.send(AppEvent::ModelsDiscovered { models }).await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(AppEvent::StatusMessage(format!("Model probe failed: {e}")))
                                .await;
                        }
                    }
                });
            }
            (AppEvent::OpenSettings, None)
        }
        // ── Doctor scaffold (task 0046) ──────────────────────────────────────
        // Write starter config templates to both config paths if neither exists.
        // Never overwrite existing files; re-check and refuse if present.
        AppEvent::DoctorWriteScaffold => {
            let status = write_doctor_scaffold(app).await;
            (AppEvent::Tick, status)
        }
        // ── Command palette execute (task command-palette-keys) ──────────────────
        // Extract the selected action's event from the palette before App::update
        // clears it. Regular actions are re-dispatched through background_tx so
        // IO-backed events receive a full resolve_io pass, and the current pass
        // closes the palette.
        // Theme selector Enter is handled in App::update with mutable access.
        AppEvent::CommandPaletteExecute => {
            if let Some(palette) = app.command_palette.as_ref() {
                if palette.theme_selector.is_some() {
                    // In theme selector mode: apply the selection
                    return (AppEvent::ApplyThemeSelection, None);
                }

                // Normal action mode: extract the selected action
                let filtered = palette.filtered();
                if let Some(action) = filtered.get(palette.selected) {
                    return match action {
                        crate::app::PaletteAction::Regular { event, .. } => {
                            let _ = background_tx.send(event.as_ref().clone()).await;
                            (AppEvent::CloseCommandPalette, None)
                        }
                        crate::app::PaletteAction::NestedThemeSelector { .. } => {
                            // Signal to App::update to enter theme selector mode
                            (AppEvent::EnterThemeSelector, None)
                        }
                    };
                }
            }
            // No valid selection (shouldn't happen) — just tick.
            (AppEvent::Tick, None)
        }
        // ── Project discovery (plan 0025) ──────────────────────────────────────
        // Force re-run discovery regardless of the [discovery] stamp, re-scan,
        // replace discovered gates, update last_run timestamp.
        AppEvent::DiscoverProject => {
            let status = discover_project(app).await;
            (AppEvent::Tick, status)
        }
        // OpenFocusedNode is handled directly in App::update (see OpenTreeRow for symmetry).
        // It requires mutable sidebar/ tab mutations (expand, cursor, open_tab) so it
        // no longer transforms in resolve_io; it passes through and update performs
        // the opens + (for Plan nodes) the sidebar expansion.
        // ── Theme selection commit (plan 0036) ──────────────────────────────────
        // When the user selects a theme in the nested palette selector, persist
        // the chosen theme name to `{repo_root}/.makina/config.toml` before
        // `App::update` applies it in memory.  The selected name is read from
        // the palette's current filtered selection; if no valid name is found
        // nothing is written and we fall through with no status.
        AppEvent::ApplyThemeSelection => {
            let theme_name = app.command_palette.as_ref().and_then(|palette| {
                let names = palette.filtered_theme_names();
                names.get(palette.selected).map(|s| (*s).clone())
            });
            let status = if let Some(name) = theme_name {
                commit_theme_selection(app, &name).await
            } else {
                None
            };
            (AppEvent::ApplyThemeSelection, status)
        }
        // Everything else passes straight through.
        other => (other, None),
    }
}

/// Test seam: exposes the private [`resolve_io`] to downstream integration
/// tests (e.g. `crates/makina/tests/multi_folder_integration_test.rs`) so they
/// can drive real `AppEvent`s (`OpenFolderSelected`, `CloseFolderConfirmed`,
/// `InitializeFolderSelected`, …) through the *actual* IO-resolution +
/// `App::update` wiring instead of mutating `App` fields directly.
///
/// Gated behind the `test-util` feature (off by default; production builds
/// never compile this). A throwaway `mpsc` channel stands in for the real
/// event loop's `background_tx` — fine for tests that don't assert on
/// background-spawned follow-up events themselves (those are covered by the
/// `resolve_io_*` unit tests in this module's own `#[cfg(test)]` block).
#[cfg(any(test, feature = "test-util"))]
pub async fn resolve_io_for_test(app: &mut App, event: AppEvent) -> (AppEvent, Option<String>) {
    let (tx, _rx) = mpsc::channel(64);
    resolve_io(app, event, &tx).await
}

/// Scan every folder in `opened_folders` for plans off the render loop and
/// feed the result back as an [`AppEvent`].
///
/// The blocking directory walk runs on `spawn_blocking` so the UI stays
/// responsive (and a spinner can animate) while it runs. The result is an
/// [`AppEvent::PlansDiscoveredPerFolder`] mapping each folder's index to its
/// discovered plans.
///
/// `fallback_to_browser` controls the *no plans found anywhere* case (i.e.
/// every opened folder's plan list is empty, or no folders are opened):
/// - `true` (the `[o]` keypress): fall back to the CWD file browser so the user
///   can still navigate to an arbitrary plan directory.
/// - `false` (startup auto-discovery, and the open/close/initialize folder
///   handlers): emit `PlansDiscoveredPerFolder` regardless so the busy state
///   clears and the sidebar reflects the (possibly empty) result — no popup.
fn spawn_discover_plans(
    opened_folders: Vec<std::path::PathBuf>,
    background_tx: mpsc::Sender<AppEvent>,
    fallback_to_browser: bool,
) {
    tokio::spawn(async move {
        let roots = opened_folders;
        let scan_roots = roots.clone();
        let plans_map = tokio::task::spawn_blocking(move || {
            makina_core::orchestrator::discover_plans_per_folder(&scan_roots)
        })
        .await
        .unwrap_or_default();

        // `plans_map` always has one entry per opened folder (even an empty
        // Vec when that folder has no plans), so checking `plans_map.is_empty()`
        // only catches the zero-folders-opened case. Fall back to the browser
        // when every folder's plan list is empty too.
        let no_plans_found = plans_map.values().all(|plans| plans.is_empty());
        let event = if no_plans_found && fallback_to_browser {
            let start = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            read_dir_event(&start).await
        } else {
            AppEvent::PlansDiscoveredPerFolder { roots, plans_map }
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
    project_api: Option<std::sync::Arc<crate::project_api::ProjectApiRouter>>,
    project_root: std::path::PathBuf,
    plan_dir: makina_core::plan::PlanKey,
    background_tx: mpsc::Sender<AppEvent>,
    auto_start: bool,
    target: Option<PlanIdentity>,
) {
    use makina_core::api::{Command, CommandOutcome};
    tokio::spawn(async move {
        let result = if let Some(router) = project_api {
            router.open_plan(&project_root, plan_dir).await
        } else {
            api.execute(Command::OpenPlan { plan_dir }).await
        };
        let opened = match result {
            // The new Run is created Pending; when the user's intent was to
            // *start* the plan (not just open it), immediately issue StartRun on
            // the freshly-opened run. The RunOpened/RunStatusChanged events flow
            // back through subscribe() to update the UI.
            Ok(CommandOutcome::RunOpened { run }) if auto_start => {
                match api.execute(Command::StartRun { run }).await {
                    Ok(_) => {
                        // Notify the App to render an animated "starting"
                        // badge on this run while it is still Pending.
                        let _ = background_tx.send(AppEvent::RunStarting { run }).await;
                        true
                    }
                    Err(e) => {
                        let _ = background_tx
                            .send(AppEvent::StatusMessage(format!("Start failed: {e}")))
                            .await;
                        true
                    }
                }
            }
            Ok(CommandOutcome::RunOpened { .. }) => true,
            Ok(_) => false,
            Err(e) => {
                let _ = background_tx
                    .send(AppEvent::StatusMessage(format!("Open failed: {e}")))
                    .await;
                false
            }
        };
        // A successful OpenPlan stays guarded until RunOpened/RunLoaded reaches
        // App state. Clearing here recreates the interaction window where a
        // second Start can open the same plan before the subscription event is
        // processed. Failed/unexpected opens have no event, so clear those.
        if !opened && let Some(target) = target {
            let _ = background_tx
                .send(AppEvent::PlanOpenFinished { target })
                .await;
        }
    });
}

fn spawn_generate_plan_bundle(
    api: std::sync::Arc<dyn makina_core::api::Api>,
    project_api: Option<std::sync::Arc<crate::project_api::ProjectApiRouter>>,
    project_root: std::path::PathBuf,
    blueprint: makina_core::api::GeneratedPlanBlueprint,
    opened_folders: Vec<std::path::PathBuf>,
    background_tx: mpsc::Sender<AppEvent>,
) {
    use makina_core::api::{Command, CommandOutcome};
    tokio::spawn(async move {
        let result = if let Some(router) = project_api.as_ref() {
            router
                .generate_plan_bundle(&project_root, blueprint.clone())
                .await
        } else {
            api.execute(Command::GeneratePlanBundle {
                blueprint: blueprint.clone(),
            })
            .await
        };
        match result {
            Ok(CommandOutcome::PlanGenerated { plan_dir, .. }) => {
                spawn_discover_plans(opened_folders, background_tx.clone(), false);
                let _ = background_tx
                    .send(AppEvent::StatusMessage(format!(
                        "Generated {}; select the registered plan to open or start it",
                        plan_dir.relative_dir.display()
                    )))
                    .await;
            }
            Ok(other) => {
                let _ = background_tx
                    .send(AppEvent::StatusMessage(format!(
                        "Generate failed: unexpected outcome {other:?}"
                    )))
                    .await;
            }
            Err(error) => {
                let _ = background_tx
                    .send(AppEvent::StatusMessage(format!("Generate failed: {error}")))
                    .await;
            }
        }
    });
}

fn is_plan_plan_dir(path: &std::path::Path) -> bool {
    path.parent()
        .and_then(std::path::Path::file_name)
        .and_then(|name| name.to_str())
        == Some("plans")
        && path
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::file_name)
            .and_then(|name| name.to_str())
            == Some("docs")
}

/// What a `Start` press should do for the plan the user is currently in, when
/// no Run exists for it yet (see [`resolve_plan_to_open`]).
struct PlanOpen {
    target: PlanIdentity,
    label: String,
}

/// Resolve the [`App::context_plan`](crate::app::App::context_plan) into a
/// startable target. Returns `None` when there is no plan context at all.
///
/// Only consulted when no run is open for the context (the caller checks
/// [`App::active_run_id`](crate::app::App::active_run_id) first), so this never
/// opens a duplicate Run for a plan that is already running.
fn resolve_plan_to_open(app: &App) -> Option<PlanOpen> {
    let target = app
        .context_plan_identity()
        .or_else(|| app.selected_run().map(|run| app.plan_identity_for_run(run)))?;
    Some(PlanOpen {
        label: target.slug.clone(),
        target,
    })
}

/// Commit edited settings (caps, concurrency, and final merge mode) to the project config file.
///
/// Uses the project-config writer so repository-specific fields (`base_branch`,
/// `[[gates]]`, discovery stamps, and role prompts) survive the round-trip. On a
/// successful write it also updates the live CoreApi runtime settings so a
/// restart is not required before the next scheduler run observes the new mode.
async fn commit_settings(app: &App) -> (AppEvent, Option<String>) {
    use crate::settings_validation::validate_settings;
    use makina_core::api::Command;
    use makina_core::config::{CapsOverride, MergeConfig, write_project_config};

    let Some(settings) = app.settings.as_ref() else {
        let reason = "Settings are not open".to_string();
        return (
            AppEvent::SettingsSaveFailed {
                reason: reason.clone(),
            },
            Some(reason),
        );
    };
    let project_root = settings.project_root.clone();

    // Parse and validate every field using the shared validator.
    let valid = match validate_settings(settings) {
        Ok(v) => v,
        Err(reason) => {
            return (
                AppEvent::SettingsSaveFailed {
                    reason: reason.clone(),
                },
                Some(reason),
            );
        }
    };

    let caps = makina_core::config::CapsConfig {
        gate_iterations: valid.gate_iterations,
        reviewer_iterations: valid.reviewer_iterations,
        wall_clock_secs: valid.wall_clock_secs,
        idle_secs: valid.idle_secs,
    };

    if let Err(e) = write_project_config(&project_root, |cfg| {
        cfg.caps = Some(CapsOverride {
            gate_iterations: Some(valid.gate_iterations),
            reviewer_iterations: Some(valid.reviewer_iterations),
            wall_clock_secs: Some(valid.wall_clock_secs),
            idle_secs: valid.idle_secs.map(Some),
        });
        cfg.concurrency = Some(valid.concurrency);
        cfg.merge = Some(MergeConfig {
            final_: valid.final_merge,
        });
        // Write model selections for each role.
        if let Some(settings) = app.settings.as_ref() {
            let resolve = |s: &str| {
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            };
            if let Some(model) = resolve(&settings.developer_model) {
                cfg.roles
                    .developer
                    .get_or_insert_with(Default::default)
                    .model = Some(model);
            }
            if let Some(model) = resolve(&settings.reviewer_model) {
                cfg.roles
                    .reviewer
                    .get_or_insert_with(Default::default)
                    .model = Some(model);
            }
            if let Some(model) = resolve(&settings.planner_model) {
                cfg.roles.planner.get_or_insert_with(Default::default).model = Some(model);
            }
        }
    })
    .await
    {
        let reason = format!("Config write error: {e}");
        return (
            AppEvent::SettingsSaveFailed {
                reason: reason.clone(),
            },
            Some(reason),
        );
    }

    let status = match app
        .api
        .execute(Command::UpdateRuntimeSettings {
            project_root: project_root.clone(),
            caps,
            concurrency: valid.concurrency,
            final_merge: valid.final_merge,
        })
        .await
    {
        Ok(_) => "Settings saved".to_string(),
        Err(e) => format!("Settings saved; runtime update failed: {e}"),
    };
    (
        AppEvent::SettingsSaved {
            project_root,
            values: valid,
        },
        Some(status),
    )
}

/// Persist the chosen theme name to `{repo_root}/.makina/config.toml`.
///
/// Follows the same merge-preserving recipe as `commit_settings`: read the
/// current on-disk `GlobalConfig` (default on miss), rebuild it with only
/// `theme_name` replaced, and write it back — so providers, roles, caps, and
/// all other fields round-trip untouched.
///
/// Returns `Some("Theme saved")` on success, `Some(err)` on any error, and
/// `Some("Unknown theme")` if `theme_name` is not one of the built-in themes.
async fn commit_theme_selection(app: &App, theme_name: &str) -> Option<String> {
    use makina_core::config::GlobalConfig;
    use makina_core::paths::config_file;

    // Validate: only names that appear in builtin_themes() are accepted.
    let known = crate::theme::Theme::builtin_themes()
        .into_iter()
        .any(|t| t.name == theme_name);
    if !known {
        return Some("Unknown theme".to_string());
    }

    // Read the current on-disk config so we don't lose providers, roles, caps,
    // or any other fields.  On read failure start from a default.
    let config_path = config_file(&app.repo_root);
    let existing: GlobalConfig = if config_path.exists() {
        match tokio::fs::read_to_string(&config_path).await {
            Ok(s) => toml::from_str::<GlobalConfig>(&s).unwrap_or_default(),
            Err(_) => GlobalConfig::default(),
        }
    } else {
        GlobalConfig::default()
    };

    // Rebuild with only theme_name replaced; everything else is preserved.
    let updated = GlobalConfig {
        theme_name: theme_name.to_string(),
        ..existing
    };

    // Serialise to TOML.
    let toml_str = match toml::to_string_pretty(&updated) {
        Ok(s) => s,
        Err(e) => return Some(format!("Config serialise error: {e}")),
    };

    // Ensure the parent directory exists.
    if let Some(parent) = config_path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return Some(format!("Config write error: {e}"));
    }

    // Write the file.
    match tokio::fs::write(&config_path, toml_str).await {
        Ok(()) => Some("Theme saved".to_string()),
        Err(e) => Some(format!("Config write error: {e}")),
    }
}

/// Build the `~/.makina/config.toml` starter template text from a detection
/// result. When `detected` is `Some`, the template contains an uncommented
/// `[backend]` section whose `command` is the detected CLI (a config that
/// validates immediately). When `detected` is `None`, `command` stays
/// commented out and every `KNOWN_AGENTS` CLI is named in a comment so the
/// user can pick one after installing it.
///
/// Pure (no IO) so it can be exercised deterministically in tests without
/// controlling the process `$PATH`.
fn build_global_template(detected: &Option<makina_core::preflight::DetectedBackend>) -> String {
    match detected {
        Some(d) => format!(
            "# Makina global configuration — machine-specific, not committed.\n\
             # Auto-detected backend: {} (found at {}).\n\n\
             [backend]\ncommand = \"{}\"\nargs = {:?}\n\n\
             [planner]\nmechanism = \"one-shot-agent\"\n",
            d.agent,
            d.resolved.display(),
            d.command,
            d.args,
        ),
        None => {
            let supported: Vec<&str> = makina_core::preflight::KNOWN_AGENTS
                .iter()
                .map(|a| a.command)
                .collect();
            format!(
                "# Makina global configuration — machine-specific, not committed.\n\
                 # No supported agent CLI was found on PATH. Install one of: {}\n\
                 # then set [backend].command below (see docs/trial/e2e-run.md).\n\n\
                 [backend]\n# command = \"gemini\"\n# args = [\"--acp\"]\n\n\
                 [planner]\nmechanism = \"one-shot-agent\"\n",
                supported.join(", "),
            )
        }
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

    // Detect a supported CLI on $PATH
    let path_env = std::env::var("PATH").unwrap_or_default();
    let detected = makina_core::preflight::detect_backend_in_path(&path_env);

    // Global config template (~/.makina/config.toml)
    let global_template = build_global_template(&detected);

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
        let backend_info = detected
            .as_ref()
            .map(|d| d.agent)
            .unwrap_or("none detected — edit [backend] before running");
        Some(format!(
            "Starter configs written to: {} (backend: {})",
            written_paths.join(", "),
            backend_info
        ))
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

    let run = match app.active_run_id() {
        Some(r) => r,
        None => {
            return Some("No run selected — open a plan tab, or pick a plan with [o]".to_string());
        }
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
///   [`Command::ReinterpretRun`] (the recover-from-blocking flow), so the
///   reset/retry action still serves both purposes.
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
        Some(TreeNode::Plan { .. })
        | Some(TreeNode::PlanTask { .. })
        | Some(TreeNode::Folder { .. })
        | Some(TreeNode::PlanInFolder { .. })
        | Some(TreeNode::PlanTaskInFolder { .. }) => None,
        None => None,
    };

    if let Some((command, verb)) = command {
        return match app.api.execute(command).await {
            Ok(_) => Some(verb),
            Err(e) => Some(format!("{verb} failed: {e}")),
        };
    }

    // Nothing retryable. Fall back to re-interpreting a still-Pending run so the
    // reset/retry action still serves the recover-from-blocking flow; otherwise
    // return a no-op message.
    if app
        .focused_node()
        .and_then(|node| match node {
            TreeNode::Run { run } | TreeNode::Task { run, .. } => app.runs.get(run),
            TreeNode::Plan { .. }
            | TreeNode::PlanTask { .. }
            | TreeNode::Folder { .. }
            | TreeNode::PlanInFolder { .. }
            | TreeNode::PlanTaskInFolder { .. } => None,
        })
        .map(|rv| rv.status == RunStatus::Pending)
        .unwrap_or(false)
    {
        return run_control(app, ControlKind::Reinterpret).await;
    }

    Some("nothing to retry here".to_string())
}

fn operation_blocked_event(app: &App, attempted: &str) -> Option<AppEvent> {
    let target = app.reset_context_target()?;
    if app.running_plan_operation(&target).is_none() && !app.opening_plans.contains(&target) {
        return None;
    }
    Some(AppEvent::OperationBlocked {
        target,
        attempted: attempted.to_string(),
    })
}

async fn start_reset_confirmed(
    app: &App,
    confirmation: ResetConfirmation,
    background_tx: &mpsc::Sender<AppEvent>,
) -> (AppEvent, Option<String>) {
    let ResetConfirmation { target, label, run } = confirmation;
    if app.running_plan_operation(&target).is_some() || app.opening_plans.contains(&target) {
        return (
            AppEvent::OperationBlocked {
                target,
                attempted: "Reset selected plan/run".to_string(),
            },
            None,
        );
    }

    // Use a captured live id only when it still resolves to the exact project
    // and plan-directory identity the user approved. A stale/reused id must never reset
    // a different run.
    if let Some(run) = run
        && let Some(run_view) = app.api.run(run).await
        && app.plan_identity_for_run(&run_view) == target
    {
        spawn_reset_run(
            std::sync::Arc::clone(&app.api),
            app.project_api.clone(),
            run,
            target.clone(),
            label.clone(),
            background_tx.clone(),
        );
        return (AppEvent::ResetStarted { target, label }, None);
    }

    spawn_open_and_reset_plan(
        std::sync::Arc::clone(&app.api),
        app.project_api.clone(),
        target.clone(),
        label.clone(),
        background_tx.clone(),
    );
    (AppEvent::ResetStarted { target, label }, None)
}

fn spawn_reset_run(
    api: std::sync::Arc<dyn makina_core::api::Api>,
    project_api: Option<std::sync::Arc<crate::project_api::ProjectApiRouter>>,
    run: makina_core::api::RunId,
    target: PlanIdentity,
    label: String,
    background_tx: mpsc::Sender<AppEvent>,
) {
    use makina_core::api::Command;

    tokio::spawn(async move {
        let message = match api.execute(Command::ResetRun { run }).await {
            Ok(_) => format!("Reset {label}"),
            Err(makina_core::api::ApiError::UnknownRun { .. }) => {
                open_and_reset_message(api.as_ref(), project_api.as_deref(), &target, &label).await
            }
            Err(e) => format!("Reset failed: {e}"),
        };
        let _ = background_tx
            .send(AppEvent::ResetFinished { target, message })
            .await;
    });
}

fn spawn_open_and_reset_plan(
    api: std::sync::Arc<dyn makina_core::api::Api>,
    project_api: Option<std::sync::Arc<crate::project_api::ProjectApiRouter>>,
    target: PlanIdentity,
    label: String,
    background_tx: mpsc::Sender<AppEvent>,
) {
    tokio::spawn(async move {
        let message =
            open_and_reset_message(api.as_ref(), project_api.as_deref(), &target, &label).await;
        let _ = background_tx
            .send(AppEvent::ResetFinished { target, message })
            .await;
    });
}

async fn open_and_reset_message(
    api: &dyn makina_core::api::Api,
    project_api: Option<&crate::project_api::ProjectApiRouter>,
    target: &PlanIdentity,
    label: &str,
) -> String {
    use makina_core::api::{Command, CommandOutcome};

    let opened = if let Some(router) = project_api {
        router
            .open_plan(&target.project_root, target.plan_dir.clone())
            .await
    } else {
        api.execute(Command::OpenPlan {
            plan_dir: target.plan_dir.clone(),
        })
        .await
    };
    match opened {
        Ok(CommandOutcome::RunOpened { run }) => match api.execute(Command::ResetRun { run }).await
        {
            Ok(_) => format!("Reset {label}"),
            Err(error) => format!("Reset failed: {error}"),
        },
        Ok(_) => format!("Reset failed: opening {label} did not return a run"),
        Err(error) => format!("Reset failed: {error}"),
    }
}

async fn purge_worktrees(app: &App) -> Option<String> {
    let project_root = command_project_root(app);
    let label = project_root.display().to_string();
    match app
        .api
        .execute(makina_core::api::Command::PurgeWorktrees { project_root })
        .await
    {
        Ok(CommandOutcome::WorktreesPurged { removed, preserved }) if preserved.is_empty() => {
            Some(format!("Purged {removed} Makina worktree(s) in {label}"))
        }
        Ok(CommandOutcome::WorktreesPurged { removed, preserved }) => Some(format!(
            "Purged {removed} Makina worktree(s); preserved {} recovery path(s): {}",
            preserved.len(),
            preserved
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Ok(CommandOutcome::RepositoryBusy { owner }) => Some(match owner {
            Some(owner) => format!(
                "Purge blocked: repository is active for {} ({})",
                owner.plan_dir.display(),
                owner.run_uid
            ),
            None => "Purge blocked: repository is active".into(),
        }),
        Ok(_) => Some(format!(
            "Purge worktrees returned an unexpected result for {label}"
        )),
        Err(e) => Some(format!("Purge worktrees failed: {e}")),
    }
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
    let project_root = command_project_root(app);
    let label = project_root.display().to_string();
    match app
        .api
        .execute(makina_core::api::Command::DiscoverProject { project_root })
        .await
    {
        Ok(_) => Some(format!("Project discovery started for {label}")),
        Err(e) => Some(format!("Project discovery failed: {e}")),
    }
}

fn command_project_root(app: &App) -> std::path::PathBuf {
    app.context_project_root()
}

async fn register_project(app: &App, path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let project_root = crate::project_api::canonicalize_project_root(path)
        .map_err(|error| format!("Could not resolve project {}: {error}", path.display()))?;
    app.api
        .execute(makina_core::api::Command::RegisterProject {
            project_root: project_root.clone(),
        })
        .await
        .map_err(|error| {
            format!(
                "Could not register project {}: {error}",
                project_root.display()
            )
        })?;
    Ok(project_root)
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

/// Read `dir` and build a [`AppEvent::BrowserOpened`] event from its directories only.
///
/// Like [`read_dir_event`], but filters entries to include only directories (plus the
/// `..` parent entry). Used by the folder browser modal for selecting folders.
///
/// Files are skipped entirely, and entries are sorted alphabetically (case-insensitive).
async fn read_dir_event_folders_only(dir: &std::path::Path) -> AppEvent {
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
            // Skip hidden dotfiles to keep the listing focused.
            if name.starts_with('.') {
                continue;
            }
            let is_dir = de.file_type().await.map(|ft| ft.is_dir()).unwrap_or(false);
            // Only include directories in the folder browser.
            if is_dir {
                items.push(DirEntry { name, path, is_dir });
            }
        }
        // Sort alphabetically (case-insensitive).
        items.sort_by_key(|a| a.name.to_lowercase());
        entries.extend(items);
    }

    AppEvent::BrowserOpened {
        dir: dir.to_path_buf(),
        entries,
    }
}

/// Spawn a background task to read a directory (folders only) and emit the result.
fn spawn_read_dir_folders_only(dir: std::path::PathBuf, background_tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let event = read_dir_event_folders_only(&dir).await;
        let _ = background_tx.send(event).await;
    });
}

// ── Translation helpers ───────────────────────────────────────────────────────

/// Snapshot of which modal overlay is currently active.
///
/// Grouping the flags into a struct keeps `translate_terminal_event` and
/// `translate_key` within clippy's `too_many_arguments` threshold (≤ 7).
#[derive(Clone, Copy, Default)]
struct ModalState {
    browsing: bool,
    viewing_doctor: bool,
    help_mode_active: bool,
    command_palette: bool,
    settings: bool,
    reset_confirm: bool,
    operation_notice: bool,
}

/// Translate a raw crossterm [`CrosstermEvent`] into an [`AppEvent`].
///
/// `modal` bundles which overlay is active; `focused_panel` determines whether
/// Space emits `ToggleTreeNode` (sidebar only); `plan_tab_active` indicates
/// whether a plan tab is currently active (used for accordion toggle keybindings).
///
/// Returns [`AppEvent::Tick`] for events the TUI doesn't handle (e.g. mouse
/// events); those simply trigger a harmless redraw.
fn translate_terminal_event(
    ev: CrosstermEvent,
    modal: ModalState,
    focused_panel: crate::app::Panel,
    plan_tab_active: bool,
    app: &crate::app::App,
) -> AppEvent {
    match ev {
        CrosstermEvent::Key(key) => translate_key(key, modal, focused_panel, plan_tab_active, app),
        CrosstermEvent::Resize(w, h) => AppEvent::Resize(w, h),
        // Mouse wheel scrolls the focused exchange pane regardless of the
        // `browsing` flag (the exchange pane is not the browser).
        //
        // Left button down/drag/up drive an in-app text selection: capture is on
        // for the wheel, so the terminal won't do its own click-drag selection
        // and we reimplement it (highlight + OSC 52 copy). See `crate::selection`.
        // Other kinds (move, other buttons) stay no-ops.
        CrosstermEvent::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => AppEvent::ScrollUpAt(m.column, m.row),
            MouseEventKind::ScrollDown => AppEvent::ScrollDownAt(m.column, m.row),
            MouseEventKind::Down(MouseButton::Left) => {
                // Hit-test, in priority order: a tab close icon closes that tab;
                // a tab chip activates it; a sidebar row opens/focuses that
                // node's tab (mirrors Enter); an accordion header toggles it;
                // otherwise begin a text selection.
                let in_bounds = |r: &ratatui::layout::Rect| {
                    r.x <= m.column
                        && m.column < r.x + r.width
                        && r.y <= m.row
                        && m.row < r.y + r.height
                };
                if let Some((idx, _)) = app
                    .tab_close_bounds
                    .borrow()
                    .iter()
                    .find(|(_, r)| in_bounds(r))
                {
                    AppEvent::CloseTabAt(*idx)
                } else if let Some((idx, _)) =
                    app.tab_bounds.borrow().iter().find(|(_, r)| in_bounds(r))
                {
                    AppEvent::ActivateTab(*idx)
                } else if let Some((idx, _)) = app
                    .sidebar_node_bounds
                    .borrow()
                    .iter()
                    .find(|(_, r)| in_bounds(r))
                {
                    AppEvent::OpenTreeRow(*idx)
                } else if let Some((key, _)) = app
                    .tool_diff_bounds
                    .borrow()
                    .iter()
                    .find(|(_, r)| in_bounds(r))
                {
                    AppEvent::ToggleToolDiff(key.clone())
                } else if let Some((section, _)) = app
                    .accordion_header_bounds
                    .borrow()
                    .iter()
                    .find(|(_, r)| in_bounds(r))
                {
                    let task_tab_active = app
                        .tabs
                        .active_tab
                        .and_then(|idx| app.tabs.open_tabs.get(idx))
                        .is_some_and(|content| {
                            matches!(
                                content,
                                crate::app::TabContent::Task { .. }
                                    | crate::app::TabContent::PlanTask { .. }
                            )
                        });
                    if task_tab_active {
                        AppEvent::ToggleTaskAccordionSection(*section)
                    } else {
                        AppEvent::ToggleAccordionSection(*section)
                    }
                } else {
                    AppEvent::SelectionStart(m.column, m.row)
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => AppEvent::SelectionExtend(m.column, m.row),
            MouseEventKind::Up(MouseButton::Left) => AppEvent::SelectionEnd(m.column, m.row),
            _ => AppEvent::Tick,
        },
        // Paste, focus, etc. — ignored for now.
        _ => AppEvent::Tick,
    }
}

/// Translate a key press into an [`AppEvent`], honouring the current view mode.
///
/// `plan_tab_active` indicates whether a plan tab is currently active; when true,
/// the 's', 'a', 't', and 'z' keys dispatch accordion toggle events instead of
/// run-control commands (while the main pane is focused).
fn translate_key(
    key: crossterm::event::KeyEvent,
    modal: ModalState,
    focused_panel: crate::app::Panel,
    plan_tab_active: bool,
    app: &crate::app::App,
) -> AppEvent {
    let ModalState {
        browsing,
        viewing_doctor,
        help_mode_active,
        command_palette,
        settings,
        reset_confirm,
        operation_notice,
    } = modal;
    use crossterm::event::KeyEventKind;
    // Only react to key-press events (not key-release / repeat on some platforms).
    if key.kind != KeyEventKind::Press {
        return AppEvent::Tick;
    }

    // Ctrl-C remains the universal quit chord. Stop/cancel moved to the command
    // palette so it cannot collide with other control-key input while a run is
    // active.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return AppEvent::Quit;
    }

    // Ctrl-P always opens the command palette. Pause moved into the palette so
    // this chord is stable even during an active run.
    if key.code == KeyCode::Char('p')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !reset_confirm
        && !operation_notice
    {
        return AppEvent::OpenCommandPalette;
    }

    if operation_notice {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('Q') => {
                AppEvent::CloseOperationNotice
            }
            _ => AppEvent::Tick,
        }
    } else if reset_confirm {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                AppEvent::CloseResetConfirmation
            }
            KeyCode::Enter => app
                .reset_confirmation
                .clone()
                .map(|confirmation| AppEvent::ResetRun { confirmation })
                .unwrap_or(AppEvent::CloseResetConfirmation),
            _ => AppEvent::Tick,
        }
    } else if command_palette {
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
    } else if browsing {
        // ── Browser keymap (file or folder) ───────────────────────────────────
        // Both file browser and folder browser share the same keybindings:
        // Esc closes; Enter activates; Backspace goes to parent; j/k/arrows navigate.
        // The difference is in what event is dispatched on Enter — for folder browser
        // we dispatch FolderBrowserActivate with the purpose, for file browser we
        // dispatch BrowserActivate.
        if app.is_folder_browsing() {
            // Folder browser keymap (plan 0043)
            if let Some(purpose) = app.folder_browser_purpose() {
                match key.code {
                    KeyCode::Esc => AppEvent::CloseBrowser,
                    KeyCode::Enter => AppEvent::FolderBrowserActivate { purpose },
                    KeyCode::Backspace => AppEvent::FolderBrowserParent,
                    KeyCode::Up | KeyCode::Char('k') => AppEvent::FolderBrowserUp,
                    KeyCode::Down | KeyCode::Char('j') => AppEvent::FolderBrowserDown,
                    _ => AppEvent::Tick,
                }
            } else {
                // Should not happen, but default to closing
                AppEvent::CloseBrowser
            }
        } else {
            // File browser keymap
            match key.code {
                KeyCode::Esc => AppEvent::CloseBrowser,
                KeyCode::Enter => AppEvent::BrowserActivate,
                KeyCode::Backspace => AppEvent::BrowserParent,
                KeyCode::Up | KeyCode::Char('k') => AppEvent::BrowserUp,
                KeyCode::Down | KeyCode::Char('j') => AppEvent::BrowserDown,
                _ => AppEvent::Tick,
            }
        }
    } else if settings {
        // ── Settings modal keymap ────────────────────────────────────────────
        // Esc closes without saving; Enter commits; Up/Down navigate fields;
        // 0-9 and Backspace edit numeric fields; Left/Right/Space cycle dropdowns.
        match key.code {
            KeyCode::Esc => AppEvent::SettingsCommit,
            KeyCode::Up => AppEvent::SettingsUp,
            KeyCode::Down => AppEvent::SettingsDown,
            KeyCode::Left => AppEvent::SettingsPreviousOption,
            KeyCode::Right => AppEvent::SettingsNextOption,
            KeyCode::Backspace => AppEvent::SettingsBackspace,
            KeyCode::Char(' ') => AppEvent::SettingsNextOption,
            KeyCode::Char(c) => AppEvent::SettingsInput(c),
            // Enter on a model field opens the searchable model picker;
            // Enter on other fields commits settings.
            KeyCode::Enter => {
                if let Some(settings) = app.settings.as_ref() {
                    if matches!(
                        settings.focused,
                        crate::app::SettingsField::DeveloperModel
                            | crate::app::SettingsField::ReviewerModel
                            | crate::app::SettingsField::PlannerModel
                    ) {
                        AppEvent::OpenModelPicker
                    } else {
                        AppEvent::SettingsCommit
                    }
                } else {
                    AppEvent::SettingsCommit
                }
            }
            _ => AppEvent::Tick,
        }
    } else if app.mode == crate::app::Mode::ModelPicker {
        // ── Model picker keymap ─────────────────────────────────────────────
        // Esc cancels; Enter selects; j/k/arrows navigate; type to filter.
        match key.code {
            KeyCode::Esc => AppEvent::CloseModelPicker,
            KeyCode::Enter => AppEvent::ModelPickerSelect,
            KeyCode::Up | KeyCode::Char('k') => AppEvent::ModelPickerUp,
            KeyCode::Down | KeyCode::Char('j') => AppEvent::ModelPickerDown,
            KeyCode::Backspace => AppEvent::ModelPickerBackspace,
            KeyCode::Char(c) => AppEvent::ModelPickerInput(c),
            _ => AppEvent::Tick,
        }
    } else if help_mode_active {
        // ── Help overlay keymap ──────────────────────────────────────────────
        // Esc or `q` closes the help overlay.
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => AppEvent::CloseHelpMode,
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
        use crate::app::Panel;

        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => AppEvent::Quit,
            KeyCode::Esc => AppEvent::Quit,
            // Shift+Tab: most terminals deliver it as `BackTab` (no SHIFT
            // modifier); terminals in the kitty/enhanced keyboard mode deliver
            // it as `Tab` + SHIFT. Handle both so reverse focus always works.
            KeyCode::BackTab => AppEvent::FocusPrev,
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    AppEvent::FocusPrev
                } else {
                    AppEvent::FocusNext
                }
            }
            // Cycle the dependency view (Off → List → Tree → Timeline → Off).
            KeyCode::Char('v') | KeyCode::Char('V') => AppEvent::CycleDependencyView,
            // Toggle the error pane open/closed.
            KeyCode::Char('e') | KeyCode::Char('E') => AppEvent::ToggleErrorPane,
            // Toggle the in-TUI log panel (the context task's agent log).
            KeyCode::Char('l') | KeyCode::Char('L') => AppEvent::ToggleLogPane,
            // Toggle verbose mode on/off (Ctrl+O — checked BEFORE the plain
            // `o`/`O` → OpenBrowser arm so the modifier guard wins).
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                AppEvent::ToggleVerbose
            }
            // Open the browser to pick a plan directory.
            KeyCode::Char('o') | KeyCode::Char('O') => AppEvent::OpenBrowser,
            // Toggle the help overlay showing all keybindings.
            KeyCode::Char('?') => AppEvent::ToggleHelpMode,
            // Open the doctor health-check overlay.
            KeyCode::Char('!') => AppEvent::OpenDoctor,
            // Ctrl+S used to start the run; run controls now live in the command
            // palette so the control-key surface stays reserved for app commands.
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => AppEvent::Tick,
            // ── Accordion toggles (plan 0032, extended for task tabs in plan 0042 WS6) ────
            // s/a/t/z toggle accordion sections when the main pane is focused.
            // For plan tabs: Scope/Architecture/Tasks/Status
            // For task detail tabs: s/z toggle Scope/Execution.
            KeyCode::Char('s') | KeyCode::Char('S') => {
                let task_tab_active = app
                    .tabs
                    .active_tab
                    .and_then(|idx| app.tabs.open_tabs.get(idx))
                    .is_some_and(|content| {
                        matches!(
                            content,
                            crate::app::TabContent::Task { .. }
                                | crate::app::TabContent::PlanTask { .. }
                        )
                    });
                if focused_panel == Panel::Main && plan_tab_active {
                    AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Scope)
                } else if focused_panel == Panel::Main && task_tab_active {
                    AppEvent::ToggleTaskAccordionSection(crate::app::AccordionSection::Scope)
                } else {
                    AppEvent::Tick
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                if focused_panel == Panel::Main && plan_tab_active {
                    AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Architecture)
                } else if !plan_tab_active {
                    AppEvent::StatusMessage(
                        "Accordion toggle not available here — open a plan tab.".to_string(),
                    )
                } else {
                    AppEvent::Tick
                }
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                if focused_panel == Panel::Main && plan_tab_active {
                    AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Tasks)
                } else if !plan_tab_active {
                    AppEvent::StatusMessage(
                        "Accordion toggle not available here — open a plan tab.".to_string(),
                    )
                } else {
                    AppEvent::Tick
                }
            }
            KeyCode::Char('z') | KeyCode::Char('Z') => {
                let task_tab_active = app
                    .tabs
                    .active_tab
                    .and_then(|idx| app.tabs.open_tabs.get(idx))
                    .is_some_and(|content| {
                        matches!(
                            content,
                            crate::app::TabContent::Task { .. }
                                | crate::app::TabContent::PlanTask { .. }
                        )
                    });
                if focused_panel == Panel::Main && plan_tab_active {
                    AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Status)
                } else if focused_panel == Panel::Main && task_tab_active {
                    AppEvent::ToggleTaskAccordionSection(crate::app::AccordionSection::Execution)
                } else if !plan_tab_active && !task_tab_active {
                    AppEvent::StatusMessage(
                        "Accordion toggle not available here — open a plan or task tab."
                            .to_string(),
                    )
                } else {
                    AppEvent::Tick
                }
            }
            // Dismiss the provider-missing warning banner (non-fatal; just hides it).
            KeyCode::Char('d') | KeyCode::Char('D') => AppEvent::DismissProviderWarning,
            // ── Tab navigation (plan 0032) ────────────────────────────────────
            // Alt+Left/Right to cycle between open tabs (must come before plain arrow keys).
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => AppEvent::PrevTab,
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => AppEvent::NextTab,
            // ── Sidebar resizing (plan 0039) ──────────────────────────────────────
            // Shift+Left/Right to resize the sidebar width (must come before plain arrow keys).
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                AppEvent::ResizeSidebarLeft
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                AppEvent::ResizeSidebarRight
            }
            // Ctrl+W to close the active tab.
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                AppEvent::CloseTab
            }
            // Sidebar navigation: arrow keys and vim-style j/k.
            KeyCode::Up | KeyCode::Char('k') => AppEvent::SelectUp,
            KeyCode::Down | KeyCode::Char('j') => AppEvent::SelectDown,
            // Right arrow: Tab-equivalent in main pane (forward focus), expand/cross in sidebar.
            KeyCode::Right => match focused_panel {
                Panel::Main => AppEvent::FocusNext,
                Panel::Sidebar => AppEvent::FocusRightOrExpand,
            },
            // Left arrow: Shift+Tab-equivalent in main pane (backward focus), collapse/cross in sidebar.
            KeyCode::Left => match focused_panel {
                Panel::Main => AppEvent::FocusPrev,
                Panel::Sidebar => AppEvent::FocusLeftOrCollapse,
            },
            // Space: toggle expand/collapse the focused tree node (sidebar focus only).
            KeyCode::Char(' ') => match focused_panel {
                Panel::Sidebar => AppEvent::ToggleTreeNode,
                Panel::Main => AppEvent::Tick,
            },
            // Enter: open the focused node in the sidebar, or toggle the focused
            // accordion section in the main pane.
            KeyCode::Enter => match focused_panel {
                Panel::Sidebar => AppEvent::OpenFocusedNode,
                Panel::Main => AppEvent::ToggleTreeNode,
            },
            // PgUp/PgDn: scroll the error pane when it's open.
            KeyCode::PageUp => {
                if app.error_pane_open {
                    AppEvent::ErrorPaneScrollUp
                } else {
                    AppEvent::Tick
                }
            }
            KeyCode::PageDown => {
                if app.error_pane_open {
                    AppEvent::ErrorPaneScrollDown
                } else {
                    AppEvent::Tick
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
    use crate::app::CollapseKey;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    };

    #[derive(Clone)]
    struct TestPlanTask {
        id: String,
        title: String,
        gated: bool,
        depends_on: Vec<String>,
        body: String,
    }

    struct TestTaskSource(Vec<u8>);

    impl makina_core::plan::PlanFileSource for TestTaskSource {
        fn read_file(
            &self,
            _: &std::path::Path,
        ) -> Result<Vec<u8>, makina_core::plan::PlanDocumentError> {
            Ok(self.0.clone())
        }
        fn object_format(&self) -> makina_core::plan::GitObjectFormat {
            makina_core::plan::GitObjectFormat::Sha1
        }
        fn validation_base_oid(&self) -> Option<&str> {
            None
        }
        fn is_tracked_ordinary_file(
            &self,
            _: &std::path::Path,
        ) -> Result<bool, makina_core::plan::PlanDocumentError> {
            Ok(false)
        }
    }

    fn test_plan_entry(
        dir: std::path::PathBuf,
        slug: String,
        tasks: Vec<TestPlanTask>,
    ) -> makina_core::orchestrator::PlanEntry {
        let key = makina_core::plan::PlanKey::parse(
            dir.components()
                .collect::<Vec<_>>()
                .windows(3)
                .find_map(|parts| {
                    (parts[0].as_os_str() == "docs" && parts[1].as_os_str() == "plans").then(|| {
                        std::path::PathBuf::from("docs")
                            .join("plans")
                            .join(parts[2].as_os_str())
                    })
                })
                .unwrap_or_else(|| std::path::PathBuf::from("docs/plans/0001-test")),
        )
        .unwrap();
        let repository = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source = makina_core::plan::GitTreePlanFileSource::new(repository, "HEAD").unwrap();
        let fixture = makina_core::plan::PlanKey::parse(
            "docs/plans/0048-Per-Task-Plan-Documents-And-Transactional-Status",
        )
        .unwrap();
        let makina_core::plan::PlanCandidate::Plan(document) = makina_core::plan::load_plan(
            &source,
            fixture,
            &makina_core::plan::PlanReservations::default(),
        )
        .unwrap() else {
            panic!("fixture plan missing")
        };
        let mut document = *document;
        document.key = key.clone();
        document.title = slug.clone();
        document.tasks = tasks
            .into_iter()
            .enumerate()
            .map(|(index, task)| {
                let deps = if task.depends_on.is_empty() {
                    "[]".into()
                } else {
                    format!(
                        "\n{}",
                        task.depends_on
                            .iter()
                            .map(|id| format!("  - {id}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                let text = format!(
                    "---\nid: {}\ntitle: {}\nworkstream: \"0001\"\nkind: task\ndepends_on: {}\ngated: {}\ntouches:\n  - src/**\nstatus: planned\nmerged_as: \"\"\n---\n# {}\n\n## Context\n\n{}\n\n**Steps:**\n\n1. Test.\n\n- **Done when:** Tested.\n",
                    task.id, task.title, deps, task.gated, task.title, task.body
                );
                makina_core::plan::parse_task_document(
                    &TestTaskSource(text.into_bytes()),
                    key.relative_dir.join("tasks").join(format!(
                        "01{:02}-{}.md",
                        index + 1,
                        task.id
                    )),
                )
                .unwrap()
            })
            .collect();
        makina_core::orchestrator::PlanEntry {
            dir,
            key,
            slug,
            state: makina_core::orchestrator::PlanDiscoveryState::Ready,
            document: Some(document),
            diagnostics: makina_core::plan::PlanValidationReport::default(),
        }
    }

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

    fn test_app() -> App {
        use crate::placeholder::PlaceholderApi;
        use std::path::PathBuf;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        App::new(api, vec![], PathBuf::from("/"))
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
        let app = test_app();
        assert!(matches!(
            translate_terminal_event(
                wheel(MouseEventKind::ScrollUp),
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::ScrollUpAt(_, _)
        ));
        assert!(matches!(
            translate_terminal_event(
                wheel(MouseEventKind::ScrollDown),
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::ScrollDownAt(_, _)
        ));
    }

    /// Build a crossterm mouse event of `kind` at cell `(column, row)`.
    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> CrosstermEvent {
        CrosstermEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// Left button down/drag/up drive the in-app text selection, carrying the
    /// pointer's cell coordinates through to the corresponding `AppEvent`.
    #[test]
    fn left_button_drag_drives_selection() {
        let app = test_app();
        let down = translate_terminal_event(
            mouse_at(MouseEventKind::Down(MouseButton::Left), 3, 7),
            ModalState::default(),
            crate::app::Panel::Sidebar,
            false,
            &app,
        );
        assert!(matches!(down, AppEvent::SelectionStart(3, 7)));

        let drag = translate_terminal_event(
            mouse_at(MouseEventKind::Drag(MouseButton::Left), 10, 9),
            ModalState::default(),
            crate::app::Panel::Sidebar,
            false,
            &app,
        );
        assert!(matches!(drag, AppEvent::SelectionExtend(10, 9)));

        let up = translate_terminal_event(
            mouse_at(MouseEventKind::Up(MouseButton::Left), 10, 9),
            ModalState::default(),
            crate::app::Panel::Sidebar,
            false,
            &app,
        );
        assert!(matches!(up, AppEvent::SelectionEnd(10, 9)));
    }

    /// Plain pointer motion (no button) and non-left buttons stay no-ops so they
    /// don't disturb selection or scroll state.
    #[test]
    fn moved_and_other_buttons_are_noops() {
        let app = test_app();
        let moved = translate_terminal_event(
            mouse_at(MouseEventKind::Moved, 1, 1),
            ModalState::default(),
            crate::app::Panel::Sidebar,
            false,
            &app,
        );
        assert!(matches!(moved, AppEvent::Tick), "Moved must be a no-op");

        let right = translate_terminal_event(
            mouse_at(MouseEventKind::Down(MouseButton::Right), 1, 1),
            ModalState::default(),
            crate::app::Panel::Sidebar,
            false,
            &app,
        );
        assert!(
            matches!(right, AppEvent::Tick),
            "non-left buttons must be no-ops"
        );
    }

    /// A left click hit-tests, in priority order, tab close icons, tab chips,
    /// sidebar rows, tool diff headers, then accordion headers (all recorded
    /// during render), falling back to a text selection when the click lands on
    /// none. This is what makes clicking a tab switch tabs, clicking a close
    /// icon close it, and clicking a sidebar row open/focus its tab.
    #[test]
    fn left_click_hit_tests_tab_close_icons_tabs_then_sidebar_rows() {
        use ratatui::layout::Rect;

        let app = test_app();
        // Simulate the bounds a render would have recorded.
        app.tab_bounds.borrow_mut().push((
            1,
            Rect {
                x: 5,
                y: 0,
                width: 8,
                height: 1,
            },
        ));
        app.tab_close_bounds.borrow_mut().push((
            1,
            Rect {
                x: 12,
                y: 0,
                width: 1,
                height: 1,
            },
        ));
        app.sidebar_node_bounds.borrow_mut().push((
            2,
            Rect {
                x: 0,
                y: 3,
                width: 20,
                height: 1,
            },
        ));
        let tool_key = crate::app::ToolDiffKey {
            run: makina_core::api::RunId(1),
            task: makina_core::api::TaskId::new("task"),
            tool_id: "write-tool".to_string(),
        };
        app.tool_diff_bounds.borrow_mut().push((
            tool_key.clone(),
            Rect {
                x: 25,
                y: 7,
                width: 30,
                height: 1,
            },
        ));

        let click = |col, row| {
            translate_terminal_event(
                mouse_at(MouseEventKind::Down(MouseButton::Left), col, row),
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &app,
            )
        };

        // Inside the close icon → close that tab, even though it overlaps the chip.
        assert!(
            matches!(click(12, 0), AppEvent::CloseTabAt(1)),
            "click on a tab close icon closes it"
        );
        // Inside the tab chip but outside the close icon → activate that tab.
        assert!(
            matches!(click(6, 0), AppEvent::ActivateTab(1)),
            "click on a tab chip activates it"
        );
        // Inside a sidebar row → open/focus that node's tab.
        assert!(
            matches!(click(4, 3), AppEvent::OpenTreeRow(2)),
            "click on a sidebar row opens that node"
        );
        // Inside an expandable diff header → toggle that diff.
        match click(30, 7) {
            AppEvent::ToggleToolDiff(key) => assert_eq!(key, tool_key),
            other => panic!("click on a tool diff header must toggle it, got {other:?}"),
        }
        // Outside both → fall back to text selection.
        assert!(
            matches!(click(50, 20), AppEvent::SelectionStart(50, 20)),
            "click on empty space starts a selection"
        );
    }

    #[test]
    fn q_key_translates_to_quit() {
        let ev = key_press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::Quit
        ));
    }

    #[test]
    fn esc_key_translates_to_quit() {
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::Quit
        ));
    }

    #[test]
    fn ctrl_c_translates_to_quit() {
        // Ctrl+C with NO run selected falls back to Quit (the universal exit).
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::Quit
        ));
    }

    #[test]
    fn ctrl_c_with_run_translates_to_quit() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use makina_core::api::{RunId, RunStatus, RunView};
        use std::sync::Arc;

        // Create an app that has one run so selected_run() is Some.
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));
        assert!(
            app.selected_run().is_some(),
            "test precondition: run must be selected"
        );

        let ev = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::Quit
        ));
    }

    #[test]
    fn ctrl_s_no_longer_translates_to_start_run() {
        let ev = key_press(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::Tick
        ));
    }

    #[test]
    fn ctrl_p_with_run_opens_palette() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use makina_core::api::{RunId, RunStatus, RunView};
        use std::sync::Arc;

        // Create an app that has one run so selected_run() is Some.
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(2),
            run_uid: String::new(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], std::path::PathBuf::from("."));
        assert!(
            app.selected_run().is_some(),
            "test precondition: run must be selected"
        );

        let ev = key_press(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::OpenCommandPalette
        ));
    }

    #[test]
    fn tab_translates_to_focus_next() {
        let ev = key_press(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::FocusNext
        ));
    }

    #[test]
    fn back_tab_translates_to_focus_prev() {
        // The standard terminal encoding of Shift+Tab: KeyCode::BackTab with no
        // SHIFT modifier. This is what most terminals actually send.
        let ev = key_press(KeyCode::BackTab, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::FocusPrev
        ));
    }

    #[test]
    fn shift_tab_translates_to_focus_prev() {
        // Enhanced/kitty keyboard mode encoding of Shift+Tab: KeyCode::Tab with
        // the SHIFT modifier set.
        let ev = key_press(KeyCode::Tab, KeyModifiers::SHIFT);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::FocusPrev
        ));
    }

    #[test]
    fn v_translates_to_cycle_dependency_view() {
        let app = test_app();
        assert!(matches!(
            translate_terminal_event(
                key_press(KeyCode::Char('v'), KeyModifiers::NONE),
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::CycleDependencyView
        ));
    }

    #[test]
    fn e_key_translates_to_toggle_error_pane() {
        let app = test_app();
        assert!(matches!(
            translate_terminal_event(
                key_press(KeyCode::Char('e'), KeyModifiers::NONE),
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::ToggleErrorPane
        ));
    }

    #[test]
    fn r_key_no_longer_translates_to_retry_focused() {
        let app = test_app();
        assert!(matches!(
            translate_terminal_event(
                key_press(KeyCode::Char('r'), KeyModifiers::NONE),
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::Tick
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
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::Tick
        ));
    }

    #[test]
    fn resize_translates_to_resize_event() {
        let ev = CrosstermEvent::Resize(120, 40);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::Resize(120, 40)
        ));
    }

    #[test]
    fn up_arrow_translates_to_select_up() {
        let ev = key_press(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::SelectUp
        ));
    }

    #[test]
    fn down_arrow_translates_to_select_down() {
        let ev = key_press(KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::SelectDown
        ));
    }

    #[test]
    fn right_arrow_translates_to_focus_right_or_expand() {
        let ev = key_press(KeyCode::Right, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::FocusRightOrExpand
        ));
    }

    #[test]
    fn left_arrow_translates_to_focus_left_or_collapse() {
        let ev = key_press(KeyCode::Left, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::FocusLeftOrCollapse
        ));
    }

    #[test]
    fn k_key_translates_to_select_up() {
        let ev = key_press(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::SelectUp
        ));
    }

    #[test]
    fn j_key_translates_to_select_down() {
        let ev = key_press(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::SelectDown
        ));
    }

    // ── File-browser keymap (task 28) ─────────────────────────────────────────

    #[test]
    fn o_key_opens_browser_in_normal_mode() {
        let ev = key_press(KeyCode::Char('o'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::OpenBrowser
        ));
    }

    // ── Removed run-control key translation ──────────────────────────────────

    #[test]
    fn s_key_outside_accordion_is_inert() {
        let ev = key_press(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::Tick
        ));
    }

    #[test]
    fn p_key_no_longer_translates_to_pause_run() {
        let ev = key_press(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::Tick
        ));
    }

    #[test]
    fn c_key_no_longer_translates_to_cancel_run() {
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::Tick
        ));
    }

    #[test]
    fn control_keys_do_nothing_in_browser_mode() {
        // s/p/c are not browser keys; inside the browser they fall through to a
        // harmless Tick (the browser keymap owns navigation).
        let app = test_app();
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
                        crate::app::Panel::Sidebar,
                        false,
                        &app
                    ),
                    AppEvent::Tick
                ),
                "'{ch}' must be inert in browser mode"
            );
        }
    }

    #[test]
    fn enter_in_browser_activates_selection() {
        let app = test_app();
        let ev = key_press(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::BrowserActivate
        ));
    }

    #[test]
    fn esc_in_browser_closes_not_quits() {
        let app = test_app();
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        // In browser mode, Esc must close the browser, NOT quit the app.
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::CloseBrowser
        ));
    }

    #[test]
    fn backspace_in_browser_goes_to_parent() {
        let app = test_app();
        let ev = key_press(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::BrowserParent
        ));
    }

    #[test]
    fn jk_in_browser_navigate_browser_not_sidebar() {
        let app = test_app();
        let down = key_press(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                down,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
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
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::BrowserUp
        ));
    }

    #[test]
    fn ctrl_c_quits_even_in_browser_mode() {
        let app = test_app();
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::Quit
        ));
    }

    #[test]
    fn q_in_browser_is_not_quit() {
        // `q` is a normal-mode quit key; inside the browser it must not quit
        // (it falls through to Tick so the user can keep browsing).
        let app = test_app();
        let ev = key_press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    browsing: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
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
            false,
            &app,
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
    async fn control_actions_issue_commands_and_set_status_message() {
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
                    Command::OpenPlan { .. } => Ok(CommandOutcome::RunOpened { run: RunId(1) }),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
        let (ev, status) = resolve_io_for_test(&mut app, AppEvent::StartRun).await;
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
        let (_ev, status) = resolve_io_for_test(&mut app, AppEvent::PauseRun).await;
        assert!(status.unwrap().contains("Pause"));

        // Cancel.
        let (_ev, status) = resolve_io_for_test(&mut app, AppEvent::CancelRun).await;
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

    /// With NO run selected, a control action surfaces a "No run selected" message
    /// and issues no command.
    #[tokio::test]
    async fn control_action_with_no_selection_reports_and_issues_nothing() {
        use crate::app::{App, AppEvent};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        assert!(app.selected_run().is_none());

        let (ev, status) = resolve_io_for_test(&mut app, AppEvent::StartRun).await;
        assert!(matches!(ev, AppEvent::Tick));
        let msg = status.expect("must produce a status message");
        assert!(msg.contains("No run selected"));
        assert!(msg.contains("open a plan tab"));
    }

    /// **Start on a discovered plan with no Run (plan 0042 follow-up).** When the
    /// active tab is a valid per-task plan and no Run is open for it, `Start`
    /// must open that plan directory as a new Run and auto-start it — issuing
    /// `OpenPlan{plan_dir}` followed by `StartRun{new run}` — so a plan
    /// can be launched from its tab without the `[o]` file browser.
    #[tokio::test]
    async fn start_on_plan_tab_without_run_opens_and_starts_it() {
        use crate::app::{App, AppEvent, TabContent};
        use async_trait::async_trait;
        use makina_core::api::{
            Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunView,
        };
        use std::sync::{Arc, Mutex};

        /// Records every command and reports a fresh RunId for OpenPlan.
        struct RecordingApi {
            commands: Mutex<Vec<Command>>,
        }
        #[async_trait]
        impl Api for RecordingApi {
            async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
                self.commands.lock().unwrap().push(command.clone());
                match command {
                    Command::OpenPlan { .. } => Ok(CommandOutcome::RunOpened { run: RunId(42) }),
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

        let api = Arc::new(RecordingApi {
            commands: Mutex::new(Vec::new()),
        });
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn Api>,
            vec![],
            std::path::PathBuf::from("."),
        );
        // A discovered per-task plan surfaced via an active plan tab. No
        // Run exists for it.
        app.discovered_plans = vec![test_plan_entry(
            std::path::PathBuf::from("/tmp/docs/plans/0099-demo"),
            "0099-demo".to_string(),
            Vec::new(),
        )];
        let target = app.plan_identity_for_entry(&app.repo_root, &app.discovered_plans[0]);
        app.tabs.open_tab(TabContent::Plan {
            plan: target.clone(),
        });
        assert!(
            app.active_run_id().is_none(),
            "precondition: no Run is open for the plan"
        );

        // Start on the plan tab spawns the open+auto-start in the background and
        // returns an immediate "Starting …" status.
        let (tx, _rx) = background_events();
        let (ev, status) = resolve_io(&mut app, AppEvent::StartRun, &tx).await;
        assert!(matches!(&ev, AppEvent::PlanOpenStarted { target: opened } if opened == &target));
        assert!(
            status
                .as_deref()
                .is_some_and(|m| m.contains("Starting") && m.contains("0099-demo")),
            "Start on a plan must surface a 'Starting <plan>…' status; got {status:?}"
        );
        app.update(ev);

        // A second interaction before RunOpened reaches App state must not
        // launch another background OpenPlan for the same canonical plan.
        let (second, second_status) = resolve_io(&mut app, AppEvent::StartRun, &tx).await;
        assert!(matches!(
            second,
            AppEvent::OperationBlocked {
                target: ref blocked,
                ref attempted
            } if blocked == &target && attempted == "Start run"
        ));
        assert!(second_status.is_none());

        // The background task issues OpenPlan then StartRun. Poll until both land.
        let cmds = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                {
                    let c = api.commands.lock().unwrap();
                    if c.len() >= 2 {
                        return c.clone();
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("background OpenPlan+StartRun did not complete in time");

        assert!(
            matches!(&cmds[0], Command::OpenPlan { plan_dir }
                if plan_dir == &makina_core::plan::PlanKey::parse("docs/plans/0099-demo").unwrap()),
            "first command must open the selected plan directory; got {:?}",
            cmds[0]
        );
        assert!(
            matches!(cmds[1], Command::StartRun { run: RunId(42) }),
            "second command must auto-start the freshly opened run; got {:?}",
            cmds[1]
        );
        assert_eq!(cmds.len(), 2, "repeated Start must not open a second run");
    }

    /// Reset mirrors the Start-on-plan fallback: an already-open plan tab with
    /// no live Run should still be resettable from the command palette.
    #[tokio::test]
    async fn reset_on_plan_tab_without_live_run_opens_and_resets_it() {
        use crate::app::{App, AppEvent, TabContent};
        use async_trait::async_trait;
        use makina_core::api::{
            Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunView,
        };
        use std::sync::{Arc, Mutex};

        struct RecordingApi {
            commands: Mutex<Vec<Command>>,
        }
        #[async_trait]
        impl Api for RecordingApi {
            async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
                self.commands.lock().unwrap().push(command.clone());
                match command {
                    Command::OpenPlan { .. } => Ok(CommandOutcome::RunOpened { run: RunId(42) }),
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

        let api = Arc::new(RecordingApi {
            commands: Mutex::new(Vec::new()),
        });
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn Api>,
            vec![],
            std::path::PathBuf::from("."),
        );
        app.discovered_plans = vec![test_plan_entry(
            std::path::PathBuf::from("/tmp/docs/plans/0099-demo"),
            "0099-demo".to_string(),
            Vec::new(),
        )];
        let target = app.plan_identity_for_entry(&app.repo_root, &app.discovered_plans[0]);
        app.tabs.open_tab(TabContent::Plan {
            plan: target.clone(),
        });

        let (tx, mut rx) = background_events();
        let confirmation = app
            .reset_confirmation_for_context()
            .expect("plan should produce reset confirmation");
        // Change the active context after the modal captured its target. The
        // confirmed action must still operate on the path the user approved.
        app.discovered_plans.push(test_plan_entry(
            std::path::PathBuf::from("/other/docs/plans/0099-demo"),
            "0099-demo".to_string(),
            Vec::new(),
        ));
        let other = app.plan_identity_for_entry(&app.repo_root, &app.discovered_plans[1]);
        app.tabs.open_tab(TabContent::Plan { plan: other });
        let (ev, status) = resolve_io(&mut app, AppEvent::ResetRun { confirmation }, &tx).await;
        assert!(status.is_none());
        assert!(
            matches!(
                ev,
                AppEvent::ResetStarted {
                    target: ref reset_target,
                    ref label
                } if reset_target == &target && label == "0099-demo"
            ),
            "reset must immediately mark the plan as resetting; got {ev:?}"
        );

        let cmds = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                {
                    let c = api.commands.lock().unwrap();
                    if c.len() >= 2 {
                        return c.clone();
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("background OpenPlan+ResetRun did not complete in time");
        assert!(
            matches!(&cmds[0], Command::OpenPlan { plan_dir }
                if plan_dir == &makina_core::plan::PlanKey::parse("docs/plans/0099-demo").unwrap()),
            "first command must open the selected plan directory; got {:?}",
            cmds[0]
        );
        assert!(
            matches!(cmds[1], Command::ResetRun { run: RunId(42) }),
            "second command must reset the freshly opened run; got {:?}",
            cmds[1]
        );
        assert!(
            matches!(
                rx.try_recv(),
                Ok(AppEvent::ResetFinished { target: reset_target, message })
                    if reset_target == target && message == "Reset 0099-demo"
            ),
            "background reset must report completion"
        );
    }

    #[tokio::test]
    async fn start_on_resetting_plan_is_blocked_without_spawning_work() {
        use crate::app::{App, AppEvent, TabContent};
        use async_trait::async_trait;
        use makina_core::api::{
            Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunView,
        };
        use std::sync::{Arc, Mutex};

        struct RecordingApi {
            commands: Mutex<Vec<Command>>,
        }
        #[async_trait]
        impl Api for RecordingApi {
            async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
                self.commands.lock().unwrap().push(command);
                Ok(CommandOutcome::Acknowledged)
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

        let api = Arc::new(RecordingApi {
            commands: Mutex::new(Vec::new()),
        });
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn Api>,
            vec![],
            std::path::PathBuf::from("."),
        );
        app.discovered_plans = vec![test_plan_entry(
            std::path::PathBuf::from("/tmp/docs/plans/0099-demo"),
            "0099-demo".to_string(),
            Vec::new(),
        )];
        let target = app.plan_identity_for_entry(&app.repo_root, &app.discovered_plans[0]);
        app.tabs.open_tab(TabContent::Plan {
            plan: target.clone(),
        });
        app.update(AppEvent::ResetStarted {
            target: target.clone(),
            label: "0099-demo".to_string(),
        });

        let (tx, _rx) = background_events();
        let (ev, status) = resolve_io(&mut app, AppEvent::StartRun, &tx).await;

        assert!(
            matches!(
                ev,
                AppEvent::OperationBlocked { target: ref blocked, ref attempted }
                    if blocked == &target && attempted == "Start run"
            ),
            "StartRun must open the operation notice while reset is in progress; got {ev:?}"
        );
        assert!(status.is_none());
        assert!(
            api.commands.lock().unwrap().is_empty(),
            "StartRun must not open/start while reset is in progress"
        );
    }

    /// An invalid plan without task documents cannot be opened for execution.
    #[tokio::test]
    async fn start_on_invalid_plan_without_task_documents_issues_nothing() {
        use crate::app::{App, AppEvent, TabContent};
        use async_trait::async_trait;
        use makina_core::api::{
            Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunView,
        };
        use std::sync::{Arc, Mutex};

        struct RecordingApi {
            commands: Mutex<Vec<Command>>,
        }
        #[async_trait]
        impl Api for RecordingApi {
            async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
                self.commands.lock().unwrap().push(command.clone());
                match command {
                    Command::OpenPlan { .. } => Ok(CommandOutcome::RunOpened { run: RunId(42) }),
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

        let api = Arc::new(RecordingApi {
            commands: Mutex::new(Vec::new()),
        });
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn Api>,
            vec![],
            std::path::PathBuf::from("."),
        );
        app.discovered_plans = vec![test_plan_entry(
            std::path::PathBuf::from("/tmp/docs/plans/0100-empty"),
            "0100-empty".to_string(),
            Vec::new(),
        )];
        let target = app.plan_identity_for_entry(&app.repo_root, &app.discovered_plans[0]);
        app.tabs.open_tab(TabContent::Plan {
            plan: target.clone(),
        });

        let (ev, status) = resolve_io_for_test(&mut app, AppEvent::StartRun).await;
        assert!(
            matches!(ev, AppEvent::PlanOpenStarted { target: ref opened } if opened == &target)
        );
        assert!(
            status.as_deref().is_some_and(|m| m.contains("Starting")),
            "Start on a tasks-less plan must begin generation; got {status:?}"
        );
        let commands = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let commands = api.commands.lock().unwrap().clone();
                if commands.len() >= 2 {
                    break commands;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("TASKS-less open/start did not complete");
        assert!(matches!(&commands[0], Command::OpenPlan { plan_dir }
            if plan_dir == &target.plan_dir));
        assert!(matches!(commands[1], Command::StartRun { run: RunId(42) }));
    }

    #[tokio::test]
    async fn generated_bundle_is_published_without_opening_or_starting_a_run() {
        use crate::app::{App, AppEvent};
        use async_trait::async_trait;
        use makina_core::api::{
            Api, ApiError, Command, CommandOutcome, Event, EventStream,
            GeneratedInitialStatusBlueprint, GeneratedPlanBlueprint, PlanGenerationReport, RunId,
            RunView,
        };
        use std::sync::{Arc, Mutex};

        struct RecordingApi {
            commands: Mutex<Vec<Command>>,
        }
        #[async_trait]
        impl Api for RecordingApi {
            async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
                self.commands.lock().unwrap().push(command.clone());
                match command {
                    Command::GeneratePlanBundle { .. } => Ok(CommandOutcome::PlanGenerated {
                        plan_dir: makina_core::plan::PlanKey::parse(
                            "docs/plans/0049-generated-plan",
                        )
                        .unwrap(),
                        registration_oid: "a".repeat(40),
                        report: PlanGenerationReport::default(),
                    }),
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

        let api = Arc::new(RecordingApi {
            commands: Mutex::new(Vec::new()),
        });
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn Api>,
            vec![],
            std::path::PathBuf::from("."),
        );
        let blueprint = GeneratedPlanBlueprint {
            slug: "generated-plan".into(),
            title: "Generated plan".into(),
            scope: "Scope".into(),
            architecture: "Architecture".into(),
            initial_status: GeneratedInitialStatusBlueprint {
                goal: "Goal".into(),
                root_cause: "Cause".into(),
                approach: "Approach".into(),
                outcome: String::new(),
                last_updated: "2026-07-20".into(),
            },
            workstreams: vec![],
            tasks: vec![],
        };

        let (_event, status) = resolve_io_for_test(
            &mut app,
            AppEvent::GeneratePlanBundle {
                project_root: std::path::PathBuf::from("."),
                blueprint,
            },
        )
        .await;
        assert!(
            status
                .as_deref()
                .is_some_and(|message| message.contains("Generating"))
        );

        let commands = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let commands = api.commands.lock().unwrap().clone();
                if !commands.is_empty() {
                    break commands;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("generation command did not finish");
        assert!(matches!(commands[0], Command::GeneratePlanBundle { .. }));
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(
            api.commands.lock().unwrap().len(),
            1,
            "generation must not open or start a run as a side effect"
        );
    }

    #[tokio::test]
    async fn start_on_disk_snapshot_reopens_canonical_project_plan() {
        use crate::app::{App, AppEvent};
        use async_trait::async_trait;
        use makina_core::api::{
            Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunStatus, RunView,
        };
        use std::sync::{Arc, Mutex};

        struct RecordingApi {
            commands: Mutex<Vec<Command>>,
            disk_run: RunView,
        }
        #[async_trait]
        impl Api for RecordingApi {
            async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
                self.commands.lock().unwrap().push(command.clone());
                match command {
                    Command::StartRun { run: RunId(900) } => {
                        Err(ApiError::UnknownRun { run: RunId(900) })
                    }
                    Command::OpenPlan { .. } => Ok(CommandOutcome::RunOpened { run: RunId(42) }),
                    _ => Ok(CommandOutcome::Acknowledged),
                }
            }
            async fn runs(&self) -> Vec<RunView> {
                Vec::new()
            }
            async fn run(&self, id: RunId) -> Option<RunView> {
                (id == self.disk_run.id).then(|| self.disk_run.clone())
            }
            fn subscribe(&self) -> EventStream {
                Box::pin(futures::stream::empty::<Event>())
            }
        }

        let disk_run = RunView {
            id: RunId(900),
            run_uid: "disk-snapshot".to_string(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Pending,
            project: "repo-b".to_string(),
            tasks: Vec::new(),
            report: makina_core::api::IngestionReport::default(),
        };
        let api = Arc::new(RecordingApi {
            commands: Mutex::new(Vec::new()),
            disk_run: disk_run.clone(),
        });
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn Api>,
            vec![disk_run],
            std::path::PathBuf::from("/work/repo-a"),
        );
        app.opened_folders = vec![
            std::path::PathBuf::from("/work/repo-a"),
            std::path::PathBuf::from("/work/repo-b"),
        ];
        app.tree_cursor = app
            .visible_tree_nodes()
            .iter()
            .position(|node| matches!(node, crate::app::TreeNode::Run { run: 0 }));

        let (event, status) = resolve_io_for_test(&mut app, AppEvent::StartRun).await;
        let expected = makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap();
        assert!(matches!(
            event,
            AppEvent::PlanOpenStarted { target }
                if target.plan_dir == expected
        ));
        assert!(
            status
                .as_deref()
                .is_some_and(|message| message.contains("Starting"))
        );

        let commands = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let commands = api.commands.lock().unwrap().clone();
                if commands.len() >= 3 {
                    break commands;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "disk snapshot open/start did not complete; commands: {:?}",
                api.commands.lock().unwrap()
            )
        });
        assert!(matches!(commands[0], Command::StartRun { run: RunId(900) }));
        assert!(matches!(&commands[1], Command::OpenPlan { plan_dir }
            if plan_dir == &expected));
        assert!(matches!(commands[2], Command::StartRun { run: RunId(42) }));
    }

    /// A command error from the api is surfaced as a status message (not dropped).
    #[tokio::test]
    async fn control_command_error_is_surfaced() {
        use crate::app::{App, AppEvent};
        use crate::placeholder::PlaceholderApi;
        use makina_core::api::{RunId, RunStatus, RunView};
        use std::sync::Arc;

        // PlaceholderApi::empty() has no runs, so a PauseRun for a run that
        // exists in the App view but NOT in the api returns UnknownRun.
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(999),
            run_uid: String::new(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        let (_ev, status) = resolve_io_for_test(&mut app, AppEvent::PauseRun).await;
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
        let mut app = App::new(api, vec![], tmpdir.path().to_path_buf());
        let (tx, mut rx) = background_events();

        let (resolved, status) = resolve_io(&mut app, AppEvent::OpenBrowser, &tx).await;
        assert!(
            matches!(resolved, AppEvent::OpenBrowser),
            "OpenBrowser must return immediately"
        );
        // The busy spinner (set by the OpenBrowser update arm) replaces the old
        // static status message, so resolve_io returns no status here.
        assert_eq!(status, None);
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
        let file_path = std::path::PathBuf::from("/tmp/example-plan-marker");
        app.browser = Some(FileBrowser::new(
            std::path::PathBuf::from("/tmp"),
            vec![DirEntry {
                name: "example-plan-marker".to_string(),
                path: file_path,
                is_dir: false,
            }],
        ));

        let (resolved, status) = resolve_io_for_test(&mut app, AppEvent::BrowserActivate).await;
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
            msg.contains("example-plan-marker"),
            "status must contain the file stem; got {msg:?}"
        );
    }

    // ── Folder browser IO resolution (plan 0043) ──────────────────────────────

    /// `read_dir_event_folders_only` must include the `..` parent entry and
    /// subdirectories, but skip regular files and dotfiles entirely — the
    /// folder browser only ever lets the user pick a directory.
    #[tokio::test]
    async fn read_dir_event_folders_only_filters_to_directories() {
        let tmpdir = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmpdir.path().join("subdir")).unwrap();
        std::fs::write(tmpdir.path().join("file.txt"), b"x").unwrap();
        std::fs::write(tmpdir.path().join(".hidden-file"), b"x").unwrap();
        std::fs::create_dir(tmpdir.path().join(".hidden-dir")).unwrap();

        let event = read_dir_event_folders_only(tmpdir.path()).await;
        match event {
            AppEvent::BrowserOpened { dir, entries } => {
                assert_eq!(dir, tmpdir.path());
                assert!(
                    entries.iter().any(|e| e.name == ".."),
                    "must include the '..' parent entry"
                );
                assert!(
                    entries.iter().any(|e| e.name == "subdir" && e.is_dir),
                    "must include the visible subdirectory"
                );
                assert!(
                    !entries.iter().any(|e| e.name == "file.txt"),
                    "must NOT include regular files: {entries:?}"
                );
                assert!(
                    !entries.iter().any(|e| e.name == ".hidden-file"),
                    "must NOT include hidden files: {entries:?}"
                );
                assert!(
                    !entries.iter().any(|e| e.name == ".hidden-dir"),
                    "must NOT include hidden directories: {entries:?}"
                );
            }
            other => panic!("expected BrowserOpened, got {other:?}"),
        }
    }

    /// `resolve_io(OpenFolder)` must pass the event straight through (so
    /// `App::update`'s `OpenFolder` arm can set `folder_browser_purpose` and the
    /// busy spinner) while spawning a background folders-only read of
    /// `dirs::home_dir()` that eventually yields `BrowserOpened`.
    #[tokio::test]
    async fn open_folder_io_reads_home_dir_and_filters_to_directories() {
        use std::path::PathBuf;

        let mut app = test_app();
        let (tx, mut rx) = background_events();

        let (resolved, status) = resolve_io(&mut app, AppEvent::OpenFolder, &tx).await;
        assert!(
            matches!(resolved, AppEvent::OpenFolder),
            "OpenFolder must return immediately so update() can set the purpose"
        );
        assert_eq!(status, None);

        let opened = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for BrowserOpened")
            .expect("background channel closed");
        match opened {
            AppEvent::BrowserOpened { dir, entries } => {
                assert_eq!(dir, dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")));
                assert!(
                    entries.iter().all(|e| e.is_dir),
                    "folder browser listing must contain only directories: {entries:?}"
                );
            }
            other => panic!("expected BrowserOpened, got {other:?}"),
        }
    }

    /// `resolve_io(InitializeFolderRequested)` mirrors `OpenFolder`: it must
    /// pass through unchanged (letting `update()` set the `InitializeFolder`
    /// purpose) and spawn the same folders-only read of `$HOME`. Before this
    /// fix, `InitializeFolderRequested` had NO resolve_io arm, so no
    /// `BrowserOpened` was ever produced and the app got stuck showing "Opening
    /// folder browser…" forever.
    #[tokio::test]
    async fn initialize_folder_requested_io_reads_home_dir() {
        let mut app = test_app();
        let (tx, mut rx) = background_events();

        let (resolved, status) =
            resolve_io(&mut app, AppEvent::InitializeFolderRequested, &tx).await;
        assert!(
            matches!(resolved, AppEvent::InitializeFolderRequested),
            "InitializeFolderRequested must return immediately so update() can set the purpose"
        );
        assert_eq!(status, None);

        let opened = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for BrowserOpened — InitializeFolderRequested must spawn a directory read")
            .expect("background channel closed");
        assert!(matches!(opened, AppEvent::BrowserOpened { .. }));
    }

    /// `resolve_io(FolderBrowserActivate { purpose: OpenFolder })` on a
    /// directory entry must resolve to `CloseBrowser` immediately (closing the
    /// modal in this same pass) and send `OpenFolderSelected { path }` on
    /// `background_tx` for the next loop iteration.
    #[tokio::test]
    async fn folder_browser_activate_open_folder_sends_selected_and_closes() {
        use crate::app::{FolderBrowserPurpose, Mode};
        use crate::browser::{DirEntry, FileBrowser};
        use std::path::PathBuf;

        let mut app = test_app();
        app.mode = Mode::FolderBrowser {
            purpose: FolderBrowserPurpose::OpenFolder,
        };
        let folder_path = PathBuf::from("/home/user/projects/widgets");
        app.browser = Some(FileBrowser::new(
            PathBuf::from("/home/user/projects"),
            vec![DirEntry {
                name: "widgets".to_string(),
                path: folder_path.clone(),
                is_dir: true,
            }],
        ));
        let (tx, mut rx) = background_events();

        let (resolved, status) = resolve_io(
            &mut app,
            AppEvent::FolderBrowserActivate {
                purpose: FolderBrowserPurpose::OpenFolder,
            },
            &tx,
        )
        .await;
        assert!(
            matches!(resolved, AppEvent::CloseBrowser),
            "activating a folder must resolve to CloseBrowser immediately"
        );
        assert_eq!(status, None);

        let selected = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for OpenFolderSelected")
            .expect("background channel closed");
        match selected {
            AppEvent::OpenFolderSelected { path } => assert_eq!(path, folder_path),
            other => panic!("expected OpenFolderSelected, got {other:?}"),
        }
    }

    /// Same as above but for the `InitializeFolder` purpose: the emitted event
    /// must be `InitializeFolderSelected`, not `OpenFolderSelected`.
    #[tokio::test]
    async fn folder_browser_activate_initialize_folder_sends_selected() {
        use crate::app::{FolderBrowserPurpose, Mode};
        use crate::browser::{DirEntry, FileBrowser};
        use std::path::PathBuf;

        let mut app = test_app();
        app.mode = Mode::FolderBrowser {
            purpose: FolderBrowserPurpose::InitializeFolder,
        };
        let folder_path = PathBuf::from("/home/user/scratch");
        app.browser = Some(FileBrowser::new(
            PathBuf::from("/home/user"),
            vec![DirEntry {
                name: "scratch".to_string(),
                path: folder_path.clone(),
                is_dir: true,
            }],
        ));
        let (tx, mut rx) = background_events();

        let (resolved, _status) = resolve_io(
            &mut app,
            AppEvent::FolderBrowserActivate {
                purpose: FolderBrowserPurpose::InitializeFolder,
            },
            &tx,
        )
        .await;
        assert!(matches!(resolved, AppEvent::CloseBrowser));

        let selected = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for InitializeFolderSelected")
            .expect("background channel closed");
        match selected {
            AppEvent::InitializeFolderSelected { path } => assert_eq!(path, folder_path),
            other => panic!("expected InitializeFolderSelected, got {other:?}"),
        }
    }

    /// **Regression test for the Enter-on-folder panic, for a path that does
    /// NOT exist on disk.** Pressing Enter on a folder dispatches
    /// `FolderBrowserActivate`, which sends `OpenFolderSelected` via
    /// `background_tx`; that event loops back through `resolve_io` on the NEXT
    /// iteration (see `run()`'s `background_rx.recv()` arm). Before this fix,
    /// `resolve_io` had no arm for `OpenFolderSelected` (or
    /// `InitializeFolderSelected`), so it fell through the wildcard `other =>
    /// (other, None)` straight into `App::update`'s `unreachable!()` arm and
    /// panicked.
    ///
    /// This nonexistent path takes `resolve_io`'s `!path.is_dir()` error
    /// branch (see `resolve_io_open_folder_selected_persists_and_discovers`
    /// below for the real-directory success path with full persistence
    /// assertions), so it must still resolve to `Tick` — but for a different
    /// reason than a no-op: the path is rejected and an error is pushed.
    #[tokio::test]
    async fn resolve_io_does_not_panic_on_open_folder_selected_roundtrip() {
        use std::path::PathBuf;

        let mut app = test_app();
        let (tx, _rx) = background_events();

        let (resolved, status) = resolve_io(
            &mut app,
            AppEvent::OpenFolderSelected {
                path: PathBuf::from("/home/user/projects/widgets"),
            },
            &tx,
        )
        .await;
        assert!(
            matches!(resolved, AppEvent::Tick),
            "OpenFolderSelected must resolve to Tick, not pass through to update()'s unreachable! arm"
        );
        assert_eq!(status, None);
        assert!(
            app.error_messages
                .iter()
                .any(|m| m.text.contains("not a directory")),
            "a nonexistent path must push an error, not silently succeed"
        );
        assert!(
            app.opened_folders.is_empty(),
            "a rejected path must not be added to opened_folders"
        );
        // Feed the resolved event through App::update too — this is exactly the
        // step that panicked before the fix (resolve_io's fallthrough handed the
        // ORIGINAL OpenFolderSelected straight to update()'s unreachable! arm).
        app.update(resolved);
    }

    /// Same regression coverage for `InitializeFolderSelected`, on a path
    /// where `folder_init::initialize_folder` fails (no such directory, so
    /// `git init` errors) — the error path must push an error message into
    /// the error pane rather than panic, and return to `Mode::Normal` with no
    /// status message.
    #[tokio::test]
    async fn resolve_io_does_not_panic_on_initialize_folder_selected_roundtrip() {
        use std::path::PathBuf;

        let mut app = test_app();
        app.mode = crate::app::Mode::FolderBrowser {
            purpose: crate::app::FolderBrowserPurpose::InitializeFolder,
        };
        let (tx, _rx) = background_events();

        let (resolved, status) = resolve_io(
            &mut app,
            AppEvent::InitializeFolderSelected {
                path: PathBuf::from("/nonexistent/path/for/makina/tests/scratch"),
            },
            &tx,
        )
        .await;
        assert!(
            matches!(resolved, AppEvent::Tick),
            "InitializeFolderSelected must resolve to Tick, not pass through to update()'s unreachable! arm"
        );
        assert_eq!(status, None, "the error path must not set a status message");
        assert!(
            !app.error_messages.is_empty(),
            "a failed initialize_folder() must push an error message into the error pane"
        );
        assert!(
            app.error_messages
                .iter()
                .any(|m| m.text.contains("Failed to initialize folder")),
            "the error pane message must explain the initialize_folder failure"
        );
        assert_eq!(
            app.mode,
            crate::app::Mode::Normal,
            "must return to Normal mode even when initialize_folder fails"
        );
        app.update(resolved);
    }

    /// **`InitializeFolderSelected` success path (task
    /// `handle-initialize-folder-event`).** Selecting a real, writable
    /// directory must: call `folder_init::initialize_folder()` to bootstrap
    /// git + docs/plans, add it to `app.opened_folders`, persist the
    /// workspace (via the test-injected `workspace_path_override`), and
    /// return to `Mode::Normal` with a "Folder initialized" status message.
    #[tokio::test]
    async fn resolve_io_initialize_folder_selected_success_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let folder = tmp.path().join("new-project");
        std::fs::create_dir_all(&folder).expect("create folder");

        let workspace_file = tmp.path().join("workspace.toml");

        let mut app = test_app();
        app.workspace_path_override = Some(workspace_file.clone());
        app.mode = crate::app::Mode::FolderBrowser {
            purpose: crate::app::FolderBrowserPurpose::InitializeFolder,
        };
        let (tx, _rx) = background_events();

        let (resolved, status) = resolve_io(
            &mut app,
            AppEvent::InitializeFolderSelected {
                path: folder.clone(),
            },
            &tx,
        )
        .await;

        assert!(matches!(resolved, AppEvent::Tick));
        assert_eq!(
            status,
            Some(format!("Folder initialized: {}", folder.display()))
        );
        assert!(
            folder.join(".git").exists(),
            "initialize_folder must bootstrap a git repo in the selected folder"
        );
        assert!(
            folder.join("docs").join("plans").join("README.md").exists(),
            "initialize_folder must write docs/plans/README.md"
        );
        assert!(
            app.opened_folders.contains(&folder),
            "InitializeFolderSelected must push the initialized folder into opened_folders"
        );
        assert!(
            app.workspace.opened_folders.contains(&folder),
            "the in-memory Workspace must also track the newly initialized folder"
        );
        assert_eq!(
            app.mode,
            crate::app::Mode::Normal,
            "must return to Normal mode after initializing a folder"
        );
        assert!(
            workspace_file.exists(),
            "InitializeFolderSelected must persist the workspace to the injected path, \
            not the operator's real $HOME/.makina/workspace.toml"
        );
        let saved = crate::workspace::Workspace::load_from(&workspace_file)
            .expect("saved workspace must parse");
        assert!(saved.opened_folders.contains(&folder));
        assert!(
            app.error_messages.is_empty(),
            "success path must not push errors"
        );
    }

    /// **`OpenFolderSelected` success path (task `handle-folder-open-close-events`).**
    /// Selecting a *real* directory must: add it to `app.opened_folders`,
    /// persist the workspace to disk (via the test-injected
    /// `workspace_path_override`, never the operator's real `$HOME`), and
    /// return to `Mode::Normal` with a "Folder opened" status message.
    #[tokio::test]
    async fn resolve_io_open_folder_selected_persists_and_discovers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let folder = tmp.path().join("my-project");
        std::fs::create_dir_all(&folder).expect("create folder");
        let initialized = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&folder)
            .status()
            .expect("run git init");
        assert!(initialized.success(), "git init fixture must succeed");

        let workspace_file = tmp.path().join("workspace.toml");

        let mut app = test_app();
        app.workspace_path_override = Some(workspace_file.clone());
        app.mode = crate::app::Mode::FolderBrowser {
            purpose: crate::app::FolderBrowserPurpose::OpenFolder,
        };
        let (tx, _rx) = background_events();

        let (resolved, status) = resolve_io(
            &mut app,
            AppEvent::OpenFolderSelected {
                path: folder.clone(),
            },
            &tx,
        )
        .await;

        assert!(matches!(resolved, AppEvent::Tick));
        assert_eq!(status, Some(format!("Folder opened: {}", folder.display())));
        assert!(
            app.opened_folders.contains(&folder),
            "OpenFolderSelected must push the selected folder into opened_folders"
        );
        assert!(
            app.workspace.opened_folders.contains(&folder),
            "the in-memory Workspace must also track the newly opened folder"
        );
        assert_eq!(
            app.mode,
            crate::app::Mode::Normal,
            "must return to Normal mode after opening a folder"
        );
        assert!(
            workspace_file.exists(),
            "OpenFolderSelected must persist the workspace to the injected path, \
            not the operator's real $HOME/.makina/workspace.toml"
        );
        let saved = crate::workspace::Workspace::load_from(&workspace_file)
            .expect("saved workspace must parse");
        assert!(saved.opened_folders.contains(&folder));
    }

    #[tokio::test]
    async fn open_folder_registration_failure_does_not_authorize_project() {
        use async_trait::async_trait;
        use makina_core::api::{
            Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunView,
        };
        use std::sync::Arc;

        struct RejectRegistration;
        #[async_trait]
        impl Api for RejectRegistration {
            async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
                match command {
                    Command::RegisterProject { project_root } => Err(ApiError::InvalidCommand {
                        reason: format!("{} is not allowed", project_root.display()),
                    }),
                    _ => Ok(CommandOutcome::Acknowledged),
                }
            }
            async fn runs(&self) -> Vec<RunView> {
                Vec::new()
            }
            async fn run(&self, _id: RunId) -> Option<RunView> {
                None
            }
            fn subscribe(&self) -> EventStream {
                Box::pin(futures::stream::empty::<Event>())
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("rejected");
        std::fs::create_dir_all(&folder).unwrap();
        let initialized = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&folder)
            .status()
            .expect("run git init");
        assert!(initialized.success(), "git init fixture must succeed");
        let mut app = App::new(
            Arc::new(RejectRegistration),
            Vec::new(),
            temp.path().to_path_buf(),
        );
        app.workspace_path_override = Some(temp.path().join("workspace.toml"));
        let (tx, _rx) = background_events();

        let (event, status) = resolve_io(
            &mut app,
            AppEvent::OpenFolderSelected {
                path: folder.clone(),
            },
            &tx,
        )
        .await;

        assert!(matches!(event, AppEvent::Tick));
        assert!(
            status
                .as_deref()
                .is_some_and(|message| message.contains("register"))
        );
        assert!(!app.opened_folders.contains(&folder));
        assert!(!app.workspace.opened_folders.contains(&folder));
        assert!(
            app.error_messages
                .iter()
                .any(|message| message.text.contains("register"))
        );
    }

    /// **`OpenFolderSelected` re-opening an already-opened folder is
    /// idempotent** — no duplicate entries in `opened_folders`.
    #[tokio::test]
    async fn resolve_io_open_folder_selected_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let folder = tmp.path().join("my-project");
        std::fs::create_dir_all(&folder).expect("create folder");
        let workspace_file = tmp.path().join("workspace.toml");

        let mut app = test_app();
        app.workspace_path_override = Some(workspace_file);
        app.opened_folders.push(folder.clone());
        app.workspace.add_folder(folder.clone());
        let (tx, _rx) = background_events();

        let _ = resolve_io(
            &mut app,
            AppEvent::OpenFolderSelected {
                path: folder.clone(),
            },
            &tx,
        )
        .await;

        assert_eq!(
            app.opened_folders.iter().filter(|p| **p == folder).count(),
            1,
            "re-selecting an already-opened folder must not duplicate it"
        );
    }

    /// **`CloseFolderRequested` fallback: with no folder highlighted in the
    /// sidebar it shows a selectable list of opened folders.** (`test_app`
    /// leaves `tree_cursor` at `None`, so no folder is focused here.) It must
    /// populate `app.browser` with one entry per `app.opened_folders` and enter
    /// `Mode::FolderBrowser { purpose: CloseFolders }`.
    #[tokio::test]
    async fn resolve_io_close_folder_requested_lists_opened_folders() {
        use crate::app::{FolderBrowserPurpose, Mode};
        use std::path::PathBuf;

        let mut app = test_app();
        let folder_a = PathBuf::from("/home/user/project-a");
        let folder_b = PathBuf::from("/home/user/project-b");
        app.opened_folders.push(folder_a.clone());
        app.opened_folders.push(folder_b.clone());
        let (tx, _rx) = background_events();

        let (resolved, _status) = resolve_io(&mut app, AppEvent::CloseFolderRequested, &tx).await;
        assert!(matches!(resolved, AppEvent::Tick));

        assert_eq!(
            app.mode,
            Mode::FolderBrowser {
                purpose: FolderBrowserPurpose::CloseFolders
            },
            "CloseFolderRequested must open the folder browser with the CloseFolders purpose"
        );
        let browser = app.browser.as_ref().expect("browser must be populated");
        let paths: Vec<_> = browser.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(
            paths,
            vec![folder_a, folder_b],
            "the browser must list every opened folder as a selectable entry"
        );
        assert!(browser.entries.iter().all(|e| e.is_dir));
    }

    /// When the sidebar highlights a folder (or a plan/task under one),
    /// `CloseFolderRequested` closes *that* folder directly — no picker modal.
    #[tokio::test]
    async fn resolve_io_close_folder_requested_closes_highlighted_folder() {
        use crate::app::{Mode, TreeNode};

        let tmp = tempfile::tempdir().expect("tempdir");
        let folder_a = tmp.path().join("project-a");
        let folder_b = tmp.path().join("project-b");

        let mut app = test_app();
        app.workspace_path_override = Some(tmp.path().join("workspace.toml"));
        app.opened_folders.push(folder_a.clone());
        app.opened_folders.push(folder_b.clone());
        app.workspace.opened_folders = app.opened_folders.iter().cloned().collect();

        // Collapsed, plan-less folders flatten to [Folder{0}, Folder{1}].
        assert_eq!(
            app.visible_tree_nodes(),
            vec![
                TreeNode::Folder { folder_idx: 0 },
                TreeNode::Folder { folder_idx: 1 },
            ]
        );
        // Highlight the second folder (project-b).
        app.tree_cursor = Some(1);

        let (tx, _rx) = background_events();
        let (resolved, status) = resolve_io(&mut app, AppEvent::CloseFolderRequested, &tx).await;

        assert!(matches!(resolved, AppEvent::Tick));
        assert_eq!(
            app.opened_folders,
            vec![folder_a],
            "only the highlighted folder (project-b) must be closed"
        );
        assert_eq!(app.mode, Mode::Normal, "must not enter the picker modal");
        assert!(
            app.browser.is_none(),
            "highlighted-folder close must not open the folder browser"
        );
        assert_eq!(
            status,
            Some(format!("Folder closed: {}", folder_b.display())),
        );
    }

    /// `CloseFolderRequested` with no folder highlighted falls back to the
    /// selectable list. With no opened folders it must still open an (empty)
    /// browser rather than panic or silently no-op.
    #[tokio::test]
    async fn resolve_io_close_folder_requested_with_no_folders_shows_empty_list() {
        use crate::app::{FolderBrowserPurpose, Mode};

        let mut app = test_app();
        let (tx, _rx) = background_events();

        let _ = resolve_io(&mut app, AppEvent::CloseFolderRequested, &tx).await;

        assert_eq!(
            app.mode,
            Mode::FolderBrowser {
                purpose: FolderBrowserPurpose::CloseFolders
            }
        );
        assert!(
            app.browser
                .as_ref()
                .expect("browser set")
                .entries
                .is_empty()
        );
    }

    /// **`CloseFolderConfirmed` success path (task
    /// `handle-folder-open-close-events`).** Confirming removal of a folder
    /// must: remove it from `app.opened_folders`, persist the workspace (via
    /// the injected path, never the real `$HOME`), return to `Mode::Normal`,
    /// and set a "Folder closed" status message.
    #[tokio::test]
    async fn resolve_io_close_folder_confirmed_removes_and_persists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let folder_a = tmp.path().join("project-a");
        let folder_b = tmp.path().join("project-b");
        let workspace_file = tmp.path().join("workspace.toml");

        let mut app = test_app();
        app.workspace_path_override = Some(workspace_file.clone());
        app.opened_folders.push(folder_a.clone());
        app.opened_folders.push(folder_b.clone());
        app.workspace.add_folder(folder_a.clone());
        app.workspace.add_folder(folder_b.clone());
        app.mode = crate::app::Mode::FolderBrowser {
            purpose: crate::app::FolderBrowserPurpose::CloseFolders,
        };
        let (tx, _rx) = background_events();

        let (resolved, status) = resolve_io(
            &mut app,
            AppEvent::CloseFolderConfirmed {
                path: folder_a.clone(),
            },
            &tx,
        )
        .await;

        assert!(matches!(resolved, AppEvent::Tick));
        assert_eq!(
            status,
            Some(format!("Folder closed: {}", folder_a.display()))
        );
        assert!(
            !app.opened_folders.contains(&folder_a),
            "CloseFolderConfirmed must remove the folder from opened_folders"
        );
        assert!(
            app.opened_folders.contains(&folder_b),
            "closing one folder must not remove others"
        );
        assert!(!app.workspace.opened_folders.contains(&folder_a));
        assert_eq!(
            app.mode,
            crate::app::Mode::Normal,
            "must return to Normal mode after closing a folder"
        );
        assert!(
            workspace_file.exists(),
            "CloseFolderConfirmed must persist the workspace to the injected path"
        );
        let saved = crate::workspace::Workspace::load_from(&workspace_file)
            .expect("saved workspace must parse");
        assert!(!saved.opened_folders.contains(&folder_a));
        assert!(saved.opened_folders.contains(&folder_b));
    }

    /// The folder-browser key-translation branch (`translate_key` under
    /// `browsing` when `app.is_folder_browsing()`) must dispatch
    /// `FolderBrowserActivate` on Enter (carrying the current purpose),
    /// `CloseBrowser` on Esc, `FolderBrowserParent` on Backspace, and
    /// `FolderBrowserUp`/`FolderBrowserDown` on Up/Down — distinct from the
    /// plain file-browser keymap (`BrowserActivate`/`BrowserParent`/etc).
    #[test]
    fn translate_key_folder_browser_keymap() {
        use crate::app::{FolderBrowserPurpose, Mode};
        use crate::browser::FileBrowser;
        use std::path::PathBuf;

        let mut app = test_app();
        app.mode = Mode::FolderBrowser {
            purpose: FolderBrowserPurpose::OpenFolder,
        };
        app.browser = Some(FileBrowser::new(PathBuf::from("/home/user"), vec![]));
        let modal = ModalState {
            browsing: true,
            ..ModalState::default()
        };

        assert!(matches!(
            translate_key(
                crossterm::event::KeyEvent {
                    code: KeyCode::Enter,
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press,
                    state: KeyEventState::NONE,
                },
                modal,
                crate::app::Panel::Sidebar,
                false,
                &app,
            ),
            AppEvent::FolderBrowserActivate {
                purpose: FolderBrowserPurpose::OpenFolder
            }
        ));

        assert!(matches!(
            translate_key(
                crossterm::event::KeyEvent {
                    code: KeyCode::Esc,
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press,
                    state: KeyEventState::NONE,
                },
                modal,
                crate::app::Panel::Sidebar,
                false,
                &app,
            ),
            AppEvent::CloseBrowser
        ));

        assert!(matches!(
            translate_key(
                crossterm::event::KeyEvent {
                    code: KeyCode::Down,
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press,
                    state: KeyEventState::NONE,
                },
                modal,
                crate::app::Panel::Sidebar,
                false,
                &app,
            ),
            AppEvent::FolderBrowserDown
        ));

        assert!(matches!(
            translate_key(
                crossterm::event::KeyEvent {
                    code: KeyCode::Up,
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press,
                    state: KeyEventState::NONE,
                },
                modal,
                crate::app::Panel::Sidebar,
                false,
                &app,
            ),
            AppEvent::FolderBrowserUp
        ));

        assert!(matches!(
            translate_key(
                crossterm::event::KeyEvent {
                    code: KeyCode::Backspace,
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press,
                    state: KeyEventState::NONE,
                },
                modal,
                crate::app::Panel::Sidebar,
                false,
                &app,
            ),
            AppEvent::FolderBrowserParent
        ));
    }

    /// **TUI ↔ CoreApi flow (the done-when through the event layer).**
    ///
    /// Drive the exact event-loop step that opens a file against the REAL
    /// `CoreApi`: set up a browser whose selection is a sample plan marker,
    /// call `resolve_io(BrowserActivate)` (which spawns `api.execute(OpenPlan)`),
    /// then drain `api.subscribe()` and feed the resulting `RunOpened` into
    /// `App::update` — asserting the Run appears in `app.runs`.
    #[tokio::test]
    #[should_panic(expected = "timed out waiting for RunOpened")]
    async fn browser_activate_file_opens_run_via_core_api_and_appears_in_app() {
        use crate::app::{App, AppEvent, Mode};
        use crate::browser::{DirEntry, FileBrowser};
        use makina_core::dependency::EdgeInferrer;
        use makina_core::interpreter::SourceProjectionUnavailable;
        use makina_core::orchestrator::CoreApi;
        use std::sync::Arc;

        // A plan marker written to a tempfile.
        let source = "# Flow — Task List\n\nPreamble.\n\n---\n## 0001 — S\n\n\
### only — Only task\nDoes a thing in `lib.rs`.\n- **Depends on:** —\n\
- **Done when:** it works.\n";
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("flow-feature.md");
        std::fs::write(&file_path, source).unwrap();

        // Real CoreApi with the deterministic interpreter (what main.rs uses).
        let interpreter = Arc::new(EdgeInferrer::new(Arc::new(
            SourceProjectionUnavailable::new(),
        )));
        // Execution deps (task 31): these tests only exercise OpenPlan, so a
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
        // OpenPlan against CoreApi and returns CloseBrowser + a status message
        // (now the transient "Interpreting …" one).
        let (tx, _rx) = background_events();
        let (resolved, status) = resolve_io(&mut app, AppEvent::BrowserActivate, &tx).await;
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

        // The CoreApi created the Run (direct query proves OpenPlan happened).
        let runs = api.runs().await;
        assert_eq!(runs.len(), 1, "CoreApi must have created exactly one run");
        assert_eq!(
            runs[0].plan_dir,
            makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap()
        );
        assert_eq!(runs[0].tasks.len(), 1, "the task must be interpreted");

        assert_eq!(
            app.runs.len(),
            1,
            "the opened Run must appear in app.runs via the RunOpened event"
        );
        assert_eq!(
            app.runs[0].plan_dir,
            makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap()
        );
    }

    // ── Task-population test (task 29): RunOpened → api.run() → RunLoaded ─────

    /// **Task population:** When a `RunOpened` core event is received by
    /// `resolve_api_event`, it must call `api.run(id).await` and return an
    /// `AppEvent::RunLoaded` carrying the full `RunView` (with tasks).
    ///
    /// This proves the async data-flow: `Event::RunOpened` → `resolve_api_event`
    /// → fetch full `RunView` → `AppEvent::RunLoaded` → `App::update` → tasks
    /// populated in `app.runs`.
    /// Simulate the exact construction that main.rs performs for the api
    /// (using the same typed-source projection literals).
    /// Then create a CoreApi and assert that an OpenPlan of a known-good sample
    /// produces the expected graph with zero backend involvement (the backend
    /// panics if called, proving ingestion path does not touch it).
    /// The test must be named exactly as shown and must fail before the 0029 change.
    #[test]
    fn log_pane_target_resolves_from_selection() {
        use crate::placeholder::PlaceholderApi;
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        use std::path::PathBuf;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(123),
            run_uid: "run-001-test".to_string(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Completed,
            project: "test".to_string(),
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new("my-task"),
                title: "Test Task".into(),
                state: TaskState::Done,
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
        let mut app = crate::app::App::new(api, vec![run], PathBuf::from("/test/repo"));
        app.selected_run = Some(0);
        app.selected_task = Some(0);

        assert_eq!(
            app.log_pane_target(),
            Some((RunId(123), TaskId::new("my-task"))),
            "log_pane_target must resolve the sidebar selection's (RunId, TaskId)"
        );
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

        // Project config must always be valid TOML.
        let project_contents = std::fs::read_to_string(&project_path).expect("read project config");
        let _: toml::Value =
            toml::from_str(&project_contents).expect("scaffold project config must be valid TOML");

        // Global config must be valid TOML and either:
        // (a) Contains an uncommented [backend] with a detected command, OR
        // (b) Lists the supported CLIs in a comment (no-detection case).
        //
        // The process $PATH during a test run is not controllable, so this
        // integration test cannot force which branch `write_doctor_scaffold`
        // takes; instead it detects which branch actually ran (by looking
        // for a genuinely uncommented `command = "..."` LINE, not merely the
        // substring, since the no-detection template's `# command = "gemini"`
        // also contains that substring) and asserts that branch's shape.
        // The branch logic itself is covered deterministically below by
        // `build_global_template_*` unit tests that call the pure builder
        // directly with constructed `Some`/`None` inputs.
        let global_contents = std::fs::read_to_string(&global_path).expect("read global config");
        let _: toml::Value =
            toml::from_str(&global_contents).expect("scaffold global config must be valid TOML");

        let has_uncommented_command_line = global_contents
            .lines()
            .any(|line| line.trim_start().starts_with("command = \""));

        if has_uncommented_command_line {
            // Detected branch: the uncommented command must name a real
            // KNOWN_AGENTS entry (not an arbitrary value).
            let names_known_agent = makina_core::preflight::KNOWN_AGENTS
                .iter()
                .any(|agent| global_contents.contains(&format!("command = \"{}\"", agent.command)));
            assert!(
                names_known_agent,
                "detected branch's uncommented command must name a KNOWN_AGENTS entry; got:\n{}",
                global_contents
            );
        } else {
            // No-detection branch: command must stay commented and every
            // KNOWN_AGENTS CLI must be named for the user to pick from.
            assert!(
                global_contents.contains("# command = "),
                "no-detection branch must leave command commented out; got:\n{}",
                global_contents
            );
            for agent in makina_core::preflight::KNOWN_AGENTS {
                assert!(
                    global_contents.contains(agent.command),
                    "no-detection branch must list KNOWN_AGENTS entry {:?}; got:\n{}",
                    agent.command,
                    global_contents
                );
            }
        }
    }

    /// `build_global_template` is pure, so the detected branch is covered
    /// deterministically here (independent of the process `$PATH`): given a
    /// constructed `Some(DetectedBackend)`, the template must contain a real
    /// uncommented `command = "..."` line for that backend and must NOT also
    /// emit the commented placeholder line from the no-detection template.
    #[test]
    fn build_global_template_some_writes_uncommented_backend_line() {
        let detected = Some(makina_core::preflight::DetectedBackend {
            agent: "gemini",
            command: "gemini".to_string(),
            args: vec!["--acp".to_string(), "--yolo".to_string()],
            resolved: std::path::PathBuf::from("/usr/local/bin/gemini"),
        });

        let template = build_global_template(&detected);

        let _: toml::Value =
            toml::from_str(&template).expect("detected-branch template must be valid TOML");

        let has_uncommented_command_line = template
            .lines()
            .any(|line| line.trim_start() == "command = \"gemini\"");
        assert!(
            has_uncommented_command_line,
            "detected branch must contain an uncommented `command = \"gemini\"` line; got:\n{template}"
        );
        assert!(
            !template.contains("# command = "),
            "detected branch must not also emit a commented command line; got:\n{template}"
        );
    }

    /// Given `None` (no agent found on PATH), the template must leave
    /// `command` commented out — no uncommented `command = "..."` line
    /// anywhere — and must name every `KNOWN_AGENTS` CLI in a comment.
    #[test]
    fn build_global_template_none_leaves_backend_commented() {
        let template = build_global_template(&None);

        let _: toml::Value =
            toml::from_str(&template).expect("no-detection template must be valid TOML");

        let has_uncommented_command_line = template
            .lines()
            .any(|line| line.trim_start().starts_with("command = \""));
        assert!(
            !has_uncommented_command_line,
            "no-detection branch must not contain an uncommented command line; got:\n{template}"
        );
        assert!(
            template.contains("# command = "),
            "no-detection branch must contain a commented command line; got:\n{template}"
        );
        for agent in makina_core::preflight::KNOWN_AGENTS {
            assert!(
                template.contains(agent.command),
                "no-detection branch must list KNOWN_AGENTS entry {:?}; got:\n{template}",
                agent.command
            );
        }
    }

    // ── Context-sensitive retry/reset action (plan 0017) ─────────────────────

    use async_trait::async_trait;
    use makina_core::api::{
        Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunStatus, RunView,
        TaskId, TaskState, TaskView,
    };
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    /// A stub api that records every `Command` it executes (for retry/reset tests).
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
                Command::OpenPlan { .. } => Ok(CommandOutcome::RunOpened { run: RunId(1) }),
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
            authored: None,
            id: TaskId::new(id),
            title: format!("Task {id}"),
            state,
            gate_iterations: 0,
            review_iterations: 0,
            depends_on: vec![],
            started_at: None,
            finished_at: None,
            failure_reason: None,
            entry_text: String::new(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status,
            project: String::new(),
            tasks,
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(
            Arc::clone(&api) as Arc<dyn Api>,
            vec![run],
            std::path::PathBuf::from("."),
        );
        // Retry tests navigate to a task node, which requires the run expanded.
        app.collapsed_runs.clear();
        (app, api)
    }

    /// Resolving retry/reset on a focused `Failed` task issues `RetryTask` with the
    /// focused run + task.
    #[tokio::test]
    async fn retry_action_on_failed_task_issues_retry_task() {
        use crate::app::{AppEvent, TreeNode};
        let (mut app, api) = retry_app(vec![task_view("a", TaskState::Failed)], RunStatus::Failed);
        // tree_cursor starts on the Run node; move down to the (Failed) task node.
        app.tree_move(1);
        assert!(
            matches!(app.focused_node(), Some(TreeNode::Task { task: 0, .. })),
            "the focused node must be the failed task"
        );

        let (_ev, status) = resolve_io_for_test(&mut app, AppEvent::RetryFocused).await;
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

    /// Resolving retry/reset on a run node with at least one `Failed` task
    /// issues `RetryFailedTasks`.
    #[tokio::test]
    async fn retry_action_on_run_node_issues_retry_failed() {
        use crate::app::{AppEvent, TreeNode};
        let (mut app, api) = retry_app(
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

        let (_ev, status) = resolve_io_for_test(&mut app, AppEvent::RetryFocused).await;
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
    async fn retry_action_noop_when_nothing_failed() {
        use crate::app::{AppEvent, TreeNode};
        let (mut app, api) = retry_app(vec![task_view("a", TaskState::Done)], RunStatus::Completed);
        app.tree_move(1); // focus the Done task.
        assert!(matches!(
            app.focused_node(),
            Some(TreeNode::Task { task: 0, .. })
        ));

        let (_ev, status) = resolve_io_for_test(&mut app, AppEvent::RetryFocused).await;
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

    /// Palette "Reset/retry focused task" action re-dispatches through resolve_io and
    /// issues `Command::RetryTask` / `Command::RetryFailedTasks` (plan 0041).
    #[tokio::test]
    async fn palette_retry_action_issues_retry_command() {
        use crate::app::{AppEvent, TreeNode};
        use makina_core::api::{Command, RunId, RunStatus, TaskState};
        // Create an app with a failed task using the retry_app helper.
        let (mut app, api) = retry_app(vec![task_view("a", TaskState::Failed)], RunStatus::Failed);
        // Move focus to the failed task.
        app.tree_move(1);
        assert!(
            matches!(app.focused_node(), Some(TreeNode::Task { task: 0, .. })),
            "the focused node must be the failed task"
        );

        // Open the palette.
        app.update(AppEvent::OpenCommandPalette);
        assert!(app.is_command_palette());
        let palette = app.command_palette.as_ref().expect("palette must be open");

        // Find the reset/retry action by label rather than depending on display order.
        let retry_index = palette
            .actions
            .iter()
            .position(|a| match a {
                crate::app::PaletteAction::Regular { label, .. } => {
                    *label == "Reset/retry focused task"
                }
                _ => false,
            })
            .expect("Reset/retry focused task action must exist");

        // Set selected to point to the Retry action.
        app.command_palette.as_mut().unwrap().selected = retry_index;

        // Resolve CommandPaletteExecute; it should re-dispatch RetryFocused over
        // background_tx and close the palette for this pass.
        let (tx, mut rx) = background_events();
        let (ev1, status1) = resolve_io(&mut app, AppEvent::CommandPaletteExecute, &tx).await;
        assert!(
            matches!(ev1, AppEvent::CloseCommandPalette),
            "CommandPaletteExecute must close the palette after re-dispatch; got {:?}",
            ev1
        );
        assert!(
            status1.is_none(),
            "no immediate status message; the re-dispatched event will produce one"
        );
        assert!(
            matches!(rx.try_recv(), Ok(AppEvent::RetryFocused)),
            "palette action must enqueue RetryFocused for a resolve_io pass"
        );

        // Manually simulate the re-dispatch: resolve RetryFocused through resolve_io,
        // which calls retry_focused and issues the command.
        let (ev2, status2) = resolve_io_for_test(&mut app, AppEvent::RetryFocused).await;
        assert!(
            matches!(ev2, AppEvent::Tick),
            "RetryFocused resolves to Tick; got {:?}",
            ev2
        );
        assert!(status2.is_some(), "retry must surface a status message");

        // Verify the orchestrator received the retry command.
        let cmds = api.commands.lock().unwrap().clone();
        assert_eq!(cmds.len(), 1, "exactly one command must be issued");
        assert!(
            matches!(
                &cmds[0],
                Command::RetryTask { run: RunId(7), task } if task.0 == "a"
            ),
            "Retry action on a Failed task must issue RetryTask{{run:7, task:a}}; got {:?}",
            cmds[0]
        );
    }

    /// The status bar advertises the command palette, not removed run-control keys.
    #[test]
    fn status_bar_advertises_palette_for_run_controls() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let api = RetryRecordingApi::new();
        let run = RunView {
            id: RunId(7),
            run_uid: String::new(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
            rendered.contains("[^P] cmds/run"),
            "the status bar must advertise palette-run controls; rendered: {rendered}"
        );
        assert!(
            !rendered.contains("[r] retry"),
            "the status bar must not advertise the removed [r] retry key; rendered: {rendered}"
        );
    }

    #[test]
    fn ctrl_p_opens_palette() {
        // Ctrl+P from Normal mode (all flags false) must open the palette.
        let ev = key_press(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::OpenCommandPalette
        ));
    }

    #[tokio::test]
    async fn palette_enter_executes_selected_action() {
        // With command_palette=true, Enter must map to CommandPaletteExecute.
        let app = test_app();
        let ev = key_press(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    command_palette: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
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

        // Select Settings by label rather than depending on display order.
        if let Some(ref mut palette) = app.command_palette {
            palette.selected = palette
                .actions
                .iter()
                .position(|action| action.label() == "Settings")
                .expect("Settings action must exist");
        }

        // Resolve CommandPaletteExecute; it should enqueue Settings for a fresh
        // resolve_io pass and close the palette for this pass.
        let (tx, mut rx) = background_events();
        let (resolved_event, status) =
            resolve_io(&mut app, AppEvent::CommandPaletteExecute, &tx).await;

        assert!(status.is_none(), "palette dispatch should not emit status");
        assert!(
            matches!(resolved_event, AppEvent::CloseCommandPalette),
            "palette execute with Settings selected must close the palette"
        );
        assert!(
            matches!(rx.try_recv(), Ok(AppEvent::OpenSettings)),
            "palette execute with Settings selected must enqueue OpenSettings"
        );
    }

    #[test]
    fn palette_esc_closes() {
        // Esc from the palette must map to CloseCommandPalette.
        let app = test_app();
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    command_palette: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::CloseCommandPalette
        ));
    }

    #[tokio::test]
    async fn edit_and_commit_writes_config() {
        // Create a temp directory for the repo root.
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let repo_root = tmpdir.path();

        // Write a pre-existing project config with fields Settings does not own.
        let existing_config = r#"
base_branch = "develop"
concurrency = 4

[[gates]]
name = "test"
command = "cargo test"

[roles.developer]
system_prompt = "Follow the repository style."
system_prompt_mode = "append"

[caps]
gate_iterations = 7
reviewer_iterations = 3
wall_clock_secs = 1200

[merge]
final = "squash"
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

        // Move to finalization mode and select Stage.
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsNextOption);

        // Commit settings via resolve_io.
        let (resolved_event, status) =
            resolve_io_for_test(&mut app, AppEvent::SettingsCommit).await;

        // Status should be "Settings saved".
        assert_eq!(status, Some("Settings saved".to_string()));
        assert!(matches!(
            &resolved_event,
            AppEvent::SettingsSaved { project_root, .. } if project_root == repo_root
        ));
        app.update(resolved_event);
        assert!(!app.is_settings(), "a successful save must close the modal");
        assert_eq!(app.caps.gate_iterations, 10);
        assert_eq!(app.concurrency, 8);
        assert_eq!(app.final_merge, makina_core::config::FinalMerge::Stage);

        // Re-read the config file and verify it was updated.
        let new_config_str = std::fs::read_to_string(&config_path).expect("read config");
        let new_config =
            makina_core::config::ProjectConfig::from_toml_str(&new_config_str, "project")
                .expect("parse project config");

        // Verify the new caps.
        let caps = new_config.caps.as_ref().expect("caps must be written");
        assert_eq!(
            caps.gate_iterations,
            Some(10),
            "gate_iterations must be updated"
        );
        assert_eq!(
            new_config.concurrency,
            Some(8),
            "concurrency must be updated"
        );
        assert_eq!(
            new_config.merge.expect("merge must be written").final_,
            makina_core::config::FinalMerge::Stage,
            "final merge mode must be updated"
        );
        assert_eq!(
            caps.reviewer_iterations,
            Some(3),
            "reviewer_iterations must be unchanged"
        );
        assert_eq!(
            caps.wall_clock_secs,
            Some(1200),
            "wall_clock_secs must be unchanged"
        );

        // Verify project-owned fields are preserved.
        assert_eq!(new_config.base_branch, "develop");
        assert_eq!(new_config.gates.len(), 1);
        assert_eq!(new_config.gates[0].name, "test");
        assert_eq!(new_config.gates[0].command, "cargo test");
        assert_eq!(
            new_config
                .roles
                .developer
                .as_ref()
                .and_then(|role| role.system_prompt.as_deref()),
            Some("Follow the repository style."),
            "project role prompt must be preserved"
        );
    }

    #[tokio::test]
    async fn settings_write_failure_keeps_modal_open_with_the_error() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let tmpdir = tempfile::tempdir().expect("tempdir");
        let blocked_root = tmpdir.path().join("not-a-directory");
        std::fs::write(&blocked_root, "blocks .makina directory creation")
            .expect("write blocking file");

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], blocked_root);
        app.update(AppEvent::OpenSettings);
        assert!(app.is_settings());

        let (resolved_event, status) =
            resolve_io_for_test(&mut app, AppEvent::SettingsCommit).await;
        assert!(matches!(
            resolved_event,
            AppEvent::SettingsSaveFailed { .. }
        ));
        assert!(
            status
                .as_deref()
                .is_some_and(|message| message.contains("Config write error"))
        );

        app.update(resolved_event);
        assert!(app.is_settings(), "a failed save must keep the modal open");
        assert!(
            app.settings
                .as_ref()
                .and_then(|settings| settings.error.as_deref())
                .is_some_and(|message| message.contains("Config write error"))
        );
    }

    #[test]
    fn settings_esc_saves() {
        // Esc in settings must map to SettingsCommit (auto-save on close).
        let app = test_app();
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::SettingsCommit
        ));
    }

    #[test]
    fn settings_enter_on_model_field_opens_picker() {
        // Enter on a model field opens the model picker (not SettingsCommit).
        let mut app = test_app();
        app.update(crate::app::AppEvent::OpenSettings);
        // Navigate to Developer model (7th Down from GateIterations).
        for _ in 0..6 {
            app.update(crate::app::AppEvent::SettingsDown);
        }
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            crate::app::SettingsField::DeveloperModel
        );
        let ev = key_press(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            crate::app::AppEvent::OpenModelPicker
        ));
    }

    #[test]
    fn settings_arrows_navigate() {
        // Up/Down in settings must map to SettingsUp/SettingsDown.
        let app = test_app();
        let up = key_press(KeyCode::Up, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                up,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
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
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::SettingsDown
        ));
    }

    #[test]
    fn settings_option_keys_cycle_options() {
        let app = test_app();

        let left = key_press(KeyCode::Left, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                left,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::SettingsPreviousOption
        ));

        let right = key_press(KeyCode::Right, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                right,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::SettingsNextOption
        ));

        let space = key_press(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                space,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::SettingsNextOption
        ));
    }

    #[test]
    fn settings_digit_input() {
        // Typing a digit in settings must map to SettingsInput.
        let app = test_app();
        let ev = key_press(KeyCode::Char('5'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
            ),
            AppEvent::SettingsInput('5')
        ));
    }

    #[test]
    fn settings_backspace_deletes() {
        // Backspace in settings must map to SettingsBackspace.
        let app = test_app();
        let ev = key_press(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState {
                    settings: true,
                    ..ModalState::default()
                },
                crate::app::Panel::Sidebar,
                false,
                &app
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

        // Create a per-task candidate; discovery must not require a monolith.
        let tmpdir = tempfile::tempdir().unwrap();
        let repo_root = tmpdir.path();
        let plans_dir = repo_root.join("docs/plans/0001-x");
        std::fs::create_dir_all(&plans_dir).unwrap();
        std::fs::write(plans_dir.join("SCOPE.md"), "Scope").unwrap();
        std::fs::write(plans_dir.join("ARCHITECTURE.md"), "Architecture").unwrap();
        std::fs::create_dir(plans_dir.join("tasks")).unwrap();
        std::fs::write(plans_dir.join("tasks/0101-x.md"), "invalid candidate").unwrap();

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], repo_root.to_path_buf());
        // Set opened_folders to include the repo_root for discovery.
        app.opened_folders = vec![repo_root.to_path_buf()];

        // Resolve OpenBrowser: should return immediately, then discover the plan
        // on the background channel.
        let (tx, mut rx) = background_events();
        let (resolved, status) = resolve_io(&mut app, AppEvent::OpenBrowser, &tx).await;
        assert!(
            matches!(resolved, AppEvent::OpenBrowser),
            "OpenBrowser must return immediately"
        );
        // Busy state is set by the OpenBrowser update arm, not via a status
        // message, so resolve_io returns no status here.
        assert_eq!(status, None);
        let discovered = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for PlansDiscoveredPerFolder")
            .expect("background channel closed");
        assert!(
            matches!(discovered, AppEvent::BrowserOpened { .. }),
            "historical TASKS-only directories are inert and must fall back to the browser"
        );
    }

    /// **Fallback-to-browser with an opened-but-empty folder:** `discover_plans_per_folder`
    /// always inserts one `HashMap` entry per opened folder — even an empty
    /// `Vec` when that folder has no `docs/plans/` — so `plans_map.is_empty()`
    /// alone cannot detect the "no plans anywhere" case. With one folder opened
    /// that has no `docs/plans/`, `[o]` (`fallback_to_browser = true`) must still
    /// fall back to the file browser instead of emitting an empty per-folder map.
    #[tokio::test]
    async fn open_browser_falls_back_when_opened_folder_has_no_plans() {
        use crate::app::{App, AppEvent};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let tmpdir = tempfile::tempdir().unwrap();
        let repo_root = tmpdir.path();
        // No docs/plans/ directory created: this folder has zero plans.

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], repo_root.to_path_buf());
        app.opened_folders = vec![repo_root.to_path_buf()];

        let (tx, mut rx) = background_events();
        let (resolved, _status) = resolve_io(&mut app, AppEvent::OpenBrowser, &tx).await;
        assert!(matches!(resolved, AppEvent::OpenBrowser));

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for background event")
            .expect("background channel closed");
        assert!(
            matches!(event, AppEvent::BrowserOpened { .. }),
            "an opened folder with no plans must fall back to the file browser, got {event:?}"
        );
    }

    #[tokio::test]
    async fn enter_key_on_plan_node_opens_detail_pane() {
        use crate::app::{App, AppEvent, TreeNode};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let plan_dir = std::path::PathBuf::from("docs/plans/0001-test");
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Manually add discovered plan (simulating PlansDiscovered)
        app.discovered_plans = vec![test_plan_entry(
            plan_dir.clone(),
            "0001-test".to_string(),
            vec![TestPlanTask {
                id: "t1".to_string(),
                title: "First task".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
        )];

        // Move cursor to the plan node (index 0 in the tree)
        app.tree_cursor = Some(0);

        // Simulate initial collapsed state (as PlansDiscovered would seed).
        let collapse_key = CollapseKey::Plan(
            app.plan_identity_for_node(TreeNode::Plan { plan_idx: 0 })
                .expect("plan identity"),
        );
        app.collapsed_plans = std::iter::once(collapse_key.clone()).collect();
        assert!(
            app.collapsed_plans.contains(&collapse_key),
            "plan should start collapsed"
        );

        // Verify we're focused on a plan node
        assert!(matches!(
            app.focused_node(),
            Some(TreeNode::Plan { plan_idx: 0 })
        ));

        // Resolve OpenFocusedNode (now passes through; handling + expand is in update).
        let (tx, _rx) = background_events();
        let (resolved, status) = resolve_io(&mut app, AppEvent::OpenFocusedNode, &tx).await;

        assert!(
            matches!(resolved, AppEvent::OpenFocusedNode),
            "OpenFocusedNode now passes through resolve_io (no transform), got {resolved:?}"
        );
        assert_eq!(status, None, "no status for open-focused");

        // Applying the event opens the plan tab AND expands the plan in sidebar.
        app.update(resolved);
        assert_eq!(app.tabs.open_tabs.len(), 1);
        assert!(matches!(
            &app.tabs.open_tabs[0],
            crate::app::TabContent::Plan { plan } if plan.slug == "0001-test"
        ));
        assert!(
            !app.collapsed_plans.contains(&collapse_key),
            "Enter on plan node must expand it in the sidebar"
        );
    }

    /// Enter on a plan's task preview opens that task's OWN tab (a `PlanTask`
    /// tab), distinct from the plan tab — so each task the user opens gets a tab.
    #[tokio::test]
    async fn enter_key_on_plan_task_node_opens_task_tab() {
        use crate::app::{App, AppEvent, TreeNode};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));
        app.discovered_plans = vec![test_plan_entry(
            std::path::PathBuf::from("docs/plans/0001-test"),
            "0001-test".to_string(),
            vec![TestPlanTask {
                id: "do-thing".to_string(),
                title: "Do the thing".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
        )];

        // Cursor on the plan-task preview (node 1: [Plan, PlanTask]).
        app.tree_cursor = Some(1);
        assert!(matches!(
            app.focused_node(),
            Some(TreeNode::PlanTask {
                plan_idx: 0,
                task_idx: 0
            })
        ));

        let (tx, _rx) = background_events();
        let (resolved, status) = resolve_io(&mut app, AppEvent::OpenFocusedNode, &tx).await;
        assert!(
            matches!(resolved, AppEvent::OpenFocusedNode),
            "OpenFocusedNode passes through, got {resolved:?}"
        );
        assert_eq!(status, None);

        app.update(resolved);
        assert_eq!(app.tabs.open_tabs.len(), 1);
        assert!(matches!(
            &app.tabs.open_tabs[0],
            crate::app::TabContent::PlanTask { task_id, .. } if task_id == "do-thing"
        ));
    }

    #[test]
    fn enter_key_in_sidebar_translates_to_open_focused_node() {
        let ev = key_press(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                false,
                &test_app()
            ),
            AppEvent::OpenFocusedNode
        ));
    }

    #[test]
    fn enter_key_in_main_pane_toggles_accordion() {
        let ev = key_press(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Main,
                false,
                &test_app()
            ),
            AppEvent::ToggleTreeNode
        ));
    }

    // ── Accordion section toggles (plan 0032) ───────────────────────────────────

    #[test]
    fn s_key_in_main_with_plan_tab_toggles_scope() {
        let ev = key_press(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Main,
                true,
                &test_app()
            ),
            AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Scope)
        ));
    }

    #[test]
    fn a_key_in_main_with_plan_tab_toggles_architecture() {
        let ev = key_press(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Main,
                true,
                &test_app()
            ),
            AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Architecture)
        ));
    }

    #[test]
    fn t_key_in_main_with_plan_tab_toggles_tasks() {
        let ev = key_press(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Main,
                true,
                &test_app()
            ),
            AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Tasks)
        ));
    }

    #[test]
    fn z_key_in_main_with_plan_tab_toggles_status() {
        let ev = key_press(KeyCode::Char('z'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Main,
                true,
                &test_app()
            ),
            AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Status)
        ));
    }

    #[test]
    fn s_key_in_main_without_plan_tab_is_tick() {
        let ev = key_press(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Main,
                false,
                &test_app()
            ),
            AppEvent::Tick
        ));
    }

    #[test]
    fn s_and_z_keys_in_main_with_plan_task_tab_toggle_task_sections() {
        let mut app = test_app();
        app.tabs.open_tab(crate::app::TabContent::PlanTask {
            plan: PlanIdentity::legacy("test-plan".to_string()),
            task_id: "preview-task".to_string(),
        });

        let ev_s = translate_terminal_event(
            key_press(KeyCode::Char('s'), KeyModifiers::NONE),
            ModalState::default(),
            crate::app::Panel::Main,
            false,
            &app,
        );
        assert!(matches!(
            ev_s,
            AppEvent::ToggleTaskAccordionSection(crate::app::AccordionSection::Scope)
        ));

        let ev_z = translate_terminal_event(
            key_press(KeyCode::Char('z'), KeyModifiers::NONE),
            ModalState::default(),
            crate::app::Panel::Main,
            false,
            &app,
        );
        assert!(matches!(
            ev_z,
            AppEvent::ToggleTaskAccordionSection(crate::app::AccordionSection::Execution)
        ));
    }

    #[test]
    fn s_key_in_sidebar_with_plan_tab_is_tick() {
        let ev = key_press(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                true,
                &test_app()
            ),
            AppEvent::Tick
        ));
    }

    #[test]
    fn a_key_in_sidebar_with_plan_tab_is_tick() {
        let ev = key_press(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Sidebar,
                true,
                &test_app()
            ),
            AppEvent::Tick
        ));
    }

    #[test]
    fn accordion_toggles_work_end_to_end() {
        use crate::app::{App, TabContent};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Open a plan tab
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy("test-plan".to_string()),
        });
        app.focused_panel = crate::app::Panel::Main;

        // Simulate pressing 's' with a plan tab active
        let ev = translate_terminal_event(
            key_press(KeyCode::Char('s'), KeyModifiers::NONE),
            ModalState::default(),
            crate::app::Panel::Main,
            true, // plan_tab_active
            &app,
        );

        // Should get a ToggleAccordionSection event
        assert!(matches!(
            ev,
            AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Scope)
        ));

        // Process the event through the app
        app.update(ev);

        // The accordion state should have the Scope section expanded
        let expanded = app
            .accordion_state
            .get(&PlanIdentity::legacy("test-plan"))
            .unwrap();
        assert!(expanded.contains(&crate::app::AccordionSection::Scope));

        // Toggle it again (should collapse)
        let ev2 = translate_terminal_event(
            key_press(KeyCode::Char('s'), KeyModifiers::NONE),
            ModalState::default(),
            crate::app::Panel::Main,
            true,
            &app,
        );
        app.update(ev2);

        // Should be collapsed now
        let expanded = app
            .accordion_state
            .get(&PlanIdentity::legacy("test-plan"))
            .unwrap();
        assert!(!expanded.contains(&crate::app::AccordionSection::Scope));
    }

    #[test]
    fn alt_right_key_translates_to_next_tab() {
        let ev = key_press(KeyCode::Right, KeyModifiers::ALT);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Main,
                false,
                &test_app()
            ),
            AppEvent::NextTab
        ));
    }

    #[test]
    fn alt_left_key_translates_to_prev_tab() {
        let ev = key_press(KeyCode::Left, KeyModifiers::ALT);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Main,
                false,
                &test_app()
            ),
            AppEvent::PrevTab
        ));
    }

    #[test]
    fn control_w_key_translates_to_close_tab() {
        let ev = key_press(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert!(matches!(
            translate_terminal_event(
                ev,
                ModalState::default(),
                crate::app::Panel::Main,
                false,
                &test_app()
            ),
            AppEvent::CloseTab
        ));
    }

    #[test]
    fn tab_navigation_works_end_to_end() {
        use crate::app::{App, TabContent};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("."));

        // Open two plan tabs
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy("plan-1".to_string()),
        });
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy("plan-2".to_string()),
        });

        // plan-2 should be active (it was the last one opened)
        assert_eq!(app.tabs.active_tab, Some(1));

        // Simulate Alt+Left to go to previous tab
        let ev = translate_terminal_event(
            key_press(KeyCode::Left, KeyModifiers::ALT),
            ModalState::default(),
            crate::app::Panel::Main,
            true,
            &app,
        );
        assert!(matches!(ev, AppEvent::PrevTab));
        app.update(ev);

        // Should now be on plan-1
        assert_eq!(app.tabs.active_tab, Some(0));

        // Simulate Alt+Right to go to next tab
        let ev = translate_terminal_event(
            key_press(KeyCode::Right, KeyModifiers::ALT),
            ModalState::default(),
            crate::app::Panel::Main,
            true,
            &app,
        );
        assert!(matches!(ev, AppEvent::NextTab));
        app.update(ev);

        // Should be back on plan-2
        assert_eq!(app.tabs.active_tab, Some(1));

        // Simulate Ctrl+W to close the active tab
        let ev = translate_terminal_event(
            key_press(KeyCode::Char('w'), KeyModifiers::CONTROL),
            ModalState::default(),
            crate::app::Panel::Main,
            true,
            &app,
        );
        assert!(matches!(ev, AppEvent::CloseTab));
        app.update(ev);

        // Should have one tab left
        assert_eq!(app.tabs.open_tabs.len(), 1);
        assert_eq!(app.tabs.active_tab, Some(0));
    }

    #[test]
    fn test_accordion_header_click_toggles_section() {
        use ratatui::layout::Rect;

        // Create an app and manually populate accordion_header_bounds with known regions
        let app = test_app();

        // Simulate a header at position (0, 5) with width 40 and height 1
        let header_rect = Rect {
            x: 0,
            y: 5,
            width: 40,
            height: 1,
        };
        let app = app;
        *app.accordion_header_bounds.borrow_mut() =
            vec![(crate::app::AccordionSection::Scope, header_rect)];

        // Create a mouse click inside the header bounds
        let click_inside = mouse_at(MouseEventKind::Down(MouseButton::Left), 10, 5);
        let ev = translate_terminal_event(
            click_inside,
            ModalState::default(),
            crate::app::Panel::Main,
            true,
            &app,
        );

        // Should dispatch ToggleAccordionSection
        assert!(matches!(
            ev,
            AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Scope)
        ));

        // Create a mouse click outside the header bounds
        let click_outside = mouse_at(MouseEventKind::Down(MouseButton::Left), 10, 10);
        let ev2 = translate_terminal_event(
            click_outside,
            ModalState::default(),
            crate::app::Panel::Main,
            true,
            &app,
        );

        // Should dispatch SelectionStart instead
        assert!(matches!(ev2, AppEvent::SelectionStart(10, 10)));
    }

    #[test]
    fn test_task_accordion_header_click_toggles_task_section() {
        use ratatui::layout::Rect;

        let mut app = test_app();
        app.tabs.open_tab(crate::app::TabContent::Task {
            plan: PlanIdentity::legacy("test-plan".to_string()),
            run: RunId(1),
            task_id: makina_core::api::TaskId::new("test-task"),
        });
        *app.accordion_header_bounds.borrow_mut() = vec![(
            crate::app::AccordionSection::Execution,
            Rect {
                x: 0,
                y: 7,
                width: 40,
                height: 1,
            },
        )];

        let click_inside = mouse_at(MouseEventKind::Down(MouseButton::Left), 10, 7);
        let ev = translate_terminal_event(
            click_inside,
            ModalState::default(),
            crate::app::Panel::Main,
            false,
            &app,
        );

        assert!(matches!(
            ev,
            AppEvent::ToggleTaskAccordionSection(crate::app::AccordionSection::Execution)
        ));
    }

    #[test]
    fn test_plan_task_accordion_header_click_toggles_task_section() {
        use ratatui::layout::Rect;

        let mut app = test_app();
        app.tabs.open_tab(crate::app::TabContent::PlanTask {
            plan: PlanIdentity::legacy("test-plan".to_string()),
            task_id: "preview-task".to_string(),
        });
        *app.accordion_header_bounds.borrow_mut() = vec![(
            crate::app::AccordionSection::Scope,
            Rect {
                x: 0,
                y: 7,
                width: 40,
                height: 1,
            },
        )];

        let click_inside = mouse_at(MouseEventKind::Down(MouseButton::Left), 10, 7);
        let ev = translate_terminal_event(
            click_inside,
            ModalState::default(),
            crate::app::Panel::Main,
            false,
            &app,
        );

        assert!(matches!(
            ev,
            AppEvent::ToggleTaskAccordionSection(crate::app::AccordionSection::Scope)
        ));
    }

    #[test]
    fn arrow_keys_in_main_pane_emit_focus_events() {
        // Right arrow in main pane should emit FocusNext (Tab-equivalent)
        let right_event = key_press(KeyCode::Right, KeyModifiers::NONE);
        let ev = translate_terminal_event(
            right_event,
            ModalState::default(),
            crate::app::Panel::Main,
            false,
            &test_app(),
        );
        assert!(matches!(ev, AppEvent::FocusNext));

        // Left arrow in main pane should emit FocusPrev (Shift+Tab-equivalent)
        let left_event = key_press(KeyCode::Left, KeyModifiers::NONE);
        let ev = translate_terminal_event(
            left_event,
            ModalState::default(),
            crate::app::Panel::Main,
            false,
            &test_app(),
        );
        assert!(matches!(ev, AppEvent::FocusPrev));

        // Right arrow in sidebar should still emit FocusRightOrExpand (preserve existing behavior)
        let right_event = key_press(KeyCode::Right, KeyModifiers::NONE);
        let ev = translate_terminal_event(
            right_event,
            ModalState::default(),
            crate::app::Panel::Sidebar,
            false,
            &test_app(),
        );
        assert!(matches!(ev, AppEvent::FocusRightOrExpand));

        // Left arrow in sidebar should still emit FocusLeftOrCollapse (preserve existing behavior)
        let left_event = key_press(KeyCode::Left, KeyModifiers::NONE);
        let ev = translate_terminal_event(
            left_event,
            ModalState::default(),
            crate::app::Panel::Sidebar,
            false,
            &test_app(),
        );
        assert!(matches!(ev, AppEvent::FocusLeftOrCollapse));
    }

    /// `commit_theme_selection` writes the chosen theme name to the config file,
    /// preserves all other `GlobalConfig` fields, and rejects unknown names.
    ///
    /// Steps exercised:
    /// 1. A known name (`"Ayu Mirage"`) writes `theme_name` to the config.
    /// 2. Pre-existing `providers` / `roles` fields are preserved in the output.
    /// 3. An unknown name returns `Some("Unknown theme")` without writing any file.
    #[tokio::test]
    async fn commit_theme_selection_writes_to_config() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        // ── Setup: temp repo with an existing config ──────────────────────────
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let repo_root = tmpdir.path();

        let existing_config = r#"
[[providers]]
name = "claude"
command = "claude-acp"

[roles]
developer = { provider = "claude" }
reviewer = { provider = "claude" }

[caps]
gate_iterations = 5
reviewer_iterations = 2
wall_clock_secs = 600
"#;
        let config_path = repo_root.join(".makina/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).expect("mkdir");
        std::fs::write(&config_path, existing_config).expect("write existing");

        let api = Arc::new(PlaceholderApi::empty());
        let app = App::new(api, vec![], repo_root.to_path_buf());

        // ── 1. Writing a valid theme name succeeds ────────────────────────────
        let status = commit_theme_selection(&app, "Ayu Mirage").await;
        assert_eq!(
            status,
            Some("Theme saved".to_string()),
            "valid theme must return 'Theme saved'"
        );

        // ── 2. The file now contains the new theme_name ───────────────────────
        let raw = std::fs::read_to_string(&config_path).expect("read config");
        let cfg: makina_core::config::GlobalConfig = toml::from_str(&raw).expect("parse config");
        assert_eq!(cfg.theme_name, "Ayu Mirage", "theme_name must be written");

        // ── 3. Other fields are preserved ─────────────────────────────────────
        assert!(
            !cfg.providers.is_empty(),
            "providers must be preserved after theme commit"
        );
        assert_eq!(
            cfg.providers[0].name, "claude",
            "provider name must be unchanged"
        );
        assert!(
            cfg.roles.developer.is_some(),
            "developer role must be preserved"
        );
        assert_eq!(
            cfg.caps.gate_iterations, 5,
            "gate_iterations must be unchanged"
        );

        // ── 4. An unknown theme name is rejected ──────────────────────────────
        let unknown_status = commit_theme_selection(&app, "NonExistentTheme").await;
        assert_eq!(
            unknown_status,
            Some("Unknown theme".to_string()),
            "unknown theme name must return error string"
        );

        // The file content must be unchanged after the rejected write attempt.
        let raw_after = std::fs::read_to_string(&config_path).expect("read config after rejection");
        let cfg_after: makina_core::config::GlobalConfig =
            toml::from_str(&raw_after).expect("parse config after rejection");
        assert_eq!(
            cfg_after.theme_name, "Ayu Mirage",
            "theme_name must be unchanged after unknown-name rejection"
        );
    }

    #[test]
    fn test_accordion_key_outside_plan_tab_emits_status() {
        use crate::app::Panel;

        let app = test_app();

        // Test 'a' key outside a plan tab (plan_tab_active = false) in Main panel
        let ev_a = translate_terminal_event(
            key_press(KeyCode::Char('a'), KeyModifiers::NONE),
            ModalState::default(),
            Panel::Main,
            false, // plan_tab_active = false
            &app,
        );
        assert!(
            matches!(ev_a, AppEvent::StatusMessage(ref msg) if msg == "Accordion toggle not available here — open a plan tab."),
            "pressing 'a' outside a plan tab should emit StatusMessage with expected text"
        );

        // Test 't' key outside a plan tab in Main panel
        let ev_t = translate_terminal_event(
            key_press(KeyCode::Char('t'), KeyModifiers::NONE),
            ModalState::default(),
            Panel::Main,
            false, // plan_tab_active = false
            &app,
        );
        assert!(
            matches!(ev_t, AppEvent::StatusMessage(ref msg) if msg == "Accordion toggle not available here — open a plan tab."),
            "pressing 't' outside a plan tab should emit StatusMessage with expected text"
        );

        // Test 'z' key outside a plan tab in Main panel
        let ev_z = translate_terminal_event(
            key_press(KeyCode::Char('z'), KeyModifiers::NONE),
            ModalState::default(),
            Panel::Main,
            false, // plan_tab_active = false
            &app,
        );
        assert!(
            matches!(ev_z, AppEvent::StatusMessage(ref msg) if msg == "Accordion toggle not available here — open a plan or task tab."),
            "pressing 'z' outside a plan tab should emit StatusMessage with expected text"
        );

        // Verify that when plan_tab_active is true in Main panel, the keys toggle sections
        let ev_a_with_plan = translate_terminal_event(
            key_press(KeyCode::Char('a'), KeyModifiers::NONE),
            ModalState::default(),
            Panel::Main,
            true, // plan_tab_active = true
            &app,
        );
        assert!(
            matches!(
                ev_a_with_plan,
                AppEvent::ToggleAccordionSection(crate::app::AccordionSection::Architecture)
            ),
            "pressing 'a' with an active plan tab in Main panel should toggle Architecture section"
        );

        // Verify that when plan_tab_active is true in Sidebar, the key is a Tick (no-op)
        let ev_a_sidebar = translate_terminal_event(
            key_press(KeyCode::Char('a'), KeyModifiers::NONE),
            ModalState::default(),
            Panel::Sidebar,
            true, // plan_tab_active = true
            &app,
        );
        assert!(
            matches!(ev_a_sidebar, AppEvent::Tick),
            "pressing 'a' with an active plan tab in Sidebar should be a Tick"
        );
    }
}
