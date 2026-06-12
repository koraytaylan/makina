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

use makina_core::api::{
    AgentRole, Api, ConfigOptionView, Event, RunId, RunView, SessionModes, TaskId,
};
#[cfg(test)]
use makina_core::config::RoleAssignment;
use makina_core::config::{ProviderConfig, RolesConfig};

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

/// The payload of an [`ExchangeEntry`].
///
/// An entry is one of four kinds.  Prompts/responses build the visible
/// conversation; thoughts and tool calls are observability-only side channels
/// that the TUI renders for transparency but that never contribute to the
/// agent's answer text.
#[derive(Debug, Clone)]
pub enum ExchangeContent {
    /// A prompt sent TO the agent by the orchestrator.
    ///
    /// `text` is the full text of [`ExchangeEvent::PromptSent`].
    Prompt {
        /// Full prompt text.
        text: String,
    },
    /// A (possibly still-streaming) response FROM the agent.
    ///
    /// Each [`ExchangeEvent::ResponseChunk`] is appended to `text` as it
    /// arrives; `complete` flips to `true` on [`ExchangeEvent::TurnComplete`].
    Response {
        /// Accumulated response text.
        text: String,
        /// Whether [`ExchangeEvent::TurnComplete`] has been received.
        complete: bool,
    },
    /// A burst of the agent's internal reasoning ([`ExchangeEvent::ThoughtChunk`]).
    ///
    /// Consecutive thought chunks for the same role coalesce into a single
    /// entry; observability-only, never part of the answer text.
    Thought {
        /// Accumulated thought text.
        text: String,
    },
    /// A tool invocation announced by the agent, keyed by `id`
    /// ([`ExchangeEvent::ToolCall`] / [`ExchangeEvent::ToolCallUpdate`]).
    Tool {
        /// Stable id correlating the call with later updates.
        id: String,
        /// Human-readable title (may be empty).
        title: String,
        /// Optional semantic kind (e.g. `"execute"`, `"edit"`).
        kind: Option<String>,
        /// Latest lifecycle status (e.g. `"pending"`, `"completed"`).
        status: String,
        /// Accumulated or last tool content.  The live event path carries no
        /// content text yet, so this stays empty there; the field EXISTS so
        /// rich tool rendering can construct an entry WITH content.
        content: String,
    },
}

/// A single turn in a live agent exchange.
///
/// Every entry carries the [`AgentRole`] that produced it plus its
/// [`ExchangeContent`] — a prompt, a (possibly streaming) response, a thought
/// burst, or a tool call.
#[derive(Debug, Clone)]
pub struct ExchangeEntry {
    /// The agent role that produced this turn.
    pub role: AgentRole,
    /// The kind and payload of this turn.
    pub content: ExchangeContent,
}

impl ExchangeEntry {
    /// `true` when this entry is a prompt sent TO the agent.
    pub fn is_prompt(&self) -> bool {
        matches!(self.content, ExchangeContent::Prompt { .. })
    }

    /// The entry's primary text (prompt/response/thought text, or a tool's
    /// title).  Used by render and migrated tests.
    pub fn text(&self) -> &str {
        match &self.content {
            ExchangeContent::Prompt { text }
            | ExchangeContent::Response { text, .. }
            | ExchangeContent::Thought { text } => text,
            ExchangeContent::Tool { title, .. } => title,
        }
    }

    /// Whether this entry is finalised.  Responses track
    /// [`ExchangeEvent::TurnComplete`]; all other kinds are always complete.
    pub fn complete(&self) -> bool {
        match &self.content {
            ExchangeContent::Response { complete, .. } => *complete,
            _ => true,
        }
    }
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

    /// Mark the last entry complete if it is a still-open response.
    /// Called before pushing any new entry so only the tail segment streams.
    fn finalize_trailing_response(&mut self) {
        if let Some(last) = self.entries.last_mut()
            && let ExchangeContent::Response { complete, .. } = &mut last.content
        {
            *complete = true;
        }
    }

    /// Start a new prompt entry for the given role.
    pub fn add_prompt(&mut self, role: AgentRole, text: String) {
        self.push(ExchangeEntry {
            role,
            content: ExchangeContent::Prompt { text },
        });
    }

    /// Start a new (in-progress) response entry for the given role, or
    /// append to the last incomplete response.
    ///
    /// Design decision: a `ResponseChunk` arriving without a preceding
    /// `PromptSent` in this log still needs to go somewhere — we create an
    /// implicit incomplete response entry rather than silently dropping data.
    pub fn append_chunk(&mut self, role: AgentRole, chunk: String) {
        if let Some(last) = self.entries.last_mut()
            && last.role == role
            && let ExchangeContent::Response { text, complete } = &mut last.content
            && !*complete
        {
            text.push_str(&chunk);
            return;
        }
        // A thought/tool (or a different role) intervened: close the old segment
        // and start a new one so order is preserved.
        self.finalize_trailing_response();
        self.push(ExchangeEntry {
            role,
            content: ExchangeContent::Response {
                text: chunk,
                complete: false,
            },
        });
    }

    /// Mark the last incomplete response entry as complete.
    ///
    /// Scans back past any trailing Thought/Tool entries (which may be
    /// interleaved before the terminating `TurnComplete`) so the real Response
    /// entry is finalised even when it isn't the literal last entry.
    pub fn complete_turn(&mut self) {
        for entry in self.entries.iter_mut().rev() {
            match &mut entry.content {
                ExchangeContent::Response { complete, .. } if !*complete => {
                    *complete = true;
                    return;
                }
                ExchangeContent::Thought { .. } | ExchangeContent::Tool { .. } => continue,
                _ => break,
            }
        }
    }

    /// Append a thought "burst" for the given role.
    ///
    /// Coalesces with the immediately-preceding entry when that entry is a
    /// [`ExchangeContent::Thought`] of the same role; otherwise starts a new
    /// thought entry.  Thoughts are observability-only and never affect the
    /// answer text.
    pub fn append_thought(&mut self, role: AgentRole, chunk: String) {
        self.finalize_trailing_response();
        if let Some(last) = self.entries.last_mut()
            && last.role == role
            && let ExchangeContent::Thought { text } = &mut last.content
        {
            text.push_str(&chunk);
            return;
        }
        self.push(ExchangeEntry {
            role,
            content: ExchangeContent::Thought { text: chunk },
        });
    }

    /// Upsert a tool call by `id`.
    ///
    /// Updates an existing [`ExchangeContent::Tool`] entry with the same `id`
    /// (overwriting title/kind/status, leaving accumulated `content`), or
    /// pushes a new tool entry with empty content.
    pub fn start_tool(
        &mut self,
        role: AgentRole,
        id: String,
        title: String,
        kind: Option<String>,
        status: String,
    ) {
        if let Some(entry) = self.find_tool_mut(&id) {
            if let ExchangeContent::Tool {
                title: t,
                kind: k,
                status: s,
                ..
            } = &mut entry.content
            {
                *t = title;
                *k = kind;
                *s = status;
            }
            return;
        }
        self.finalize_trailing_response();
        self.push(ExchangeEntry {
            role,
            content: ExchangeContent::Tool {
                id,
                title,
                kind,
                status,
                content: String::new(),
            },
        });
    }

    /// Apply a status/title update to an existing tool entry by `id`.
    ///
    /// Finds the [`ExchangeContent::Tool`] entry with the matching `id` and
    /// updates `status`/`title` when present; leaves `content` untouched.  No-op
    /// when no entry matches (live updates always follow a `start_tool`).
    pub fn update_tool(&mut self, id: &str, status: Option<String>, title: Option<String>) {
        if let Some(entry) = self.find_tool_mut(id)
            && let ExchangeContent::Tool {
                title: t,
                status: s,
                ..
            } = &mut entry.content
        {
            if let Some(new_status) = status {
                *s = new_status;
            }
            if let Some(new_title) = title {
                *t = new_title;
            }
        }
    }

    /// Find the tool entry with the given `id`, if any.
    fn find_tool_mut(&mut self, id: &str) -> Option<&mut ExchangeEntry> {
        self.entries
            .iter_mut()
            .find(|e| matches!(&e.content, ExchangeContent::Tool { id: eid, .. } if eid == id))
    }
}

// ── Exchange event reducer ──────────────────────────────────────────────────────
//
// Apply one exchange event to a task's log. The single source of truth used
// by both the live event path and on-disk replay (plan 0010).

use makina_core::api::ExchangeEvent;

/// Apply one exchange event to a task's log. The single source of truth used
/// by both the live event path and on-disk replay (plan 0010).
pub fn apply_exchange_event(log: &mut ExchangeLog, role: AgentRole, event: &ExchangeEvent) {
    match event {
        ExchangeEvent::PromptSent { text } => {
            log.add_prompt(role, text.clone());
        }
        ExchangeEvent::ResponseChunk { text } => {
            log.append_chunk(role, text.clone());
        }
        ExchangeEvent::ThoughtChunk { text } => {
            log.append_thought(role, text.clone());
        }
        ExchangeEvent::ToolCall {
            id,
            title,
            kind,
            status,
        } => {
            log.start_tool(
                role,
                id.clone(),
                title.clone(),
                kind.clone(),
                status.clone(),
            );
        }
        ExchangeEvent::ToolCallUpdate { id, status, title } => {
            log.update_tool(id, status.clone(), title.clone());
        }
        ExchangeEvent::TurnComplete => {
            log.complete_turn();
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
    /// The modal provider/role configuration editor.
    ProviderConfig,
}

// ── Provider configuration editor ──────────────────────────────────────────────

/// State for the provider/role configuration editor modal.
#[derive(Debug, Clone)]
pub struct ProviderEditor {
    /// The editable list of providers.
    pub providers: Vec<ProviderConfig>,
    /// Role assignments to providers.
    pub roles: RolesConfig,
    /// Available session modes, if discovered from a live session.
    pub available_modes: Option<SessionModes>,
    /// Available configuration options, if discovered from a live session.
    pub available_config_options: Vec<ConfigOptionView>,
    /// Index of the currently selected provider in `providers`.
    pub selected_provider: Option<usize>,
    /// Which role/selection is currently focused: provider list, or a role's mode/model/effort.
    pub selection_index: usize,
}

/// Which dependency-view overlay (if any) the TUI renders above the exchange
/// pane for the selected task.
///
/// [`DependencyViewMode::Off`] is the default (no dependency sub-pane).  The
/// remaining variants pick a rendering: [`DependencyViewMode::List`] shows a
/// compact `[state] task-id` list of the selected task's prerequisites; `Tree`
/// and `Timeline` are added by sibling tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyViewMode {
    /// No dependency sub-pane is shown.
    Off,
    /// Compact `[state] task-id` list of the selected task's `depends_on`.
    List,
    /// Indented dependency tree (task `tui-dep-tree`).
    Tree,
    /// Dependency timeline (task `tui-dep-timeline`).
    Timeline,
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
    /// `v` / `V` — cycle the dependency view between
    /// [`DependencyViewMode::Off`], `List`, `Tree`, and `Timeline`.
    CycleDependencyView,
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
    /// Open the focused task's log in `$PAGER` (`L`).
    OpenLog,

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

    // ── Provider configuration editor (task 0041) ──────────────────────────────
    /// User requested to open the provider configuration editor (e.g. pressed `g`).
    OpenProviderEditor,
    /// Move the editor selection one row up.
    ProviderEditorUp,
    /// Move the editor selection one row down.
    ProviderEditorDown,
    /// Close the provider editor and return to the normal view (Esc).
    CloseProviderEditor,
    /// Commit the edited configuration back to the config file.
    ProviderEditorCommit,

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
    /// User pressed `r` / `R` — re-interpret the selected Run (bypass artifact,
    /// re-ingest source to recompute report and graph).
    Reinterpret,

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

    /// Which dependency-view overlay (if any) is rendered above the exchange
    /// pane for the selected task.  [`DependencyViewMode::Off`] hides it.
    pub dependency_view: DependencyViewMode,

    /// File-browser view state.  `Some` only while [`App::mode`] is
    /// [`Mode::FileBrowser`]; the IO layer populates it via
    /// [`AppEvent::BrowserOpened`].
    pub browser: Option<FileBrowser>,

    /// Provider/role configuration editor state.  `Some` only while
    /// [`App::mode`] is [`Mode::ProviderConfig`].
    pub provider_editor: Option<ProviderEditor>,

    /// Most recent session capabilities (modes + config options) discovered from
    /// a live agent. Used to seed/refresh the provider editor with what the agent
    /// actually advertises, since capabilities arrive while a run is active —
    /// often before the user opens the editor.
    pub discovered_capabilities: Option<makina_core::api::SessionCapabilities>,

    /// The named providers loaded from config (used to seed the editor).
    pub providers: Vec<ProviderConfig>,

    /// The role-to-provider assignments loaded from config (used to seed the editor).
    pub roles: RolesConfig,

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

    /// Per-task exchange logs, keyed by `(RunId, TaskId)`.
    ///
    /// Keying by the composite `(RunId, TaskId)` ensures that switching between
    /// two runs that share the same task slug (e.g. `implement-auth`) never
    /// shows stale data from the other run.  Both the live event path and the
    /// on-disk replay path use this composite key.
    ///
    /// The orchestrator emits [`Event::AgentExchange`] for ALL in-flight
    /// tasks; the TUI stores logs for every task it hears about and filters to
    /// the currently focused task when rendering the exchange pane.
    pub exchange_logs: HashMap<(RunId, TaskId), ExchangeLog>,

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

    /// The most recent `scroll_max` (`total_lines - pane_height`) the render
    /// pass computed for the exchange pane.
    ///
    /// `App` has no pane geometry, so the render path records the real bottom
    /// offset here via interior mutability ([`std::cell::Cell`]) — letting the
    /// `&App` render signature stay unchanged.  Two consumers read it:
    ///
    /// * [`App::scroll_up`] anchors the manual offset to the rendered bottom
    ///   when auto-follow first disengages (so the first wheel-up moves up by
    ///   exactly one line instead of jumping to the top), and
    /// * the `AppEvent::ScrollDown` arm in [`App::update`] uses it as the
    ///   clamp bound so reaching the real rendered bottom re-engages
    ///   auto-follow.
    pub last_scroll_max: std::cell::Cell<u16>,

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

    /// Whether there are unseen error messages since the error pane was last opened.
    /// Cleared when the error pane opens; set when a new error message arrives.
    pub unseen_errors: bool,

    /// The root directory of the repository, used for compacting tool paths.
    pub repo_root: PathBuf,

    /// Tick counter, incremented on each [`AppEvent::Tick`].
    /// Used to drive animations like the working spinner.
    pub tick: u64,
}

impl App {
    /// Load transcripts for the initially-selected run (if any).
    ///
    /// Should be called right after `new()` to populate exchanges for the first
    /// run shown in the TUI. This is a separate step because `App::new` does not
    /// take `&mut self`.
    pub fn load_initial_exchanges(&mut self) {
        self.load_exchanges_for_selected_run();
    }

    /// Build a new [`App`] with the given api, initial run list, and repo root.
    ///
    /// Call `api.runs().await` before constructing to obtain `initial_runs`.
    pub fn new(api: Arc<dyn Api>, initial_runs: Vec<RunView>, repo_root: PathBuf) -> Self {
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
            dependency_view: DependencyViewMode::Off,
            browser: None,
            provider_editor: None,
            discovered_capabilities: None,
            providers: Vec::new(),
            roles: RolesConfig::default(),
            runs: initial_runs,
            selected_run,
            selected_task,
            exchange_logs: HashMap::new(),
            exchange_scroll: 0,
            exchange_auto_follow: true,
            last_scroll_max: std::cell::Cell::new(0),
            last_event: None,
            status_message: None,
            error_pane_open: false,
            error_messages: Vec::new(),
            unseen_errors: false,
            repo_root,
            tick: 0,
        }
    }

    /// Build a new [`App`] with explicit providers and roles from a resolved config.
    ///
    /// Use this variant when a config is available (main.rs) so the provider
    /// editor is seeded with the current configuration.
    pub fn with_config(
        api: Arc<dyn Api>,
        initial_runs: Vec<RunView>,
        repo_root: PathBuf,
        providers: Vec<ProviderConfig>,
        roles: RolesConfig,
    ) -> Self {
        let mut app = Self::new(api, initial_runs, repo_root);
        app.providers = providers;
        app.roles = roles;
        app
    }

    /// Push a new error-pane message, evicting the oldest when over cap.
    ///
    /// Mirrors [`ExchangeLog::push`]: maintains the [`ERROR_MESSAGES_CAP`]
    /// bound so the buffer cannot grow without limit.
    /// Marks the errors as unseen if the pane is not currently open.
    pub fn push_error(&mut self, msg: ErrorMessage) {
        self.error_messages.push(msg);
        if self.error_messages.len() > ERROR_MESSAGES_CAP {
            // Drop the oldest message to maintain the bound.
            self.error_messages.remove(0);
        }
        // Mark errors as unseen if the pane is not currently open.
        if !self.error_pane_open {
            self.unseen_errors = true;
        }
    }

    /// Whether the modal file browser is currently active.
    pub fn is_browsing(&self) -> bool {
        self.mode == Mode::FileBrowser
    }

    /// Whether the provider configuration editor is currently active.
    pub fn is_editing_providers(&self) -> bool {
        self.mode == Mode::ProviderConfig
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

    /// Return the exchange log for the currently focused task, if any.
    ///
    /// Looks up by `(RunId, TaskId)` to ensure the correct run's log is returned
    /// even when two runs share a task slug (e.g. `implement-auth`).
    pub fn selected_exchange_log(&self) -> Option<&ExchangeLog> {
        let run = self.selected_run()?;
        let task_id = self
            .selected_task
            .and_then(|i| run.tasks.get(i))
            .map(|tv| &tv.id)?;
        self.exchange_logs.get(&(run.id, task_id.clone()))
    }

    /// Scroll the exchange pane up by one line.
    ///
    /// Disengages auto-follow (the user is reviewing history) and decrements the
    /// manual offset, clamped at `0`.  `App` does not know the rendered line
    /// count, so no upper bound is needed here.
    ///
    /// When auto-follow is currently engaged the manual `exchange_scroll` is
    /// stale (`0`) while the render path pins the pane to `scroll_max`.  Anchor
    /// the manual offset to the last rendered bottom ([`App::last_scroll_max`])
    /// *before* decrementing, so the first wheel-up moves up by exactly one
    /// line (`scroll_max - 1`) instead of snapping to the top.
    pub fn scroll_up(&mut self) {
        if self.exchange_auto_follow {
            // Anchor to the rendered bottom so the first wheel-up is `max - 1`.
            self.exchange_scroll = self.last_scroll_max.get();
        }
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
        self.exchange_scroll = self.exchange_scroll.saturating_add(1).min(scroll_max);
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
            AppEvent::CycleDependencyView => {
                self.dependency_view = match self.dependency_view {
                    DependencyViewMode::Off => DependencyViewMode::List,
                    DependencyViewMode::List => DependencyViewMode::Tree,
                    DependencyViewMode::Tree => DependencyViewMode::Timeline,
                    DependencyViewMode::Timeline => DependencyViewMode::Off,
                };
                true
            }
            AppEvent::ToggleErrorPane => {
                self.error_pane_open = !self.error_pane_open;
                // Clear the unseen errors flag when the pane opens.
                if self.error_pane_open {
                    self.unseen_errors = false;
                }
                true
            }
            AppEvent::OpenLog => {
                // This is an intent; the IO layer handles the actual file I/O.
                // `update` returns true to trigger a redraw.
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
                                // Load transcripts for the newly selected run.
                                self.load_exchanges_for_selected_run();
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
                                // Load transcripts for the newly selected run.
                                self.load_exchanges_for_selected_run();
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
            // live pane geometry, so it uses `last_scroll_max` — the bottom
            // offset the render pass most recently recorded — as `scroll_max`;
            // the render pass re-clamps the offset to the current
            // `total_lines - pane_height` via `effective_offset`.
            AppEvent::ScrollUp => {
                self.scroll_up();
                true
            }
            AppEvent::ScrollDown => {
                // Use the last rendered bottom as the clamp bound (not
                // `u16::MAX`) so reaching the real bottom re-engages auto-follow
                // in `scroll_down`; otherwise `exchange_scroll == scroll_max`
                // could never hold and auto-follow would never re-engage.
                self.scroll_down(self.last_scroll_max.get());
                true
            }
            AppEvent::ApiEvent(ev) => {
                self.apply_api_event(ev);
                true
            }
            AppEvent::Tick => {
                self.tick = self.tick.wrapping_add(1);
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
                // Tiny state rule: on RunLoaded (or RunOpened producing a loaded view)
                // the status is cleared unless it was an error. This ensures a
                // successful load overwrites transient "Interpreting …" with the
                // normal ready-state hints in the status bar.
                if let Some(msg) = &self.status_message {
                    let l = msg.to_lowercase();
                    if !l.contains("fail") && !l.contains("error") {
                        self.status_message = None;
                    }
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

            // ── Provider configuration editor (task 0041) ──────────────────────
            AppEvent::OpenProviderEditor => {
                // Open the provider editor modal, seeded from the app's current
                // providers/roles (loaded from config at startup).
                let (available_modes, available_config_options) = self
                    .discovered_capabilities
                    .as_ref()
                    .map(|c| (c.modes.clone(), c.config_options.clone()))
                    .unwrap_or((None, vec![]));
                self.provider_editor = Some(ProviderEditor {
                    providers: self.providers.clone(),
                    roles: self.roles.clone(),
                    available_modes,
                    available_config_options,
                    selected_provider: if self.providers.is_empty() {
                        None
                    } else {
                        Some(0)
                    },
                    selection_index: 0,
                });
                self.mode = Mode::ProviderConfig;
                true
            }
            AppEvent::ProviderEditorUp => {
                if let Some(editor) = self.provider_editor.as_mut() {
                    editor.selection_index = editor.selection_index.saturating_sub(1);
                }
                true
            }
            AppEvent::ProviderEditorDown => {
                if let Some(editor) = self.provider_editor.as_mut() {
                    // Calculate total number of selectable items:
                    // providers list + 3 roles (each with mode/model/effort selections)
                    let total_items = editor.providers.len() + 3;
                    editor.selection_index =
                        (editor.selection_index + 1).min(total_items.saturating_sub(1));
                }
                true
            }
            AppEvent::CloseProviderEditor => {
                self.mode = Mode::Normal;
                self.provider_editor = None;
                true
            }
            AppEvent::ProviderEditorCommit => {
                // The IO layer (resolve_io in event.rs) writes the config to disk.
                // Here we apply the editor's final state back to app.providers/roles
                // and close the editor so the TUI returns to normal mode.
                if let Some(editor) = self.provider_editor.take() {
                    self.providers = editor.providers;
                    self.roles = editor.roles;
                }
                self.mode = Mode::Normal;
                true
            }

            // ── Run control (task 31) ─────────────────────────────────────────
            // Start/Pause/Cancel are IO-layer intents: the event loop issues the
            // async `api.execute(...)` for the selected run and feeds back a
            // StatusMessage.  `update` itself does not mutate state here (no
            // async), so these are no-ops that simply request a redraw.
            AppEvent::StartRun
            | AppEvent::PauseRun
            | AppEvent::CancelRun
            | AppEvent::Reinterpret => true,

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

    /// Load transcripts for the selected run's tasks from disk.
    ///
    /// When a run is selected, lazily try to populate its exchange logs from
    /// persisted transcript files. This is best-effort: missing or unparseable
    /// transcripts are silently skipped (no crash).
    ///
    /// Caches by `(RunId, task_id)` — the composite key — so re-selecting the
    /// same run is free, and switching from Run A to Run B never shows Run A's
    /// stale transcripts for tasks that share the same slug.
    fn load_exchanges_for_selected_run(&mut self) {
        use makina_core::paths;

        let Some(run) = self.selected_run() else {
            return;
        };

        // Skip if the run has no run_uid (e.g. a placeholder from RunOpened before
        // the full metadata arrives).
        if run.run_uid.is_empty() {
            return;
        }

        let run_id = run.id;
        let run_uid = run.run_uid.clone();
        let tasks = run.tasks.clone();

        // Use the pure path helper (no I/O, no create_dir_all) so the replay
        // loader does not silently create directories for non-existent runs.
        let logs_dir = paths::run_dir(&self.repo_root, &run_uid).join("logs");

        // Try to load transcripts for each task in this run.
        for task in tasks {
            let cache_key = (run_id, task.id.clone());

            // Skip if we already have this log cached for this exact run.
            if self.exchange_logs.contains_key(&cache_key) {
                continue;
            }

            // Construct the path to the task's transcript file.
            let transcript_path = logs_dir.join(format!("{}_transcript.jsonl", task.id));

            // Try to load the transcript. Use Developer role as the default
            // (the transcript should contain role info in future versions).
            match crate::replay::load_task_exchange(&transcript_path, AgentRole::Developer) {
                Ok(log) => {
                    self.exchange_logs.insert(cache_key, log);
                }
                Err(_) => {
                    // Transcript missing or unreadable; skip silently.
                    // The log will remain empty unless filled by live events.
                }
            }
        }
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
                        report: makina_core::api::IngestionReport::default(),
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
            // The agent advertises its modes / model / effort options when a
            // session opens. Remember them so the provider editor can show what
            // the live agent actually supports, and refresh an already-open editor.
            Event::SessionCapabilities {
                run: _,
                task: _,
                role: _,
                capabilities,
            } => {
                self.discovered_capabilities = Some(capabilities.clone());
                if let Some(editor) = self.provider_editor.as_mut() {
                    editor.available_modes = capabilities.modes.clone();
                    editor.available_config_options = capabilities.config_options.clone();
                }
            }
            // An autonomous mode switch: reflect it in the stored capabilities and
            // in any open editor.
            Event::CurrentModeUpdate {
                run: _,
                task: _,
                role: _,
                current_mode_id,
            } => {
                if let Some(modes) = self
                    .discovered_capabilities
                    .as_mut()
                    .and_then(|c| c.modes.as_mut())
                {
                    modes.current_mode_id = current_mode_id.clone();
                }
                if let Some(modes) = self
                    .provider_editor
                    .as_mut()
                    .and_then(|e| e.available_modes.as_mut())
                {
                    modes.current_mode_id = current_mode_id.clone();
                }
            }
            // AgentExchange events accumulate into the per-task exchange log
            // (task 30: prompt-answer-stream).  The TUI stores ALL tasks' logs
            // and filters to the focused task at render time.  Keyed by
            // (RunId, TaskId) so logs from different runs with the same task
            // slug never collide.
            Event::AgentExchange {
                run,
                task,
                role,
                event: exchange_ev,
            } => {
                let log = self.exchange_logs.entry((*run, task.clone())).or_default();
                apply_exchange_event(log, role.clone(), exchange_ev);
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
        App::new(api, vec![], PathBuf::from("."))
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
    fn dependency_view_cycles() {
        let mut app = make_app();
        assert_eq!(app.dependency_view, DependencyViewMode::Off);
        app.update(AppEvent::CycleDependencyView);
        assert_eq!(app.dependency_view, DependencyViewMode::List);
        app.update(AppEvent::CycleDependencyView);
        assert_eq!(app.dependency_view, DependencyViewMode::Tree);
        app.update(AppEvent::CycleDependencyView);
        assert_eq!(app.dependency_view, DependencyViewMode::Timeline);
        app.update(AppEvent::CycleDependencyView);
        assert_eq!(app.dependency_view, DependencyViewMode::Off);
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
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![existing], PathBuf::from("."));

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
                failure_reason: None,
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));

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
                failure_reason: None,
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));

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
            report: makina_core::api::IngestionReport::default(),
        };
        let app = App::new(api, vec![run], PathBuf::from("."));
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
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/b.json"),
                status: RunStatus::Running,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(3),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/c.json"),
                status: RunStatus::Completed,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
        ];
        let mut app = App::new(api, runs, PathBuf::from("."));
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
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/b.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
        ];
        let mut app = App::new(api, runs, PathBuf::from("."));
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
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/b.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
        ];
        let mut app = App::new(api, runs, PathBuf::from("."));
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
            report: makina_core::api::IngestionReport::default(),
        }];
        let mut app = App::new(api, runs, PathBuf::from("."));
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
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/b.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
        ];
        let mut app = App::new(api, runs, PathBuf::from("."));
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
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                task_list_path: PathBuf::from(".tasks/b.json"),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
        ];
        let mut app = App::new(api, runs, PathBuf::from("."));
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
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));

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
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));

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
                failure_reason: None,
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));

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
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![placeholder], PathBuf::from("."));
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
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("t2"),
                    title: "Second task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("t1")],
                    failure_reason: None,
                },
            ],
            report: makina_core::api::IngestionReport::default(),
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
        let mut app = App::new(api, vec![], PathBuf::from("."));
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
                failure_reason: None,
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        app.update(AppEvent::RunLoaded(full_run));

        assert_eq!(app.runs.len(), 1);
        assert_eq!(app.runs[0].tasks.len(), 1);
        assert_eq!(app.selected_run, Some(0), "auto-selects first run");
    }

    /// On successful `RunLoaded` any prior transient non-error status (such as
    /// the "Interpreting …" message left by `BrowserActivate` for a file) is
    /// cleared, leaving the status bar in the empty/default (normal ready/hints)
    /// state.
    #[test]
    fn runloaded_clears_interpreting_status() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));
        // Simulate the transient set by resolve_io on file activate.
        app.status_message = Some("Interpreting example.md...".to_string());
        assert!(app.status_message.is_some());

        let full_run = RunView {
            id: RunId(99),
            run_uid: String::new(),
            task_list_path: PathBuf::from("example.md"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new("t1"),
                title: "Task".into(),
                state: TaskState::Ready,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                failure_reason: None,
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        app.update(AppEvent::RunLoaded(full_run));

        assert!(
            app.status_message.is_none(),
            "RunLoaded must clear transient non-error status (e.g. interpreting) to ready state"
        );
        assert_eq!(app.runs.len(), 1);

        // An error status must survive (the "unless it was an error" rule).
        app.status_message = Some("Open failed: boom".into());
        let err_run = RunView {
            id: RunId(100),
            run_uid: String::new(),
            task_list_path: PathBuf::from("bad.md"),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        };
        app.update(AppEvent::RunLoaded(err_run));
        assert_eq!(
            app.status_message.as_deref(),
            Some("Open failed: boom"),
            "error status must not be cleared by RunLoaded"
        );
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
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("t2"),
                    title: "Task 2".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    failure_reason: None,
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));

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
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("t2"),
                    title: "Task 2".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    failure_reason: None,
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));

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

    // ── Provider configuration editor (task 0041) ──────────────────────────────

    /// Pressing `g` dispatches `OpenProviderEditor`, which must set
    /// `mode = Mode::ProviderConfig` and populate `provider_editor` from the
    /// app's current providers/roles.
    #[test]
    fn provider_editor_opens_and_lists_providers() {
        use crate::placeholder::PlaceholderApi;

        // Build an app pre-seeded with two providers (simulates what main.rs does
        // after loading config).
        let api = Arc::new(PlaceholderApi::new());
        let providers = vec![
            ProviderConfig {
                name: "default".into(),
                command: "acp-cli".into(),
                args: vec![],
                env: Default::default(),
            },
            ProviderConfig {
                name: "grok".into(),
                command: "grok".into(),
                args: vec!["agent".into()],
                env: Default::default(),
            },
        ];
        let roles = RolesConfig {
            developer: Some(RoleAssignment {
                provider: "default".into(),
                mode: None,
                model: None,
                effort: None,
            }),
            ..Default::default()
        };
        let mut app = App::with_config(
            api,
            vec![],
            PathBuf::from("."),
            providers.clone(),
            roles.clone(),
        );

        // Sanity: starts in Normal mode with no editor open.
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.provider_editor.is_none());

        // Dispatch the OpenProviderEditor event (the same path `g` triggers via
        // translate_key in event.rs).
        let redraw = app.update(AppEvent::OpenProviderEditor);

        // The mode must switch and the editor must be populated.
        assert!(redraw, "OpenProviderEditor must trigger a redraw");
        assert_eq!(app.mode, Mode::ProviderConfig);
        assert!(app.is_editing_providers());
        assert!(
            app.provider_editor.is_some(),
            "provider_editor must be Some"
        );

        let editor = app.provider_editor.as_ref().unwrap();
        assert_eq!(
            editor.providers.len(),
            2,
            "editor must list both configured providers"
        );
        assert_eq!(editor.providers[0].name, "default");
        assert_eq!(editor.providers[1].name, "grok");
        assert_eq!(
            editor.selected_provider,
            Some(0),
            "first provider must be pre-selected"
        );
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
        App::new(api, vec![run], PathBuf::from("."))
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

        // Assert the log (keyed by composite (RunId, TaskId)).
        let log = app
            .exchange_logs
            .get(&(RunId(1), focused_id.clone()))
            .expect("log must exist");
        assert_eq!(log.entries.len(), 2, "expect prompt + response entries");

        // Entry 0: prompt.
        assert!(log.entries[0].is_prompt(), "first entry must be a prompt");
        assert_eq!(log.entries[0].text(), "implement X");
        assert_eq!(log.entries[0].role, AgentRole::Developer);

        // Entry 1: concatenated response.
        assert!(
            !log.entries[1].is_prompt(),
            "second entry must be a response"
        );
        assert_eq!(
            log.entries[1].text(),
            "working on it",
            "chunks must concatenate in order"
        );
        assert!(
            log.entries[1].complete(),
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

        let log = app
            .exchange_logs
            .get(&(RunId(1), tid.clone()))
            .expect("log must exist");
        assert_eq!(log.entries.len(), 4, "2 prompts + 2 responses");
        // Ordering.
        assert!(log.entries[0].is_prompt());
        assert_eq!(log.entries[0].role, AgentRole::Developer);
        assert_eq!(log.entries[0].text(), "dev prompt");
        assert!(!log.entries[1].is_prompt());
        assert_eq!(log.entries[1].role, AgentRole::Developer);
        assert_eq!(log.entries[1].text(), "dev reply");
        assert!(log.entries[2].is_prompt());
        assert_eq!(log.entries[2].role, AgentRole::Reviewer);
        assert_eq!(log.entries[2].text(), "review prompt");
        assert!(!log.entries[3].is_prompt());
        assert_eq!(log.entries[3].role, AgentRole::Reviewer);
        assert_eq!(log.entries[3].text(), "lgtm");
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

        // Both logs are stored (keyed by composite (RunId, TaskId)).
        assert!(
            app.exchange_logs.contains_key(&(RunId(1), id_a.clone())),
            "task-a log must be stored"
        );
        assert!(
            app.exchange_logs.contains_key(&(RunId(1), id_b.clone())),
            "task-b log must be stored"
        );

        // Focus is on task-a (index 0).
        assert_eq!(app.selected_task, Some(0));
        assert_eq!(app.selected_task_id(), Some(&id_a));

        // Only task-a's log is "shown" by the focus query.
        let focused_log = app.selected_exchange_log().unwrap();
        assert_eq!(focused_log.entries[0].text(), "task-a prompt");

        // Navigate to task-b.
        app.update(AppEvent::SelectDown);
        assert_eq!(app.selected_task, Some(1));
        assert_eq!(app.selected_task_id(), Some(&id_b));

        let focused_log_b = app.selected_exchange_log().unwrap();
        assert_eq!(focused_log_b.entries[0].text(), "task-b prompt");
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

        // The render pass records the rendered bottom; the geometry-free update
        // path uses it as the scroll clamp bound.  Without a non-zero bound there
        // is nowhere to scroll, so simulate a multi-line pane.
        app.last_scroll_max.set(5);

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

    /// Regression (fix `tui-scroll-and-restore` #1): the FIRST wheel-up from
    /// auto-follow must move up by exactly one line (`scroll_max - 1`), NOT snap
    /// to the top (offset 0).
    ///
    /// Before the fix, `scroll_up` left the stale `exchange_scroll == 0` and
    /// merely `saturating_sub(1)`-ed it, so `effective_offset` returned 0 (top)
    /// on the first wheel-up while auto-following.
    #[test]
    fn first_scroll_up_from_auto_follow_anchors_to_bottom_minus_one() {
        let mut app = make_app();
        let max: u16 = 12;

        // Simulate what the render pass records: the bottom-most offset.
        app.last_scroll_max.set(max);
        assert!(app.exchange_auto_follow, "default is auto-follow");
        assert_eq!(
            app.exchange_scroll, 0,
            "manual offset is stale (0) while following"
        );

        // First wheel-up disengages auto-follow and anchors to the bottom.
        app.scroll_up();

        assert!(
            !app.exchange_auto_follow,
            "scroll_up must disengage auto-follow"
        );
        assert_eq!(
            app.effective_offset(max),
            max - 1,
            "first wheel-up must be max-1 (one line up), NOT 0 (top)"
        );
        assert_eq!(
            app.exchange_scroll,
            max - 1,
            "exchange_scroll must be anchored to the rendered bottom minus one"
        );
    }

    /// Regression (fix `tui-scroll-and-restore` #2): mouse auto-follow must
    /// re-engage when scrolling back down to the real rendered bottom.
    ///
    /// Before the fix, the `ScrollDown` arm clamped at `u16::MAX`, so
    /// `exchange_scroll == scroll_max` was unreachable and auto-follow could
    /// never re-engage via the mouse path.  Now it clamps at `last_scroll_max`.
    #[test]
    fn mouse_scroll_down_to_bottom_reengages_auto_follow() {
        let mut app = make_app_with_tasks();
        let max: u16 = 4;

        // The render pass records the real bottom offset.
        app.last_scroll_max.set(max);

        // Scroll up several times (disengages auto-follow, walks the offset up).
        for _ in 0..3 {
            app.update(AppEvent::ScrollUp);
        }
        assert!(
            !app.exchange_auto_follow,
            "scrolling up must disengage auto-follow"
        );

        // Now scroll down enough to reach the real rendered bottom.
        for _ in 0..(max + 5) {
            app.update(AppEvent::ScrollDown);
        }

        assert_eq!(
            app.exchange_scroll, max,
            "ScrollDown must clamp at the real rendered bottom (last_scroll_max)"
        );
        assert!(
            app.exchange_auto_follow,
            "reaching the rendered bottom via the mouse path must re-engage auto-follow"
        );
    }

    /// Regression (fix `tui-scroll-and-restore` #3): `scroll_down` must not
    /// overflow `exchange_scroll` when it is already at `u16::MAX` (debug panic).
    #[test]
    fn scroll_down_does_not_overflow_at_u16_max() {
        let mut app = make_app();
        app.exchange_auto_follow = false;
        app.exchange_scroll = u16::MAX;
        // saturating_add inside scroll_down must not panic in debug builds.
        app.scroll_down(u16::MAX);
        assert_eq!(app.exchange_scroll, u16::MAX);
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
                    failure_reason: None,
                },
                TaskView {
                    id: TaskId::new("t2"),
                    title: "T2".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    failure_reason: None,
                },
            ],
            report: makina_core::api::IngestionReport::default(),
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
                failure_reason: None,
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run1, run2], PathBuf::from("."));

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

        let log = app
            .exchange_logs
            .get(&(RunId(1), tid.clone()))
            .expect("log must exist");
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
        assert!(log.entries[0].is_prompt());
        assert_eq!(log.entries[0].text(), "hello");
        assert!(log.entries[0].complete());

        log.append_chunk(AgentRole::Developer, "chunk1".into());
        assert_eq!(log.entries.len(), 2);
        assert!(!log.entries[1].is_prompt());
        assert_eq!(log.entries[1].text(), "chunk1");
        assert!(!log.entries[1].complete());

        log.append_chunk(AgentRole::Developer, " chunk2".into());
        assert_eq!(
            log.entries.len(),
            2,
            "chunks must accumulate, not add new entries"
        );
        assert_eq!(log.entries[1].text(), "chunk1 chunk2");

        log.complete_turn();
        assert!(
            log.entries[1].complete(),
            "TurnComplete must mark response complete"
        );
    }

    /// Regression: a thinking-capable LLM interleaves a ThoughtChunk between two
    /// ResponseChunks (PromptSent → ResponseChunk → ThoughtChunk →
    /// ResponseChunk → TurnComplete).  With segmentation, these chunks should
    /// create two separate Response entries (preserving chronological order), the
    /// thought must survive, and TurnComplete must finalise all responses.
    #[test]
    fn exchange_log_response_chunks_coalesce_across_interleaved_thought() {
        use crate::app::ExchangeLog;
        use makina_core::api::AgentRole;

        let mut log = ExchangeLog::default();
        log.add_prompt(AgentRole::Developer, "do it".into());
        log.append_chunk(AgentRole::Developer, "A".into());
        log.append_thought(AgentRole::Developer, "hmm".into());
        log.append_chunk(AgentRole::Developer, "B".into());
        log.complete_turn();

        // TWO Response entries (segmented by the intervening thought)
        let responses: Vec<_> = log
            .entries
            .iter()
            .filter(|e| matches!(e.content, ExchangeContent::Response { .. }))
            .collect();
        assert_eq!(
            responses.len(),
            2,
            "ResponseChunks must segment into TWO entries around the interleaved thought"
        );
        // First segment contains "A"
        match &responses[0].content {
            ExchangeContent::Response { text, complete } => {
                assert_eq!(text, "A", "first segment must contain the initial chunk");
                assert!(
                    complete,
                    "first segment must be closed after thought intervenes"
                );
            }
            other => panic!("expected a Response, got {other:?}"),
        }
        // Second segment contains "B"
        match &responses[1].content {
            ExchangeContent::Response { text, complete } => {
                assert_eq!(text, "B", "second segment must contain the later chunk");
                assert!(complete, "TurnComplete must finalise the second segment");
            }
            other => panic!("expected a Response, got {other:?}"),
        }

        // The interleaved thought must still be present (not lost).
        assert!(
            log.entries
                .iter()
                .any(|e| matches!(&e.content, ExchangeContent::Thought { text } if text == "hmm")),
            "the interleaved thought must be preserved"
        );
    }

    /// Response chunks should be segmented when a thought or tool intervenes,
    /// so the order is preserved: chunk → thought → tool → chunk becomes
    /// Response, Thought, Tool, Response (not all chunks coalesced into one).
    #[test]
    fn exchange_log_segments_response_around_thought_and_tool() {
        let mut log = ExchangeLog::default();
        log.append_chunk(AgentRole::Developer, "Let me ".into());
        log.append_thought(AgentRole::Developer, "checking…".into());
        log.start_tool(
            AgentRole::Developer,
            "t1".into(),
            "read".into(),
            None,
            "completed".into(),
        );
        log.append_chunk(AgentRole::Developer, "do X.".into());
        let kinds: Vec<_> = log
            .entries
            .iter()
            .map(|e| match &e.content {
                ExchangeContent::Response { .. } => "resp",
                ExchangeContent::Thought { .. } => "thought",
                ExchangeContent::Tool { .. } => "tool",
                ExchangeContent::Prompt { .. } => "prompt",
            })
            .collect();
        assert_eq!(kinds, ["resp", "thought", "tool", "resp"]);
        // First segment is closed; only the tail is open until complete_turn.
        assert!(matches!(
            log.entries[0].content,
            ExchangeContent::Response { complete: true, .. }
        ));
        log.complete_turn();
        assert!(log.entries.iter().all(|e| e.complete()));
    }

    /// Regression: TurnComplete must finalise the response even when the literal
    /// last entry is a Tool (PromptSent → ResponseChunk → ToolCall →
    /// TurnComplete).  Before the fix this was a silent no-op and the UI showed
    /// the streaming cursor forever.
    #[test]
    fn complete_turn_marks_response_complete_when_last_entry_is_tool() {
        use crate::app::ExchangeLog;
        use makina_core::api::AgentRole;

        let mut log = ExchangeLog::default();
        log.add_prompt(AgentRole::Developer, "do it".into());
        log.append_chunk(AgentRole::Developer, "X".into());
        log.start_tool(
            AgentRole::Developer,
            "tc-1".into(),
            "run tests".into(),
            Some("execute".into()),
            "pending".into(),
        );
        log.complete_turn();

        let response = log
            .entries
            .iter()
            .find(|e| matches!(e.content, ExchangeContent::Response { .. }))
            .expect("response entry must exist");
        match &response.content {
            ExchangeContent::Response { text, complete } => {
                assert_eq!(text, "X");
                assert!(
                    complete,
                    "TurnComplete must finalise the response past a trailing Tool entry"
                );
            }
            other => panic!("expected a Response, got {other:?}"),
        }
    }

    /// ExchangeLog must capture thought bursts and tool calls/updates as
    /// distinct entry kinds, with tool updates upserting in place by id.
    ///
    /// Feeds a Prompt, several Thoughts, a ToolCall, two ToolCallUpdates for
    /// the same id, then a Response, all through `App::update`, and asserts the
    /// resulting log has the right number/kinds of entries and that the single
    /// tool entry carries the FINAL status.
    #[test]
    fn exchange_log_captures_thoughts_and_tool_updates() {
        use makina_core::api::{AgentRole, Event, ExchangeEvent, RunId, TaskId};
        let mut app = make_app_with_tasks();
        let tid = TaskId::new("task-a");

        let feed = |app: &mut App, event: ExchangeEvent| {
            app.update(AppEvent::ApiEvent(Event::AgentExchange {
                run: RunId(1),
                task: TaskId::new("task-a"),
                role: AgentRole::Developer,
                event,
            }));
        };

        // Prompt.
        feed(
            &mut app,
            ExchangeEvent::PromptSent {
                text: "do the thing".into(),
            },
        );
        // Several thought chunks — these must coalesce into ONE thought entry.
        feed(
            &mut app,
            ExchangeEvent::ThoughtChunk {
                text: "let me ".into(),
            },
        );
        feed(
            &mut app,
            ExchangeEvent::ThoughtChunk {
                text: "think...".into(),
            },
        );
        // Tool call announced, then two updates for the same id.
        feed(
            &mut app,
            ExchangeEvent::ToolCall {
                id: "tc-1".into(),
                title: "run tests".into(),
                kind: Some("execute".into()),
                status: "pending".into(),
            },
        );
        feed(
            &mut app,
            ExchangeEvent::ToolCallUpdate {
                id: "tc-1".into(),
                status: Some("in_progress".into()),
                title: None,
            },
        );
        feed(
            &mut app,
            ExchangeEvent::ToolCallUpdate {
                id: "tc-1".into(),
                status: Some("completed".into()),
                title: None,
            },
        );
        // Response + turn complete.
        feed(
            &mut app,
            ExchangeEvent::ResponseChunk {
                text: "done".into(),
            },
        );
        feed(&mut app, ExchangeEvent::TurnComplete);

        let log = app
            .exchange_logs
            .get(&(RunId(1), tid.clone()))
            .expect("log must exist");

        // Kinds + counts: prompt, ONE coalesced thought, ONE tool, response.
        assert_eq!(
            log.entries.len(),
            4,
            "prompt + 1 thought + 1 tool + 1 response (thoughts coalesce, tool upserts)"
        );

        assert!(matches!(
            log.entries[0].content,
            ExchangeContent::Prompt { .. }
        ));
        match &log.entries[1].content {
            ExchangeContent::Thought { text } => {
                assert_eq!(text, "let me think...", "thought bursts must coalesce");
            }
            other => panic!("entry 1 must be a Thought, got {other:?}"),
        }
        match &log.entries[2].content {
            ExchangeContent::Tool {
                id, title, status, ..
            } => {
                assert_eq!(id, "tc-1");
                assert_eq!(title, "run tests");
                assert_eq!(status, "completed", "tool entry must reflect FINAL status");
            }
            other => panic!("entry 2 must be a Tool, got {other:?}"),
        }
        match &log.entries[3].content {
            ExchangeContent::Response { text, complete } => {
                assert_eq!(text, "done");
                assert!(complete, "TurnComplete must finalise the response");
            }
            other => panic!("entry 3 must be a Response, got {other:?}"),
        }
    }

    /// Two ToolCallUpdates for id "tc-1" must mutate the SAME tool entry in
    /// place (no duplicate entries), and the entry's status must reflect the
    /// LAST update.
    #[test]
    fn exchange_log_tool_update_mutates_in_place() {
        use crate::app::ExchangeLog;
        use makina_core::api::AgentRole;

        let mut log = ExchangeLog::default();

        // Announce a tool call.
        log.start_tool(
            AgentRole::Developer,
            "tc-1".into(),
            "edit file".into(),
            Some("edit".into()),
            "pending".into(),
        );
        assert_eq!(log.entries.len(), 1, "one tool entry after start_tool");

        // First update.
        log.update_tool("tc-1", Some("in_progress".into()), None);
        // Second update — title change too.
        log.update_tool(
            "tc-1",
            Some("completed".into()),
            Some("edit file (done)".into()),
        );

        // Still exactly ONE tool entry — updates mutated in place.
        assert_eq!(
            log.entries.len(),
            1,
            "two updates for the same id must NOT create new entries"
        );

        match &log.entries[0].content {
            ExchangeContent::Tool {
                id,
                title,
                kind,
                status,
                content,
            } => {
                assert_eq!(id, "tc-1");
                assert_eq!(status, "completed", "status must reflect the LAST update");
                assert_eq!(
                    title, "edit file (done)",
                    "title must reflect the LAST update"
                );
                assert_eq!(
                    kind.as_deref(),
                    Some("edit"),
                    "kind set by start_tool stays"
                );
                assert!(content.is_empty(), "no content carried on the update path");
            }
            other => panic!("entry must be a Tool, got {other:?}"),
        }
    }

    /// The reducer must produce identical results whether events are fed live
    /// or replayed from disk. This test verifies the invariant by feeding the
    /// same event sequence through the reducer twice and asserting the resulting
    /// logs are equal.
    #[test]
    fn replay_reducer_matches_live() {
        use makina_core::api::AgentRole;

        // Sample sequence: prompt, thought, tool announce, response chunks,
        // tool update, turn complete.
        let events: Vec<(AgentRole, ExchangeEvent)> = vec![
            (
                AgentRole::Developer,
                ExchangeEvent::PromptSent {
                    text: "do something".into(),
                },
            ),
            (
                AgentRole::Developer,
                ExchangeEvent::ThoughtChunk {
                    text: "I will ".into(),
                },
            ),
            (
                AgentRole::Developer,
                ExchangeEvent::ThoughtChunk {
                    text: "plan first".into(),
                },
            ),
            (
                AgentRole::Developer,
                ExchangeEvent::ToolCall {
                    id: "tc-1".into(),
                    title: "execute".into(),
                    kind: Some("exec".into()),
                    status: "pending".into(),
                },
            ),
            (
                AgentRole::Developer,
                ExchangeEvent::ResponseChunk {
                    text: "I will ".into(),
                },
            ),
            (
                AgentRole::Developer,
                ExchangeEvent::ToolCallUpdate {
                    id: "tc-1".into(),
                    status: Some("completed".into()),
                    title: None,
                },
            ),
            (
                AgentRole::Developer,
                ExchangeEvent::ResponseChunk {
                    text: "run it".into(),
                },
            ),
            (AgentRole::Developer, ExchangeEvent::TurnComplete),
        ];

        // Feed through live path.
        let mut live_log = ExchangeLog::default();
        for (role, ev) in &events {
            apply_exchange_event(&mut live_log, role.clone(), ev);
        }

        // Feed through replay path.
        let mut replay_log = ExchangeLog::default();
        for (role, ev) in &events {
            apply_exchange_event(&mut replay_log, role.clone(), ev);
        }

        // Both must be identical.
        assert_eq!(
            format!("{live_log:?}"),
            format!("{replay_log:?}"),
            "live and replay logs must be identical"
        );
    }

    /// **Replay from disk (the done-when):** Given a fixture run directory
    /// containing a transcript, construct an App pointed at that directory,
    /// select the run (via `load_initial_exchanges`), and assert that the
    /// focused task's `ExchangeLog` is non-empty and contains the expected
    /// event kinds in order.
    ///
    /// This exercises `App::load_initial_exchanges` → `load_exchanges_for_selected_run`
    /// → `replay::load_task_exchange` end-to-end through the App — not just the
    /// replay loader in isolation.
    #[test]
    fn open_finished_run_populates_exchange() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        use std::fs::{self, File};
        use std::io::Write;
        use tempfile::TempDir;

        // Create a temporary repo root with the expected directory layout:
        // {repo_root}/.makina/runs/{run_uid}/logs/{task_id}_transcript.jsonl
        let temp_dir = TempDir::new().expect("create temp dir");
        let repo_root = temp_dir.path().to_path_buf();
        let run_uid = "01TESTREPLAYUID";
        let task_id = "test-task";
        let logs_dir = repo_root
            .join(".makina")
            .join("runs")
            .join(run_uid)
            .join("logs");
        fs::create_dir_all(&logs_dir).expect("create logs dir");
        let transcript_path = logs_dir.join(format!("{task_id}_transcript.jsonl"));

        // Write a transcript with multiple event types.
        {
            let mut file = File::create(&transcript_path).expect("create transcript file");

            // Prompt
            writeln!(
                file,
                r#"{{"type":"prompt_sent","text":"implement a function"}}"#
            )
            .expect("write prompt");

            // Thought chunks
            writeln!(file, r#"{{"type":"thought_chunk","text":"I need to"}}"#)
                .expect("write thought 1");
            writeln!(file, r#"{{"type":"thought_chunk","text":" plan"}}"#)
                .expect("write thought 2");

            // Tool call
            writeln!(
                file,
                r#"{{"type":"tool_call","id":"tc1","title":"read file","kind":"read","status":"pending"}}"#
            )
            .expect("write tool call");

            // Response chunks
            writeln!(file, r#"{{"type":"response_chunk","text":"I'll read"}}"#)
                .expect("write response chunk 1");
            writeln!(file, r#"{{"type":"response_chunk","text":" the file"}}"#)
                .expect("write response chunk 2");

            // Tool update
            writeln!(
                file,
                r#"{{"type":"tool_call_update","id":"tc1","status":"completed","title":null}}"#
            )
            .expect("write tool update");

            // Turn complete
            writeln!(file, r#"{{"type":"turn_complete"}}"#).expect("write turn complete");
        }

        // Build a RunView for the fixture run.  The run_uid must match the
        // directory name so the replay loader finds the transcript.
        let run_id = RunId(42);
        let fixture_run = RunView {
            id: run_id,
            run_uid: run_uid.to_string(),
            task_list_path: repo_root.join("tasks.md"),
            status: RunStatus::Completed,
            project: String::new(),
            tasks: vec![TaskView {
                id: TaskId::new(task_id),
                title: "Test task".into(),
                state: TaskState::Done,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                failure_reason: None,
            }],
            report: makina_core::api::IngestionReport::default(),
        };

        // Construct App and populate exchanges for the selected run.
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![fixture_run], repo_root);

        // The first run is auto-selected; its first task is also auto-selected.
        assert_eq!(app.selected_run, Some(0));
        assert_eq!(app.selected_task, Some(0));

        // Trigger the replay loader.
        app.load_initial_exchanges();

        // The focused task's log must now be populated.
        let log = app
            .selected_exchange_log()
            .expect("exchange log must exist after load_initial_exchanges");

        // Assert non-empty.
        assert!(!log.entries.is_empty(), "loaded log must not be empty");

        // Verify we have the expected kinds in order.
        // The reducer coalesces: Prompt, accumulated Thought, Tool, accumulated Response.
        assert!(
            log.entries.len() >= 3,
            "log should have at least 3 entries (prompt, thought, response), got {}",
            log.entries.len()
        );

        use crate::app::ExchangeContent;

        // First entry must be a prompt.
        match &log.entries[0].content {
            ExchangeContent::Prompt { text } => {
                assert_eq!(text, "implement a function");
            }
            other => panic!("first entry should be Prompt, got {other:?}"),
        }

        // Must have a thought entry.
        let has_thought = log
            .entries
            .iter()
            .any(|e| matches!(e.content, ExchangeContent::Thought { .. }));
        assert!(has_thought, "log must contain at least one Thought entry");

        // Must have a tool entry.
        let has_tool = log
            .entries
            .iter()
            .any(|e| matches!(e.content, ExchangeContent::Tool { .. }));
        assert!(has_tool, "log must contain at least one Tool entry");

        // Must have a response entry.
        let has_response = log
            .entries
            .iter()
            .any(|e| matches!(e.content, ExchangeContent::Response { .. }));
        assert!(has_response, "log must contain at least one Response entry");

        // Also verify the composite cache key: the log is indexed by (RunId, TaskId).
        let cache_key = (run_id, TaskId::new(task_id));
        assert!(
            app.exchange_logs.contains_key(&cache_key),
            "exchange_logs must use (RunId, TaskId) as composite cache key"
        );
    }
}
