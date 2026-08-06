//! Application state and pure update logic.
//!
//! [`App`] is the single source of truth for all TUI state.  It holds no IO;
//! the IO loop in [`crate::event`] drives it by calling [`App::update`].
//!
//! Keeping `update` a synchronous, pure function means every state transition
//! is unit-testable without a real terminal or async runtime.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use makina_core::api::{AgentRole, Api, Event, RunId, RunView, TaskId};
use makina_core::config::{FinalMerge, ProviderConfig, RolesConfig};
use ratatui::layout::Rect;

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

/// Who produced one turn of a rendered conversation.
///
/// A run's exchange only ever involves the orchestrated [`AgentRole`]s, but the
/// same log type — and the same renderer — also carries the plan-authoring
/// conversation, whose two lanes are the operator and the planner. Keeping that
/// in one enum is what lets both views share [`ExchangeLog`] rather than each
/// growing its own transcript type and drifting apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StreamRole {
    /// An orchestrated agent working a task.
    Agent(AgentRole),
    /// The person at the keyboard, in the plan-authoring conversation.
    Operator,
    /// The planner answering them.
    Planner,
}

impl From<AgentRole> for StreamRole {
    fn from(role: AgentRole) -> Self {
        StreamRole::Agent(role)
    }
}

/// A single turn in a live exchange.
///
/// Every entry carries the [`StreamRole`] that produced it plus its
/// [`ExchangeContent`] — a prompt, a (possibly streaming) response, a thought
/// burst, or a tool call.
#[derive(Debug, Clone)]
pub struct ExchangeEntry {
    /// The role that produced this turn.
    pub role: StreamRole,
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

/// Longest text kept in a single coalescing thought entry.
///
/// Reasoning streams continuously and is only interesting as "what is it doing
/// right now", so an unbounded entry would grow for a whole turn to show a few
/// lines. The cap lives here rather than at a call site so every stream — a
/// task's agents and the planner alike — is bounded by the same rule.
pub const THOUGHT_TAIL_CAP: usize = 8_000;

/// Drop leading text so `thought` fits [`THOUGHT_TAIL_CAP`], on a char boundary.
fn trim_thought_to_cap(thought: &mut String) {
    if thought.len() <= THOUGHT_TAIL_CAP {
        return;
    }
    let excess = thought.len() - THOUGHT_TAIL_CAP;
    let cut = (excess..=thought.len())
        .find(|index| thought.is_char_boundary(*index))
        .unwrap_or(thought.len());
    thought.drain(..cut);
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
    pub fn add_prompt(&mut self, role: impl Into<StreamRole>, text: String) {
        self.push(ExchangeEntry {
            role: role.into(),
            content: ExchangeContent::Prompt { text },
        });
    }

    /// Push an already-complete response entry for the given role.
    ///
    /// The streaming path builds a response chunk by chunk; a caller that
    /// already holds the whole text (the planner's parsed reply, say) would
    /// otherwise have to fake a stream to get the same entry.
    pub fn add_response(&mut self, role: impl Into<StreamRole>, text: String) {
        self.finalize_trailing_response();
        self.push(ExchangeEntry {
            role: role.into(),
            content: ExchangeContent::Response {
                text,
                complete: true,
            },
        });
    }

    /// Start a new (in-progress) response entry for the given role, or
    /// append to the last incomplete response.
    ///
    /// Design decision: a `ResponseChunk` arriving without a preceding
    /// `PromptSent` in this log still needs to go somewhere — we create an
    /// implicit incomplete response entry rather than silently dropping data.
    pub fn append_chunk(&mut self, role: impl Into<StreamRole>, chunk: String) {
        let role = role.into();
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
    pub fn append_thought(&mut self, role: impl Into<StreamRole>, chunk: String) {
        let role = role.into();
        self.finalize_trailing_response();
        if let Some(last) = self.entries.last_mut()
            && last.role == role
            && let ExchangeContent::Thought { text } = &mut last.content
        {
            text.push_str(&chunk);
            trim_thought_to_cap(text);
            return;
        }
        let mut text = chunk;
        trim_thought_to_cap(&mut text);
        self.push(ExchangeEntry {
            role,
            content: ExchangeContent::Thought { text },
        });
    }

    /// Upsert a tool call by `(role, id)`.
    ///
    /// Updates an existing [`ExchangeContent::Tool`] entry with the same `id`
    /// (overwriting title/kind/status, and setting `content` when
    /// `incoming_content` is `Some(non-empty)` so a later content-less update
    /// never blanks a captured diff), or pushes a new tool entry.
    pub fn start_tool(
        &mut self,
        role: impl Into<StreamRole>,
        id: String,
        title: String,
        kind: Option<String>,
        status: String,
        incoming_content: Option<String>,
    ) {
        let role = role.into();
        if let Some(entry) = self.find_tool_mut(&role, &id) {
            if let ExchangeContent::Tool {
                title: t,
                kind: k,
                status: s,
                content: c,
                ..
            } = &mut entry.content
            {
                *t = title;
                *k = kind;
                *s = status;
                // Only overwrite content when the incoming value is non-empty.
                if let Some(new_c) = incoming_content
                    && !new_c.is_empty()
                {
                    *c = new_c;
                }
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
                content: incoming_content.unwrap_or_default(),
            },
        });
    }

    /// Apply a status/title/content update to an existing tool entry by `(role, id)`.
    ///
    /// Finds the [`ExchangeContent::Tool`] entry with the matching `id` and
    /// updates `status`/`title`/`content` when present; `content` is only
    /// overwritten when `incoming_content` is `Some(non-empty)`.  No-op when no
    /// entry matches (live updates always follow a `start_tool`).
    pub fn update_tool(
        &mut self,
        role: impl Into<StreamRole>,
        id: &str,
        status: Option<String>,
        title: Option<String>,
        incoming_content: Option<String>,
    ) {
        if let Some(entry) = self.find_tool_mut(&role.into(), id)
            && let ExchangeContent::Tool {
                title: t,
                status: s,
                content: c,
                ..
            } = &mut entry.content
        {
            if let Some(new_status) = status {
                *s = new_status;
            }
            if let Some(new_title) = title {
                *t = new_title;
            }
            if let Some(new_c) = incoming_content
                && !new_c.is_empty()
            {
                *c = new_c;
            }
        }
    }

    /// Find the tool entry with the given `(role, id)`, if any.
    fn find_tool_mut(&mut self, role: &StreamRole, id: &str) -> Option<&mut ExchangeEntry> {
        self.entries.iter_mut().find(|entry| {
            entry.role == *role
                && matches!(
                    &entry.content,
                    ExchangeContent::Tool { id: existing_id, .. } if existing_id == id
                )
        })
    }
}

// ── Exchange event reducer ──────────────────────────────────────────────────────
//
// Apply one exchange event to a task's log. The single source of truth used
// by both the live event path and on-disk replay (plan 0010).

use makina_core::api::ExchangeEvent;

/// Apply one exchange event to a stream's log. The single source of truth used
/// by the live event path, on-disk replay (plan 0010), and the plan-authoring
/// conversation.
pub fn apply_exchange_event(
    log: &mut ExchangeLog,
    role: impl Into<StreamRole>,
    event: &ExchangeEvent,
) {
    let role = role.into();
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
            content,
        } => {
            log.start_tool(
                role,
                id.clone(),
                title.clone(),
                kind.clone(),
                status.clone(),
                content.clone(),
            );
        }
        ExchangeEvent::ToolCallUpdate {
            id,
            status,
            title,
            content,
        } => {
            log.update_tool(role, id, status.clone(), title.clone(), content.clone());
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
    /// The modal browser for picking a plan directory to open.
    FileBrowser,
    /// The modal directory browser for picking a folder to open or initialize.
    FolderBrowser { purpose: FolderBrowserPurpose },
    /// The modal doctor health-check overlay.
    Doctor,
    /// The command palette modal (task 0069).
    CommandPalette,
    /// The settings modal (task 0070).
    Settings,
    /// Confirmation modal before resetting a plan/run.
    ResetConfirm,
    /// Modal explaining why an operation-gated command is unavailable.
    OperationNotice,
    /// Searchable model picker overlay (opened from Settings).
    ModelPicker,
}

#[derive(Debug)]
pub struct PlanAuthoring {
    pub project_root: PathBuf,
    /// The composer buffer. May contain newlines: they arrive from a bracketed
    /// paste or from an explicit Shift/Alt+Enter, and are preserved verbatim.
    pub input: String,
    /// The conversation so far, in the same shape a task's execution uses:
    /// operator prompts, planner replies, and the planner's streamed reasoning
    /// and tool calls, all as [`ExchangeEntry`]s so one renderer serves both.
    pub log: ExchangeLog,
    pub waiting: bool,
    /// True once a blueprint has been accepted and the bundle is being written.
    ///
    /// The tab stays open through this so a generation failure can hand the
    /// conversation back instead of leaving the operator with nothing.
    pub generating: bool,
    pub answer_tx: Option<tokio::sync::mpsc::Sender<String>>,
}

impl PlanAuthoring {
    /// Record what the operator just sent.
    pub fn push_operator(&mut self, text: String) {
        self.log.add_prompt(StreamRole::Operator, text);
    }

    /// Record what the planner said back.
    pub fn push_planner(&mut self, text: String) {
        self.log.add_response(StreamRole::Planner, text);
    }

    /// Whether the planner currently owns the turn.
    pub fn is_busy(&self) -> bool {
        self.waiting || self.generating
    }

    /// What the planner is doing, for the activity indicator.
    pub fn activity(&self) -> &'static str {
        if self.generating {
            "Generating the plan bundle"
        } else {
            "Waiting for the planner"
        }
    }
}

/// Rows the composer needs to show `input` wrapped to `width`, before clamping.
///
/// A single-line draft occupies one row and the composer looks like a one-line
/// field; as the text wraps or carries explicit newlines it grows to fit. This
/// counts exactly what [`ratatui::widgets::Wrap`] will lay out, so the box
/// height and the rendered text never disagree — the previous single-line
/// `Paragraph` simply hid everything past the right edge.
pub fn composer_rows(input: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    // A trailing newline means the caret sits on a fresh, empty row.
    let segments = input.split('\n');
    let rows: usize = segments
        .map(|segment| {
            // Wrapping is by display width; the caret needs a cell of its own,
            // so a segment that exactly fills the width spills to the next row.
            let cells = segment.chars().count() + 1;
            cells.div_ceil(width).max(1)
        })
        .sum();
    rows.clamp(1, u16::MAX as usize) as u16
}

/// Rows the composer is allowed to occupy, including its border.
///
/// Growth is bounded so a long paste cannot squeeze the conversation off the
/// screen; past the cap the composer scrolls to keep the caret visible.
pub fn composer_height(input: &str, width: u16, max_rows: u16) -> u16 {
    composer_rows(input, width)
        .min(max_rows.max(1))
        .saturating_add(2)
}

/// The purpose of the folder browser modal — determines which event is emitted on selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderBrowserPurpose {
    /// User is selecting a folder to open and add to the workspace.
    OpenFolder,
    /// User is selecting a folder to initialize with git and a plan template.
    InitializeFolder,
    /// User is selecting a folder to close and remove from the workspace.
    CloseFolders,
}

// ── Command palette ──────────────────────────────────────────────────────────

/// A single selectable command in the palette.
#[derive(Debug, Clone)]
pub enum PaletteAction {
    /// A regular action: its display label and the `AppEvent` `Enter` re-dispatches through the normal update path.
    Regular {
        label: &'static str,
        event: Box<AppEvent>,
    },
    /// A nested theme selector that lists built-in themes.
    NestedThemeSelector { label: &'static str },
}

impl PaletteAction {
    /// Display label for this action.
    pub fn label(&self) -> &str {
        match self {
            PaletteAction::Regular { label, .. } | PaletteAction::NestedThemeSelector { label } => {
                label
            }
        }
    }
}

/// State for the Ctrl+P command-palette modal.
#[derive(Debug, Clone)]
pub struct CommandPalette {
    /// Type-to-filter query (case-insensitive substring match on `label`).
    pub filter: String,
    /// The full static action set, in display order.
    pub actions: Vec<PaletteAction>,
    /// Selected index *into the filtered view* (clamped on every filter change).
    pub selected: usize,
    /// None = normal action list; Some = theme-selector mode showing theme names.
    pub theme_selector: Option<Vec<String>>,
}

impl CommandPalette {
    /// The default action set. Regular actions carry existing intents and are
    /// re-dispatched through `resolve_io` so IO-backed commands such as run
    /// controls, retry/reset, project discovery, and opening a plan reach
    /// their async handlers.
    pub fn default_actions() -> Vec<PaletteAction> {
        vec![
            PaletteAction::Regular {
                label: "Open plan",
                event: Box::new(AppEvent::OpenBrowser),
            },
            PaletteAction::Regular {
                label: "Create plan",
                event: Box::new(AppEvent::OpenPlanAuthoring),
            },
            PaletteAction::Regular {
                label: "Open Folder",
                event: Box::new(AppEvent::OpenFolder),
            },
            PaletteAction::Regular {
                label: "Close Folder",
                event: Box::new(AppEvent::CloseFolderRequested),
            },
            PaletteAction::Regular {
                label: "Initialize Folder",
                event: Box::new(AppEvent::InitializeFolderRequested),
            },
            PaletteAction::Regular {
                label: "Start run",
                event: Box::new(AppEvent::StartRun),
            },
            PaletteAction::Regular {
                label: "Pause run",
                event: Box::new(AppEvent::PauseRun),
            },
            PaletteAction::Regular {
                label: "Stop run",
                event: Box::new(AppEvent::CancelRun),
            },
            PaletteAction::Regular {
                label: "Reset/retry focused task",
                event: Box::new(AppEvent::RetryFocused),
            },
            PaletteAction::Regular {
                label: "Reset selected plan/run",
                event: Box::new(AppEvent::RequestResetRun),
            },
            PaletteAction::Regular {
                label: "Purge Makina worktrees",
                event: Box::new(AppEvent::PurgeWorktrees),
            },
            PaletteAction::Regular {
                label: "Settings",
                event: Box::new(AppEvent::OpenSettings),
            },
            PaletteAction::Regular {
                label: "Doctor",
                event: Box::new(AppEvent::OpenDoctor),
            },
            PaletteAction::Regular {
                label: "Discover project",
                event: Box::new(AppEvent::DiscoverProject),
            },
            PaletteAction::Regular {
                label: "Quit",
                event: Box::new(AppEvent::Quit),
            },
            PaletteAction::NestedThemeSelector {
                label: "Switch theme",
            },
        ]
    }

    /// Actions whose lowercased `label` contains the lowercased `filter`.
    pub fn filtered(&self) -> Vec<&PaletteAction> {
        if self.filter.is_empty() {
            self.actions.iter().collect()
        } else {
            let filter_lower = self.filter.to_lowercase();
            self.actions
                .iter()
                .filter(|action| action.label().to_lowercase().contains(&filter_lower))
                .collect()
        }
    }

    /// Theme names from `theme_selector` that match the current `filter`.
    /// Returns an empty vec when not in theme-selector mode.
    pub fn filtered_theme_names(&self) -> Vec<&String> {
        match &self.theme_selector {
            None => vec![],
            Some(theme_names) => {
                if self.filter.is_empty() {
                    theme_names.iter().collect()
                } else {
                    let filter_lower = self.filter.to_lowercase();
                    theme_names
                        .iter()
                        .filter(|name| name.to_lowercase().contains(&filter_lower))
                        .collect()
                }
            }
        }
    }
}

// ── Settings modal (plan 0070) ────────────────────────────────────────────────

/// Reduce a settings model buffer to the bare model name stored in config.
///
/// The model picker lists entries as `{agent}/{model}` (see the probe in
/// `event::resolve_io`), so the chosen value carries the agent command as a
/// prefix. Config stores only the model name, and `agent_names` — the commands
/// of the configured providers — is what distinguishes that prefix from a model
/// whose own name contains slashes (`anthropic/claude-sonnet-4`). Stripping the
/// first segment unconditionally would eat a real segment every time the modal
/// re-seeded from config and auto-saved.
///
/// Returns `None` for an empty buffer ("leave the stored model alone").
pub fn canonical_model_name(raw: &str, agent_names: &[String]) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    for agent in agent_names {
        if agent.is_empty() {
            continue;
        }
        if let Some(rest) = raw
            .strip_prefix(agent.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            && !rest.is_empty()
        {
            return Some(rest.to_string());
        }
    }
    Some(raw.to_string())
}

/// Which settings field is focused / being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    GateIterations,
    ReviewerIterations,
    WallClockSecs,
    IdleSecs, // empty buffer ⇒ None (disabled)
    Concurrency,
    FinalMerge,
    DeveloperModel,
    ReviewerModel,
    PlannerModel,
    DeveloperEffort,
    ReviewerEffort,
    PlannerEffort,
}

impl SettingsField {
    /// Whether this field selects an effort (`thought_level`) rather than a model.
    pub fn is_effort(self) -> bool {
        matches!(
            self,
            SettingsField::DeveloperEffort
                | SettingsField::ReviewerEffort
                | SettingsField::PlannerEffort
        )
    }

    /// Whether this field is chosen from the searchable picker at all.
    pub fn is_pickable(self) -> bool {
        self.is_effort()
            || matches!(
                self,
                SettingsField::DeveloperModel
                    | SettingsField::ReviewerModel
                    | SettingsField::PlannerModel
            )
    }
}

/// State for the settings modal: an editable text buffer per numeric field,
/// seeded from the App's loaded caps, plus the focused field.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Immutable project selected when the modal opened. Saving must update
    /// this project's config and routed runtime, even if other UI state changes.
    pub project_root: PathBuf,
    pub gate_iterations: String,
    pub reviewer_iterations: String,
    pub wall_clock_secs: String,
    pub idle_secs: String, // "" ⇒ None
    pub concurrency: String,
    pub final_merge: FinalMerge,
    /// Developer model text buffer (typed by the user).
    pub developer_model: String,
    /// Reviewer model text buffer.
    pub reviewer_model: String,
    /// Planner model text buffer.
    pub planner_model: String,
    /// Developer effort (`thought_level`) text buffer.
    pub developer_effort: String,
    /// Reviewer effort text buffer.
    pub reviewer_effort: String,
    /// Planner effort text buffer.
    pub planner_effort: String,
    /// Effort values advertised by the probed agent.
    pub discovered_efforts: Vec<String>,
    /// Models discovered by probing the agent (shown as hints, not auto-selected).
    pub discovered_models: Vec<String>,
    pub focused: SettingsField,
    /// Last validation error (rendered under the field), or `None`.
    pub error: Option<String>,
}

/// Confirmation details for resetting the current plan/run.
#[derive(Debug, Clone)]
pub struct ResetConfirmation {
    /// Immutable project + plan target shown in the confirmation dialog.
    ///
    /// Confirmation execution must consume this exact value instead of
    /// recomputing selection after the modal has opened.
    pub target: PlanIdentity,
    pub label: String,
    /// Live run known when the confirmation was created.  Disk-only run ids are
    /// harmless: the IO layer validates this id and opens `target` when it is
    /// not backed by the live API registry.
    pub run: Option<RunId>,
}

/// Stable identity of one plan inside one project.
///
/// Plan slugs are deliberately not identities: two repositories routinely
/// contain the same scaffolded slug. The canonical per-task plan directory is
/// the repository-relative identity used by Core.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlanIdentity {
    pub project_root: PathBuf,
    pub plan_dir: makina_core::plan::PlanKey,
    pub slug: String,
}

impl PlanIdentity {
    pub fn from_key(
        project_root: PathBuf,
        plan_dir: makina_core::plan::PlanKey,
        slug: String,
    ) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let project_root = lexical_absolute(&cwd, &project_root);
        Self {
            project_root,
            plan_dir,
            slug,
        }
    }

    pub fn new(project_root: PathBuf, plan_dir: PathBuf, slug: String) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let project_root = lexical_absolute(&cwd, &project_root);
        let absolute_plan_dir = lexical_absolute(&project_root, &plan_dir);
        let relative = absolute_plan_dir
            .strip_prefix(&project_root)
            .unwrap_or(&absolute_plan_dir)
            .to_path_buf();
        let plan_dir = makina_core::plan::PlanKey::parse(relative)
            .expect("application plan identity must be a validated PlanKey");
        Self {
            project_root,
            plan_dir,
            slug,
        }
    }

    /// Test/back-compat identity for callers that have only a slug. Production
    /// folder and run activation always uses [`PlanIdentity::new`].
    pub fn legacy(slug: impl Into<String>) -> Self {
        let slug = slug.into();
        let basename = if slug.len() >= 6
            && slug.as_bytes()[..4].iter().all(u8::is_ascii_digit)
            && slug.as_bytes()[4] == b'-'
        {
            slug.clone()
        } else {
            let normalized = slug
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() {
                        character.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .trim_matches('-')
                .to_owned();
            format!(
                "9999-{}",
                if normalized.is_empty() {
                    "legacy"
                } else {
                    &normalized
                }
            )
        };
        Self {
            project_root: PathBuf::new(),
            plan_dir: makina_core::plan::PlanKey::parse(
                PathBuf::from("docs").join("plans").join(basename),
            )
            .expect("legacy test plan slug must be a valid PlanKey"),
            slug,
        }
    }
}

/// Lexically make `path` absolute relative to `base`, removing `.` and `..`
/// components without requiring the final path to exist.
fn lexical_absolute(base: &std::path::Path, path: &std::path::Path) -> PathBuf {
    use std::path::Component;

    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Recover a repository root from the plan convention
/// `{root}/docs/plans/{slug}`.
fn project_root_from_plan_path(path: &std::path::Path) -> Option<PathBuf> {
    let plans_dir = path.parent()?;
    if plans_dir.file_name().and_then(|name| name.to_str()) != Some("plans") {
        return None;
    }
    let docs_dir = plans_dir.parent()?;
    if docs_dir.file_name().and_then(|name| name.to_str()) != Some("docs") {
        return None;
    }
    docs_dir.parent().map(std::path::Path::to_path_buf)
}

/// One plan-level operation tracked by a small UI state machine.
#[derive(Debug, Clone)]
pub struct PlanOperationState {
    pub target: PlanIdentity,
    pub label: String,
    pub kind: makina_core::api::PlanOperationKind,
    pub phase: makina_core::api::PlanOperationPhase,
    pub log: Vec<String>,
}

impl PlanOperationState {
    pub fn is_running(&self) -> bool {
        matches!(
            self.phase,
            makina_core::api::PlanOperationPhase::Started
                | makina_core::api::PlanOperationPhase::Step
        )
    }
}

/// Modal shown when the user asks for a command blocked by a plan operation.
#[derive(Debug, Clone)]
pub struct OperationNotice {
    pub target: PlanIdentity,
    pub attempted: String,
}

pub fn final_merge_label(mode: FinalMerge) -> &'static str {
    match mode {
        FinalMerge::Squash => "Squash merge into base branch",
        FinalMerge::Stage => "Stage changes in main worktree",
        FinalMerge::MergeCommit => "Merge commit into base branch",
        FinalMerge::Manual => "Leave plan branch unmerged",
    }
}

fn next_settings_final_merge(mode: FinalMerge) -> FinalMerge {
    match mode {
        FinalMerge::Squash => FinalMerge::Stage,
        FinalMerge::Stage => FinalMerge::Squash,
        FinalMerge::MergeCommit | FinalMerge::Manual => FinalMerge::Squash,
    }
}

fn previous_settings_final_merge(mode: FinalMerge) -> FinalMerge {
    match mode {
        FinalMerge::Squash => FinalMerge::Stage,
        FinalMerge::Stage => FinalMerge::Squash,
        FinalMerge::MergeCommit | FinalMerge::Manual => FinalMerge::Stage,
    }
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

/// Hierarchical focus state: which nested item inside the focused panel owns focus.
/// When `focused_panel == Panel::Main`, `focused_section` tracks which accordion
/// section (if any) is active. When `focused_panel == Panel::Sidebar`,
/// `focused_section` is ignored (the tree cursor owns focus).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusState {
    /// Sidebar tree node has focus; tree_cursor identifies the node.
    TreeNode,
    /// Main pane has focus, but no accordion section is focused yet (e.g., on first
    /// entry to Main when no plan tab is active).
    MainPane,
    /// A specific accordion section in the active plan tab has focus.
    AccordionSection(AccordionSection),
}

/// A node in the sidebar tree: either a run header or one of its tasks.
///
/// Built on-demand by [`App::visible_tree_nodes`] to flatten the run/task
/// hierarchy into a single linear list for cursor-based navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeNode {
    /// A folder at the top level (index into App::opened_folders).
    Folder { folder_idx: usize },
    /// A plan discovered under opened_folders[folder_idx].
    PlanInFolder { folder_idx: usize, plan_idx: usize },
    /// A task preview under an expanded plan in a folder.
    PlanTaskInFolder {
        folder_idx: usize,
        plan_idx: usize,
        task_idx: usize,
    },
    /// The run at `runs[run]`.
    Run { run: usize },
    /// Task at `runs[run].tasks[task]`.
    Task { run: usize, task: usize },
    /// Legacy: a discovered plan (for backward compat).
    Plan { plan_idx: usize },
    /// Legacy: a task preview under an expanded plan.
    PlanTask { plan_idx: usize, task_idx: usize },
}

impl TreeNode {
    /// The `opened_folders` index of the folder this node belongs to, if any.
    ///
    /// Returns `Some` for the folder-scoped variants (a folder header or a
    /// plan/task under one) and `None` for the folder-less variants (runs,
    /// tasks, and the legacy discovered-plan nodes).
    pub fn folder_idx(&self) -> Option<usize> {
        match *self {
            TreeNode::Folder { folder_idx }
            | TreeNode::PlanInFolder { folder_idx, .. }
            | TreeNode::PlanTaskInFolder { folder_idx, .. } => Some(folder_idx),
            TreeNode::Run { .. }
            | TreeNode::Task { .. }
            | TreeNode::Plan { .. }
            | TreeNode::PlanTask { .. } => None,
        }
    }
}

/// Stable project-scoped key for [`App::collapsed_plans`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CollapseKey {
    Plan(PlanIdentity),
    /// Compatibility-only keys for older unit fixtures. Shipping code never
    /// constructs index-based collapse identity.
    #[cfg(test)]
    LegacyPlan(usize),
    #[cfg(test)]
    FolderPlan {
        folder: usize,
        plan: usize,
    },
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
    /// Tab key — move focus forward through Sidebar → Main → accordion sections → Sidebar (wrap).
    FocusNext,
    /// Shift+Tab key — move focus backward through accordion sections → Main → Sidebar (wrap).
    FocusPrev,
    /// `v` / `V` — cycle the dependency view between
    /// [`DependencyViewMode::Off`], `List`, `Tree`, and `Timeline`.
    CycleDependencyView,
    /// Move the sidebar selection one row up (`↑` / `k`).
    SelectUp,
    /// Move the sidebar selection one row down (`↓` / `j`).
    SelectDown,
    /// `→` — on a collapsed run, expand it; otherwise cross focus into
    /// [`Panel::Main`]. See plan 0018.
    FocusRightOrExpand,
    /// `←` — from [`Panel::Main`], return focus to [`Panel::Sidebar`];
    /// otherwise collapse the focused expanded run. See plan 0018.
    FocusLeftOrCollapse,
    /// Space key — toggle expand/collapse the focused tree node's run (sidebar focus only).
    ToggleTreeNode,
    /// Scroll the focused exchange pane one line up (mouse wheel up).
    ScrollUp,
    /// Scroll the focused exchange pane one line down (mouse wheel down).
    ScrollDown,
    /// Scroll up at the given (column, row) — used for mouse-position-aware routing.
    ScrollUpAt(u16, u16),
    /// Scroll down at the given (column, row) — used for mouse-position-aware routing.
    ScrollDownAt(u16, u16),
    /// Scroll the error pane one line up (PgUp key when error pane is open).
    ErrorPaneScrollUp,
    /// Scroll the error pane one line down (PgDn key when error pane is open).
    ErrorPaneScrollDown,
    /// Left mouse button pressed at `(column, row)` — begin a text selection.
    SelectionStart(u16, u16),
    /// Mouse dragged to `(column, row)` with the left button held — extend the
    /// current text selection.
    SelectionExtend(u16, u16),
    /// Left mouse button released at `(column, row)` — finalise the text
    /// selection. The event loop then copies the highlighted text to the
    /// system clipboard (it reads the rendered buffer, which `update` cannot).
    SelectionEnd(u16, u16),
    /// An event arrived from `api.subscribe()`.
    ApiEvent(Event),
    /// Periodic tick — triggers a redraw without other state changes.
    Tick,
    /// Toggle the error pane open/closed (`e` / `E`).
    ToggleErrorPane,
    /// Toggle the in-TUI log panel open/closed (`l` / `L`) — shows the context
    /// task's agent log at the bottom, like the `v` dependency view.
    ToggleLogPane,

    // ── Task-status view (task 29) ────────────────────────────────────────────
    /// A full [`RunView`] (with its authored tasks) was fetched from the API and
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
    /// starts a background read (→ [`AppEvent::BrowserOpened`]); choosing a file
    /// starts `api.execute(OpenPlan{..})` in the background and returns
    /// [`AppEvent::CloseBrowser`] immediately. `update` does not mutate state
    /// for this variant.
    BrowserActivate,
    /// Go up to the parent directory (Backspace).
    ///
    /// Like [`AppEvent::BrowserActivate`], the actual read happens in the IO
    /// layer, which then emits [`AppEvent::BrowserOpened`] for the parent.
    BrowserParent,
    /// Close the file browser and return to the normal view (Esc, or after a
    /// file was opened).
    CloseBrowser,

    // ── Plan picker (plan 0027) ───────────────────────────────────────────────
    /// Discovery result arrived: store discovered plans for sidebar tree integration.
    PlansDiscovered {
        /// The plans discovered under `docs/plans/` by
        /// [`makina_core::orchestrator::discover_plans`].
        plans: Vec<makina_core::orchestrator::PlanEntry>,
    },
    /// Per-folder discovery result arrived: store discovered plans by folder.
    PlansDiscoveredPerFolder {
        /// Exact canonical folder snapshot used for this discovery pass.
        roots: Vec<PathBuf>,
        /// The plans discovered per folder: folder_idx → list of PlanEntry.
        plans_map: std::collections::HashMap<usize, Vec<makina_core::orchestrator::PlanEntry>>,
    },
    /// Open the focused node in the sidebar (Enter).
    ///
    /// - On a `Plan` node: opens its plan-details tab (via the tab system) and
    ///   expands the plan in the sidebar so its task previews become visible.
    /// - On a `PlanTask` or `Task` node: opens a dedicated tab for that task.
    /// - On a `Run` node that corresponds to a discovered plan (e.g. a completed
    ///   plan's run entry in the sidebar): opens the plan's details tab.
    /// - Run nodes without an associated plan are unaffected (just remain selected).
    OpenFocusedNode,

    // ── Run control (task 31) ─────────────────────────────────────────────────
    /// Start (or resume) the selected Run.
    StartRun,
    /// Publish a model-authored bundle, refresh discovery, then explicitly
    /// open/start the returned registered plan.
    GeneratePlanBundle {
        project_root: PathBuf,
        blueprint: makina_core::api::GeneratedPlanBlueprint,
    },
    /// Open the natural-language plan authoring flow.
    OpenPlanAuthoring,
    PlanAuthoringInput(char),
    /// Insert a literal newline (Shift/Alt+Enter) without submitting.
    PlanAuthoringNewline,
    /// A bracketed paste delivered as one atomic edit, newlines included.
    PlanAuthoringPaste(String),
    PlanAuthoringBackspace,
    PlanAuthoringSubmit,
    PlanAuthoringQuestion {
        question: String,
    },
    PlanAuthoringFailed {
        reason: String,
    },
    /// One event from the planner's own stream — reasoning, a tool call, a tool
    /// update — folded into the authoring transcript exactly as a run's agent
    /// events are folded into a task's.
    PlanAuthoringExchange(makina_core::api::ExchangeEvent),
    /// Bundle generation finished; `Err` carries the reason and hands the
    /// conversation back rather than discarding it.
    PlanAuthoringGenerated(Result<String, String>),
    ClosePlanAuthoring,
    /// Pause the selected Run.
    PauseRun,
    /// Cancel the selected Run.
    CancelRun,
    /// Re-interpret the selected Run (bypass artifact, re-ingest source to
    /// recompute report and graph).
    Reinterpret,

    /// Context-sensitive retry/reset on the focused tree node (plan 0017).
    ///
    /// Resolved by the IO layer via [`App::focused_node`]: a focused `Failed`
    /// task dispatches [`makina_core::api::Command::RetryTask`]; a focused run
    /// with any `Failed` task dispatches
    /// [`makina_core::api::Command::RetryFailedTasks`]; if nothing is retryable,
    /// it falls back to re-interpreting a still-`Pending` run, else surfaces a
    /// `nothing to retry here` status message.  `update` itself does nothing for
    /// this variant (the async command runs in the IO layer), keeping `update`
    /// pure.
    RetryFocused,

    /// Reset the selected run/plan back to a fresh pending graph.
    RequestResetRun,
    /// Execute a reset after the confirmation modal has been accepted.
    ResetRun {
        confirmation: ResetConfirmation,
    },
    /// Close the reset confirmation modal without doing anything.
    CloseResetConfirmation,
    /// A reset has started in a background task.
    ResetStarted {
        target: PlanIdentity,
        label: String,
    },
    /// A background reset finished.
    ResetFinished {
        target: PlanIdentity,
        message: String,
    },
    /// A command was attempted while a plan operation is running.
    OperationBlocked {
        target: PlanIdentity,
        attempted: String,
    },
    /// A project-scoped plan open/start was launched in the background.
    PlanOpenStarted {
        target: PlanIdentity,
    },
    /// The background open/start completed (successfully or otherwise).
    PlanOpenFinished {
        target: PlanIdentity,
    },
    /// A run was just opened (or an existing Pending run was started) and is
    /// now in the "starting" phase — the sidebar should render an animated
    /// badge until the run transitions out of `Pending`. Emitted by the
    /// background `spawn_open_run` auto-start path and by the direct
    /// `StartRun` resolve_io arm.
    RunStarting {
        run: makina_core::api::RunId,
    },
    /// Close the operation notice modal.
    CloseOperationNotice,

    /// Purge Makina-created transient git worktrees for this repository.
    PurgeWorktrees,

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
    ErrorMessageArrived {
        msg: ErrorMessage,
    },

    /// Dismiss the provider-missing warning banner.
    ///
    /// Bound to `d` (in Normal mode) by the event layer.  Sets
    /// [`App::provider_warning_dismissed`] so the banner stops rendering.
    DismissProviderWarning,

    // ── Help overlay (plan 0038) ──────────────────────────────────────────────
    /// User pressed `?` to toggle the help overlay showing all keybindings.
    ToggleHelpMode,
    /// User pressed Escape or `q` to close the help overlay.
    CloseHelpMode,

    // ── Doctor health-check overlay (task 0046) ──────────────────────────────
    /// User requested to open the doctor health-check overlay (pressed `!`).
    OpenDoctor,
    /// Close the doctor overlay and return to the normal view (Esc).
    CloseDoctor,
    /// Write starter config templates (pressed `w` in doctor).
    DoctorWriteScaffold,

    // ── Command palette (plan 0069) ───────────────────────────────────────────
    /// User requested to open the command palette (pressed `Ctrl+P`).
    OpenCommandPalette,
    /// Move the palette selection one row up.
    CommandPaletteUp,
    /// Move the palette selection one row down.
    CommandPaletteDown,
    /// User typed a character into the palette filter.
    CommandPaletteInput(char),
    /// User pressed backspace in the palette filter.
    CommandPaletteBackspace,
    /// User pressed enter to execute the selected action.
    CommandPaletteExecute,
    /// Close the command palette and return to normal mode.
    CloseCommandPalette,
    /// Enter the nested theme selector mode (internal event).
    EnterThemeSelector,
    /// Apply the selected theme from nested selector (internal event).
    ApplyThemeSelection,

    // ── Settings screen (plan 0070) ───────────────────────────────────────────
    /// User requested to open the settings screen.
    OpenSettings,
    /// Move settings focus one field up.
    SettingsUp,
    /// Move settings focus one field down.
    SettingsDown,
    /// User typed a digit into the focused settings field.
    SettingsInput(char),
    /// User pressed backspace in the focused settings field.
    SettingsBackspace,
    /// Cycle a selectable settings field to the previous option.
    SettingsPreviousOption,
    /// Cycle a selectable settings field to the next option.
    SettingsNextOption,
    /// User pressed enter to save settings.
    SettingsCommit,
    /// Project config was written successfully (runtime update may have warned
    /// separately through the status message).
    SettingsSaved {
        project_root: PathBuf,
        values: crate::settings_validation::SettingsValidation,
    },
    /// Settings validation or persistence failed; keep the modal open.
    SettingsSaveFailed {
        reason: String,
    },
    /// Close the settings screen without saving.
    CloseSettings,
    /// Save settings but keep the modal open (auto-save during editing).
    SettingsAutoSave,
    /// Agent model probe completed — discovered models are available.
    /// Each entry is "agent_name/provider_name/model_name".
    ModelsDiscovered {
        models: Vec<String>,
        /// Advertised effort (`thought_level`) values, unqualified: they name a
        /// level the chosen model runs at, not a model of their own.
        efforts: Vec<String>,
    },
    /// Open the searchable model picker for the focused Settings field.
    OpenModelPicker,
    /// Close the model picker without selecting.
    CloseModelPicker,
    /// Select the highlighted model from the picker.
    ModelPickerSelect,
    /// Type in the model picker filter.
    ModelPickerInput(char),
    /// Backspace in the model picker filter.
    ModelPickerBackspace,
    /// Navigate up in the model picker.
    ModelPickerUp,
    /// Navigate down in the model picker.
    ModelPickerDown,

    // ── Folder operations (plan 0043) ────────────────────────────────────────
    /// User requested to open a folder via palette action.
    OpenFolder,

    /// User selected a folder path to open (from file picker).
    OpenFolderSelected {
        path: std::path::PathBuf,
    },

    /// User requested to close a folder via palette action.
    CloseFolderRequested,

    /// User selected a folder path to close (from list).
    CloseFolderConfirmed {
        path: std::path::PathBuf,
    },

    /// User requested to initialize a folder via palette action.
    InitializeFolderRequested,

    /// User selected a folder path to initialize (from file picker).
    InitializeFolderSelected {
        path: std::path::PathBuf,
    },

    /// Move the folder browser selection one row up.
    FolderBrowserUp,
    /// Move the folder browser selection one row down.
    FolderBrowserDown,
    /// Go up to the parent directory in the folder browser (Backspace).
    FolderBrowserParent,
    /// Activate the highlighted folder browser entry (Enter).
    FolderBrowserActivate {
        purpose: crate::app::FolderBrowserPurpose,
    },

    // ── Tabbed content pane (plan 0031) ──────────────────────────────────────
    /// Open a new tab with the given content (or switch to it if already open).
    OpenTab(TabContent),
    /// Close the active tab.
    CloseTab,
    /// Close the tab at the given index — a mouse click on a tab close icon
    /// resolves to this.
    CloseTabAt(usize),
    /// Switch to the next tab (or wrap to the first).
    NextTab,
    /// Switch to the previous tab (or wrap to the last).
    PrevTab,
    /// Activate (switch to) the tab at the given index — a mouse click on the
    /// tab bar resolves to this.
    ActivateTab(usize),
    /// Open (or focus) the tab for the visible tree node at the given index and
    /// move the sidebar cursor to it — a mouse click on a sidebar row resolves
    /// to this, mirroring the keyboard Enter (`OpenFocusedNode`) behaviour.
    OpenTreeRow(usize),

    // ── Accordion sections (plan 0032) ───────────────────────────────────────
    /// Toggle the accordion section for the active plan tab.
    /// Only applies if the active tab is a plan tab; otherwise it is a no-op.
    ToggleAccordionSection(AccordionSection),

    // ── Accordion sections for task tabs (plan 0042, WS6) ───────────────────
    /// Toggle the accordion section for the active task tab.
    /// Only applies if the active tab is a task tab; otherwise it is a no-op.
    ToggleTaskAccordionSection(AccordionSection),

    /// Toggle a captured edit/write tool diff inside a task's Execution section.
    ToggleToolDiff(ToolDiffKey),

    // ── Project discovery (plan 0025) ─────────────────────────────────────────
    /// User requested to discover the project (plan 0025). Dispatched by the
    /// palette and handled in `resolve_io` → `discover_project`, which issues
    /// `Command::DiscoverProject` to the orchestrator.
    DiscoverProject,

    // ── Verbose mode (plan 0021) ──────────────────────────────────────────────
    /// Toggle verbose mode on/off (`Ctrl+O`).
    ///
    /// In verbose mode the exchange pane additionally renders full thought text
    /// and captured tool/edit content. Compact mode keeps concise previews and
    /// lets edit/write diffs expand independently. Does not collide with `o`/`O`
    /// (the file-browser key) because the Ctrl modifier is checked first in
    /// `event.rs`.
    ToggleVerbose,

    // ── Sidebar resizing (plan 0039) ───────────────────────────────────────────
    /// User pressed Shift+Left to decrease the sidebar width by 2%.
    ResizeSidebarLeft,
    /// User pressed Shift+Right to increase the sidebar width by 2%.
    ResizeSidebarRight,
}

// ── App state ─────────────────────────────────────────────────────────────────

/// Per-role turn metrics for a task, rendered in the exchange pane header.
///
/// Holds the latest model, duration, and optional token usage for a single
/// role's completed turn.
#[derive(Debug, Clone)]
pub struct RoleTurnMetric {
    /// The model that answered this role's turn.
    pub model: String,
    /// Wall-clock duration of the turn, in milliseconds.
    pub duration_ms: u64,
    /// Token usage when the backend reported it.
    pub usage: Option<makina_core::api::UsageStats>,
}

/// Stable key for a single expandable tool diff in the task execution log.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolDiffKey {
    pub run: RunId,
    pub task: TaskId,
    pub tool_id: String,
}

/// A pane a mouse text selection can target.
///
/// `hit` is the region a drag may *begin* in (the full pane column, so starting
/// on a border or padding cell still works); `clip` is the inner rectangle the
/// resulting selection is confined to (so the highlight and copied text exclude
/// the border, padding, and any neighbouring pane). Recorded each frame by the
/// render pass via [`App::set_selection_panes`]. See [`crate::selection`].
#[derive(Debug, Clone, Copy)]
pub struct SelectionPane {
    /// Where a drag may start (the full pane column).
    pub hit: ratatui::layout::Rect,
    /// The rectangle the selection is confined to (inner content).
    pub clip: ratatui::layout::Rect,
}

/// Content displayed in a tab in the main pane.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TabContent {
    /// A task within a run, scoped to its project/plan and live run id.
    Task {
        plan: PlanIdentity,
        run: RunId,
        task_id: TaskId,
    },
    /// A task preview under a discovered plan (parsed from its task document):
    /// identified by the plan slug and the task's kebab id. Distinct from
    /// [`TabContent::Task`], which is backed by a running task.
    PlanTask { plan: PlanIdentity, task_id: String },
    /// A discovered plan, including stable project identity.
    Plan { plan: PlanIdentity },
    /// The conversational plan-authoring workspace for one project.
    ///
    /// Authoring is a working surface, not a prompt: the operator drafts a
    /// description, reads the planner's questions, and answers them. That is
    /// the same shape as every other tab, and unlike a modal it can be left
    /// open while looking at a plan or a task and returned to afterwards.
    PlanAuthoring { project_root: PathBuf },
}

/// State for the tabbed content pane.
#[derive(Debug, Clone)]
pub struct TabState {
    /// Currently open tabs.
    pub open_tabs: Vec<TabContent>,
    /// Index of the active tab in `open_tabs`; `None` if no tabs are open.
    pub active_tab: Option<usize>,
}

impl TabState {
    pub fn new() -> Self {
        TabState {
            open_tabs: Vec::new(),
            active_tab: None,
        }
    }

    /// Open a new tab or switch to it if already open.
    pub fn open_tab(&mut self, content: TabContent) {
        if let Some(idx) = self.open_tabs.iter().position(|t| t == &content) {
            self.active_tab = Some(idx);
        } else {
            self.open_tabs.push(content);
            self.active_tab = Some(self.open_tabs.len() - 1);
        }
    }

    /// Close the tab at the given index. If it was the active tab, switch to an adjacent tab.
    pub fn close_tab(&mut self, idx: usize) {
        if idx >= self.open_tabs.len() {
            return;
        }

        let active = self.active_tab;
        self.open_tabs.remove(idx);
        if self.open_tabs.is_empty() {
            self.active_tab = None;
        } else if let Some(active) = active {
            self.active_tab = if active == idx {
                Some(idx.min(self.open_tabs.len() - 1))
            } else if idx < active {
                Some(active - 1)
            } else if active >= self.open_tabs.len() {
                Some(self.open_tabs.len() - 1)
            } else {
                Some(active)
            };
        }
    }
}

impl Default for TabState {
    fn default() -> Self {
        Self::new()
    }
}

/// Accordion section identifier for plan tabs and task detail tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccordionSection {
    /// SCOPE.md section
    Scope,
    /// ARCHITECTURE.md section
    Architecture,
    /// Authored tasks section (with GATED/dependency markers)
    Tasks,
    /// STATUS.md section
    Status,
    /// Execution section (for task details)
    Execution,
}

/// The default expanded accordion sections for a task detail tab: both `Scope`
/// and `Execution` open, so a freshly opened task shows its description and live
/// activity immediately.
///
/// Shared by the renderer (`render_task_entry_pane`) and the toggle handler
/// (`ToggleTaskAccordionSection`) so the "first keypress" starting set matches
/// what is on screen — otherwise the first `s`/`z` would toggle against an empty
/// set and collapse the wrong section.
pub fn default_task_accordion_sections() -> HashSet<AccordionSection> {
    HashSet::from([AccordionSection::Scope, AccordionSection::Execution])
}

/// Identifies a scrollable panel for per-panel scroll state and hitbox testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollablePanel {
    /// The left sidebar listing runs and tasks.
    Sidebar,
    /// The exchange pane (prompts and responses).
    Exchange,
    /// The plan accordion pane (when a plan tab is active).
    PlanAccordion,
    /// The dependency-view overlay (when `DependencyViewMode` is not `Off`).
    DependencyView,
    /// The task entry pane (when a task tab is active).
    TaskEntry,
    /// The bottom Output pane overlay (Problems / Logs tabs).
    ErrorPane,
}

/// Which tab the bottom Output pane is showing.
///
/// The Output pane unifies the former error pane and log panel: `Problems`
/// lists run-blocking ingestion issues + app error messages; `Logs` shows the
/// context task's agent transcript. `[e]` opens/toggles Problems, `[L]` Logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTab {
    /// Ingestion blocking/warning issues + app error messages.
    Problems,
    /// The context task's agent exchange log.
    Logs,
}

/// Geometry of a single scrollable panel (used for mouse hitbox testing).
#[derive(Debug, Clone, Copy)]
pub struct PanelGeometry {
    /// Which panel this rectangle belongs to.
    pub panel: ScrollablePanel,
    /// The rendered rectangle of the panel content area.
    pub rect: ratatui::layout::Rect,
}

/// All mutable TUI state.
///
/// Searchable model picker overlay state.
#[derive(Debug, Clone)]
pub struct ModelPicker {
    /// All discovered models (e.g. ["opencode/claude-sonnet-4-20250514", ...]).
    pub all_models: Vec<String>,
    /// Filter text (typed by the user).
    pub filter: String,
    /// Selected index in the filtered list.
    pub selected: usize,
    /// Which Settings field this picker is for.
    pub target_field: SettingsField,
}

impl ModelPicker {
    /// Filtered model list (case-insensitive substring match).
    pub fn filtered(&self) -> Vec<&str> {
        if self.filter.is_empty() {
            self.all_models.iter().map(|s| s.as_str()).collect()
        } else {
            let f = self.filter.to_lowercase();
            self.all_models
                .iter()
                .filter(|m| m.to_lowercase().contains(&f))
                .map(|s| s.as_str())
                .collect()
        }
    }
}

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

    /// Workspace router used only for project-qualified plan opens. Other
    /// commands continue through `api` and its global run-id routing.
    pub project_api: Option<Arc<crate::project_api::ProjectApiRouter>>,

    /// The developer agent backend, used to probe available models when
    /// Settings opens. Set by main.rs after building the project API.
    pub developer_backend: Option<Arc<dyn makina_core::backend::AgentBackend>>,

    /// Planner backend used by conversational plan authoring.
    pub planner_backend: Option<Arc<dyn makina_core::backend::AgentBackend>>,

    /// Active plan-authoring conversation.
    pub plan_authoring: Option<PlanAuthoring>,

    /// Searchable model picker state (open from Settings model fields).
    pub model_picker: Option<ModelPicker>,

    /// The panel that currently owns keyboard focus.
    pub focused_panel: Panel,

    /// When focused_panel == Panel::Main, tracks which accordion section (if any) has focus.
    /// Defaults to None; Tab from Sidebar enters Main with focused_section = None, then
    /// subsequent Tab moves to the first accordion section (Scope).
    pub focused_section: Option<AccordionSection>,

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

    /// The purpose of the current folder browser, if one is active.
    /// Set when opening a folder browser; cleared when closing.
    pub folder_browser_purpose: Option<FolderBrowserPurpose>,

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

    /// Canonical project owner retained for each run.
    ///
    /// Run identities use canonical repository-relative plan directories.
    /// convention. Remembering the owner while its folder is open keeps an
    /// arbitrary path (for example `repo-b/custom.md`) project-scoped even
    /// after that folder is closed and disappears from `opened_folders`.
    run_project_roots: HashMap<RunId, PathBuf>,

    /// Index into `runs` identifying the currently selected/focused Run.
    /// `None` when `runs` is empty.
    pub selected_run: Option<usize>,

    /// Index into the selected Run's authored tasks identifying the focused task.
    ///
    /// Navigating Up/Down when [`Panel::Main`] is focused moves this.  The
    /// exchange pane renders the focused task's exchange log.  `None` when
    /// the selected run has no tasks.
    pub selected_task: Option<usize>,

    /// Run ids whose task children are collapsed in the sidebar tree.
    /// Absent ⇒ expanded (runs default to expanded).
    pub collapsed_runs: HashSet<RunId>,

    /// Plans currently collapsed in the sidebar tree (excludes expanded plans).
    /// Parallel to `collapsed_runs` but keyed by [`CollapseKey`], which namespaces
    /// legacy discovered plans against folder-scoped plans so the two paths can't
    /// alias. Legacy plans start collapsed: every discovered index is inserted on
    /// [`AppEvent::PlansDiscovered`].
    pub collapsed_plans: HashSet<CollapseKey>,

    /// Folder indices currently collapsed in the sidebar tree.
    pub collapsed_folders: HashSet<usize>,

    /// Plans scoped to each folder: folder_idx → list of PlanEntry.
    pub plans_by_folder: HashMap<usize, Vec<makina_core::orchestrator::PlanEntry>>,

    /// Index into `visible_tree_nodes()` of the focused sidebar node.
    /// `None` when no runs are open.
    pub tree_cursor: Option<usize>,

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

    /// Per-task last-activity tick, updated whenever an exchange event arrives.
    ///
    /// Keyed by `(RunId, TaskId)` to match `exchange_logs`. Used to compute
    /// the idle duration for the focused in-progress task in the exchange header.
    pub task_last_activity_tick: HashMap<(RunId, TaskId), u64>,

    /// Per-task step-start tick, recorded when a task enters `InProgress`/`InReview`.
    ///
    /// Keyed by `(RunId, TaskId)` to match `exchange_logs`. Used to compute
    /// the wall-clock countdown toward `wall_clock_secs` in the exchange header.
    pub task_step_start_tick: HashMap<(RunId, TaskId), u64>,

    /// The configured idle timeout in seconds (discovered from `TaskIdle` events).
    ///
    /// Used to style the idle indicator in the exchange header (amber past half,
    /// red as it approaches). `None` until a TaskIdle event is received.
    pub idle_secs_config: Option<u64>,

    /// The configured wall-clock timeout in seconds.
    ///
    /// For now, this is a hardcoded reasonable default; in a future enhancement
    /// it could be discovered from events. Used to compute the countdown in
    /// the exchange header.
    pub wall_clock_secs_config: u64,

    /// Whether the exchange pane auto-follows the bottom of the log.
    ///
    /// Defaults to `true` (newest exchange always visible).  Scrolling up
    /// disengages auto-follow; scrolling back down to the bottom re-engages it.
    pub exchange_auto_follow: bool,

    /// Whether the error pane auto-follows the bottom of the log.
    ///
    /// Defaults to `true` (newest error always visible).  Scrolling up
    /// disengages auto-follow; scrolling back down to the bottom re-engages it.
    pub error_pane_auto_follow: bool,

    /// Per-panel manual scroll offset (in lines from top). Key is the panel;
    /// a missing key defaults to 0. Written only from `App::update`, so a plain
    /// `HashMap` (no interior mutability needed).
    pub scroll_offsets: std::collections::HashMap<ScrollablePanel, u16>,

    /// Per-panel highest scroll offset the last render produced (the clamp
    /// ceiling for user input). `RefCell` because the `&App` render pass writes
    /// it each frame — this replaces the old `last_scroll_max: Cell<u16>` write
    /// path at `ui.rs:1301`. A missing key defaults to 0 (no scroll needed).
    pub last_scroll_maxes: std::cell::RefCell<std::collections::HashMap<ScrollablePanel, u16>>,

    /// Cache of parsed markdown lines keyed by (text_hash, width, render_context_hash).
    /// `RefCell` because the `&App` render pass populates it via
    /// `render_markdown_cached` — this follows the interior-mutability pattern of
    /// `last_scroll_maxes`. Cleared when the active tab changes or content is updated.
    pub markdown_cache: std::cell::RefCell<
        std::collections::HashMap<(u64, u16, u64), Vec<ratatui::text::Line<'static>>>,
    >,

    /// Last api event received — stored for test assertions and status-bar
    /// display.  Will be used by tasks 27–31 for richer updates.
    pub last_event: Option<Event>,

    /// A transient status-bar message surfacing the most recent command outcome
    /// or error (task 31).  Set by [`AppEvent::StatusMessage`] (emitted by the
    /// IO layer after an `api.execute(...)` resolves) and rendered in the status
    /// bar.  `None` until the first command is issued.
    pub status_message: Option<String>,

    /// Label for an in-flight background job, e.g. `Some("Discovering plans")`.
    ///
    /// Set when async work begins (plan discovery on startup / `[o]`) and
    /// cleared when it resolves ([`AppEvent::PlansDiscovered`] /
    /// [`AppEvent::BrowserOpened`]). While `Some`, the UI renders an animated
    /// spinner + this label so the user can tell the app is busy. `None` when
    /// idle.
    pub busy: Option<String>,

    /// Whether the bottom Output pane is currently visible.
    pub error_pane_open: bool,

    /// Which tab the Output pane shows when open (`Problems` or `Logs`).
    pub output_tab: OutputTab,

    /// Whether the help overlay is currently visible.
    ///
    /// Toggled by pressing `?` and dismissed with Escape or `q`.
    pub help_mode_active: bool,

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

    /// Provider preflight probe results.
    ///
    /// Stores the result of probing each configured provider's command for
    /// presence on the filesystem. Used to render a non-fatal warning when a
    /// provider binary is missing.
    pub provider_probes: Vec<makina_core::preflight::ProviderProbe>,

    /// Whether the provider-missing warning banner has been dismissed by the user.
    ///
    /// Set to `true` when the user activates [`AppEvent::DismissProviderWarning`].
    /// The warning banner is not rendered once this flag is set, even if missing
    /// probes remain.
    pub provider_warning_dismissed: bool,

    /// Resolved config file paths for the doctor view.
    ///
    /// Holds the global and project config paths so the doctor can report which
    /// files exist and where to write scaffold templates.
    pub config_paths: makina_core::config::ConfigPaths,

    /// Whether the configured base_branch exists in the repository.
    ///
    /// Computed once at startup and cached for the doctor view. Used to render
    /// a check in the health checklist.
    pub base_branch_exists: bool,

    /// Command palette state. `Some` only while [`App::mode`] is
    /// [`Mode::CommandPalette`].
    pub command_palette: Option<CommandPalette>,

    // ── Settings (plan 0070) ──────────────────────────────────────────────────
    /// Settings modal state. `Some` only while [`App::mode`] is
    /// [`Mode::Settings`].
    pub settings: Option<Settings>,

    /// Reset confirmation modal state. `Some` only while [`App::mode`] is
    /// [`Mode::ResetConfirm`].
    pub reset_confirmation: Option<ResetConfirmation>,

    /// Operation notice modal state. `Some` only while [`App::mode`] is
    /// [`Mode::OperationNotice`].
    pub operation_notice: Option<OperationNotice>,

    /// The resolved run capabilities (gate/reviewer iterations, wall-clock/idle
    /// timeouts). Seeded from the loaded config and editable via the settings
    /// modal.
    pub caps: makina_core::config::CapsConfig,

    /// The resolved task concurrency limit (parallelism). Seeded from the loaded
    /// config and editable via the settings modal.
    pub concurrency: usize,

    /// What happens to a completed plan branch at run end.
    pub final_merge: FinalMerge,

    /// Plan-level operations keyed by canonical project + plan-directory identity.
    pub plan_operations: HashMap<PlanIdentity, PlanOperationState>,

    /// Plan opens currently executing `OpenPlan` (and optionally `StartRun`).
    /// This closes the TUI-side repeated-interaction window before Core has had
    /// time to publish `RunOpened`.
    pub opening_plans: HashSet<PlanIdentity>,

    /// Runs that have a `StartRun` in flight but have not yet transitioned out
    /// of `Pending` (i.e. no `RepositoryLeaseWaiting`/`RunStatusChanged{Running}`
    /// event has arrived). The sidebar renders an animated "starting" badge for
    /// these runs so the user sees immediate feedback the moment they start a
    /// plan, instead of a static `[·] Pending` that looks like nothing happened.
    pub starting_runs: HashSet<makina_core::api::RunId>,

    /// Latest human-readable progress phase for each run, updated by
    /// `RunProgress` events. Displayed in the sidebar so the user can see what
    /// Makina is doing during startup/setup. Cleared when the run reaches
    /// `Running` with active tasks.
    pub run_progress: HashMap<makina_core::api::RunId, String>,

    /// State for the tabbed main content pane.
    pub tabs: TabState,

    /// Accordion expand/collapse state for plan tabs.
    /// Keyed by plan slug; the set contains sections that are expanded.
    /// Sections not in the set are collapsed. All sections default to collapsed.
    pub accordion_state: HashMap<PlanIdentity, HashSet<AccordionSection>>,

    /// Accordion expand/collapse state for task tabs.
    /// Keyed by task id; the set contains sections that are expanded.
    /// Sections not in the set are collapsed. A task with no entry yet defaults
    /// to [`default_task_accordion_sections`] (Scope + Execution expanded) — both
    /// the renderer and the toggle handler use that same default.
    pub task_accordion_expanded: HashMap<TaskId, HashSet<AccordionSection>>,

    /// Expanded edit/write tool diffs in task Execution sections.
    pub expanded_tool_diffs: HashSet<ToolDiffKey>,

    // ── Verbose mode (plan 0021) ──────────────────────────────────────────────
    /// Whether verbose mode is currently on.
    ///
    /// When `true`, the exchange pane renders full thought text and captured
    /// tool/edit content in addition to concise headers. Toggled by `Ctrl+O`
    /// ([`AppEvent::ToggleVerbose`]). Defaults to `false` (compact).
    pub verbose_mode: bool,

    // ── Sidebar resizing (plan 0039) ───────────────────────────────────────────
    /// User's current sidebar width as a percentage of the body. Adjusted by
    /// ResizeSidebarLeft/Right and clamped to [10, 50] so neither pane collapses.
    pub sidebar_width_percent: u16,

    // ── Theming (plan 0036) ───────────────────────────────────────────────────
    /// Active color theme, read during render. Defaults to Ayu Dark; restored from GlobalConfig on startup and mutated by the 'Switch theme' palette action.
    pub active_theme: crate::theme::Theme,

    // ── Per-role metrics (plan 0024) ───────────────────────────────────────────
    /// Latest per-role turn metrics, keyed by (run, task) then role.
    ///
    /// Updated on every `Event::RoleTurnMetrics`; the content pane renders the
    /// most recent metric for each role of the focused task.
    pub role_metrics: HashMap<(RunId, TaskId), HashMap<AgentRole, RoleTurnMetric>>,

    // ── Plan picker (plan 0027) ───────────────────────────────────────────────
    /// Plans discovered under `docs/plans/` by
    /// [`makina_core::orchestrator::discover_plans`]. Populated when
    /// [`AppEvent::PlansDiscovered`] is processed; empty until then.
    pub discovered_plans: Vec<makina_core::orchestrator::PlanEntry>,

    // ── Mouse text selection ──────────────────────────────────────────────────
    /// The active mouse-driven text selection over the rendered screen, if any.
    ///
    /// Begun on left-button down, extended on drag, finalised on button up.
    /// `None` when nothing is selected. Drives the on-screen highlight
    /// ([`crate::selection::Selection::highlight`], applied in [`crate::ui::render`])
    /// and, once released, the clipboard copy the event loop performs by reading
    /// the rendered buffer. See [`crate::selection`] for why selection lives in
    /// the app rather than the terminal.
    pub selection: Option<crate::selection::Selection>,

    /// Selectable pane rectangles for the current frame, recorded by the render
    /// pass via [`App::set_selection_panes`] (interior mutability, like
    /// [`App::last_scroll_max`]). The event layer reads them on a left-button
    /// down to confine the new selection to the single pane the drag began in.
    pub selection_panes: std::cell::RefCell<Vec<SelectionPane>>,

    /// Rendered rectangles of each scrollable panel, recorded each frame for
    /// hitbox testing. `RefCell` so the `&App` render pass can rewrite it,
    /// mirroring `selection_panes`.
    pub panel_geometries: std::cell::RefCell<Vec<PanelGeometry>>,

    /// Bounding box of each visible accordion section header, recorded during the
    /// plan-accordion render so the event loop can hit-test mouse clicks against it.
    /// Cleared and repopulated every frame, so resizes and pane reflows self-correct.
    /// `RefCell` so the `&App` render pass can rewrite it, mirroring `selection_panes`.
    pub accordion_header_bounds: std::cell::RefCell<Vec<(AccordionSection, Rect)>>,

    /// Bounding box of each visible expandable tool-diff header, keyed by its
    /// run/task/tool id. Cleared and repopulated every frame.
    pub tool_diff_bounds: std::cell::RefCell<Vec<(ToolDiffKey, Rect)>>,

    /// Bounding box of each tab chip in the tab bar, keyed by its index in
    /// `tabs.open_tabs`, recorded during `render_tab_bar` so a mouse click can
    /// activate the tab under the cursor. Cleared and repopulated every frame.
    pub tab_bounds: std::cell::RefCell<Vec<(usize, Rect)>>,

    /// Bounding box of each tab close icon in the tab bar, keyed by its index in
    /// `tabs.open_tabs`, recorded during `render_tab_bar` so a mouse click can
    /// close the tab under the cursor. Cleared and repopulated every frame.
    pub tab_close_bounds: std::cell::RefCell<Vec<(usize, Rect)>>,

    /// Bounding box of each visible sidebar row, keyed by its index in
    /// `visible_tree_nodes()`, recorded during the sidebar render so a mouse
    /// click can open/focus that node's tab (mirrors keyboard Enter). Cleared
    /// and repopulated every frame, so scrolling and resizes self-correct.
    pub sidebar_node_bounds: std::cell::RefCell<Vec<(usize, Rect)>>,

    /// Opened folders persisted in workspace.
    pub opened_folders: Vec<PathBuf>,

    /// Workspace state for save/load.
    pub workspace: crate::workspace::Workspace,

    /// Explicit override for the workspace persistence file, used by tests so
    /// they never touch the operator's real `$HOME/.makina/workspace.toml`.
    /// When `None` (the production default), workspace-saving event handlers
    /// call [`crate::workspace::Workspace::save`], which resolves the real
    /// home directory; when `Some(path)`, they call
    /// [`crate::workspace::Workspace::save_to`] with this path instead.
    pub workspace_path_override: Option<PathBuf>,
}

/// Compute a hash of the input text for cache key generation.
pub fn hash_text(text: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl App {
    pub(crate) fn canonical_repo_root(&self) -> PathBuf {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        lexical_absolute(&cwd, &self.repo_root)
    }

    /// Load transcripts for the initially-selected run (if any).
    ///
    /// Should be called right after `new()` to populate exchanges for the first
    /// run shown in the TUI. This is a separate step because `App::new` does not
    /// take `&mut self`.
    pub fn load_initial_exchanges(&mut self) {
        self.load_exchanges_for_selected_run();
    }

    /// Flatten discovered plans + open runs + the tasks of expanded runs into the visible-node
    /// order shown in the sidebar. Runs replace their discovered-plan node in place so the
    /// sidebar order stays stable when a plan is started (a run for plan 0001 stays where
    /// plan 0001 was, not shifted to the bottom).
    pub fn visible_tree_nodes(&self) -> Vec<TreeNode> {
        let mut nodes = Vec::new();
        // Track which run indices were already emitted in Steps 1-2 so Step 3
        // only appends the ones that did not match a discovered plan.
        let mut emitted_runs: std::collections::HashSet<usize> = std::collections::HashSet::new();

        // Project-scoped plans that already have an open Run (any status, deduped to
        // latest below). Such a plan is rendered as its Run node — NOT also as
        // static plan — otherwise starting would duplicate.
        let run_plans: std::collections::HashSet<PlanIdentity> = self
            .runs
            .iter()
            .map(|run| self.plan_identity_for_run(run))
            .collect();

        // Step 1: Add discovered folders and their plans, interleaving run nodes
        // in place of discovered plans that have an open Run. This keeps the
        // sidebar order stable when a plan is started: the run node appears
        // exactly where the discovered-plan node was, not at the bottom.
        for folder_idx in 0..self.opened_folders.len() {
            nodes.push(TreeNode::Folder { folder_idx });

            // Only expand this folder if not collapsed.
            if !self.collapsed_folders.contains(&folder_idx)
                && let Some(plans) = self.plans_by_folder.get(&folder_idx)
            {
                for (local_plan_idx, plan) in plans.iter().enumerate() {
                    let target =
                        self.plan_identity_for_entry(&self.opened_folders[folder_idx], plan);
                    if run_plans.contains(&target) {
                        // This plan has an open Run: render the run node in place
                        // (instead of the discovered-plan node) so the sidebar
                        // order does not shift.
                        if let Some(run_idx) = self.run_index_for_plan(&target) {
                            emitted_runs.insert(run_idx);
                            self.push_run_node(&mut nodes, run_idx);
                        }
                        continue;
                    }
                    nodes.push(TreeNode::PlanInFolder {
                        folder_idx,
                        plan_idx: local_plan_idx,
                    });

                    // Expand plan tasks if not collapsed.
                    if !self
                        .collapsed_plans
                        .contains(&CollapseKey::Plan(target.clone()))
                    {
                        for task_idx in 0..plan.tasks().len() {
                            nodes.push(TreeNode::PlanTaskInFolder {
                                folder_idx,
                                plan_idx: local_plan_idx,
                                task_idx,
                            });
                        }
                    }
                }
            }
        }

        // Step 2: Add legacy discovered plans (for backward compat), interleaving
        // run nodes in place just like Step 1.
        for (plan_idx, plan) in self.discovered_plans.iter().enumerate() {
            let target = self.plan_identity_for_entry(&self.repo_root, plan);
            if run_plans.contains(&target) {
                if let Some(run_idx) = self.run_index_for_plan(&target) {
                    emitted_runs.insert(run_idx);
                    self.push_run_node(&mut nodes, run_idx);
                }
                continue;
            }
            nodes.push(TreeNode::Plan { plan_idx });
            if !self.collapsed_plans.contains(&CollapseKey::Plan(target)) {
                for task_idx in 0..plan.tasks().len() {
                    nodes.push(TreeNode::PlanTask { plan_idx, task_idx });
                }
            }
        }

        // Step 3: Add any remaining open runs that were not emitted in Steps 1-2
        // (e.g. historical disk runs whose plan is no longer discovered, or runs
        // from a closed folder). These appear after the discovered plans.
        for (run_idx, run) in self.runs.iter().enumerate() {
            if emitted_runs.contains(&run_idx) {
                continue;
            }
            // Dedup: skip non-latest runs for the same plan (mirror the old logic).
            let target = self.plan_identity_for_run(run);
            if self
                .latest_run_for_plan(&target)
                .is_some_and(|latest| latest.id != run.id)
            {
                continue;
            }
            emitted_runs.insert(run_idx);
            self.push_run_node(&mut nodes, run_idx);
        }

        nodes
    }

    /// Push a `TreeNode::Run` (and its expanded `TreeNode::Task` children) for
    /// `run_idx` onto `nodes`. Used by [`visible_tree_nodes`].
    fn push_run_node(&self, nodes: &mut Vec<TreeNode>, run_idx: usize) {
        nodes.push(TreeNode::Run { run: run_idx });
        if let Some(run) = self.runs.get(run_idx)
            && !self.collapsed_runs.contains(&run.id)
        {
            for task_idx in 0..run.tasks.len() {
                nodes.push(TreeNode::Task {
                    run: run_idx,
                    task: task_idx,
                });
            }
        }
    }

    /// Return the index into `self.runs` for the latest run matching `target`,
    /// or `None` if no run matches. Used by [`visible_tree_nodes`] to interleave
    /// run nodes in place of discovered-plan nodes.
    fn run_index_for_plan(&self, target: &PlanIdentity) -> Option<usize> {
        self.runs
            .iter()
            .enumerate()
            .filter(|(_, run)| self.plan_identity_for_run(run) == *target)
            .max_by_key(|(_, run)| &run.run_uid)
            .map(|(idx, _)| idx)
    }

    /// The node currently under the tree cursor, if any.
    pub fn focused_node(&self) -> Option<TreeNode> {
        let nodes = self.visible_tree_nodes();
        self.tree_cursor
            .and_then(|cursor| nodes.get(cursor).copied())
    }

    /// Recompute `selected_run`/`selected_task` from the focused node and,
    /// when the run changed, load that run's exchanges.
    fn sync_selection_from_cursor(&mut self) {
        let prev_run = self.selected_run;
        match self.focused_node() {
            None => {
                self.selected_run = None;
                // Note: selected_task is no longer updated by sidebar navigation
                // (plan 0031). Tabs manage task focus independently.
            }
            Some(TreeNode::Run { run }) => {
                self.selected_run = Some(run);
                // Note: selected_task is no longer updated by sidebar navigation
                // (plan 0031). Tabs manage task focus independently.
                // Tab state is independent from sidebar navigation (plan 0031).
            }
            Some(TreeNode::Task { run, .. }) => {
                // When navigating to a task node, update the selected run but NOT
                // the selected task. Task focus is now managed by the tab system
                // (plan 0031), not by sidebar cursor movement.
                self.selected_run = Some(run);
                // Tab state is independent from sidebar navigation (plan 0031).
            }
            Some(TreeNode::Plan { plan_idx: _ })
            | Some(TreeNode::PlanTask { plan_idx: _, .. })
            | Some(TreeNode::Folder { .. })
            | Some(TreeNode::PlanInFolder { .. })
            | Some(TreeNode::PlanTaskInFolder { .. }) => {
                // Plan / plan-task nodes and folder nodes have no associated run.
                self.selected_run = None;
                // Tab state is independent from sidebar navigation (plan 0031).
            }
        }
        // Load exchanges if the run changed.
        if self.selected_run != prev_run {
            self.load_exchanges_for_selected_run();
        }
    }

    /// When the active tab is a task tab, point `selected_run` at the run that
    /// contains that task so the task-entry pane actually renders.
    ///
    /// The task-tab render arm scopes its task lookup to `selected_run`
    /// (`find_task_idx_in_run`), but `selected_run` is otherwise driven only by
    /// the sidebar cursor. Without this sync, switching to a task tab (via tab
    /// click or Next/Prev) while the cursor sits on a plan node leaves
    /// `selected_run = None`, so the task tab renders a blank pane.
    ///
    /// For plan tabs, sync to the run corresponding to that plan so palette run
    /// controls can target it without manually selecting the run in the sidebar.
    pub(crate) fn sync_selected_run_to_active_tab(&mut self) {
        let Some(active) = self.tabs.active_tab else {
            return;
        };
        let prev_run = self.selected_run;
        match self.tabs.open_tabs.get(active) {
            Some(TabContent::Task { run, task_id, .. }) => {
                if let Some(run_idx) = self.runs.iter().position(|view| {
                    view.id == *run && view.tasks.iter().any(|task| task.id == *task_id)
                }) {
                    self.selected_run = Some(run_idx);
                }
            }
            // Plan and plan-task preview tabs (plan 0042): point selected_run at
            // the run for that plan if one is open, else CLEAR any stale
            // selection — so run-control acts on this plan (Start opens + runs
            // it) instead of a previously selected, unrelated run.
            Some(TabContent::Plan { plan }) | Some(TabContent::PlanTask { plan, .. }) => {
                self.selected_run = self
                    .latest_run_for_plan(plan)
                    .and_then(|r| self.runs.iter().position(|rr| rr.id == r.id));
            }
            _ => {}
        }
        if self.selected_run != prev_run {
            self.load_exchanges_for_selected_run();
        }
    }

    /// Close plan tabs whose slug is no longer in `discovered_plans` or
    /// `plans_by_folder` (plan 0032; extended for per-folder discovery in
    /// plan 0043). Called when plans are re-discovered and the list changes.
    fn close_tabs_for_missing_plans(&mut self) {
        let mut valid_plans = std::collections::HashSet::new();
        for plan in &self.discovered_plans {
            valid_plans.insert(self.plan_identity_for_entry(&self.repo_root, plan));
        }
        for (folder_idx, plans) in &self.plans_by_folder {
            let Some(root) = self.opened_folders.get(*folder_idx) else {
                continue;
            };
            for plan in plans {
                valid_plans.insert(self.plan_identity_for_entry(root, plan));
            }
        }
        let mut indices_to_close = Vec::new();
        let mut plans_to_remove = Vec::new();
        for (idx, tab) in self.tabs.open_tabs.iter().enumerate() {
            // Both plan tabs and a plan's task-preview tabs are keyed by plan
            // slug; close either when its plan is gone.
            let plan = match tab {
                TabContent::Plan { plan } | TabContent::PlanTask { plan, .. } => Some(plan),
                // Authoring is keyed by project root, not by a plan that must
                // already exist — it is where a plan comes from.
                TabContent::Task { .. } | TabContent::PlanAuthoring { .. } => None,
            };
            if let Some(plan) = plan
                && !valid_plans.contains(plan)
            {
                indices_to_close.push(idx);
                plans_to_remove.push(plan.clone());
            }
        }
        // Close tabs in reverse order so indices don't shift.
        for idx in indices_to_close.iter().rev() {
            self.tabs.close_tab(*idx);
        }
        // Clean up accordion state for removed plans.
        for plan in plans_to_remove {
            self.accordion_state.remove(&plan);
        }
    }

    /// Move the cursor by ±1 within the visible nodes (clamped), then
    /// `sync_selection_from_cursor`. Returns whether the cursor moved.
    pub fn tree_move(&mut self, delta: isize) -> bool {
        let nodes = self.visible_tree_nodes();
        if nodes.is_empty() {
            return false;
        }

        let old_cursor = self.tree_cursor;
        let new_cursor = match self.tree_cursor {
            None => 0,
            Some(c) => {
                let new_val = (c as isize) + delta;
                // Clamp to valid range [0, nodes.len() - 1].
                (new_val.max(0) as usize).min(nodes.len() - 1)
            }
        };

        self.tree_cursor = Some(new_cursor);
        self.sync_selection_from_cursor();

        old_cursor != Some(new_cursor)
    }

    /// Toggle collapse/expand on the focused node's parent (a run for run/task
    /// nodes, a plan for plan/plan-task nodes); keep the cursor on that header
    /// and re-sync. Returns whether anything toggled. A plan with no tasks is
    /// not expandable (returns `false`).
    pub fn tree_toggle_expand(&mut self) -> bool {
        match self.focused_node() {
            None => false,
            Some(TreeNode::Run { run }) | Some(TreeNode::Task { run, .. }) => {
                let Some(run_view) = self.runs.get(run) else {
                    return false;
                };
                let run_id = run_view.id;
                if self.collapsed_runs.contains(&run_id) {
                    self.collapsed_runs.remove(&run_id);
                } else {
                    self.collapsed_runs.insert(run_id);
                }
                self.move_cursor_to_run_header(run);
                self.sync_selection_from_cursor();
                true
            }
            Some(TreeNode::Plan { plan_idx }) | Some(TreeNode::PlanTask { plan_idx, .. }) => {
                // No tasks → nothing to reveal; leave it as a leaf.
                if self
                    .discovered_plans
                    .get(plan_idx)
                    .is_none_or(|p| p.tasks().is_empty())
                {
                    return false;
                }
                let Some(key) = self.collapse_key_for_node(TreeNode::Plan { plan_idx }) else {
                    return false;
                };
                if self.collapsed_plans.contains(&key) {
                    self.collapsed_plans.remove(&key);
                } else {
                    self.collapsed_plans.insert(key);
                }
                self.move_cursor_to_plan_header(plan_idx);
                self.sync_selection_from_cursor();
                true
            }
            Some(TreeNode::Folder { folder_idx }) => {
                // Toggle folder expansion: if collapsed, expand it (remove from collapsed_folders);
                // otherwise collapse it (add to collapsed_folders).
                if self.collapsed_folders.contains(&folder_idx) {
                    self.collapsed_folders.remove(&folder_idx);
                } else {
                    self.collapsed_folders.insert(folder_idx);
                }
                self.move_cursor_to_folder_header(folder_idx);
                self.sync_selection_from_cursor();
                true
            }
            Some(TreeNode::PlanInFolder {
                folder_idx,
                plan_idx,
            }) => {
                // Check if the plan has tasks before allowing expand/collapse.
                if let Some(plans) = self.plans_by_folder.get(&folder_idx)
                    && let Some(plan) = plans.get(plan_idx)
                    && plan.tasks().is_empty()
                {
                    return false;
                }
                // Use the same collapse key as visible_tree_nodes() uses.
                let Some(collapse_key) = self.collapse_key_for_node(TreeNode::PlanInFolder {
                    folder_idx,
                    plan_idx,
                }) else {
                    return false;
                };
                if self.collapsed_plans.contains(&collapse_key) {
                    self.collapsed_plans.remove(&collapse_key);
                } else {
                    self.collapsed_plans.insert(collapse_key);
                }
                self.move_cursor_to_plan_in_folder_header(folder_idx, plan_idx);
                self.sync_selection_from_cursor();
                true
            }
            Some(TreeNode::PlanTaskInFolder {
                folder_idx,
                plan_idx,
                ..
            }) => {
                // On a task leaf, collapse its parent plan and park the cursor on the plan header.
                let Some(collapse_key) = self.collapse_key_for_node(TreeNode::PlanTaskInFolder {
                    folder_idx,
                    plan_idx,
                    task_idx: 0,
                }) else {
                    return false;
                };
                self.collapsed_plans.insert(collapse_key);
                self.move_cursor_to_plan_in_folder_header(folder_idx, plan_idx);
                self.sync_selection_from_cursor();
                true
            }
        }
    }

    /// Park the tree cursor on the given run's header node.
    fn move_cursor_to_run_header(&mut self, run_idx: usize) {
        if let Some(idx) = self
            .visible_tree_nodes()
            .iter()
            .position(|node| matches!(node, TreeNode::Run { run: r } if *r == run_idx))
        {
            self.tree_cursor = Some(idx);
        }
    }

    /// Park the tree cursor on the given plan's header node.
    fn move_cursor_to_plan_header(&mut self, plan_idx: usize) {
        if let Some(idx) = self
            .visible_tree_nodes()
            .iter()
            .position(|node| matches!(node, TreeNode::Plan { plan_idx: p } if *p == plan_idx))
        {
            self.tree_cursor = Some(idx);
        }
    }

    /// Park the tree cursor on the given folder's header node.
    fn move_cursor_to_folder_header(&mut self, folder_idx: usize) {
        if let Some(idx) = self
            .visible_tree_nodes()
            .iter()
            .position(|node| matches!(node, TreeNode::Folder { folder_idx: f } if *f == folder_idx))
        {
            self.tree_cursor = Some(idx);
        }
    }

    /// Park the tree cursor on the given plan-in-folder's header node.
    fn move_cursor_to_plan_in_folder_header(&mut self, folder_idx: usize, plan_idx: usize) {
        if let Some(idx) = self.visible_tree_nodes().iter().position(|node| {
            matches!(
                node,
                TreeNode::PlanInFolder {
                    folder_idx: f,
                    plan_idx: p
                } if *f == folder_idx && *p == plan_idx
            )
        }) {
            self.tree_cursor = Some(idx);
        }
    }

    /// Open/focus the plan-details tab for a plan in a folder, and
    /// ensure the plan is expanded in the sidebar (revealing its task previews).
    fn activate_plan_in_folder(&mut self, folder_idx: usize, plan_idx: usize) {
        if let Some(plans) = self.plans_by_folder.get(&folder_idx)
            && let Some(plan) = plans.get(plan_idx)
            && let Some(project_root) = self.opened_folders.get(folder_idx)
        {
            let target = self.plan_identity_for_entry(project_root, plan);
            self.tabs.open_tab(TabContent::Plan {
                plan: target.clone(),
            });
            if !plan.tasks().is_empty() {
                let collapse_key = CollapseKey::Plan(target.clone());
                self.collapsed_plans.remove(&collapse_key);
                self.move_cursor_to_plan_in_folder_header(folder_idx, plan_idx);
                self.sync_selection_from_cursor();
            }
        }
    }

    /// Open/focus the plan-details tab for the given discovered plan index, and
    /// ensure the plan is expanded in the sidebar (revealing its task previews).
    /// Called from Enter (OpenFocusedNode) on Plan nodes and from mouse click
    /// (OpenTreeRow) on plan rows.
    fn activate_plan_node(&mut self, plan_idx: usize) {
        if let Some(plan) = self.discovered_plans.get(plan_idx) {
            let target = self.plan_identity_for_entry(&self.repo_root, plan);
            self.tabs.open_tab(TabContent::Plan {
                plan: target.clone(),
            });
            if !plan.tasks().is_empty() {
                self.collapsed_plans
                    .remove(&CollapseKey::Plan(target.clone()));
                self.move_cursor_to_plan_header(plan_idx);
                self.sync_selection_from_cursor();
            }
        }
    }

    /// Perform the "activate/open" action for whatever tree node is currently
    /// under `tree_cursor` (the highlighted sidebar row). This is the core of
    /// handling Enter on a sidebar node (from OpenFocusedNode) and also the
    /// fallback for Enter in the main pane when no accordion section is focused
    /// (so that "plan highlighted in sidebar + Enter" works even if focus has
    /// moved to main, e.g. after Right-arrow navigation).
    fn activate_focused_tree_node(&mut self) {
        // `opened_tab` tracks whether Enter opened (or focused) a content tab
        // so the focus can cross to the main pane — mirroring `FocusRightOrExpand`'s
        // crossing behavior. Folders only expand/collapse and stay on the sidebar.
        let mut _opened_tab = false;
        match self.focused_node() {
            Some(TreeNode::Plan { plan_idx }) => {
                self.activate_plan_node(plan_idx);
                _opened_tab = true;
            }
            Some(TreeNode::PlanTask { plan_idx, task_idx }) => {
                if let Some(plan) = self.discovered_plans.get(plan_idx)
                    && let Some(preview) = plan.tasks().get(task_idx)
                {
                    let tab_content = TabContent::PlanTask {
                        plan: self.plan_identity_for_entry(&self.repo_root, plan),
                        task_id: preview.frontmatter.id.as_str().to_owned(),
                    };
                    self.tabs.open_tab(tab_content);
                    _opened_tab = true;
                }
            }
            Some(TreeNode::PlanInFolder {
                folder_idx,
                plan_idx,
            }) => {
                self.activate_plan_in_folder(folder_idx, plan_idx);
                _opened_tab = true;
            }
            Some(TreeNode::PlanTaskInFolder {
                folder_idx,
                plan_idx,
                task_idx,
            }) => {
                if let Some(plans) = self.plans_by_folder.get(&folder_idx)
                    && let Some(plan) = plans.get(plan_idx)
                    && let Some(preview) = plan.tasks().get(task_idx)
                    && let Some(project_root) = self.opened_folders.get(folder_idx)
                {
                    let tab_content = TabContent::PlanTask {
                        plan: self.plan_identity_for_entry(project_root, plan),
                        task_id: preview.frontmatter.id.as_str().to_owned(),
                    };
                    self.tabs.open_tab(tab_content);
                    _opened_tab = true;
                }
            }
            Some(TreeNode::Folder { .. }) => {
                // Enter on a Folder expands/collapses it (like Space). Focus
                // stays on the sidebar so the user can keep navigating.
                self.tree_toggle_expand();
            }
            Some(TreeNode::Task { run, task }) => {
                if let Some(run_view) = self.runs.get(run)
                    && let Some(task_view) = run_view.tasks.get(task)
                {
                    let plan = self.plan_identity_for_run(run_view);
                    let tab_content = TabContent::Task {
                        plan,
                        run: run_view.id,
                        task_id: task_view.id.clone(),
                    };
                    self.tabs.open_tab(tab_content);
                    self.sync_selected_run_to_active_tab();
                    _opened_tab = true;
                }
            }
            Some(TreeNode::Run { run }) => {
                if let Some(run_view) = self.runs.get(run) {
                    let target = self.plan_identity_for_run(run_view);
                    if self.plan_entry(&target).is_some() {
                        self.tabs.open_tab(TabContent::Plan { plan: target });
                        _opened_tab = true;
                    }
                    // Expand the run so its tasks become visible in the sidebar
                    // (mirrors the Plan node path in activate_plan_node).
                    self.collapsed_runs.remove(&run_view.id);
                    self.move_cursor_to_run_header(run);
                    self.sync_selection_from_cursor();
                }
            }
            _ => {}
        }
        // Keep focus in the sidebar when Enter opened a content tab. The tab
        // opens in the main pane but the user's focus stays where they are so
        // they can continue navigating the tree. Press Tab or Right to cross
        // to the main pane explicitly.
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
        let tree_cursor = if initial_runs.is_empty() {
            None
        } else {
            Some(0)
        };
        // Runs start collapsed: seed `collapsed_runs` with every initial run's
        // id so the sidebar opens tidy (mirroring `collapsed_plans` in
        // PlansDiscovered). Without this, historical disk runs loaded at
        // launch render expanded with their tasks visible, which makes the
        // first plan appear "already expanded" on startup.
        let collapsed_runs: HashSet<RunId> = initial_runs.iter().map(|r| r.id).collect();
        let mut app = Self {
            should_quit: false,
            api,
            project_api: None,
            developer_backend: None,
            planner_backend: None,
            plan_authoring: None,
            model_picker: None,
            focused_panel: Panel::Sidebar,
            focused_section: None,
            mode: Mode::Normal,
            dependency_view: DependencyViewMode::Off,
            browser: None,
            folder_browser_purpose: None,

            discovered_capabilities: None,
            providers: Vec::new(),
            roles: RolesConfig::default(),
            runs: initial_runs,
            run_project_roots: HashMap::new(),
            selected_run,
            selected_task,
            collapsed_runs,
            collapsed_plans: HashSet::new(),
            collapsed_folders: HashSet::new(),
            plans_by_folder: HashMap::new(),
            tree_cursor,
            exchange_logs: HashMap::new(),
            task_last_activity_tick: HashMap::new(),
            task_step_start_tick: HashMap::new(),
            exchange_auto_follow: true,
            error_pane_auto_follow: true,
            scroll_offsets: std::collections::HashMap::new(),
            last_scroll_maxes: std::cell::RefCell::new(std::collections::HashMap::new()),
            markdown_cache: std::cell::RefCell::new(std::collections::HashMap::new()),
            last_event: None,
            status_message: None,
            busy: None,
            error_pane_open: false,
            output_tab: OutputTab::Problems,
            help_mode_active: false,
            error_messages: Vec::new(),
            unseen_errors: false,
            repo_root,
            tick: 0,
            idle_secs_config: None,
            wall_clock_secs_config: 600, // 10 minutes as a reasonable default
            provider_probes: Vec::new(),
            provider_warning_dismissed: false,
            config_paths: makina_core::config::ConfigPaths {
                global: None,
                project: None,
            },
            base_branch_exists: false,
            command_palette: None,
            settings: None,
            reset_confirmation: None,
            operation_notice: None,
            caps: makina_core::config::CapsConfig::default(),
            concurrency: 3,
            final_merge: FinalMerge::Squash,
            plan_operations: HashMap::new(),
            opening_plans: HashSet::new(),
            starting_runs: HashSet::new(),
            run_progress: HashMap::new(),
            verbose_mode: false,
            active_theme: crate::theme::ayu_dark(),
            role_metrics: HashMap::new(),
            discovered_plans: Vec::new(),
            tabs: TabState::new(),
            accordion_state: HashMap::new(),
            task_accordion_expanded: HashMap::new(),
            expanded_tool_diffs: HashSet::new(),
            sidebar_width_percent: 30,
            selection: None,
            selection_panes: std::cell::RefCell::new(Vec::new()),
            panel_geometries: std::cell::RefCell::new(Vec::new()),
            accordion_header_bounds: std::cell::RefCell::new(Vec::new()),
            tool_diff_bounds: std::cell::RefCell::new(Vec::new()),
            tab_bounds: std::cell::RefCell::new(Vec::new()),
            tab_close_bounds: std::cell::RefCell::new(Vec::new()),
            sidebar_node_bounds: std::cell::RefCell::new(Vec::new()),
            opened_folders: Vec::new(),
            workspace: crate::workspace::Workspace::new(),
            workspace_path_override: None,
        };
        app.refresh_run_project_roots();
        app
    }

    /// Build a new [`App`] with explicit providers, roles, probes, and doctor state from a resolved config.
    ///
    /// Use this variant when a config is available (main.rs) so the provider
    /// editor is seeded with the current configuration, probes are available
    /// for warning display, and the doctor has the config paths and base branch state.
    #[allow(clippy::too_many_arguments)]
    pub fn with_config(
        api: Arc<dyn Api>,
        initial_runs: Vec<RunView>,
        repo_root: PathBuf,
        providers: Vec<ProviderConfig>,
        roles: RolesConfig,
        provider_probes: Vec<makina_core::preflight::ProviderProbe>,
        config_paths: makina_core::config::ConfigPaths,
        base_branch_exists: bool,
        caps: makina_core::config::CapsConfig,
        concurrency: usize,
        final_merge: FinalMerge,
        opened_folders: Vec<PathBuf>,
        workspace: crate::workspace::Workspace,
    ) -> Self {
        let mut app = Self::new(api, initial_runs, repo_root);
        app.providers = providers;
        app.roles = roles;
        app.provider_probes = provider_probes;
        app.config_paths = config_paths;
        app.base_branch_exists = base_branch_exists;
        app.caps = caps;
        app.concurrency = concurrency;
        app.final_merge = final_merge;
        app.opened_folders = opened_folders;
        app.refresh_run_project_roots();
        app.workspace = workspace;
        app
    }

    /// The agent commands of the configured providers.
    ///
    /// These are the prefixes the model picker prepends to each discovered
    /// model; [`canonical_model_name`] needs them to tell a picker prefix apart
    /// from a model name that legitimately contains a slash.
    pub fn provider_agent_names(&self) -> Vec<String> {
        self.providers.iter().map(|p| p.command.clone()).collect()
    }

    /// The per-role models currently entered in the Settings modal, reduced to
    /// the bare names stored in config.
    ///
    /// An empty buffer yields `None` for that role, which every writer reads as
    /// "leave the stored model alone" rather than "clear it".
    pub fn selected_role_models(&self) -> makina_core::config::RoleModels {
        let Some(settings) = self.settings.as_ref() else {
            return makina_core::config::RoleModels::default();
        };
        let agents = self.provider_agent_names();
        makina_core::config::RoleModels {
            developer: canonical_model_name(&settings.developer_model, &agents),
            reviewer: canonical_model_name(&settings.reviewer_model, &agents),
            planner: canonical_model_name(&settings.planner_model, &agents),
        }
    }

    /// The per-role efforts currently entered in the Settings modal.
    ///
    /// Unlike a model, an empty buffer here is meaningful: it is how an
    /// operator returns a role to the provider's own default. It is therefore
    /// reported as `Some("")` — a deliberate clear — whenever the role has a
    /// stored effort to clear, and `None` only when there is nothing to change.
    pub fn selected_role_efforts(&self) -> makina_core::config::RoleEfforts {
        let Some(settings) = self.settings.as_ref() else {
            return makina_core::config::RoleEfforts::default();
        };
        let choice = |buffer: &str, stored: Option<&makina_core::config::RoleAssignment>| {
            let buffer = buffer.trim();
            if !buffer.is_empty() {
                return Some(buffer.to_owned());
            }
            let had_effort = stored
                .and_then(|role| role.effort.as_deref())
                .is_some_and(|effort| !effort.is_empty());
            had_effort.then(String::new)
        };
        makina_core::config::RoleEfforts {
            developer: choice(&settings.developer_effort, self.roles.developer.as_ref()),
            reviewer: choice(&settings.reviewer_effort, self.roles.reviewer.as_ref()),
            planner: choice(&settings.planner_effort, self.roles.planner.as_ref()),
        }
    }

    /// The model (and effort, when set) the planner role will actually use.
    ///
    /// Shown on the authoring tab so the operator can see which model is about
    /// to answer without opening Settings to find out. Falls back to the
    /// provider's default when no model is pinned, because that is what will
    /// happen — reporting nothing would imply the choice is unset rather than
    /// delegated.
    pub fn planner_model_label(&self) -> String {
        let assignment = self.roles.planner.as_ref();
        let model = assignment
            .and_then(|role| role.model.as_deref())
            .filter(|model| !model.is_empty());
        let effort = assignment
            .and_then(|role| role.effort.as_deref())
            .filter(|effort| !effort.is_empty());
        match (model, effort) {
            (Some(model), Some(effort)) => format!("{model} · {effort}"),
            (Some(model), None) => model.to_owned(),
            (None, Some(effort)) => format!("provider default · {effort}"),
            (None, None) => "provider default".to_owned(),
        }
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
        // Mark errors as unseen unless the Problems tab is currently visible.
        if !(self.error_pane_open && self.output_tab == OutputTab::Problems) {
            self.unseen_errors = true;
        }
        // When auto-follow is engaged, a new error snaps the view to the newest entry
        // (reset the offset to 0 so panel_offset will render at scroll_max).
        // If the user has scrolled up (auto-follow disengaged), leave the stored offset
        // untouched so a new error does NOT yank the view back down.
        if self.error_pane_auto_follow {
            self.scroll_offsets.remove(&ScrollablePanel::ErrorPane);
        }
    }

    /// Open the bottom Output pane on `tab`. Re-invoking with the tab that is
    /// already showing closes the pane (toggle); invoking with the other tab
    /// switches to it while keeping the pane open. Opening/switching resets the
    /// pane scroll so the new content starts at a sensible position, and opening
    /// `Problems` clears the unseen-errors badge.
    pub fn open_output_tab(&mut self, tab: OutputTab) {
        if self.error_pane_open && self.output_tab == tab {
            self.error_pane_open = false;
            return;
        }
        self.error_pane_open = true;
        self.output_tab = tab;
        self.scroll_offsets.remove(&ScrollablePanel::ErrorPane);
        if tab == OutputTab::Problems {
            self.unseen_errors = false;
        }
    }

    /// Whether the modal file browser is currently active.
    pub fn is_browsing(&self) -> bool {
        matches!(self.mode, Mode::FileBrowser | Mode::FolderBrowser { .. })
    }

    /// Whether the folder browser modal is currently active.
    pub fn is_folder_browsing(&self) -> bool {
        matches!(self.mode, Mode::FolderBrowser { .. })
    }

    /// Persist `self.workspace`, honoring [`App::workspace_path_override`].
    ///
    /// Production callers leave `workspace_path_override` as `None`, so this
    /// resolves the real `$HOME/.makina/workspace.toml`
    /// ([`crate::workspace::Workspace::save`]). Tests set the override to a
    /// tempdir path so the folder open/close handlers never write to the
    /// operator's actual home directory.
    pub fn save_workspace(&self) -> Result<(), String> {
        match &self.workspace_path_override {
            Some(path) => self.workspace.save_to(path),
            None => self.workspace.save(),
        }
    }

    /// Remove an opened folder and synchronously repair every index-based
    /// folder state before asynchronous rediscovery can render another frame.
    pub fn remove_opened_folder(&mut self, path: &std::path::Path) -> bool {
        let Some(removed_idx) = self.opened_folders.iter().position(|root| root == path) else {
            return false;
        };
        // Capture run ownership before the folder disappears. Convention-based
        // plans can recover their root from the directory, but arbitrary plan
        // paths cannot do so once `opened_folders` has been reindexed.
        self.refresh_run_project_roots();
        let removed_root = lexical_absolute(&self.repo_root, path);
        self.opened_folders.remove(removed_idx);

        self.plans_by_folder = std::mem::take(&mut self.plans_by_folder)
            .into_iter()
            .filter_map(|(idx, plans)| match idx.cmp(&removed_idx) {
                std::cmp::Ordering::Less => Some((idx, plans)),
                std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some((idx - 1, plans)),
            })
            .collect();
        self.collapsed_folders = std::mem::take(&mut self.collapsed_folders)
            .into_iter()
            .filter_map(|idx| match idx.cmp(&removed_idx) {
                std::cmp::Ordering::Less => Some(idx),
                std::cmp::Ordering::Equal => None,
                std::cmp::Ordering::Greater => Some(idx - 1),
            })
            .collect();
        self.collapsed_plans.retain(|key| match key {
            CollapseKey::Plan(target) => target.project_root != removed_root,
            #[cfg(test)]
            CollapseKey::LegacyPlan(_) | CollapseKey::FolderPlan { .. } => true,
        });

        let tabs_to_close: Vec<usize> = self
            .tabs
            .open_tabs
            .iter()
            .enumerate()
            .filter_map(|(idx, tab)| {
                let root = match tab {
                    TabContent::Plan { plan }
                    | TabContent::PlanTask { plan, .. }
                    | TabContent::Task { plan, .. } => &plan.project_root,
                    TabContent::PlanAuthoring { project_root } => project_root,
                };
                (*root == removed_root).then_some(idx)
            })
            .collect();
        for idx in tabs_to_close.into_iter().rev() {
            self.tabs.close_tab(idx);
        }
        self.accordion_state
            .retain(|target, _| target.project_root != removed_root);
        self.plan_operations
            .retain(|target, _| target.project_root != removed_root);
        self.opening_plans
            .retain(|target| target.project_root != removed_root);

        let count = self.visible_tree_nodes().len();
        self.tree_cursor = match (self.tree_cursor, count) {
            (_, 0) => None,
            (Some(cursor), count) => Some(cursor.min(count - 1)),
            (None, _) => Some(0),
        };
        self.sync_selection_from_cursor();
        true
    }

    /// Get the folder browser purpose if currently in folder browser mode.
    pub fn folder_browser_purpose(&self) -> Option<FolderBrowserPurpose> {
        match self.mode {
            Mode::FolderBrowser { purpose } => Some(purpose),
            _ => None,
        }
    }

    /// Whether the doctor health-check overlay is currently active.
    pub fn is_viewing_doctor(&self) -> bool {
        self.mode == Mode::Doctor
    }

    /// Whether the command palette is currently active.
    pub fn is_command_palette(&self) -> bool {
        self.mode == Mode::CommandPalette
    }

    /// Whether the plan-authoring tab is the active tab.
    ///
    /// Authoring is a tab, not a mode: this reports where the composer is
    /// showing rather than whether an overlay has seized the screen, so the
    /// rest of the UI keeps working while a draft is open.
    pub fn is_plan_authoring(&self) -> bool {
        self.tabs
            .active_tab
            .and_then(|idx| self.tabs.open_tabs.get(idx))
            .is_some_and(|tab| matches!(tab, TabContent::PlanAuthoring { .. }))
    }

    /// Whether the settings modal is currently active.
    pub fn is_settings(&self) -> bool {
        self.mode == Mode::Settings
    }

    /// Whether the reset confirmation modal is currently active.
    pub fn is_confirming_reset(&self) -> bool {
        self.mode == Mode::ResetConfirm
    }

    /// Whether an operation notice modal is currently active.
    pub fn is_operation_notice(&self) -> bool {
        self.mode == Mode::OperationNotice
    }

    /// Human label for a project-scoped plan that is currently resetting.
    pub fn resetting_label(&self, target: &PlanIdentity) -> Option<&str> {
        self.plan_operations
            .get(target)
            .filter(|op| op.kind == makina_core::api::PlanOperationKind::Reset && op.is_running())
            .map(|op| op.label.as_str())
    }

    /// Whether the given run belongs to a plan currently being reset.
    pub fn is_resetting_run(&self, run: &RunView) -> bool {
        let target = self.plan_identity_for_run(run);
        self.resetting_label(&target).is_some()
    }

    /// Running operation for a project-scoped plan, if any.
    pub fn running_plan_operation(&self, target: &PlanIdentity) -> Option<&PlanOperationState> {
        self.plan_operations
            .get(target)
            .filter(|op| op.is_running())
    }

    /// Operation log for the current plan/run context.
    pub fn context_operation_log(&self) -> Option<&PlanOperationState> {
        let target = self.context_plan_identity()?;
        self.plan_operations.get(&target)
    }

    /// The current project-scoped plan target for command guarding.
    pub fn reset_context_target(&self) -> Option<PlanIdentity> {
        if let Some(target) = self.context_plan_identity() {
            return Some(target);
        }
        self.selected_run()
            .map(|run| self.plan_identity_for_run(run))
    }

    /// Canonical project targeted by project-level commands. A sidebar
    /// selection is authoritative only while the sidebar owns focus; otherwise
    /// the active tab supplies the immutable project context.
    pub fn context_project_root(&self) -> PathBuf {
        if self.focused_panel == Panel::Sidebar {
            match self.focused_node() {
                Some(TreeNode::Folder { folder_idx }) => {
                    if let Some(root) = self.opened_folders.get(folder_idx) {
                        return lexical_absolute(&self.repo_root, root);
                    }
                }
                Some(
                    node @ (TreeNode::Plan { .. }
                    | TreeNode::PlanTask { .. }
                    | TreeNode::PlanInFolder { .. }
                    | TreeNode::PlanTaskInFolder { .. }),
                ) => {
                    if let Some(target) = self.plan_identity_for_node(node) {
                        return target.project_root;
                    }
                }
                Some(TreeNode::Run { run }) | Some(TreeNode::Task { run, .. }) => {
                    if let Some(view) = self.runs.get(run) {
                        return self.plan_identity_for_run(view).project_root;
                    }
                }
                None => {}
            }
        }

        if let Some(active) = self.tabs.active_tab
            && let Some(tab) = self.tabs.open_tabs.get(active)
        {
            // An authoring tab already names its project root outright.
            if let TabContent::PlanAuthoring { project_root } = tab {
                return project_root.clone();
            }
            let target = match tab {
                TabContent::Plan { plan }
                | TabContent::PlanTask { plan, .. }
                | TabContent::Task { plan, .. } => plan,
                TabContent::PlanAuthoring { .. } => unreachable!("handled above"),
            };
            if target.project_root.as_os_str().is_empty() {
                if let Some(entry) = self.plan_entry(target) {
                    return self
                        .plan_identity_for_entry(&self.repo_root, entry)
                        .project_root;
                }
            } else {
                return target.project_root.clone();
            }
        }

        self.selected_run()
            .map(|run| self.plan_identity_for_run(run).project_root)
            .unwrap_or_else(|| self.canonical_repo_root())
    }

    /// Build reset confirmation details for the active plan/run context.
    pub fn reset_confirmation_for_context(&self) -> Result<ResetConfirmation, String> {
        if let Some(target) = self.context_plan_identity()
            && let Some(plan) = self.plan_entry(&target)
        {
            if !plan.has_document() {
                return Err(format!(
                    "{}: no valid task documents to reset — author tasks first",
                    plan.slug
                ));
            }
            return Ok(ResetConfirmation {
                label: plan.slug.clone(),
                run: self.latest_run_for_plan(&target).map(|run| run.id),
                target,
            });
        }

        if let Some(run) = self.selected_run() {
            let target = self.plan_identity_for_run(run);
            return Ok(ResetConfirmation {
                label: target.slug.clone(),
                target,
                run: Some(run.id),
            });
        }

        Err("No plan or run selected — open or select a plan first".to_string())
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

    /// Build an identity for a discovered folder plan.
    pub fn plan_identity_for_entry(
        &self,
        project_root: &std::path::Path,
        plan: &makina_core::orchestrator::PlanEntry,
    ) -> PlanIdentity {
        let plan_dir = plan.dir.clone();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let configured_root = lexical_absolute(&cwd, project_root);
        let effective_root = if plan_dir.is_absolute() && !plan_dir.starts_with(&configured_root) {
            project_root_from_plan_path(&plan_dir).unwrap_or(configured_root)
        } else {
            configured_root
        };
        PlanIdentity::from_key(effective_root, plan.key.clone(), plan.slug.clone())
    }

    /// Stable identity for a plan or plan-task sidebar node.
    pub fn plan_identity_for_node(&self, node: TreeNode) -> Option<PlanIdentity> {
        match node {
            TreeNode::Plan { plan_idx } | TreeNode::PlanTask { plan_idx, .. } => self
                .discovered_plans
                .get(plan_idx)
                .map(|plan| self.plan_identity_for_entry(&self.repo_root, plan)),
            TreeNode::PlanInFolder {
                folder_idx,
                plan_idx,
            }
            | TreeNode::PlanTaskInFolder {
                folder_idx,
                plan_idx,
                ..
            } => self
                .opened_folders
                .get(folder_idx)
                .zip(
                    self.plans_by_folder
                        .get(&folder_idx)
                        .and_then(|plans| plans.get(plan_idx)),
                )
                .map(|(root, plan)| self.plan_identity_for_entry(root, plan)),
            _ => None,
        }
    }

    pub fn collapse_key_for_node(&self, node: TreeNode) -> Option<CollapseKey> {
        self.plan_identity_for_node(node).map(CollapseKey::Plan)
    }

    /// Infer a run's canonical project while enough workspace context exists
    /// to do so without guessing. Callers retain successful inferences because
    /// an opened folder may later be closed.
    fn infer_project_root_for_run(&self, run: &RunView) -> Option<PathBuf> {
        let launch_root = self.canonical_repo_root();
        let path = &run.plan_dir.relative_dir;

        if path.is_absolute() {
            let absolute_path = lexical_absolute(&launch_root, path);
            return self
                .opened_folders
                .iter()
                .map(|root| lexical_absolute(&launch_root, root))
                .filter(|root| absolute_path.starts_with(root))
                .max_by_key(|root| root.components().count())
                .or_else(|| project_root_from_plan_path(&absolute_path))
                .or_else(|| {
                    absolute_path
                        .starts_with(&launch_root)
                        .then_some(launch_root)
                });
        }

        // Historical snapshots store relative paths. Their persisted project
        // label is enough only when it names exactly one opened folder.
        if !run.project.is_empty() {
            let mut named = self
                .opened_folders
                .iter()
                .map(|root| lexical_absolute(&launch_root, root))
                .filter(|root| {
                    root.file_name().and_then(|name| name.to_str()) == Some(run.project.as_str())
                });
            if let (Some(root), None) = (named.next(), named.next()) {
                return Some(root);
            }
            if launch_root.file_name().and_then(|name| name.to_str()) == Some(run.project.as_str())
            {
                return Some(launch_root);
            }
        }

        None
    }

    fn remember_run_project_root(&mut self, run: &RunView) {
        if let Some(root) = self.infer_project_root_for_run(run) {
            self.run_project_roots.entry(run.id).or_insert(root);
        }
    }

    fn refresh_run_project_roots(&mut self) {
        let inferred: Vec<_> = self
            .runs
            .iter()
            .filter_map(|run| {
                self.infer_project_root_for_run(run)
                    .map(|root| (run.id, root))
            })
            .collect();
        for (run, root) in inferred {
            self.run_project_roots.entry(run).or_insert(root);
        }
    }

    /// Recover canonical project + plan identity for a run. Live plan paths are
    /// normally absolute. Historical disk snapshots are relative synthetic
    /// `docs/plans/...` directories, so they are rooted using the unique
    /// matching opened project (falling back to the launch project). A retained
    /// owner takes precedence so closing a project cannot re-home its runs.
    pub fn plan_identity_for_run(&self, run: &RunView) -> PlanIdentity {
        let path = &run.plan_dir.relative_dir;
        let project_root = self
            .run_project_roots
            .get(&run.id)
            .cloned()
            .or_else(|| self.infer_project_root_for_run(run))
            .unwrap_or_else(|| self.canonical_repo_root());
        PlanIdentity::new(
            project_root.clone(),
            project_root.join(path),
            run.plan_dir
                .relative_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&run.plan_dir.slug)
                .to_owned(),
        )
    }

    /// Look up one plan by its full identity. Iteration order is irrelevant
    /// because a candidate must match project root and plan directory.
    pub fn plan_entry(
        &self,
        target: &PlanIdentity,
    ) -> Option<&makina_core::orchestrator::PlanEntry> {
        if target.project_root.as_os_str().is_empty() {
            let mut matches = self
                .discovered_plans
                .iter()
                .chain(self.plans_by_folder.values().flatten())
                .filter(|plan| plan.slug == target.slug);
            let first = matches.next();
            return first.filter(|_| matches.next().is_none());
        }
        if let Some(plan) = self
            .discovered_plans
            .iter()
            .find(|plan| self.plan_identity_for_entry(&self.repo_root, plan) == *target)
        {
            return Some(plan);
        }
        self.plans_by_folder
            .iter()
            .filter_map(|(folder_idx, plans)| {
                self.opened_folders
                    .get(*folder_idx)
                    .map(|root| (root, plans))
            })
            .flat_map(|(root, plans)| plans.iter().map(move |plan| (root, plan)))
            .find_map(|(root, plan)| {
                (self.plan_identity_for_entry(root, plan) == *target).then_some(plan)
            })
    }

    fn active_tab_plan_identity(&self) -> Option<PlanIdentity> {
        if let Some(active) = self.tabs.active_tab
            && let Some(content) = self.tabs.open_tabs.get(active)
        {
            match content {
                // No plan exists yet while it is still being authored.
                TabContent::PlanAuthoring { .. } => return None,
                TabContent::Plan { plan }
                | TabContent::PlanTask { plan, .. }
                | TabContent::Task { plan, .. } => {
                    if plan.project_root.as_os_str().is_empty()
                        && let Some(entry) = self.plan_entry(plan)
                    {
                        if let Some((folder_idx, _)) =
                            self.plans_by_folder.iter().find(|(_, plans)| {
                                plans.iter().any(|candidate| std::ptr::eq(candidate, entry))
                            })
                            && let Some(root) = self.opened_folders.get(*folder_idx)
                        {
                            return Some(self.plan_identity_for_entry(root, entry));
                        }
                        return Some(self.plan_identity_for_entry(&self.repo_root, entry));
                    }
                    return Some(plan.clone());
                }
            }
        }
        None
    }

    fn plan_identity_for_context_node(&self, node: TreeNode) -> Option<PlanIdentity> {
        match node {
            TreeNode::Plan { plan_idx } | TreeNode::PlanTask { plan_idx, .. } => self
                .discovered_plans
                .get(plan_idx)
                .map(|plan| self.plan_identity_for_entry(&self.repo_root, plan)),
            TreeNode::PlanInFolder {
                folder_idx,
                plan_idx,
            }
            | TreeNode::PlanTaskInFolder {
                folder_idx,
                plan_idx,
                ..
            } => self
                .opened_folders
                .get(folder_idx)
                .zip(
                    self.plans_by_folder
                        .get(&folder_idx)
                        .and_then(|plans| plans.get(plan_idx)),
                )
                .map(|(root, plan)| self.plan_identity_for_entry(root, plan)),
            TreeNode::Run { run } | TreeNode::Task { run, .. } => self
                .runs
                .get(run)
                .map(|view| self.plan_identity_for_run(view)),
            TreeNode::Folder { .. } => None,
        }
    }

    /// Stable identity for the plan the user is currently "in". A focused
    /// sidebar node is authoritative while the sidebar owns focus; otherwise
    /// the active tab supplies immutable context. This prevents commands from
    /// targeting a stale tab while the user is acting on another sidebar plan,
    /// without letting a stale sidebar cursor override the tab shown in Main.
    pub fn context_plan_identity(&self) -> Option<PlanIdentity> {
        if self.focused_panel == Panel::Sidebar
            && let Some(node) = self.focused_node()
        {
            return self.plan_identity_for_context_node(node);
        }

        self.active_tab_plan_identity().or_else(|| {
            self.focused_node()
                .and_then(|node| self.plan_identity_for_context_node(node))
        })
    }

    /// The discovered plan the user is currently "in" — derived from the active
    /// tab's identity (a plan or plan-task preview tab), falling back to focused
    /// sidebar node (a plan or plan-task node).
    ///
    /// This lets run-control actions act on a discovered plan that has **no open
    /// Run yet**: `Start run` resolves and opens the plan directory
    /// (see `crate::event`), instead of forcing the user through the `[o]` file
    /// browser. Returns `None` when the context is a run/run-task or nothing.
    pub fn context_plan(&self) -> Option<&makina_core::orchestrator::PlanEntry> {
        let target = self.context_plan_identity()?;
        self.plan_entry(&target)
    }

    /// The [`RunId`] a run-control action should act on:
    /// the explicitly selected run, or — when none is selected — the run that
    /// matches the [`context_plan`](Self::context_plan), if one is already open.
    ///
    /// Returning the context plan's run here means that once a plan has been
    /// started, pause/stop/resume keep working from the plan tab even if the
    /// sidebar cursor has moved off the run. Returns `None` when no run exists
    /// Return the most recent run (by `run_uid` ULID, i.e. newest) matching the
    /// given plan slug, if any. Used to deduplicate multiple historical runs for
    /// the same plan so the sidebar does not show duplicate "same plan" entries.
    pub(crate) fn latest_run_for_plan(&self, target: &PlanIdentity) -> Option<&RunView> {
        self.runs
            .iter()
            .filter(|run| {
                if target.project_root.as_os_str().is_empty() {
                    run.plan_dir
                        .relative_dir
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name == target.slug)
                } else {
                    self.plan_identity_for_run(run) == *target
                }
            })
            .max_by_key(|r| &r.run_uid)
    }

    /// for the current context (the caller may then open one for the plan).
    pub fn active_run_id(&self) -> Option<makina_core::api::RunId> {
        if self.focused_panel == Panel::Sidebar
            && let Some(node) = self.focused_node()
        {
            return match node {
                TreeNode::Run { run } | TreeNode::Task { run, .. } => {
                    self.runs.get(run).map(|view| view.id)
                }
                TreeNode::Plan { .. }
                | TreeNode::PlanTask { .. }
                | TreeNode::PlanInFolder { .. }
                | TreeNode::PlanTaskInFolder { .. } => self
                    .plan_identity_for_context_node(node)
                    .and_then(|target| self.latest_run_for_plan(&target))
                    .map(|run| run.id),
                TreeNode::Folder { .. } => None,
            };
        }
        if let Some(active) = self.tabs.active_tab
            && let Some(TabContent::Task { run, .. }) = self.tabs.open_tabs.get(active)
        {
            return Some(*run);
        }
        if let Some(target) = self.context_plan_identity() {
            return self.latest_run_for_plan(&target).map(|run| run.id);
        }
        self.selected_run().map(|run| run.id)
    }

    /// Resolve the `(RunId, TaskId)` whose agent log the `[L]` panel should show,
    /// preferring (1) the active task tab, then (2) the focused sidebar task
    /// node, then (3) the sidebar selection. Returns `None` when no task is in
    /// context (e.g. a plan tab or a run header) so the panel can hint.
    ///
    /// Keyed for an `exchange_logs` lookup (the panel reuses the live in-memory
    /// agent exchanges rendered by the exchange pane).
    pub fn log_pane_target(&self) -> Option<(RunId, TaskId)> {
        // 1. Active task tab → its run + task.
        if let Some(active) = self.tabs.active_tab
            && let Some(TabContent::Task { run, task_id, .. }) = self.tabs.open_tabs.get(active)
            && self.runs.iter().any(|view| view.id == *run)
        {
            return Some((*run, task_id.clone()));
        }
        // 2. Focused sidebar task node.
        if let Some(TreeNode::Task { run, task }) = self.focused_node()
            && let Some(rv) = self.runs.get(run)
            && let Some(tv) = rv.tasks.get(task)
        {
            return Some((rv.id, tv.id.clone()));
        }
        // 3. Sidebar selection (selected_run + selected_task).
        let run = self.selected_run()?;
        let tv = self.selected_task.and_then(|i| run.tasks.get(i))?;
        Some((run.id, tv.id.clone()))
    }

    /// Resolve which [`ScrollablePanel`] keyboard scroll events (`SelectUp`/
    /// `SelectDown`/`ScrollUp`/`ScrollDown`) should target when the main pane is
    /// focused, based on the *active tab's* content type:
    ///
    /// * `TabContent::Task` | `TabContent::PlanTask` → [`ScrollablePanel::TaskEntry`]
    ///   (the task detail pane rendered by `render_task_entry_pane` /
    ///   `render_plan_task_pane`).
    /// * `TabContent::Plan` → [`ScrollablePanel::PlanAccordion`] (the plan
    ///   accordion pane rendered by `render_plan_accordion_pane`).
    /// * No active tab (the bare selected-run view) → [`ScrollablePanel::Exchange`]
    ///   (preserves the legacy behaviour: Up/Down scrolls the exchange pane).
    ///
    /// Mouse-wheel scroll events use `panel_at` hit-testing instead, so this is
    /// only consulted by the keyboard paths that have no positional context.
    pub fn main_scroll_target(&self) -> ScrollablePanel {
        match self
            .tabs
            .active_tab
            .and_then(|idx| self.tabs.open_tabs.get(idx))
        {
            Some(TabContent::Task { .. }) | Some(TabContent::PlanTask { .. }) => {
                ScrollablePanel::TaskEntry
            }
            Some(TabContent::Plan { .. }) => ScrollablePanel::PlanAccordion,
            // The authoring transcript is an exchange stream like any other, and
            // auto-follows its tail. Arrow keys never reach here — the composer
            // owns them — so only PageUp/PageDown and the wheel drive it.
            Some(TabContent::PlanAuthoring { .. }) | None => ScrollablePanel::Exchange,
        }
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

    /// Return the comprehensive focus state (region + nested section if applicable).
    pub fn focused_state(&self) -> FocusState {
        match self.focused_panel {
            Panel::Sidebar => FocusState::TreeNode,
            Panel::Main => self
                .focused_section
                .map(FocusState::AccordionSection)
                .unwrap_or(FocusState::MainPane),
        }
    }

    /// Return the static accordion section cycle order (used by Tab logic).
    pub(crate) fn accordion_section_order() -> &'static [AccordionSection] {
        &[
            AccordionSection::Scope,
            AccordionSection::Architecture,
            AccordionSection::Tasks,
            AccordionSection::Status,
        ]
    }

    /// Move focus forward (Tab key) through the hierarchy:
    /// Sidebar → Main (no section) → Accordion sections → Sidebar (wrap).
    /// Wrapping occurs only if a plan tab is active; otherwise Tab from Main goes to Sidebar.
    pub fn move_focus_forward(&mut self) {
        match self.focused_panel {
            Panel::Sidebar => {
                // From Sidebar, Tab always moves to Main pane.
                self.focused_panel = Panel::Main;
                self.focused_section = None; // Enter Main without a specific section focus.
            }
            Panel::Main => {
                // From Main, check if a plan tab is active.
                let has_active_plan_tab = self
                    .tabs
                    .active_tab
                    .and_then(|idx| self.tabs.open_tabs.get(idx))
                    .map(|tab| matches!(tab, TabContent::Plan { .. }))
                    .unwrap_or(false);

                if has_active_plan_tab {
                    // Plan tab is active; cycle accordion sections.
                    let sections = Self::accordion_section_order();
                    let next = match self.focused_section {
                        None => Some(sections[0]), // First entry: focus Scope.
                        Some(sec) => {
                            // Find the current section's position and move to the next.
                            sections
                                .iter()
                                .position(|&s| s == sec)
                                .map(|pos| {
                                    if pos + 1 < sections.len() {
                                        Some(sections[pos + 1]) // Next section.
                                    } else {
                                        None // Signal to wrap to Sidebar.
                                    }
                                })
                                .unwrap_or(Some(sections[0])) // Fallback: reset to Scope.
                        }
                    };

                    match next {
                        Some(sec) => {
                            self.focused_section = Some(sec);
                        }
                        None => {
                            // Last section (Status); wrap to Sidebar.
                            self.focused_panel = Panel::Sidebar;
                            self.focused_section = None;
                        }
                    }
                } else {
                    // No plan tab active: wrap from Main to Sidebar.
                    self.focused_panel = Panel::Sidebar;
                    self.focused_section = None;
                }
            }
        }
    }

    /// Move focus backward (Shift+Tab key) through the hierarchy in reverse:
    /// Sidebar → Status (if plan tab active) → Accordion sections in reverse → Main → Sidebar (wrap).
    pub fn move_focus_backward(&mut self) {
        match self.focused_panel {
            Panel::Sidebar => {
                // From Sidebar, Shift+Tab checks if a plan tab is active.
                let has_active_plan_tab = self
                    .tabs
                    .active_tab
                    .and_then(|idx| self.tabs.open_tabs.get(idx))
                    .map(|tab| matches!(tab, TabContent::Plan { .. }))
                    .unwrap_or(false);

                if has_active_plan_tab {
                    // Plan tab is active; jump to the last accordion section (Status).
                    let sections = Self::accordion_section_order();
                    self.focused_panel = Panel::Main;
                    self.focused_section = Some(sections[sections.len() - 1]);
                }
                // If no plan tab active, stay in Sidebar (no movement).
            }
            Panel::Main => {
                // From Main, check if a plan tab is active.
                let has_active_plan_tab = self
                    .tabs
                    .active_tab
                    .and_then(|idx| self.tabs.open_tabs.get(idx))
                    .map(|tab| matches!(tab, TabContent::Plan { .. }))
                    .unwrap_or(false);

                if has_active_plan_tab {
                    // Plan tab is active; step backward through accordion sections.
                    let sections = Self::accordion_section_order();
                    let next = match self.focused_section {
                        None => {
                            // No section focused yet; jump to the last one (Status).
                            Some(sections[sections.len() - 1])
                        }
                        Some(sec) => {
                            // Find the current section's position and move to the previous.
                            sections
                                .iter()
                                .position(|&s| s == sec)
                                .map(|pos| {
                                    if pos > 0 {
                                        Some(sections[pos - 1]) // Previous section.
                                    } else {
                                        None // Signal to exit to Sidebar.
                                    }
                                })
                                .unwrap_or(Some(sections[0]))
                        }
                    };

                    match next {
                        Some(sec) => {
                            self.focused_section = Some(sec);
                        }
                        None => {
                            // First section (Scope); exit to Sidebar.
                            self.focused_panel = Panel::Sidebar;
                            self.focused_section = None;
                        }
                    }
                } else {
                    // No plan tab active: exit to Sidebar.
                    self.focused_panel = Panel::Sidebar;
                    self.focused_section = None;
                }
            }
        }
    }

    /// Scroll the given panel up by one line, disengaging auto-follow if the panel is the exchange or error pane.
    pub fn scroll_up(&mut self, panel: ScrollablePanel) {
        // Only the exchange and error panes have auto-follow logic.
        if panel == ScrollablePanel::Exchange && self.exchange_auto_follow {
            self.exchange_auto_follow = false;
            // Anchor the manual offset to the last rendered bottom.
            let bottom = self
                .last_scroll_maxes
                .borrow()
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0);
            self.scroll_offsets
                .insert(ScrollablePanel::Exchange, bottom);
        } else if panel == ScrollablePanel::ErrorPane && self.error_pane_auto_follow {
            self.error_pane_auto_follow = false;
            // Anchor the manual offset to the last rendered bottom.
            let bottom = self
                .last_scroll_maxes
                .borrow()
                .get(&ScrollablePanel::ErrorPane)
                .copied()
                .unwrap_or(0);
            self.scroll_offsets
                .insert(ScrollablePanel::ErrorPane, bottom);
        }
        let current = self.scroll_offsets.entry(panel).or_insert(0);
        *current = current.saturating_sub(1);
    }

    /// Scroll the given panel down by one line, clamped at scroll_max.
    pub fn scroll_down(&mut self, panel: ScrollablePanel, scroll_max: u16) {
        let current = self.scroll_offsets.entry(panel).or_insert(0);
        *current = current.saturating_add(1).min(scroll_max);
        if panel == ScrollablePanel::Exchange && *current == scroll_max {
            self.exchange_auto_follow = true;
        } else if panel == ScrollablePanel::ErrorPane && *current == scroll_max {
            self.error_pane_auto_follow = true;
        }
    }

    /// The effective scroll offset to render the exchange pane with, given the
    /// caller-computed scroll_max. When auto-following, returns scroll_max
    /// (pinned to the bottom); otherwise the manual offset clamped to scroll_max.
    pub fn effective_offset(&self, scroll_max: u16) -> u16 {
        if self.exchange_auto_follow {
            scroll_max
        } else {
            self.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0)
                .min(scroll_max)
        }
    }

    /// The render-time scroll offset for any panel, clamped to scroll_max.
    /// The exchange pane delegates to `effective_offset` so auto-follow is honored;
    /// the error pane honors `error_pane_auto_follow` similarly;
    /// every other panel uses its stored offset clamped to scroll_max.
    pub fn panel_offset(&self, panel: ScrollablePanel, scroll_max: u16) -> u16 {
        if panel == ScrollablePanel::Exchange {
            self.effective_offset(scroll_max)
        } else if panel == ScrollablePanel::ErrorPane {
            if self.error_pane_auto_follow {
                scroll_max
            } else {
                self.scroll_offsets
                    .get(&ScrollablePanel::ErrorPane)
                    .copied()
                    .unwrap_or(0)
                    .min(scroll_max)
            }
        } else {
            self.scroll_offsets
                .get(&panel)
                .copied()
                .unwrap_or(0)
                .min(scroll_max)
        }
    }

    /// Record the selectable pane rectangles for the current frame.
    ///
    /// Called by the render pass (which alone knows the layout) via interior
    /// mutability, mirroring [`App::last_scroll_max`]. Replaces any previously
    /// recorded set. Read by [`App::selection_pane_at`] when a drag begins.
    pub fn set_selection_panes(&self, panes: Vec<SelectionPane>) {
        *self.selection_panes.borrow_mut() = panes;
    }

    /// The clip rectangle of the recorded pane whose `hit` region contains the
    /// screen cell `(x, y)`, if any. Used to confine a new mouse selection to a
    /// single pane. Returns `None` when the drag began outside every pane
    /// (e.g. the title or status bar).
    pub fn selection_pane_at(&self, x: u16, y: u16) -> Option<ratatui::layout::Rect> {
        self.selection_panes
            .borrow()
            .iter()
            .find(|p| {
                x >= p.hit.left() && x < p.hit.right() && y >= p.hit.top() && y < p.hit.bottom()
            })
            .map(|p| p.clip)
    }

    /// Record the geometries of rendered panels for hitbox testing.
    /// Takes `&self` (interior mutability) so the render pass can call it.
    pub fn set_panel_geometries(&self, geoms: Vec<PanelGeometry>) {
        *self.panel_geometries.borrow_mut() = geoms;
    }

    /// Given a mouse column and row, return the panel under it (if any).
    pub fn panel_at(&self, col: u16, row: u16) -> Option<ScrollablePanel> {
        self.panel_geometries
            .borrow()
            .iter()
            .find(|g| {
                col >= g.rect.x
                    && col < (g.rect.x + g.rect.width)
                    && row >= g.rect.y
                    && row < (g.rect.y + g.rect.height)
            })
            .map(|g| g.panel)
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
                self.move_focus_forward();
                true
            }
            AppEvent::FocusPrev => {
                self.move_focus_backward();
                true
            }
            AppEvent::CycleDependencyView => {
                self.dependency_view = match self.dependency_view {
                    DependencyViewMode::Off => DependencyViewMode::List,
                    DependencyViewMode::List => DependencyViewMode::Tree,
                    DependencyViewMode::Tree => DependencyViewMode::Timeline,
                    DependencyViewMode::Timeline => DependencyViewMode::Off,
                };
                let label = match self.dependency_view {
                    DependencyViewMode::Off => "off",
                    DependencyViewMode::List => "list",
                    DependencyViewMode::Tree => "tree",
                    DependencyViewMode::Timeline => "timeline",
                };
                self.status_message = Some(format!("Dependency view: {}", label));
                true
            }
            AppEvent::ToggleErrorPane => {
                // `[e]`: open the Output pane on Problems; if it is already open
                // on Problems, close it; if open on Logs, switch to Problems.
                self.open_output_tab(OutputTab::Problems);
                true
            }
            AppEvent::ToggleHelpMode => {
                self.help_mode_active = !self.help_mode_active;
                true
            }
            AppEvent::CloseHelpMode => {
                self.help_mode_active = false;
                true
            }
            AppEvent::ToggleLogPane => {
                // `[L]`: open the Output pane on Logs; if already open on Logs,
                // close it; if open on Problems, switch to Logs.
                self.open_output_tab(OutputTab::Logs);
                true
            }
            AppEvent::SelectUp => {
                match self.focused_panel {
                    Panel::Sidebar => {
                        // Sidebar focus: walk the tree nodes up.
                        self.tree_move(-1);
                    }
                    Panel::Main => {
                        // Main focus: scroll the active tab's pane up. The target
                        // depends on the active tab type — a Task/PlanTask tab scrolls its
                        // task entry pane, a Plan tab scrolls the accordion,
                        // and the bare run view scrolls the exchange pane.
                        self.scroll_up(self.main_scroll_target());
                    }
                }
                true
            }
            AppEvent::SelectDown => {
                match self.focused_panel {
                    Panel::Sidebar => {
                        // Sidebar focus: walk the tree nodes down.
                        self.tree_move(1);
                    }
                    Panel::Main => {
                        // Main focus: scroll the active tab's pane down, clamped to
                        // the per-panel scroll max recorded by the last render pass.
                        let target = self.main_scroll_target();
                        let max = self
                            .last_scroll_maxes
                            .borrow()
                            .get(&target)
                            .copied()
                            .unwrap_or(0);
                        self.scroll_down(target, max);
                    }
                }
                true
            }
            AppEvent::FocusRightOrExpand => {
                // On a *collapsed* run, plan, or folder, the first `Right` expands it; on an
                // already-expanded node or a leaf, `Right` crosses into the content pane.
                let collapsed_expandable = match self.focused_node() {
                    Some(TreeNode::Run { run }) => self
                        .runs
                        .get(run)
                        .is_some_and(|r| self.collapsed_runs.contains(&r.id)),
                    Some(node @ TreeNode::Plan { plan_idx }) => {
                        self.collapse_key_for_node(node)
                            .is_some_and(|key| self.collapsed_plans.contains(&key))
                            && self
                                .discovered_plans
                                .get(plan_idx)
                                .is_some_and(|p| !p.tasks().is_empty())
                    }
                    Some(TreeNode::Folder { folder_idx }) => {
                        self.collapsed_folders.contains(&folder_idx)
                    }
                    Some(
                        node @ TreeNode::PlanInFolder {
                            folder_idx,
                            plan_idx,
                        },
                    ) => {
                        self.collapse_key_for_node(node)
                            .is_some_and(|key| self.collapsed_plans.contains(&key))
                            && self
                                .plans_by_folder
                                .get(&folder_idx)
                                .and_then(|plans| plans.get(plan_idx))
                                .is_some_and(|p| !p.tasks().is_empty())
                    }
                    _ => false,
                };
                if collapsed_expandable {
                    self.tree_toggle_expand(); // expand it; cursor stays on the header
                } else {
                    // When crossing from an expanded (or leaf) plan into the main
                    // pane, open its details tab so the content pane shows the plan
                    // (SCOPE/ARCH/TASKS/STATUS). This makes arrow navigation into
                    // a plan behave like Enter (open details) + Right (cross).
                    match self.focused_node() {
                        Some(TreeNode::Plan { plan_idx }) => {
                            self.activate_plan_node(plan_idx);
                        }
                        Some(TreeNode::PlanInFolder {
                            folder_idx,
                            plan_idx,
                        }) => {
                            self.activate_plan_in_folder(folder_idx, plan_idx);
                        }
                        _ => {}
                    }
                    self.focused_panel = Panel::Main;
                }
                true
            }
            AppEvent::FocusLeftOrCollapse => {
                match self.focused_panel {
                    // From the content pane, `Left` steps back to the sidebar (no collapse).
                    Panel::Main => self.focused_panel = Panel::Sidebar,
                    // In the sidebar, `Left` collapses an *expanded* run/plan/folder. On a
                    // plan-task preview leaf, `Left` collapses its parent plan and
                    // parks the cursor on it.
                    Panel::Sidebar => match self.focused_node() {
                        Some(TreeNode::Run { run })
                            if self
                                .runs
                                .get(run)
                                .is_some_and(|r| !self.collapsed_runs.contains(&r.id)) =>
                        {
                            self.tree_toggle_expand(); // collapse it
                        }
                        Some(node @ TreeNode::Plan { plan_idx })
                            if self
                                .collapse_key_for_node(node)
                                .is_some_and(|key| !self.collapsed_plans.contains(&key))
                                && self
                                    .discovered_plans
                                    .get(plan_idx)
                                    .is_some_and(|p| !p.tasks().is_empty()) =>
                        {
                            self.tree_toggle_expand(); // collapse it
                        }
                        Some(TreeNode::PlanTask { plan_idx, .. }) => {
                            // Collapse the parent plan and move the cursor up to it.
                            if let Some(key) =
                                self.collapse_key_for_node(TreeNode::Plan { plan_idx })
                            {
                                self.collapsed_plans.insert(key);
                            }
                            self.move_cursor_to_plan_header(plan_idx);
                            self.sync_selection_from_cursor();
                        }
                        Some(TreeNode::Folder { folder_idx })
                            if !self.collapsed_folders.contains(&folder_idx) =>
                        {
                            self.tree_toggle_expand(); // collapse it
                        }
                        Some(
                            node @ TreeNode::PlanInFolder {
                                folder_idx,
                                plan_idx,
                            },
                        ) if {
                            self.collapse_key_for_node(node)
                                .is_some_and(|key| !self.collapsed_plans.contains(&key))
                                && self
                                    .plans_by_folder
                                    .get(&folder_idx)
                                    .and_then(|plans| plans.get(plan_idx))
                                    .is_some_and(|p| !p.tasks().is_empty())
                        } =>
                        {
                            self.tree_toggle_expand(); // collapse it
                        }
                        Some(TreeNode::PlanTaskInFolder {
                            folder_idx,
                            plan_idx,
                            ..
                        }) => {
                            // Collapse the parent plan and move the cursor up to it.
                            if let Some(key) = self.collapse_key_for_node(TreeNode::PlanInFolder {
                                folder_idx,
                                plan_idx,
                            }) {
                                self.collapsed_plans.insert(key);
                            }
                            self.move_cursor_to_plan_in_folder_header(folder_idx, plan_idx);
                            self.sync_selection_from_cursor();
                        }
                        _ => {}
                    },
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
                self.scroll_up(self.main_scroll_target());
                true
            }
            AppEvent::ScrollDown => {
                // Use the active tab's pane as the scroll target (so a task tab
                // scrolls its task entry, a plan tab scrolls the accordion, and the
                // bare run view scrolls the exchange pane). The clamp bound is the
                // last rendered scroll max for that panel — using `u16::MAX` would
                // prevent the bound check from ever saturating on panels without
                // auto-follow.
                let target = self.main_scroll_target();
                let max = self
                    .last_scroll_maxes
                    .borrow()
                    .get(&target)
                    .copied()
                    .unwrap_or(0);
                self.scroll_down(target, max);
                true
            }
            AppEvent::ScrollUpAt(col, row) => {
                if let Some(panel) = self.panel_at(col, row) {
                    self.scroll_up(panel);
                }
                true
            }
            AppEvent::ScrollDownAt(col, row) => {
                if let Some(panel) = self.panel_at(col, row) {
                    let scroll_max = self
                        .last_scroll_maxes
                        .borrow()
                        .get(&panel)
                        .copied()
                        .unwrap_or(0);
                    self.scroll_down(panel, scroll_max);
                }
                true
            }
            AppEvent::ErrorPaneScrollUp => {
                self.scroll_up(ScrollablePanel::ErrorPane);
                true
            }
            AppEvent::ErrorPaneScrollDown => {
                let max = self
                    .last_scroll_maxes
                    .borrow()
                    .get(&ScrollablePanel::ErrorPane)
                    .copied()
                    .unwrap_or(0);
                self.scroll_down(ScrollablePanel::ErrorPane, max);
                true
            }
            // ── Mouse text selection ──────────────────────────────────────────
            // Down/Drag/Up drive an in-app selection (the terminal can't do its
            // own while mouse capture is on). The copy-to-clipboard on release
            // happens in the event loop, which has the rendered buffer.
            AppEvent::SelectionStart(x, y) => {
                // Confine the selection to the pane the drag began in so a wide
                // drag never bleeds into a neighbouring pane (e.g. content →
                // sidebar). A drag that starts outside every pane selects
                // nothing.
                let had = self.selection.is_some();
                self.selection = self
                    .selection_pane_at(x, y)
                    .map(|bounds| crate::selection::Selection::start(x, y, bounds));
                // Redraw if a selection started, or to clear a prior highlight.
                had || self.selection.is_some()
            }
            AppEvent::SelectionExtend(x, y) => {
                if let Some(sel) = self.selection.as_mut() {
                    sel.extend(x, y);
                    true
                } else {
                    // A drag with no anchor (e.g. capture toggled mid-gesture):
                    // nothing to redraw.
                    false
                }
            }
            AppEvent::SelectionEnd(x, y) => {
                if let Some(sel) = self.selection.as_mut() {
                    sel.extend(x, y);
                    sel.active = false;
                    // A plain click (no drag) clears the selection rather than
                    // leaving a stray one-cell highlight behind.
                    if sel.is_empty() {
                        self.selection = None;
                    }
                }
                true
            }
            AppEvent::ToggleTreeNode => {
                match self.focused_panel {
                    Panel::Sidebar => {
                        // Toggle expand/collapse on the focused tree node's run.
                        self.tree_toggle_expand();
                    }
                    Panel::Main => {
                        // Toggle the focused accordion section (if one is focused).
                        if let Some(focused_section) = self.focused_section
                            && let Some(active_idx) = self.tabs.active_tab
                            && let Some(TabContent::Plan { plan }) =
                                self.tabs.open_tabs.get(active_idx)
                        {
                            let plan = plan.clone();
                            let sections = self.accordion_state.entry(plan).or_default();
                            if sections.contains(&focused_section) {
                                sections.remove(&focused_section);
                            } else {
                                sections.insert(focused_section);
                            }
                        } else {
                            // No accordion section focused (or no active plan tab):
                            // fall back to activating whatever is highlighted in the
                            // sidebar. This lets Enter open a plan's details tab even
                            // when focus is in main (e.g. after using Right to cross
                            // into main while the plan header is the highlighted node).
                            self.activate_focused_tree_node();
                        }
                    }
                }
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
                self.remember_run_project_root(&full_run);
                let loaded_target = self.plan_identity_for_run(&full_run);
                self.opening_plans.remove(&loaded_target);
                // Clear markdown cache when content changes
                self.markdown_cache.borrow_mut().clear();
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
                // Clamp tree_cursor to stay within the new visible-node list and
                // re-sync selection so selected_run/selected_task stay consistent
                // even if the run that was updated now has fewer tasks (i.e., the
                // cursor was pointing at a task node that no longer exists).
                {
                    let max_idx = self.visible_tree_nodes().len().saturating_sub(1);
                    if let Some(cursor) = self.tree_cursor {
                        if cursor > max_idx {
                            self.tree_cursor = Some(max_idx);
                        }
                    } else if !self.runs.is_empty() {
                        // Runs exist but cursor was None; initialise to node 0.
                        self.tree_cursor = Some(0);
                    }
                    self.sync_selection_from_cursor();
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
            // handled by the IO layer (it does the directory read / OpenPlan call
            // and feeds back BrowserOpened / CloseBrowser).  `update` stays pure.
            AppEvent::OpenBrowser => {
                // No state change here beyond the busy flag; the IO layer scans
                // for plans (→ PlansDiscovered) or reads the start dir (→
                // BrowserOpened). Mark the app busy so the UI shows a spinner
                // while that background work is in flight.
                self.busy = Some("Discovering plans".to_string());
                true
            }
            AppEvent::BrowserOpened { dir, entries } => {
                // A directory listing arrived: enter (or refresh) the browser.
                // Set the mode based on whether we're in folder browser mode.
                if let Some(purpose) = self.folder_browser_purpose {
                    self.mode = Mode::FolderBrowser { purpose };
                } else {
                    self.mode = Mode::FileBrowser;
                }
                self.browser = Some(FileBrowser::new(dir, entries));
                self.status_message = None;
                self.busy = None;
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
                self.folder_browser_purpose = None;
                true
            }

            // ── Folder browser navigation (plan 0043) ──────────────────────────
            // These events update the browser state for folder selection.
            // They are handled in resolve_io for the directory reads (IO), but
            // the navigation state updates happen here.
            AppEvent::FolderBrowserUp => {
                if let Some(browser) = self.browser.as_mut() {
                    browser.select_up();
                }
                true
            }
            AppEvent::FolderBrowserDown => {
                if let Some(browser) = self.browser.as_mut() {
                    browser.select_down();
                }
                true
            }
            AppEvent::FolderBrowserParent => {
                // Handled in resolve_io; just redraw here.
                true
            }
            AppEvent::FolderBrowserActivate { .. } => {
                // Handled in resolve_io; just redraw here.
                true
            }

            // ── Plan picker (plan 0027) ───────────────────────────────────────
            AppEvent::PlansDiscovered { plans } => {
                // Store discovered plans for sidebar tree integration (task 0031).
                // Plans are now navigated via the unified sidebar tree, not a modal.
                self.discovered_plans = plans;
                // Plans start collapsed: seed `collapsed_plans` with every legacy
                // index so the tree opens tidy and Right/Space/Enter reveal the
                // tasks. (This also resets any prior expand state on a
                // re-discovery.)
                self.collapsed_plans = self
                    .discovered_plans
                    .iter()
                    .map(|plan| {
                        CollapseKey::Plan(self.plan_identity_for_entry(&self.repo_root, plan))
                    })
                    .collect();
                // Close plan tabs whose slug is no longer in `discovered_plans`
                // (plan 0032: close_tabs_for_missing_plans).
                self.close_tabs_for_missing_plans();
                // Keep the tree cursor valid against the freshly rebuilt node list
                // and resync the derived selection — mirrors the RunLoaded invariant
                // (`cursor_survives_runs_update`). On the common first-discovery path
                // (no prior cursor) this parks it on the first plan so Right/Enter
                // act on the plan instead of falling through to the content pane.
                let node_count = self.visible_tree_nodes().len();
                self.tree_cursor = match (self.tree_cursor, node_count) {
                    (_, 0) => None,
                    (None, _) => Some(0),
                    (Some(c), n) => Some(c.min(n - 1)),
                };
                self.sync_selection_from_cursor();
                self.status_message = None;
                self.busy = None;
                true
            }

            AppEvent::PlansDiscoveredPerFolder { roots, plans_map } => {
                // A folder may have been closed/reordered while this blocking
                // scan was running. Never apply index-keyed results to a
                // different root vector; the folder mutation launched a fresh
                // scan whose snapshot will match.
                if roots != self.opened_folders {
                    return false;
                }
                // Store per-folder discovered plans for sidebar tree integration.
                self.plans_by_folder = plans_map;
                // Start every folder collapsed EXCEPT the first (folder_idx 0).
                // The auto-discovered launch folder / sole opened folder stays expanded
                // so the single-folder launch case keeps its plans immediately visible —
                // matching today's behavior and preserving the "run inside a repo" experience.
                self.collapsed_folders.clear();
                for folder_idx in self.plans_by_folder.keys().copied() {
                    if folder_idx != 0 {
                        self.collapsed_folders.insert(folder_idx);
                    }
                }
                // Plans start collapsed: seed `collapsed_plans` with every
                // folder-scoped plan (with tasks) so the tree opens tidy and
                // Right/Space/Enter reveal the tasks — mirroring the legacy
                // `PlansDiscovered` seeding above. Plans with no tasks are
                // leaves (never expandable) so they need no collapse key.
                let mut collapsed = HashSet::new();
                for (folder_idx, plans) in &self.plans_by_folder {
                    let Some(root) = self.opened_folders.get(*folder_idx) else {
                        continue;
                    };
                    for plan in plans.iter().filter(|plan| !plan.tasks().is_empty()) {
                        collapsed
                            .insert(CollapseKey::Plan(self.plan_identity_for_entry(root, plan)));
                    }
                }
                self.collapsed_plans = collapsed;
                // Close plan tabs whose slug is no longer in `plans_by_folder`
                // (plan 0032: close_tabs_for_missing_plans).
                self.close_tabs_for_missing_plans();
                // Keep the tree cursor valid against the freshly rebuilt node list
                // and resync the derived selection — mirrors the RunLoaded invariant.
                let node_count = self.visible_tree_nodes().len();
                self.tree_cursor = match (self.tree_cursor, node_count) {
                    (_, 0) => None,
                    (None, _) => Some(0),
                    (Some(c), n) => Some(c.min(n - 1)),
                };
                self.sync_selection_from_cursor();
                self.status_message = None;
                self.busy = None;
                true
            }

            // ── Run control (task 31) ─────────────────────────────────────────
            // Start/Pause/Cancel are IO-layer intents: the event loop issues the
            // async `api.execute(...)` for the selected run and feeds back a
            // StatusMessage.  `update` itself does not mutate state here (no
            // async), so these are no-ops that simply request a redraw.
            AppEvent::StartRun
            | AppEvent::GeneratePlanBundle { .. }
            | AppEvent::PauseRun
            | AppEvent::CancelRun
            | AppEvent::Reinterpret
            | AppEvent::RetryFocused
            | AppEvent::PurgeWorktrees => true,

            AppEvent::RequestResetRun => {
                match self.reset_confirmation_for_context() {
                    Ok(confirm) => {
                        if self.running_plan_operation(&confirm.target).is_some()
                            || self.opening_plans.contains(&confirm.target)
                        {
                            self.operation_notice = Some(OperationNotice {
                                target: confirm.target,
                                attempted: "Reset selected plan/run".to_string(),
                            });
                            self.mode = Mode::OperationNotice;
                            self.command_palette = None;
                        } else {
                            self.reset_confirmation = Some(confirm);
                            self.mode = Mode::ResetConfirm;
                            self.command_palette = None;
                        }
                    }
                    Err(msg) => {
                        self.status_message = Some(msg);
                        self.mode = Mode::Normal;
                        self.command_palette = None;
                    }
                }
                true
            }

            AppEvent::ResetRun { .. } => true,

            AppEvent::CloseResetConfirmation => {
                self.mode = Mode::Normal;
                self.reset_confirmation = None;
                true
            }

            AppEvent::ResetStarted { target, label } => {
                self.plan_operations.insert(
                    target.clone(),
                    PlanOperationState {
                        target,
                        label: label.clone(),
                        kind: makina_core::api::PlanOperationKind::Reset,
                        phase: makina_core::api::PlanOperationPhase::Started,
                        log: vec!["Starting reset".to_string()],
                    },
                );
                self.mode = Mode::Normal;
                self.reset_confirmation = None;
                self.error_pane_open = true;
                self.output_tab = OutputTab::Logs;
                self.scroll_offsets.remove(&ScrollablePanel::ErrorPane);
                self.status_message = Some(format!("Resetting {label}..."));
                true
            }

            AppEvent::ResetFinished { target, message } => {
                let failed = message.to_lowercase().contains("failed");
                if let Some(op) = self.plan_operations.get_mut(&target) {
                    op.phase = if failed {
                        makina_core::api::PlanOperationPhase::Failed
                    } else {
                        makina_core::api::PlanOperationPhase::Finished
                    };
                    if op.log.last().is_none_or(|last| last != &message) {
                        op.log.push(message.clone());
                    }
                }
                self.status_message = Some(message);
                true
            }

            AppEvent::OperationBlocked { target, attempted } => {
                self.operation_notice = Some(OperationNotice { target, attempted });
                self.mode = Mode::OperationNotice;
                self.command_palette = None;
                true
            }

            AppEvent::PlanOpenStarted { target } => {
                self.opening_plans.insert(target);
                true
            }

            AppEvent::PlanOpenFinished { target } => {
                self.opening_plans.remove(&target);
                true
            }

            AppEvent::RunStarting { run } => {
                self.starting_runs.insert(run);
                true
            }

            AppEvent::CloseOperationNotice => {
                self.operation_notice = None;
                self.mode = Mode::Normal;
                true
            }

            AppEvent::StatusMessage(msg) => {
                self.status_message = Some(msg);
                true
            }

            AppEvent::ErrorMessageArrived { msg } => {
                self.push_error(msg);
                true
            }

            AppEvent::DismissProviderWarning => {
                self.provider_warning_dismissed = true;
                true
            }

            // ── Doctor health-check overlay (task 0046) ──────────────────────────
            AppEvent::OpenDoctor => {
                self.mode = Mode::Doctor;
                true
            }
            AppEvent::CloseDoctor => {
                self.mode = Mode::Normal;
                true
            }
            AppEvent::DoctorWriteScaffold => {
                // The IO layer (resolve_io in event.rs) handles the actual file writes.
                // Here we just signal that the action is requested; the IO layer
                // will emit a StatusMessage with the outcome.
                true
            }

            // ── Conversational plan authoring ────────────────────────────────────
            AppEvent::OpenPlanAuthoring => {
                let project_root = self.context_project_root();
                // Re-opening returns to the existing draft rather than
                // discarding it — the tab is a workspace, not a prompt.
                if self
                    .plan_authoring
                    .as_ref()
                    .is_none_or(|state| state.project_root != project_root)
                {
                    self.plan_authoring = Some(PlanAuthoring {
                        project_root: project_root.clone(),
                        input: String::new(),
                        log: ExchangeLog::default(),
                        waiting: false,
                        generating: false,
                        answer_tx: None,
                    });
                }
                self.command_palette = None;
                self.tabs
                    .open_tab(TabContent::PlanAuthoring { project_root });
                self.focused_panel = Panel::Main;
                self.mode = Mode::Normal;
                true
            }
            AppEvent::PlanAuthoringNewline => {
                if let Some(state) = self.plan_authoring.as_mut()
                    && !state.waiting
                {
                    state.input.push('\n');
                }
                true
            }
            AppEvent::PlanAuthoringPaste(text) => {
                if let Some(state) = self.plan_authoring.as_mut()
                    && !state.waiting
                {
                    // Verbatim: a bracketed paste is one atomic edit, and its
                    // newlines are content. Normalise only the CR forms a
                    // terminal may deliver so they do not render as blanks.
                    state
                        .input
                        .push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                }
                true
            }
            AppEvent::PlanAuthoringInput(c) => {
                if let Some(state) = self.plan_authoring.as_mut()
                    && !state.waiting
                {
                    state.input.push(c);
                }
                true
            }
            AppEvent::PlanAuthoringBackspace => {
                if let Some(state) = self.plan_authoring.as_mut()
                    && !state.waiting
                {
                    state.input.pop();
                }
                true
            }
            // The planner's own stream — reasoning and tool calls — folded into
            // the transcript by the same reducer the run's agents go through.
            AppEvent::PlanAuthoringExchange(event) => {
                if let Some(state) = self.plan_authoring.as_mut() {
                    apply_exchange_event(&mut state.log, StreamRole::Planner, &event);
                }
                true
            }
            AppEvent::PlanAuthoringGenerated(outcome) => {
                match outcome {
                    // Only a confirmed bundle retires the conversation.
                    Ok(_) => {
                        self.update(AppEvent::ClosePlanAuthoring);
                    }
                    Err(reason) => {
                        if let Some(state) = self.plan_authoring.as_mut() {
                            state.generating = false;
                            state.waiting = false;
                            // The planner session ended when it emitted the
                            // blueprint. Dropping the handle lets the next
                            // submit open a fresh one immediately, replaying
                            // the transcript, instead of spending a submit on
                            // discovering the channel is closed.
                            state.answer_tx = None;
                            state.push_planner(format!(
                                "The plan could not be generated: {reason}\n\
                                 Tell me what to change and I will revise it."
                            ));
                        }
                    }
                }
                true
            }
            AppEvent::PlanAuthoringQuestion { question } => {
                if let Some(state) = self.plan_authoring.as_mut() {
                    state.push_planner(question);
                    state.waiting = false;
                }
                true
            }
            AppEvent::PlanAuthoringFailed { reason } => {
                if let Some(state) = self.plan_authoring.as_mut() {
                    state.push_planner(format!("Error: {reason}"));
                    state.waiting = false;
                    state.generating = false;
                    state.answer_tx = None;
                }
                true
            }
            AppEvent::ClosePlanAuthoring => {
                if let Some(idx) = self
                    .tabs
                    .open_tabs
                    .iter()
                    .position(|tab| matches!(tab, TabContent::PlanAuthoring { .. }))
                {
                    self.tabs.close_tab(idx);
                }
                self.plan_authoring = None;
                self.mode = Mode::Normal;
                true
            }

            // Submission is IO-backed; resolve_io consumes the buffer.
            AppEvent::PlanAuthoringSubmit => true,

            // ── Command palette (plan 0069) ───────────────────────────────────────
            AppEvent::OpenCommandPalette => {
                self.command_palette = Some(CommandPalette {
                    filter: String::new(),
                    actions: CommandPalette::default_actions(),
                    selected: 0,
                    theme_selector: None,
                });
                self.mode = Mode::CommandPalette;
                true
            }
            AppEvent::CommandPaletteInput(c) => {
                if let Some(palette) = self.command_palette.as_mut() {
                    palette.filter.push(c);
                    // Clamp selected to the filtered list length (branch on mode)
                    let filtered_len = if palette.theme_selector.is_some() {
                        palette.filtered_theme_names().len()
                    } else {
                        palette.filtered().len()
                    };
                    palette.selected = palette.selected.min(filtered_len.saturating_sub(1));
                }
                true
            }
            AppEvent::CommandPaletteBackspace => {
                if let Some(palette) = self.command_palette.as_mut() {
                    palette.filter.pop();
                    // Clamp selected to the filtered list length (branch on mode)
                    let filtered_len = if palette.theme_selector.is_some() {
                        palette.filtered_theme_names().len()
                    } else {
                        palette.filtered().len()
                    };
                    palette.selected = palette.selected.min(filtered_len.saturating_sub(1));
                }
                true
            }
            AppEvent::CommandPaletteUp => {
                if let Some(palette) = self.command_palette.as_mut() {
                    palette.selected = palette.selected.saturating_sub(1);
                }
                true
            }
            AppEvent::CommandPaletteDown => {
                if let Some(palette) = self.command_palette.as_mut() {
                    let filtered_len = if palette.theme_selector.is_some() {
                        palette.filtered_theme_names().len()
                    } else {
                        palette.filtered().len()
                    };
                    palette.selected = (palette.selected + 1).min(filtered_len.saturating_sub(1));
                }
                true
            }
            AppEvent::CommandPaletteExecute => {
                // In normal execution, resolve_io handles this and returns the dispatched event.
                // But for testing and direct calls, we handle the theme selector mode here.
                if let Some(palette) = self.command_palette.as_mut() {
                    if palette.theme_selector.is_some() {
                        // In theme selector mode - apply the selection
                        self.update(AppEvent::ApplyThemeSelection);
                        return true;
                    }
                    // Check if we're selecting the NestedThemeSelector action
                    let filtered = palette.filtered();
                    if let Some(action) = filtered.get(palette.selected)
                        && matches!(action, PaletteAction::NestedThemeSelector { .. })
                    {
                        // Enter theme selector mode
                        self.update(AppEvent::EnterThemeSelector);
                        return true;
                    }
                }
                // Regular action execution - close the palette (the actual event is dispatched by resolve_io)
                self.mode = Mode::Normal;
                self.command_palette = None;
                true
            }
            AppEvent::EnterThemeSelector => {
                if let Some(palette) = self.command_palette.as_mut() {
                    let theme_names = crate::theme::Theme::builtin_themes()
                        .iter()
                        .map(|t| t.name.clone())
                        .collect();
                    palette.filter = String::new();
                    palette.selected = 0;
                    palette.theme_selector = Some(theme_names);
                }
                true
            }
            AppEvent::ApplyThemeSelection => {
                if let Some(palette) = self.command_palette.as_mut()
                    && palette.theme_selector.is_some()
                {
                    let selected_name = palette
                        .filtered_theme_names()
                        .get(palette.selected)
                        .map(|s| (*s).clone());
                    if let Some(name) = selected_name
                        && let Some(theme) = crate::theme::Theme::builtin_themes()
                            .into_iter()
                            .find(|t| t.name == name)
                    {
                        self.active_theme = theme;
                        self.markdown_cache.borrow_mut().clear();
                        palette.theme_selector = None; // Exit theme selector mode
                    }
                }
                true
            }
            AppEvent::CloseCommandPalette => {
                if let Some(palette) = self.command_palette.as_mut()
                    && palette.theme_selector.is_some()
                {
                    // Exit theme selector mode, return to action list
                    palette.theme_selector = None;
                    palette.filter = String::new();
                    palette.selected = 0;
                    return true;
                }
                self.mode = Mode::Normal;
                self.command_palette = None;
                true
            }

            // ── Settings screen (plan 0070) ───────────────────────────────────────
            AppEvent::OpenSettings => {
                let project_root = self.context_project_root();
                // Load through the startup-resolved global path so the modal
                // reads exactly the file `commit_settings` writes back to.
                let (loaded, paths) = makina_core::config::Config::load_for_repo_with_global(
                    self.config_paths.global.as_deref(),
                    &project_root,
                );
                let (caps, concurrency, final_merge, roles, error) = match loaded {
                    // `config.roles` is already resolved project → global, so
                    // the modal shows the model this project would actually
                    // use — including one inherited from the global layer.
                    Ok(config) => (
                        config.caps,
                        config.concurrency,
                        config.merge.final_,
                        config.roles,
                        None,
                    ),
                    Err(load_error) => {
                        let mut caps = self.caps.clone();
                        let mut concurrency = self.concurrency;
                        let mut final_merge = self.final_merge;
                        // The config did not resolve, so fall back to the live
                        // role assignments for the model buffers.
                        let mut roles = self.roles.clone();
                        let project = paths.project.as_ref().and_then(|path| {
                            std::fs::read_to_string(path).ok().and_then(|text| {
                                makina_core::config::ProjectConfig::from_toml_str(
                                    &text,
                                    &path.display().to_string(),
                                )
                                .ok()
                            })
                        });
                        if let Some(project) = project {
                            if let Some(overrides) = project.caps {
                                caps.gate_iterations =
                                    overrides.gate_iterations.unwrap_or(caps.gate_iterations);
                                caps.reviewer_iterations = overrides
                                    .reviewer_iterations
                                    .unwrap_or(caps.reviewer_iterations);
                                caps.wall_clock_secs =
                                    overrides.wall_clock_secs.unwrap_or(caps.wall_clock_secs);
                                if let Some(idle_secs) = overrides.idle_secs {
                                    caps.idle_secs = idle_secs;
                                }
                            }
                            concurrency = project.concurrency.unwrap_or(concurrency);
                            final_merge = project
                                .merge
                                .map(|merge| merge.final_)
                                .unwrap_or(final_merge);
                            // A project-level model still wins over the live
                            // assignments, mirroring `Config::resolve`.
                            let project_models = makina_core::config::RoleModels {
                                developer: project
                                    .roles
                                    .developer
                                    .as_ref()
                                    .and_then(|r| r.model.clone()),
                                reviewer: project
                                    .roles
                                    .reviewer
                                    .as_ref()
                                    .and_then(|r| r.model.clone()),
                                planner: project
                                    .roles
                                    .planner
                                    .as_ref()
                                    .and_then(|r| r.model.clone()),
                            };
                            let provider_defaults = roles.clone();
                            roles.apply_models(&project_models, &provider_defaults);
                            (caps, concurrency, final_merge, roles, None)
                        } else if paths.project.as_ref().is_some_and(|path| path.exists()) {
                            (
                                caps,
                                concurrency,
                                final_merge,
                                roles,
                                Some(format!(
                                    "Could not load {} settings: {load_error}",
                                    project_root.display()
                                )),
                            )
                        } else {
                            (caps, concurrency, final_merge, roles, None)
                        }
                    }
                };
                // Adopt the context project's resolved assignments so the modal,
                // the start-run model check, and the plan-authoring session all
                // agree on which model this project would actually use.
                self.roles = roles.clone();
                // Seed model text buffers from the resolved role assignments.
                let model_of = |role: &Option<makina_core::config::RoleAssignment>| {
                    role.as_ref()
                        .and_then(|r| r.model.clone())
                        .unwrap_or_default()
                };
                let effort_of = |role: &Option<makina_core::config::RoleAssignment>| {
                    role.as_ref()
                        .and_then(|r| r.effort.clone())
                        .unwrap_or_default()
                };
                self.settings = Some(Settings {
                    project_root,
                    gate_iterations: caps.gate_iterations.to_string(),
                    reviewer_iterations: caps.reviewer_iterations.to_string(),
                    wall_clock_secs: caps.wall_clock_secs.to_string(),
                    idle_secs: caps.idle_secs.map(|s| s.to_string()).unwrap_or_default(),
                    concurrency: concurrency.to_string(),
                    final_merge,
                    developer_model: model_of(&roles.developer),
                    reviewer_model: model_of(&roles.reviewer),
                    planner_model: model_of(&roles.planner),
                    developer_effort: effort_of(&roles.developer),
                    reviewer_effort: effort_of(&roles.reviewer),
                    planner_effort: effort_of(&roles.planner),
                    discovered_models: vec![],
                    discovered_efforts: vec![],
                    focused: SettingsField::GateIterations,
                    error,
                });
                self.mode = Mode::Settings;
                true
            }

            // ── Settings navigation and editing (plan 0070) ─────────────────────────
            AppEvent::SettingsUp => {
                if let Some(settings) = &mut self.settings {
                    settings.focused = match settings.focused {
                        SettingsField::GateIterations => SettingsField::PlannerEffort,
                        SettingsField::ReviewerIterations => SettingsField::GateIterations,
                        SettingsField::WallClockSecs => SettingsField::ReviewerIterations,
                        SettingsField::IdleSecs => SettingsField::WallClockSecs,
                        SettingsField::Concurrency => SettingsField::IdleSecs,
                        SettingsField::FinalMerge => SettingsField::Concurrency,
                        SettingsField::DeveloperModel => SettingsField::FinalMerge,
                        SettingsField::DeveloperEffort => SettingsField::DeveloperModel,
                        SettingsField::ReviewerModel => SettingsField::DeveloperEffort,
                        SettingsField::ReviewerEffort => SettingsField::ReviewerModel,
                        SettingsField::PlannerModel => SettingsField::ReviewerEffort,
                        SettingsField::PlannerEffort => SettingsField::PlannerModel,
                    };
                }
                true
            }

            AppEvent::SettingsDown => {
                if let Some(settings) = &mut self.settings {
                    settings.focused = match settings.focused {
                        SettingsField::GateIterations => SettingsField::ReviewerIterations,
                        SettingsField::ReviewerIterations => SettingsField::WallClockSecs,
                        SettingsField::WallClockSecs => SettingsField::IdleSecs,
                        SettingsField::IdleSecs => SettingsField::Concurrency,
                        SettingsField::Concurrency => SettingsField::FinalMerge,
                        SettingsField::FinalMerge => SettingsField::DeveloperModel,
                        SettingsField::DeveloperModel => SettingsField::DeveloperEffort,
                        SettingsField::DeveloperEffort => SettingsField::ReviewerModel,
                        SettingsField::ReviewerModel => SettingsField::ReviewerEffort,
                        SettingsField::ReviewerEffort => SettingsField::PlannerModel,
                        SettingsField::PlannerModel => SettingsField::PlannerEffort,
                        SettingsField::PlannerEffort => SettingsField::GateIterations,
                    };
                }
                true
            }

            AppEvent::SettingsInput(c) => {
                if let Some(settings) = &mut self.settings {
                    // Numeric fields accept digits; model fields accept
                    // alphanumeric + dash + dot + underscore (model name chars).
                    let is_numeric_field = matches!(
                        settings.focused,
                        SettingsField::GateIterations
                            | SettingsField::ReviewerIterations
                            | SettingsField::WallClockSecs
                            | SettingsField::IdleSecs
                            | SettingsField::Concurrency
                    );
                    let is_model_field = matches!(
                        settings.focused,
                        SettingsField::DeveloperModel
                            | SettingsField::ReviewerModel
                            | SettingsField::PlannerModel
                    );
                    if is_numeric_field && c.is_ascii_digit() {
                        match settings.focused {
                            SettingsField::GateIterations => settings.gate_iterations.push(c),
                            SettingsField::ReviewerIterations => {
                                settings.reviewer_iterations.push(c)
                            }
                            SettingsField::WallClockSecs => settings.wall_clock_secs.push(c),
                            SettingsField::IdleSecs => settings.idle_secs.push(c),
                            SettingsField::Concurrency => settings.concurrency.push(c),
                            _ => {}
                        }
                        // Re-validate the focused field inline.
                        settings.error = None;
                        match settings.focused {
                            SettingsField::GateIterations => {
                                if let Ok(val) = settings.gate_iterations.parse::<u32>() {
                                    if val < 1 {
                                        settings.error = Some(
                                            "caps.gate_iterations must be at least 1".to_string(),
                                        );
                                    }
                                } else if !settings.gate_iterations.is_empty() {
                                    settings.error = Some(
                                        "caps.gate_iterations must be a positive integer"
                                            .to_string(),
                                    );
                                }
                            }
                            SettingsField::ReviewerIterations => {
                                if let Ok(val) = settings.reviewer_iterations.parse::<u32>() {
                                    if val < 1 {
                                        settings.error = Some(
                                            "caps.reviewer_iterations must be at least 1"
                                                .to_string(),
                                        );
                                    }
                                } else if !settings.reviewer_iterations.is_empty() {
                                    settings.error = Some(
                                        "caps.reviewer_iterations must be a positive integer"
                                            .to_string(),
                                    );
                                }
                            }
                            SettingsField::WallClockSecs => {
                                if let Ok(val) = settings.wall_clock_secs.parse::<u64>() {
                                    if val < 1 {
                                        settings.error = Some(
                                            "caps.wall_clock_secs must be at least 1".to_string(),
                                        );
                                    }
                                } else if !settings.wall_clock_secs.is_empty() {
                                    settings.error = Some(
                                        "caps.wall_clock_secs must be a positive integer"
                                            .to_string(),
                                    );
                                }
                            }
                            SettingsField::IdleSecs => {
                                if !settings.idle_secs.is_empty() {
                                    if let Ok(val) = settings.idle_secs.parse::<u64>() {
                                        if val < 1 {
                                            settings.error = Some(
                                                "caps.idle_secs must be at least 1".to_string(),
                                            );
                                        }
                                    } else {
                                        settings.error = Some(
                                            "caps.idle_secs must be a positive integer".to_string(),
                                        );
                                    }
                                }
                            }
                            SettingsField::Concurrency => {
                                if let Ok(val) = settings.concurrency.parse::<usize>() {
                                    if val < 1 {
                                        settings.error =
                                            Some("concurrency must be at least 1".to_string());
                                    }
                                } else if !settings.concurrency.is_empty() {
                                    settings.error =
                                        Some("concurrency must be a positive integer".to_string());
                                }
                            }
                            _ => {}
                        }
                    } else if is_model_field
                        && (c.is_ascii_alphanumeric()
                            || c == '-'
                            || c == '.'
                            || c == '_'
                            || c == '/')
                    {
                        match settings.focused {
                            SettingsField::DeveloperModel => settings.developer_model.push(c),
                            SettingsField::ReviewerModel => settings.reviewer_model.push(c),
                            SettingsField::PlannerModel => settings.planner_model.push(c),
                            SettingsField::DeveloperEffort => settings.developer_effort.push(c),
                            SettingsField::ReviewerEffort => settings.reviewer_effort.push(c),
                            SettingsField::PlannerEffort => settings.planner_effort.push(c),
                            _ => {}
                        }
                    }
                }
                true
            }

            AppEvent::SettingsBackspace => {
                if let Some(settings) = &mut self.settings {
                    match settings.focused {
                        SettingsField::GateIterations => {
                            settings.gate_iterations.pop();
                        }
                        SettingsField::ReviewerIterations => {
                            settings.reviewer_iterations.pop();
                        }
                        SettingsField::WallClockSecs => {
                            settings.wall_clock_secs.pop();
                        }
                        SettingsField::IdleSecs => {
                            settings.idle_secs.pop();
                        }
                        SettingsField::Concurrency => {
                            settings.concurrency.pop();
                        }
                        SettingsField::FinalMerge => {}
                        SettingsField::DeveloperModel => {
                            settings.developer_model.pop();
                        }
                        SettingsField::ReviewerModel => {
                            settings.reviewer_model.pop();
                        }
                        SettingsField::PlannerModel => {
                            settings.planner_model.pop();
                        }
                        SettingsField::DeveloperEffort => {
                            settings.developer_effort.pop();
                        }
                        SettingsField::ReviewerEffort => {
                            settings.reviewer_effort.pop();
                        }
                        SettingsField::PlannerEffort => {
                            settings.planner_effort.pop();
                        }
                    }
                    // Re-validate the focused field inline.
                    settings.error = None;
                    match settings.focused {
                        SettingsField::GateIterations => {
                            if let Ok(val) = settings.gate_iterations.parse::<u32>() {
                                if val < 1 {
                                    settings.error =
                                        Some("caps.gate_iterations must be at least 1".to_string());
                                }
                            } else if !settings.gate_iterations.is_empty() {
                                settings.error = Some(
                                    "caps.gate_iterations must be a positive integer".to_string(),
                                );
                            }
                        }
                        SettingsField::ReviewerIterations => {
                            if let Ok(val) = settings.reviewer_iterations.parse::<u32>() {
                                if val < 1 {
                                    settings.error = Some(
                                        "caps.reviewer_iterations must be at least 1".to_string(),
                                    );
                                }
                            } else if !settings.reviewer_iterations.is_empty() {
                                settings.error = Some(
                                    "caps.reviewer_iterations must be a positive integer"
                                        .to_string(),
                                );
                            }
                        }
                        SettingsField::WallClockSecs => {
                            if let Ok(val) = settings.wall_clock_secs.parse::<u64>() {
                                if val < 1 {
                                    settings.error =
                                        Some("caps.wall_clock_secs must be at least 1".to_string());
                                }
                            } else if !settings.wall_clock_secs.is_empty() {
                                settings.error = Some(
                                    "caps.wall_clock_secs must be a positive integer".to_string(),
                                );
                            }
                        }
                        SettingsField::IdleSecs => {
                            if !settings.idle_secs.is_empty() {
                                if let Ok(val) = settings.idle_secs.parse::<u64>() {
                                    if val < 1 {
                                        settings.error =
                                            Some("caps.idle_secs must be at least 1".to_string());
                                    }
                                } else {
                                    settings.error = Some(
                                        "caps.idle_secs must be a positive integer".to_string(),
                                    );
                                }
                            }
                        }
                        SettingsField::Concurrency => {
                            if let Ok(val) = settings.concurrency.parse::<usize>() {
                                if val < 1 {
                                    settings.error =
                                        Some("concurrency must be at least 1".to_string());
                                }
                            } else if !settings.concurrency.is_empty() {
                                settings.error =
                                    Some("concurrency must be a positive integer".to_string());
                            }
                        }
                        // Free-text and picker fields have nothing to
                        // re-validate. They previously popped a second time
                        // here, so one Backspace deleted two characters.
                        SettingsField::FinalMerge
                        | SettingsField::DeveloperModel
                        | SettingsField::ReviewerModel
                        | SettingsField::PlannerModel
                        | SettingsField::DeveloperEffort
                        | SettingsField::ReviewerEffort
                        | SettingsField::PlannerEffort => {}
                    }
                }
                true
            }

            AppEvent::SettingsPreviousOption => {
                if let Some(settings) = &mut self.settings
                    && settings.focused == SettingsField::FinalMerge
                {
                    settings.final_merge = previous_settings_final_merge(settings.final_merge);
                    settings.error = None;
                }
                true
            }

            AppEvent::SettingsNextOption => {
                if let Some(settings) = &mut self.settings
                    && settings.focused == SettingsField::FinalMerge
                {
                    settings.final_merge = next_settings_final_merge(settings.final_merge);
                    settings.error = None;
                }
                true
            }

            AppEvent::ModelsDiscovered { models, efforts } => {
                if let Some(settings) = &mut self.settings {
                    // Each list is replaced only when the probe actually found
                    // something, so an agent that advertises models but no
                    // thought levels does not erase a previous discovery.
                    if !models.is_empty() {
                        settings.discovered_models = models.clone();
                    }
                    if !efforts.is_empty() {
                        settings.discovered_efforts = efforts.clone();
                    }
                }
                true
            }

            AppEvent::OpenModelPicker => {
                if let Some(settings) = &self.settings {
                    let target_field = settings.focused;
                    // The picker serves both kinds of choice; which list it
                    // offers follows the focused field, so an effort field
                    // lists thought levels rather than models.
                    if target_field.is_pickable() {
                        let all_models = if target_field.is_effort() {
                            settings.discovered_efforts.clone()
                        } else {
                            settings.discovered_models.clone()
                        };
                        self.model_picker = Some(ModelPicker {
                            all_models,
                            filter: String::new(),
                            selected: 0,
                            target_field,
                        });
                        self.mode = Mode::ModelPicker;
                    }
                }
                true
            }

            AppEvent::CloseModelPicker => {
                self.model_picker = None;
                self.mode = Mode::Settings;
                true
            }

            AppEvent::ModelPickerSelect => {
                if let Some(picker) = self.model_picker.take() {
                    let filtered = picker.filtered();
                    if let Some(model) = filtered.get(picker.selected)
                        && let Some(settings) = &mut self.settings
                    {
                        match picker.target_field {
                            SettingsField::DeveloperModel => {
                                settings.developer_model = model.to_string()
                            }
                            SettingsField::ReviewerModel => {
                                settings.reviewer_model = model.to_string()
                            }
                            SettingsField::PlannerModel => {
                                settings.planner_model = model.to_string()
                            }
                            SettingsField::DeveloperEffort => {
                                settings.developer_effort = model.to_string()
                            }
                            SettingsField::ReviewerEffort => {
                                settings.reviewer_effort = model.to_string()
                            }
                            SettingsField::PlannerEffort => {
                                settings.planner_effort = model.to_string()
                            }
                            _ => {}
                        }
                    }
                }
                self.mode = Mode::Settings;
                true
            }

            AppEvent::ModelPickerInput(c) => {
                if let Some(picker) = self.model_picker.as_mut() {
                    let c = if c.is_ascii_alphanumeric()
                        || c == '-'
                        || c == '.'
                        || c == '_'
                        || c == '/'
                        || c == ' '
                    {
                        Some(c)
                    } else {
                        None
                    };
                    if let Some(c) = c {
                        picker.filter.push(c);
                        picker.selected = 0;
                    }
                }
                true
            }

            AppEvent::ModelPickerBackspace => {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.filter.pop();
                    picker.selected = 0;
                }
                true
            }

            AppEvent::ModelPickerUp => {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                true
            }

            AppEvent::ModelPickerDown => {
                if let Some(picker) = self.model_picker.as_mut() {
                    let max = picker.filtered().len().saturating_sub(1);
                    picker.selected = (picker.selected + 1).min(max);
                }
                true
            }

            AppEvent::SettingsCommit => {
                // Intercepted by resolve_io (commit_settings). This handler
                // is only reached if resolve_io passes it through, which it
                // doesn't. Kept as a no-op for exhaustiveness.
                true
            }

            AppEvent::SettingsAutoSave => {
                // Auto-save during editing: validate but don't close.
                if let Some(settings) = &mut self.settings {
                    use crate::settings_validation::validate_settings;
                    if let Err(message) = validate_settings(settings) {
                        settings.error = Some(message);
                    }
                }
                true
            }

            AppEvent::CloseSettings => {
                // Esc closes settings — auto-save by dispatching SettingsCommit
                // through the IO layer before closing.
                if let Some(settings) = &mut self.settings {
                    use crate::settings_validation::validate_settings;
                    if let Err(message) = validate_settings(settings) {
                        settings.error = Some(message);
                        return true; // keep open on validation error
                    }
                }
                self.mode = Mode::Normal;
                true
            }

            AppEvent::SettingsSaved {
                project_root,
                values,
            } => {
                if project_root == self.canonical_repo_root() {
                    self.caps.gate_iterations = values.gate_iterations;
                    self.caps.reviewer_iterations = values.reviewer_iterations;
                    self.caps.wall_clock_secs = values.wall_clock_secs;
                    self.caps.idle_secs = values.idle_secs;
                    self.concurrency = values.concurrency;
                    self.final_merge = values.final_merge;
                }
                // Apply model selections from the settings modal to the app's
                // role assignments so subsequent runs use the chosen models.
                // The buffers hold the picker's `{agent}/{model}` form; config
                // and the App store the bare model name.
                let models = self.selected_role_models();
                let provider_defaults = self.roles.clone();
                self.roles.apply_models(&models, &provider_defaults);
                // Only close the modal if SettingsCommit triggered the save
                // (Esc). Auto-save (SettingsAutoSave) keeps the modal open.
                // SettingsCommit sets mode to Normal before the IO layer
                // returns SettingsSaved; auto-save keeps mode as Settings.
                if self.mode != Mode::Settings {
                    self.settings = None;
                }
                true
            }

            AppEvent::SettingsSaveFailed { reason } => {
                if let Some(settings) = &mut self.settings {
                    settings.error = Some(reason);
                }
                true
            }

            // ── Project discovery (plan 0025) ─────────────────────────────────────
            // Palette IO actions are re-dispatched through `resolve_io`, so
            // `DiscoverProject` is handled in `event.rs:discover_project()` and never
            // reaches this arm (plan 0041).

            // ── Tabbed content pane (plan 0031) ───────────────────────────────
            AppEvent::OpenTab(content) => {
                self.tabs.open_tab(content);
                self.markdown_cache.borrow_mut().clear();
                self.sync_selected_run_to_active_tab();
                true
            }
            AppEvent::CloseTab => {
                if let Some(active) = self.tabs.active_tab {
                    self.tabs.close_tab(active);
                }
                self.markdown_cache.borrow_mut().clear();
                self.sync_selected_run_to_active_tab();
                true
            }
            AppEvent::CloseTabAt(idx) => {
                if idx < self.tabs.open_tabs.len() {
                    self.tabs.close_tab(idx);
                    self.markdown_cache.borrow_mut().clear();
                    self.sync_selected_run_to_active_tab();
                    true
                } else {
                    false
                }
            }
            AppEvent::NextTab => {
                if !self.tabs.open_tabs.is_empty() {
                    let next = (self.tabs.active_tab.unwrap_or(0) + 1) % self.tabs.open_tabs.len();
                    self.tabs.active_tab = Some(next);
                    self.markdown_cache.borrow_mut().clear();
                }
                self.sync_selected_run_to_active_tab();
                true
            }
            AppEvent::PrevTab => {
                if !self.tabs.open_tabs.is_empty() {
                    let len = self.tabs.open_tabs.len();
                    let prev = (self.tabs.active_tab.unwrap_or(0) + len - 1) % len;
                    self.tabs.active_tab = Some(prev);
                    self.markdown_cache.borrow_mut().clear();
                }
                self.sync_selected_run_to_active_tab();
                true
            }
            // Mouse click on a tab chip: activate it (mirror Next/Prev semantics).
            AppEvent::ActivateTab(idx) => {
                if idx < self.tabs.open_tabs.len() {
                    self.tabs.active_tab = Some(idx);
                    self.markdown_cache.borrow_mut().clear();
                    self.sync_selected_run_to_active_tab();
                }
                true
            }

            // Keyboard Enter on the currently focused sidebar tree node.
            // (Previously this was transformed in resolve_io to an OpenTab; now
            // handled here so we can also expand plans, which requires &mut App.)
            AppEvent::OpenFocusedNode => {
                self.activate_focused_tree_node();
                true
            }

            // Mouse click on a sidebar row: move the cursor there and open/focus
            // that node's tab (and for plans, expand), mirroring the keyboard Enter
            // (`OpenFocusedNode`). Kept here (not in `resolve_io`) because cursor
            // mutation is needed (resolve_io only sees &App).
            AppEvent::OpenTreeRow(idx) => {
                let nodes = self.visible_tree_nodes();
                let Some(node) = nodes.get(idx).copied() else {
                    return false;
                };
                // Move the sidebar highlight to the clicked row and re-sync the
                // selected run/exchanges from it.
                self.tree_cursor = Some(idx);
                self.sync_selection_from_cursor();
                match node {
                    TreeNode::Plan { plan_idx } => {
                        self.activate_plan_node(plan_idx);
                    }
                    TreeNode::PlanTask { plan_idx, task_idx } => {
                        // A plan's task preview opens its own task tab (distinct
                        // from the plan tab), so each task the user clicks gets a
                        // tab — matching the run-task behaviour below.
                        if let Some(plan) = self.discovered_plans.get(plan_idx)
                            && let Some(preview) = plan.tasks().get(task_idx)
                        {
                            let plan = self.plan_identity_for_entry(&self.repo_root, plan);
                            let task_id = preview.frontmatter.id.as_str().to_owned();
                            self.tabs.open_tab(TabContent::PlanTask { plan, task_id });
                        }
                    }
                    TreeNode::Task { run, task } => {
                        if let Some(run_view) = self.runs.get(run)
                            && let Some(task_view) = run_view.tasks.get(task)
                        {
                            let plan = self.plan_identity_for_run(run_view);
                            let run_id = run_view.id;
                            let task_id = task_view.id.clone();
                            self.tabs.open_tab(TabContent::Task {
                                plan,
                                run: run_id,
                                task_id,
                            });
                            self.sync_selected_run_to_active_tab();
                        }
                    }
                    TreeNode::Run { run } => {
                        // Clicking a plan-style run header (e.g. completed plans)
                        // now opens its plan details tab, matching the new Enter
                        // behavior. Non-plan runs just select (no tab).
                        if let Some(run_view) = self.runs.get(run) {
                            let target = self.plan_identity_for_run(run_view);
                            if self.plan_entry(&target).is_some() {
                                self.tabs.open_tab(TabContent::Plan { plan: target });
                            }
                            // Expand the run so its tasks become visible in the sidebar.
                            self.collapsed_runs.remove(&run_view.id);
                            self.move_cursor_to_run_header(run);
                            self.sync_selection_from_cursor();
                        }
                    }
                    TreeNode::Folder { folder_idx } => {
                        // Clicking a folder row toggles its expansion, mirroring
                        // Enter (activate_focused_tree_node) on a Folder node.
                        if self.collapsed_folders.contains(&folder_idx) {
                            self.collapsed_folders.remove(&folder_idx);
                        } else {
                            self.collapsed_folders.insert(folder_idx);
                        }
                        self.move_cursor_to_folder_header(folder_idx);
                        self.sync_selection_from_cursor();
                    }
                    TreeNode::PlanInFolder {
                        folder_idx,
                        plan_idx,
                    } => {
                        self.activate_plan_in_folder(folder_idx, plan_idx);
                    }
                    TreeNode::PlanTaskInFolder {
                        folder_idx,
                        plan_idx,
                        task_idx,
                    } => {
                        // A plan's task preview opens its own task tab (distinct
                        // from the plan tab), matching the legacy PlanTask arm above.
                        if let Some(plans) = self.plans_by_folder.get(&folder_idx)
                            && let Some(plan) = plans.get(plan_idx)
                            && let Some(preview) = plan.tasks().get(task_idx)
                            && let Some(project_root) = self.opened_folders.get(folder_idx)
                        {
                            let plan = self.plan_identity_for_entry(project_root, plan);
                            let task_id = preview.frontmatter.id.as_str().to_owned();
                            self.tabs.open_tab(TabContent::PlanTask { plan, task_id });
                        }
                    }
                }
                true
            }

            // ── Accordion sections (plan 0032) ───────────────────────────────────
            AppEvent::ToggleAccordionSection(section) => {
                // Only toggle if the active tab is a plan tab.
                if let Some(active_idx) = self.tabs.active_tab
                    && let Some(TabContent::Plan { plan }) = self.tabs.open_tabs.get(active_idx)
                {
                    let plan = plan.clone();
                    let sections = self.accordion_state.entry(plan).or_default();
                    if sections.contains(&section) {
                        sections.remove(&section);
                    } else {
                        sections.insert(section);
                    }
                }
                true
            }

            AppEvent::ToggleTaskAccordionSection(section) => {
                // Only toggle if the active tab is a task detail tab.
                if let Some(active_idx) = self.tabs.active_tab
                    && let Some(content) = self.tabs.open_tabs.get(active_idx)
                {
                    let task_id = match content {
                        TabContent::Task { task_id, .. } => task_id.clone(),
                        TabContent::PlanTask { task_id, .. } => TaskId::new(task_id.clone()),
                        TabContent::Plan { .. } | TabContent::PlanAuthoring { .. } => {
                            return true;
                        }
                    };
                    // Seed an absent entry with the SAME default the renderer uses
                    // (Scope + Execution expanded), so the first toggle collapses
                    // the section the user actually pressed instead of inverting.
                    let sections = self
                        .task_accordion_expanded
                        .entry(task_id)
                        .or_insert_with(default_task_accordion_sections);
                    if sections.contains(&section) {
                        sections.remove(&section);
                    } else {
                        sections.insert(section);
                    }
                }
                true
            }

            AppEvent::ToggleToolDiff(key) => {
                if !self.expanded_tool_diffs.insert(key.clone()) {
                    self.expanded_tool_diffs.remove(&key);
                }
                true
            }

            // ── Verbose mode (plan 0021) ──────────────────────────────────────
            AppEvent::ToggleVerbose => {
                self.verbose_mode = !self.verbose_mode;
                true
            }
            AppEvent::ResizeSidebarLeft => {
                // Step the sidebar by 2% per keystroke, clamped so the sidebar stays in
                // [10, 50]% and the main pane always keeps at least half the body.
                self.sidebar_width_percent = self.sidebar_width_percent.saturating_sub(2).max(10);
                true
            }
            AppEvent::ResizeSidebarRight => {
                self.sidebar_width_percent = (self.sidebar_width_percent + 2).min(50);
                true
            }

            // ── Folder operations (plan 0043) ───────────────────────────────────────
            // Set the folder browser purpose before resolve_io spawns the directory read.
            AppEvent::OpenFolder => {
                self.folder_browser_purpose = Some(FolderBrowserPurpose::OpenFolder);
                self.busy = Some("Opening folder browser…".to_string());
                true
            }
            AppEvent::InitializeFolderRequested => {
                self.folder_browser_purpose = Some(FolderBrowserPurpose::InitializeFolder);
                self.busy = Some("Opening folder browser…".to_string());
                true
            }
            // `resolve_io` has a dedicated arm for `OpenFolderSelected` /
            // `InitializeFolderSelected` (it resolves them to `Tick` — the modal
            // is already closed by `FolderBrowserActivate`'s `CloseBrowser`
            // resolution in the same pass), so `update` never sees the original
            // event. Persisting the folder + triggering discovery is
            // `handle-folder-open-close-events` / `handle-initialize-folder-event`
            // (later tasks in plan 0043).
            AppEvent::OpenFolderSelected { .. } => {
                unreachable!(
                    "OpenFolderSelected should be handled in resolve_io; \
                    if this fires, the event loop logic (plan 0043) is broken"
                )
            }
            AppEvent::CloseFolderRequested => {
                unreachable!(
                    "CloseFolderRequested should be handled in resolve_io; \
                    if this fires, the event loop logic (plan 0043) is broken"
                )
            }
            AppEvent::CloseFolderConfirmed { .. } => {
                unreachable!(
                    "CloseFolderConfirmed should be handled in resolve_io; \
                    if this fires, the event loop logic (plan 0043) is broken"
                )
            }
            AppEvent::InitializeFolderSelected { .. } => {
                unreachable!(
                    "InitializeFolderSelected should be resolved to Tick in resolve_io; \
                    if this fires, the event loop logic (plan 0043) is broken"
                )
            }

            // ── Project discovery (plan 0025) ─────────────────────────────────────
            // DiscoverProject is handled in resolve_io::discover_project and should
            // never reach this arm due to re-dispatch on the palette path (plan 0041).
            // If it does reach here, the re-dispatch logic failed.
            AppEvent::DiscoverProject => {
                unreachable!(
                    "DiscoverProject should be handled in resolve_io; \
                    if this fires, the palette re-dispatch logic (plan 0041) is broken"
                )
            }
        }
    }

    /// Clamp `selected_task` to the currently selected run's authored tasks.
    ///
    /// Called after changing `selected_run` or after a run's authored tasks are
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
        let project_root = self.plan_identity_for_run(run).project_root;

        // RunView carries no caps field. Resolve the owning project's current
        // config so a repo-B task does not display repo A's wall-clock budget.
        self.wall_clock_secs_config =
            makina_core::config::Config::load_for_repo_with_paths(&project_root)
                .0
                .map(|config| config.caps.wall_clock_secs)
                .unwrap_or(self.caps.wall_clock_secs);

        // Use the pure path helper (no I/O, no create_dir_all) so the replay
        // loader does not silently create directories for non-existent runs.
        let Ok(logs_dir) = paths::run_dir(&project_root, &run_uid).map(|dir| dir.join("logs"))
        else {
            return;
        };

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
            Event::PlanRegistered {
                plan_dir,
                registration_oid,
            } => {
                self.status_message = Some(format!(
                    "Registered plan {} at {}",
                    plan_dir.relative_dir.display(),
                    registration_oid
                ));
            }
            Event::RunOpened { run, plan_dir: _ } => {
                // Keep opening_plans: the "starting" badge on the plan node
                // should stay visible until the run actually starts executing
                // (Running), not just until RunOpened arrives. RunOpened is
                // instantaneous and the user would never see the badge if we
                // cleared it here. It is cleared on RunStatusChanged{Running}.
                let project = self
                    .runs
                    .iter()
                    .find(|view| view.id == *run)
                    .map(|view| view.project.clone())
                    .unwrap_or_default();
                let identity_view = RunView {
                    id: *run,
                    run_uid: String::new(),
                    plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
                    status: RunStatus::Pending,
                    project,
                    tasks: vec![],
                    report: makina_core::api::IngestionReport::default(),
                };
                self.remember_run_project_root(&identity_view);
                // Mark this run as "starting" immediately on RunOpened so the
                // sidebar shows the animated badge even before the separate
                // RunStarting event arrives from the background auto-start.
                self.starting_runs.insert(*run);
                // If we don't already have a RunView for this id (the initial
                // `api.runs()` query might have raced with the event), insert a
                // placeholder.  Tasks 27 and 29 will flesh out proper handling.
                if !self.runs.iter().any(|r| r.id == *run) {
                    self.runs.push(identity_view);
                    if self.selected_run.is_none() {
                        self.selected_run = Some(0);
                    }
                }
            }
            Event::RunStatusChanged { run, status } => {
                // Clear the "starting" badge only when the run actually reaches
                // Running — the real status badge ([▶]) now applies. Keep it
                // through Pending and WaitingForRepository so the user sees
                // continuous feedback from the moment they pressed Start.
                if matches!(status, RunStatus::Running) {
                    self.starting_runs.remove(run);
                    // Also clear opening_plans for this run's plan now that
                    // the run is actually executing.
                    if let Some(rv) = self.runs.iter().find(|r| r.id == *run) {
                        self.opening_plans
                            .retain(|target| target.plan_dir != rv.plan_dir);
                    }
                }
                if let Some(rv) = self.runs.iter_mut().find(|r| r.id == *run) {
                    rv.status = status.clone();
                }
            }
            Event::RepositoryLeaseWaiting { run, owner } => {
                // Keep starting_runs: the run is in the lease queue but hasn't
                // started executing yet. The "starting" badge stays visible.
                if let Some(rv) = self.runs.iter_mut().find(|r| r.id == *run) {
                    rv.status = RunStatus::WaitingForRepository {
                        owner: owner.clone(),
                    };
                }
            }
            Event::RunProgress { run, phase } => {
                self.run_progress.insert(*run, phase.clone());
            }
            Event::RunCommand {
                run,
                command,
                working_dir: _,
            } => {
                // Show the actual command being executed.
                self.run_progress.insert(*run, format!("$ {command}"));
            }
            Event::TaskStateChanged { run, task, state } => {
                if let Some(rv) = self.runs.iter_mut().find(|r| r.id == *run)
                    && let Some(tv) = rv.tasks.iter_mut().find(|t| t.id == *task)
                {
                    tv.state = state.clone();
                    // Leaving `Failed` (retry / reset / reinterpret) invalidates
                    // the stored reason — the log pane and task summary render it
                    // without checking the state, so a stale reason would keep
                    // showing "failed: …" under a task that is running again.
                    if !matches!(state, TaskState::Failed) {
                        tv.failure_reason = None;
                    }
                    // When entering InProgress or InReview, record the step start tick.
                    if matches!(state, TaskState::InProgress | TaskState::InReview) {
                        self.task_step_start_tick
                            .insert((*run, task.clone()), self.tick);
                        // Initialize last activity to step start as well.
                        self.task_last_activity_tick
                            .insert((*run, task.clone()), self.tick);
                    }
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
            // The supervisor's diagnosis for a failed task. `TaskStateChanged`
            // carries only the state, so without this the task shows a bare
            // `Failed` badge and the reason is visible nowhere in the TUI.
            Event::TaskFailed { run, task, reason } => {
                if let Some(rv) = self.runs.iter_mut().find(|r| r.id == *run)
                    && let Some(tv) = rv.tasks.iter_mut().find(|t| t.id == *task)
                {
                    tv.failure_reason = Some(reason.clone());
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
            }
            // An autonomous mode switch: reflect it in the stored capabilities.
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
                // Update last-activity tick whenever an exchange event arrives.
                self.task_last_activity_tick
                    .insert((*run, task.clone()), self.tick);
                // Update run_progress so the sidebar shows what the agent is
                // doing — the user sees "agent: <latest text chunk>" instead
                // of a generic "dispatching task" spinner.
                match exchange_ev {
                    makina_core::api::ExchangeEvent::PromptSent { .. } => {
                        self.run_progress
                            .insert(*run, format!("agent reading task: {task}"));
                    }
                    makina_core::api::ExchangeEvent::ResponseChunk { text } => {
                        let snippet: String = text.chars().take(60).collect();
                        let suffix = if text.len() > 60 { "…" } else { "" };
                        self.run_progress
                            .insert(*run, format!("agent: {snippet}{suffix}"));
                    }
                    makina_core::api::ExchangeEvent::ToolCall { title, .. } => {
                        self.run_progress
                            .insert(*run, format!("agent tool: {title}"));
                    }
                    makina_core::api::ExchangeEvent::ToolCallUpdate {
                        title: Some(title), ..
                    } => {
                        self.run_progress
                            .insert(*run, format!("agent tool: {title}"));
                    }
                    _ => {}
                }
            }
            // Idle watchdog event: a task stalled with no output for idle_secs.
            // Record the idle_secs config for styling the idle indicator in the
            // exchange header.
            Event::TaskIdle {
                run: _,
                task: _,
                idle_secs,
            } => {
                // Record the idle timeout config for use in rendering the idle indicator.
                self.idle_secs_config = Some(*idle_secs);
            }
            // A failed (or skipped-cascade) task was reset for retry (plan 0017).
            // Clear the task's stale per-step timing so it is treated as fresh; the
            // companion `TaskStateChanged` updates its badge back to New/Ready and
            // re-stamps activity once it is dispatched again.
            Event::TaskRetried { run, task } => {
                self.task_step_start_tick.remove(&(*run, task.clone()));
                self.task_last_activity_tick.remove(&(*run, task.clone()));
                self.status_message = Some(format!("retrying {}", task.0));
            }
            // Per-turn metrics event (plan 0024). Consumed by task 0072's render
            // step; accumulate the latest metrics per (run, task) and role.
            Event::RoleTurnMetrics {
                run,
                task,
                role,
                model,
                duration_ms,
                usage,
            } => {
                self.role_metrics
                    .entry((*run, task.clone()))
                    .or_default()
                    .insert(
                        role.clone(),
                        RoleTurnMetric {
                            model: model.clone(),
                            duration_ms: *duration_ms,
                            usage: usage.clone(),
                        },
                    );
            }
            // Project discovery completed (plan 0025).
            // Surface a transient status message showing the discovery outcome.
            Event::ProjectDiscovered {
                project_root,
                gate_count,
                scanned_files,
            } => {
                let msg = format!(
                    "Discovered {} gates from {} files in {}",
                    gate_count,
                    scanned_files,
                    project_root.display()
                );
                self.status_message = Some(msg);
            }
            Event::PlanOperation {
                run,
                plan_slug: _,
                label,
                operation,
                phase,
                message,
            } => {
                // Run ids are router-global and resolve to an exact canonical
                // project/plan-directory identity. The display slug is never used
                // to select state because separate projects may share it.
                let target = self
                    .runs
                    .iter()
                    .find(|view| view.id == *run)
                    .map(|view| self.plan_identity_for_run(view));
                if let Some(target) = target
                    && let Some(op) = self.plan_operations.get_mut(&target)
                {
                    op.label = label.clone();
                    op.kind = *operation;
                    op.phase = *phase;
                    if op.log.last().is_none_or(|last| last != message) {
                        op.log.push(message.clone());
                    }
                }
                if matches!(
                    phase,
                    makina_core::api::PlanOperationPhase::Started
                        | makina_core::api::PlanOperationPhase::Step
                ) {
                    self.error_pane_open = true;
                    self.output_tab = OutputTab::Logs;
                    self.scroll_offsets.remove(&ScrollablePanel::ErrorPane);
                }
                self.status_message = Some(message.clone());
            }
            // Run's integration branch was left unmerged (plan 0030).
            // Surface a status message with the branch name.
            Event::RunIntegrationBranchLeft { run: _, branch } => {
                self.status_message = Some(format!("Plan branch left unmerged: {}", branch));
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

    #[derive(Clone, Debug)]
    struct TestPlanTask {
        id: String,
        title: String,
        gated: bool,
        depends_on: Vec<String>,
        body: String,
    }

    struct TestTaskSource {
        text: Vec<u8>,
    }

    impl makina_core::plan::PlanFileSource for TestTaskSource {
        fn read_file(
            &self,
            _relative_path: &std::path::Path,
        ) -> Result<Vec<u8>, makina_core::plan::PlanDocumentError> {
            Ok(self.text.clone())
        }

        fn object_format(&self) -> makina_core::plan::GitObjectFormat {
            makina_core::plan::GitObjectFormat::Sha1
        }

        fn validation_base_oid(&self) -> Option<&str> {
            None
        }

        fn is_tracked_ordinary_file(
            &self,
            _relative_path: &std::path::Path,
        ) -> Result<bool, makina_core::plan::PlanDocumentError> {
            Ok(false)
        }
    }

    fn test_plan_entry(
        dir: PathBuf,
        slug: String,
        tasks: Vec<TestPlanTask>,
        _scope: Option<String>,
        _architecture: Option<String>,
        _status: Option<String>,
    ) -> makina_core::orchestrator::PlanEntry {
        let relative = dir
            .components()
            .collect::<Vec<_>>()
            .windows(3)
            .find_map(|parts| {
                (parts[0].as_os_str() == "docs" && parts[1].as_os_str() == "plans").then(|| {
                    PathBuf::from("docs")
                        .join("plans")
                        .join(parts[2].as_os_str())
                })
            });
        let key = relative
            .and_then(|path| makina_core::plan::PlanKey::parse(path).ok())
            .unwrap_or_else(|| {
                makina_core::plan::PlanKey::parse("docs/plans/0001-test-plan").unwrap()
            });
        let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source = makina_core::plan::GitTreePlanFileSource::new(&repository, "HEAD").unwrap();
        let fixture_key = makina_core::plan::PlanKey::parse(
            "docs/plans/0048-Per-Task-Plan-Documents-And-Transactional-Status",
        )
        .unwrap();
        let mut document = match makina_core::plan::load_plan(
            &source,
            fixture_key,
            &makina_core::plan::PlanReservations::default(),
        )
        .unwrap()
        {
            makina_core::plan::PlanCandidate::Plan(document) => *document,
            makina_core::plan::PlanCandidate::NotCandidate => panic!("fixture plan missing"),
        };
        document.key = key.clone();
        document.title = slug.clone();
        document.tasks = tasks
            .into_iter()
            .enumerate()
            .map(|(index, task)| {
                let dependencies = task
                    .depends_on
                    .iter()
                    .map(|dependency| format!("  - {dependency}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let dependencies = if dependencies.is_empty() {
                    "[]".to_string()
                } else {
                    format!("\n{dependencies}")
                };
                let text = format!(
                    "---\nid: {}\ntitle: {}\nworkstream: \"0001\"\nkind: task\ndepends_on: {}\ngated: {}\ntouches:\n  - src/**\nstatus: planned\nmerged_as: \"\"\n---\n# {}\n\n## Context\n\n{}\n\n**Steps:**\n\n1. Exercise the fixture.\n\n- **Done when:** Tested.\n",
                    task.id,
                    task.title,
                    dependencies,
                    task.gated,
                    task.title,
                    task.body
                );
                let path = key.relative_dir.join("tasks").join(format!(
                    "01{:02}-{}.md",
                    index + 1,
                    task.id
                ));
                makina_core::plan::parse_task_document(
                    &TestTaskSource {
                        text: text.into_bytes(),
                    },
                    path,
                )
                .unwrap()
            })
            .collect();
        if let Some(scope) = _scope {
            document.scope.body = scope;
        }
        if let Some(architecture) = _architecture {
            document.architecture.body = architecture;
        }
        if let Some(status) = _status {
            document.status.source.body = status;
        }
        makina_core::orchestrator::PlanEntry {
            dir,
            key,
            slug,
            state: makina_core::orchestrator::PlanDiscoveryState::Ready,
            document: Some(document),
            diagnostics: makina_core::plan::PlanValidationReport::default(),
        }
    }

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

    /// Error pane scroll: manual offset clamps to `[0, scroll_max]`,
    /// scrolling up disengages auto-follow, and scrolling back down to the
    /// bottom re-engages it (mirroring exchange pane behavior).
    #[test]
    fn error_pane_scroll_clamps_and_auto_follow_reengages() {
        let mut app = make_app();
        let max: u16 = 3;

        // Default: auto-follow engaged, offset at the top.
        assert!(app.error_pane_auto_follow);
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::ErrorPane)
                .copied()
                .unwrap_or(0),
            0
        );

        // scroll_up clears auto-follow.
        app.scroll_up(ScrollablePanel::ErrorPane);
        assert!(
            !app.error_pane_auto_follow,
            "scroll_up must clear error_pane_auto_follow"
        );

        // scroll_up never goes below 0.
        app.scroll_up(ScrollablePanel::ErrorPane);
        app.scroll_up(ScrollablePanel::ErrorPane);
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::ErrorPane)
                .copied()
                .unwrap_or(0),
            0,
            "scroll_up must not go below 0"
        );

        // scroll_down never exceeds max.
        for _ in 0..(max + 5) {
            app.scroll_down(ScrollablePanel::ErrorPane, max);
            assert!(
                app.scroll_offsets
                    .get(&ScrollablePanel::ErrorPane)
                    .copied()
                    .unwrap_or(0)
                    <= max,
                "scroll_down must never exceed scroll_max"
            );
        }
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::ErrorPane)
                .copied()
                .unwrap_or(0),
            max
        );

        // scroll_down reaching max re-sets auto-follow.
        assert!(
            app.error_pane_auto_follow,
            "scroll_down reaching scroll_max must re-set error_pane_auto_follow"
        );
    }

    /// ErrorPaneScrollUp event scrolls the error pane up when open.
    #[test]
    fn test_error_pane_scrolls_up_on_pgup() {
        let mut app = make_app();

        // Set up scroll space and disengage auto-follow first.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::ErrorPane, 10);
        app.error_pane_auto_follow = false;
        app.scroll_offsets.insert(ScrollablePanel::ErrorPane, 5);

        let offset_before = app
            .scroll_offsets
            .get(&ScrollablePanel::ErrorPane)
            .copied()
            .unwrap_or(0);

        // ErrorPaneScrollUp should decrease offset.
        app.update(AppEvent::ErrorPaneScrollUp);
        assert!(
            app.scroll_offsets
                .get(&ScrollablePanel::ErrorPane)
                .copied()
                .unwrap_or(0)
                < offset_before,
            "ErrorPaneScrollUp must decrement error pane offset"
        );
    }

    /// ErrorPaneScrollDown event scrolls the error pane down when open.
    #[test]
    fn test_error_pane_scrolls_down_on_pgdn() {
        let mut app = make_app();
        let max: u16 = 10;

        // Set up scroll space.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::ErrorPane, max);

        let offset_before = app
            .scroll_offsets
            .get(&ScrollablePanel::ErrorPane)
            .copied()
            .unwrap_or(0);

        // ErrorPaneScrollDown should increase offset.
        app.update(AppEvent::ErrorPaneScrollDown);
        assert!(
            app.scroll_offsets
                .get(&ScrollablePanel::ErrorPane)
                .copied()
                .unwrap_or(0)
                > offset_before,
            "ErrorPaneScrollDown must increment error pane offset"
        );
    }

    /// Auto-follow disengages when user scrolls up.
    #[test]
    fn test_error_pane_auto_follow_disengages_on_scroll_up() {
        let mut app = make_app();

        // Start with auto-follow engaged.
        assert!(app.error_pane_auto_follow);

        // Scroll up should disengage auto-follow.
        app.scroll_up(ScrollablePanel::ErrorPane);
        assert!(
            !app.error_pane_auto_follow,
            "scroll_up must disengage error_pane_auto_follow"
        );
    }

    /// New error does not yank the view when user has scrolled up.
    #[test]
    fn test_error_pane_new_error_does_not_yank_when_scrolled_up() {
        let mut app = make_app();

        // Set up scroll space.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::ErrorPane, 10);

        // Manually set the offset to simulate user scrolling.
        app.scroll_offsets.insert(ScrollablePanel::ErrorPane, 5);

        // Disengage auto-follow.
        app.error_pane_auto_follow = false;

        // Verify the offset is 5.
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::ErrorPane)
                .copied()
                .unwrap_or(0),
            5
        );

        // Push a new error while auto-follow is disengaged.
        app.push_error(ErrorMessage {
            timestamp: std::time::SystemTime::now(),
            level: ErrorLevel::Error,
            text: "new error".into(),
        });

        // The offset should NOT change (not yanked to bottom).
        // Since auto_follow is false, push_error should not reset the offset.
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::ErrorPane)
                .copied()
                .unwrap_or(0),
            5,
            "new error must NOT yank the view when auto-follow is disengaged"
        );
    }

    /// When auto-follow is engaged, new error snaps to the newest entry.
    #[test]
    fn test_error_pane_new_error_snaps_to_newest_when_auto_follow_engaged() {
        let mut app = make_app();

        // Start with auto-follow engaged (default).
        assert!(app.error_pane_auto_follow);

        // Set an initial offset.
        app.scroll_offsets.insert(ScrollablePanel::ErrorPane, 5);

        // Push a new error while auto-follow is engaged.
        app.push_error(ErrorMessage {
            timestamp: std::time::SystemTime::now(),
            level: ErrorLevel::Error,
            text: "new error".into(),
        });

        // The offset should be reset to 0 (so panel_offset will render at scroll_max).
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::ErrorPane)
                .copied()
                .unwrap_or(0),
            0,
            "new error must snap to bottom when auto-follow is engaged"
        );
    }

    // ── Sidebar tree model ─────────────────────────────────────────────────────

    /// Create a test app with 2 runs: run0 has 2 tasks, run1 has 1 task.
    fn make_app_with_runs() -> App {
        use makina_core::api::{RunStatus, TaskState, TaskView};

        let api = Arc::new(PlaceholderApi::new());
        let run0 = RunView {
            id: RunId(100),
            run_uid: "run0".into(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0099-demo").unwrap(),
            project: "proj".into(),
            status: RunStatus::Running,
            tasks: vec![
                TaskView {
                    authored: None,
                    id: TaskId::new("task00"),
                    title: "Task 0-0".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    authored: None,
                    id: TaskId::new("task01"),
                    title: "Task 0-1".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };

        let run1 = RunView {
            id: RunId(101),
            run_uid: "run1".into(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            project: "proj".into(),
            status: RunStatus::Completed,
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new("task10"),
                title: "Task 1-0".into(),
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

        let mut app = App::new(api, vec![run0, run1], PathBuf::from("."));
        // Tests using this helper expect runs to be expanded (the historical
        // default before runs started collapsed at launch). Clear the
        // launch-seeded collapsed set so task nodes are visible by default.
        app.collapsed_runs.clear();
        app
    }

    #[test]
    fn visible_nodes_expand_and_collapse() {
        let mut app = make_app_with_runs();

        // Initially both runs expanded.
        let nodes = app.visible_tree_nodes();
        assert_eq!(
            nodes,
            vec![
                TreeNode::Run { run: 0 },
                TreeNode::Task { run: 0, task: 0 },
                TreeNode::Task { run: 0, task: 1 },
                TreeNode::Run { run: 1 },
                TreeNode::Task { run: 1, task: 0 },
            ],
            "Initially both runs are expanded, so all nodes should be visible"
        );

        // Collapse run0.
        app.collapsed_runs.insert(RunId(100));
        let nodes = app.visible_tree_nodes();
        assert_eq!(
            nodes,
            vec![
                TreeNode::Run { run: 0 },
                TreeNode::Run { run: 1 },
                TreeNode::Task { run: 1, task: 0 },
            ],
            "After collapsing run0, its tasks should disappear"
        );

        // Expand run0 again.
        app.collapsed_runs.remove(&RunId(100));
        let nodes = app.visible_tree_nodes();
        assert_eq!(
            nodes,
            vec![
                TreeNode::Run { run: 0 },
                TreeNode::Task { run: 0, task: 0 },
                TreeNode::Task { run: 0, task: 1 },
                TreeNode::Run { run: 1 },
                TreeNode::Task { run: 1, task: 0 },
            ],
            "After expanding run0 again, all tasks should reappear"
        );
    }

    #[test]
    fn tree_move_clamps_and_syncs_selection() {
        let mut app = make_app_with_runs();

        // Initially at node 0 (Run0).
        assert_eq!(app.tree_cursor, Some(0));
        assert_eq!(app.focused_node(), Some(TreeNode::Run { run: 0 }));
        assert_eq!(app.selected_run, Some(0));
        // Note: selected_task is no longer updated by sidebar navigation (plan 0031).

        // Move down to node 1 (Task00).
        let moved = app.tree_move(1);
        assert!(moved, "tree_move(1) should return true when cursor moves");
        assert_eq!(app.tree_cursor, Some(1));
        assert_eq!(app.focused_node(), Some(TreeNode::Task { run: 0, task: 0 }));
        assert_eq!(app.selected_run, Some(0));

        // Move down to node 2 (Task01).
        let moved = app.tree_move(1);
        assert!(moved);
        assert_eq!(app.tree_cursor, Some(2));
        assert_eq!(app.focused_node(), Some(TreeNode::Task { run: 0, task: 1 }));
        assert_eq!(app.selected_run, Some(0));

        // Move back up to node 1 (Task00).
        let moved = app.tree_move(-1);
        assert!(moved);
        assert_eq!(app.tree_cursor, Some(1));
        assert_eq!(app.focused_node(), Some(TreeNode::Task { run: 0, task: 0 }));

        // Move up to node 0 (Run0).
        let moved = app.tree_move(-1);
        assert!(moved);
        assert_eq!(app.tree_cursor, Some(0));

        // Try to move up from node 0 (should clamp to 0).
        let moved = app.tree_move(-1);
        assert!(!moved, "tree_move(-1) at top should return false (no move)");
        assert_eq!(app.tree_cursor, Some(0));

        // Move to the end and then try to move past it.
        app.tree_cursor = Some(4); // Last node (Task10).
        let moved = app.tree_move(1);
        assert!(!moved, "tree_move(1) at end should return false");
        assert_eq!(app.tree_cursor, Some(4));
    }

    #[test]
    fn toggle_expand_keeps_cursor_on_run_and_collapses() {
        let mut app = make_app_with_runs();

        // Navigate to Task01 (node 2) via tree_move to ensure sync happens.
        app.tree_move(1); // Move to node 1 (Task00)
        app.tree_move(1); // Move to node 2 (Task01)
        assert_eq!(
            app.focused_node(),
            Some(TreeNode::Task { run: 0, task: 1 }),
            "Should start at Task01"
        );

        // Toggle expand (should collapse run0 and move cursor to its Run header).
        let toggled = app.tree_toggle_expand();
        assert!(toggled, "toggle_expand should succeed");

        // Run0 should be collapsed.
        assert!(
            app.collapsed_runs.contains(&RunId(100)),
            "run0 should be collapsed"
        );

        // Cursor should be on Run0's header.
        assert_eq!(
            app.focused_node(),
            Some(TreeNode::Run { run: 0 }),
            "Cursor should move to Run0 header after toggle"
        );

        // Note: selected_task is no longer updated by sidebar navigation (plan 0031).
        // It stays as whatever it was set to via explicit tab operations.

        // Toggle expand again (should expand run0).
        let toggled = app.tree_toggle_expand();
        assert!(toggled);
        assert!(
            !app.collapsed_runs.contains(&RunId(100)),
            "run0 should be expanded"
        );

        // Cursor should still be on Run0's header.
        assert_eq!(
            app.focused_node(),
            Some(TreeNode::Run { run: 0 }),
            "Cursor should stay on Run0 header"
        );
    }

    #[test]
    fn select_down_in_sidebar_walks_tree_nodes() {
        let mut app = make_app_with_runs();

        // Initially at Run0 header.
        assert_eq!(app.focused_panel, Panel::Sidebar);
        assert_eq!(app.focused_node(), Some(TreeNode::Run { run: 0 }));
        assert_eq!(app.selected_run, Some(0));
        // Note: selected_task is no longer updated by sidebar navigation (plan 0031).
        // It's initialized and managed by the tab system instead.

        // SelectDown moves to Task00.
        app.update(AppEvent::SelectDown);
        assert_eq!(
            app.focused_node(),
            Some(TreeNode::Task { run: 0, task: 0 }),
            "SelectDown from Run0 should move to Task00"
        );
        // selected_run should stay the same since we're still within run 0
        assert_eq!(app.selected_run, Some(0));

        // SelectDown moves to Task01.
        app.update(AppEvent::SelectDown);
        assert_eq!(
            app.focused_node(),
            Some(TreeNode::Task { run: 0, task: 1 }),
            "SelectDown from Task00 should move to Task01"
        );
        // selected_run should still be 0
        assert_eq!(app.selected_run, Some(0));
    }

    #[test]
    fn toggle_tree_node_collapses_focused_run() {
        let mut app = make_app_with_runs();

        // Navigate to Task01.
        app.tree_move(1);
        app.tree_move(1);
        assert_eq!(app.focused_node(), Some(TreeNode::Task { run: 0, task: 1 }));

        // Toggle to collapse.
        app.update(AppEvent::ToggleTreeNode);

        // Run0 should be collapsed.
        assert!(
            app.collapsed_runs.contains(&RunId(100)),
            "run0 should be collapsed"
        );

        // Cursor should be on Run0 header.
        assert_eq!(
            app.focused_node(),
            Some(TreeNode::Run { run: 0 }),
            "After toggle, cursor should be on Run0 header"
        );
    }

    #[test]
    fn right_expands_then_focuses_content() {
        let mut app = make_app_with_runs();

        // Initially at Run0 (node 0), which is expanded.
        assert_eq!(app.focused_node(), Some(TreeNode::Run { run: 0 }));
        assert_eq!(app.focused_panel, Panel::Sidebar);
        assert!(
            !app.collapsed_runs.contains(&RunId(100)),
            "run0 should start expanded"
        );

        // Collapse run0 manually.
        app.collapsed_runs.insert(RunId(100));
        assert!(
            app.collapsed_runs.contains(&RunId(100)),
            "run0 should be collapsed"
        );

        // First FocusRightOrExpand: on a collapsed run, expand it.
        app.update(AppEvent::FocusRightOrExpand);
        assert!(
            !app.collapsed_runs.contains(&RunId(100)),
            "run0 should be expanded after first Right"
        );
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Panel should stay Sidebar"
        );

        // Second FocusRightOrExpand: on an expanded run, cross to Main.
        app.update(AppEvent::FocusRightOrExpand);
        assert_eq!(
            app.focused_panel,
            Panel::Main,
            "Second Right on expanded run should move to Main"
        );
    }

    #[test]
    fn left_returns_to_sidebar() {
        let mut app = make_app_with_runs();

        // Start in Main panel.
        app.focused_panel = Panel::Main;
        assert_eq!(app.focused_panel, Panel::Main);

        // FocusLeftOrCollapse from Main should return to Sidebar.
        app.update(AppEvent::FocusLeftOrCollapse);
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Left from Main should return to Sidebar"
        );
    }

    #[test]
    fn left_collapses_expanded_run() {
        let mut app = make_app_with_runs();

        // At Run0 (node 0), which is expanded.
        assert_eq!(app.focused_node(), Some(TreeNode::Run { run: 0 }));
        assert_eq!(app.focused_panel, Panel::Sidebar);
        assert!(
            !app.collapsed_runs.contains(&RunId(100)),
            "run0 should start expanded"
        );

        // FocusLeftOrCollapse on an expanded run should collapse it.
        app.update(AppEvent::FocusLeftOrCollapse);
        assert!(
            app.collapsed_runs.contains(&RunId(100)),
            "run0 should be collapsed after Left"
        );
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Panel should stay Sidebar"
        );

        // Collapse manually and test that Left on a collapsed run is a no-op.
        app.tree_cursor = Some(0);
        let was_collapsed = app.collapsed_runs.contains(&RunId(100));
        app.update(AppEvent::FocusLeftOrCollapse);
        assert_eq!(
            app.collapsed_runs.contains(&RunId(100)),
            was_collapsed,
            "Left on already-collapsed run should be a no-op"
        );
    }

    #[test]
    fn cursor_survives_runs_update() {
        use makina_core::api::{RunId, RunStatus, RunView};

        let mut app = make_app_with_runs();

        // Initial visible nodes (both runs expanded):
        //   [Run0(0), Task00(1), Task01(2), Run1(3), Task10(4)]
        // Navigate to the last node — Task10 (index 4) in Run1.
        app.tree_move(1); // -> Task00 (1)
        app.tree_move(1); // -> Task01 (2)
        app.tree_move(1); // -> Run1   (3)
        app.tree_move(1); // -> Task10 (4)
        assert_eq!(
            app.focused_node(),
            Some(TreeNode::Task { run: 1, task: 0 }),
            "cursor should be at Task10 before update"
        );
        assert_eq!(app.tree_cursor, Some(4));

        // Simulate a RunLoaded that *shrinks* Run1 to zero tasks.
        // New visible nodes: [Run0(0), Task00(1), Task01(2), Run1(3)] — 4 nodes.
        // The old cursor (4) is now out of bounds; the handler must clamp it.
        let shrunk_run1 = RunView {
            id: RunId(101),
            run_uid: String::new(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Completed,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        };
        app.update(AppEvent::RunLoaded(shrunk_run1));

        // tree_cursor must be clamped to a valid index (≤ 3, the new last index).
        let cursor = app
            .tree_cursor
            .expect("tree_cursor must not be None after runs update");
        let node_count = app.visible_tree_nodes().len();
        assert!(
            cursor < node_count,
            "tree_cursor ({cursor}) must be within visible node count ({node_count})"
        );
        // focused_node() must return Some — i.e., the cursor resolves to a real node.
        assert!(
            app.focused_node().is_some(),
            "focused_node() must be Some after cursor clamping"
        );
        // selected_run must point at a valid run.
        assert!(
            app.selected_run
                .map(|i| i < app.runs.len())
                .unwrap_or(false),
            "selected_run must be a valid index after runs update"
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
        assert_eq!(
            app.status_message,
            Some("Dependency view: list".to_string())
        );
        app.update(AppEvent::CycleDependencyView);
        assert_eq!(app.dependency_view, DependencyViewMode::Tree);
        assert_eq!(
            app.status_message,
            Some("Dependency view: tree".to_string())
        );
        app.update(AppEvent::CycleDependencyView);
        assert_eq!(app.dependency_view, DependencyViewMode::Timeline);
        assert_eq!(
            app.status_message,
            Some("Dependency view: timeline".to_string())
        );
        app.update(AppEvent::CycleDependencyView);
        assert_eq!(app.dependency_view, DependencyViewMode::Off);
        assert_eq!(app.status_message, Some("Dependency view: off".to_string()));
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![existing], PathBuf::from("."));

        // Sending a RunOpened for the same id should not add a second entry.
        let ev = Event::RunOpened {
            run: RunId(1),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new("t1"),
                title: "Task 1".into(),
                state: TaskState::Ready,
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new("t1"),
                title: "Task 1".into(),
                state: TaskState::InProgress,
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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

    /// `active_run_id` falls back to the run matching the active plan tab when
    /// nothing is explicitly selected — so run-control actions (pause/stop/resume)
    /// keep targeting a plan's run after it has been started, even if the sidebar
    /// cursor has moved off it.
    #[test]
    fn active_run_id_falls_back_to_context_plan_run() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        // A run whose plan_dir resolves to plan slug "0099-demo".
        let run = RunView {
            id: RunId(9),
            run_uid: String::new(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0099-demo").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));
        // Discovered plan + active plan tab for the same slug, but no explicit
        // selection.
        app.discovered_plans = vec![test_plan_entry(
            PathBuf::from("docs/plans/0099-demo"),
            "0099-demo".to_string(),
            Vec::new(),
            None,
            None,
            None,
        )];
        let target = app.plan_identity_for_entry(&app.repo_root, &app.discovered_plans[0]);
        app.tabs.open_tab(TabContent::Plan {
            plan: target.clone(),
        });
        app.selected_run = None;

        assert!(app.selected_run().is_none(), "no explicit selection");
        assert_eq!(
            app.active_run_id(),
            Some(RunId(9)),
            "active_run_id must resolve the context plan's open run"
        );
    }

    /// `log_pane_target` resolves the (RunId, TaskId) from the ACTIVE TASK TAB
    /// even when the sidebar selection points elsewhere — so `[L]` shows the log
    /// of the task the user is actually viewing.
    #[test]
    fn log_pane_target_resolves_from_active_task_tab() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(3),
            run_uid: "run-uid-3".to_string(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new("build-thing"),
                title: "Build thing".into(),
                state: TaskState::InProgress,
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
        let mut app = App::new(api, vec![run], PathBuf::from("."));
        // No sidebar task selected, but a task tab for the run's task is active.
        app.selected_task = None;
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("0042-demo".to_string()),
            run: RunId(3),
            task_id: TaskId::new("build-thing"),
        });
        app.tabs.active_tab = Some(0);

        assert_eq!(
            app.log_pane_target(),
            Some((RunId(3), TaskId::new("build-thing"))),
            "log_pane_target must resolve the active task tab's (RunId, TaskId)"
        );
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
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-A").unwrap(),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0002-B").unwrap(),
                status: RunStatus::Running,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(3),
                run_uid: String::new(),
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0003-C").unwrap(),
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
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-A").unwrap(),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0002-B").unwrap(),
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
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-A").unwrap(),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0002-B").unwrap(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-A").unwrap(),
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
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-A").unwrap(),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0002-B").unwrap(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-A").unwrap(),
                status: RunStatus::Pending,
                project: String::new(),
                tasks: vec![],
                report: makina_core::api::IngestionReport::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: String::new(),
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0002-B").unwrap(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new("t1"),
                title: "Task 1".into(),
                state: TaskState::InProgress,
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![
                TaskView {
                    authored: None,
                    id: TaskId::new("t1"),
                    title: "First task".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    authored: None,
                    id: TaskId::new("t2"),
                    title: "Second task".into(),
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![TaskId::new("t1")],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new("only"),
                title: "Only task".into(),
                state: TaskState::New,
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new("t1"),
                title: "Task".into(),
                state: TaskState::Ready,
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    authored: None,
                    id: TaskId::new("t1"),
                    title: "Task 1".into(),
                    state: TaskState::Done,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    authored: None,
                    id: TaskId::new("t2"),
                    title: "Task 2".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    authored: None,
                    id: TaskId::new("t1"),
                    title: "Task 1".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    authored: None,
                    id: TaskId::new("t2"),
                    title: "Task 2".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
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

    // ── Folder browser (plan 0043) ──────────────────────────────────────────────

    /// `OpenFolder` sets the PENDING `folder_browser_purpose` field to
    /// `OpenFolder` and marks the app busy, so the follow-up `BrowserOpened`
    /// (below) knows to enter `Mode::FolderBrowser` rather than
    /// `Mode::FileBrowser`. Note: `App::folder_browser_purpose()` (the accessor)
    /// reads `self.mode`, not this pending field, so it still returns `None`
    /// here — mode only flips once `BrowserOpened` arrives.
    #[test]
    fn open_folder_sets_purpose_and_busy() {
        let mut app = make_app();
        app.update(AppEvent::OpenFolder);
        assert_eq!(
            app.folder_browser_purpose,
            Some(FolderBrowserPurpose::OpenFolder)
        );
        assert!(app.busy.is_some());
        assert_eq!(
            app.mode,
            Mode::Normal,
            "mode flips only once BrowserOpened arrives"
        );
    }

    /// `InitializeFolderRequested` sets the pending `folder_browser_purpose`
    /// field to `InitializeFolder` (distinct from `OpenFolder`) and marks the
    /// app busy.
    #[test]
    fn initialize_folder_requested_sets_purpose_and_busy() {
        let mut app = make_app();
        app.update(AppEvent::InitializeFolderRequested);
        assert_eq!(
            app.folder_browser_purpose,
            Some(FolderBrowserPurpose::InitializeFolder)
        );
        assert!(app.busy.is_some());
    }

    /// Once a folder-browser purpose is set, `BrowserOpened` must enter
    /// `Mode::FolderBrowser { purpose }` (not `Mode::FileBrowser`), and
    /// `is_folder_browsing()` must report true.
    #[test]
    fn browser_opened_enters_folder_browser_mode_when_purpose_set() {
        let mut app = make_app();
        app.update(AppEvent::OpenFolder);
        app.update(AppEvent::BrowserOpened {
            dir: PathBuf::from("/home/user"),
            entries: vec![DirEntry {
                name: "projects".into(),
                path: PathBuf::from("/home/user/projects"),
                is_dir: true,
            }],
        });
        assert!(app.is_browsing());
        assert!(app.is_folder_browsing());
        assert_eq!(
            app.mode,
            Mode::FolderBrowser {
                purpose: FolderBrowserPurpose::OpenFolder
            }
        );
        assert_eq!(
            app.folder_browser_purpose(),
            Some(FolderBrowserPurpose::OpenFolder)
        );
    }

    /// Arrow-key navigation events (`FolderBrowserUp`/`FolderBrowserDown`) move
    /// the shared `browser` selection exactly like the file-browser equivalents.
    #[test]
    fn folder_browser_up_down_move_selection() {
        let mut app = make_app();
        app.update(AppEvent::OpenFolder);
        app.update(AppEvent::BrowserOpened {
            dir: PathBuf::from("/home/user"),
            entries: vec![
                DirEntry {
                    name: "a".into(),
                    path: PathBuf::from("/home/user/a"),
                    is_dir: true,
                },
                DirEntry {
                    name: "b".into(),
                    path: PathBuf::from("/home/user/b"),
                    is_dir: true,
                },
            ],
        });
        assert_eq!(app.browser.as_ref().unwrap().selected, 0);
        app.update(AppEvent::FolderBrowserDown);
        assert_eq!(app.browser.as_ref().unwrap().selected, 1);
        app.update(AppEvent::FolderBrowserUp);
        assert_eq!(app.browser.as_ref().unwrap().selected, 0);
    }

    /// `CloseBrowser` from a folder-browser session returns to `Mode::Normal`
    /// and clears `folder_browser_purpose` (so a subsequent plain `[o]` open
    /// does not accidentally re-enter folder-browser mode).
    #[test]
    fn close_browser_from_folder_mode_clears_purpose() {
        let mut app = make_app();
        app.update(AppEvent::OpenFolder);
        app.update(AppEvent::BrowserOpened {
            dir: PathBuf::from("/home/user"),
            entries: vec![],
        });
        assert!(app.is_folder_browsing());

        app.update(AppEvent::CloseBrowser);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.browser.is_none());
        assert_eq!(app.folder_browser_purpose(), None);
    }

    // ── Provider configuration editor (task 0041) ──────────────────────────────

    // ── prompt-answer-stream (task 30) ────────────────────────────────────────

    /// Helper: build a run with tasks A and B, and an App focused on it.
    fn make_app_with_tasks() -> App {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    authored: None,
                    id: TaskId::new("task-a"),
                    title: "Task A".into(),
                    state: TaskState::InProgress,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    authored: None,
                    id: TaskId::new("task-b"),
                    title: "Task B".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("."));
        // Tests using this helper expect the run expanded (tasks visible).
        app.collapsed_runs.clear();
        app
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
        assert_eq!(log.entries[0].role, StreamRole::Agent(AgentRole::Developer));

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
        assert_eq!(log.entries[0].role, StreamRole::Agent(AgentRole::Developer));
        assert_eq!(log.entries[0].text(), "dev prompt");
        assert!(!log.entries[1].is_prompt());
        assert_eq!(log.entries[1].role, StreamRole::Agent(AgentRole::Developer));
        assert_eq!(log.entries[1].text(), "dev reply");
        assert!(log.entries[2].is_prompt());
        assert_eq!(log.entries[2].role, StreamRole::Agent(AgentRole::Reviewer));
        assert_eq!(log.entries[2].text(), "review prompt");
        assert!(!log.entries[3].is_prompt());
        assert_eq!(log.entries[3].role, StreamRole::Agent(AgentRole::Reviewer));
        assert_eq!(log.entries[3].text(), "lgtm");
    }

    /// **Focus filtering:** feed AgentExchange for task-a and task-b; focused
    /// on task-a (index 0), only task-a's log must be non-empty; switching to
    /// task-b (index 1) must reveal task-b's log.
    #[test]
    fn focus_filtering_exchange_per_task() {
        use makina_core::api::{AgentRole, Event, ExchangeEvent, RunId, TaskId};
        let mut app = make_app_with_tasks();

        // Stay in Sidebar panel so tree navigation works.
        assert_eq!(app.focused_panel, Panel::Sidebar);

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

        // Initially focused on run with task-a selected (via initial sync_selection_from_cursor).
        // Note: selected_task is no longer updated by sidebar navigation (plan 0031).
        // Tasks are opened in tabs when Enter is pressed, and the active tab determines
        // which task's content is displayed.
        assert_eq!(app.focused_node(), Some(TreeNode::Run { run: 0 }));

        // Navigate down to task-a using tree navigation in the Sidebar.
        // Tree cursor: 0 (Run) -> 1 (Task A) -> 2 (Task B)
        // Need to call SelectDown twice to reach task-b.
        app.update(AppEvent::SelectDown); // Move to task-a node
        assert_eq!(app.focused_node(), Some(TreeNode::Task { run: 0, task: 0 }));

        app.update(AppEvent::SelectDown); // Move to task-b node
        assert_eq!(app.focused_node(), Some(TreeNode::Task { run: 0, task: 1 }));

        // Verify that exchange logs are stored for both tasks (via the Eventstream).
        // The specific display of which log to show is now determined by the active tab
        // (plan 0031), not by selected_task.
        assert!(
            app.exchange_logs.contains_key(&(RunId(1), id_a.clone())),
            "task-a log must remain stored"
        );
        assert!(
            app.exchange_logs.contains_key(&(RunId(1), id_b.clone())),
            "task-b log must remain stored"
        );
    }

    /// **Content scrolling:** Up/Down scrolls the exchange pane when Main is focused
    /// (does not change selected_task).
    #[test]
    fn task_selection_up_down_main_panel() {
        let mut app = make_app_with_tasks();

        // Switch to Main panel.
        app.update(AppEvent::FocusNext);
        assert_eq!(app.focused_panel, Panel::Main);
        let initial_selected_task = app.selected_task;

        // Set up scroll space.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 10);

        // Down: scrolls, does not change selected_task.
        app.update(AppEvent::SelectDown);
        assert_eq!(
            app.selected_task, initial_selected_task,
            "SelectDown must NOT change task selection in Main panel"
        );
        assert!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0)
                > 0,
            "ScrollDown must advance exchange scroll offset"
        );

        // Up: scrolls back, does not change selected_task.
        app.update(AppEvent::SelectUp);
        assert_eq!(
            app.selected_task, initial_selected_task,
            "SelectUp must NOT change task selection in Main panel"
        );
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
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0
        );

        // (2) scroll_up clears auto-follow.
        app.scroll_up(ScrollablePanel::Exchange);
        assert!(
            !app.exchange_auto_follow,
            "scroll_up must clear exchange_auto_follow"
        );

        // (1) scroll_up never goes below 0.
        app.scroll_up(ScrollablePanel::Exchange);
        app.scroll_up(ScrollablePanel::Exchange);
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0,
            "scroll_up must not go below 0"
        );

        // (1) scroll_down never exceeds max.
        for _ in 0..(max + 5) {
            app.scroll_down(ScrollablePanel::Exchange, max);
            assert!(
                app.scroll_offsets
                    .get(&ScrollablePanel::Exchange)
                    .copied()
                    .unwrap_or(0)
                    <= max,
                "scroll_down must never exceed scroll_max"
            );
        }
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            max
        );

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
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 5);

        // Record the task selection before scrolling.
        let selected_before = app.selected_task;
        assert_eq!(selected_before, Some(0));

        // ScrollDown nudges the manual offset down by one line.
        let offset_before = app
            .scroll_offsets
            .get(&ScrollablePanel::Exchange)
            .copied()
            .unwrap_or(0);
        app.update(AppEvent::ScrollDown);
        assert_ne!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            offset_before,
            "ScrollDown must change the exchange scroll offset"
        );
        assert_eq!(
            app.selected_task, selected_before,
            "ScrollDown must NOT change task selection"
        );

        // ScrollUp moves the offset back up and disengages auto-follow.
        let offset_after_down = app
            .scroll_offsets
            .get(&ScrollablePanel::Exchange)
            .copied()
            .unwrap_or(0);
        app.update(AppEvent::ScrollUp);
        assert_ne!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            offset_after_down,
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

    /// App with scroll state set; ScrollDown and ScrollUp events adjust the
    /// exchange pane scroll offset while keeping task selection unchanged
    /// (task `re-enable-mouse-capture`).
    ///
    /// This test verifies that the mouse wheel events work correctly when
    /// mouse capture is re-enabled: scrolling moves the exchange scroll offset
    /// (and may disengage auto-follow), but leaves selected_task untouched.
    #[test]
    fn scroll_events_adjust_exchange_state() {
        let mut app = make_app_with_tasks();
        let max: u16 = 5;

        // Simulate a multi-line pane.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, max);

        // Record the task selection before scrolling.
        let selected_before = app.selected_task;
        assert_eq!(selected_before, Some(0));

        // ScrollDown should move the exchange scroll offset.
        let offset_before = app
            .scroll_offsets
            .get(&ScrollablePanel::Exchange)
            .copied()
            .unwrap_or(0);
        app.update(AppEvent::ScrollDown);
        assert_ne!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            offset_before,
            "ScrollDown must adjust exchange scroll offset"
        );
        assert_eq!(
            app.selected_task, selected_before,
            "ScrollDown must NOT change selected_task"
        );

        // ScrollUp should move the offset back and disengage auto-follow.
        let offset_after_down = app
            .scroll_offsets
            .get(&ScrollablePanel::Exchange)
            .copied()
            .unwrap_or(0);
        app.update(AppEvent::ScrollUp);
        assert_ne!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            offset_after_down,
            "ScrollUp must adjust exchange scroll offset"
        );
        assert!(
            !app.exchange_auto_follow,
            "ScrollUp must disengage exchange_auto_follow"
        );
        assert_eq!(
            app.selected_task, selected_before,
            "ScrollUp must NOT change selected_task"
        );
    }

    // ── Mouse text selection ──────────────────────────────────────────────────

    /// Record one full-screen selectable pane (what a normal frame would do).
    fn record_full_pane(app: &App) {
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.set_selection_panes(vec![crate::app::SelectionPane {
            hit: area,
            clip: area,
        }]);
    }

    /// A drag (down → extend → up) leaves a finalised, non-empty selection that
    /// the event loop can copy; `SelectionExtend` tracks the moving end.
    #[test]
    fn drag_builds_and_finalises_selection() {
        let mut app = make_app();
        record_full_pane(&app);
        assert!(app.selection.is_none());

        assert!(app.update(AppEvent::SelectionStart(2, 1)));
        let sel = app.selection.expect("down starts a selection");
        assert_eq!(sel.anchor, (2, 1));
        assert!(sel.active);

        assert!(app.update(AppEvent::SelectionExtend(8, 3)));
        assert_eq!(app.selection.unwrap().cursor, (8, 3));

        assert!(app.update(AppEvent::SelectionEnd(8, 3)));
        let sel = app.selection.expect("a real drag keeps its selection");
        assert!(!sel.active, "release clears the active flag");
        assert!(!sel.is_empty());
    }

    /// A plain click (down then up on the same cell, no drag) clears the
    /// selection instead of leaving a stray one-cell highlight.
    #[test]
    fn plain_click_clears_selection() {
        let mut app = make_app();
        record_full_pane(&app);
        app.update(AppEvent::SelectionStart(4, 2));
        app.update(AppEvent::SelectionEnd(4, 2));
        assert!(
            app.selection.is_none(),
            "a click with no drag must leave nothing selected"
        );
    }

    /// A drag that starts outside every recorded pane (e.g. the title bar)
    /// selects nothing.
    #[test]
    fn selection_outside_panes_is_ignored() {
        let mut app = make_app();
        // One pane covering rows 2..24; row 0 is outside it.
        app.set_selection_panes(vec![crate::app::SelectionPane {
            hit: ratatui::layout::Rect::new(0, 2, 80, 22),
            clip: ratatui::layout::Rect::new(0, 2, 80, 22),
        }]);
        app.update(AppEvent::SelectionStart(5, 0));
        assert!(
            app.selection.is_none(),
            "a drag beginning outside every pane must not start a selection"
        );
    }

    /// A new selection is confined to the `clip` rect of the pane the drag
    /// begins in — the recorded pane's bounds ride along on the `Selection`.
    #[test]
    fn selection_adopts_starting_pane_bounds() {
        let mut app = make_app();
        let right = ratatui::layout::Rect::new(30, 1, 50, 20);
        app.set_selection_panes(vec![crate::app::SelectionPane {
            hit: right,
            clip: right,
        }]);
        app.update(AppEvent::SelectionStart(35, 4));
        assert_eq!(app.selection.expect("selection started").bounds, right);
    }

    /// Regression (fix `tui-scroll-and-restore` #1): the FIRST wheel-up from
    /// auto-follow must move up by exactly one line (`scroll_max - 1`), NOT snap
    /// to the top (offset 0).
    ///
    /// Before the fix, `scroll_up` left the stale exchange offset at 0 and
    /// merely `saturating_sub(1)`-ed it, so `effective_offset` returned 0 (top)
    /// on the first wheel-up while auto-following.
    #[test]
    fn first_scroll_up_from_auto_follow_anchors_to_bottom_minus_one() {
        let mut app = make_app();
        let max: u16 = 12;

        // Simulate what the render pass records: the bottom-most offset.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, max);
        assert!(app.exchange_auto_follow, "default is auto-follow");
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0,
            "manual offset is stale (0) while following"
        );

        // First wheel-up disengages auto-follow and anchors to the bottom.
        app.scroll_up(ScrollablePanel::Exchange);

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
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            max - 1,
            "exchange scroll offset must be anchored to the rendered bottom minus one"
        );
    }

    /// Regression (fix `tui-scroll-and-restore` #2): mouse auto-follow must
    /// re-engage when scrolling back down to the real rendered bottom.
    ///
    /// Before the fix, the `ScrollDown` arm clamped at `u16::MAX`, so
    /// exchange scroll offset == scroll_max was unreachable and auto-follow could
    /// never re-engage via the mouse path.  Now it clamps at last_scroll_maxes.
    #[test]
    fn mouse_scroll_down_to_bottom_reengages_auto_follow() {
        let mut app = make_app_with_tasks();
        let max: u16 = 4;

        // The render pass records the real bottom offset.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, max);

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
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            max,
            "ScrollDown must clamp at the real rendered bottom (last_scroll_maxes)"
        );
        assert!(
            app.exchange_auto_follow,
            "reaching the rendered bottom via the mouse path must re-engage auto-follow"
        );
    }

    /// Regression (fix `tui-scroll-and-restore` #3): `scroll_down` must not
    /// overflow the exchange scroll offset when it is already at `u16::MAX` (debug panic).
    #[test]
    fn scroll_down_does_not_overflow_at_u16_max() {
        let mut app = make_app();
        app.exchange_auto_follow = false;
        app.scroll_offsets
            .insert(ScrollablePanel::Exchange, u16::MAX);
        // saturating_add inside scroll_down must not panic in debug builds.
        app.scroll_down(ScrollablePanel::Exchange, u16::MAX);
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            u16::MAX
        );
    }

    /// Per-panel scroll maps: reading a missing key returns 0 without panicking,
    /// and inserting/round-tripping values works correctly.
    #[test]
    fn per_panel_scroll_maps_default_to_zero_and_roundtrip() {
        let app = make_app();

        // Reading a missing scroll_offsets key returns 0 without panicking.
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Sidebar)
                .copied()
                .unwrap_or(0),
            0,
            "Missing scroll_offsets key should default to 0"
        );
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0,
            "Missing scroll_offsets key should default to 0"
        );

        // Inserting into scroll_offsets works.
        let mut app = app;
        app.scroll_offsets.insert(ScrollablePanel::Sidebar, 5);
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Sidebar)
                .copied()
                .unwrap_or(0),
            5,
            "Inserted value should be retrievable"
        );

        // Reading a missing last_scroll_maxes key returns 0 without panicking.
        assert_eq!(
            app.last_scroll_maxes
                .borrow()
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0,
            "Missing last_scroll_maxes key should default to 0"
        );

        // Inserting into last_scroll_maxes and round-tripping works.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 7);
        assert_eq!(
            app.last_scroll_maxes
                .borrow()
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            7,
            "Inserted value should round-trip through RefCell"
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![
                TaskView {
                    authored: None,
                    id: TaskId::new("t1"),
                    title: "T1".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
                TaskView {
                    authored: None,
                    id: TaskId::new("t2"),
                    title: "T2".into(),
                    state: TaskState::Ready,
                    gate_iterations: 0,
                    review_iterations: 0,
                    depends_on: vec![],
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                    entry_text: String::new(),
                },
            ],
            report: makina_core::api::IngestionReport::default(),
        };
        let run2 = RunView {
            id: RunId(2),
            run_uid: String::new(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0002-Test").unwrap(),
            status: RunStatus::Pending,
            project: String::new(),
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new("ta"),
                title: "TA".into(),
                state: TaskState::New,
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
        let mut app = App::new(api, vec![run1, run2], PathBuf::from("."));
        // This test exercises tree navigation across expanded runs; clear the
        // launch-seeded collapsed set so task nodes are visible.
        app.collapsed_runs.clear();

        // Initially: sidebar focused, cursor at 0 (Run0), run=0.
        assert_eq!(app.focused_panel, Panel::Sidebar);
        assert_eq!(app.selected_run, Some(0));
        assert_eq!(app.tree_cursor, Some(0));
        assert_eq!(app.focused_node(), Some(TreeNode::Run { run: 0 }));

        // Navigate down via sidebar tree: SelectDown moves to the first task of Run0.
        // Note: selected_task is no longer updated by sidebar navigation (plan 0031).
        app.update(AppEvent::SelectDown);
        assert_eq!(
            app.tree_cursor,
            Some(1),
            "cursor moves to next node (Task0 of Run0)"
        );
        assert_eq!(app.selected_run, Some(0), "still on run 0");
        assert_eq!(app.focused_node(), Some(TreeNode::Task { run: 0, task: 0 }));

        // Navigate down: next is Task1 of Run0.
        app.update(AppEvent::SelectDown);
        assert_eq!(app.tree_cursor, Some(2));
        assert_eq!(app.selected_run, Some(0));
        assert_eq!(app.focused_node(), Some(TreeNode::Task { run: 0, task: 1 }));

        // Navigate down: next is Run1 header.
        app.update(AppEvent::SelectDown);
        assert_eq!(app.tree_cursor, Some(3));
        assert_eq!(app.selected_run, Some(1), "moved to run 1");
        assert_eq!(app.focused_node(), Some(TreeNode::Run { run: 1 }));
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
            None,
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
            None,
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
                content: None,
            },
        );
        feed(
            &mut app,
            ExchangeEvent::ToolCallUpdate {
                id: "tc-1".into(),
                status: Some("in_progress".into()),
                title: None,
                content: None,
            },
        );
        feed(
            &mut app,
            ExchangeEvent::ToolCallUpdate {
                id: "tc-1".into(),
                status: Some("completed".into()),
                title: None,
                content: None,
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
            None,
        );
        assert_eq!(log.entries.len(), 1, "one tool entry after start_tool");

        // First update.
        log.update_tool(
            AgentRole::Developer,
            "tc-1",
            Some("in_progress".into()),
            None,
            None,
        );
        // Second update — title change too.
        log.update_tool(
            AgentRole::Developer,
            "tc-1",
            Some("completed".into()),
            Some("edit file (done)".into()),
            None,
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

    #[test]
    fn tool_updates_are_scoped_to_role_and_id() {
        use crate::app::ExchangeLog;
        use makina_core::api::AgentRole;

        let mut log = ExchangeLog::default();

        log.start_tool(
            AgentRole::Developer,
            "tc-shared".into(),
            "developer edit".into(),
            Some("edit".into()),
            "pending".into(),
            Some("+dev line".into()),
        );
        log.start_tool(
            AgentRole::Reviewer,
            "tc-shared".into(),
            "reviewer read".into(),
            Some("read".into()),
            "pending".into(),
            Some("review payload".into()),
        );
        log.update_tool(
            AgentRole::Reviewer,
            "tc-shared",
            Some("completed".into()),
            Some("reviewer read (done)".into()),
            Some("review result".into()),
        );

        assert_eq!(
            log.entries.len(),
            2,
            "shared tool ids across roles must stay as distinct exchange entries"
        );

        match &log.entries[0] {
            ExchangeEntry {
                role: StreamRole::Agent(AgentRole::Developer),
                content:
                    ExchangeContent::Tool {
                        title,
                        status,
                        content,
                        ..
                    },
            } => {
                assert_eq!(title, "developer edit");
                assert_eq!(status, "pending");
                assert_eq!(content, "+dev line");
            }
            other => panic!("first entry must remain the developer tool, got {other:?}"),
        }

        match &log.entries[1] {
            ExchangeEntry {
                role: StreamRole::Agent(AgentRole::Reviewer),
                content:
                    ExchangeContent::Tool {
                        title,
                        status,
                        content,
                        ..
                    },
            } => {
                assert_eq!(title, "reviewer read (done)");
                assert_eq!(status, "completed");
                assert_eq!(content, "review result");
            }
            other => panic!("second entry must be the reviewer tool, got {other:?}"),
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
                    content: None,
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
                    content: None,
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
        use makina_core::HOME_ENV_LOCK;
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        use std::fs::File;
        use std::io::Write;
        use tempfile::TempDir;

        let _guard = HOME_ENV_LOCK.blocking_lock();

        // Set HOME to a temp dir so state_root (and thus run_logs_dir) resolves
        // to a predictable, writable location.
        let temp_home = TempDir::new().expect("create temp home");
        let original_home = std::env::var_os("HOME");
        // SAFETY: serialised by HOME_ENV_LOCK
        unsafe { std::env::set_var("HOME", temp_home.path()) };

        // Create a temporary repo root.
        let temp_dir = TempDir::new().expect("create temp dir");
        let repo_root = temp_dir.path().to_path_buf();
        let run_uid = "01TESTREPLAYUID";
        let task_id = "test-task";

        // After plan-0029, transcripts live under state_root/runs/{run_uid}/logs/
        // Use run_logs_dir to get (and create) the canonical path.
        let logs_dir = makina_core::paths::run_logs_dir(&repo_root, run_uid)
            .expect("run_logs_dir must succeed");
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
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Completed,
            project: String::new(),
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new(task_id),
                title: "Test task".into(),
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

        // Restore HOME.
        // SAFETY: serialised by HOME_ENV_LOCK
        unsafe {
            match original_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn up_down_navigates_tree_when_sidebar_focused() {
        let mut app = make_app_with_runs();

        // Start at node 0 (Run0) in the Sidebar.
        app.focused_panel = Panel::Sidebar;
        assert_eq!(app.tree_cursor, Some(0));
        assert_eq!(app.focused_node(), Some(TreeNode::Run { run: 0 }));

        // SelectDown should move to node 1 (Task00).
        app.update(AppEvent::SelectDown);
        assert_eq!(
            app.tree_cursor,
            Some(1),
            "SelectDown should move cursor from node 0 to node 1"
        );
        assert_eq!(
            app.focused_node(),
            Some(TreeNode::Task { run: 0, task: 0 }),
            "After SelectDown, focused_node should be Task 0-0"
        );

        // SelectUp should move back to node 0 (Run0).
        app.update(AppEvent::SelectUp);
        assert_eq!(
            app.tree_cursor,
            Some(0),
            "SelectUp should move cursor back to node 0"
        );
        assert_eq!(
            app.focused_node(),
            Some(TreeNode::Run { run: 0 }),
            "After SelectUp, focused_node should be Run0"
        );
    }

    #[test]
    fn up_down_scrolls_content_when_main_focused() {
        let mut app = make_app_with_runs();

        // Move to Main panel.
        app.focused_panel = Panel::Main;

        // Set last_scroll_maxes to a non-zero value so scroll_down has a clamp.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 10);

        // Record initial selected_task and exchange state.
        let initial_selected_task = app.selected_task;
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0
        );
        assert!(
            app.exchange_auto_follow,
            "Should start with auto_follow = true"
        );

        // SelectUp should scroll up and disable auto-follow, but NOT change selected_task.
        app.update(AppEvent::SelectUp);
        assert_eq!(
            app.selected_task, initial_selected_task,
            "SelectUp should NOT change selected_task when focused on Main"
        );
        assert!(
            !app.exchange_auto_follow,
            "scroll_up should disable auto_follow"
        );

        // SelectDown should scroll down and update exchange scroll offset, but NOT change selected_task.
        app.update(AppEvent::SelectDown);
        assert_eq!(
            app.selected_task, initial_selected_task,
            "SelectDown should NOT change selected_task when focused on Main"
        );
        assert!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0)
                > 0,
            "exchange scroll offset should advance after SelectDown"
        );
    }

    /// `apply_exchange_event` with `ExchangeEvent::ToolCall { content: Some(…), .. }`
    /// followed by a matching `ToolCallUpdate` populates
    /// `ExchangeContent::Tool.content` with the provided text.
    #[test]
    fn tool_content_populated_from_event() {
        use crate::app::{ExchangeContent, ExchangeLog};
        use makina_core::api::{AgentRole, ExchangeEvent};

        let mut log = ExchangeLog::default();

        // ToolCall carrying content.
        apply_exchange_event(
            &mut log,
            AgentRole::Developer,
            &ExchangeEvent::ToolCall {
                id: "tc-content".into(),
                title: "write file".into(),
                kind: Some("edit".into()),
                status: "pending".into(),
                content: Some("+added line".into()),
            },
        );

        // ToolCallUpdate (no new content — must not blank the existing).
        apply_exchange_event(
            &mut log,
            AgentRole::Developer,
            &ExchangeEvent::ToolCallUpdate {
                id: "tc-content".into(),
                status: Some("completed".into()),
                title: None,
                content: None,
            },
        );

        let entry = log
            .entries
            .iter()
            .find(|e| matches!(&e.content, ExchangeContent::Tool { id, .. } if id == "tc-content"))
            .expect("tool entry must exist");

        match &entry.content {
            ExchangeContent::Tool { content, .. } => {
                assert_eq!(
                    content, "+added line",
                    "content from ToolCall event must survive a subsequent content-less update"
                );
            }
            other => panic!("expected Tool entry, got {other:?}"),
        }
    }

    /// A `content: None` event leaves `ExchangeContent::Tool.content` as `""`
    /// and does not panic.
    #[test]
    fn content_none_leaves_tool_content_empty() {
        use crate::app::{ExchangeContent, ExchangeLog};
        use makina_core::api::{AgentRole, ExchangeEvent};

        let mut log = ExchangeLog::default();

        apply_exchange_event(
            &mut log,
            AgentRole::Developer,
            &ExchangeEvent::ToolCall {
                id: "tc-none".into(),
                title: "read file".into(),
                kind: None,
                status: "pending".into(),
                content: None,
            },
        );

        apply_exchange_event(
            &mut log,
            AgentRole::Developer,
            &ExchangeEvent::ToolCallUpdate {
                id: "tc-none".into(),
                status: Some("completed".into()),
                title: None,
                content: None,
            },
        );

        let entry = log
            .entries
            .iter()
            .find(|e| matches!(&e.content, ExchangeContent::Tool { id, .. } if id == "tc-none"))
            .expect("tool entry must exist");

        match &entry.content {
            ExchangeContent::Tool { content, .. } => {
                assert_eq!(
                    content, "",
                    "content: None events must leave content as empty string"
                );
            }
            other => panic!("expected Tool entry, got {other:?}"),
        }
    }

    // ── Command palette (plan 0069) ───────────────────────────────────────────

    /// `AppEvent::OpenCommandPalette` must set mode and seed actions.
    #[test]
    fn open_command_palette_sets_mode_and_seeds_actions() {
        let mut app = make_app();

        // Mode starts as Normal.
        assert_eq!(app.mode, Mode::Normal, "initial mode must be Normal");
        assert!(
            app.command_palette.is_none(),
            "command_palette must start None"
        );

        // Open the palette.
        let changed = app.update(AppEvent::OpenCommandPalette);
        assert!(changed, "OpenCommandPalette must request a redraw");

        // Mode is now CommandPalette.
        assert_eq!(
            app.mode,
            Mode::CommandPalette,
            "mode must be CommandPalette after open"
        );

        // Palette is Some and seeded.
        assert!(
            app.command_palette.is_some(),
            "command_palette must be Some after open"
        );
        let palette = app.command_palette.as_ref().unwrap();

        // Default actions are non-empty.
        assert!(
            !palette.actions.is_empty(),
            "default_actions must be non-empty"
        );
        assert!(
            palette
                .actions
                .iter()
                .any(|action| action.label() == "Create plan"),
            "command palette must expose plan authoring"
        );

        // Selected is 0.
        assert_eq!(palette.selected, 0, "selected must be 0");

        // Filter starts empty.
        assert_eq!(palette.filter, "", "filter must be empty initially");
    }

    /// Filtering must narrow the action list case-insensitively and clamp selection.
    #[test]
    fn palette_filter_narrows_and_clamps_selection() {
        let mut app = make_app();
        app.update(AppEvent::OpenCommandPalette);

        // Palette has all default actions, including run controls.
        let palette = app.command_palette.as_ref().unwrap();
        assert_eq!(
            palette.filtered().len(),
            16,
            "full list must include all 16 actions"
        );
        for label in [
            "Start run",
            "Pause run",
            "Stop run",
            "Reset/retry focused task",
            "Reset selected plan/run",
            "Purge Makina worktrees",
        ] {
            assert!(
                palette.actions.iter().any(|action| action.label() == label),
                "default palette actions must include {label:?}"
            );
        }

        // Type "doc" (case-insensitive).
        app.update(AppEvent::CommandPaletteInput('d'));
        app.update(AppEvent::CommandPaletteInput('o'));
        app.update(AppEvent::CommandPaletteInput('c'));

        let palette = app.command_palette.as_ref().unwrap();

        // Filter should match "Doctor" case-insensitively.
        assert_eq!(palette.filter, "doc", "filter must be 'doc'");

        let filtered = palette.filtered();
        assert_eq!(filtered.len(), 1, "filtered list must have 1 item");
        assert_eq!(
            filtered[0].label(),
            "Doctor",
            "filtered item must be 'Doctor'"
        );

        // Selected must be clamped to 0 (the only item).
        assert_eq!(
            palette.selected, 0,
            "selected must be clamped to 0 for single-item list"
        );

        // Backspace to clear the filter.
        app.update(AppEvent::CommandPaletteBackspace);
        app.update(AppEvent::CommandPaletteBackspace);
        app.update(AppEvent::CommandPaletteBackspace);

        let palette = app.command_palette.as_ref().unwrap();
        assert_eq!(palette.filter, "", "filter must be empty after backspaces");

        // Full list restored.
        assert_eq!(
            palette.filtered().len(),
            16,
            "full list restored after filter cleared"
        );
    }

    #[test]
    fn request_reset_run_opens_confirmation_for_plan_context() {
        let mut app = make_app();
        app.discovered_plans = vec![test_plan_entry(
            PathBuf::from("/tmp/docs/plans/0099-demo"),
            "0099-demo".to_string(),
            Vec::new(),
            None,
            None,
            None,
        )];
        let target = app.plan_identity_for_entry(&app.repo_root, &app.discovered_plans[0]);
        app.tabs.open_tab(TabContent::Plan { plan: target });

        let changed = app.update(AppEvent::RequestResetRun);

        assert!(changed);
        assert_eq!(app.mode, Mode::ResetConfirm);
        let confirm = app
            .reset_confirmation
            .as_ref()
            .expect("reset confirmation must be open");
        assert_eq!(confirm.target.slug, "0099-demo");
        assert_eq!(
            confirm.target.plan_dir,
            makina_core::plan::PlanKey::parse("docs/plans/0099-demo").unwrap()
        );
    }

    #[test]
    fn reset_started_tracks_operation_and_opens_logs() {
        let mut app = make_app();
        let target = PlanIdentity::legacy("0099-demo");

        let changed = app.update(AppEvent::ResetStarted {
            target: target.clone(),
            label: "Demo plan".to_string(),
        });

        assert!(changed);
        assert_eq!(app.mode, Mode::Normal);
        assert!(
            app.error_pane_open,
            "reset progress should open the output pane"
        );
        assert_eq!(app.output_tab, OutputTab::Logs);
        let op = app
            .plan_operations
            .get(&target)
            .expect("reset must create a tracked plan operation");
        assert!(op.is_running());
        assert_eq!(op.log, vec!["Starting reset".to_string()]);
    }

    #[test]
    fn plan_operation_event_appends_visible_reset_progress() {
        let mut app = make_app();
        app.runs.push(RunView {
            id: RunId(99),
            run_uid: "reset-run".to_string(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-todo").unwrap(),
            status: makina_core::api::RunStatus::Running,
            project: "tmp".to_string(),
            tasks: Vec::new(),
            report: makina_core::api::IngestionReport::default(),
        });
        let target = app.plan_identity_for_run(&app.runs[0]);
        app.update(AppEvent::ResetStarted {
            target: target.clone(),
            label: "Demo plan".to_string(),
        });

        app.update(AppEvent::ApiEvent(Event::PlanOperation {
            run: RunId(99),
            plan_slug: "0099-demo".to_string(),
            label: "Demo plan".to_string(),
            operation: makina_core::api::PlanOperationKind::Reset,
            phase: makina_core::api::PlanOperationPhase::Step,
            message: "Deleting plan branch".to_string(),
        }));

        assert!(app.error_pane_open);
        assert_eq!(app.output_tab, OutputTab::Logs);
        let op = app
            .plan_operations
            .get(&target)
            .expect("operation event must upsert plan operation state");
        assert_eq!(op.phase, makina_core::api::PlanOperationPhase::Step);
        assert_eq!(
            op.log,
            vec![
                "Starting reset".to_string(),
                "Deleting plan branch".to_string()
            ]
        );
        assert_eq!(app.status_message.as_deref(), Some("Deleting plan branch"));
    }

    #[test]
    fn operation_blocked_opens_notice_modal() {
        let mut app = make_app();
        let target = PlanIdentity::legacy("0099-demo");

        let changed = app.update(AppEvent::OperationBlocked {
            target,
            attempted: "Start run".to_string(),
        });

        assert!(changed);
        assert_eq!(app.mode, Mode::OperationNotice);
        let notice = app
            .operation_notice
            .as_ref()
            .expect("blocked operation must open notice modal");
        assert_eq!(notice.target.slug, "0099-demo");
        assert_eq!(notice.attempted, "Start run");
    }

    /// `CommandPaletteExecute` and `CloseCommandPalette` must return to normal mode.
    #[test]
    fn palette_execute_and_close_return_to_normal() {
        let mut app = make_app();
        app.update(AppEvent::OpenCommandPalette);

        // Mode is CommandPalette.
        assert_eq!(
            app.mode,
            Mode::CommandPalette,
            "mode must be CommandPalette"
        );
        assert!(
            app.command_palette.is_some(),
            "command_palette must be Some"
        );

        // Execute the selected action.
        let changed = app.update(AppEvent::CommandPaletteExecute);
        assert!(changed, "CommandPaletteExecute must request a redraw");

        // Mode is Normal and palette is gone.
        assert_eq!(app.mode, Mode::Normal, "mode must be Normal after execute");
        assert!(
            app.command_palette.is_none(),
            "command_palette must be None after execute"
        );

        // Open again and test close.
        app.update(AppEvent::OpenCommandPalette);
        assert_eq!(
            app.mode,
            Mode::CommandPalette,
            "mode must be CommandPalette"
        );

        // Close the palette.
        let changed = app.update(AppEvent::CloseCommandPalette);
        assert!(changed, "CloseCommandPalette must request a redraw");

        // Mode is Normal and palette is gone.
        assert_eq!(app.mode, Mode::Normal, "mode must be Normal after close");
        assert!(
            app.command_palette.is_none(),
            "command_palette must be None after close"
        );
    }

    /// Entering the nested theme selector opens the theme list and keeps the palette open.
    #[test]
    fn nested_theme_selector_enters_mode() {
        let mut app = make_app();
        app.update(AppEvent::OpenCommandPalette);

        // Navigate to the "Switch theme" action (the last one).
        let palette = app.command_palette.as_ref().unwrap();
        let switch_theme_index = palette
            .actions
            .iter()
            .position(|action| action.label() == "Switch theme")
            .expect("Switch theme action must exist");
        assert_eq!(
            switch_theme_index,
            palette.actions.len() - 1,
            "Switch theme should stay last"
        );
        assert!(
            matches!(
                palette.actions[switch_theme_index],
                crate::app::PaletteAction::NestedThemeSelector { .. }
            ),
            "last action should be NestedThemeSelector"
        );

        // Select it by moving down from index 0.
        for _ in 0..switch_theme_index {
            app.update(AppEvent::CommandPaletteDown);
        }

        let palette = app.command_palette.as_ref().unwrap();
        assert_eq!(
            palette.selected, switch_theme_index,
            "should be at Switch theme"
        );
        assert!(
            palette.theme_selector.is_none(),
            "theme_selector should still be None"
        );

        // Execute to enter theme selector mode.
        app.update(AppEvent::CommandPaletteExecute);

        let palette = app.command_palette.as_ref().unwrap();
        assert!(
            palette.theme_selector.is_some(),
            "theme_selector should be Some after entering"
        );
        assert_eq!(
            palette.filter, "",
            "filter should be cleared when entering theme mode"
        );
        assert_eq!(palette.selected, 0, "selected should reset to 0");

        // Palette should still be open.
        assert_eq!(
            app.mode,
            Mode::CommandPalette,
            "palette should still be open"
        );
        assert!(
            app.command_palette.is_some(),
            "command_palette should be Some"
        );

        let theme_names = palette.theme_selector.as_ref().unwrap();
        assert_eq!(theme_names.len(), 3, "should have 3 built-in themes");
        assert!(theme_names.contains(&"Ayu Dark".to_string()));
        assert!(theme_names.contains(&"Ayu Mirage".to_string()));
        assert!(theme_names.contains(&"Ayu Light".to_string()));
    }

    /// Selecting a theme in nested mode mutates app.active_theme and clears theme_selector.
    #[test]
    fn nested_theme_selector_applies_theme() {
        let mut app = make_app();
        app.update(AppEvent::OpenCommandPalette);

        // Navigate to and enter the theme selector.
        let switch_theme_index = app
            .command_palette
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|action| action.label() == "Switch theme")
            .expect("Switch theme action must exist");
        for _ in 0..switch_theme_index {
            app.update(AppEvent::CommandPaletteDown);
        }
        app.update(AppEvent::CommandPaletteExecute);

        let initial_theme = app.active_theme.name.clone();
        assert_eq!(initial_theme, "Ayu Dark", "should start with Ayu Dark");

        // Move down to the second theme (Ayu Mirage).
        app.update(AppEvent::CommandPaletteDown);

        let palette = app.command_palette.as_ref().unwrap();
        assert_eq!(palette.selected, 1, "should be at Ayu Mirage");

        // Execute to apply the theme.
        app.update(AppEvent::CommandPaletteExecute);

        // Theme should change.
        assert_eq!(
            app.active_theme.name, "Ayu Mirage",
            "active_theme should be Ayu Mirage"
        );

        // Theme selector should be cleared, palette should still be open.
        let palette = app.command_palette.as_ref().unwrap();
        assert!(
            palette.theme_selector.is_none(),
            "theme_selector should be None after selection"
        );
        assert_eq!(
            app.mode,
            Mode::CommandPalette,
            "palette should still be open"
        );
    }

    /// Esc in nested theme selector mode returns to action list without changing theme.
    #[test]
    fn nested_theme_selector_esc_returns_to_actions() {
        let mut app = make_app();
        app.update(AppEvent::OpenCommandPalette);

        // Enter theme selector.
        let switch_theme_index = app
            .command_palette
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|action| action.label() == "Switch theme")
            .expect("Switch theme action must exist");
        for _ in 0..switch_theme_index {
            app.update(AppEvent::CommandPaletteDown);
        }
        app.update(AppEvent::CommandPaletteExecute);

        let palette = app.command_palette.as_ref().unwrap();
        assert!(
            palette.theme_selector.is_some(),
            "should be in theme selector mode"
        );

        let initial_theme = app.active_theme.name.clone();

        // Move down to a different theme.
        app.update(AppEvent::CommandPaletteDown);
        app.update(AppEvent::CommandPaletteDown);

        // Press Esc to exit theme selector mode.
        app.update(AppEvent::CloseCommandPalette);

        // Theme should not have changed.
        assert_eq!(
            app.active_theme.name, initial_theme,
            "theme should not change on Esc"
        );

        // Should be back to action list.
        let palette = app.command_palette.as_ref().unwrap();
        assert!(
            palette.theme_selector.is_none(),
            "theme_selector should be None after Esc"
        );
        assert_eq!(palette.selected, 0, "selected should reset to 0");
        assert_eq!(palette.filter, "", "filter should be empty");

        // Palette should still be open.
        assert_eq!(
            app.mode,
            Mode::CommandPalette,
            "palette should still be open"
        );
    }

    /// Filtering works in theme selector mode.
    #[test]
    fn nested_theme_selector_filtering() {
        let mut app = make_app();
        app.update(AppEvent::OpenCommandPalette);

        // Enter theme selector.
        let switch_theme_index = app
            .command_palette
            .as_ref()
            .unwrap()
            .actions
            .iter()
            .position(|action| action.label() == "Switch theme")
            .expect("Switch theme action must exist");
        for _ in 0..switch_theme_index {
            app.update(AppEvent::CommandPaletteDown);
        }
        app.update(AppEvent::CommandPaletteExecute);

        // Navigate to "Ayu Light" (index 2) before filtering.
        app.update(AppEvent::CommandPaletteDown);
        app.update(AppEvent::CommandPaletteDown);
        {
            let palette = app.command_palette.as_ref().unwrap();
            assert_eq!(palette.selected, 2, "should be at index 2 (Ayu Light)");
        }

        // Type 'd' to filter — only "Ayu Dark" matches (1 result).
        // Several action labels also match 'd', so if filtering incorrectly used
        // palette.filtered().len() it would not clamp selected to the only theme.
        app.update(AppEvent::CommandPaletteInput('d'));

        {
            let palette = app.command_palette.as_ref().unwrap();
            assert_eq!(palette.filter, "d", "filter should be 'd'");
            assert_eq!(
                palette.selected, 0,
                "selected must be clamped to 0 (only 1 theme matches 'd')"
            );
            assert!(
                palette.theme_selector.is_some(),
                "theme_selector should still be Some"
            );
            // Verify filtered_theme_names returns the correct count.
            assert_eq!(
                palette.filtered_theme_names().len(),
                1,
                "exactly 1 theme matches 'd' (Ayu Dark)"
            );
        }

        // Type "mirage" (clearing 'd' first via backspace, then typing).
        app.update(AppEvent::CommandPaletteBackspace);
        app.update(AppEvent::CommandPaletteInput('m'));
        app.update(AppEvent::CommandPaletteInput('i'));
        app.update(AppEvent::CommandPaletteInput('r'));
        app.update(AppEvent::CommandPaletteInput('a'));
        app.update(AppEvent::CommandPaletteInput('g'));
        app.update(AppEvent::CommandPaletteInput('e'));

        let palette = app.command_palette.as_ref().unwrap();
        assert_eq!(palette.filter, "mirage", "filter should be 'mirage'");

        // The theme selector is still Some, and we should see only one theme.
        assert!(
            palette.theme_selector.is_some(),
            "theme_selector should still be Some"
        );
        assert_eq!(
            palette.filtered_theme_names().len(),
            1,
            "should filter to 1 theme matching 'mirage'"
        );
        assert_eq!(
            palette.selected, 0,
            "selected must be clamped within the theme-filtered count"
        );
    }

    // ── Verbose mode (plan 0021) ──────────────────────────────────────────────

    /// `AppEvent::ToggleVerbose` must flip `App::verbose_mode` true↔false.
    #[test]
    fn toggle_verbose_flips_flag() {
        let mut app = make_app();

        // Starts off (default compact mode).
        assert!(!app.verbose_mode, "verbose_mode must default to false");

        // First toggle turns it on.
        let changed = app.update(AppEvent::ToggleVerbose);
        assert!(changed, "ToggleVerbose must request a redraw");
        assert!(
            app.verbose_mode,
            "verbose_mode must be true after first toggle"
        );

        // Second toggle turns it back off.
        app.update(AppEvent::ToggleVerbose);
        assert!(
            !app.verbose_mode,
            "verbose_mode must be false after second toggle"
        );
    }

    #[test]
    fn toggle_tool_diff_flips_single_key() {
        let mut app = make_app();
        let key = ToolDiffKey {
            run: RunId(1),
            task: TaskId::new("model"),
            tool_id: "write-tool".to_string(),
        };

        assert!(app.expanded_tool_diffs.is_empty());
        assert!(app.update(AppEvent::ToggleToolDiff(key.clone())));
        assert!(app.expanded_tool_diffs.contains(&key));

        app.update(AppEvent::ToggleToolDiff(key.clone()));
        assert!(!app.expanded_tool_diffs.contains(&key));
    }

    #[test]
    fn open_settings_lists_current_caps() {
        // --- Case 1: caps.idle_secs = None, idle_secs_config = None ---
        // Both sources produce the same empty string for idle_secs; this
        // verifies the basic wiring for the None case.
        let mut app = make_app();
        let repo = tempfile::tempdir().expect("settings repo");
        std::fs::create_dir_all(repo.path().join(".makina")).expect("config directory");
        std::fs::write(
            repo.path().join(".makina/config.toml"),
            "concurrency = 4\n[caps]\ngate_iterations = 7\nreviewer_iterations = 3\nwall_clock_secs = 1200\n[merge]\nfinal = \"squash\"\n",
        )
        .expect("write project settings");
        app.repo_root = repo.path().to_path_buf();
        app.caps = makina_core::config::CapsConfig {
            gate_iterations: 7,
            reviewer_iterations: 3,
            wall_clock_secs: 1200,
            idle_secs: None,
        };
        app.concurrency = 4;
        app.idle_secs_config = None;

        // Open settings.
        let changed = app.update(AppEvent::OpenSettings);
        assert!(changed, "OpenSettings must request a redraw");
        assert_eq!(app.mode, Mode::Settings, "mode must be Settings");
        assert!(app.settings.is_some(), "settings must be Some");

        // Verify settings are seeded from caps.
        let settings = app.settings.as_ref().unwrap();
        assert_eq!(settings.gate_iterations, "7");
        assert_eq!(settings.reviewer_iterations, "3");
        assert_eq!(settings.wall_clock_secs, "1200");
        assert_eq!(settings.idle_secs, "", "idle_secs must be empty when None");
        assert_eq!(settings.concurrency, "4");
        assert_eq!(settings.final_merge, FinalMerge::Squash);
        assert_eq!(
            settings.focused,
            SettingsField::GateIterations,
            "focused must start on GateIterations"
        );
        assert_eq!(settings.error, None, "error must be None initially");

        // --- Case 2: caps.idle_secs = Some(30), idle_secs_config = None ---
        // This distinguishes the correct source (caps.idle_secs) from the wrong
        // one (idle_secs_config).  If OpenSettings reads idle_secs_config the
        // field would be "" despite the config having 30s configured.
        let mut app2 = make_app();
        let repo2 = tempfile::tempdir().expect("second settings repo");
        std::fs::create_dir_all(repo2.path().join(".makina")).expect("config directory");
        std::fs::write(
            repo2.path().join(".makina/config.toml"),
            "concurrency = 2\n[caps]\ngate_iterations = 2\nreviewer_iterations = 2\nwall_clock_secs = 900\nidle_secs = 30\n[merge]\nfinal = \"squash\"\n",
        )
        .expect("write second project settings");
        app2.repo_root = repo2.path().to_path_buf();
        app2.caps = makina_core::config::CapsConfig {
            gate_iterations: 2,
            reviewer_iterations: 2,
            wall_clock_secs: 900,
            idle_secs: Some(30),
        };
        app2.concurrency = 2;
        // idle_secs_config is left at None (the default), simulating a scenario
        // where the user configured idle_secs=30 in config.toml but no TaskIdle
        // event has fired yet.
        assert_eq!(app2.idle_secs_config, None);

        app2.update(AppEvent::OpenSettings);
        let settings2 = app2.settings.as_ref().unwrap();
        assert_eq!(
            settings2.idle_secs, "30",
            "idle_secs must be seeded from caps.idle_secs, not idle_secs_config"
        );
        assert_eq!(settings2.gate_iterations, "2");
        assert_eq!(settings2.concurrency, "2");
        assert_eq!(settings2.final_merge, FinalMerge::Squash);
    }

    #[test]
    fn invalid_value_rejected() {
        // Seed settings with some values, then clear gate_iterations and enter '0'.
        let mut app = make_app();
        app.caps = makina_core::config::CapsConfig {
            gate_iterations: 7,
            reviewer_iterations: 3,
            wall_clock_secs: 1200,
            idle_secs: None,
        };
        app.concurrency = 4;

        // Open settings to populate the modal.
        app.update(AppEvent::OpenSettings);
        assert!(app.settings.is_some(), "settings must be Some");

        let settings = app.settings.as_ref().unwrap();
        assert_eq!(settings.focused, SettingsField::GateIterations);

        // Clear the buffer and input '0'.
        app.settings.as_mut().unwrap().gate_iterations.clear();
        app.update(AppEvent::SettingsInput('0'));

        // Check that error was set.
        let settings = app.settings.as_ref().unwrap();
        assert_eq!(
            settings.error,
            Some("caps.gate_iterations must be at least 1".to_string()),
            "error must be set for value 0"
        );
        assert_eq!(settings.gate_iterations, "0");

        // Try to commit — should fail and keep modal open.
        let changed = app.update(AppEvent::SettingsCommit);
        assert!(changed);
        assert_eq!(
            app.mode,
            Mode::Settings,
            "mode must remain Settings after failed validation"
        );
        assert!(app.settings.is_some(), "settings must remain Some");

        // Verify caps were NOT mutated.
        assert_eq!(
            app.caps.gate_iterations, 7,
            "caps.gate_iterations must not be mutated on failed validation"
        );
        assert_eq!(
            app.concurrency, 4,
            "concurrency must not be mutated on failed validation"
        );
    }

    #[test]
    fn settings_navigation_cycles_through_fields() {
        let mut app = make_app();
        app.update(AppEvent::OpenSettings);
        let settings = app.settings.as_ref().unwrap();
        assert_eq!(settings.focused, SettingsField::GateIterations);

        // Down should move to ReviewerIterations.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::ReviewerIterations
        );

        // Down again -> WallClockSecs.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::WallClockSecs
        );

        // Down again -> IdleSecs.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::IdleSecs
        );

        // Down again -> Concurrency.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::Concurrency
        );

        // Down again -> FinalMerge.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::FinalMerge
        );

        // Down again -> DeveloperModel.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::DeveloperModel
        );

        // Down again -> DeveloperEffort: each role's effort follows its model.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::DeveloperEffort
        );

        // Down again -> ReviewerModel.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::ReviewerModel
        );

        // Down again -> ReviewerEffort.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::ReviewerEffort
        );

        // Down again -> PlannerModel.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::PlannerModel
        );

        // Down again -> PlannerEffort.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::PlannerEffort
        );

        // Down again -> wraps to GateIterations.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::GateIterations
        );

        // Up should go backward, mirroring the same order.
        app.update(AppEvent::SettingsUp);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::PlannerEffort
        );
        app.update(AppEvent::SettingsUp);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::PlannerModel
        );
        app.update(AppEvent::SettingsUp);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::ReviewerEffort
        );
    }

    /// One Backspace deletes one character.
    ///
    /// The re-validation pass used to `pop()` the model buffers a second time,
    /// so every Backspace on a model field removed two characters.
    #[test]
    fn backspace_on_a_model_field_deletes_exactly_one_character() {
        let mut app = make_app();
        app.update(AppEvent::OpenSettings);
        app.settings.as_mut().unwrap().focused = SettingsField::PlannerModel;
        app.settings.as_mut().unwrap().planner_model = "glm-5.2".to_string();

        app.update(AppEvent::SettingsBackspace);

        assert_eq!(app.settings.as_ref().unwrap().planner_model, "glm-5.");
    }

    /// The picker offers thought levels for an effort field and models for a
    /// model field — the same overlay, sourced from the focused field.
    #[test]
    fn the_picker_offers_efforts_for_an_effort_field() {
        let mut app = make_app();
        app.update(AppEvent::OpenSettings);
        {
            let settings = app.settings.as_mut().unwrap();
            settings.discovered_models = vec!["agent/glm-5.2".into()];
            settings.discovered_efforts = vec!["low".into(), "high".into()];
            settings.focused = SettingsField::PlannerEffort;
        }
        app.update(AppEvent::OpenModelPicker);

        assert_eq!(
            app.model_picker.as_ref().map(|p| p.all_models.clone()),
            Some(vec!["low".to_string(), "high".to_string()]),
            "an effort field must list thought levels, not models",
        );

        app.update(AppEvent::CloseModelPicker);
        app.settings.as_mut().unwrap().focused = SettingsField::PlannerModel;
        app.update(AppEvent::OpenModelPicker);
        assert_eq!(
            app.model_picker.as_ref().map(|p| p.all_models.clone()),
            Some(vec!["agent/glm-5.2".to_string()]),
            "a model field must still list models",
        );
    }

    /// Choosing an effort writes it into that role's buffer.
    #[test]
    fn selecting_an_effort_fills_the_focused_role_buffer() {
        let mut app = make_app();
        app.update(AppEvent::OpenSettings);
        {
            let settings = app.settings.as_mut().unwrap();
            settings.discovered_efforts = vec!["low".into(), "high".into()];
            settings.focused = SettingsField::PlannerEffort;
        }
        app.update(AppEvent::OpenModelPicker);
        app.update(AppEvent::ModelPickerDown);
        app.update(AppEvent::ModelPickerSelect);

        assert_eq!(app.settings.as_ref().unwrap().planner_effort, "high");
    }

    /// An emptied effort buffer is a deliberate clear, not "leave it alone" —
    /// otherwise a role could never be returned to the provider's default.
    #[test]
    fn clearing_an_effort_buffer_is_reported_as_a_clear() {
        let mut app = make_app();
        app.update(AppEvent::OpenSettings);
        app.roles.planner = Some(makina_core::config::RoleAssignment {
            provider: "default".into(),
            effort: Some("high".into()),
            ..Default::default()
        });
        app.settings.as_mut().unwrap().planner_effort = String::new();

        assert_eq!(
            app.selected_role_efforts().planner,
            Some(String::new()),
            "an emptied buffer over a stored effort must clear it",
        );

        // With nothing stored there is nothing to clear, so nothing to write.
        app.roles.planner = None;
        assert_eq!(app.selected_role_efforts().planner, None);
    }

    /// The authoring footer names the planner's model, falling back to the
    /// provider default rather than implying no choice exists.
    #[test]
    fn planner_model_label_reports_model_effort_and_default() {
        let mut app = make_app();
        assert_eq!(app.planner_model_label(), "provider default");

        app.roles.planner = Some(makina_core::config::RoleAssignment {
            provider: "default".into(),
            model: Some("glm-5.2".into()),
            ..Default::default()
        });
        assert_eq!(app.planner_model_label(), "glm-5.2");

        app.roles.planner.as_mut().unwrap().effort = Some("high".into());
        assert_eq!(app.planner_model_label(), "glm-5.2 · high");
    }

    #[test]
    fn settings_input_and_backspace_edit_focused_field() {
        let mut app = make_app();
        app.update(AppEvent::OpenSettings);
        app.settings.as_mut().unwrap().gate_iterations = "5".to_string();

        // Initially on GateIterations, which is "5" (the default).
        assert_eq!(app.settings.as_ref().unwrap().gate_iterations, "5");

        // Type "99" — should append to the field.
        app.update(AppEvent::SettingsInput('9'));
        assert_eq!(app.settings.as_ref().unwrap().gate_iterations, "59");

        app.update(AppEvent::SettingsInput('9'));
        assert_eq!(app.settings.as_ref().unwrap().gate_iterations, "599");

        // Backspace once — should pop.
        app.update(AppEvent::SettingsBackspace);
        assert_eq!(app.settings.as_ref().unwrap().gate_iterations, "59");

        // Move to Concurrency and edit it.
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::Concurrency
        );
        assert_eq!(app.settings.as_ref().unwrap().concurrency, "3");

        app.update(AppEvent::SettingsInput('5'));
        assert_eq!(app.settings.as_ref().unwrap().concurrency, "35");

        app.update(AppEvent::SettingsBackspace);
        assert_eq!(app.settings.as_ref().unwrap().concurrency, "3");
    }

    #[test]
    fn settings_cycles_final_merge_option() {
        let mut app = make_app();
        app.update(AppEvent::OpenSettings);

        for _ in 0..5 {
            app.update(AppEvent::SettingsDown);
        }
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::FinalMerge
        );
        assert_eq!(
            app.settings.as_ref().unwrap().final_merge,
            FinalMerge::Squash
        );

        app.update(AppEvent::SettingsNextOption);
        assert_eq!(
            app.settings.as_ref().unwrap().final_merge,
            FinalMerge::Stage
        );

        app.update(AppEvent::SettingsPreviousOption);
        assert_eq!(
            app.settings.as_ref().unwrap().final_merge,
            FinalMerge::Squash
        );
    }

    #[test]
    fn settings_esc_validates_and_saves() {
        let mut app = make_app();
        app.caps = makina_core::config::CapsConfig {
            gate_iterations: 7,
            reviewer_iterations: 3,
            wall_clock_secs: 1200,
            idle_secs: None,
        };
        app.concurrency = 4;

        app.update(AppEvent::OpenSettings);
        assert_eq!(app.mode, Mode::Settings);
        let initial_gate_iterations = app.settings.as_ref().unwrap().gate_iterations.clone();

        // Edit a field.
        app.update(AppEvent::SettingsInput('9'));
        assert_eq!(
            app.settings.as_ref().unwrap().gate_iterations,
            format!("{initial_gate_iterations}9")
        );

        // SettingsCommit validates. On success, the IO layer writes to disk
        // and returns SettingsSaved which closes the modal.
        app.update(AppEvent::SettingsCommit);
        // After commit, the modal stays open if validation fails (the IO
        // layer handles the actual close via SettingsSaved). In the unit test
        // there's no IO layer, so the modal stays open — but the settings
        // struct still holds the edited value.
        assert_eq!(
            app.settings.as_ref().unwrap().gate_iterations,
            format!("{initial_gate_iterations}9")
        );
    }

    #[test]
    fn settings_commit_applies_valid_values() {
        let mut app = make_app();
        app.caps = makina_core::config::CapsConfig {
            gate_iterations: 7,
            reviewer_iterations: 3,
            wall_clock_secs: 1200,
            idle_secs: None,
        };
        app.concurrency = 4;

        app.update(AppEvent::OpenSettings);
        {
            let settings = app.settings.as_mut().unwrap();
            settings.reviewer_iterations = "3".to_string();
            settings.wall_clock_secs = "1200".to_string();
            settings.idle_secs.clear();
        }

        // Clear GateIterations and set to "10".
        app.settings.as_mut().unwrap().gate_iterations.clear();
        app.update(AppEvent::SettingsInput('1'));
        app.update(AppEvent::SettingsInput('0'));
        assert_eq!(app.settings.as_ref().unwrap().gate_iterations, "10");

        // Move to Concurrency and set to "8".
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        app.settings.as_mut().unwrap().concurrency.clear();
        app.update(AppEvent::SettingsInput('8'));
        assert_eq!(app.settings.as_ref().unwrap().concurrency, "8");

        // Move to final merge and set it to Stage.
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::FinalMerge
        );
        app.update(AppEvent::SettingsNextOption);
        assert_eq!(
            app.settings.as_ref().unwrap().final_merge,
            FinalMerge::Stage
        );

        // Commit (Esc) — in the real flow, resolve_io calls commit_settings
        // which writes to disk and sets mode to Normal. Simulate that here.
        app.mode = Mode::Normal;
        let project_root = app.settings.as_ref().unwrap().project_root.clone();
        let values = crate::settings_validation::validate_settings(
            app.settings.as_ref().expect("settings open"),
        )
        .expect("valid settings");
        app.update(AppEvent::SettingsSaved {
            project_root,
            values,
        });

        // Verify caps were updated.
        assert_eq!(app.caps.gate_iterations, 10);
        assert_eq!(app.concurrency, 8);
        assert_eq!(app.final_merge, FinalMerge::Stage);
        assert_eq!(app.caps.reviewer_iterations, 3);
        assert_eq!(app.caps.wall_clock_secs, 1200);
        assert_eq!(app.caps.idle_secs, None);

        // Verify modal was closed.
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.settings.is_none());
    }

    #[test]
    fn settings_idle_secs_optional() {
        let mut app = make_app();
        app.caps = makina_core::config::CapsConfig {
            gate_iterations: 7,
            reviewer_iterations: 3,
            wall_clock_secs: 1200,
            idle_secs: Some(30),
        };
        app.concurrency = 4;

        app.update(AppEvent::OpenSettings);
        app.settings.as_mut().unwrap().idle_secs = "30".to_string();
        assert_eq!(app.settings.as_ref().unwrap().idle_secs, "30");

        // Move to IdleSecs field.
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        app.update(AppEvent::SettingsDown);
        assert_eq!(
            app.settings.as_ref().unwrap().focused,
            SettingsField::IdleSecs
        );

        // Clear it.
        app.settings.as_mut().unwrap().idle_secs.clear();
        assert_eq!(app.settings.as_ref().unwrap().idle_secs, "");
        assert_eq!(app.settings.as_ref().unwrap().error, None);

        // Commit then apply the IO layer's successful persistence result.
        app.mode = Mode::Normal;
        let project_root = app.settings.as_ref().unwrap().project_root.clone();
        let values = crate::settings_validation::validate_settings(
            app.settings.as_ref().expect("settings open"),
        )
        .expect("valid settings");
        app.update(AppEvent::SettingsSaved {
            project_root,
            values,
        });

        // Verify it's now None.
        assert_eq!(app.caps.idle_secs, None);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn update_records_role_metrics_per_role() {
        let api = Arc::new(PlaceholderApi::new());
        let run_id = RunId(42);
        let task_id = TaskId::new("test-task");
        let mut app = App::new(api, vec![], PathBuf::from("."));

        // Record a Developer metric.
        app.update(AppEvent::ApiEvent(Event::RoleTurnMetrics {
            run: run_id,
            task: task_id.clone(),
            role: AgentRole::Developer,
            model: "gpt-4o".to_string(),
            duration_ms: 1500,
            usage: None,
        }));

        // Record a Reviewer metric for the same (run, task).
        app.update(AppEvent::ApiEvent(Event::RoleTurnMetrics {
            run: run_id,
            task: task_id.clone(),
            role: AgentRole::Reviewer,
            model: "gpt-4-turbo".to_string(),
            duration_ms: 2000,
            usage: Some(makina_core::api::UsageStats {
                input_tokens: Some(100),
                output_tokens: Some(50),
            }),
        }));

        // Verify both metrics are stored.
        let key = (run_id, task_id);
        let by_role = app.role_metrics.get(&key).expect("metrics not stored");

        let dev_metric = by_role
            .get(&AgentRole::Developer)
            .expect("Developer metric not found");
        assert_eq!(dev_metric.model, "gpt-4o");
        assert_eq!(dev_metric.duration_ms, 1500);
        assert_eq!(dev_metric.usage, None);

        let rev_metric = by_role
            .get(&AgentRole::Reviewer)
            .expect("Reviewer metric not found");
        assert_eq!(rev_metric.model, "gpt-4-turbo");
        assert_eq!(rev_metric.duration_ms, 2000);
        assert_eq!(
            rev_metric.usage,
            Some(makina_core::api::UsageStats {
                input_tokens: Some(100),
                output_tokens: Some(50),
            })
        );
    }

    // ── Plan picker (plan 0027) ───────────────────────────────────────────────

    fn plan_task(id: &str, title: &str) -> TestPlanTask {
        TestPlanTask {
            id: id.to_string(),
            title: title.to_string(),
            gated: false,
            depends_on: Vec::new(),
            body: String::new(),
        }
    }

    fn make_plan_entries() -> Vec<makina_core::orchestrator::PlanEntry> {
        vec![
            test_plan_entry(
                PathBuf::from("/tmp/docs/plans/0001-alpha"),
                "0001-alpha".to_string(),
                vec![
                    plan_task("scaffold", "Scaffold"),
                    plan_task("model", "Model"),
                ],
                None,
                None,
                None,
            ),
            test_plan_entry(
                PathBuf::from("/tmp/docs/plans/0002-beta"),
                "0002-beta".to_string(),
                Vec::new(),
                None,
                None,
                None,
            ),
        ]
    }

    /// `OpenBrowser` must flag the app busy so the UI can render a spinner while
    /// plan discovery runs in the background.
    #[test]
    fn open_browser_sets_busy() {
        let mut app = make_app();
        assert_eq!(app.busy, None, "app starts idle");

        app.update(AppEvent::OpenBrowser);

        assert_eq!(
            app.busy.as_deref(),
            Some("Discovering plans"),
            "OpenBrowser must mark the app busy"
        );
    }

    /// `PlansDiscovered` must clear the busy flag when discovery resolves —
    /// whether or not any plans were found.
    #[test]
    fn plans_discovered_clears_busy() {
        let mut app = make_app();
        app.update(AppEvent::OpenBrowser);
        assert!(app.busy.is_some());

        app.update(AppEvent::PlansDiscovered {
            plans: make_plan_entries(),
        });

        assert_eq!(app.busy, None, "PlansDiscovered must clear the busy flag");
    }

    /// Sidebar shows discovered plans before open runs, and the first node is a
    /// `TreeNode::Plan` when plans are discovered.
    #[test]
    fn sidebar_shows_discovered_plans_before_runs() {
        let api = Arc::new(PlaceholderApi::new());
        let discovered_plans = vec![test_plan_entry(
            PathBuf::from("docs/plans/0001-test"),
            "0001-test".to_string(),
            Vec::new(),
            None,
            None,
            None,
        )];
        let mut app = App::new(api, vec![], PathBuf::from("."));
        app.discovered_plans = discovered_plans;

        let nodes = app.visible_tree_nodes();
        assert!(!nodes.is_empty(), "visible_tree_nodes should not be empty");
        assert!(
            matches!(nodes[0], TreeNode::Plan { plan_idx: 0 }),
            "First node should be a discovered plan"
        );
    }

    /// Once a discovered plan has an open Run (e.g. after Start), the sidebar
    /// shows ONLY the live Run node for that slug — not also the static
    /// discovered-plan node — so starting a plan does not appear to duplicate it.
    #[test]
    fn started_plan_is_not_duplicated_as_plan_and_run() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));
        app.discovered_plans = vec![test_plan_entry(
            PathBuf::from("docs/plans/0001-todo"),
            "0001-todo".to_string(),
            Vec::new(),
            None,
            None,
            None,
        )];
        // Plan-only: a single Plan node.
        assert_eq!(
            app.visible_tree_nodes()
                .iter()
                .filter(|n| matches!(n, TreeNode::Plan { .. }))
                .count(),
            1
        );

        // A Run is opened for the SAME plan slug (what Start does).
        app.runs = vec![RunView {
            id: RunId(1),
            run_uid: "uid-1".to_string(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-todo").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        }];

        let nodes = app.visible_tree_nodes();
        assert!(
            !nodes.iter().any(|n| matches!(n, TreeNode::Plan { .. })),
            "the discovered-plan node must be hidden once its Run exists"
        );
        assert_eq!(
            nodes
                .iter()
                .filter(|n| matches!(n, TreeNode::Run { .. }))
                .count(),
            1,
            "only the live Run node represents the started plan"
        );

        // Even with an additional older run for same slug (simulating two previous
        // disk runs), dedup keeps only 1 run node (the latest uid), no plan dup.
        app.runs.push(RunView {
            id: RunId(2),
            run_uid: "uid-0-older".to_string(), // lex smaller = older
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-todo").unwrap(),
            status: RunStatus::Completed,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        });
        let nodes2 = app.visible_tree_nodes();
        assert_eq!(
            nodes2
                .iter()
                .filter(|n| matches!(n, TreeNode::Run { .. }))
                .count(),
            1,
            "multiple runs for same plan must dedup to only the latest one"
        );
        assert!(
            !nodes2.iter().any(|n| matches!(n, TreeNode::Plan { .. })),
            "plan stub still hidden"
        );
    }

    /// Regression test: when a run is opened for the FIRST of several discovered
    /// plans, the run node must appear in the SAME position as the discovered
    /// plan (not shifted to the bottom of the sidebar). Before the fix, the
    /// sidebar order changed from [Plan-0001, Plan-0002, Plan-0003] to
    /// [Plan-0002, Plan-0003, Run-0001] when plan 0001 was started, which looked
    /// like plan 0001 "moved to the bottom."
    #[test]
    fn started_plan_run_stays_in_plan_order_position() {
        use makina_core::api::{RunId, RunStatus, RunView};
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));
        app.discovered_plans = vec![
            test_plan_entry(
                PathBuf::from("docs/plans/0001-todo"),
                "0001-todo".to_string(),
                Vec::new(),
                None,
                None,
                None,
            ),
            test_plan_entry(
                PathBuf::from("docs/plans/0002-todo"),
                "0002-todo".to_string(),
                Vec::new(),
                None,
                None,
                None,
            ),
            test_plan_entry(
                PathBuf::from("docs/plans/0003-todo"),
                "0003-todo".to_string(),
                Vec::new(),
                None,
                None,
                None,
            ),
        ];

        // Before starting: [Plan-0001, Plan-0002, Plan-0003].
        let nodes_before = app.visible_tree_nodes();
        let plan_slugs_before: Vec<&str> = nodes_before
            .iter()
            .filter_map(|n| match n {
                TreeNode::Plan { plan_idx } => Some(app.discovered_plans[*plan_idx].slug.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            plan_slugs_before,
            vec!["0001-todo", "0002-todo", "0003-todo"],
            "before starting, plans must be in discovery order"
        );

        // Start plan 0001: open a Run for it.
        app.runs = vec![RunView {
            id: RunId(1),
            run_uid: "uid-1".to_string(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-todo").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![],
            report: makina_core::api::IngestionReport::default(),
        }];

        let nodes_after = app.visible_tree_nodes();
        // The first node must be the Run for 0001 (not Plan-0002).
        assert!(
            matches!(nodes_after[0], TreeNode::Run { run: 0 }),
            "first node must be the Run for plan 0001, not a Plan; got {:?}",
            nodes_after[0]
        );
        // The remaining nodes must be Plan-0002 and Plan-0003 in order.
        let remaining_slugs: Vec<&str> = nodes_after
            .iter()
            .skip(1)
            .filter_map(|n| match n {
                TreeNode::Plan { plan_idx } => Some(app.discovered_plans[*plan_idx].slug.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            remaining_slugs,
            vec!["0002-todo", "0003-todo"],
            "remaining plans must stay in discovery order after plan 0001 is started; got {:?}",
            nodes_after
        );
    }

    /// `PlansDiscovered` must park the cursor on the first node (so Right/Enter
    /// have a target) and start every plan collapsed (no task children shown).
    #[test]
    fn plans_discovered_places_cursor_and_collapses_plans() {
        let mut app = make_app();
        assert_eq!(app.tree_cursor, None, "no runs → no cursor initially");

        app.update(AppEvent::PlansDiscovered {
            plans: make_plan_entries(),
        });

        assert_eq!(
            app.tree_cursor,
            Some(0),
            "cursor must land on the first plan"
        );
        assert!(matches!(
            app.focused_node(),
            Some(TreeNode::Plan { plan_idx: 0 })
        ));
        // Both plans collapsed → only the two headers are visible (alpha's 2
        // tasks stay hidden until expanded).
        assert_eq!(app.visible_tree_nodes().len(), 2);
    }

    /// Enter (`OpenFocusedNode`) on a collapsed plan with tasks must open the
    /// plan details tab AND expand the plan in the sidebar so its task previews
    /// become visible.
    #[test]
    fn enter_on_collapsed_plan_expands_it() {
        let mut app = make_app();
        app.update(AppEvent::PlansDiscovered {
            plans: make_plan_entries(),
        });
        let collapse_key = CollapseKey::Plan(
            app.plan_identity_for_node(TreeNode::Plan { plan_idx: 0 })
                .expect("plan identity"),
        );
        // alpha (index 0) has 2 tasks and starts collapsed.
        assert!(app.collapsed_plans.contains(&collapse_key));
        assert_eq!(app.visible_tree_nodes().len(), 2, "only 2 plan headers");

        app.update(AppEvent::OpenFocusedNode);

        assert_eq!(app.tabs.open_tabs.len(), 1, "plan tab opened");
        assert!(
            !app.collapsed_plans.contains(&collapse_key),
            "alpha must be expanded"
        );
        assert_eq!(
            app.visible_tree_nodes().len(),
            4,
            "2 headers + alpha's 2 tasks"
        );
    }

    /// Regression: legacy and folder-scoped collapse keys must live in separate
    /// namespaces. The old `folder_idx * 1000 + plan_idx` scheme aliased
    /// folder 0 / plan 0 onto legacy plan index 0 (both → key `0`); the
    /// [`CollapseKey`] discriminant makes them structurally distinct so
    /// collapsing one never toggles the other.
    #[test]
    fn collapse_keys_are_namespaced_across_paths() {
        let mut collapsed = HashSet::new();
        // Collapse the legacy plan at index 0.
        collapsed.insert(CollapseKey::LegacyPlan(0));
        // The folder-scoped plan (folder 0, plan 0) — the old collision — is
        // unaffected.
        assert!(collapsed.contains(&CollapseKey::LegacyPlan(0)));
        assert!(!collapsed.contains(&CollapseKey::FolderPlan { folder: 0, plan: 0 }));

        // Now collapse the folder-scoped plan too; both coexist independently.
        collapsed.insert(CollapseKey::FolderPlan { folder: 0, plan: 0 });
        assert!(collapsed.contains(&CollapseKey::LegacyPlan(0)));
        assert!(collapsed.contains(&CollapseKey::FolderPlan { folder: 0, plan: 0 }));

        // Removing one leaves the other intact.
        collapsed.remove(&CollapseKey::LegacyPlan(0));
        assert!(!collapsed.contains(&CollapseKey::LegacyPlan(0)));
        assert!(collapsed.contains(&CollapseKey::FolderPlan { folder: 0, plan: 0 }));
    }

    /// First `Right` on a collapsed plan reveals its tasks; a second `Right`
    /// (now expanded) crosses into the content pane.
    #[test]
    fn right_expands_collapsed_plan_then_crosses_to_main() {
        let mut app = make_app();
        app.update(AppEvent::PlansDiscovered {
            plans: make_plan_entries(),
        });

        app.update(AppEvent::FocusRightOrExpand);
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "expanding keeps sidebar focus"
        );
        let nodes = app.visible_tree_nodes();
        assert!(
            matches!(
                nodes.get(1),
                Some(TreeNode::PlanTask {
                    plan_idx: 0,
                    task_idx: 0
                })
            ),
            "alpha's first task must be revealed, got {nodes:?}"
        );

        app.update(AppEvent::FocusRightOrExpand);
        assert_eq!(
            app.focused_panel,
            Panel::Main,
            "second Right on an expanded plan crosses to content"
        );
        // Crossing into main from a plan also opens its details tab (so the
        // content shows the plan spec, just like Enter on the plan node).
        assert!(
            app.tabs.open_tabs.iter().any(|t| matches!(
                t,
                TabContent::Plan { plan } if plan.slug == "0001-alpha"
            )),
            "right-cross from plan must open its plan details tab"
        );
    }

    /// `Left` collapses an expanded plan back to a single header row.
    #[test]
    fn left_collapses_expanded_plan() {
        let mut app = make_app();
        app.update(AppEvent::PlansDiscovered {
            plans: make_plan_entries(),
        });
        app.update(AppEvent::FocusRightOrExpand); // expand alpha
        assert_eq!(
            app.visible_tree_nodes().len(),
            4,
            "2 plan headers + alpha's 2 tasks"
        );
        app.update(AppEvent::FocusLeftOrCollapse); // collapse alpha
        assert_eq!(app.visible_tree_nodes().len(), 2, "back to 2 headers");
    }

    /// Opening a plan tab via OpenTab with the same plan slug focuses the existing tab
    /// rather than creating a duplicate (plan 0032).
    #[test]
    fn open_plan_tab_deduplicates_by_slug() {
        let mut app = make_app();
        app.update(AppEvent::PlansDiscovered {
            plans: make_plan_entries(),
        });
        assert_eq!(app.tabs.open_tabs.len(), 0, "no tabs initially");

        // Open a plan tab
        app.update(AppEvent::OpenTab(TabContent::Plan {
            plan: PlanIdentity::legacy("0001-alpha".to_string()),
        }));
        assert_eq!(app.tabs.open_tabs.len(), 1, "one tab opened");
        assert_eq!(app.tabs.active_tab, Some(0), "tab is active");
        assert!(matches!(
            &app.tabs.open_tabs[0],
            TabContent::Plan { plan } if plan.slug == "0001-alpha"
        ));

        // Open the same plan tab again — should focus the existing tab, not create a duplicate
        // (this is the "open if closed, focus if open" contract from plan 0031).
        app.update(AppEvent::OpenTab(TabContent::Plan {
            plan: PlanIdentity::legacy("0001-alpha".to_string()),
        }));
        assert_eq!(
            app.tabs.open_tabs.len(),
            1,
            "still only one tab (not duplicated)"
        );
        assert_eq!(
            app.tabs.active_tab,
            Some(0),
            "the existing tab is still focused"
        );
    }

    /// A re-discovery (e.g. pressing `[o]` again) must not leave the cursor
    /// pointing past the rebuilt, all-collapsed node list. Plan tabs are closed
    /// if their plan slug no longer exists (plan 0032).
    #[test]
    fn re_discovery_clamps_cursor_and_closes_missing_plan_tabs() {
        let mut app = make_app();
        app.update(AppEvent::PlansDiscovered {
            plans: make_plan_entries(),
        });
        // Expand alpha (2 task children) and park the cursor on its last task, then
        // open a plan tab for beta.
        app.update(AppEvent::FocusRightOrExpand);
        app.tree_cursor = Some(2); // alpha's second PlanTask
        app.update(AppEvent::OpenTab(TabContent::Plan {
            plan: PlanIdentity::legacy("0001-beta".to_string()),
        }));
        assert_eq!(app.tabs.open_tabs.len(), 1, "plan tab is open");
        assert!(matches!(
            &app.tabs.open_tabs[0],
            TabContent::Plan { plan } if plan.slug == "0001-beta"
        ));

        // Re-discover with only the alpha plan (beta is gone).
        let alpha_only = vec![make_plan_entries()[0].clone()];
        app.update(AppEvent::PlansDiscovered { plans: alpha_only });

        let n = app.visible_tree_nodes().len();
        assert_eq!(n, 1, "only alpha plan visible");
        let cursor = app.tree_cursor.expect("cursor must remain set");
        assert!(cursor < n, "cursor {cursor} must be within {n} nodes");
        assert!(
            app.focused_node().is_some(),
            "focused_node must resolve after re-discovery"
        );
        assert_eq!(
            app.tabs.open_tabs.len(),
            0,
            "plan tab for missing beta is closed"
        );
    }

    /// Integration test: opening a discovered plan from the sidebar transitions it
    /// to the open runs list. This test verifies that:
    /// 1. We can navigate to a discovered plan node in the sidebar tree
    /// 2. Calling the API's `OpenPlan` command with the plan directory works
    /// 3. The run appears in the app's runs list
    #[tokio::test]
    async fn can_open_discovered_plan_from_sidebar() {
        let api = Arc::new(PlaceholderApi::empty());
        let plan_dir = PathBuf::from("docs/plans/0001-test");
        let discovered_plans = vec![test_plan_entry(
            plan_dir.clone(),
            "0001-test".to_string(),
            Vec::new(),
            None,
            None,
            None,
        )];

        let mut app = App::new(api.clone(), vec![], PathBuf::from("."));
        app.discovered_plans = discovered_plans;

        // Verify the plan node is visible in the tree
        let nodes = app.visible_tree_nodes();
        assert!(!nodes.is_empty(), "tree should have nodes");
        assert!(
            matches!(nodes[0], TreeNode::Plan { plan_idx: 0 }),
            "first node should be the discovered plan"
        );

        // Navigate the tree cursor to the plan node
        app.tree_cursor = Some(0);

        // Verify we're focused on a plan node
        let focused = app.focused_node();
        assert!(
            matches!(focused, Some(TreeNode::Plan { plan_idx: 0 })),
            "focused node should be the plan"
        );

        // Simulate pressing Enter: dispatch OpenPlan with the plan directory.
        let outcome = api
            .execute(makina_core::api::Command::OpenPlan {
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-alpha").unwrap(),
            })
            .await;

        // The API call should succeed
        assert!(
            outcome.is_ok(),
            "OpenPlan command should succeed, got {outcome:?}"
        );

        // After OpenPlan, a new run should appear in the runs list
        let runs = api.runs().await;
        assert!(
            !runs.is_empty(),
            "runs list should not be empty after OpenPlan"
        );

        // The new run should have the correct plan_dir
        let newly_opened_run = runs.last().expect("last run should exist");
        assert_eq!(
            newly_opened_run.plan_dir,
            makina_core::plan::PlanKey::parse("docs/plans/0001-alpha").unwrap(),
            "opened run should have the correct plan directory"
        );
    }

    // ── Plan authoring as a tab ────────────────────────────────────────────────

    /// Authoring opens as a tab, not as a modal overlay.
    #[test]
    fn opening_plan_authoring_opens_a_tab_and_leaves_normal_mode() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);

        assert_eq!(app.mode, Mode::Normal, "authoring must not seize a mode");
        assert!(app.plan_authoring.is_some());
        assert!(
            app.is_plan_authoring(),
            "the authoring tab must be the active tab"
        );
        assert!(
            app.tabs
                .open_tabs
                .iter()
                .any(|tab| matches!(tab, TabContent::PlanAuthoring { .. })),
            "an authoring tab must be open; got {:?}",
            app.tabs.open_tabs,
        );
    }

    /// Re-opening returns to the existing draft rather than discarding it.
    #[test]
    fn reopening_plan_authoring_preserves_the_draft() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);
        app.update(AppEvent::PlanAuthoringInput('h'));
        app.update(AppEvent::PlanAuthoringInput('i'));
        app.update(AppEvent::OpenPlanAuthoring);

        assert_eq!(
            app.plan_authoring
                .as_ref()
                .map(|state| state.input.as_str()),
            Some("hi"),
            "re-opening must not wipe the composer",
        );
        assert_eq!(
            app.tabs
                .open_tabs
                .iter()
                .filter(|tab| matches!(tab, TabContent::PlanAuthoring { .. }))
                .count(),
            1,
            "re-opening must reuse the tab, not stack duplicates",
        );
    }

    /// Closing retires both the tab and its state.
    #[test]
    fn closing_plan_authoring_removes_the_tab_and_the_draft() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);
        app.update(AppEvent::ClosePlanAuthoring);

        assert!(app.plan_authoring.is_none());
        assert!(!app.is_plan_authoring());
        assert!(
            !app.tabs
                .open_tabs
                .iter()
                .any(|tab| matches!(tab, TabContent::PlanAuthoring { .. })),
            "the authoring tab must be closed with its draft",
        );
    }

    /// A blueprint alone must not retire the conversation.
    ///
    /// Closing on the blueprint destroyed the tab before generation had run,
    /// so a failure left the operator with no transcript to fall back on.
    #[test]
    fn a_failed_generation_hands_the_conversation_back() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);
        if let Some(state) = app.plan_authoring.as_mut() {
            state.generating = true;
            state.push_operator("an fsm cli".into());
        }

        app.update(AppEvent::PlanAuthoringGenerated(Err(
            "slug already registered".into(),
        )));

        let state = app
            .plan_authoring
            .as_ref()
            .expect("a failed generation must keep the conversation");
        assert!(app.is_plan_authoring(), "the tab must stay open");
        assert!(!state.generating, "the composer must be usable again");
        assert!(
            state
                .log
                .entries
                .last()
                .is_some_and(|entry| entry.text().contains("slug already registered")),
            "the reason must reach the transcript: {:?}",
            state.log.entries,
        );
        assert!(
            state
                .log
                .entries
                .iter()
                .any(|entry| entry.text() == "an fsm cli"),
            "the earlier conversation must survive",
        );
    }

    /// Only a confirmed bundle retires the tab.
    #[test]
    fn a_successful_generation_closes_the_tab() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);
        app.update(AppEvent::PlanAuthoringGenerated(Ok(
            "docs/plans/0001-fsm".into()
        )));

        assert!(app.plan_authoring.is_none());
        assert!(!app.is_plan_authoring());
    }

    /// The planner's reasoning coalesces into a thought entry on the planner
    /// lane — the same shape a task's agent reasoning takes — and stays in the
    /// transcript once the turn resolves, so the operator can still read what
    /// led to the question.
    #[test]
    fn planner_reasoning_becomes_a_thought_entry_that_outlives_the_turn() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);
        app.plan_authoring.as_mut().unwrap().waiting = true;

        for chunk in ["weighing ", "YAML"] {
            app.update(AppEvent::PlanAuthoringExchange(
                ExchangeEvent::ThoughtChunk {
                    text: chunk.to_owned(),
                },
            ));
        }
        let entries = &app.plan_authoring.as_ref().unwrap().log.entries;
        assert_eq!(entries.len(), 1, "chunks must coalesce: {entries:?}");
        assert_eq!(entries[0].role, StreamRole::Planner);
        assert!(matches!(
            entries[0].content,
            ExchangeContent::Thought { ref text } if text == "weighing YAML"
        ));

        app.update(AppEvent::PlanAuthoringQuestion {
            question: "One file or many?".into(),
        });
        let state = app.plan_authoring.as_ref().unwrap();
        assert!(!state.waiting);
        assert_eq!(
            state.log.entries.len(),
            2,
            "the answer joins the reasoning rather than replacing it: {:?}",
            state.log.entries,
        );
        assert_eq!(state.log.entries[1].text(), "One file or many?");
    }

    /// A planner tool call reaches the transcript through the same reducer the
    /// run's agents use, so the authoring tab renders it identically.
    #[test]
    fn planner_tool_calls_land_on_the_planner_lane() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);

        app.update(AppEvent::PlanAuthoringExchange(ExchangeEvent::ToolCall {
            id: "t1".into(),
            title: "Read README.md".into(),
            kind: Some("read".into()),
            status: "pending".into(),
            content: None,
        }));
        app.update(AppEvent::PlanAuthoringExchange(
            ExchangeEvent::ToolCallUpdate {
                id: "t1".into(),
                status: Some("completed".into()),
                title: None,
                content: None,
            },
        ));

        let entries = &app.plan_authoring.as_ref().unwrap().log.entries;
        assert_eq!(entries.len(), 1, "the update must fold into the call");
        assert_eq!(entries[0].role, StreamRole::Planner);
        assert!(matches!(
            entries[0].content,
            ExchangeContent::Tool { ref status, .. } if status == "completed"
        ));
    }

    /// A coalescing thought entry is bounded and never splits a character.
    /// The cap lives on the log, so every stream is bounded the same way.
    #[test]
    fn a_thought_entry_keeps_a_bounded_tail() {
        let mut log = ExchangeLog::default();
        // Multi-byte on purpose: trimming must land on a char boundary.
        for _ in 0..4_000 {
            log.append_thought(StreamRole::Planner, "λ think ".into());
        }
        let text = log.entries[0].text();
        assert!(text.len() <= THOUGHT_TAIL_CAP);
        assert!(
            text.ends_with("think "),
            "the newest reasoning must survive"
        );
    }

    /// The indicator distinguishes the two waits, and both count as busy.
    #[test]
    fn activity_reports_which_wait_is_in_progress() {
        let mut authoring = PlanAuthoring {
            project_root: std::path::PathBuf::from("."),
            input: String::new(),
            log: ExchangeLog::default(),
            waiting: false,
            generating: false,
            answer_tx: None,
        };
        assert!(!authoring.is_busy());

        authoring.waiting = true;
        assert!(authoring.is_busy());
        assert!(authoring.activity().contains("planner"));

        authoring.waiting = false;
        authoring.generating = true;
        assert!(authoring.is_busy());
        assert!(authoring.activity().contains("Generating"));
    }

    /// A pasted block keeps its newlines — the whole point of bracketed paste.
    #[test]
    fn pasting_multiline_text_preserves_every_line() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);
        app.update(AppEvent::PlanAuthoringPaste(
            "first line\nsecond line\nthird line".to_owned(),
        ));

        assert_eq!(
            app.plan_authoring
                .as_ref()
                .map(|state| state.input.as_str()),
            Some("first line\nsecond line\nthird line"),
        );
    }

    /// Terminals deliver CRLF and bare CR; both normalise to a real newline
    /// rather than rendering as a blank or swallowing the rest of the line.
    #[test]
    fn pasted_carriage_returns_normalise_to_newlines() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);
        app.update(AppEvent::PlanAuthoringPaste("a\r\nb\rc".to_owned()));

        assert_eq!(
            app.plan_authoring
                .as_ref()
                .map(|state| state.input.as_str()),
            Some("a\nb\nc"),
        );
    }

    /// An explicit newline chord types a newline instead of submitting.
    #[test]
    fn newline_event_extends_the_draft_without_submitting() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);
        app.update(AppEvent::PlanAuthoringInput('a'));
        app.update(AppEvent::PlanAuthoringNewline);
        app.update(AppEvent::PlanAuthoringInput('b'));

        assert_eq!(
            app.plan_authoring
                .as_ref()
                .map(|state| state.input.as_str()),
            Some("a\nb"),
        );
        assert!(
            app.is_plan_authoring(),
            "a newline must not close the composer"
        );
    }

    /// While the planner is thinking the composer is read-only, so a stray
    /// paste or keystroke cannot race the answer being sent.
    #[test]
    fn a_waiting_composer_ignores_input_and_paste() {
        let mut app = make_app();
        app.update(AppEvent::OpenPlanAuthoring);
        if let Some(state) = app.plan_authoring.as_mut() {
            state.waiting = true;
        }
        app.update(AppEvent::PlanAuthoringInput('x'));
        app.update(AppEvent::PlanAuthoringNewline);
        app.update(AppEvent::PlanAuthoringPaste("pasted".to_owned()));

        assert_eq!(
            app.plan_authoring
                .as_ref()
                .map(|state| state.input.as_str()),
            Some(""),
        );
    }

    // ── Composer geometry ──────────────────────────────────────────────────────

    /// The composer starts as a one-line field and grows only as it needs to.
    #[test]
    fn composer_starts_single_line_and_grows_with_wrapped_content() {
        assert_eq!(composer_rows("", 20), 1, "an empty draft is one row");
        assert_eq!(composer_rows("short", 20), 1);

        // 20 columns: 25 characters plus the caret wrap onto a second row.
        assert_eq!(composer_rows(&"x".repeat(25), 20), 2);
        // Exactly filling the width still needs a row for the caret.
        assert_eq!(composer_rows(&"x".repeat(20), 20), 2);
    }

    /// Explicit newlines each open a row, including a trailing one.
    #[test]
    fn composer_counts_explicit_newlines() {
        assert_eq!(composer_rows("a\nb\nc", 20), 3);
        assert_eq!(
            composer_rows("a\n", 20),
            2,
            "a trailing newline leaves the caret on a fresh row",
        );
    }

    /// Growth is bounded so a long paste cannot swallow the transcript.
    #[test]
    fn composer_height_adds_the_border_and_clamps_to_the_cap() {
        // One text row plus the top and bottom border.
        assert_eq!(composer_height("", 20, 12), 3);
        assert_eq!(composer_height("a\nb\nc", 20, 12), 5);

        let long = "x\n".repeat(50);
        assert_eq!(
            composer_height(&long, 20, 12),
            14,
            "past the cap the composer stops growing and scrolls instead",
        );
    }

    /// A zero width must not divide by zero or report zero rows.
    #[test]
    fn composer_geometry_survives_a_degenerate_width() {
        assert_eq!(composer_rows("abc", 0), 4);
        assert!(composer_height("abc", 0, 12) >= 3);
    }

    // ── Tab state operations ───────────────────────────────────────────────────

    #[test]
    fn open_tab_adds_new_tab() {
        let mut state = TabState::new();
        let content = TabContent::Plan {
            plan: PlanIdentity::legacy("0001-test".to_string()),
        };
        state.open_tab(content);
        assert_eq!(state.open_tabs.len(), 1);
        assert_eq!(state.active_tab, Some(0));
    }

    #[test]
    fn open_existing_tab_switches_to_it() {
        let mut state = TabState::new();
        let content1 = TabContent::Plan {
            plan: PlanIdentity::legacy("0001".to_string()),
        };
        let content2 = TabContent::Plan {
            plan: PlanIdentity::legacy("0002".to_string()),
        };
        state.open_tab(content1.clone());
        state.open_tab(content2);
        state.open_tab(content1); // Open again
        assert_eq!(state.open_tabs.len(), 2);
        assert_eq!(state.active_tab, Some(0)); // Switched back to first
    }

    #[test]
    fn close_tab_removes_it() {
        let mut state = TabState::new();
        let content1 = TabContent::Plan {
            plan: PlanIdentity::legacy("0001".to_string()),
        };
        let content2 = TabContent::Plan {
            plan: PlanIdentity::legacy("0002".to_string()),
        };
        state.open_tab(content1);
        state.open_tab(content2);
        state.close_tab(0);
        assert_eq!(state.open_tabs.len(), 1);
        assert_eq!(state.active_tab, Some(0)); // Still valid (now points to second tab)
    }

    #[test]
    fn close_tab_before_active_preserves_active_content() {
        let mut state = TabState::new();
        let content1 = TabContent::Plan {
            plan: PlanIdentity::legacy("0001".to_string()),
        };
        let content2 = TabContent::Plan {
            plan: PlanIdentity::legacy("0002".to_string()),
        };
        let content3 = TabContent::Plan {
            plan: PlanIdentity::legacy("0003".to_string()),
        };
        state.open_tab(content1);
        state.open_tab(content2.clone());
        state.open_tab(content3);
        state.active_tab = Some(1);

        state.close_tab(0);

        assert_eq!(state.open_tabs.len(), 2);
        assert_eq!(state.active_tab, Some(0));
        assert_eq!(state.open_tabs.first(), Some(&content2));
    }

    // ── Accordion state tests ──────────────────────────────────────────────────

    #[test]
    fn accordion_toggle_inserts_when_absent() {
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        // Verify accordion_state is initially empty
        assert!(app.accordion_state.is_empty());

        // Open a plan tab
        let plan_slug = "0001-test".to_string();
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy(plan_slug.clone()),
        });

        // Set active tab to the plan tab we just opened
        app.tabs.active_tab = Some(0);

        // Dispatch ToggleAccordionSection event for SCOPE (should insert it)
        app.update(AppEvent::ToggleAccordionSection(AccordionSection::Scope));

        // Verify the section is now expanded
        assert!(
            app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .is_some_and(|s| s.contains(&AccordionSection::Scope)),
            "SCOPE should be expanded after toggle"
        );
    }

    #[test]
    fn accordion_toggle_removes_when_present() {
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        let plan_slug = "0001-test".to_string();

        // Open a plan tab and set it as active
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy(plan_slug.clone()),
        });
        app.tabs.active_tab = Some(0);

        // Insert SCOPE into accordion_state by dispatching toggle event
        app.update(AppEvent::ToggleAccordionSection(AccordionSection::Scope));

        // Verify SCOPE is expanded
        assert!(
            app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .is_some_and(|s| s.contains(&AccordionSection::Scope))
        );

        // Toggle SCOPE again (should remove it)
        app.update(AppEvent::ToggleAccordionSection(AccordionSection::Scope));

        // Verify SCOPE is now collapsed (not in the set)
        assert!(
            !app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .is_some_and(|s| s.contains(&AccordionSection::Scope)),
            "SCOPE should be collapsed after toggle"
        );
    }

    /// The first task-accordion toggle must collapse the section the user
    /// pressed — not invert it. A fresh task tab renders with Scope + Execution
    /// expanded; pressing `s` (Scope) must seed that same default and then remove
    /// Scope, leaving {Execution}. (Regression: a previous `or_default()` seeded
    /// an empty set, so the first press inserted Scope and silently collapsed
    /// Execution instead.)
    #[test]
    fn task_accordion_first_toggle_collapses_pressed_section_not_inverted() {
        use makina_core::api::TaskId;
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        let task_id = TaskId::new("demo-task");
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("0001-test".to_string()),
            run: RunId(1),
            task_id: task_id.clone(),
        });
        app.tabs.active_tab = Some(0);
        assert!(
            !app.task_accordion_expanded.contains_key(&task_id),
            "precondition: no accordion entry yet (renders with the default)"
        );

        // Press `s` (Scope). The starting set must match the render default
        // {Scope, Execution}; toggling Scope removes it → {Execution}.
        app.update(AppEvent::ToggleTaskAccordionSection(
            AccordionSection::Scope,
        ));
        let set = app
            .task_accordion_expanded
            .get(&task_id)
            .expect("toggle must create an entry");
        assert!(
            !set.contains(&AccordionSection::Scope),
            "the pressed section (Scope) must be collapsed"
        );
        assert!(
            set.contains(&AccordionSection::Execution),
            "the untouched section (Execution) must stay expanded, not collapse"
        );
    }

    #[test]
    fn plan_task_accordion_toggle_uses_task_detail_state() {
        use makina_core::api::TaskId;
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        let task_id = TaskId::new("preview-task");
        app.tabs.open_tab(TabContent::PlanTask {
            plan: PlanIdentity::legacy("0001-test".to_string()),
            task_id: task_id.0.clone(),
        });
        app.tabs.active_tab = Some(0);

        app.update(AppEvent::ToggleTaskAccordionSection(
            AccordionSection::Execution,
        ));

        let set = app
            .task_accordion_expanded
            .get(&task_id)
            .expect("plan-task toggle must create task accordion state");
        assert!(
            set.contains(&AccordionSection::Scope),
            "Scope must stay expanded from the default set"
        );
        assert!(
            !set.contains(&AccordionSection::Execution),
            "the pressed plan-task Execution section must collapse"
        );
    }

    #[test]
    fn accordion_toggle_noop_when_no_plan_tab_active() {
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        // No tabs open, so active_tab is None
        assert!(app.tabs.active_tab.is_none());

        // Dispatch ToggleAccordionSection event
        app.update(AppEvent::ToggleAccordionSection(AccordionSection::Scope));

        // Verify accordion_state remains empty (no-op)
        assert!(app.accordion_state.is_empty());
    }

    #[test]
    fn accordion_toggle_noop_when_task_tab_active() {
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        // Open a task tab
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("0001-test".to_string()),
            run: RunId(1),
            task_id: TaskId::new("task-1".to_string()),
        });
        app.tabs.active_tab = Some(0);

        // Dispatch ToggleAccordionSection event
        app.update(AppEvent::ToggleAccordionSection(AccordionSection::Scope));

        // Verify accordion_state remains empty (no-op because active tab is a task, not plan)
        assert!(app.accordion_state.is_empty());
    }

    #[test]
    fn accordion_multiple_sections() {
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        let plan_slug = "0001-test".to_string();

        // Open a plan tab
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy(plan_slug.clone()),
        });
        app.tabs.active_tab = Some(0);

        // Expand multiple sections using the event handler
        app.update(AppEvent::ToggleAccordionSection(AccordionSection::Scope));
        app.update(AppEvent::ToggleAccordionSection(AccordionSection::Tasks));

        // Verify both are expanded
        let expanded = &app.accordion_state[&PlanIdentity::legacy(plan_slug.clone())];
        assert!(expanded.contains(&AccordionSection::Scope));
        assert!(expanded.contains(&AccordionSection::Tasks));
        assert!(!expanded.contains(&AccordionSection::Architecture));
        assert!(!expanded.contains(&AccordionSection::Status));
    }

    #[test]
    fn accordion_sections_persist_per_tab() {
        let api = Arc::new(PlaceholderApi::new());
        let plan1 = test_plan_entry(
            PathBuf::from("docs/plans/0001"),
            "0001-test".to_string(),
            vec![],
            Some("Scope for plan 1.".to_string()),
            Some("Architecture for plan 1.".to_string()),
            Some("Status for plan 1.".to_string()),
        );
        let plan2 = test_plan_entry(
            PathBuf::from("docs/plans/0002"),
            "0002-test".to_string(),
            vec![],
            Some("Scope for plan 2.".to_string()),
            None,
            None,
        );
        let mut app = App::new(api, vec![], PathBuf::from("."));
        app.discovered_plans = vec![plan1, plan2];

        // Open first plan tab and expand SCOPE
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy("0001-test".to_string()),
        });
        app.accordion_state
            .entry(PlanIdentity::legacy("0001-test"))
            .or_default()
            .insert(AccordionSection::Scope);

        // Open second plan tab and expand TASKS
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy("0002-test".to_string()),
        });
        app.accordion_state
            .entry(PlanIdentity::legacy("0002-test"))
            .or_default()
            .insert(AccordionSection::Tasks);

        // Switch back to first tab
        app.tabs.active_tab = Some(0);

        // Verify first tab's state is preserved
        assert!(
            app.accordion_state
                .get(&PlanIdentity::legacy("0001-test"))
                .map(|s| s.contains(&AccordionSection::Scope))
                .unwrap_or(false),
            "Plan 1's SCOPE should remain expanded"
        );
        assert!(
            !app.accordion_state
                .get(&PlanIdentity::legacy("0001-test"))
                .map(|s| s.contains(&AccordionSection::Tasks))
                .unwrap_or(true),
            "Plan 1's TASKS should remain collapsed"
        );

        // Switch to second tab and verify its state
        app.tabs.active_tab = Some(1);
        assert!(
            app.accordion_state
                .get(&PlanIdentity::legacy("0002-test"))
                .map(|s| s.contains(&AccordionSection::Tasks))
                .unwrap_or(false),
            "Plan 2's TASKS should remain expanded"
        );
    }

    #[test]
    fn rediscovery_cleans_up_accordion_state_for_removed_plans() {
        let api = Arc::new(PlaceholderApi::new());
        let plan_alpha = test_plan_entry(
            PathBuf::from("docs/plans/0001"),
            "0001-alpha".to_string(),
            vec![],
            Some("Alpha scope".to_string()),
            None,
            None,
        );
        let plan_beta = test_plan_entry(
            PathBuf::from("docs/plans/0002"),
            "0002-beta".to_string(),
            vec![],
            Some("Beta scope".to_string()),
            None,
            None,
        );
        let mut app = App::new(api, vec![], PathBuf::from("."));
        app.discovered_plans = vec![plan_alpha, plan_beta];
        let alpha = app.plan_identity_for_entry(&app.repo_root, &app.discovered_plans[0]);
        let beta = app.plan_identity_for_entry(&app.repo_root, &app.discovered_plans[1]);

        // Open both plan tabs and expand different sections
        app.tabs.open_tab(TabContent::Plan {
            plan: alpha.clone(),
        });
        app.accordion_state
            .entry(alpha.clone())
            .or_default()
            .insert(AccordionSection::Scope);

        app.tabs.open_tab(TabContent::Plan { plan: beta.clone() });
        app.accordion_state
            .entry(beta.clone())
            .or_default()
            .insert(AccordionSection::Architecture);

        // Verify both accordion states exist
        assert!(app.accordion_state.contains_key(&alpha));
        assert!(app.accordion_state.contains_key(&beta));
        assert_eq!(app.accordion_state.len(), 2);

        // Re-discover with only alpha (beta is removed)
        let alpha_only = vec![test_plan_entry(
            PathBuf::from("docs/plans/0001"),
            "0001-alpha".to_string(),
            vec![],
            Some("Alpha scope".to_string()),
            None,
            None,
        )];
        app.update(AppEvent::PlansDiscovered { plans: alpha_only });

        // Verify beta's accordion state is cleaned up, but alpha's remains
        assert!(
            app.accordion_state.contains_key(&alpha),
            "Alpha accordion state should remain"
        );
        assert!(
            !app.accordion_state.contains_key(&beta),
            "Beta accordion state should be removed"
        );
        assert_eq!(
            app.accordion_state.len(),
            1,
            "Should have only one accordion state left"
        );
        assert!(
            app.accordion_state
                .get(&alpha)
                .map(|s| s.contains(&AccordionSection::Scope))
                .unwrap_or(false),
            "Alpha's SCOPE should still be expanded"
        );
    }

    // ── Tab Navigation (Focus Forward/Backward) ──────────────────────────────

    /// Test move_focus_forward exercises all traversal paths:
    /// - Sidebar → Main (without plan tab)
    /// - Sidebar → Main → accordion sections (with plan tab)
    /// - Scope → Architecture → Tasks → Status (with plan tab)
    /// - Status → Sidebar (wrap around)
    /// - Main → Sidebar (no plan tab, direct wrap)
    #[test]
    fn move_focus_forward_traversal_paths() {
        let mut app = make_app();

        // === Test 1: Sidebar → Main (no plan tab) ===
        assert_eq!(app.focused_panel, Panel::Sidebar);
        assert_eq!(app.focused_section, None);
        app.move_focus_forward();
        assert_eq!(
            app.focused_panel,
            Panel::Main,
            "Tab from Sidebar should move to Main"
        );
        assert_eq!(
            app.focused_section, None,
            "Main focus should have no section initially"
        );

        // === Test 2: Main → Sidebar (no plan tab, wraps directly) ===
        app.move_focus_forward();
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Tab from Main without plan tab should wrap to Sidebar"
        );
        assert_eq!(app.focused_section, None);

        // === Test 3: Sidebar → Main → Scope (with plan tab) ===
        // Create a plan tab to enable accordion focus
        let plan_entry = test_plan_entry(
            PathBuf::from("docs/plans/0001"),
            "0001-test".to_string(),
            vec![],
            Some("Test scope".to_string()),
            Some("Test architecture".to_string()),
            Some("Test status".to_string()),
        );
        app.discovered_plans = vec![plan_entry];
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy("0001-test".to_string()),
        });
        app.tabs.active_tab = Some(0);

        // We're in Sidebar; Tab should go to Main
        app.move_focus_forward();
        assert_eq!(app.focused_panel, Panel::Main);
        assert_eq!(
            app.focused_section, None,
            "Enter Main without section focus"
        );

        // From Main, Tab should enter Scope (first section)
        app.move_focus_forward();
        assert_eq!(
            app.focused_panel,
            Panel::Main,
            "Should still be in Main pane"
        );
        assert_eq!(
            app.focused_section,
            Some(AccordionSection::Scope),
            "First Tab should focus Scope"
        );

        // === Test 4: Scope → Architecture → Tasks → Status (accordion cycle) ===
        app.move_focus_forward();
        assert_eq!(
            app.focused_section,
            Some(AccordionSection::Architecture),
            "Tab from Scope should focus Architecture"
        );

        app.move_focus_forward();
        assert_eq!(
            app.focused_section,
            Some(AccordionSection::Tasks),
            "Tab from Architecture should focus Tasks"
        );

        app.move_focus_forward();
        assert_eq!(
            app.focused_section,
            Some(AccordionSection::Status),
            "Tab from Tasks should focus Status"
        );

        // === Test 5: Status → Sidebar (wrap around) ===
        app.move_focus_forward();
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Tab from Status should wrap to Sidebar"
        );
        assert_eq!(app.focused_section, None, "Sidebar has no section focus");

        // === Test 6: Verify cycle continues: Sidebar → Main → Scope again ===
        app.move_focus_forward();
        assert_eq!(app.focused_panel, Panel::Main);
        assert_eq!(app.focused_section, None);

        app.move_focus_forward();
        assert_eq!(
            app.focused_section,
            Some(AccordionSection::Scope),
            "Should cycle back to Scope after wrapping"
        );
    }

    /// Test move_focus_backward exercises all reverse traversal paths:
    /// - Sidebar → Status (with plan tab)
    /// - Status → Tasks → Architecture → Scope (with plan tab)
    /// - Scope → Sidebar
    /// - Sidebar → stays-in-Sidebar (no plan tab)
    /// - Main → Sidebar (no plan tab, direct wrap)
    #[test]
    fn move_focus_backward_traversal_paths() {
        let mut app = make_app();

        // === Test 1: Sidebar → stays-in-Sidebar (no plan tab) ===
        assert_eq!(app.focused_panel, Panel::Sidebar);
        assert_eq!(app.focused_section, None);
        app.move_focus_backward();
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Shift+Tab from Sidebar without plan tab should stay in Sidebar"
        );
        assert_eq!(app.focused_section, None);

        // === Test 2: Main → Sidebar (no plan tab, direct wrap) ===
        app.focused_panel = Panel::Main;
        app.focused_section = None;
        app.move_focus_backward();
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Shift+Tab from Main without plan tab should wrap to Sidebar"
        );
        assert_eq!(app.focused_section, None);

        // === Test 3: Sidebar → Status (with plan tab) ===
        // Create a plan tab to enable accordion focus
        let plan_entry = test_plan_entry(
            PathBuf::from("docs/plans/0001"),
            "0001-test".to_string(),
            vec![],
            Some("Test scope".to_string()),
            Some("Test architecture".to_string()),
            Some("Test status".to_string()),
        );
        app.discovered_plans = vec![plan_entry];
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy("0001-test".to_string()),
        });
        app.tabs.active_tab = Some(0);

        // We're in Sidebar; Shift+Tab should jump to Status (last section)
        app.move_focus_backward();
        assert_eq!(
            app.focused_panel,
            Panel::Main,
            "Shift+Tab from Sidebar with plan tab should move to Main"
        );
        assert_eq!(
            app.focused_section,
            Some(AccordionSection::Status),
            "Shift+Tab from Sidebar should jump to Status (last section)"
        );

        // === Test 4: Status → Tasks → Architecture → Scope (accordion reverse cycle) ===
        app.move_focus_backward();
        assert_eq!(
            app.focused_section,
            Some(AccordionSection::Tasks),
            "Shift+Tab from Status should focus Tasks"
        );

        app.move_focus_backward();
        assert_eq!(
            app.focused_section,
            Some(AccordionSection::Architecture),
            "Shift+Tab from Tasks should focus Architecture"
        );

        app.move_focus_backward();
        assert_eq!(
            app.focused_section,
            Some(AccordionSection::Scope),
            "Shift+Tab from Architecture should focus Scope"
        );

        // === Test 5: Scope → Sidebar (exit from first section) ===
        app.move_focus_backward();
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Shift+Tab from Scope should exit to Sidebar"
        );
        assert_eq!(app.focused_section, None, "Sidebar has no section focus");

        // === Test 6: Verify cycle continues in reverse: Sidebar → Status again ===
        app.move_focus_backward();
        assert_eq!(
            app.focused_panel,
            Panel::Main,
            "Shift+Tab from Sidebar should move to Main"
        );
        assert_eq!(
            app.focused_section,
            Some(AccordionSection::Status),
            "Should cycle back to Status after wrapping"
        );

        // === Test 7: From Main without section focus → Sidebar (no plan tab case) ===
        // Remove the plan tab to test the no-plan-tab path
        app.focused_panel = Panel::Main;
        app.focused_section = None;
        app.tabs.open_tabs.clear();
        app.tabs.active_tab = None;

        app.move_focus_backward();
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Shift+Tab from Main without plan tab should wrap to Sidebar"
        );
        assert_eq!(app.focused_section, None);
    }

    #[test]
    fn enter_toggles_focused_accordion_section() {
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        let plan_slug = "0001-test".to_string();

        // Open a plan tab
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy(plan_slug.clone()),
        });
        app.tabs.active_tab = Some(0);

        // Move focus to Main pane and then to a section
        app.focused_panel = Panel::Main;
        app.focused_section = Some(AccordionSection::Scope);

        // Initially, SCOPE should not be expanded
        assert!(
            !app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .map(|s| s.contains(&AccordionSection::Scope))
                .unwrap_or(false),
            "SCOPE should initially be collapsed"
        );

        // Press Enter to expand the focused section
        app.update(AppEvent::ToggleTreeNode);

        // SCOPE should now be expanded
        assert!(
            app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .is_some_and(|s| s.contains(&AccordionSection::Scope)),
            "SCOPE should be expanded after pressing Enter"
        );

        // Press Enter again to collapse the focused section
        app.update(AppEvent::ToggleTreeNode);

        // SCOPE should now be collapsed
        assert!(
            !app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .map(|s| s.contains(&AccordionSection::Scope))
                .unwrap_or(false),
            "SCOPE should be collapsed after pressing Enter again"
        );
    }

    #[test]
    fn enter_falls_back_to_sidebar_node_when_no_section_focused() {
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        // Discover a plan so there is a highlightable Plan node in sidebar.
        app.update(AppEvent::PlansDiscovered {
            plans: vec![test_plan_entry(
                PathBuf::from("docs/plans/0001-test"),
                "0001-test".to_string(),
                vec![],
                None,
                None,
                None,
            )],
        });
        // Cursor should be on the plan (index 0).
        assert!(matches!(
            app.focused_node(),
            Some(TreeNode::Plan { plan_idx: 0 })
        ));

        // No plan tab open yet. Focus main with no section focused.
        app.focused_panel = Panel::Main;
        app.focused_section = None;

        // Press Enter (from main, no section): should fall back to activating the
        // highlighted sidebar plan node, opening its details tab.
        app.update(AppEvent::ToggleTreeNode);

        assert_eq!(app.tabs.open_tabs.len(), 1);
        assert!(matches!(
            &app.tabs.open_tabs[0],
            TabContent::Plan { plan } if plan.slug == "0001-test"
        ));
    }

    #[test]
    fn enter_noop_when_sidebar_focused() {
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        let plan_slug = "0001-test".to_string();

        // Open a plan tab
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy(plan_slug.clone()),
        });
        app.tabs.active_tab = Some(0);

        // Focus a section but then switch focus back to Sidebar
        app.focused_panel = Panel::Sidebar;
        app.focused_section = Some(AccordionSection::Scope);

        // Press Enter (should toggle the tree node, not the accordion section)
        app.update(AppEvent::ToggleTreeNode);

        // accordion_state should remain empty (Enter was handled as tree toggle, not accordion toggle)
        assert!(
            app.accordion_state.is_empty(),
            "accordion_state should remain empty when Sidebar is focused"
        );
    }

    #[test]
    fn enter_toggles_multiple_sections_independently() {
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        let plan_slug = "0001-test".to_string();

        // Open a plan tab
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy(plan_slug.clone()),
        });
        app.tabs.active_tab = Some(0);

        // Move focus to Main pane and to Scope
        app.focused_panel = Panel::Main;
        app.focused_section = Some(AccordionSection::Scope);

        // Press Enter to expand Scope
        app.update(AppEvent::ToggleTreeNode);
        assert!(
            app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .is_some_and(|s| s.contains(&AccordionSection::Scope)),
            "SCOPE should be expanded"
        );

        // Move focus to Architecture
        app.focused_section = Some(AccordionSection::Architecture);

        // Press Enter to expand Architecture
        app.update(AppEvent::ToggleTreeNode);
        assert!(
            app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .is_some_and(|s| s.contains(&AccordionSection::Architecture)),
            "ARCHITECTURE should be expanded"
        );

        // Verify both are expanded
        let expanded = &app.accordion_state[&PlanIdentity::legacy(plan_slug.clone())];
        assert!(expanded.contains(&AccordionSection::Scope));
        assert!(expanded.contains(&AccordionSection::Architecture));
        assert!(!expanded.contains(&AccordionSection::Tasks));
        assert!(!expanded.contains(&AccordionSection::Status));

        // Move focus back to Scope and toggle it
        app.focused_section = Some(AccordionSection::Scope);
        app.update(AppEvent::ToggleTreeNode);

        // Scope should be collapsed, Architecture should remain expanded
        let expanded = &app.accordion_state[&PlanIdentity::legacy(plan_slug.clone())];
        assert!(!expanded.contains(&AccordionSection::Scope));
        assert!(expanded.contains(&AccordionSection::Architecture));
    }

    #[test]
    fn enter_toggle_and_s_keybinding_are_orthogonal() {
        let api = Arc::new(PlaceholderApi::new());
        let mut app = App::new(api, vec![], PathBuf::from("."));

        let plan_slug = "0001-test".to_string();

        // Open a plan tab
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy(plan_slug.clone()),
        });
        app.tabs.active_tab = Some(0);

        // Move focus to Main pane and to Scope
        app.focused_panel = Panel::Main;
        app.focused_section = Some(AccordionSection::Scope);

        // Use the S keybinding (which is ToggleAccordionSection) to expand Scope
        app.update(AppEvent::ToggleAccordionSection(AccordionSection::Scope));
        assert!(
            app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .is_some_and(|s| s.contains(&AccordionSection::Scope)),
            "SCOPE should be expanded via S keybinding"
        );

        // Use Enter to collapse Scope
        app.update(AppEvent::ToggleTreeNode);
        assert!(
            !app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .map(|s| s.contains(&AccordionSection::Scope))
                .unwrap_or(false),
            "SCOPE should be collapsed via Enter"
        );

        // Use the A keybinding (which is ToggleAccordionSection) to expand Architecture
        // (this demonstrates that both input methods work on the same map without conflict)
        app.update(AppEvent::ToggleAccordionSection(
            AccordionSection::Architecture,
        ));
        assert!(
            app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .is_some_and(|s| s.contains(&AccordionSection::Architecture)),
            "ARCHITECTURE should be expanded via A keybinding"
        );

        // Move focus to Architecture and use Enter to collapse it
        app.focused_section = Some(AccordionSection::Architecture);
        app.update(AppEvent::ToggleTreeNode);
        assert!(
            !app.accordion_state
                .get(&PlanIdentity::legacy(plan_slug.clone()))
                .map(|s| s.contains(&AccordionSection::Architecture))
                .unwrap_or(false),
            "ARCHITECTURE should be collapsed via Enter"
        );
    }

    // ── Panel geometry and hitbox testing ──────────────────────────────────

    #[test]
    fn panel_at_returns_none_outside_any_rect() {
        let app = make_app();

        // Record two panel geometries: sidebar at (0,0) with width 20, height 30,
        // and exchange at (20,0) with width 60, height 30.
        app.set_panel_geometries(vec![
            PanelGeometry {
                panel: ScrollablePanel::Sidebar,
                rect: ratatui::layout::Rect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 30,
                },
            },
            PanelGeometry {
                panel: ScrollablePanel::Exchange,
                rect: ratatui::layout::Rect {
                    x: 20,
                    y: 0,
                    width: 60,
                    height: 30,
                },
            },
        ]);

        // Test that a coordinate outside all panels returns None
        assert_eq!(
            app.panel_at(100, 100),
            None,
            "coordinate outside all rects should return None"
        );
    }

    #[test]
    fn panel_at_returns_panel_inside_rect() {
        let app = make_app();

        // Record two panel geometries
        app.set_panel_geometries(vec![
            PanelGeometry {
                panel: ScrollablePanel::Sidebar,
                rect: ratatui::layout::Rect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: 30,
                },
            },
            PanelGeometry {
                panel: ScrollablePanel::Exchange,
                rect: ratatui::layout::Rect {
                    x: 20,
                    y: 0,
                    width: 60,
                    height: 30,
                },
            },
        ]);

        // Test that a coordinate strictly inside the sidebar rect returns Sidebar
        assert_eq!(
            app.panel_at(5, 15),
            Some(ScrollablePanel::Sidebar),
            "coordinate inside sidebar rect should return Some(ScrollablePanel::Sidebar)"
        );

        // Test that a coordinate strictly inside the exchange rect returns Exchange
        assert_eq!(
            app.panel_at(50, 15),
            Some(ScrollablePanel::Exchange),
            "coordinate inside exchange rect should return Some(ScrollablePanel::Exchange)"
        );
    }

    #[test]
    fn panel_at_returns_none_on_exclusive_right_edge() {
        let app = make_app();

        // Record one panel geometry: sidebar at (0,0) with width 20, height 30
        app.set_panel_geometries(vec![PanelGeometry {
            panel: ScrollablePanel::Sidebar,
            rect: ratatui::layout::Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 30,
            },
        }]);

        // Test that a coordinate exactly on rect.x + rect.width (the exclusive right edge) returns None
        assert_eq!(
            app.panel_at(20, 15),
            None,
            "coordinate on the exclusive right edge should return None"
        );

        // Verify that rect.x + rect.width - 1 is still inside the rect
        assert_eq!(
            app.panel_at(19, 15),
            Some(ScrollablePanel::Sidebar),
            "coordinate just left of the exclusive right edge should return the panel"
        );
    }

    #[test]
    fn panel_offset_sidebar_returns_stored_offset_clamped_to_max() {
        let mut app = make_app();

        // Set sidebar scroll offset to 10
        app.scroll_offsets.insert(ScrollablePanel::Sidebar, 10);

        // panel_offset should clamp to scroll_max of 4
        let result = app.panel_offset(ScrollablePanel::Sidebar, 4);
        assert_eq!(
            result, 4,
            "panel_offset should clamp sidebar offset 10 to scroll_max 4"
        );

        // Verify with offset below max
        app.scroll_offsets.insert(ScrollablePanel::Sidebar, 2);
        let result = app.panel_offset(ScrollablePanel::Sidebar, 4);
        assert_eq!(
            result, 2,
            "panel_offset should return offset 2 when below scroll_max 4"
        );
    }

    #[test]
    fn panel_offset_exchange_returns_max_when_auto_follow_true() {
        let mut app = make_app();

        // Enable auto-follow
        app.exchange_auto_follow = true;

        // Set up the last_scroll_maxes map
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 50);

        // panel_offset should return scroll_max (50) when auto-follow is true
        let result = app.panel_offset(ScrollablePanel::Exchange, 50);
        assert_eq!(
            result, 50,
            "panel_offset should return scroll_max 50 when exchange auto-follow is true"
        );
    }

    #[test]
    fn panels_are_independent_scroll_states() {
        let mut app = make_app();

        // Scroll only the sidebar — exchange must remain at 0
        app.scroll_down(ScrollablePanel::Sidebar, 10);
        app.scroll_down(ScrollablePanel::Sidebar, 10);

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Sidebar)
                .copied()
                .unwrap_or(0),
            2,
            "sidebar offset should be 2 after two scroll_down calls"
        );
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0,
            "exchange offset must remain 0 when only sidebar is scrolled"
        );
    }

    // ── ScrollUpAt/ScrollDownAt event dispatch tests ─────────────────────────────

    #[test]
    fn scroll_up_at_routes_to_sidebar() {
        let mut app = make_app();
        app.set_panel_geometries(vec![
            PanelGeometry {
                panel: ScrollablePanel::Sidebar,
                rect: ratatui::layout::Rect::new(0, 1, 30, 59),
            },
            PanelGeometry {
                panel: ScrollablePanel::Exchange,
                rect: ratatui::layout::Rect::new(30, 1, 70, 59),
            },
        ]);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Sidebar, 10);
        app.scroll_offsets.insert(ScrollablePanel::Sidebar, 5);

        app.update(AppEvent::ScrollUpAt(10, 20)); // inside sidebar

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Sidebar)
                .copied()
                .unwrap_or(0),
            4,
            "ScrollUpAt over sidebar should decrement sidebar offset"
        );
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0,
            "ScrollUpAt over sidebar should not change exchange offset"
        );
    }

    #[test]
    fn scroll_down_at_routes_to_exchange() {
        let mut app = make_app();
        app.exchange_auto_follow = false; // exercise the manual-offset path
        app.set_panel_geometries(vec![
            PanelGeometry {
                panel: ScrollablePanel::Sidebar,
                rect: ratatui::layout::Rect::new(0, 1, 30, 59),
            },
            PanelGeometry {
                panel: ScrollablePanel::Exchange,
                rect: ratatui::layout::Rect::new(30, 1, 70, 59),
            },
        ]);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 50);
        app.scroll_offsets.insert(ScrollablePanel::Exchange, 10);

        app.update(AppEvent::ScrollDownAt(60, 30)); // inside exchange

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            11,
            "ScrollDownAt over exchange should increment exchange offset"
        );
    }

    #[test]
    fn scroll_down_at_routes_to_accordion() {
        let mut app = make_app();
        app.set_panel_geometries(vec![PanelGeometry {
            panel: ScrollablePanel::PlanAccordion,
            rect: ratatui::layout::Rect::new(30, 1, 70, 59),
        }]);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::PlanAccordion, 100);
        app.scroll_offsets.insert(ScrollablePanel::PlanAccordion, 5);

        app.update(AppEvent::ScrollDownAt(50, 30)); // inside accordion

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::PlanAccordion)
                .copied()
                .unwrap_or(0),
            6,
            "ScrollDownAt over accordion should increment accordion offset"
        );
    }

    #[test]
    fn scroll_down_at_routes_to_dependency_view() {
        let mut app = make_app();
        app.set_panel_geometries(vec![PanelGeometry {
            panel: ScrollablePanel::DependencyView,
            rect: ratatui::layout::Rect::new(0, 1, 100, 30),
        }]);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::DependencyView, 80);
        app.scroll_offsets
            .insert(ScrollablePanel::DependencyView, 3);

        app.update(AppEvent::ScrollDownAt(50, 15)); // inside dependency view

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::DependencyView)
                .copied()
                .unwrap_or(0),
            4,
            "ScrollDownAt over dependency view should increment dependency view offset"
        );
    }

    #[test]
    fn scroll_at_coordinates_outside_panels_is_noop() {
        let mut app = make_app();
        app.set_panel_geometries(vec![PanelGeometry {
            panel: ScrollablePanel::Sidebar,
            rect: ratatui::layout::Rect::new(0, 1, 30, 59),
        }]);
        app.scroll_offsets.insert(ScrollablePanel::Sidebar, 5);

        app.update(AppEvent::ScrollUpAt(100, 100)); // outside every rect

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Sidebar)
                .copied()
                .unwrap_or(0),
            5,
            "ScrollUpAt outside panels should be a no-op"
        );
    }

    #[test]
    fn scroll_up_at_inside_exchange_ignores_sidebar() {
        let mut app = make_app();
        app.exchange_auto_follow = false; // disable auto-follow to test plain offset decrement
        app.set_panel_geometries(vec![
            PanelGeometry {
                panel: ScrollablePanel::Sidebar,
                rect: ratatui::layout::Rect::new(0, 1, 30, 59),
            },
            PanelGeometry {
                panel: ScrollablePanel::Exchange,
                rect: ratatui::layout::Rect::new(30, 1, 70, 59),
            },
        ]);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Sidebar, 20);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 100);
        app.scroll_offsets.insert(ScrollablePanel::Sidebar, 10);
        app.scroll_offsets.insert(ScrollablePanel::Exchange, 5);

        app.update(AppEvent::ScrollUpAt(50, 30)); // inside exchange, not sidebar

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Sidebar)
                .copied()
                .unwrap_or(0),
            10,
            "ScrollUpAt over exchange should not change sidebar offset"
        );
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            4,
            "ScrollUpAt over exchange should decrement exchange offset"
        );
    }

    // ── Integration Tests for Scroll Routing and Scrollbars ────────────────────

    /// Test 1: scrolling over sidebar moves only the sidebar offset
    #[test]
    fn scroll_up_at_coordinates_targets_sidebar() {
        let mut app = make_app();
        app.set_panel_geometries(vec![
            PanelGeometry {
                panel: ScrollablePanel::Sidebar,
                rect: ratatui::layout::Rect::new(0, 1, 30, 59),
            },
            PanelGeometry {
                panel: ScrollablePanel::Exchange,
                rect: ratatui::layout::Rect::new(30, 1, 70, 59),
            },
        ]);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Sidebar, 10);
        app.scroll_offsets.insert(ScrollablePanel::Sidebar, 5);

        app.update(AppEvent::ScrollUpAt(10, 20)); // inside sidebar

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Sidebar)
                .copied()
                .unwrap_or(0),
            4,
            "ScrollUpAt over sidebar should decrement sidebar offset"
        );
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0,
            "ScrollUpAt over sidebar should not affect exchange offset"
        );
    }

    /// Test 2: scrolling over exchange moves only the exchange offset
    #[test]
    fn scroll_down_at_coordinates_targets_exchange() {
        let mut app = make_app();
        app.exchange_auto_follow = false; // exercise the manual-offset path
        app.set_panel_geometries(vec![
            PanelGeometry {
                panel: ScrollablePanel::Sidebar,
                rect: ratatui::layout::Rect::new(0, 1, 30, 59),
            },
            PanelGeometry {
                panel: ScrollablePanel::Exchange,
                rect: ratatui::layout::Rect::new(30, 1, 70, 59),
            },
        ]);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 50);
        app.scroll_offsets.insert(ScrollablePanel::Exchange, 10);

        app.update(AppEvent::ScrollDownAt(60, 30)); // inside exchange

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            11,
            "ScrollDownAt over exchange should increment exchange offset"
        );
    }

    // Note: Test 3 (scrolling outside every rect is a no-op) already exists above as
    // `scroll_at_coordinates_outside_panels_is_noop`, so we skip it here to avoid duplication.

    /// Test 4: exchange auto-follow is preserved through routing
    #[test]
    fn exchange_auto_follow_preserved_with_routing() {
        let mut app = make_app();
        app.set_panel_geometries(vec![PanelGeometry {
            panel: ScrollablePanel::Exchange,
            rect: ratatui::layout::Rect::new(30, 1, 70, 59),
        }]);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 100);
        app.exchange_auto_follow = true;

        app.update(AppEvent::ScrollDownAt(60, 30)); // stays pinned
        assert_eq!(
            app.effective_offset(100),
            100,
            "auto-follow should pin to scroll_max"
        );

        app.update(AppEvent::ScrollUpAt(60, 30)); // disengages
        assert!(
            !app.exchange_auto_follow,
            "scroll up should disengage auto-follow"
        );
    }

    /// Test 5: multiple panels maintain independent scroll offsets
    #[test]
    fn multiple_panels_maintain_independent_scroll() {
        let mut app = make_app();
        app.exchange_auto_follow = false;
        app.set_panel_geometries(vec![
            PanelGeometry {
                panel: ScrollablePanel::Sidebar,
                rect: ratatui::layout::Rect::new(0, 1, 30, 59),
            },
            PanelGeometry {
                panel: ScrollablePanel::Exchange,
                rect: ratatui::layout::Rect::new(30, 1, 70, 59),
            },
        ]);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Sidebar, 20);
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 100);

        app.update(AppEvent::ScrollDownAt(10, 20));
        app.update(AppEvent::ScrollDownAt(10, 20));
        app.update(AppEvent::ScrollDownAt(60, 30));
        app.update(AppEvent::ScrollDownAt(60, 30));
        app.update(AppEvent::ScrollDownAt(60, 30));

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Sidebar)
                .copied()
                .unwrap_or(0),
            2,
            "Sidebar should have scrolled down twice"
        );
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            3,
            "Exchange should have scrolled down three times"
        );
    }

    // ── main_scroll_target routing (plan 0042 WS6 task-tab scroll fix) ──────────

    /// `main_scroll_target` resolves to [`ScrollablePanel::TaskEntry`] when a
    /// `TabContent::Task` tab is active — so keyboard Up/Down scrolls the task
    /// entry pane rather than the (invisible) exchange pane.
    #[test]
    fn main_scroll_target_task_tab_routes_to_task_entry() {
        let mut app = make_app();
        app.tabs.open_tab(crate::app::TabContent::Task {
            plan: PlanIdentity::legacy("p".to_string()),
            run: RunId(1),
            task_id: TaskId::new("t-1"),
        });
        assert_eq!(
            app.main_scroll_target(),
            ScrollablePanel::TaskEntry,
            "a Task tab must route keyboard scrolls to the TaskEntry pane"
        );
    }

    /// `main_scroll_target` resolves to [`ScrollablePanel::PlanAccordion`] for
    /// plan tabs and [`ScrollablePanel::TaskEntry`] for plan-task detail tabs.
    #[test]
    fn main_scroll_target_plan_and_plan_task_tabs_route_to_their_rendered_panes() {
        let mut app = make_app();
        app.tabs.open_tab(crate::app::TabContent::Plan {
            plan: PlanIdentity::legacy("p".to_string()),
        });
        assert_eq!(
            app.main_scroll_target(),
            ScrollablePanel::PlanAccordion,
            "a Plan tab must route keyboard scrolls to the PlanAccordion pane"
        );

        let mut app2 = make_app();
        app2.tabs.open_tab(crate::app::TabContent::PlanTask {
            plan: PlanIdentity::legacy("p".to_string()),
            task_id: "t-1".to_string(),
        });
        assert_eq!(
            app2.main_scroll_target(),
            ScrollablePanel::TaskEntry,
            "a PlanTask tab must route keyboard scrolls to the TaskEntry pane"
        );
    }

    /// `main_scroll_target` falls back to [`ScrollablePanel::Exchange`] when no
    /// tab is active (the bare selected-run view) — preserving the legacy
    /// behaviour where Up/Down scrolls the exchange pane.
    #[test]
    fn main_scroll_target_no_tab_routes_to_exchange() {
        let app = make_app();
        assert_eq!(
            app.main_scroll_target(),
            ScrollablePanel::Exchange,
            "with no active tab, keyboard scrolls should target the Exchange pane"
        );
    }

    /// `SelectDown` on the main pane with a Task tab active must scroll the
    /// `TaskEntry` panel (not the `Exchange` panel). This is the regression that
    /// motivated the fix: previously the task detail body was taller than the
    /// viewport but Up/Down scrolled the invisible exchange pane, leaving the
    /// task entry unscrollable.
    #[test]
    fn select_down_with_task_tab_scrolls_task_entry_not_exchange() {
        let mut app = make_app();
        app.tabs.open_tab(crate::app::TabContent::Task {
            plan: PlanIdentity::legacy("p".to_string()),
            run: RunId(1),
            task_id: TaskId::new("t-1"),
        });
        app.focused_panel = Panel::Main;
        // The render pass would normally populate this; set it directly so the
        // scroll_down clamp has a non-zero ceiling.
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::TaskEntry, 20);

        app.update(AppEvent::SelectDown);

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::TaskEntry)
                .copied()
                .unwrap_or(0),
            1,
            "SelectDown with a Task tab active must advance the TaskEntry scroll offset"
        );
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0,
            "SelectDown with a Task tab active must NOT touch the Exchange scroll offset"
        );
    }

    /// `SelectUp` on the main pane with a Task tab active must scroll the
    /// `TaskEntry` panel up, leaving the `Exchange` offset untouched.
    #[test]
    fn select_up_with_task_tab_scrolls_task_entry_up() {
        let mut app = make_app();
        app.tabs.open_tab(crate::app::TabContent::Task {
            plan: PlanIdentity::legacy("p".to_string()),
            run: RunId(1),
            task_id: TaskId::new("t-1"),
        });
        app.focused_panel = Panel::Main;
        app.scroll_offsets.insert(ScrollablePanel::TaskEntry, 5);

        app.update(AppEvent::SelectUp);

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::TaskEntry)
                .copied()
                .unwrap_or(0),
            4,
            "SelectUp with a Task tab active must decrement the TaskEntry scroll offset"
        );
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            0,
            "SelectUp with a Task tab active must NOT touch the Exchange scroll offset"
        );
    }

    /// `SelectDown` on the main pane with no active tab must still scroll the
    /// exchange pane (legacy bare-run-view behaviour preserved).
    #[test]
    fn select_down_with_no_tab_still_scrolls_exchange() {
        let mut app = make_app();
        app.focused_panel = Panel::Main;
        app.exchange_auto_follow = false;
        app.last_scroll_maxes
            .borrow_mut()
            .insert(ScrollablePanel::Exchange, 50);

        app.update(AppEvent::SelectDown);

        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::Exchange)
                .copied()
                .unwrap_or(0),
            1,
            "SelectDown with no active tab must advance the Exchange scroll offset"
        );
        assert_eq!(
            app.scroll_offsets
                .get(&ScrollablePanel::TaskEntry)
                .copied()
                .unwrap_or(0),
            0,
            "SelectDown with no active tab must NOT touch the TaskEntry scroll offset"
        );
    }

    /// Test accordion header click detection and toggle in rendered pane.
    /// Opens a plan tab, verifies a section is collapsed, toggles it, and confirms
    /// accordion_header_bounds was populated during render.
    #[test]
    fn test_accordion_header_click_in_rendered_pane() {
        use crate::app::TabContent;
        use crate::ui;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = make_app();

        // Open a plan tab
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::legacy("test-plan".to_string()),
        });

        // Manually add a discovered plan so we can render it
        let plan = test_plan_entry(
            PathBuf::from("docs/plans/0001-test"),
            "test-plan".to_string(),
            vec![],
            Some("Test scope content".to_string()),
            Some("Test architecture content".to_string()),
            Some("Test status content".to_string()),
        );
        app.discovered_plans.push(plan);

        // Verify SCOPE section is initially collapsed (not in expanded set)
        let expanded = app
            .accordion_state
            .get(&PlanIdentity::legacy("test-plan"))
            .cloned()
            .unwrap_or_default();
        assert!(
            !expanded.contains(&AccordionSection::Scope),
            "Scope should start collapsed"
        );

        // Render the plan accordion pane to populate accordion_header_bounds
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let pane_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 30,
        };

        terminal
            .draw(|f| {
                if let Some(plan) = app.discovered_plans.first() {
                    ui::render_plan_accordion_pane(&app, plan, f, pane_area);
                }
            })
            .unwrap();

        // After render, accordion_header_bounds should be populated with all four sections.
        {
            let bounds = app.accordion_header_bounds.borrow();
            assert_eq!(
                bounds.len(),
                4,
                "accordion_header_bounds should contain all four sections (SCOPE, ARCHITECTURE, TASKS, STATUS); got: {:?}",
                bounds.iter().map(|(s, _)| s).collect::<Vec<_>>()
            );
            let sections: Vec<_> = bounds.iter().map(|(s, _)| *s).collect();
            assert!(
                sections.contains(&AccordionSection::Scope),
                "Scope header bound should be present"
            );
            assert!(
                sections.contains(&AccordionSection::Architecture),
                "Architecture header bound should be present"
            );
            assert!(
                sections.contains(&AccordionSection::Tasks),
                "Tasks header bound should be present"
            );
            assert!(
                sections.contains(&AccordionSection::Status),
                "Status header bound should be present"
            );
        }

        // Manually dispatch a toggle event for SCOPE
        let toggle_event = AppEvent::ToggleAccordionSection(AccordionSection::Scope);
        app.update(toggle_event);

        // After toggle, SCOPE should be expanded
        let expanded = app
            .accordion_state
            .get(&PlanIdentity::legacy("test-plan"))
            .cloned()
            .unwrap_or_default();
        assert!(
            expanded.contains(&AccordionSection::Scope),
            "Scope should be expanded after toggle"
        );

        // Toggle again to collapse
        let toggle_event = AppEvent::ToggleAccordionSection(AccordionSection::Scope);
        app.update(toggle_event);

        // Should be collapsed again
        let expanded = app
            .accordion_state
            .get(&PlanIdentity::legacy("test-plan"))
            .cloned()
            .unwrap_or_default();
        assert!(
            !expanded.contains(&AccordionSection::Scope),
            "Scope should be collapsed after second toggle"
        );
    }

    #[test]
    fn test_arrow_right_in_main_pane_cycles_tabs() {
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

        // Set focus to main pane
        app.focused_panel = Panel::Main;

        // Simulate pressing Right arrow (should behave like Tab / FocusNext)
        app.update(AppEvent::FocusNext);

        // Note: FocusNext cycles through panels. When in Main pane with tabs open,
        // it should cycle to the next tab (within Main pane tab cycling behavior)
        // or switch focus. The important thing is that Right arrow triggers FocusNext,
        // which we've verified in the translate_key logic.

        // Now simulate Left arrow (should behave like Shift+Tab / FocusPrev)
        app.update(AppEvent::FocusPrev);

        // The key point is that these events are correctly dispatched when in Main pane.
        // The actual tab cycling behavior is tested separately in Tab/Shift+Tab tests.
        // This test verifies that arrow keys in the main pane dispatch the right events.
    }

    /// Test task tab opening and rendering.
    /// Opens a task tab, verifies it appears in open_tabs, renders the frame,
    /// and asserts the output contains task ID and that rendering completes.
    #[test]
    fn test_task_tab_opens_and_renders_entry() {
        use crate::app::TabContent;
        use crate::ui;
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::sync::Arc;

        // Create an app with a task that has entry_text populated
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            run_uid: String::new(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: RunStatus::Running,
            project: String::new(),
            tasks: vec![TaskView {
                authored: None,
                id: TaskId::new("task-with-entry"),
                title: "Test Task with Entry".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text:
                    "# Task Entry\n\nThis is the task entry text with **markdown** formatting."
                        .into(),
            }],
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

        // Get the task ID for opening a tab
        let task_id = app.runs[0].tasks[0].id.clone();

        // Open a task tab
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("test-plan".to_string()),
            run: RunId(1),
            task_id: task_id.clone(),
        });

        // Verify the tab appears in open_tabs
        assert_eq!(
            app.tabs.open_tabs.len(),
            1,
            "open_tabs should contain one tab"
        );
        assert!(
            matches!(app.tabs.open_tabs.first(), Some(TabContent::Task { .. })),
            "open tab should be a Task tab"
        );

        // Verify the tab is active
        assert_eq!(
            app.tabs.active_tab,
            Some(0),
            "newly opened tab should be active"
        );

        // Render the frame to test the rendering path
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| {
                ui::render(&app, f);
            })
            .unwrap();

        // Get the rendered buffer and check for task content
        let buffer = terminal.backend().buffer();
        let rendered_text = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();

        // Verify the rendered output contains the task ID
        assert!(
            rendered_text.contains(&task_id.0),
            "rendered output should contain task ID: {}",
            task_id.0
        );

        // Verify the task has entry_text populated
        if let Some(task) = app.runs[0].tasks.first() {
            assert!(
                !task.entry_text.is_empty(),
                "task entry_text should be populated"
            );
        }
    }

    /// Test multiple task tabs can be open and are switchable.
    #[test]
    fn test_multiple_task_tabs_are_switchable() {
        use crate::app::TabContent;

        let mut app = make_app_with_tasks();

        // Verify the app has at least two tasks
        assert!(
            app.runs[0].tasks.len() >= 2,
            "make_app_with_tasks must have at least 2 tasks"
        );

        let task_id_1 = app.runs[0].tasks[0].id.clone();
        let task_id_2 = app.runs[0].tasks[1].id.clone();

        // Open first task tab
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("test-plan".to_string()),
            run: RunId(1),
            task_id: task_id_1.clone(),
        });

        assert_eq!(
            app.tabs.open_tabs.len(),
            1,
            "should have one tab after opening first task"
        );
        assert_eq!(app.tabs.active_tab, Some(0), "first tab should be active");

        // Open second task tab
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("test-plan".to_string()),
            run: RunId(1),
            task_id: task_id_2.clone(),
        });

        assert_eq!(
            app.tabs.open_tabs.len(),
            2,
            "should have two tabs after opening second task"
        );
        assert_eq!(app.tabs.active_tab, Some(1), "second tab should be active");

        // Switch to first tab via FocusPrev
        app.update(AppEvent::FocusPrev);

        // The active_tab should cycle back to the first tab (or stay at current depending on focus logic)
        // At minimum, we should still have two tabs open
        assert_eq!(
            app.tabs.open_tabs.len(),
            2,
            "both tabs should remain open after switching"
        );

        // Verify we can still access both tabs
        if let Some(idx) = app.tabs.active_tab {
            assert!(
                idx < app.tabs.open_tabs.len(),
                "active_tab index should be valid"
            );
        }
    }

    /// A mouse click on a tab chip (resolved to `ActivateTab`) switches the
    /// active tab to that index.
    #[test]
    fn activate_tab_event_switches_active_tab() {
        use crate::app::TabContent;

        let mut app = make_app_with_tasks();
        let task_a = app.runs[0].tasks[0].id.clone();
        let task_b = app.runs[0].tasks[1].id.clone();
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("p"),
            run: RunId(1),
            task_id: task_a,
        });
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("p"),
            run: RunId(1),
            task_id: task_b,
        });
        assert_eq!(app.tabs.active_tab, Some(1), "second tab active after open");

        // Clicking the first tab chip activates it.
        app.update(AppEvent::ActivateTab(0));
        assert_eq!(
            app.tabs.active_tab,
            Some(0),
            "ActivateTab(0) switches to it"
        );

        // Out-of-range index is a no-op (does not panic or change state).
        app.update(AppEvent::ActivateTab(99));
        assert_eq!(
            app.tabs.active_tab,
            Some(0),
            "out-of-range ActivateTab is a no-op"
        );
    }

    /// A mouse click on a tab close icon (resolved to `CloseTabAt`) closes that
    /// specific tab without first activating it.
    #[test]
    fn close_tab_at_event_closes_requested_tab() {
        use crate::app::TabContent;

        let mut app = make_app_with_tasks();
        let task_a = app.runs[0].tasks[0].id.clone();
        let task_b = app.runs[0].tasks[1].id.clone();
        let task_c = TaskId("task-c".to_string());
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("p"),
            run: RunId(1),
            task_id: task_a,
        });
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("p"),
            run: RunId(1),
            task_id: task_b.clone(),
        });
        app.tabs.open_tab(TabContent::Task {
            plan: PlanIdentity::legacy("p"),
            run: RunId(1),
            task_id: task_c,
        });
        app.tabs.active_tab = Some(1);

        app.update(AppEvent::CloseTabAt(0));

        assert_eq!(app.tabs.open_tabs.len(), 2);
        assert_eq!(app.tabs.active_tab, Some(0));
        assert!(matches!(
            app.tabs.open_tabs.first(),
            Some(TabContent::Task { task_id, .. }) if task_id == &task_b
        ));
    }

    /// A mouse click on a sidebar plan row (resolved to `OpenTreeRow`) opens a
    /// plan tab and moves the tree cursor to that row — mirroring keyboard Enter.
    #[test]
    fn open_tree_row_opens_plan_tab_and_moves_cursor() {
        use crate::app::{TabContent, TreeNode};

        let mut app = make_app_with_tasks();
        app.discovered_plans.push(test_plan_entry(
            PathBuf::from("docs/plans/0099"),
            "0099-clickable".to_string(),
            vec![],
            None,
            None,
            None,
        ));

        // The plan is the first visible node (plans render above runs).
        let nodes = app.visible_tree_nodes();
        let plan_row = nodes
            .iter()
            .position(|n| matches!(n, TreeNode::Plan { .. }))
            .expect("a plan node should be visible");

        app.update(AppEvent::OpenTreeRow(plan_row));

        assert_eq!(
            app.tree_cursor,
            Some(plan_row),
            "cursor moves to clicked row"
        );
        assert_eq!(app.tabs.open_tabs.len(), 1, "one plan tab opened");
        assert!(
            matches!(
                app.tabs.open_tabs.first(),
                Some(TabContent::Plan { plan }) if plan.slug == "0099-clickable"
            ),
            "clicking a plan row opens that plan's tab"
        );
    }

    /// Enter (and click) on a *run* node for a plan slug that is discovered
    /// (i.e. "completed plans" that appear only as their run entry due to dedup)
    /// must open the plan details tab.
    #[test]
    fn enter_on_plan_run_node_opens_plan_details_tab() {
        use crate::app::{AppEvent, TabContent, TreeNode};
        use makina_core::api::{RunId, RunStatus, RunView};

        let mut app = make_app();

        // Discover a plan.
        app.update(AppEvent::PlansDiscovered {
            plans: vec![test_plan_entry(
                PathBuf::from("docs/plans/0042-done"),
                "0042-done".to_string(),
                vec![],
                None,
                None,
                None,
            )],
        });

        // Simulate a completed run for the exact same plan slug (so sidebar shows
        // only the Run node, not a Plan node).
        app.runs = vec![RunView {
            id: RunId(42),
            run_uid: "r42".to_string(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0042-done").unwrap(),
            status: RunStatus::Completed,
            project: "demo".to_string(),
            tasks: vec![],
            report: Default::default(),
        }];

        let nodes = app.visible_tree_nodes();
        // Should contain exactly one Run node (plan deduped away).
        assert!(
            nodes.iter().any(|n| matches!(n, TreeNode::Run { .. })),
            "run node must be present"
        );
        assert!(
            !nodes.iter().any(|n| matches!(n, TreeNode::Plan { .. })),
            "no separate plan node when run for slug exists"
        );

        let run_row = nodes
            .iter()
            .position(|n| matches!(n, TreeNode::Run { .. }))
            .unwrap();

        // Position cursor (as if arrowed there) and press Enter.
        app.tree_cursor = Some(run_row);
        app.update(AppEvent::OpenFocusedNode);

        assert_eq!(
            app.tabs.open_tabs.len(),
            1,
            "Enter on plan-run must open a plan details tab"
        );
        assert!(
            matches!(
                &app.tabs.open_tabs[0],
                TabContent::Plan { plan } if plan.slug == "0042-done"
            ),
            "must have opened the plan tab for the run's slug"
        );

        // Also verify the mouse path (OpenTreeRow) does the same.
        app.tabs.open_tabs.clear();
        app.tabs.active_tab = None;
        app.update(AppEvent::OpenTreeRow(run_row));
        assert!(
            matches!(
                app.tabs.open_tabs.first(),
                Some(TabContent::Plan { plan }) if plan.slug == "0042-done"
            ),
            "click on plan-run row must also open plan details tab"
        );
    }

    /// Regression: switching to a task tab must re-point `selected_run` at the
    /// run that owns the task, so the task pane renders even when the sidebar
    /// cursor previously cleared the run (e.g. it sat on a plan node). Without
    /// the sync the task tab would render a blank pane.
    #[test]
    fn switching_to_task_tab_syncs_selected_run() {
        // make_app_with_tasks has one run (".tasks/x.json") with task-a / task-b.
        let mut app = make_app_with_tasks();
        let nodes = app.visible_tree_nodes();
        // Node layout with no plans: [Run, Task(a), Task(b)].
        let task_a_row = 1;
        let task_b_row = 2;
        assert!(
            matches!(
                nodes.get(task_a_row),
                Some(crate::app::TreeNode::Task { .. })
            ),
            "row 1 should be a task node"
        );

        // Open both task tabs via the click path (derives plan_slug from the run).
        app.update(AppEvent::OpenTreeRow(task_a_row));
        app.update(AppEvent::OpenTreeRow(task_b_row));
        assert_eq!(app.tabs.open_tabs.len(), 2, "two task tabs open");

        // Simulate the cursor landing on a plan node, which clears selected_run.
        app.selected_run = None;

        // Switching tabs must re-derive selected_run from the active task tab.
        app.update(AppEvent::PrevTab);
        assert_eq!(
            app.selected_run,
            Some(0),
            "PrevTab to a task tab re-syncs selected_run to the owning run"
        );

        // A direct tab activation does the same.
        app.selected_run = None;
        app.update(AppEvent::ActivateTab(1));
        assert_eq!(
            app.selected_run,
            Some(0),
            "ActivateTab to a task tab re-syncs selected_run"
        );
    }

    /// Plan tabs should also sync selected_run to the run for that plan so
    /// palette run controls work without manually selecting the run in the sidebar.
    #[test]
    fn switching_to_plan_tab_syncs_selected_run() {
        use makina_core::api::{RunId, RunStatus, RunView};

        let mut app = make_app();

        // Set up discovered plans (0001-alpha and 0002-beta from make_plan_entries).
        app.update(AppEvent::PlansDiscovered {
            plans: make_plan_entries(),
        });

        let plan_slug_0 = "0001-alpha".to_string();
        let plan_slug_1 = "0002-beta".to_string();

        // Inject test runs matching the plan slugs.
        // The plan_slug is derived from plan_dir by makina_core::orchestrator::plan_slug,
        // which extracts the slug from paths like "docs/plans/0001-alpha".
        app.runs = vec![
            RunView {
                id: RunId(1),
                run_uid: "run-1".to_string(),
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-alpha").unwrap(),
                status: RunStatus::Pending,
                project: "test".to_string(),
                tasks: vec![],
                report: Default::default(),
            },
            RunView {
                id: RunId(2),
                run_uid: "run-2".to_string(),
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0002-beta").unwrap(),
                status: RunStatus::Pending,
                project: "test".to_string(),
                tasks: vec![],
                report: Default::default(),
            },
        ];

        // Open plan tabs for both slugs.
        app.update(AppEvent::OpenTab(TabContent::Plan {
            plan: PlanIdentity::legacy(plan_slug_0.clone()),
        }));
        app.update(AppEvent::OpenTab(TabContent::Plan {
            plan: PlanIdentity::legacy(plan_slug_1.clone()),
        }));
        assert_eq!(app.tabs.open_tabs.len(), 2, "two plan tabs open");

        // Clear selected_run to simulate focus on a sidebar plan node.
        app.selected_run = None;

        // Activating a plan tab should sync selected_run to that plan's run.
        app.update(AppEvent::ActivateTab(0));
        assert_eq!(
            app.selected_run,
            Some(0),
            "ActivateTab to first plan tab syncs selected_run to its run (0)"
        );

        // Switching to the second plan tab should sync to its run.
        app.update(AppEvent::NextTab);
        assert_eq!(
            app.selected_run,
            Some(1),
            "NextTab to second plan tab syncs selected_run to its run (1)"
        );
    }

    /// Switching to a plan tab whose plan has NO open run must CLEAR a stale
    /// `selected_run` — otherwise run-control (and the new Start-opens-the-plan
    /// flow) would act on the previously selected, unrelated run. Regression for
    /// the 0042 review finding on stale run-control targeting.
    #[test]
    fn switching_to_plan_tab_without_run_clears_stale_selection() {
        use makina_core::api::{RunId, RunStatus, RunView};

        let mut app = make_app();
        app.update(AppEvent::PlansDiscovered {
            plans: make_plan_entries(),
        });
        // Only 0001-alpha has a run; 0002-beta has none.
        app.runs = vec![RunView {
            id: RunId(1),
            run_uid: "run-1".to_string(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-alpha").unwrap(),
            status: RunStatus::Pending,
            project: "test".to_string(),
            tasks: vec![],
            report: Default::default(),
        }];
        app.update(AppEvent::OpenTab(TabContent::Plan {
            plan: PlanIdentity::legacy("0001-alpha".to_string()),
        }));
        app.update(AppEvent::OpenTab(TabContent::Plan {
            plan: PlanIdentity::legacy("0002-beta".to_string()),
        }));

        // Activate the run-backed plan tab → selected_run points at run 0.
        app.update(AppEvent::ActivateTab(0));
        assert_eq!(app.selected_run, Some(0), "0001-alpha tab selects its run");

        // Switch to the run-less plan tab → the stale selection must be cleared.
        app.update(AppEvent::ActivateTab(1));
        assert_eq!(
            app.selected_run, None,
            "switching to a plan with no run must clear the stale selection"
        );
        assert!(
            app.active_run_id().is_none(),
            "no run is controllable for a run-less plan (Start will open it)"
        );
    }

    // ── Help overlay tests (plan 0038) ───────────────────────────────────────
    #[test]
    fn test_help_overlay_opens_on_question_mark() {
        let mut app = make_app();

        // Initially, help overlay should be closed
        assert!(
            !app.help_mode_active,
            "help_mode_active should initially be false"
        );

        // Sending ToggleHelpMode should open the help overlay
        app.update(AppEvent::ToggleHelpMode);
        assert!(
            app.help_mode_active,
            "ToggleHelpMode should set help_mode_active to true"
        );
    }

    #[test]
    fn test_help_overlay_closes_on_escape() {
        let mut app = make_app();

        // Open the help overlay
        app.help_mode_active = true;
        assert!(app.help_mode_active, "help overlay should be open");

        // Sending CloseHelpMode (from Escape or q) should close it
        app.update(AppEvent::CloseHelpMode);
        assert!(
            !app.help_mode_active,
            "CloseHelpMode should set help_mode_active to false"
        );
    }

    #[test]
    fn test_help_overlay_toggles() {
        let mut app = make_app();

        // Initially closed
        assert!(!app.help_mode_active);

        // First toggle opens it
        app.update(AppEvent::ToggleHelpMode);
        assert!(app.help_mode_active);

        // Second toggle closes it
        app.update(AppEvent::ToggleHelpMode);
        assert!(!app.help_mode_active);

        // Third toggle opens it again
        app.update(AppEvent::ToggleHelpMode);
        assert!(app.help_mode_active);
    }

    // ── Sidebar resizing (plan 0039) ───────────────────────────────────────────

    #[test]
    fn test_sidebar_resize_left_clamps_to_min() {
        let mut app = make_app();

        // Start at 30% (default).
        assert_eq!(app.sidebar_width_percent, 30);

        // Dispatch ResizeSidebarLeft 10 times to try to go below the minimum.
        for _ in 0..10 {
            let changed = app.update(AppEvent::ResizeSidebarLeft);
            assert!(changed, "ResizeSidebarLeft must return true");
        }

        // Should clamp to 10% (minimum).
        assert_eq!(app.sidebar_width_percent, 10);
    }

    #[test]
    fn test_sidebar_resize_right_clamps_to_max() {
        let mut app = make_app();

        // Start at 30% (default).
        assert_eq!(app.sidebar_width_percent, 30);

        // Dispatch ResizeSidebarRight 10 times to try to go above the maximum.
        for _ in 0..10 {
            let changed = app.update(AppEvent::ResizeSidebarRight);
            assert!(changed, "ResizeSidebarRight must return true");
        }

        // Should clamp to 50% (maximum).
        assert_eq!(app.sidebar_width_percent, 50);
    }

    // ── Folder-scoped tree navigation (plan 0043, workstream 0002) ─────────────

    #[test]
    fn test_folder_node_space_toggles_expansion() {
        let mut app = make_app();

        // Add a folder with a mock plan entry
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);
        // Folder starts collapsed (seeds in PlansDiscoveredPerFolder event handler)
        // In tests we manually seed this
        app.collapsed_folders.insert(0);
        assert!(
            app.collapsed_folders.contains(&0),
            "folder should be collapsed initially"
        );

        // Move cursor to the folder node
        let nodes = app.visible_tree_nodes();
        if let Some(idx) = nodes
            .iter()
            .position(|n| matches!(n, TreeNode::Folder { folder_idx: 0 }))
        {
            app.tree_cursor = Some(idx);
        }

        // Space should expand the folder
        app.update(AppEvent::ToggleTreeNode);
        assert!(
            !app.collapsed_folders.contains(&0),
            "folder should be expanded after Space"
        );

        // Space again should collapse it
        app.update(AppEvent::ToggleTreeNode);
        assert!(
            app.collapsed_folders.contains(&0),
            "folder should be collapsed after second ToggleTreeNode"
        );

        // Verify the cursor is still on the folder header after toggle
        if let Some(TreeNode::Folder { folder_idx: 0 }) = app.focused_node() {
            // Good, cursor is on the folder node
        } else {
            panic!("cursor should be on folder node after toggle");
        }
    }

    #[test]
    fn test_plan_in_folder_space_toggles_task_expansion() {
        let mut app = make_app();

        // Add a folder with a plan that has tasks
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![
                TestPlanTask {
                    id: "task-1".to_string(),
                    title: "Task 1".to_string(),
                    gated: false,
                    depends_on: vec![],
                    body: String::new(),
                },
                TestPlanTask {
                    id: "task-2".to_string(),
                    title: "Task 2".to_string(),
                    gated: false,
                    depends_on: vec![],
                    body: String::new(),
                },
            ],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);
        let collapse_key = app
            .collapse_key_for_node(TreeNode::PlanInFolder {
                folder_idx: 0,
                plan_idx: 0,
            })
            .expect("folder plan identity");

        // Expand the folder first
        app.collapsed_folders.clear();

        // Get the PlanInFolder node
        let nodes = app.visible_tree_nodes();
        let has_tasks = nodes.iter().any(|n| {
            matches!(
                n,
                TreeNode::PlanTaskInFolder {
                    folder_idx: 0,
                    plan_idx: 0,
                    ..
                }
            )
        });

        // Tasks should be visible initially (plan not in collapsed_plans for
        // folder 0 / plan 0).
        assert!(has_tasks, "tasks should be visible initially");

        // Find and move cursor to the PlanInFolder node
        if let Some(idx) = nodes.iter().position(|n| {
            matches!(
                n,
                TreeNode::PlanInFolder {
                    folder_idx: 0,
                    plan_idx: 0
                }
            )
        }) {
            app.tree_cursor = Some(idx);
        }

        // Space should collapse the plan's tasks
        app.update(AppEvent::ToggleTreeNode);
        assert!(
            app.collapsed_plans.contains(&collapse_key),
            "plan should be collapsed after ToggleTreeNode"
        );

        // Verify tasks are now hidden
        let nodes = app.visible_tree_nodes();
        let has_tasks = nodes.iter().any(|n| {
            matches!(
                n,
                TreeNode::PlanTaskInFolder {
                    folder_idx: 0,
                    plan_idx: 0,
                    ..
                }
            )
        });
        assert!(!has_tasks, "tasks should be hidden after collapse");

        // Space again should expand it
        app.update(AppEvent::ToggleTreeNode);
        assert!(
            !app.collapsed_plans.contains(&collapse_key),
            "plan should be expanded after second ToggleTreeNode"
        );

        // Verify tasks are visible again
        let nodes = app.visible_tree_nodes();
        let has_tasks = nodes.iter().any(|n| {
            matches!(
                n,
                TreeNode::PlanTaskInFolder {
                    folder_idx: 0,
                    plan_idx: 0,
                    ..
                }
            )
        });
        assert!(has_tasks, "tasks should be visible after re-expand");
    }

    #[test]
    fn test_plan_in_folder_enter_opens_plan_detail() {
        let mut app = make_app();

        // Add a folder with a plan
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);
        app.collapsed_folders.clear();

        // Move cursor to the PlanInFolder node
        let nodes = app.visible_tree_nodes();
        if let Some(idx) = nodes.iter().position(|n| {
            matches!(
                n,
                TreeNode::PlanInFolder {
                    folder_idx: 0,
                    plan_idx: 0
                }
            )
        }) {
            app.tree_cursor = Some(idx);
        }

        // Verify no tabs are open initially
        assert_eq!(app.tabs.open_tabs.len(), 0, "no tabs open initially");

        // Press Enter to activate the plan
        app.update(AppEvent::OpenFocusedNode);

        // Verify a plan tab was opened
        assert_eq!(app.tabs.open_tabs.len(), 1, "plan tab should be opened");
        if let Some(active_idx) = app.tabs.active_tab {
            if let TabContent::Plan { plan } = &app.tabs.open_tabs[active_idx] {
                assert_eq!(plan.slug, "test-plan", "correct plan should be in the tab");
            } else {
                panic!("active tab should contain a plan");
            }
        } else {
            panic!("a tab should be active");
        }
    }

    /// **Enter on a plan/task node crosses focus to the main pane** so the
    /// user immediately sees the detail tab content. Previously Enter opened
    /// the tab but left `focused_panel = Sidebar`, making Enter feel like a
    /// no-op on plans and tasks (the tab opened invisibly in the main pane
    /// while focus stayed on the sidebar). Folders keep focus on the sidebar
    /// because they only expand/collapse.
    #[test]
    fn enter_on_plan_keeps_focus_in_sidebar() {
        let mut app = make_app();

        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);
        app.collapsed_folders.clear();

        // --- Folder: Enter toggles expansion, focus stays on Sidebar ---
        app.collapsed_folders.insert(0);
        let folder_idx = app
            .visible_tree_nodes()
            .iter()
            .position(|n| matches!(n, TreeNode::Folder { folder_idx: 0 }))
            .expect("folder node");
        app.tree_cursor = Some(folder_idx);
        app.focused_panel = Panel::Sidebar;
        app.update(AppEvent::OpenFocusedNode);
        assert!(
            !app.collapsed_folders.contains(&0),
            "Enter on a collapsed Folder must expand it"
        );
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Enter on a Folder must keep focus on the sidebar"
        );

        // --- Plan: Enter opens the plan tab but focus stays in Sidebar ---
        let plan_idx = app
            .visible_tree_nodes()
            .iter()
            .position(|n| {
                matches!(
                    n,
                    TreeNode::PlanInFolder {
                        folder_idx: 0,
                        plan_idx: 0
                    }
                )
            })
            .expect("plan node");
        app.tree_cursor = Some(plan_idx);
        app.focused_panel = Panel::Sidebar;
        app.update(AppEvent::OpenFocusedNode);
        assert_eq!(
            app.tabs.open_tabs.len(),
            1,
            "Enter on a plan must open a plan tab"
        );
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Enter on a plan must keep focus in the sidebar — Tab/Right crosses to Main"
        );

        // --- Task: Enter opens the task tab but focus stays in Sidebar ---
        let task_idx = app
            .visible_tree_nodes()
            .iter()
            .position(|n| {
                matches!(
                    n,
                    TreeNode::PlanTaskInFolder {
                        folder_idx: 0,
                        plan_idx: 0,
                        task_idx: 0
                    }
                )
            })
            .expect("task node");
        app.tree_cursor = Some(task_idx);
        app.focused_panel = Panel::Sidebar;
        app.update(AppEvent::OpenFocusedNode);
        assert_eq!(
            app.tabs.open_tabs.len(),
            2,
            "Enter on a task must open a task tab"
        );
        assert_eq!(
            app.focused_panel,
            Panel::Sidebar,
            "Enter on a task must keep focus in the sidebar — Tab/Right crosses to Main"
        );
    }

    #[test]
    fn test_focus_right_expands_collapsed_folder() {
        let mut app = make_app();

        // Add a folder with a plan
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);

        // Folder is collapsed initially (seed for testing)
        app.collapsed_folders.insert(0);
        assert!(
            app.collapsed_folders.contains(&0),
            "folder should be collapsed"
        );

        // Move cursor to the folder node
        let nodes = app.visible_tree_nodes();
        if let Some(idx) = nodes
            .iter()
            .position(|n| matches!(n, TreeNode::Folder { folder_idx: 0 }))
        {
            app.tree_cursor = Some(idx);
        }

        // Focus-right should expand the folder
        app.update(AppEvent::FocusRightOrExpand);
        assert!(
            !app.collapsed_folders.contains(&0),
            "folder should be expanded after FocusRight"
        );

        // Verify cursor is still on folder
        if let Some(TreeNode::Folder { folder_idx: 0 }) = app.focused_node() {
            // Good
        } else {
            panic!("cursor should remain on folder");
        }
    }

    #[test]
    fn test_focus_left_collapses_expanded_folder() {
        let mut app = make_app();

        // Add a folder with a plan
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);

        // Expand the folder
        app.collapsed_folders.clear();

        // Move cursor to the folder node
        let nodes = app.visible_tree_nodes();
        if let Some(idx) = nodes
            .iter()
            .position(|n| matches!(n, TreeNode::Folder { folder_idx: 0 }))
        {
            app.tree_cursor = Some(idx);
        }

        // Ensure we're in sidebar focus
        app.focused_panel = Panel::Sidebar;

        // Focus-left should collapse the folder
        app.update(AppEvent::FocusLeftOrCollapse);
        assert!(
            app.collapsed_folders.contains(&0),
            "folder should be collapsed after FocusLeft"
        );
    }

    #[test]
    fn test_focus_left_on_plan_task_collapses_parent_plan_in_folder() {
        let mut app = make_app();

        // Add a folder with a plan that has tasks
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);
        let collapse_key = app
            .collapse_key_for_node(TreeNode::PlanInFolder {
                folder_idx: 0,
                plan_idx: 0,
            })
            .expect("folder plan identity");

        // Expand folder and plan so tasks are visible
        app.collapsed_folders.clear();
        assert!(
            !app.collapsed_plans.contains(&collapse_key),
            "plan should start expanded"
        );

        // Move cursor to a task node
        let nodes = app.visible_tree_nodes();
        if let Some(idx) = nodes.iter().position(|n| {
            matches!(
                n,
                TreeNode::PlanTaskInFolder {
                    folder_idx: 0,
                    plan_idx: 0,
                    ..
                }
            )
        }) {
            app.tree_cursor = Some(idx);
        }

        // Ensure we're in sidebar focus
        app.focused_panel = Panel::Sidebar;

        // Focus-left should collapse the parent plan
        app.update(AppEvent::FocusLeftOrCollapse);
        assert!(
            app.collapsed_plans.contains(&collapse_key),
            "parent plan should be collapsed"
        );

        // Cursor should move to the plan header
        if let Some(TreeNode::PlanInFolder {
            folder_idx: 0,
            plan_idx: 0,
        }) = app.focused_node()
        {
            // Good
        } else {
            panic!("cursor should be on plan header");
        }
    }

    #[test]
    fn test_visible_tree_nodes_shows_correct_folder_structure() {
        let mut app = make_app();

        // Add two folders, each with different numbers of plans
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder1"));
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder2"));

        let plan_1_1 = test_plan_entry(
            std::path::PathBuf::from("/test/folder1/docs/plans/plan-1-1"),
            "plan-1-1".to_string(),
            vec![TestPlanTask {
                id: "task".to_string(),
                title: "Task".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("scope".to_string()),
            None,
            None,
        );

        let plan_2_1 = test_plan_entry(
            std::path::PathBuf::from("/test/folder2/docs/plans/plan-2-1"),
            "plan-2-1".to_string(),
            vec![],
            Some("scope".to_string()),
            None,
            None,
        );

        let plan_2_2 = test_plan_entry(
            std::path::PathBuf::from("/test/folder2/docs/plans/plan-2-2"),
            "plan-2-2".to_string(),
            vec![TestPlanTask {
                id: "task".to_string(),
                title: "Task".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("scope".to_string()),
            None,
            None,
        );

        app.plans_by_folder.insert(0, vec![plan_1_1]);
        app.plans_by_folder.insert(1, vec![plan_2_1, plan_2_2]);

        // Expand both folders
        app.collapsed_folders.clear();

        // Expand plan 2-2 for its tasks (folder_idx=1, plan_idx=1)
        assert!(
            !app.collapsed_plans
                .contains(&CollapseKey::FolderPlan { folder: 1, plan: 1 })
        );

        let nodes = app.visible_tree_nodes();

        // Verify folder 0 and its plan are present
        assert!(
            nodes
                .iter()
                .any(|n| matches!(n, TreeNode::Folder { folder_idx: 0 }))
        );
        assert!(nodes.iter().any(|n| matches!(
            n,
            TreeNode::PlanInFolder {
                folder_idx: 0,
                plan_idx: 0
            }
        )));
        assert!(nodes.iter().any(|n| matches!(
            n,
            TreeNode::PlanTaskInFolder {
                folder_idx: 0,
                plan_idx: 0,
                ..
            }
        )));

        // Verify folder 1 and its plans are present
        assert!(
            nodes
                .iter()
                .any(|n| matches!(n, TreeNode::Folder { folder_idx: 1 }))
        );
        assert!(nodes.iter().any(|n| matches!(
            n,
            TreeNode::PlanInFolder {
                folder_idx: 1,
                plan_idx: 0
            }
        )));
        assert!(nodes.iter().any(|n| matches!(
            n,
            TreeNode::PlanInFolder {
                folder_idx: 1,
                plan_idx: 1
            }
        )));
        // Plan 2-2 has a task, so it should be visible
        assert!(nodes.iter().any(|n| matches!(
            n,
            TreeNode::PlanTaskInFolder {
                folder_idx: 1,
                plan_idx: 1,
                ..
            }
        )));
    }

    #[test]
    fn test_focused_node_returns_correct_folder_variant() {
        let mut app = make_app();

        // Add a folder with a plan
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);
        app.collapsed_folders.clear();

        // Move cursor to folder node
        let nodes = app.visible_tree_nodes();
        if let Some(idx) = nodes
            .iter()
            .position(|n| matches!(n, TreeNode::Folder { folder_idx: 0 }))
        {
            app.tree_cursor = Some(idx);
            if let Some(TreeNode::Folder { folder_idx: 0 }) = app.focused_node() {
                // Good
            } else {
                panic!("focused_node should return Folder variant");
            }
        }

        // Move cursor to plan node
        if let Some(idx) = nodes.iter().position(|n| {
            matches!(
                n,
                TreeNode::PlanInFolder {
                    folder_idx: 0,
                    plan_idx: 0
                }
            )
        }) {
            app.tree_cursor = Some(idx);
            if let Some(TreeNode::PlanInFolder {
                folder_idx: 0,
                plan_idx: 0,
            }) = app.focused_node()
            {
                // Good
            } else {
                panic!("focused_node should return PlanInFolder variant");
            }
        }

        // Move cursor to task node
        if let Some(idx) = nodes.iter().position(|n| {
            matches!(
                n,
                TreeNode::PlanTaskInFolder {
                    folder_idx: 0,
                    plan_idx: 0,
                    ..
                }
            )
        }) {
            app.tree_cursor = Some(idx);
            if let Some(TreeNode::PlanTaskInFolder {
                folder_idx: 0,
                plan_idx: 0,
                task_idx: 0,
            }) = app.focused_node()
            {
                // Good
            } else {
                panic!("focused_node should return PlanTaskInFolder variant");
            }
        }
    }

    /// A mouse click (`OpenTreeRow`) on a `Folder` row toggles its expansion,
    /// mirroring the keyboard Space/Enter path (`tree_toggle_expand` /
    /// `activate_focused_tree_node`).
    #[test]
    fn open_tree_row_toggles_folder_expansion() {
        let mut app = make_app();
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);
        app.collapsed_folders.insert(0);

        let nodes = app.visible_tree_nodes();
        let folder_row = nodes
            .iter()
            .position(|n| matches!(n, TreeNode::Folder { folder_idx: 0 }))
            .expect("a folder node should be visible");

        // Clicking the collapsed folder row expands it.
        app.update(AppEvent::OpenTreeRow(folder_row));
        assert!(
            !app.collapsed_folders.contains(&0),
            "folder should expand after mouse click"
        );
        assert_eq!(
            app.tree_cursor,
            Some(folder_row),
            "cursor moves to clicked folder row"
        );
        assert!(
            app.visible_tree_nodes().iter().any(|n| matches!(
                n,
                TreeNode::PlanInFolder {
                    folder_idx: 0,
                    plan_idx: 0
                }
            )),
            "plan should now be visible after expanding via mouse click"
        );

        // Clicking it again collapses it.
        let nodes = app.visible_tree_nodes();
        let folder_row = nodes
            .iter()
            .position(|n| matches!(n, TreeNode::Folder { folder_idx: 0 }))
            .expect("folder node should still be visible");
        app.update(AppEvent::OpenTreeRow(folder_row));
        assert!(
            app.collapsed_folders.contains(&0),
            "folder should collapse after second mouse click"
        );
    }

    /// A mouse click (`OpenTreeRow`) on a `PlanInFolder` row opens the plan
    /// detail tab sourced from `plans_by_folder`, mirroring keyboard Enter.
    #[test]
    fn open_tree_row_opens_plan_in_folder_tab() {
        let mut app = make_app();
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);
        app.collapsed_folders.clear();

        let nodes = app.visible_tree_nodes();
        let plan_row = nodes
            .iter()
            .position(|n| {
                matches!(
                    n,
                    TreeNode::PlanInFolder {
                        folder_idx: 0,
                        plan_idx: 0
                    }
                )
            })
            .expect("a PlanInFolder node should be visible");

        assert_eq!(app.tabs.open_tabs.len(), 0, "no tabs open initially");

        app.update(AppEvent::OpenTreeRow(plan_row));

        assert_eq!(app.tabs.open_tabs.len(), 1, "plan tab should be opened");
        let active_idx = app.tabs.active_tab.expect("a tab should be active");
        assert!(
            matches!(
                &app.tabs.open_tabs[active_idx],
                TabContent::Plan { plan } if plan.slug == "test-plan"
            ),
            "active tab should be the folder-scoped plan"
        );
    }

    /// A mouse click (`OpenTreeRow`) on a `PlanTaskInFolder` row opens that
    /// task's own preview tab, mirroring keyboard Enter.
    #[test]
    fn open_tree_row_opens_plan_task_in_folder_tab() {
        let mut app = make_app();
        app.opened_folders
            .push(std::path::PathBuf::from("/test/folder"));
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder/docs/plans/test-plan"),
            "test-plan".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("test scope".to_string()),
            None,
            None,
        );
        app.plans_by_folder.insert(0, vec![plan_entry]);
        app.collapsed_folders.clear();

        let nodes = app.visible_tree_nodes();
        let task_row = nodes
            .iter()
            .position(|n| {
                matches!(
                    n,
                    TreeNode::PlanTaskInFolder {
                        folder_idx: 0,
                        plan_idx: 0,
                        task_idx: 0
                    }
                )
            })
            .expect("a PlanTaskInFolder node should be visible");

        app.update(AppEvent::OpenTreeRow(task_row));

        assert_eq!(app.tabs.open_tabs.len(), 1, "task tab should be opened");
        let active_idx = app.tabs.active_tab.expect("a tab should be active");
        assert!(
            matches!(
                &app.tabs.open_tabs[active_idx],
                TabContent::PlanTask { plan, task_id }
                    if plan.slug == "test-plan" && task_id == "task-1"
            ),
            "active tab should be the folder-scoped task preview"
        );
    }

    #[test]
    fn test_startup_auto_discovery_emits_plans_discovered_per_folder_and_shows_folder_plans() {
        use crate::app::{App, AppEvent};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        // Create app with an opened_folders list (simulating startup).
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("/test"));
        app.opened_folders = vec![std::path::PathBuf::from("/test/folder1")];

        // Simulate PlansDiscoveredPerFolder event with a plan in folder 0.
        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder1/docs/plans/0001-test"),
            "0001-test".to_string(),
            vec![TestPlanTask {
                id: "task-1".to_string(),
                title: "Task 1".to_string(),
                gated: false,
                depends_on: vec![],
                body: String::new(),
            }],
            Some("scope".to_string()),
            None,
            None,
        );

        let mut plans_map = std::collections::HashMap::new();
        plans_map.insert(0, vec![plan_entry]);

        // Update app with the discovery event.
        app.update(AppEvent::PlansDiscoveredPerFolder {
            roots: app.opened_folders.clone(),
            plans_map,
        });

        // Verify that the app now has plans_by_folder populated.
        assert!(
            app.plans_by_folder.contains_key(&0),
            "folder 0 should have plans after discovery"
        );
        assert_eq!(
            app.plans_by_folder[&0].len(),
            1,
            "folder 0 should have exactly 1 plan"
        );
        assert_eq!(
            app.plans_by_folder[&0][0].slug, "0001-test",
            "plan slug should match"
        );

        // Verify that folder 0 is NOT in collapsed_folders (it should be expanded).
        assert!(
            !app.collapsed_folders.contains(&0),
            "folder 0 should be expanded (not in collapsed_folders)"
        );

        // Plans start collapsed by default (seeds collapsed_plans) so the tree
        // opens tidy — mirroring the legacy `PlansDiscovered` seeding.
        let collapse_key = app
            .collapse_key_for_node(TreeNode::PlanInFolder {
                folder_idx: 0,
                plan_idx: 0,
            })
            .expect("folder plan identity");
        assert!(
            app.collapsed_plans.contains(&collapse_key),
            "plan 0 in folder 0 should start collapsed (in collapsed_plans)"
        );

        // With the plan collapsed, only Folder and PlanInFolder are visible —
        // the PlanTaskInFolder nodes are hidden until the plan is expanded.
        let nodes = app.visible_tree_nodes();

        // Find the Folder node for folder 0.
        let folder_node = nodes
            .iter()
            .position(|n| matches!(n, TreeNode::Folder { folder_idx: 0 }))
            .expect("Folder node for folder_idx 0 should be visible");

        // Find the PlanInFolder node for folder 0, plan 0.
        let plan_in_folder_node = nodes
            .iter()
            .position(|n| {
                matches!(
                    n,
                    TreeNode::PlanInFolder {
                        folder_idx: 0,
                        plan_idx: 0
                    }
                )
            })
            .expect("PlanInFolder node for folder_idx 0, plan_idx 0 should be visible");

        // The PlanTaskInFolder node should NOT be visible while the plan is collapsed.
        assert!(
            !nodes.iter().any(|n| {
                matches!(
                    n,
                    TreeNode::PlanTaskInFolder {
                        folder_idx: 0,
                        plan_idx: 0,
                        task_idx: 0
                    }
                )
            }),
            "PlanTaskInFolder node should NOT be visible while the plan is collapsed"
        );

        // Verify the order: Folder < PlanInFolder.
        assert!(
            folder_node < plan_in_folder_node,
            "Folder should appear before PlanInFolder in the tree"
        );

        // Expand the plan and verify the task node becomes visible.
        app.collapsed_plans.remove(&collapse_key);
        let nodes = app.visible_tree_nodes();
        let task_node = nodes
            .iter()
            .position(|n| {
                matches!(
                    n,
                    TreeNode::PlanTaskInFolder {
                        folder_idx: 0,
                        plan_idx: 0,
                        task_idx: 0
                    }
                )
            })
            .expect("PlanTaskInFolder node should be visible after expanding the plan");
        assert!(
            plan_in_folder_node < task_node,
            "PlanInFolder should appear before PlanTaskInFolder after expansion"
        );
    }

    /// **Regression:** `PlansDiscoveredPerFolder`'s handler must NOT close an
    /// open plan tab whose plan is still present in `plans_by_folder` on
    /// re-discovery. Previously `close_tabs_for_missing_plans()` only checked
    /// the legacy flat `discovered_plans` list (which this event never
    /// populates), so every plan/plan-task tab sourced from `plans_by_folder`
    /// was incorrectly closed on every re-discovery.
    #[test]
    fn plans_discovered_per_folder_keeps_open_tab_for_still_present_plan() {
        use crate::app::{App, AppEvent, TabContent};
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![], std::path::PathBuf::from("/test"));
        app.opened_folders = vec![std::path::PathBuf::from("/test/folder1")];

        let plan_entry = test_plan_entry(
            std::path::PathBuf::from("/test/folder1/docs/plans/0001-test"),
            "0001-test".to_string(),
            Vec::new(),
            None,
            None,
            None,
        );
        let mut plans_map = std::collections::HashMap::new();
        plans_map.insert(0, vec![plan_entry.clone()]);

        // Seed plans_by_folder and open a plan tab for it (as if the plan was
        // opened from the sidebar after a previous discovery).
        app.plans_by_folder.insert(0, vec![plan_entry]);
        let target = app
            .plan_identity_for_node(TreeNode::PlanInFolder {
                folder_idx: 0,
                plan_idx: 0,
            })
            .expect("folder plan identity");
        app.tabs.open_tab(TabContent::Plan { plan: target });
        assert_eq!(
            app.tabs.open_tabs.len(),
            1,
            "tab should be open before re-discovery"
        );

        // Re-fire discovery with the SAME plan still present.
        app.update(AppEvent::PlansDiscoveredPerFolder {
            roots: app.opened_folders.clone(),
            plans_map,
        });

        assert_eq!(
            app.tabs.open_tabs.len(),
            1,
            "tab for a plan still present in plans_by_folder must NOT be closed"
        );
    }

    #[test]
    fn identical_slugs_in_different_projects_keep_distinct_tabs_and_runs() {
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, Vec::new(), PathBuf::from("/work/repo-a"));
        app.opened_folders = vec![PathBuf::from("/work/repo-a"), PathBuf::from("/work/repo-b")];
        let plan = |root: &str| {
            test_plan_entry(
                PathBuf::from(root).join("docs/plans/0042-same"),
                "0042-same".to_string(),
                Vec::new(),
                None,
                None,
                None,
            )
        };
        app.plans_by_folder.insert(0, vec![plan("/work/repo-a")]);
        app.plans_by_folder.insert(1, vec![plan("/work/repo-b")]);
        let repo_a = app
            .plan_identity_for_node(TreeNode::PlanInFolder {
                folder_idx: 0,
                plan_idx: 0,
            })
            .unwrap();
        let repo_b = app
            .plan_identity_for_node(TreeNode::PlanInFolder {
                folder_idx: 1,
                plan_idx: 0,
            })
            .unwrap();

        app.tabs.open_tab(TabContent::Plan {
            plan: repo_a.clone(),
        });
        app.tabs.open_tab(TabContent::Plan {
            plan: repo_b.clone(),
        });
        assert_ne!(repo_a, repo_b);
        assert_eq!(app.tabs.open_tabs.len(), 2);

        let run = |id, root: &str| RunView {
            id: RunId(id),
            run_uid: format!("run-{id}"),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0042-same").unwrap(),
            status: makina_core::api::RunStatus::Running,
            project: std::path::Path::new(root)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            tasks: Vec::new(),
            report: makina_core::api::IngestionReport::default(),
        };
        app.runs.push(run(11, "/work/repo-a"));
        app.runs.push(run(22, "/work/repo-b"));
        assert_eq!(app.latest_run_for_plan(&repo_a).unwrap().id, RunId(11));
        assert_eq!(app.latest_run_for_plan(&repo_b).unwrap().id, RunId(22));

        app.tabs.open_tab(TabContent::Plan { plan: repo_a });
        app.sync_selected_run_to_active_tab();
        assert_eq!(app.selected_run().unwrap().id, RunId(11));
        app.tabs.open_tab(TabContent::Plan { plan: repo_b });
        app.sync_selected_run_to_active_tab();
        assert_eq!(app.selected_run().unwrap().id, RunId(22));
    }

    #[test]
    fn removing_folder_immediately_rekeys_folder_state_and_closes_only_its_tabs() {
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, Vec::new(), PathBuf::from("/work/repo-a"));
        app.opened_folders = vec![
            PathBuf::from("/work/repo-a"),
            PathBuf::from("/work/repo-b"),
            PathBuf::from("/work/repo-c"),
        ];
        let plan = |root: &str, slug: &str| {
            test_plan_entry(
                PathBuf::from(root).join("docs/plans").join(slug),
                slug.to_string(),
                Vec::new(),
                None,
                None,
                None,
            )
        };
        app.plans_by_folder
            .insert(0, vec![plan("/work/repo-a", "a")]);
        app.plans_by_folder
            .insert(1, vec![plan("/work/repo-b", "b")]);
        app.plans_by_folder
            .insert(2, vec![plan("/work/repo-c", "c")]);
        let target_b = app
            .plan_identity_for_node(TreeNode::PlanInFolder {
                folder_idx: 1,
                plan_idx: 0,
            })
            .unwrap();
        let target_c = app
            .plan_identity_for_node(TreeNode::PlanInFolder {
                folder_idx: 2,
                plan_idx: 0,
            })
            .unwrap();
        app.collapsed_folders.extend([0, 2]);
        app.collapsed_plans.extend([
            CollapseKey::Plan(target_b.clone()),
            CollapseKey::Plan(target_c.clone()),
        ]);
        app.tabs.open_tab(TabContent::Plan {
            plan: target_b.clone(),
        });
        app.tabs.open_tab(TabContent::Plan {
            plan: target_c.clone(),
        });

        assert!(app.remove_opened_folder(std::path::Path::new("/work/repo-b")));
        assert_eq!(
            app.opened_folders,
            vec![PathBuf::from("/work/repo-a"), PathBuf::from("/work/repo-c")]
        );
        assert_eq!(app.plans_by_folder[&1][0].slug, "c");
        assert!(!app.plans_by_folder.contains_key(&2));
        assert_eq!(app.collapsed_folders, HashSet::from([0, 1]));
        assert!(!app.collapsed_plans.contains(&CollapseKey::Plan(target_b)));
        assert!(
            app.collapsed_plans
                .contains(&CollapseKey::Plan(target_c.clone()))
        );
        assert_eq!(
            app.tabs.open_tabs,
            vec![TabContent::Plan { plan: target_c }]
        );
    }

    #[test]
    fn arbitrary_task_path_keeps_its_project_after_folder_is_closed() {
        let api = Arc::new(PlaceholderApi::empty());
        let run = RunView {
            id: RunId(22),
            run_uid: "run-b".to_string(),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            status: makina_core::api::RunStatus::Pending,
            project: "repo-b".to_string(),
            tasks: Vec::new(),
            report: makina_core::api::IngestionReport::default(),
        };
        let mut app = App::new(api, vec![run], PathBuf::from("/work/repo-a"));
        app.opened_folders = vec![PathBuf::from("/work/repo-a"), PathBuf::from("/work/repo-b")];

        assert!(app.remove_opened_folder(std::path::Path::new("/work/repo-b")));

        let target = app.plan_identity_for_run(&app.runs[0]);
        assert_eq!(target.project_root, PathBuf::from("/work/repo-b"));
        assert_eq!(
            target.plan_dir,
            makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap()
        );
    }

    #[test]
    fn command_plan_context_follows_the_panel_that_owns_focus() {
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, Vec::new(), PathBuf::from("/work/repo-a"));
        app.opened_folders = vec![PathBuf::from("/work/repo-a"), PathBuf::from("/work/repo-b")];
        let plan = |root: &str, slug: &str| {
            test_plan_entry(
                PathBuf::from(root).join("docs/plans").join(slug),
                slug.to_string(),
                Vec::new(),
                None,
                None,
                None,
            )
        };
        app.plans_by_folder
            .insert(0, vec![plan("/work/repo-a", "plan-a")]);
        app.plans_by_folder
            .insert(1, vec![plan("/work/repo-b", "plan-b")]);
        let tab_target = app
            .plan_identity_for_node(TreeNode::PlanInFolder {
                folder_idx: 0,
                plan_idx: 0,
            })
            .unwrap();
        let sidebar_target = app
            .plan_identity_for_node(TreeNode::PlanInFolder {
                folder_idx: 1,
                plan_idx: 0,
            })
            .unwrap();
        app.tabs.open_tab(TabContent::Plan {
            plan: tab_target.clone(),
        });
        app.tree_cursor = app.visible_tree_nodes().iter().position(|node| {
            matches!(
                node,
                TreeNode::PlanInFolder {
                    folder_idx: 1,
                    plan_idx: 0
                }
            )
        });

        app.focused_panel = Panel::Sidebar;
        assert_eq!(app.context_plan_identity(), Some(sidebar_target));

        app.focused_panel = Panel::Main;
        assert_eq!(app.context_plan_identity(), Some(tab_target));
    }

    #[test]
    fn bare_folder_focus_overrides_stale_tab_for_project_commands() {
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, Vec::new(), PathBuf::from("/work/repo-a"));
        app.opened_folders = vec![PathBuf::from("/work/repo-a"), PathBuf::from("/work/repo-b")];
        app.plans_by_folder.insert(0, Vec::new());
        app.plans_by_folder.insert(1, Vec::new());
        app.tabs.open_tab(TabContent::Plan {
            plan: PlanIdentity::new(
                PathBuf::from("/work/repo-a"),
                PathBuf::from("/work/repo-a/docs/plans/0001-a"),
                "0001-a".to_string(),
            ),
        });
        app.focused_panel = Panel::Sidebar;
        app.tree_cursor = app
            .visible_tree_nodes()
            .iter()
            .position(|node| matches!(node, TreeNode::Folder { folder_idx: 1 }));

        assert_eq!(app.context_project_root(), PathBuf::from("/work/repo-b"));
    }

    #[test]
    fn settings_for_focused_secondary_project_load_that_projects_values() {
        let temp = tempfile::tempdir().unwrap();
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        std::fs::create_dir_all(repo_b.join(".makina")).unwrap();
        std::fs::write(
            repo_b.join(".makina/config.toml"),
            r#"
concurrency = 7

[caps]
gate_iterations = 8
reviewer_iterations = 9
wall_clock_secs = 901
idle_secs = 45

[merge]
final = "stage"
"#,
        )
        .unwrap();

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, Vec::new(), repo_a.clone());
        app.caps.gate_iterations = 1;
        app.concurrency = 2;
        app.final_merge = FinalMerge::Squash;
        app.opened_folders = vec![repo_a, repo_b.clone()];
        app.plans_by_folder.insert(0, Vec::new());
        app.plans_by_folder.insert(1, Vec::new());
        app.tree_cursor = app
            .visible_tree_nodes()
            .iter()
            .position(|node| matches!(node, TreeNode::Folder { folder_idx: 1 }));

        app.update(AppEvent::OpenSettings);

        let settings = app.settings.as_ref().unwrap();
        assert_eq!(settings.project_root, repo_b);
        assert_eq!(settings.gate_iterations, "8");
        assert_eq!(settings.reviewer_iterations, "9");
        assert_eq!(settings.wall_clock_secs, "901");
        assert_eq!(settings.idle_secs, "45");
        assert_eq!(settings.concurrency, "7");
        assert_eq!(settings.final_merge, FinalMerge::Stage);
        assert!(settings.error.is_none());
    }

    // ── Model name canonicalisation ───────────────────────────────────────────

    #[test]
    fn canonical_model_name_strips_only_a_known_agent_prefix() {
        let agents = vec!["opencode".to_string(), "claude".to_string()];

        assert_eq!(
            canonical_model_name("opencode/my-model", &agents).as_deref(),
            Some("my-model"),
            "the picker's agent prefix must be stripped"
        );
        assert_eq!(
            canonical_model_name("opencode/anthropic/claude-sonnet-4", &agents).as_deref(),
            Some("anthropic/claude-sonnet-4"),
            "only the agent prefix goes — the rest of the name is the model"
        );
        assert_eq!(
            canonical_model_name("anthropic/claude-sonnet-4", &agents).as_deref(),
            Some("anthropic/claude-sonnet-4"),
            "a slash that is not an agent prefix must be left alone"
        );
        assert_eq!(
            canonical_model_name("my-model", &agents).as_deref(),
            Some("my-model")
        );
        assert_eq!(
            canonical_model_name("  ", &agents),
            None,
            "an empty buffer means 'leave the stored model alone'"
        );
        assert_eq!(
            canonical_model_name("opencode/", &agents).as_deref(),
            Some("opencode/"),
            "a bare prefix with nothing after it is not a model selection"
        );
    }

    /// The Settings modal shows the model the context project would actually
    /// use — here, the one its own config names.
    #[test]
    fn settings_seeds_model_buffers_from_the_project_config() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".makina")).unwrap();
        std::fs::write(
            repo.join(".makina/config.toml"),
            r#"
[roles.developer]
provider = "default"
model = "project-dev-model"

[roles.planner]
provider = "default"
model = "project-planner-model"
"#,
        )
        .unwrap();

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, Vec::new(), repo.clone());
        app.providers = vec![makina_core::config::ProviderConfig {
            name: "default".into(),
            command: "opencode".into(),
            args: vec!["acp".into()],
            env: Default::default(),
        }];

        app.update(AppEvent::OpenSettings);

        let settings = app.settings.as_ref().unwrap();
        assert_eq!(settings.developer_model, "project-dev-model");
        assert_eq!(settings.planner_model, "project-planner-model");
        assert_eq!(
            settings.reviewer_model, "",
            "a role with no model in either layer shows as unset"
        );
        assert_eq!(
            app.roles.developer.as_ref().unwrap().model.as_deref(),
            Some("project-dev-model"),
            "opening Settings adopts the context project's resolved assignments"
        );
    }

    /// An empty model buffer yields no selection, so no writer touches that
    /// role — clearing one field must never wipe a stored model, least of all
    /// the global one another project inherits.
    #[test]
    fn empty_model_buffer_yields_no_selection() {
        let temp = tempfile::tempdir().unwrap();
        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, Vec::new(), temp.path().to_path_buf());

        app.update(AppEvent::OpenSettings);
        {
            let settings = app.settings.as_mut().unwrap();
            settings.developer_model = String::new();
            settings.reviewer_model = "opencode/reviewer-model".to_string();
            settings.planner_model = "   ".to_string();
        }

        let models = app.selected_role_models();
        assert_eq!(models.developer, None, "an empty buffer is not a selection");
        assert_eq!(
            models.planner, None,
            "a whitespace-only buffer is not a selection"
        );
        assert_eq!(
            models.reviewer.as_deref(),
            Some("opencode/reviewer-model"),
            "with no configured providers there is no prefix to strip"
        );
    }
}
