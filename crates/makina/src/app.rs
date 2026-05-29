//! Application state and pure update logic.
//!
//! [`App`] is the single source of truth for all TUI state.  It holds no IO;
//! the IO loop in [`crate::event`] drives it by calling [`App::update`].
//!
//! Keeping `update` a synchronous, pure function means every state transition
//! is unit-testable without a real terminal or async runtime.

use std::sync::Arc;

use makina_core::api::{Api, Event, RunView};

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
    /// An event arrived from `api.subscribe()`.
    ApiEvent(Event),
    /// Periodic tick — triggers a redraw without other state changes.
    Tick,
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

    /// The list of open Runs, seeded from `api.runs()` at startup and
    /// incrementally updated from api events.
    pub runs: Vec<RunView>,

    /// Index into `runs` identifying the currently selected/focused Run.
    /// `None` when `runs` is empty.
    pub selected_run: Option<usize>,

    /// Last api event received — stored for test assertions and status-bar
    /// display.  Will be used by tasks 27–31 for richer updates.
    pub last_event: Option<Event>,
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
        Self {
            should_quit: false,
            api,
            focused_panel: Panel::Sidebar,
            runs: initial_runs,
            selected_run,
            last_event: None,
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
            AppEvent::ApiEvent(ev) => {
                self.apply_api_event(ev);
                true
            }
            AppEvent::Tick => {
                // Tick drives the redraw loop; no state changes needed here.
                true
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
                        task_list_path: task_list_path.clone(),
                        status: RunStatus::Pending,
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
            // AgentExchange events are displayed by task 30 (prompt-answer-stream).
            // The scaffold stores the last event for status-bar hints.
            Event::AgentExchange { .. } => {}
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
            task_list_path: PathBuf::from(".tasks/demo.json"),
            status: RunStatus::Pending,
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
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Pending,
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
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Running,
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

    #[test]
    fn api_event_iterations_updated() {
        use makina_core::api::{RunId, RunStatus, RunView, TaskId, TaskState, TaskView};
        let api = Arc::new(PlaceholderApi::new());
        let run = RunView {
            id: RunId(1),
            task_list_path: PathBuf::from(".tasks/x.json"),
            status: RunStatus::Running,
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
}
