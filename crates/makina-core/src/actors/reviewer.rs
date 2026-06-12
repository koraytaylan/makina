//! `Reviewer` actor — spoke that evaluates a Developer's output.
//!
//! # Role
//!
//! The `Reviewer` is a spoke in the star topology.  It holds a reference to the
//! domain [`Supervisor`] hub and communicates only with it.  Its job is to
//! receive a review assignment from the Supervisor, evaluate the Developer's
//! changes (by driving the injected [`AgentBackend`]), parse the agent's
//! structured verdict, and return a [`ReviewVerdict`] to the Supervisor.
//!
//! # Star topology
//!
//! The `Reviewer` holds an `ActorRef<Supervisor>` in its state — the **only**
//! actor ref it is allowed to hold.  Reviewer-to-Developer communication is
//! forbidden; all coordination routes through the Supervisor.
//!
//! **Note on stale refs after hub restart**: `ActorRef<Supervisor>` is `Clone +
//! Send + Sync`, so it is a valid `Args` field and survives spoke-level restarts.
//! However, if the domain `Supervisor` itself is restarted by the
//! `RootSupervisor`, all spokes will hold a stale ref.  Resolving this is
//! deferred to a later fault-tolerance task.
//!
//! # Backend injection
//!
//! The agent backend is injected as `Arc<dyn AgentBackend>` via [`ReviewerArgs`]
//! (mirroring the Developer).  Tests inject
//! [`NoopBackend`](crate::backend::noop::NoopBackend) configured to return a
//! verdict JSON; production injects the ACP backend.
//!
//! # The review turn (task 21)
//!
//! On [`Review`] the actor:
//! 1. Builds a [`SessionConfig`] via [`session_config_for(Role::Reviewer, …)`].
//! 2. Spawns a session on the backend with the task's worktree as the working dir.
//! 3. Sends a prompt asking for a review of the task's work (per the
//!    [`REVIEWER_SYSTEM_PROMPT`](crate::roles::REVIEWER_SYSTEM_PROMPT) contract).
//! 4. Drains the [`ResponseStream`], then [`parse_review_verdict`]s the output.
//! 5. Terminates the session and returns the [`ReviewVerdict`] to the Supervisor.
//!
//! [`session_config_for(Role::Reviewer, …)`]: crate::roles::session_config_for
//! [`SessionConfig`]: crate::backend::SessionConfig
//! [`ResponseStream`]: crate::backend::ResponseStream
//! [`parse_review_verdict`]: crate::roles::parse_review_verdict

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kameo::actor::ActorRef;

use crate::api;
use crate::backend::{AgentBackend, Prompt, ResponseEvent};
use crate::config::RoleAssignment;
use crate::roles::{Role, parse_review_verdict, session_config_for};
use crate::task::Task;

use super::supervisor::{EventSink, Supervisor};

// ── Typed error for the Review reply ─────────────────────────────────────────

/// Typed failure returned by the [`Review`] message handler.
///
/// Having a typed enum (rather than a bare `String`) lets the Supervisor
/// classify the failure without fragile substring matching.
#[derive(Debug)]
pub enum ReviewerError {
    /// The idle watchdog fired: no agent output for the configured duration.
    IdleTimeout {
        /// The idle timeout in seconds that was exceeded.
        idle_secs: u64,
    },
    /// Any other error (backend spawn, transport, verdict parse failure, etc.).
    Other(String),
}

impl std::fmt::Display for ReviewerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewerError::IdleTimeout { idle_secs } => {
                write!(f, "no agent output for {idle_secs}s")
            }
            ReviewerError::Other(msg) => f.write_str(msg),
        }
    }
}

// ── ReviewVerdict re-export ───────────────────────────────────────────────────

/// Re-exported from [`crate::roles`] where the type now lives.
///
/// Kept here so that existing callers (`actors::reviewer::ReviewVerdict`) continue
/// to compile without change.
pub use crate::roles::ReviewVerdict;

// ── Actor ─────────────────────────────────────────────────────────────────────

/// Spoke actor that evaluates a Developer's output and returns a
/// [`ReviewVerdict`].
///
/// Spawnable as a supervised child of [`crate::supervision::RootSupervisor`].
/// The `Supervisor` ref and the [`AgentBackend`] are passed via
/// [`ReviewerArgs`].
pub struct Reviewer {
    /// Reference to the domain Supervisor hub.
    ///
    /// Retained as the star-topology anchor.  The current sequential loop
    /// (task 21) drives the Reviewer via `ask` and reads the verdict reply, so
    /// this ref is presently unused for outbound messages.
    #[allow(dead_code)]
    supervisor: ActorRef<Supervisor>,

    /// The injected agent backend used to spawn reviewer sessions.
    ///
    /// Shared (`Arc`) with the Developer and the Supervisor's wiring.
    backend: Arc<dyn AgentBackend>,

    /// The role assignment (provider, mode, model, effort) for the Reviewer.
    /// Carried to `session_config_for` so the ACP backend applies the selections.
    assignment: Option<RoleAssignment>,
}

/// Construction arguments for [`Reviewer`].
///
/// Both fields are `Clone + Sync` (see [`DeveloperArgs`](super::developer::DeveloperArgs)
/// for the reasoning), satisfying the `C::Args: Clone + Sync` bound required by
/// [`crate::supervision::RootSupervisor::spawn_child`].
#[derive(Clone)]
pub struct ReviewerArgs {
    /// The domain Supervisor hub this Reviewer will report to.
    pub supervisor: ActorRef<Supervisor>,

    /// The agent backend the Reviewer drives to produce a verdict.
    pub backend: Arc<dyn AgentBackend>,

    /// The role assignment (provider, mode, model, effort) for the Reviewer.
    ///
    /// When `Some`, the defaults from the assignment (mode/model/effort) are
    /// threaded into [`SessionConfig`] so the ACP backend can apply them after
    /// `session/new`. When `None`, no selections are applied.
    pub assignment: Option<RoleAssignment>,
}

impl kameo::actor::Actor for Reviewer {
    type Args = ReviewerArgs;
    type Error = std::convert::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Reviewer {
            supervisor: args.supervisor,
            backend: args.backend,
            assignment: args.assignment,
        })
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Instruct the Reviewer to evaluate the Developer's output for `task` inside
/// the given `worktree`.
///
/// `run` + `sink` (task 31) mirror [`Develop`](super::developer::Develop): the
/// handler publishes the live `AgentExchange` stream for the **Reviewer** role
/// (`PromptSent` / `ResponseChunk` / `TurnComplete`) so the TUI shows the
/// review turn alongside the develop turn.
pub struct Review {
    /// The task being reviewed.
    pub task: Task,
    /// Path to the git worktree containing the Developer's output.
    pub worktree: PathBuf,
    /// The Run this exchange belongs to (for `AgentExchange` events).
    pub run: api::RunId,
    /// Live-event sink: where `AgentExchange` events are published.
    pub sink: EventSink,
    /// Idle timeout in seconds; `None` means no idle watchdog.
    pub idle_secs: Option<u64>,
}

/// Reply returned by the [`Review`] handler.
///
/// `Ok(ReviewVerdict)` carries the parsed verdict (approve / reject+feedback);
/// `Err(ReviewerError)` signals a handler-level failure (backend spawn,
/// transport, verdict parse failure, or idle timeout).  The Supervisor inspects
/// the typed error to decide the [`crate::api::FailureKind`] without string
/// matching.
///
/// This alias mirrors [`DevelopAck`](super::developer::DevelopAck) and
/// [`InterpretTaskListAck`](super::planner::InterpretTaskListAck) for consistency
/// across the spokes (addressing the actor-traits review note).
pub type ReviewReply = Result<ReviewVerdict, ReviewerError>;

impl kameo::message::Message<Review> for Reviewer {
    type Reply = ReviewReply;

    async fn handle(
        &mut self,
        msg: Review,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 1. Build a reviewer session config rooted at the task's worktree,
        //    carrying the role assignment (mode/model/effort) from `self.assignment`.
        let config = session_config_for(
            Role::Reviewer,
            msg.worktree.clone(),
            self.assignment.clone(),
        );

        // 2. Spawn a session on the injected backend.
        let mut session = self
            .backend
            .spawn(config)
            .await
            .map_err(|e| ReviewerError::Other(format!("reviewer backend spawn failed: {e}")))?;

        let task_id = api::TaskId(msg.task.id.0.clone());

        // Surface discovered capabilities to the TUI (step 5 of task 0040).
        if let Some(capabilities) = session.capabilities() {
            (msg.sink)(api::Event::SessionCapabilities {
                run: msg.run,
                task: task_id.clone(),
                role: api::AgentRole::Reviewer,
                capabilities,
            });
        }

        // 3. Prompt for a structured review verdict.
        let prompt_text = build_review_prompt(&msg.task);

        // Publish the outgoing prompt as the Reviewer "user turn" (task 31).
        (msg.sink)(api::Event::AgentExchange {
            run: msg.run,
            task: task_id.clone(),
            role: api::AgentRole::Reviewer,
            event: api::ExchangeEvent::PromptSent {
                text: prompt_text.clone(),
            },
        });

        let stream = match session.prompt(Prompt::new(prompt_text)).await {
            Ok(stream) => stream,
            Err(e) => {
                let _ = session.terminate().await;
                return Err(ReviewerError::Other(format!("reviewer prompt failed: {e}")));
            }
        };

        // 4. Drain the response stream into the raw verdict text, publishing each
        //    chunk as a live `ResponseChunk` and the turn end as `TurnComplete`.
        //    When `idle_secs` is configured, wrap each next() await with a timeout
        //    so the watchdog fires on prolonged silence.
        let mut output = String::new();
        let mut events = stream;
        loop {
            let item = match msg.idle_secs {
                Some(idle) => {
                    let timeout_duration = Duration::from_secs(idle);
                    match tokio::time::timeout(timeout_duration, events.next()).await {
                        Ok(item) => item,
                        Err(_elapsed) => {
                            // Idle timeout fired: no output for idle_secs.
                            drop(events);
                            let _ = session.terminate().await;
                            (msg.sink)(api::Event::TaskIdle {
                                run: msg.run,
                                task: task_id.clone(),
                                idle_secs: idle,
                            });
                            return Err(ReviewerError::IdleTimeout { idle_secs: idle });
                        }
                    }
                }
                None => events.next().await,
            };

            match item {
                Some(Ok(ResponseEvent::TextChunk { text })) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Reviewer,
                        event: api::ExchangeEvent::ResponseChunk { text: text.clone() },
                    });
                    output.push_str(&text);
                }
                // Thought and tool events are side-channel only: they are
                // forwarded to the live `AgentExchange` stream for observability
                // but MUST NOT contribute to `output` (the verdict text is built
                // solely from `TextChunk`/`ResponseChunk`).
                Some(Ok(ResponseEvent::ThoughtChunk { text })) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Reviewer,
                        event: api::ExchangeEvent::ThoughtChunk { text },
                    });
                }
                Some(Ok(ResponseEvent::ToolCall {
                    id,
                    title,
                    kind,
                    status,
                })) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Reviewer,
                        event: api::ExchangeEvent::ToolCall {
                            id,
                            title,
                            kind,
                            status,
                        },
                    });
                }
                Some(Ok(ResponseEvent::ToolCallUpdate { id, status, title })) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Reviewer,
                        event: api::ExchangeEvent::ToolCallUpdate { id, status, title },
                    });
                }
                Some(Ok(ResponseEvent::CurrentModeUpdate { current_mode_id })) => {
                    (msg.sink)(api::Event::CurrentModeUpdate {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Reviewer,
                        current_mode_id,
                    });
                }
                Some(Ok(ResponseEvent::TurnComplete)) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Reviewer,
                        event: api::ExchangeEvent::TurnComplete,
                    });
                    break;
                }
                Some(Err(e)) => {
                    drop(events);
                    let _ = session.terminate().await;
                    return Err(ReviewerError::Other(format!("reviewer stream error: {e}")));
                }
                None => {
                    drop(events);
                    let _ = session.terminate().await;
                    return Err(ReviewerError::Other(
                        "reviewer stream ended unexpectedly".to_string(),
                    ));
                }
            }
        }
        drop(events);

        // 5. Terminate the session (idempotent).
        let _ = session.terminate().await;

        // 6. Parse the agent's output into a structured verdict.
        parse_review_verdict(&output)
            .map_err(|e| ReviewerError::Other(format!("failed to parse review verdict: {e}")))
    }
}

// ── Prompt construction ─────────────────────────────────────────────────────────

/// Build the user prompt for a review turn.
///
/// Restates the task's intent and `done_when` criterion, and reminds the agent of
/// the strict JSON-verdict output contract (also enforced by
/// [`REVIEWER_SYSTEM_PROMPT`](crate::roles::REVIEWER_SYSTEM_PROMPT)).
fn build_review_prompt(task: &Task) -> String {
    format!(
        "Review the work done in the current working directory for the following \
         task. Decide whether it satisfies the acceptance criterion.\n\n\
         Task ID: {id}\n\
         Title: {title}\n\
         Description: {description}\n\
         Done when: {done_when}\n\n\
         Respond with ONLY the JSON verdict object, e.g. {{\"verdict\":\"approve\"}} \
         or {{\"verdict\":\"reject\",\"feedback\":\"<what must change>\"}}.",
        id = task.id,
        title = task.title,
        description = task.description,
        done_when = task.done_when,
    )
}
