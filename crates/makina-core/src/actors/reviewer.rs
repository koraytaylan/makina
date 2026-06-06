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

use futures::StreamExt;
use kameo::actor::ActorRef;

use crate::api;
use crate::backend::{AgentBackend, Prompt, ResponseEvent};
use crate::roles::{Role, parse_review_verdict, session_config_for};
use crate::task::Task;

use super::supervisor::{EventSink, Supervisor};

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
}

impl kameo::actor::Actor for Reviewer {
    type Args = ReviewerArgs;
    type Error = std::convert::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Reviewer {
            supervisor: args.supervisor,
            backend: args.backend,
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
}

/// Reply returned by the [`Review`] handler.
///
/// `Ok(ReviewVerdict)` carries the parsed verdict (approve / reject+feedback);
/// `Err(String)` signals a handler-level failure (backend spawn/prompt/transport
/// error, or a verdict that could not be parsed).  The `Result` wrapper also
/// satisfies kameo's `Reply` bound via the blanket `impl Reply for Result<T, E>`
/// (`ReviewVerdict` itself does not implement `Reply`).
///
/// This alias mirrors [`DevelopAck`](super::developer::DevelopAck) and
/// [`InterpretTaskListAck`](super::planner::InterpretTaskListAck) for consistency
/// across the spokes (addressing the actor-traits review note).
pub type ReviewReply = Result<ReviewVerdict, String>;

impl kameo::message::Message<Review> for Reviewer {
    type Reply = ReviewReply;

    async fn handle(
        &mut self,
        msg: Review,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 1. Build a reviewer session config rooted at the task's worktree.
        let config = session_config_for(Role::Reviewer, msg.worktree.clone());

        // 2. Spawn a session on the injected backend.
        let mut session = self
            .backend
            .spawn(config)
            .await
            .map_err(|e| format!("reviewer backend spawn failed: {e}"))?;

        // 3. Prompt for a structured review verdict.
        let prompt_text = build_review_prompt(&msg.task);

        // Publish the outgoing prompt as the Reviewer "user turn" (task 31).
        let task_id = api::TaskId(msg.task.id.0.clone());
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
                return Err(format!("reviewer prompt failed: {e}"));
            }
        };

        // 4. Drain the response stream into the raw verdict text, publishing each
        //    chunk as a live `ResponseChunk` and the turn end as `TurnComplete`.
        let mut output = String::new();
        let mut events = stream;
        while let Some(item) = events.next().await {
            match item {
                Ok(ResponseEvent::TextChunk { text }) => {
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
                Ok(ResponseEvent::ThoughtChunk { text }) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Reviewer,
                        event: api::ExchangeEvent::ThoughtChunk { text },
                    });
                }
                Ok(ResponseEvent::ToolCall {
                    id,
                    title,
                    kind,
                    status,
                }) => {
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
                Ok(ResponseEvent::ToolCallUpdate { id, status, title }) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Reviewer,
                        event: api::ExchangeEvent::ToolCallUpdate { id, status, title },
                    });
                }
                Ok(ResponseEvent::TurnComplete) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Reviewer,
                        event: api::ExchangeEvent::TurnComplete,
                    });
                    break;
                }
                Err(e) => {
                    drop(events);
                    let _ = session.terminate().await;
                    return Err(format!("reviewer stream error: {e}"));
                }
            }
        }
        drop(events);

        // 5. Terminate the session (idempotent).
        let _ = session.terminate().await;

        // 6. Parse the agent's output into a structured verdict.
        parse_review_verdict(&output).map_err(|e| format!("failed to parse review verdict: {e}"))
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
