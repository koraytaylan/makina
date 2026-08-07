//! Test-only `Api` double.
//!
//! # `#[cfg(test)]` — the binary uses the real [`makina_core::orchestrator::CoreApi`]
//!
//! As of task 28 (file-browser) the production binary wires the **real**
//! core-backed `Api` ([`makina_core::orchestrator::CoreApi`]) in `main.rs`.  This
//! module is therefore gated behind `#[cfg(test)]` and exists only as a
//! lightweight, controllable [`Api`] double for the TUI's unit/integration
//! tests (app-state, rendering, key-translation) that do **not** need a real
//! interpreter or filesystem reads:
//!
//! * `runs()` returns a single sample [`RunView`] so rendering tests have data.
//! * `subscribe()` emits a handful of sample events, then stays open on a live
//!   broadcast channel.
//! * `execute()` accepts all commands and returns plausible outcomes.
//!
//! It holds **no orchestration logic**; it is a test double, not a coordinator.
//! Tests that need the real `OpenPlan` → validate → register → broadcast path
//! (e.g. the TUI↔CoreApi flow test in [`crate::event`]) use `CoreApi` directly.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures::stream;
use makina_core::api::{
    AgentRole, Api, ApiError, Command, CommandOutcome, Event, EventStream, ExchangeEvent, RunId,
    RunStatus, RunView, TaskId, TaskState, TaskView,
};
use makina_core::plan::PlanKey;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

// ── PlaceholderApi ────────────────────────────────────────────────────────────

/// A minimal, test-only stand-in for the real core-backed [`Api`].
///
/// Used by the TUI's unit/integration tests.  The production binary uses
/// [`makina_core::orchestrator::CoreApi`]; see the module-level doc.
pub struct PlaceholderApi {
    runs: Mutex<Vec<RunView>>,
    next_id: AtomicU64,
    /// Broadcast channel — any subscriber gets all future events.
    /// Capacity 64 is more than enough for the sample bursts used here.
    event_tx: broadcast::Sender<Event>,
    /// Whether to emit static sample events from `subscribe()`.
    emit_samples: bool,
}

impl PlaceholderApi {
    /// Create a new [`PlaceholderApi`] pre-populated with one sample Run.
    ///
    /// The sample run and sample events give the TUI something to render when
    /// launched without a real orchestrator (interactive demo).
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(64);
        let sample_run = sample_run(RunId(1));
        Self {
            runs: Mutex::new(vec![sample_run]),
            next_id: AtomicU64::new(2),
            event_tx,
            emit_samples: true,
        }
    }

    /// Create a new [`PlaceholderApi`] with an empty run list and no static
    /// sample events.
    ///
    /// Useful for tests that need full control over the initial state and
    /// want to observe only the events they explicitly trigger via `execute()`.
    #[cfg(test)]
    pub fn empty() -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            runs: Mutex::new(vec![]),
            next_id: AtomicU64::new(1),
            event_tx,
            emit_samples: false,
        }
    }

    fn alloc_id(&self) -> RunId {
        RunId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for PlaceholderApi {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Api for PlaceholderApi {
    async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
        match command {
            Command::GeneratePlanBundle { .. } => Err(ApiError::InvalidCommand {
                reason: "plan generation requires a repository-bound API".into(),
            }),
            Command::RegisterPlan { .. } => Ok(CommandOutcome::Acknowledged),
            Command::CheckOutPlan { .. } => Err(ApiError::InvalidCommand {
                reason: "checking a plan out requires a repository-bound API".into(),
            }),
            Command::SetTaskDisposition { run, .. } => {
                if self
                    .runs
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|entry| entry.id == run)
                {
                    Ok(CommandOutcome::Acknowledged)
                } else {
                    Err(ApiError::UnknownRun { run })
                }
            }
            Command::FinalizePlan {
                run_uid,
                expected_plan_oid,
                ..
            }
            | Command::ReprepareFinalization {
                run_uid,
                expected_plan_oid,
                ..
            } => {
                if self
                    .runs
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|entry| entry.run_uid == run_uid)
                {
                    Ok(CommandOutcome::FinalizationAccepted {
                        plan_oid: expected_plan_oid,
                    })
                } else {
                    Err(ApiError::InvalidCommand {
                        reason: "unknown run_uid".into(),
                    })
                }
            }
            Command::OpenPlan { plan_dir } => {
                let id = self.alloc_id();
                let run = RunView {
                    id,
                    run_uid: String::new(),
                    plan_dir: plan_dir.clone(),
                    status: RunStatus::Pending,
                    project: String::new(),
                    tasks: vec![],
                    report: makina_core::api::IngestionReport::default(),
                };
                self.runs.lock().unwrap().push(run);
                // Broadcast a RunOpened event so any subscriber sees it.
                let _ = self.event_tx.send(Event::RunOpened { run: id, plan_dir });
                Ok(CommandOutcome::RunOpened { run: id })
            }
            Command::StartRun { run } => {
                let mut runs = self.runs.lock().unwrap();
                match runs.iter_mut().find(|r| r.id == run) {
                    Some(r) => {
                        r.status = RunStatus::Running;
                        let _ = self.event_tx.send(Event::RunStatusChanged {
                            run,
                            status: RunStatus::Running,
                        });
                        Ok(CommandOutcome::Acknowledged)
                    }
                    None => Err(ApiError::UnknownRun { run }),
                }
            }
            Command::PauseRun { run } => {
                let mut runs = self.runs.lock().unwrap();
                match runs.iter_mut().find(|r| r.id == run) {
                    Some(r) => {
                        r.status = RunStatus::Paused;
                        let _ = self.event_tx.send(Event::RunStatusChanged {
                            run,
                            status: RunStatus::Paused,
                        });
                        Ok(CommandOutcome::Acknowledged)
                    }
                    None => Err(ApiError::UnknownRun { run }),
                }
            }
            Command::CancelRun { run } => {
                let mut runs = self.runs.lock().unwrap();
                let before = runs.len();
                runs.retain(|r| r.id != run);
                if runs.len() < before {
                    Ok(CommandOutcome::Acknowledged)
                } else {
                    Err(ApiError::UnknownRun { run })
                }
            }
            Command::ReinterpretRun { run } => {
                let runs = self.runs.lock().unwrap();
                if runs.iter().any(|r| r.id == run) {
                    Ok(CommandOutcome::Acknowledged)
                } else {
                    Err(ApiError::UnknownRun { run })
                }
            }
            Command::ResetRun { run } => {
                let mut runs = self.runs.lock().unwrap();
                let Some(rv) = runs.iter_mut().find(|r| r.id == run) else {
                    return Err(ApiError::UnknownRun { run });
                };
                rv.status = RunStatus::Pending;
                for task in &mut rv.tasks {
                    task.state = TaskState::New;
                    task.gate_iterations = 0;
                    task.review_iterations = 0;
                    task.started_at = None;
                    task.finished_at = None;
                    task.failure_reason = None;
                }
                drop(runs);
                let _ = self.event_tx.send(Event::RunStatusChanged {
                    run,
                    status: RunStatus::Pending,
                });
                Ok(CommandOutcome::Acknowledged)
            }
            // Plan 0017: flip the targeted Failed task(s) back to Ready, mark the
            // run Running, and broadcast TaskRetried + the state changes — enough
            // for TUI tests to observe a retry without a real scheduler.
            Command::RetryTask { run, task } => {
                let mut runs = self.runs.lock().unwrap();
                let Some(rv) = runs.iter_mut().find(|r| r.id == run) else {
                    return Err(ApiError::UnknownRun { run });
                };
                let Some(tv) = rv.tasks.iter_mut().find(|t| t.id == task) else {
                    return Err(ApiError::InvalidCommand {
                        reason: format!("task {} is not in run {run}", task.0),
                    });
                };
                if tv.state != TaskState::Failed {
                    return Err(ApiError::InvalidCommand {
                        reason: format!(
                            "task {} is in state {:?}, not Failed; cannot retry",
                            task.0, tv.state
                        ),
                    });
                }
                tv.state = TaskState::Ready;
                tv.failure_reason = None;
                tv.gate_iterations = 0;
                tv.review_iterations = 0;
                rv.status = RunStatus::Running;
                drop(runs);
                let _ = self.event_tx.send(Event::TaskRetried {
                    run,
                    task: task.clone(),
                });
                let _ = self.event_tx.send(Event::TaskStateChanged {
                    run,
                    task,
                    state: TaskState::Ready,
                });
                let _ = self.event_tx.send(Event::RunStatusChanged {
                    run,
                    status: RunStatus::Running,
                });
                Ok(CommandOutcome::Acknowledged)
            }
            Command::RetryFailedTasks { run } => {
                let mut runs = self.runs.lock().unwrap();
                let Some(rv) = runs.iter_mut().find(|r| r.id == run) else {
                    return Err(ApiError::UnknownRun { run });
                };
                let retried: Vec<TaskId> = rv
                    .tasks
                    .iter()
                    .filter(|t| t.state == TaskState::Failed)
                    .map(|t| t.id.clone())
                    .collect();
                for tv in rv.tasks.iter_mut().filter(|t| t.state == TaskState::Failed) {
                    tv.state = TaskState::Ready;
                    tv.failure_reason = None;
                    tv.gate_iterations = 0;
                    tv.review_iterations = 0;
                }
                if !retried.is_empty() {
                    rv.status = RunStatus::Running;
                }
                drop(runs);
                for task in retried {
                    let _ = self.event_tx.send(Event::TaskRetried {
                        run,
                        task: task.clone(),
                    });
                    let _ = self.event_tx.send(Event::TaskStateChanged {
                        run,
                        task,
                        state: TaskState::Ready,
                    });
                }
                let _ = self.event_tx.send(Event::RunStatusChanged {
                    run,
                    status: RunStatus::Running,
                });
                Ok(CommandOutcome::Acknowledged)
            }
            // Plan 0025: force-re-run project discovery. In the placeholder, just
            // acknowledge; the real implementation is in the orchestrator.
            Command::RegisterProject { .. } => Ok(CommandOutcome::Acknowledged),
            Command::UnregisterProject { .. } => Ok(CommandOutcome::Acknowledged),
            Command::UpdateRuntimeSettings { .. } => Ok(CommandOutcome::Acknowledged),
            Command::DiscoverProject { .. } => Ok(CommandOutcome::Acknowledged),
            Command::PurgeWorktrees { .. } => Ok(CommandOutcome::Acknowledged),
        }
    }

    async fn runs(&self) -> Vec<RunView> {
        self.runs.lock().unwrap().clone()
    }

    async fn run(&self, id: RunId) -> Option<RunView> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    fn subscribe(&self) -> EventStream {
        let rx = self.event_tx.subscribe();
        let live = BroadcastStream::new(rx).filter_map(|r: Result<Event, _>| r.ok());
        if self.emit_samples {
            // Emit sample events immediately (as a static stream), then switch to
            // the live broadcast channel for any future events.
            let static_events = sample_events();
            Box::pin(stream::iter(static_events).chain(live))
        } else {
            // No static events — only live broadcast events.
            Box::pin(live)
        }
    }
}

// ── Sample data helpers ───────────────────────────────────────────────────────

fn sample_run(id: RunId) -> RunView {
    RunView {
        id,
        run_uid: String::new(),
        plan_dir: PlanKey::parse("docs/plans/0001-demo-feature").expect("valid sample plan"),
        status: RunStatus::Running,
        project: "makina".into(),
        tasks: vec![
            TaskView {
                authored: None,
                id: TaskId::new("core-api"),
                title: "Core API surface".into(),
                state: TaskState::Done,
                gate_iterations: 0,
                review_iterations: 1,
                depends_on: vec![],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            },
            TaskView {
                authored: None,
                id: TaskId::new("tui-scaffold"),
                title: "TUI scaffold".into(),
                state: TaskState::InProgress,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![TaskId::new("core-api")],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            },
            TaskView {
                authored: None,
                id: TaskId::new("runs-sidebar"),
                title: "Runs sidebar".into(),
                state: TaskState::Ready,
                gate_iterations: 0,
                review_iterations: 0,
                depends_on: vec![TaskId::new("tui-scaffold")],
                started_at: None,
                finished_at: None,
                failure_reason: None,
                entry_text: String::new(),
            },
        ],
        report: makina_core::api::IngestionReport::default(),
    }
}

/// A short sequence of sample events broadcast at subscribe time.
///
/// These demonstrate that the TUI reacts correctly to api events before the
/// real orchestrator is wired up.
fn sample_events() -> Vec<Event> {
    vec![
        Event::RunStatusChanged {
            run: RunId(1),
            status: RunStatus::Running,
        },
        Event::TaskStateChanged {
            run: RunId(1),
            task: TaskId::new("tui-scaffold"),
            state: TaskState::InProgress,
        },
        Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("tui-scaffold"),
            role: AgentRole::Developer,
            event: ExchangeEvent::PromptSent {
                text: "[placeholder] Implement the TUI scaffold.".into(),
            },
        },
        Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("tui-scaffold"),
            role: AgentRole::Developer,
            event: ExchangeEvent::ResponseChunk {
                text: "[placeholder] Building the ratatui skeleton…".into(),
            },
        },
        Event::AgentExchange {
            run: RunId(1),
            task: TaskId::new("tui-scaffold"),
            role: AgentRole::Developer,
            event: ExchangeEvent::TurnComplete,
        },
    ]
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::Arc;

    #[tokio::test]
    async fn placeholder_runs_returns_sample_run() {
        let api = PlaceholderApi::new();
        let runs = api.runs().await;
        assert_eq!(
            runs.len(),
            1,
            "PlaceholderApi::new should seed one sample run"
        );
        assert_eq!(runs[0].id, RunId(1));
        assert_eq!(runs[0].status, RunStatus::Running);
    }

    #[tokio::test]
    async fn placeholder_empty_has_no_runs() {
        let api = PlaceholderApi::empty();
        assert!(api.runs().await.is_empty());
    }

    #[tokio::test]
    async fn placeholder_subscribe_yields_sample_events() {
        let api = Arc::new(PlaceholderApi::new());
        let mut stream = api.subscribe();

        // Drain sample events (the static ones, not the live channel).
        let mut events = Vec::new();
        for _ in 0..5 {
            if let Some(ev) = stream.next().await {
                events.push(ev);
            }
        }
        assert_eq!(events.len(), 5, "should get the 5 sample events");
        assert!(matches!(events[0], Event::RunStatusChanged { .. }));
        assert!(matches!(events[1], Event::TaskStateChanged { .. }));
        assert!(matches!(
            events[2],
            Event::AgentExchange {
                event: ExchangeEvent::PromptSent { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn placeholder_execute_open_plan_adds_run() {
        let api = PlaceholderApi::empty();
        let outcome: Result<CommandOutcome, ApiError> = api
            .execute(Command::OpenPlan {
                plan_dir: PlanKey::parse("docs/plans/0001-test").expect("valid plan"),
            })
            .await;
        assert!(matches!(outcome.unwrap(), CommandOutcome::RunOpened { .. }));
        assert_eq!(api.runs().await.len(), 1);
    }

    #[tokio::test]
    async fn placeholder_execute_broadcasts_run_opened_event() {
        // Use empty() so there are no static sample events to skip.
        let api = Arc::new(PlaceholderApi::empty());
        let mut stream = api.subscribe();

        let _: Result<CommandOutcome, ApiError> = api
            .execute(Command::OpenPlan {
                plan_dir: PlanKey::parse("docs/plans/0001-test").expect("valid plan"),
            })
            .await;

        // The empty PlaceholderApi has no static sample events, so the first
        // event on the live broadcast channel must be the RunOpened we just
        // triggered via execute().
        let ev: Event = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .expect("timed out waiting for RunOpened event")
            .expect("stream ended unexpectedly");
        assert!(
            matches!(ev, Event::RunOpened { .. }),
            "expected RunOpened, got: {ev:?}"
        );
    }

    #[tokio::test]
    async fn placeholder_unknown_run_returns_error() {
        let api = PlaceholderApi::empty();
        let result = api.execute(Command::StartRun { run: RunId(99) }).await;
        assert!(matches!(result, Err(ApiError::UnknownRun { .. })));
    }
}
