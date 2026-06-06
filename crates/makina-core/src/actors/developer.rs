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
//! 5. Terminates the session.
//! 6. Commits the worktree's changes to `task/{id}` (`git add -A` + `git commit
//!    --allow-empty`) so task 23's squash-merge has the work to land on
//!    `develop`.  Hands the collected output back to the Supervisor (as the
//!    reply).
//!
//! [`session_config_for(Role::Developer, …)`]: crate::roles::session_config_for
//! [`SessionConfig`]: crate::backend::SessionConfig
//! [`ResponseStream`]: crate::backend::ResponseStream

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use kameo::actor::ActorRef;

use crate::api;
use crate::backend::{AgentBackend, Prompt, ResponseEvent};
use crate::roles::{Role, session_config_for};
use crate::task::Task;

use super::supervisor::{EventSink, Supervisor};

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
///
/// `run` + `sink` (task 31) let the handler publish the live
/// [`api::Event::AgentExchange`] stream the TUI consumes — `PromptSent` when the
/// prompt is dispatched, one `ResponseChunk` per streamed `TextChunk`, and
/// `TurnComplete` at the turn's end.  Both default to a no-op/`RunId(0)` for the
/// task-21–25 ask paths via the helper constructors; the scheduler threads the
/// real values from its [`RunControl`].
pub struct Develop {
    /// The task to implement.
    pub task: Task,
    /// Path to the git worktree where work should be done.
    pub worktree: PathBuf,
    /// Reviewer feedback to address on a retry; `None` on the first attempt.
    pub feedback: Option<String>,
    /// The Run this exchange belongs to (for `AgentExchange` events).
    pub run: api::RunId,
    /// Live-event sink: where `AgentExchange` events are published.
    pub sink: EventSink,
}

/// Successful outcome of a [`Develop`] turn.
///
/// Carries the agent's collected text output.  Task 22 (gate-runner) will extend
/// this with gate results and iteration counts; task 23 (squash-merge) relies on
/// the branch carrying the committed work — so after the agent turn the handler
/// commits the worktree to `task/{id}` (`--allow-empty`, so the `NoopBackend`
/// no-change case still produces a commit; see the handler's commit step).
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

        // Publish the outgoing prompt as the Developer "user turn" (task 31).
        let task_id = api::TaskId(msg.task.id.0.clone());
        (msg.sink)(api::Event::AgentExchange {
            run: msg.run,
            task: task_id.clone(),
            role: api::AgentRole::Developer,
            event: api::ExchangeEvent::PromptSent {
                text: prompt_text.clone(),
            },
        });

        let stream = match session.prompt(Prompt::new(prompt_text)).await {
            Ok(stream) => stream,
            Err(e) => {
                // Best-effort cleanup before surfacing the error.
                let _ = session.terminate().await;
                return Err(format!("developer prompt failed: {e}"));
            }
        };

        // 4. Drain the response stream, concatenating TextChunk text until
        //    TurnComplete (or surfacing a transport error).  Each chunk is also
        //    published as a live `ResponseChunk`, and the turn end as
        //    `TurnComplete` (task 31).
        let mut output = String::new();
        let mut events = stream;
        while let Some(item) = events.next().await {
            match item {
                Ok(ResponseEvent::TextChunk { text }) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Developer,
                        event: api::ExchangeEvent::ResponseChunk { text: text.clone() },
                    });
                    output.push_str(&text);
                }
                // Thought and tool events are side-channel only: they are
                // forwarded to the live `AgentExchange` stream for observability
                // but MUST NOT contribute to `output` (the final answer text is
                // built solely from `TextChunk`/`ResponseChunk`).
                Ok(ResponseEvent::ThoughtChunk { text }) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Developer,
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
                        role: api::AgentRole::Developer,
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
                        role: api::AgentRole::Developer,
                        event: api::ExchangeEvent::ToolCallUpdate { id, status, title },
                    });
                }
                Ok(ResponseEvent::TurnComplete) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Developer,
                        event: api::ExchangeEvent::TurnComplete,
                    });
                    break;
                }
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

        // ── Commit the agent's changes to the task branch (task 23) ───────────
        //
        // The squash-merge (task 23) merges `task/{id}` into `develop`, so the
        // agent's work must be COMMITTED to the branch first.  We stage everything
        // and commit in the worktree:
        //
        //   git -C {worktree} add -A
        //   git -C {worktree} commit --allow-empty -m "..."
        //
        // `--allow-empty` is deliberate: with the `NoopBackend` there are NO file
        // changes, so without it `commit` would fail ("nothing to commit") and the
        // squash-merge would have nothing — and no commit — to land.  Allowing an
        // empty commit means a no-op task still produces a branch commit that the
        // squash-merge records on `develop` (uniform audit trail); real agent
        // edits are captured the same way (a non-empty commit).
        //
        // Placement choice: the commit lives in the Developer handler (right after
        // the agent turn) rather than in the Supervisor.  Rationale — committing
        // is intrinsically part of "the Developer produced work"; the Supervisor
        // then runs gates against the committed worktree and later squash-merges
        // the branch.  A failed commit is surfaced as a hard error for the task.
        //
        // NOTE (gate loop, task 22): the Supervisor re-dispatches this handler on a
        // gate failure or reviewer rejection.  Each re-dispatch commits again, so a
        // re-worked branch may carry MULTIPLE commits — which is fine: the squash
        // collapses them all into one commit on `develop`.
        if let Err(e) = commit_worktree(&msg.worktree, &msg.task).await {
            return Err(format!("developer commit failed: {e}"));
        }

        Ok(DevelopOutcome { output })
    }
}

// ── Worktree commit ───────────────────────────────────────────────────────────

/// Stage and commit the worktree's changes to the task branch.
///
/// Runs, in `worktree`:
/// - `git add -A` — stage all changes (new/modified/deleted files).
/// - `git commit --allow-empty -m "task({id}): {title}"` — record them as a
///   commit on the checked-out `task/{id}` branch.  `--allow-empty` ensures a
///   no-op (NoopBackend) task still produces a commit for the squash-merge to
///   land (see the handler's commit-step comment).
///
/// Returns `Err(String)` (with captured stderr) if either git command fails or
/// could not be launched; the handler maps that to a hard error for the task.
async fn commit_worktree(worktree: &std::path::Path, task: &Task) -> Result<(), String> {
    run_git_in(worktree, &["add", "-A"]).await?;

    let message = format!("task({id}): {title}", id = task.id, title = task.title);
    run_git_in(worktree, &["commit", "--allow-empty", "-m", &message]).await?;

    Ok(())
}

/// Run a `git -C {worktree} {args}` command, returning `Err(String)` (with
/// captured stderr) on a non-zero exit or a spawn failure.
async fn run_git_in(worktree: &std::path::Path, args: &[&str]) -> Result<(), String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to launch `git {}`: {e}", args.join(" ")))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`git {}` failed in {}: {}",
            args.join(" "),
            worktree.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
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
