//! `Developer` actor — spoke that executes a single task in a worktree.
//!
//! # Role
//!
//! The `Developer` is a spoke in the star topology.  It holds a reference to the
//! domain [`Supervisor`] hub and communicates only with it.  Its job is to receive
//! a task assignment from the Supervisor, work on it inside a git worktree (by
//! driving the injected [`AgentBackend`]), and hand the result back to the
//! Supervisor.
//!
//! # Star topology
//!
//! The `Developer` holds an `ActorRef<Supervisor>` in its state — the **only**
//! actor ref it is allowed to hold.  Spoke-to-spoke communication is forbidden;
//! any cross-spoke coordination routes through the Supervisor.
//!
//! **Note on stale refs after hub restart**: `ActorRef<Supervisor>` is `Clone +
//! Send + Sync`, so it is a valid `Args` field and survives spoke-level restarts.
//! However, if the domain `Supervisor` itself is restarted by the
//! `RootSupervisor`, all spokes will hold a stale ref.  Resolving this is
//! deferred to a later fault-tolerance task.
//!
//! # Backend injection
//!
//! The agent backend is injected as `Arc<dyn AgentBackend>` via [`DeveloperArgs`]
//! (mirroring the Planner's interpreter injection).  Tests inject
//! [`NoopBackend`](crate::backend::noop::NoopBackend); production injects the ACP
//! backend.  The Developer never knows which concrete backend it is driving.
//!
//! # The develop turn (task 21)
//!
//! On [`Develop`] the actor:
//! 1. Builds a [`SessionConfig`] via [`session_config_for(Role::Developer, …)`].
//! 2. Spawns a session on the backend with the task's worktree as the working dir.
//! 3. Sends a single prompt describing the task (title/description/`done_when`,
//!    plus any reviewer feedback on a retry).
//! 4. Drains the [`ResponseStream`], concatenating the agent's text output.
//! 5. Terminates the session and hands the collected output back to the
//!    Supervisor (as the reply).
//!
//! [`session_config_for(Role::Developer, …)`]: crate::roles::session_config_for
//! [`SessionConfig`]: crate::backend::SessionConfig
//! [`ResponseStream`]: crate::backend::ResponseStream

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use kameo::actor::ActorRef;

use crate::backend::{AgentBackend, Prompt, ResponseEvent};
use crate::roles::{Role, session_config_for};
use crate::task::Task;

use super::supervisor::Supervisor;

// ── Actor ─────────────────────────────────────────────────────────────────────

/// Spoke actor that implements a single task inside a git worktree.
///
/// Spawnable as a supervised child of [`crate::supervision::RootSupervisor`].
/// The `Supervisor` ref and the [`AgentBackend`] are passed via
/// [`DeveloperArgs`].
///
/// The system may spawn multiple Developer instances in parallel (one per
/// concurrent task slot); each holds the same Supervisor ref and a clone of the
/// shared backend `Arc`.  Concurrency management is handled by a later task
/// (task 24 — concurrency).
pub struct Developer {
    /// Reference to the domain Supervisor hub.
    ///
    /// Retained so the Developer can push progress/error events to the hub in a
    /// future event-driven design.  The current sequential loop (task 21) drives
    /// the Developer via `ask` and reads the reply, so this ref is presently only
    /// the star-topology anchor.
    #[allow(dead_code)]
    supervisor: ActorRef<Supervisor>,

    /// The injected agent backend used to spawn developer sessions.
    ///
    /// `Arc<dyn AgentBackend>` is shared with the Reviewer and the Supervisor's
    /// wiring; all sessions spawned from the same backend share its state (e.g.
    /// the `NoopBackend` recorder).
    backend: Arc<dyn AgentBackend>,
}

/// Construction arguments for [`Developer`].
///
/// Both fields are `Clone + Sync`:
/// - `ActorRef<Supervisor>` is `Clone + Send + Sync` by design.
/// - `Arc<dyn AgentBackend>` is `Clone` (reference-counted) and `Sync` because
///   the trait bound includes `Send + Sync`.
///
/// This satisfies the `C::Args: Clone + Sync` bound required by
/// [`crate::supervision::RootSupervisor::spawn_child`].
#[derive(Clone)]
pub struct DeveloperArgs {
    /// The domain Supervisor hub this Developer will report to.
    pub supervisor: ActorRef<Supervisor>,

    /// The agent backend the Developer drives to produce code changes.
    ///
    /// Inject [`NoopBackend`](crate::backend::noop::NoopBackend) in tests; inject
    /// the ACP backend in production.
    pub backend: Arc<dyn AgentBackend>,
}

impl kameo::actor::Actor for Developer {
    type Args = DeveloperArgs;
    type Error = std::convert::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Developer {
            supervisor: args.supervisor,
            backend: args.backend,
        })
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Instruct the Developer to work on `task` inside the given `worktree`.
///
/// `feedback` carries the Reviewer's rejection feedback on a retry attempt, or
/// `None` on the first attempt.  When present, the feedback is appended to the
/// prompt so the agent can address the requested changes.
pub struct Develop {
    /// The task to implement.
    pub task: Task,
    /// Path to the git worktree where work should be done.
    pub worktree: PathBuf,
    /// Reviewer feedback to address on a retry; `None` on the first attempt.
    pub feedback: Option<String>,
}

/// Successful outcome of a [`Develop`] turn.
///
/// Carries the agent's collected text output.  Task 22 (gate-runner) will extend
/// this with gate results and iteration counts; task 23 (squash-merge) relies on
/// the branch carrying the committed work (with the `NoopBackend` there are no
/// file changes, so no commit is made here — see the handler's seam comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevelopOutcome {
    /// The concatenated text the agent produced for this turn.
    pub output: String,
}

/// Reply returned by the [`Develop`] handler.
///
/// `Ok(DevelopOutcome)` on success; `Err(String)` if the backend session failed
/// (spawn/prompt/transport error).  The Supervisor treats `Err` as a hard error
/// for the task.
pub type DevelopAck = Result<DevelopOutcome, String>;

impl kameo::message::Message<Develop> for Developer {
    type Reply = DevelopAck;

    async fn handle(
        &mut self,
        msg: Develop,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 1. Build a developer session config rooted at the task's worktree.
        let config = session_config_for(Role::Developer, msg.worktree.clone());

        // 2. Spawn a session on the injected backend.
        let mut session = self
            .backend
            .spawn(config)
            .await
            .map_err(|e| format!("developer backend spawn failed: {e}"))?;

        // 3. Build the prompt describing the task (and any reviewer feedback).
        let prompt_text = build_develop_prompt(&msg.task, msg.feedback.as_deref());

        let stream = match session.prompt(Prompt::new(prompt_text)).await {
            Ok(stream) => stream,
            Err(e) => {
                // Best-effort cleanup before surfacing the error.
                let _ = session.terminate().await;
                return Err(format!("developer prompt failed: {e}"));
            }
        };

        // 4. Drain the response stream, concatenating TextChunk text until
        //    TurnComplete (or surfacing a transport error).
        let mut output = String::new();
        let mut events = stream;
        while let Some(item) = events.next().await {
            match item {
                Ok(ResponseEvent::TextChunk { text }) => output.push_str(&text),
                Ok(ResponseEvent::TurnComplete) => break,
                Err(e) => {
                    drop(events);
                    let _ = session.terminate().await;
                    return Err(format!("developer stream error: {e}"));
                }
            }
        }
        drop(events);

        // 5. Terminate the session (idempotent).
        let _ = session.terminate().await;

        // ── Seam: commit the agent's changes to the task branch ───────────────
        //
        // The real flow commits the worktree's changes to `task/{id}` here so
        // that task 23 (squash-merge) has the work to merge into `develop`.  With
        // the `NoopBackend` there are NO file changes, so committing is a no-op
        // and is intentionally skipped — keeping this task focused on the loop.
        // When a real backend lands, add a `git -C {worktree} commit -am …` step
        // (or fold it into the gate-runner of task 22) before handing back.

        Ok(DevelopOutcome { output })
    }
}

// ── Prompt construction ─────────────────────────────────────────────────────────

/// Build the user prompt for a develop turn.
///
/// Includes the task's title, description, and `done_when` acceptance criterion.
/// On a retry, the Reviewer's `feedback` is appended so the agent addresses the
/// requested changes (this is how the Supervisor "relays feedback on reject").
fn build_develop_prompt(task: &Task, feedback: Option<&str>) -> String {
    let mut prompt = format!(
        "Implement the following task in the current working directory.\n\n\
         Task ID: {id}\n\
         Title: {title}\n\
         Description: {description}\n\
         Done when: {done_when}\n",
        id = task.id,
        title = task.title,
        description = task.description,
        done_when = task.done_when,
    );

    if let Some(feedback) = feedback {
        prompt.push_str(&format!(
            "\nThis is a revision. A reviewer rejected the previous attempt with \
             the following feedback — address it specifically:\n{feedback}\n"
        ));
    }

    prompt
}
