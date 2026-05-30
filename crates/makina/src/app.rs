//! Application state and pure update logic.
//!
//! [`App`] is the single source of truth for all TUI state.  It holds no IO;
//! the IO loop in [`crate::event`] drives it by calling [`App::update`].
//!
//! Keeping `update` a synchronous, pure function means every state transition
//! is unit-testable without a real terminal or async runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use makina_core::api::{AgentRole, Api, Event, RunView, TaskId};

use crate::browser::{DirEntry, FileBrowser};

// ── Exchange log ─────────────────────────────────────────────────────────────

/// Maximum number of exchange entries retained per task.
///
/// When more turns arrive the oldest entries are dropped so the buffer stays
/// bounded and cannot cause unbounded memory growth.
pub const EXCHANGE_LOG_CAP: usize = 100;

/// Maximum number of error-pane messages retained on [`App`].
///
/// When more messages arrive the oldest are evicted so the buffer stays
/// bounded and cannot cause unbounded memory growth.
pub const ERROR_MESSAGES_CAP: usize = 50;

/// Severity of an error-pane message.
///
/// Kept self-contained (no `tracing`/`chrono` dependency) so the `makina`
/// crate's dependency set stays minimal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorLevel {
    Error,
    Warn,
    Info,
}

/// A single message shown in the error pane.
#[derive(Debug, Clone)]
pub struct ErrorMessage {
    /// When the message was produced.
    pub timestamp: std::time::SystemTime,
    /// Severity of the message.
    pub level: ErrorLevel,
    /// Human-readable text.
    pub text: String,
}

/// A single turn in a live agent exchange: either a prompt from the
/// orchestrator or a (possibly still-streaming) response from the agent.
#[derive(Debug, Clone)]
pub struct ExchangeEntry {
    /// The agent role that produced this turn.
    pub role: AgentRole,
    /// `true` → this is a prompt sent TO the agent; `false` → response.
    pub is_prompt: bool,
    /// Accumulated text.  For prompts this is the full text of
    /// [`ExchangeEvent::PromptSent`].  For responses each
    /// [`ExchangeEvent::ResponseChunk`] is appended here as it arrives.
    pub text: String,
    /// Whether [`ExchangeEvent::TurnComplete`] has been received for the
    /// current response turn.  Always `true` for prompt entries.
    pub complete: bool,
}

/// Per-task bounded ring of exchange entries.
///
/// The buffer caps at [`EXCHANGE_LOG_CAP`] entries by dropping the oldest
/// when the cap is exceeded.  This prevents unbounded memory growth even when
/// a task generates many prompt/response turns.
#[derive(Debug, Default, Clone)]
pub struct ExchangeLog {
    pub entries: Vec<ExchangeEntry>,
}

impl ExchangeLog {
    /// Push a new entry, evicting the oldest when over cap.
    fn push(&mut self, entry: ExchangeEntry) {
        self.entries.push(entry);
        if self.entries.len() > EXCHANGE_LOG_CAP {
            // Drop the oldest entry to maintain the bound.
            self.entries.remove(0);
        }
    }

    /// Start a new prompt entry for the given role.
    pub fn add_prompt(&mut self, role: AgentRole, text: String) {
        self.push(ExchangeEntry {
            role,
            is_prompt: true,
            text,
            complete: true,
        });
    }

    /// Start a new (in-progress) response entry for the given role, or
    /// append to the last incomplete response.
    ///
    /// Design decision: a `ResponseChunk` arriving without a preceding
    /// `PromptSent` in this log still needs to go somewhere — we create an
    /// implicit incomplete response entry rather than silently dropping data.
    pub fn append_chunk(&mut self, role: AgentRole, chunk: String) {
        // Look for the last incomplete response entry with the same role so we
        // can accumulate streaming chunks.
        if let Some(last) = self.entries.last_mut()
            && !last.is_prompt
            && !last.complete
            && last.role == role
        {
            last.text.push_str(&chunk);
            return;
        }
        // No open response entry for this role — start a new one.
        self.push(ExchangeEntry {
            role,
            is_prompt: false,
            text: chunk,
            complete: false,
        });
    }

    /// Mark the last incomplete response entry as complete.
    pub fn complete_turn(&mut self) {
        if let Some(last) = self.entries.last_mut()
            && !last.is_prompt
            && !last.complete
        {
            last.complete = true;
        }
    }
}

// ── View mode ───────────────────────────────────────────────────────────────────

/// Which top-level view the TUI is currently showing.
///
/// The file browser is modal: while [`Mode::FileBrowser`] is active it overlays
/// the normal sidebar/main layout and captures navigation keys.  Closing it
/// (Esc, or after selecting a file) returns to [`Mode::Normal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The normal runs-sidebar + detail layout (task 27).
    Normal,
    /// The modal file browser for picking a task-list file to open.
    FileBrowser,
}

// ── Panel focus ───────────────────────────────────────────────────────────────

/// Which panel the keyboard focus is currently on.
///
/// The TUI has two top-level areas:
///
/// * [`Panel::Sidebar`] — the left column listing open Runs.
/// * [`Panel::Main`] — the right content area showing the focused Run.
///
/// Task 27 (runs-sidebar) and tasks 29–31 will extend the rendering of these
/// panels; the skeleton simply tracks which one owns focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Sidebar,
    Main,
}

// ── App input event ───────────────────────────────────────────────────────────

/// An event consumed by [`App::update`].
///
/// The IO loop in [`crate::event`] translates raw terminal input and api events
/// into this enum so that [`App::update`] is free of IO concerns.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// User pressed a quit key (`q`, `Ctrl-C`, or `Esc`).
    Quit,
    /// Terminal window was resized to the given dimensions (width, height).
    #[allow(dead_code)] // dimensions stored for future use by tasks 27-31
    Resize(u16, u16),
    /// Tab key — cycle focus between [`Panel::Sidebar`] and [`Panel::Main`].
    FocusNext,
    /// Move the sidebar selection one row up (`↑` / `k`).
    SelectUp,
    /// Move the sidebar selection one row down (`↓` / `j`).
    SelectDown,
    /// Scroll the focused exchange pane one line up (mouse wheel up).
    ScrollUp,
    /// Scroll the focused exchange pane one line down (mouse wheel down).
    ScrollDown,
    /// An event arrived from `api.subscribe()`.
    ApiEvent(Event),
    /// Periodic tick — triggers a redraw without other state changes.
    Tick,
    /// Toggle the error pane open/closed (`e` / `E`).
    ToggleErrorPane,

    // ── Task-status view (task 29) ────────────────────────────────────────────
    /// A full [`RunView`] (with its task list) was fetched from the api and
    /// should be merged into [`App::runs`], replacing any placeholder entry.
    ///
    /// The async event layer calls `api.run(id).await` when a `RunOpened` event
    /// arrives (or when the selection changes to a Run with no tasks yet) and
    /// feeds the result back as this event so that [`App::update`] stays pure.
    RunLoaded(RunView),

    // ── File browser (task 28) ────────────────────────────────────────────────
    //
    // Opening, navigating, and reading directories is IO; those reads live in
    // [`crate::event`].  The IO layer reads a directory off-thread and feeds the
    // result back as [`AppEvent::BrowserOpened`], keeping `update` pure.
    /// User requested to open the file browser (e.g. pressed `o`).
    ///
    /// Translated to a key intent here; the IO layer reacts by reading the
    /// starting directory and emitting [`AppEvent::BrowserOpened`].  `update`
    /// itself does nothing for this variant (no state to mutate without a
    /// listing) — it is handled entirely in the event loop.
    OpenBrowser,
    /// A directory was read by the IO layer: switch to / refresh the browser
    /// with this listing.  Carries the directory and its entries.
    BrowserOpened {
        /// The directory that was listed.
        dir: PathBuf,
        /// The entries under `dir`, in display order.
        entries: Vec<DirEntry>,
    },
    /// Move the browser selection one row up.
    BrowserUp,
    /// Move the browser selection one row down.
    BrowserDown,
    /// Activate the highlighted browser entry (Enter).
    ///
    /// The IO layer interprets the current selection: entering a directory
    /// triggers a fresh read (→ [`AppEvent::BrowserOpened`]); choosing a file
    /// triggers `api.execute(OpenRun{..})` and then [`AppEvent::CloseBrowser`].
    /// `update` does not mutate state for this variant.
    BrowserActivate,
    /// Go up to the parent directory (Backspace).
    ///
    /// Like [`AppEvent::BrowserActivate`], the actual read happens in the IO
    /// layer, which then emits [`AppEvent::BrowserOpened`] for the parent.
    BrowserParent,
    /// Close the file browser and return to the normal view (Esc, or after a
    /// file was opened).
    CloseBrowser,

    // ── Run control (task 31) ─────────────────────────────────────────────────
    //
    // Start/Pause/Cancel are INTENT signals (like the browser intents): the IO
    // layer in [`crate::event`] performs the async `api.execute(...)` for
    // `app.selected_run()`'s `RunId` and feeds the result back as a
    // [`AppEvent::StatusMessage`].  `update` itself does nothing for these
    // variants (it cannot issue the async command), keeping `update` pure.
    /// User pressed `s` — start (or resume) the selected Run.
    StartRun,
    /// User pressed `p` — pause the selected Run.
    PauseRun,
    /// User pressed `c` — cancel the selected Run.
    CancelRun,

    /// A transient status-bar message to display (command outcome or error).
    ///
    /// Set by the IO layer after an `api.execute(...)` resolves so the user sees
    /// feedback (success or failure) instead of a silently-dropped result
    /// (resolves the task-28 outcome-surfacing note).
    StatusMessage(String),

    /// A log record arrived on the tracing→TUI channel and should be appended
    /// to the error pane.
    ///
    /// Emitted by the event loop's `log_rx` drain arm (task
    /// `tui-error-pane-channel-wire`) after converting a
    /// [`makina_core::log_record::LogRecord`] into an [`ErrorMessage`].
    /// `update` pushes it via [`App::push_error`] (respecting
    /// [`ERROR_MESSAGES_CAP`]).
    ErrorMessageArrived { msg: ErrorMessage },
}

// ── App state ─────────────────────────────────────────────────────────────────

/// All mutable TUI state.
///
/// # Arc<dyn Api>
///
/// The App holds a shared reference to the api surface so that the IO loop can
/// call `api.subscribe()` once at startup, and any future action handler can
/// call `api.execute(…)` without needing a second handle.
///
/// # Runs
///
/// The initial `Vec<RunView>` is fetched from `api.runs()` at startup and
/// kept up-to-date via [`Event`]s from the subscription stream.  Only the
/// fields the scaffold needs are stored here; tasks 27–31 will extend this.
pub struct App {
    /// Whether the event loop should exit on the next iteration.
    pub should_quit: bool,

    /// The shared api surface.  Held here so action handlers can call
    /// `api.execute(…)` and callers in tests can pass a placeholder.
    pub api: Arc<dyn Api>,

    /// The panel that currently owns keyboard focus.
    pub focused_panel: Panel,

    /// Which top-level view is showing.  [`Mode::FileBrowser`] overlays a modal
    /// file picker; [`Mode::Normal`] shows the runs sidebar + detail panel.
    pub mode: Mode,

    /// File-browser view state.  `Some` only while [`App::mode`] is
    /// [`Mode::FileBrowser`]; the IO layer populates it via
    /// [`AppEvent::BrowserOpened`].
    pub browser: Option<FileBrowser>,

    /// The list of open Runs, seeded from `api.runs()` at startup and
    /// incrementally updated from api events.
    pub runs: Vec<RunView>,

    /// Index into `runs` identifying the currently selected/focused Run.
    /// `None` when `runs` is empty.
    pub selected_run: Option<usize>,

    /// Index into the selected Run's task list identifying the focused task.
    ///
    /// Navigating Up/Down when [`Panel::Main`] is focused moves this.  The
    /// exchange pane renders the focused task's exchange log.  `None` when
    /// the selected run has no tasks.
    pub selected_task: Option<usize>,

    /// Per-task exchange logs, keyed by [`TaskId`].
    ///
    /// The orchestrator emits [`Event::AgentExchange`] for ALL in-flight
    /// tasks; the TUI stores logs for every task it hears about and filters to
    /// the currently focused task when rendering the exchange pane.
    pub exchange_logs: HashMap<TaskId, ExchangeLog>,

    /// Manual scroll offset for the exchange pane, in lines from the top.
    ///
    /// `App` does not know the rendered line count or pane height, so the
    /// clamp upper bound (`scroll_max`) is passed in by the render/event layer
    /// (see [`App::scroll_down`] / [`App::effective_offset`]).
    pub exchange_scroll: u16,

    /// Whether the exchange pane auto-follows the bottom of the log.
    ///
    /// Defaults to `true` (newest exchange always visible).  Scrolling up
    /// disengages auto-follow; scrolling back down to the bottom re-engages it.
    pub exchange_auto_follow: bool,

    /// Last api event received — stored for test assertions and status-bar
    /// display.  Will be used by tasks 27–31 for richer updates.
    pub last_event: Option<Event>,

    /// A transient status-bar message surfacing the most recent command outcome
    /// or error (task 31).  Set by [`AppEvent::StatusMessage`] (emitted by the
    /// IO layer after an `api.execute(...)` resolves) and rendered in the status
    /// bar.  `None` until the first command is issued.
    pub status_message: Option<String>,

    /// Whether the error pane is currently visible.
    pub error_pane_open: bool,

    /// Bounded ring of error-pane messages.  Capped at [`ERROR_MESSAGES_CAP`]
    /// by evicting the oldest; see [`App::push_error`].
    pub error_messages: Vec<ErrorMessage>,
}

impl App {
    /// Build a new [`App`] with the given api and initial run list.
    ///
    /// Call `api.runs().await` before constructing to obtain `initial_runs`.
    pub fn new(api: Arc<dyn Api>, initial_runs: Vec<RunView>) -> Self {
        let selected_run = if initial_runs.is_empty() {
            None
        } else {
            Some(0)
        };
        // Auto-select the first task of the first run (if any).
        let selected_task = initial_runs
            .first()
            .and_then(|r| if r.tasks.is_empty() { None } else { Some(0) });
        Self {
            should_quit: false,
            api,
            focused_panel: Panel::Sidebar,
            mode: Mode::Normal,
            browser: None,
            runs: initial_runs,
            selected_run,
            selected_task,
            exchange_logs: HashMap::new(),
            exchange_scroll: 0,
            exchange_auto_follow: true,
            last_event: None,
            status_message: None,
            error_pane_open: false,
            error_messages: Vec::new(),
        }
    }

    /// Push a new error-pane message, evicting the oldest when over cap.
    ///
    /// Mirrors [`ExchangeLog::push`]: maintains the [`ERROR_MESSAGES_CAP`]
    /// bound so the buffer cannot grow without limit.
    pub fn push_error(&mut self, msg: ErrorMessage) {
        self.error_messages.push(msg);
        if self.error_messages.len() > ERROR_MESSAGES_CAP {
            // Drop the oldest message to maintain the bound.
            self.error_messages.remove(0);
        }
    }

    /// Whether the modal file browser is currently active.
    pub fn is_browsing(&self) -> bool {
        self.mode == Mode::FileBrowser
    }

    /// Return the currently selected [`RunView`], if any.
    ///
    /// The sidebar highlights this Run; the main panel displays its details.
    /// Task 29 (task-status-view) and task 31 (run-control) read this to know
    /// which Run to act on.
    ///
    /// This accessor is the primary seam between the sidebar and the detail
    /// panel.  Task 29 (task-status-view) and task 31 (run-control) call this
    /// to obtain the currently focused Run.
    pub fn selected_run(&self) -> Option<&RunView> {
        self.selected_run.and_then(|i| self.runs.get(i))
    }

    /// Return the [`TaskId`] of the currently focused task within the selected
    /// Run, if any.
    ///
    /// Used by the exchange pane to determine which task's log to display.
    pub fn selected_task_id(&self) -> Option<&TaskId> {
        self.selected_run()
            .and_then(|run| self.selected_task.and_then(|i| run.tasks.get(i)))
            .map(|tv| &tv.id)
    }

    /// Scroll the exchange pane up by one line.
    ///
    /// Disengages auto-follow (the user is reviewing history) and decrements the
    /// manual offset, clamped at `0`.  `App` does not know the rendered line
    /// count, so no upper bound is needed here.
    pub fn scroll_up(&mut self) {
        self.exchange_auto_follow = false;
        self.exchange_scroll = self.exchange_scroll.saturating_sub(1);
    }

    /// Scroll the exchange pane down by one line, clamped at `scroll_max`.
    ///
    /// `scroll_max` is computed by the caller exactly like
    /// [`render_exchange_pane`](crate::ui) — `total_lines.saturating_sub(pane_height)`.
    /// Reaching the bottom re-engages auto-follow so new exchanges keep the pane
    /// pinned to the latest line.
    pub fn scroll_down(&mut self, scroll_max: u16) {
        self.exchange_scroll = (self.exchange_scroll + 1).min(scroll_max);
        if self.exchange_scroll == scroll_max {
            self.exchange_auto_follow = true;
        }
    }

    /// The effective scroll offset to render with, given the current
    /// `scroll_max` (computed by the caller as in `render_exchange_pane`).
    ///
    /// When auto-following, returns `scroll_max` (pinned to the bottom);
    /// otherwise returns the manual offset, clamped to `scroll_max`.
    pub fn effective_offset(&self, scroll_max: u16) -> u16 {
        if self.exchange_auto_follow {
            scroll_max
        } else {
            self.exchange_scroll.min(scroll_max)
        }
    }

    /// Apply one [`AppEvent`] to the App state.
    ///
    /// This function is **pure** (no async, no IO) so it can be called from unit
    /// tests without a real terminal or runtime.
    ///
    /// Returns `true` when the caller should trigger an immediate redraw (for
    /// any event that changes visible state).
    pub fn update(&mut self, event: AppEvent) -> bool {
        match event {
            AppEvent::Quit => {
                self.should_quit = true;
                true
            }
            AppEvent::Resize(_, _) => {
                // Terminal resize doesn't update App fields; the next render
                // pass will pick up the new dimensions from the frame.
                true
            }
            AppEvent::FocusNext => {
                self.focused_panel = match self.focused_panel {
                    Panel::Sidebar => Panel::Main,
                    Panel::Main => Panel::Sidebar,
                };
                true
            }
            AppEvent::ToggleErrorPane => {
                self.error_pane_open = !self.error_pane_open;
                true
            }
            AppEvent::SelectUp => {
                match self.focused_panel {
                    Panel::Sidebar => {
                        // Sidebar focus: navigate runs.
                        if let Some(current) = self.selected_run {
                            let new_idx = current.saturating_sub(1);
                            if new_idx != current {
                                self.selected_run = Some(new_idx);
                                // Re-clamp selected_task to the new run's task list.
                                self.clamp_selected_task();
                            }
                        }
                    }
                    Panel::Main => {
                        // Main focus: navigate tasks within the selected run.
                        if let Some(current) = self.selected_task {
                            self.selected_task = Some(current.saturating_sub(1));
                        }
                    }
                }
                true
            }
            AppEvent::SelectDown => {
                match self.focused_panel {
                    Panel::Sidebar => {
                        // Sidebar focus: navigate runs.
                        if let Some(current) = self.selected_run {
                            let last = self.runs.len().saturating_sub(1);
                            let new_idx = (current + 1).min(last);
                            if new_idx != current {
                                self.selected_run = Some(new_idx);
                                // Re-clamp selected_task to the new run's task list.
                                self.clamp_selected_task();
                            }
                        }
                    }
                    Panel::Main => {
                        // Main focus: navigate tasks within the selected run.
                        if let Some(current) = self.selected_task {
                            let last = self
                                .selected_run()
                                .map(|r| r.tasks.len().saturating_sub(1))
                                .unwrap_or(0);
                            self.selected_task = Some((current + 1).min(last));
                        }
                    }
                }
                true
            }
            // ── Exchange-pane scroll (task `tui-mouse-scroll`) ────────────────
            // Mirror BrowserUp/BrowserDown: a wheel event nudges the manual
            // scroll offset via the `tui-scroll-state` helpers.  `update` has no
            // pane geometry, so it uses `u16::MAX` as `scroll_max`; the render
            // pass re-clamps the offset to the real `total_lines - pane_height`
            // via `effective_offset`.
            AppEvent::ScrollUp => {
                self.scroll_up();
                true
            }
            AppEvent::ScrollDown => {
                let scroll_max = u16::MAX;
                self.scroll_down(scroll_max);
                true
            }
            AppEvent::ApiEvent(ev) => {
                self.apply_api_event(ev);
                true
            }
            AppEvent::Tick => {
                // Tick drives the redraw loop; no state changes needed here.
                true
            }

            // ── Task-status view (task 29) ────────────────────────────────────
            AppEvent::RunLoaded(full_run) => {
                // Replace or insert the RunView with the fully-populated one from
                // the api.  If an entry with the same id already exists (i.e. the
                // placeholder inserted by RunOpened), replace it in-place so the
                // sidebar index / selection stays stable.
                let is_selected = self
                    .selected_run
                    .and_then(|i| self.runs.get(i))
                    .map(|r| r.id == full_run.id)
                    .unwrap_or(false);
                if let Some(existing) = self.runs.iter_mut().find(|r| r.id == full_run.id) {
                    *existing = full_run;
                } else {
                    self.runs.push(full_run);
                    if self.selected_run.is_none() {
                        self.selected_run = Some(0);
                    }
                }
                // When the currently selected run's tasks arrive (or change),
                // clamp selected_task so it points to a valid slot.
                if is_selected || self.selected_run == Some(self.runs.len().saturating_sub(1)) {
                    self.clamp_selected_task();
                }
                true
            }

            // ── File browser ──────────────────────────────────────────────────
            // OpenBrowser / BrowserActivate / BrowserParent are intent signals
            // handled by the IO layer (it does the directory read / OpenRun call
            // and feeds back BrowserOpened / CloseBrowser).  `update` stays pure.
            AppEvent::OpenBrowser => {
                // No state change here; the IO layer reads the start dir and
                // emits BrowserOpened.  Returning true is harmless (redraw).
                true
            }
            AppEvent::BrowserOpened { dir, entries } => {
                // A directory listing arrived: enter (or refresh) the browser.
                self.mode = Mode::FileBrowser;
                self.browser = Some(FileBrowser::new(dir, entries));
                true
            }
            AppEvent::BrowserUp => {
                if let Some(browser) = self.browser.as_mut() {
                    browser.select_up();
                }
                true
            }
            AppEvent::BrowserDown => {
                if let Some(browser) = self.browser.as_mut() {
                    browser.select_down();
                }
                true
            }
            AppEvent::BrowserActivate => {
                // Handled by the IO layer (enter dir or open file). No-op here.
                true
            }
            AppEvent::BrowserParent => {
                // Handled by the IO layer (read parent dir). No-op here.
                true
            }
            AppEvent::CloseBrowser => {
                self.mode = Mode::Normal;
                self.browser = None;
                true
            }

            // ── Run control (task 31) ─────────────────────────────────────────
            // Start/Pause/Cancel are IO-layer intents: the event loop issues the
            // async `api.execute(...)` for the selected run and feeds back a
            // StatusMessage.  `update` itself does not mutate state here (no
            // async), so these are no-ops that simply request a redraw.
            AppEvent::StartRun | AppEvent::PauseRun | AppEvent::CancelRun => true,

            AppEvent::StatusMessage(msg) => {
                self.status_message = Some(msg);
                true
            }

            AppEvent::ErrorMessageArrived { msg } => {
                self.push_error(msg);
                true
            }
        }
    }

    /// Clamp `selected_task` to the currently selected run's task list.
    ///
    /// Called after changing `selected_run` or after a run's task list is
    /// (re)loaded so the task index always points to a valid slot.
    fn clamp_selected_task(&mut self) {
        let task_count = self.selected_run().map(|r| r.tasks.len()).unwrap_or(0);
        self.selected_task = if task_count == 0 {
            None
        } else {
            Some(self.selected_task.unwrap_or(0).min(task_count - 1))
        };
    }

    /// Apply a core api [`Event`] to the App state.
    ///
    /// Tasks 27–31 will handle the full set of variants here; the scaffold
    /// handles the variants needed to keep `runs` and `selected_run` coherent.
    fn apply_api_event(&mut self, event: Event) {
        use makina_core::api::{RunStatus, TaskState};

        match &event {
            Event::RunOpened {
                run,
                task_list_path,
            } => {
                // If we don't already have a RunView for this id (the initial
                // `api.runs()` query might have raced with the event), insert a
                // placeholder.  Tasks 27 and 29 will flesh out proper handling.
                if !self.runs.iter().any(|r| r.id == *run) {
                    self.runs.push(RunView {
                        id: *run,
                        run_uid: String::new(),
                        task_list_path: task_list_path.clone(),
                        status: RunStatus::Pending,
                        project: String::new(),
                        tasks: vec![],
                    });
                    if self.selected_run.is_none() {
                        self.selected_run = Some(0);
                    }
                }
            }
            Event::RunStatusChanged { run, status } => {
                if let Some(rv) = self.runs.iter_mut().find(|r| r.id == *run) {
                    rv.status = status.clone();
                }
            }
            Event::TaskStateChanged { run, task, state } => {
                if let Some(rv) = self.runs.iter_mut().find(|r| r.id == *run)
                    && let Some(tv) = rv.tasks.iter_mut().find(|t| t.id == *task)
                {
                    tv.state = state.clone();
                    // Recompute aggregate run status from task states (simple heuristic).
                    let any_failed = rv.tasks.iter().any(|t| t.state == TaskState::Failed);
                    let all_done = rv.tasks.iter().all(|t| t.state == TaskState::Done);
                    let any_active = rv
                        .tasks
                        .iter()
                        .any(|t| matches!(t.state, TaskState::InProgress | TaskState::InReview));
                    rv.status = if any_failed && !any_active {
                        RunStatus::Failed
                    } else if all_done {
                        RunStatus::Completed
                    } else if any_active {
                        RunStatus::Running
                    } else {
                        rv.status.clone()
                    };
                }
            }
            Event::TaskIterationsUpdated {
                run,
                task,
                gate_iterations,
                review_iterations,
            } => {
                if let Some(rv) = self.runs.iter_mut().find(|r| r.id == *run)
                    && let Some(tv) = rv.tasks.iter_mut().find(|t| t.id == *task)
                {
                    tv.gate_iterations = *gate_iterations;
                    tv.review_iterations = *review_iterations;
                }
            }
            // AgentExchange events accumulate into the per-task exchange log
            // (task 30: prompt-answer-stream).  The TUI stores ALL tasks' logs
            // and filters to the focused task at render time.
            Event::AgentExchange {
                task,
                role,
                event: exchange_ev,
                ..
            } => {
                use makina_core::api::ExchangeEvent;
                let log = self.exchange_logs.entry(task.clone()).or_default();
                match exchange_ev {
                    ExchangeEvent::PromptSent { text } => {
                        log.add_prompt(role.clone(), text.clone());
                    }
                    ExchangeEvent::ResponseChunk { text } => {
                        log.append_chunk(role.clone(), text.clone());
                    }
                    ExchangeEvent::TurnComplete => {
                        log.complete_turn();
                    }
                }
            }
        }

        self.last_event = Some(event);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placeholder::PlaceholderApi;
    use std::path::PathBuf;

    fn make_app() -> App {
        let api = Arc::new(PlaceholderApi::new());
        App::new(api, vec![])
    }

    // ── Error pane ────────────────────────────────────────────────────────────

    /// `push_error` must keep the error-pane buffer bounded at
    /// [`ERROR_MESSAGES_CAP`] by evicting the OLDEST message (FIFO), not merely
    /// capping the length.  Stronger than `exchange_log_bounded_at_cap`.
    #[test]
    fn error_messages_bounded_at_cap() {
        use crate::app::ERROR_MESSAGES_CAP;

        let mut app = make_app();
        let total = ERROR_MESSAGES_CAP + 5;
        for i in 0..total {
            app.push_error(ErrorMessage {
                timestamp: std::time::SystemTime::now(),
                level: ErrorLevel::Error,
                text: format!("msg {i}"),
            });
        }

        // Exactly capped.
        assert_eq!(
            app.error_messages.len(),
            ERROR_MESSAGES_CAP,
            "error_messages must be capped at ERROR_MESSAGES_CAP={ERROR_MESSAGES_CAP} but has {}",
            app.error_messages.len()
        );
        // The 5 oldest were evicted: the first retained is the 6th pushed
        // (index 5, "msg 5").
        assert_eq!(
            app.error_messages.first().unwrap().text,
            "msg 5",
            "oldest messages must be evicted, not the newest"
        );
        // The last retained is the most recently pushed.
        assert_eq!(
            app.error_messages.last().unwrap().text,
            format!("msg {}", total - 1),
            "most recent message must be retained"
        );
    }

    // ── Quit logic ────────────────────────────────────────────────────────────

    #[test]
    fn quit_event_sets_should_quit() {
        let mut app = make_app();
        assert!(!app.should_quit);
        let redraws = app.update(AppEvent::Quit);
        assert!(app.should_quit, "update(Quit) must set should_quit");
        assert!(redraws, "Quit must return true (trigger redraw)");
    }

    #[test]
    fn quit_is_idempotent() {
        let mut app = make_app();
        app.update(AppEvent::Quit);
        app.update(AppEvent::Quit);
        assert!(app.should_quit);
    }

    // ── Focus navigation ──────────────────────────────────────────────────────

    #[test]
    fn focus_cycles_between_panels() {
        let mut app = make_app();
        assert_eq!(app.focused_panel, Panel::Sidebar);
        app.update(AppEvent::FocusNext);
        assert_eq!(app.focused_panel, Panel::Main);
        app.update(AppEvent::FocusNext);
        assert_eq!(app.focused_panel, Panel::Sidebar);
    }

    #[test]
    fn error_pane_toggle_flips_flag() {
        let mut app = make_app();
        assert!(!app.error_pane_open);
        assert!(app.update(AppEvent::ToggleErrorPane));
        assert!(app.error_pane_open);
        assert!(app.update(AppEvent::ToggleErrorPane));
        assert!(!app.error_pane_open);
    }

    // ── Tick does not quit ────────────────────────────────────────────────────

    #[test]
    fn tick_does_not_quit() {
        let mut app = make_app();
        app.update(AppEvent::Tick);
        assert!(!app.should_quit);
    }

    // ── Api event handling ────────────────────────────────────────────────────

    #[test]
    fn api_event_run_opened_adds_run() {
        use makina_core::api::RunId;
        let mut app = make_app();
        assert!(app.runs.is_empty());

        let ev = Event::RunOpened {
            run: RunId(1),
            task_list_path: PathBuf::from(".tasks/demo.json"),
        };
        app.update(AppEvent::ApiEvent(ev));

        assert_eq!(app.runs.len(), 1);
        assert_eq!(app.runs[0].id, RunId(1));
        assert_eq!(app.selected_run, Some(0));
    }

    #[test]
    fn api_event_run_opened_does_not_duplicate() {
        use makina_core::api::RunView;
        use makina_core::api::{RunId, RunStatus};
        let api = Arc::new(PlaceholderApi::new());
        let existing = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/demo.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
        };
        let mut app = App::new(api, vec![existing]);

        // Sending a RunOpened for the same id should not add a second entry.
        let ev = Event::RunOpened {
            run: RunId(1),
            task_list_path: PathBuf::from(".tasks/demo.json"),
        };
        app.update(AppEvent::ApiEvent(ev));
        assert_eq!(
            app.runs.len(),
            1,
            "duplicate RunOpened must not add a second entry"
        );
    }

    #[test]
    fn api_event_task_state_changed_updates_state() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("t1"),
                title: "Task 1".into(),
                state: TaskState::Ready,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
            }],
        };
        let mut app = App::new(api, vec![run]);

        let ev = Event::TaskStateChanged {
            run: RunId(1),
            task: TaskId::new("t1"),
            state: TaskState::InProgress,
        };
        app.update(AppEvent::ApiEvent(ev));

        assert_eq!(app.runs[0].tasks[0].state, TaskState::InProgress);
        assert!(
            matches!(app.last_event, Some(Event::TaskStateChanged { .. })),
            "last_event should be set after api event"
        );
    }

    #[test]
    fn api_event_task_all_done_marks_run_completed() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("t1"),
                title: "Task 1".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
            }],
        };
        let mut app = App::new(api, vec![run]);

        let ev = Event::TaskStateChanged {
            run: RunId(1),
            task: TaskId::new("t1"),
            state: TaskState::Done,
        };
        app.update(AppEvent::ApiEvent(ev));
        assert_eq!(app.runs[0].status, RunStatus::Completed);
    }

    // ── selected_run accessor ─────────────────────────────────────────────────

    #[test]
    fn selected_run_accessor_returns_correct_run() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(7),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/accessor.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![],
        };
        let app = App::new(api, vec![run]);
        let selected = app.selected_run();
        assert!(
            selected.is_some(),
            "accessor should return Some when a run exists"
        );
        assert_eq!(selected.unwrap().id, RunId(7));
    }

    #[test]
    fn selected_run_accessor_returns_none_when_empty() {
        let app = make_app();
        assert!(app.selected_run().is_none());
    }

    // ── SelectUp / SelectDown navigation ─────────────────────────────────────

    #[test]
    fn select_down_moves_selection_forward() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let runs = vec![
            RunView {
                id: RunId(1),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/a.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/b.json"),
                status: RunStatus::Running,
                project: String::new(),
                tasks: vec![],
            },
            RunView {
                id: RunId(3),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/c.json"),
                status: RunStatus::Completed,
                project: String::new(),
                tasks: vec![],
            },
        ];
        let mut app = App::new(api, runs);
        assert_eq!(app.selected_run, Some(0));

        app.update(AppEvent::SelectDown);
        assert_eq!(app.selected_run, Some(1));

        app.update(AppEvent::SelectDown);
        assert_eq!(app.selected_run, Some(2));
    }

    #[test]
    fn select_down_clamps_at_last() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let runs = vec![
            RunView {
                id: RunId(1),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/a.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/b.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
            },
        ];
        let mut app = App::new(api, runs);
        // Move to last entry.
        app.update(AppEvent::SelectDown);
        assert_eq!(app.selected_run, Some(1));
        // Attempting to move past the end must clamp.
        app.update(AppEvent::SelectDown);
        assert_eq!(app.selected_run, Some(1), "should clamp at last index");
    }

    #[test]
    fn select_up_moves_selection_backward() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let runs = vec![
            RunView {
                id: RunId(1),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/a.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/b.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
            },
        ];
        let mut app = App::new(api, runs);
        app.update(AppEvent::SelectDown);
        assert_eq!(app.selected_run, Some(1));
        app.update(AppEvent::SelectUp);
        assert_eq!(app.selected_run, Some(0));
    }

    #[test]
    fn select_up_clamps_at_zero() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let runs = vec![RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/a.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
        }];
        let mut app = App::new(api, runs);
        assert_eq!(app.selected_run, Some(0));
        app.update(AppEvent::SelectUp);
        assert_eq!(app.selected_run, Some(0), "should clamp at zero");
    }

    #[test]
    fn navigation_ignored_when_main_panel_focused() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let runs = vec![
            RunView {
                id: RunId(1),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/a.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/b.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
            },
        ];
        let mut app = App::new(api, runs);
        // Switch focus to main panel.
        app.update(AppEvent::FocusNext);
        assert_eq!(app.focused_panel, Panel::Main);
        // Navigation events must be no-ops when main panel is focused.
        app.update(AppEvent::SelectDown);
        assert_eq!(
            app.selected_run,
            Some(0),
            "SelectDown must be ignored when main is focused"
        );
        app.update(AppEvent::SelectUp);
        assert_eq!(
            app.selected_run,
            Some(0),
            "SelectUp must be ignored when main is focused"
        );
    }

    #[test]
    fn select_down_no_op_when_runs_empty() {
        let mut app = make_app();
        assert_eq!(app.selected_run, None);
        app.update(AppEvent::SelectDown);
        assert_eq!(
            app.selected_run, None,
            "SelectDown on empty list must be a no-op"
        );
    }

    // ── RunOpened keeps selection valid ───────────────────────────────────────

    #[test]
    fn run_opened_selects_first_when_list_was_empty() {
        use makina_core::api::RunId;
        let mut app = make_app();
        assert_eq!(app.selected_run, None);

        let ev = Event::RunOpened {
            run: RunId(5),
            task_list_path: PathBuf::from(".tasks/new.json"),
        };
        app.update(AppEvent::ApiEvent(ev));

        assert_eq!(app.runs.len(), 1);
        assert_eq!(
            app.selected_run,
            Some(0),
            "first run should auto-select when list was empty"
        );
    }

    #[test]
    fn run_opened_keeps_existing_selection() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let runs = vec![
            RunView {
                id: RunId(1),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/a.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/b.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
            },
        ];
        let mut app = App::new(api, runs);
        // Select second run.
        app.update(AppEvent::SelectDown);
        assert_eq!(app.selected_run, Some(1));

        // A new run opens — selection must NOT move.
        let ev = Event::RunOpened {
            run: RunId(9),
            task_list_path: PathBuf::from(".tasks/new.json"),
        };
        app.update(AppEvent::ApiEvent(ev));

        assert_eq!(app.runs.len(), 3, "new run must be added");
        assert_eq!(
            app.selected_run,
            Some(1),
            "existing selection must be preserved"
        );
    }

    // ── RunStatusChanged updates displayed status ─────────────────────────────

    #[test]
    fn run_status_changed_updates_status() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
        };
        let mut app = App::new(api, vec![run]);

        let ev = Event::RunStatusChanged {
            run: RunId(1),
            status: RunStatus::Running,
        };
        app.update(AppEvent::ApiEvent(ev));
        assert_eq!(app.runs[0].status, RunStatus::Running);
    }

    #[test]
    fn run_status_changed_to_failed() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![],
        };
        let mut app = App::new(api, vec![run]);

        let ev = Event::RunStatusChanged {
            run: RunId(1),
            status: RunStatus::Failed,
        };
        app.update(AppEvent::ApiEvent(ev));
        assert_eq!(app.runs[0].status, RunStatus::Failed);
    }

    #[test]
    fn api_event_iterations_updated() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("t1"),
                title: "Task 1".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
            }],
        };
        let mut app = App::new(api, vec![run]);

        let ev = Event::TaskIterationsUpdated {
            run: RunId(1),
            task: TaskId::new("t1"),
            gate_iterations: 2,
            review_iterations: 1,
        };
        app.update(AppEvent::ApiEvent(ev));
        assert_eq!(app.runs[0].tasks[0].gate_iterations, 2);
        assert_eq!(app.runs[0].tasks[0].review_iterations, 1);
    }

    // ── Task-status view (task 29): RunLoaded ────────────────────────────────

    /// `AppEvent::RunLoaded` with a full RunView replaces the placeholder entry.
    #[test]
    fn run_loaded_replaces_placeholder_with_full_run_view() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        // Start with a placeholder (empty tasks) inserted by RunOpened.
        let placeholder = RunView {
            id: RunId(42),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
        };
        let mut app = App::new(api, vec![placeholder]);
        assert!(
            app.runs[0].tasks.is_empty(),
            "placeholder should have no tasks"
        );

        // Now feed a RunLoaded event with the full RunView (with tasks).
        let full_run = RunView {
            id: RunId(42),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/test.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("t1"),
                    title: "First task".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
                TaskView {
                    id: TaskId::new("t2"),
                    title: "Second task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("t1")],
                },
            ],
        };
        app.update(AppEvent::RunLoaded(full_run));

        // The placeholder must be replaced (not appended).
        assert_eq!(app.runs.len(), 1, "must still have exactly one run entry");
        assert_eq!(
            app.runs[0].tasks.len(),
            2,
            "tasks must be populated after RunLoaded"
        );
        assert_eq!(app.runs[0].tasks[0].id.0, "t1");
        assert_eq!(app.runs[0].tasks[1].id.0, "t2");
        // Selection must be stable.
        assert_eq!(app.selected_run, Some(0));
    }

    /// `AppEvent::RunLoaded` for an unknown id inserts a new entry.
    #[test]
    fn run_loaded_inserts_new_run_when_id_unknown() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![]);
        assert!(app.runs.is_empty());

        let full_run = RunView {
            id: RunId(7),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/new.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("only"),
                title: "Only task".into(),
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
            }],
        };
        app.update(AppEvent::RunLoaded(full_run));

        assert_eq!(app.runs.len(), 1);
        assert_eq!(app.runs[0].tasks.len(), 1);
        assert_eq!(app.selected_run, Some(0), "auto-selects first run");
    }

    /// `TaskStateChanged` updates the task state and recomputes the aggregate
    /// `RunStatus`, keeping the sidebar badge consistent.
    #[test]
    fn task_state_changed_updates_state_and_aggregate_status() {
        use makina_core::api::{Event, RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("t1"),
                    title: "Task 1".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
                TaskView {
                    id: TaskId::new("t2"),
                    title: "Task 2".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
            ],
        };
        let mut app = App::new(api, vec![run]);

        // Transition t2 to Done → all tasks Done → Completed.
        app.update(AppEvent::ApiEvent(Event::TaskStateChanged {
            run: RunId(1),
            task: TaskId::new("t2"),
            state: TaskState::Done,
        }));

        assert_eq!(
            app.runs[0].tasks[1].state,
            TaskState::Done,
            "t2 state must be updated"
        );
        assert_eq!(
            app.runs[0].status,
            RunStatus::Completed,
            "aggregate status must be Completed when all tasks Done"
        );
    }

    /// `TaskIterationsUpdated` updates only the specified task's counters.
    #[test]
    fn task_iterations_updated_updates_only_targeted_task() {
        use makina_core::api::{Event, RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("t1"),
                    title: "Task 1".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
                TaskView {
                    id: TaskId::new("t2"),
                    title: "Task 2".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
            ],
        };
        let mut app = App::new(api, vec![run]);

        app.update(AppEvent::ApiEvent(Event::TaskIterationsUpdated {
            run: RunId(1),
            task: TaskId::new("t1"),
            gate_iterations: 2,
            review_iterations: 1,
        }));

        assert_eq!(app.runs[0].tasks[0].gate_iterations, 2, "t1 gate iters");
        assert_eq!(app.runs[0].tasks[0].review_iterations, 1, "t1 review iters");
        // t2 must be unchanged.
        assert_eq!(app.runs[0].tasks[1].gate_iterations, 0, "t2 unchanged");
        assert_eq!(app.runs[0].tasks[1].review_iterations, 0, "t2 unchanged");
    }

    // ── Run control + status message (task 31) ────────────────────────────────

    #[test]
    fn status_message_event_sets_field() {
        let mut app = make_app();
        assert!(app.status_message.is_none());
        let redraw = app.update(AppEvent::StatusMessage("Cancel run:3".into()));
        assert!(redraw, "StatusMessage must trigger a redraw");
        assert_eq!(app.status_message.as_deref(), Some("Cancel run:3"));
    }

    #[test]
    fn control_intents_are_pure_noops_in_update() {
        // Start/Pause/Cancel are IO-layer intents; `update` must not mutate
        // state for them (the async execute + status flow lives in the event
        // loop).  They only request a redraw.
        let mut app = make_app();
        for ev in [AppEvent::StartRun, AppEvent::PauseRun, AppEvent::CancelRun] {
            let before = app.status_message.clone();
            let redraw = app.update(ev);
            assert!(redraw);
            assert_eq!(
                app.status_message, before,
                "control intents must not change status_message in update()"
            );
        }
    }

    // ── File browser update logic (task 28) ───────────────────────────────────

    fn browser_entries() -> Vec<DirEntry> {
        vec![
            DirEntry {
                name: "src".into(),
                path: PathBuf::from("/p/src"),
                is_dir: true,
            },
            DirEntry {
                name: "a.md".into(),
                path: PathBuf::from("/p/a.md"),
                is_dir: false,
            },
            DirEntry {
                name: "b.md".into(),
                path: PathBuf::from("/p/b.md"),
                is_dir: false,
            },
        ]
    }

    #[test]
    fn app_starts_in_normal_mode_without_browser() {
        let app = make_app();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.browser.is_none());
        assert!(!app.is_browsing());
    }

    #[test]
    fn browser_opened_enters_browser_mode_with_entries() {
        let mut app = make_app();
        app.update(AppEvent::BrowserOpened {
            dir: PathBuf::from("/p"),
            entries: browser_entries(),
        });
        assert!(app.is_browsing());
        assert_eq!(app.mode, Mode::FileBrowser);
        let b = app.browser.as_ref().expect("browser must be set");
        assert_eq!(b.cwd, PathBuf::from("/p"));
        assert_eq!(b.entries.len(), 3);
        assert_eq!(b.selected, 0);
    }

    #[test]
    fn browser_navigation_moves_and_clamps_selection() {
        let mut app = make_app();
        app.update(AppEvent::BrowserOpened {
            dir: PathBuf::from("/p"),
            entries: browser_entries(),
        });

        app.update(AppEvent::BrowserDown);
        assert_eq!(app.browser.as_ref().unwrap().selected, 1);
        app.update(AppEvent::BrowserDown);
        assert_eq!(app.browser.as_ref().unwrap().selected, 2);
        // Clamp at last.
        app.update(AppEvent::BrowserDown);
        assert_eq!(app.browser.as_ref().unwrap().selected, 2);

        app.update(AppEvent::BrowserUp);
        assert_eq!(app.browser.as_ref().unwrap().selected, 1);
        app.update(AppEvent::BrowserUp);
        app.update(AppEvent::BrowserUp);
        // Clamp at zero.
        assert_eq!(app.browser.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn close_browser_returns_to_normal_mode() {
        let mut app = make_app();
        app.update(AppEvent::BrowserOpened {
            dir: PathBuf::from("/p"),
            entries: browser_entries(),
        });
        assert!(app.is_browsing());

        app.update(AppEvent::CloseBrowser);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.browser.is_none());
        assert!(!app.is_browsing());
    }

    #[test]
    fn browser_opened_refreshes_listing_on_navigation_into_dir() {
        // Simulate entering a subdirectory: a second BrowserOpened replaces the
        // listing and resets the selection (the IO layer drives this).
        let mut app = make_app();
        app.update(AppEvent::BrowserOpened {
            dir: PathBuf::from("/p"),
            entries: browser_entries(),
        });
        app.update(AppEvent::BrowserDown);
        assert_eq!(app.browser.as_ref().unwrap().selected, 1);

        app.update(AppEvent::BrowserOpened {
            dir: PathBuf::from("/p/src"),
            entries: vec![DirEntry {
                name: "deep.md".into(),
                path: PathBuf::from("/p/src/deep.md"),
                is_dir: false,
            }],
        });
        let b = app.browser.as_ref().unwrap();
        assert_eq!(b.cwd, PathBuf::from("/p/src"));
        assert_eq!(b.entries.len(), 1);
        assert_eq!(b.selected, 0, "selection resets when entering a new dir");
    }

    #[test]
    fn open_browser_event_is_noop_for_state() {
        // OpenBrowser is an IO-layer intent; update() itself must not mutate
        // state (no listing is available yet).
        let mut app = make_app();
        app.update(AppEvent::OpenBrowser);
        assert_eq!(app.mode, Mode::Normal, "OpenBrowser alone changes nothing");
        assert!(app.browser.is_none());
    }

    // ── prompt-answer-stream (task 30) ────────────────────────────────────────

    /// Helper: build a run with tasks A and B, and an App focused on it.
    fn make_app_with_tasks() -> App {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/x.json"),
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
        App::new(api, vec![run])
    }

    /// **Live streaming (the done-when):** Feed PromptSent, several
    /// ResponseChunks, and TurnComplete for the focused task and assert the
    /// exchange log accumulates correctly — prompt present + chunks concatenated
    /// into the answer.
    #[test]
    fn live_streaming_prompt_chunks_and_turn_complete() {
        use makina_core::api::{AgentRole, Event, ExchangeEvent, RunId, TaskId};
        let mut app = make_app_with_tasks();

        // Default: task-a is at index 0 (auto-selected).
        assert_eq!(app.selected_task, Some(0));
        let focused_id = TaskId::new("task-a");

        // Feed PromptSent.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: focused_id.clone(),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "implement X".into(),
            },
        }));

        // Feed several ResponseChunks.
        for chunk in &["work", "ing", " on it"] {
            app.update(AppEvent::ApiEvent(Event::AgentExchange {
                run: RunId(1),
                task: focused_id.clone(),
                role: AgentRole::Developer,
                event: ExchangeEvent::ResponseChunk {
                    text: (*chunk).into(),
                },
            }));
        }

        // Feed TurnComplete.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: focused_id.clone(),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        // Assert the log.
        let log = app.exchange_logs.get(&focused_id).expect("log must exist");
        assert_eq!(log.entries.len(), 2, "expect prompt + response entries");

        // Entry 0: prompt.
        assert!(log.entries[0].is_prompt, "first entry must be a prompt");
        assert_eq!(log.entries[0].text, "implement X");
        assert_eq!(log.entries[0].role, AgentRole::Developer);

        // Entry 1: concatenated response.
        assert!(!log.entries[1].is_prompt, "second entry must be a response");
        assert_eq!(
            log.entries[1].text, "working on it",
            "chunks must concatenate in order"
        );
        assert!(
            log.entries[1].complete,
            "TurnComplete must mark entry complete"
        );
    }

    /// Feed a second turn (interleaved Reviewer) and assert ordering + role labels.
    #[test]
    fn live_streaming_second_turn_and_reviewer_role() {
        use makina_core::api::{AgentRole, Event, ExchangeEvent, RunId, TaskId};
        let mut app = make_app_with_tasks();
        let tid = TaskId::new("task-a");

        // First turn: Developer prompt + response.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: tid.clone(),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "dev prompt".into(),
            },
        }));
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: tid.clone(),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: "dev reply".into(),
            },
        }));
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: tid.clone(),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        }));

        // Second turn: Reviewer prompt + response.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: tid.clone(),
            role: AgentRole::Reviewer,
            event: ExchangeEvent::PromptSent {
                text: "review prompt".into(),
            },
        }));
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: tid.clone(),
            role: AgentRole::Reviewer,
            event: ExchangeEvent::ResponseChunk {
                text: "lgtm".into(),
            },
        }));
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: tid.clone(),
            role: AgentRole::Reviewer,
            event: ExchangeEvent::TurnComplete,
        }));

        let log = app.exchange_logs.get(&tid).expect("log must exist");
        assert_eq!(log.entries.len(), 4, "2 prompts + 2 responses");
        // Ordering.
        assert!(log.entries[0].is_prompt);
        assert_eq!(log.entries[0].role, AgentRole::Developer);
        assert_eq!(log.entries[0].text, "dev prompt");
        assert!(!log.entries[1].is_prompt);
        assert_eq!(log.entries[1].role, AgentRole::Developer);
        assert_eq!(log.entries[1].text, "dev reply");
        assert!(log.entries[2].is_prompt);
        assert_eq!(log.entries[2].role, AgentRole::Reviewer);
        assert_eq!(log.entries[2].text, "review prompt");
        assert!(!log.entries[3].is_prompt);
        assert_eq!(log.entries[3].role, AgentRole::Reviewer);
        assert_eq!(log.entries[3].text, "lgtm");
    }

    /// **Focus filtering:** feed AgentExchange for task-a and task-b; focused
    /// on task-a (index 0), only task-a's log must be non-empty; switching to
    /// task-b (index 1) must reveal task-b's log.
    #[test]
    fn focus_filtering_exchange_per_task() {
        use makina_core::api::{AgentRole, Event, ExchangeEvent, RunId, TaskId};
        let mut app = make_app_with_tasks();

        // Switch to Main panel so task navigation works.
        app.update(AppEvent::FocusNext);
        assert_eq!(app.focused_panel, Panel::Main);

        let id_a = TaskId::new("task-a");
        let id_b = TaskId::new("task-b");

        // Feed exchange for task-a.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: id_a.clone(),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "task-a prompt".into(),
            },
        }));

        // Feed exchange for task-b.
        app.update(AppEvent::ApiEvent(Event::AgentExchange {
            run: RunId(1),
            task: id_b.clone(),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "task-b prompt".into(),
            },
        }));

        // Both logs are stored.
        assert!(
            app.exchange_logs.contains_key(&id_a),
            "task-a log must be stored"
        );
        assert!(
            app.exchange_logs.contains_key(&id_b),
            "task-b log must be stored"
        );

        // Focus is on task-a (index 0).
        assert_eq!(app.selected_task, Some(0));
        assert_eq!(app.selected_task_id(), Some(&id_a));

        // Only task-a's log is "shown" by the focus query.
        let focused_log = app
            .exchange_logs
            .get(app.selected_task_id().unwrap())
            .unwrap();
        assert_eq!(focused_log.entries[0].text, "task-a prompt");

        // Navigate to task-b.
        app.update(AppEvent::SelectDown);
        assert_eq!(app.selected_task, Some(1));
        assert_eq!(app.selected_task_id(), Some(&id_b));

        let focused_log_b = app
            .exchange_logs
            .get(app.selected_task_id().unwrap())
            .unwrap();
        assert_eq!(focused_log_b.entries[0].text, "task-b prompt");
    }

    /// **Task selection:** Up/Down moves the focused task when Main is focused
    /// (clamped at both ends).
    #[test]
    fn task_selection_up_down_main_panel() {
        let mut app = make_app_with_tasks();

        // Switch to Main panel.
        app.update(AppEvent::FocusNext);
        assert_eq!(app.focused_panel, Panel::Main);
        assert_eq!(app.selected_task, Some(0));

        // Down: moves to index 1.
        app.update(AppEvent::SelectDown);
        assert_eq!(
            app.selected_task,
            Some(1),
            "SelectDown must move task selection"
        );

        // Down again: clamps at last (index 1 with 2 tasks).
        app.update(AppEvent::SelectDown);
        assert_eq!(app.selected_task, Some(1), "must clamp at last task");

        // Up: moves back to index 0.
        app.update(AppEvent::SelectUp);
        assert_eq!(
            app.selected_task,
            Some(0),
            "SelectUp must move task selection"
        );

        // Up again: clamps at 0.
        app.update(AppEvent::SelectUp);
        assert_eq!(app.selected_task, Some(0), "must clamp at first task");
    }

    /// Exchange-pane scroll: manual offset clamps to `[0, scroll_max]`,
    /// scrolling up disengages auto-follow, and scrolling back down to the
    /// bottom re-engages it.
    #[test]
    fn exchange_scroll_clamps_and_auto_follow_reengages() {
        let mut app = make_app();
        let max: u16 = 3;

        // Default: auto-follow engaged, offset at the top.
        assert!(app.exchange_auto_follow);
        assert_eq!(app.exchange_scroll, 0);

        // (2) scroll_up clears auto-follow.
        app.scroll_up();
        assert!(
            !app.exchange_auto_follow,
            "scroll_up must clear exchange_auto_follow"
        );

        // (1) scroll_up never goes below 0.
        app.scroll_up();
        app.scroll_up();
        assert_eq!(app.exchange_scroll, 0, "scroll_up must not go below 0");

        // (1) scroll_down never exceeds max.
        for _ in 0..(max + 5) {
            app.scroll_down(max);
            assert!(
                app.exchange_scroll <= max,
                "scroll_down must never exceed scroll_max"
            );
        }
        assert_eq!(app.exchange_scroll, max);

        // (3) scroll_down reaching max re-sets auto-follow.
        assert!(
            app.exchange_auto_follow,
            "scroll_down reaching scroll_max must re-set exchange_auto_follow"
        );
    }

    /// Mouse-wheel scroll events (`ScrollUp`/`ScrollDown`) change the exchange
    /// scroll offset WITHOUT touching task selection (task `tui-mouse-scroll`).
    /// Scrolling is orthogonal to task switching, which stays on keys/sidebar.
    #[test]
    fn scroll_event_changes_offset_not_selection() {
        let mut app = make_app_with_tasks();

        // Record the task selection before scrolling.
        let selected_before = app.selected_task;
        assert_eq!(selected_before, Some(0));

        // ScrollDown nudges the manual offset down by one line.
        let offset_before = app.exchange_scroll;
        app.update(AppEvent::ScrollDown);
        assert_ne!(
            app.exchange_scroll, offset_before,
            "ScrollDown must change the exchange scroll offset"
        );
        assert_eq!(
            app.selected_task, selected_before,
            "ScrollDown must NOT change task selection"
        );

        // ScrollUp moves the offset back up and disengages auto-follow.
        let offset_after_down = app.exchange_scroll;
        app.update(AppEvent::ScrollUp);
        assert_ne!(
            app.exchange_scroll, offset_after_down,
            "ScrollUp must change the exchange scroll offset"
        );
        assert!(
            !app.exchange_auto_follow,
            "ScrollUp must disengage auto-follow"
        );
        assert_eq!(
            app.selected_task, selected_before,
            "ScrollUp must NOT change task selection"
        );
    }

    /// Task selection: run navigation (Sidebar focus) must NOT change
    /// `selected_task` beyond what the new run's task count allows.
    #[test]
    fn task_selection_sidebar_nav_does_not_touch_run_selection() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        // Two runs, each with tasks.
        let run1 = RunView {
            id: RunId(1),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/r1.json"),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    id: TaskId::new("t1"),
                    title: "T1".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
                TaskView {
                    id: TaskId::new("t2"),
                    title: "T2".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                },
            ],
        };
        let run2 = RunView {
            id: RunId(2),
            run_uid: String::new(),
            task_list_path: PathBuf::from(".tasks/r2.json"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("ta"),
                title: "TA".into(),
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
            }],
        };
        let mut app = App::new(api, vec![run1, run2]);

        // Initially: sidebar focused, run index 0, task index 0.
        assert_eq!(app.focused_panel, Panel::Sidebar);
        assert_eq!(app.selected_run, Some(0));
        assert_eq!(app.selected_task, Some(0));

        // Navigate to run 1 via sidebar.
        app.update(AppEvent::SelectDown);
        assert_eq!(app.selected_run, Some(1), "run selection must move");
        // task index should be clamped to the new run's bounds (run1 has 1 task).
        assert_eq!(
            app.selected_task,
            Some(0),
            "task selection clamped to new run's bounds"
        );
    }

    /// **Bounding:** feeding many chunks/turns must cap the exchange log at
    /// [`EXCHANGE_LOG_CAP`] entries (no unbounded growth).
    #[test]
    fn exchange_log_bounded_at_cap() {
        use crate::app::EXCHANGE_LOG_CAP;
        use makina_core::api::{AgentRole, Event, ExchangeEvent, RunId, TaskId};
        let mut app = make_app_with_tasks();
        let tid = TaskId::new("task-a");

        // Feed more than EXCHANGE_LOG_CAP entries (each prompt + response is 2).
        let total_turns = EXCHANGE_LOG_CAP + 10;
        for i in 0..total_turns {
            app.update(AppEvent::ApiEvent(Event::AgentExchange {
                run: RunId(1),
                task: tid.clone(),
                role: AgentRole::Developer,
                event: ExchangeEvent::PromptSent {
                    text: format!("prompt {i}"),
                },
            }));
            app.update(AppEvent::ApiEvent(Event::AgentExchange {
                run: RunId(1),
                task: tid.clone(),
                role: AgentRole::Developer,
                event: ExchangeEvent::ResponseChunk {
                    text: format!("resp {i}"),
                },
            }));
            app.update(AppEvent::ApiEvent(Event::AgentExchange {
                run: RunId(1),
                task: tid.clone(),
                role: AgentRole::Developer,
                event: ExchangeEvent::TurnComplete,
            }));
        }

        let log = app.exchange_logs.get(&tid).expect("log must exist");
        assert!(
            log.entries.len() <= EXCHANGE_LOG_CAP,
            "log must be capped at EXCHANGE_LOG_CAP={} but has {} entries",
            EXCHANGE_LOG_CAP,
            log.entries.len()
        );
    }

    /// ExchangeLog direct unit tests — PromptSent, ResponseChunk, TurnComplete.
    #[test]
    fn exchange_log_prompt_and_chunk_accumulation() {
        use crate::app::ExchangeLog;
        use makina_core::api::AgentRole;

        let mut log = ExchangeLog::default();
        log.add_prompt(AgentRole::Developer, "hello".into());
        assert_eq!(log.entries.len(), 1);
        assert!(log.entries[0].is_prompt);
        assert_eq!(log.entries[0].text, "hello");
        assert!(log.entries[0].complete);

        log.append_chunk(AgentRole::Developer, "chunk1".into());
        assert_eq!(log.entries.len(), 2);
        assert!(!log.entries[1].is_prompt);
        assert_eq!(log.entries[1].text, "chunk1");
        assert!(!log.entries[1].complete);

        log.append_chunk(AgentRole::Developer, " chunk2".into());
        assert_eq!(
            log.entries.len(),
            2,
            "chunks must accumulate, not add new entries"
        );
        assert_eq!(log.entries[1].text, "chunk1 chunk2");

        log.complete_turn();
        assert!(
            log.entries[1].complete,
            "TurnComplete must mark response complete"
        );
    }
}
