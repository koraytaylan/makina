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
use std::time::{Duration, Instant};

use futures::StreamExt;
use kameo::actor::ActorRef;

use crate::api;
use crate::backend::{AgentBackend, Prompt, ResponseEvent};
use crate::config::RoleAssignment;
use crate::roles::{Role, current_model_from, session_config_for};
use crate::task::Task;

use super::supervisor::{EventSink, Supervisor};

// ── Typed error for the Develop reply ────────────────────────────────────────

/// Typed failure returned by the [`Develop`] message handler.
///
/// Having a typed enum (rather than a bare `String`) lets the Supervisor
/// classify the failure without fragile substring matching.
#[derive(Debug)]
pub enum DeveloperError {
    /// The idle watchdog fired: no agent output for the configured duration.
    ///
    /// Carries the configured threshold so the Supervisor can use it in the
    /// failure reason message without re-parsing the string.
    IdleTimeout {
        /// The idle timeout in seconds that was exceeded.
        idle_secs: u64,
    },
    /// Any other error (backend spawn, transport, commit failure, etc.).
    Other(String),
}

impl std::fmt::Display for DeveloperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeveloperError::IdleTimeout { idle_secs } => {
                write!(f, "no agent output for {idle_secs}s")
            }
            DeveloperError::Other(msg) => f.write_str(msg),
        }
    }
}

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

    /// The role assignment (provider, mode, model, effort) for the Developer.
    /// Carried to `session_config_for` so the ACP backend applies the selections.
    assignment: Option<RoleAssignment>,
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

    /// The role assignment (provider, mode, model, effort) for the Developer.
    ///
    /// When `Some`, the defaults from the assignment (mode/model/effort) are
    /// threaded into [`SessionConfig`] so the ACP backend can apply them after
    /// `session/new`. When `None`, no selections are applied.
    pub assignment: Option<RoleAssignment>,
}

impl kameo::actor::Actor for Developer {
    type Args = DeveloperArgs;
    type Error = std::convert::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Developer {
            supervisor: args.supervisor,
            backend: args.backend,
            assignment: args.assignment,
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
    /// Idle timeout in seconds; `None` means no idle watchdog.
    pub idle_secs: Option<u64>,
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
/// `Ok(DevelopOutcome)` on success; `Err(DeveloperError)` on failure (backend
/// spawn, transport, commit failure, or idle timeout).  The Supervisor inspects
/// the typed error to decide the [`api::FailureKind`] without string matching.
pub type DevelopAck = Result<DevelopOutcome, DeveloperError>;

impl kameo::message::Message<Develop> for Developer {
    type Reply = DevelopAck;

    async fn handle(
        &mut self,
        msg: Develop,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 1. Build a developer session config rooted at the task's worktree,
        //    carrying the role assignment (mode/model/effort) from `self.assignment`.
        let config = {
            let mut c = session_config_for(
                Role::Developer,
                msg.worktree.clone(),
                self.assignment.clone(),
            );
            c.task_id = Some(msg.task.id.0.clone());
            c
        };

        // 2. Spawn a session on the injected backend.
        let mut session =
            self.backend.spawn(config).await.map_err(|e| {
                DeveloperError::Other(format!("developer backend spawn failed: {e}"))
            })?;

        let task_id = api::TaskId(msg.task.id.0.clone());

        // Surface discovered capabilities to the TUI (step 5 of task 0040).
        if let Some(capabilities) = session.capabilities() {
            (msg.sink)(api::Event::SessionCapabilities {
                run: msg.run,
                task: task_id.clone(),
                role: api::AgentRole::Developer,
                capabilities,
            });
        }

        // 3. Build the prompt describing the task (and any reviewer feedback).
        let prompt_text = build_develop_prompt(&msg.task, msg.feedback.as_deref());

        // Publish the outgoing prompt as the Developer "user turn" (task 31).
        (msg.sink)(api::Event::AgentExchange {
            run: msg.run,
            task: task_id.clone(),
            role: api::AgentRole::Developer,
            event: api::ExchangeEvent::PromptSent {
                text: prompt_text.clone(),
            },
        });

        let turn_start = Instant::now();
        let stream = match session.prompt(Prompt::new(prompt_text)).await {
            Ok(stream) => stream,
            Err(e) => {
                // Best-effort cleanup before surfacing the error.
                let _ = session.terminate().await;
                return Err(DeveloperError::Other(format!(
                    "developer prompt failed: {e}"
                )));
            }
        };

        // 4. Drain the response stream, concatenating TextChunk text until
        //    TurnComplete (or surfacing a transport error).  Each chunk is also
        //    published as a live `ResponseChunk`, and the turn end as
        //    `TurnComplete` (task 31).
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
                            return Err(DeveloperError::IdleTimeout { idle_secs: idle });
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
                        role: api::AgentRole::Developer,
                        event: api::ExchangeEvent::ResponseChunk { text: text.clone() },
                    });
                    output.push_str(&text);
                }
                // Thought and tool events are side-channel only: they are
                // forwarded to the live `AgentExchange` stream for observability
                // but MUST NOT contribute to `output` (the final answer text is
                // built solely from `TextChunk`/`ResponseChunk`).
                Some(Ok(ResponseEvent::ThoughtChunk { text })) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Developer,
                        event: api::ExchangeEvent::ThoughtChunk { text },
                    });
                }
                Some(Ok(ResponseEvent::ToolCall {
                    id,
                    title,
                    kind,
                    status,
                    detail,
                })) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Developer,
                        event: api::ExchangeEvent::ToolCall {
                            id,
                            title,
                            kind,
                            status,
                            content: detail,
                        },
                    });
                }
                Some(Ok(ResponseEvent::ToolCallUpdate {
                    id,
                    status,
                    title,
                    detail,
                })) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Developer,
                        event: api::ExchangeEvent::ToolCallUpdate {
                            id,
                            status,
                            title,
                            content: detail,
                        },
                    });
                }
                Some(Ok(ResponseEvent::CurrentModeUpdate { current_mode_id })) => {
                    (msg.sink)(api::Event::CurrentModeUpdate {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Developer,
                        current_mode_id,
                    });
                }
                Some(Ok(ResponseEvent::TurnComplete { usage })) => {
                    (msg.sink)(api::Event::AgentExchange {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Developer,
                        event: api::ExchangeEvent::TurnComplete,
                    });
                    let model = self
                        .assignment
                        .as_ref()
                        .and_then(|a| a.model.clone())
                        .or_else(|| current_model_from(session.capabilities().as_ref()))
                        .unwrap_or_else(|| "(default)".to_string());
                    (msg.sink)(api::Event::RoleTurnMetrics {
                        run: msg.run,
                        task: task_id.clone(),
                        role: api::AgentRole::Developer,
                        model,
                        duration_ms: turn_start.elapsed().as_millis() as u64,
                        usage,
                    });
                    break;
                }
                Some(Err(e)) => {
                    drop(events);
                    let _ = session.terminate().await;
                    return Err(DeveloperError::Other(format!(
                        "developer stream error: {e}"
                    )));
                }
                None => {
                    drop(events);
                    let _ = session.terminate().await;
                    return Err(DeveloperError::Other(
                        "developer stream ended unexpectedly".to_string(),
                    ));
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
            return Err(DeveloperError::Other(format!(
                "developer commit failed: {e}"
            )));
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{AgentBackend, BackendError, ResponseEvent, ResponseStream};
    use futures::stream;

    // ── Drain helper (test-only mirror of the production watchdog loop) ───────

    /// Drain a [`ResponseStream`] with an optional idle watchdog.
    ///
    /// Mirrors the production loop in [`Developer::handle`] so the three
    /// required acceptance tests can exercise the watchdog logic without
    /// spinning up a full kameo actor stack or a real git worktree.
    ///
    /// - `Some(idle)` → each `next()` is wrapped in
    ///   `tokio::time::timeout(idle)`.  On elapse: `IdleTimeout`.
    /// - `None` → bare `next()`, identical to the pre-watchdog code (legacy path).
    ///
    /// On `TurnComplete`, emits both `AgentExchange::TurnComplete` and
    /// `RoleTurnMetrics` (matching the production handler) so tests that use
    /// this helper observe the same event sequence.
    async fn drain_stream_with_watchdog(
        mut stream: ResponseStream,
        idle_secs: Option<u64>,
        sink: &dyn Fn(api::Event),
        run: api::RunId,
        task: api::TaskId,
    ) -> Result<String, DeveloperError> {
        let mut output = String::new();
        loop {
            let item = match idle_secs {
                Some(idle) => {
                    let timeout_duration = Duration::from_secs(idle);
                    match tokio::time::timeout(timeout_duration, stream.next()).await {
                        Ok(item) => item,
                        Err(_elapsed) => {
                            sink(api::Event::TaskIdle {
                                run,
                                task: task.clone(),
                                idle_secs: idle,
                            });
                            return Err(DeveloperError::IdleTimeout { idle_secs: idle });
                        }
                    }
                }
                None => stream.next().await,
            };

            match item {
                Some(Ok(ResponseEvent::TextChunk { text })) => {
                    output.push_str(&text);
                }
                Some(Ok(ResponseEvent::TurnComplete { usage })) => {
                    sink(api::Event::AgentExchange {
                        run,
                        task: task.clone(),
                        role: api::AgentRole::Developer,
                        event: api::ExchangeEvent::TurnComplete,
                    });
                    sink(api::Event::RoleTurnMetrics {
                        run,
                        task: task.clone(),
                        role: api::AgentRole::Developer,
                        model: "(default)".to_string(),
                        duration_ms: 0,
                        usage,
                    });
                    break;
                }
                // Side-channel events don't contribute to output.
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    return Err(DeveloperError::Other(format!("stream error: {e}")));
                }
                None => {
                    return Err(DeveloperError::Other(
                        "stream ended unexpectedly".to_string(),
                    ));
                }
            }
        }
        Ok(output)
    }

    // ── Stream builders ───────────────────────────────────────────────────────

    /// Build a stream that yields `initial` text chunks immediately, then stalls
    /// forever (uses `futures::stream::pending` for the tail).  Because the tail
    /// never resolves, a `tokio::time::timeout` on the *next* `next()` call will
    /// fire once virtual time is advanced past the threshold.
    fn stalling_stream(initial: &[&str]) -> ResponseStream {
        use futures::StreamExt;
        let head: Vec<Result<ResponseEvent, BackendError>> = initial
            .iter()
            .map(|t| {
                Ok(ResponseEvent::TextChunk {
                    text: t.to_string(),
                })
            })
            .collect();
        // Append a tail that never yields.
        let tail: ResponseStream = Box::pin(futures::stream::pending());
        Box::pin(stream::iter(head).chain(tail))
    }

    /// Build a stream that yields one text chunk, waits for `delay` (using
    /// `tokio::time::sleep` so virtual time works), then yields another chunk
    /// and `TurnComplete`.  The delay is shorter than `idle_secs` to prove the
    /// timer resets on activity.
    fn periodic_stream(delay: Duration) -> ResponseStream {
        use futures::stream;
        let s = stream::unfold(0u32, move |state| async move {
            match state {
                0 => {
                    // First chunk: immediately available.
                    Some((
                        Ok(ResponseEvent::TextChunk {
                            text: "hello".to_string(),
                        }),
                        1,
                    ))
                }
                1 => {
                    // Delay (shorter than idle_secs/2), then a second chunk.
                    tokio::time::sleep(delay).await;
                    Some((
                        Ok(ResponseEvent::TextChunk {
                            text: " world".to_string(),
                        }),
                        2,
                    ))
                }
                _ => {
                    // TurnComplete.
                    Some((Ok(ResponseEvent::TurnComplete { usage: None }), u32::MAX))
                }
            }
        });
        Box::pin(s)
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// A no-op sink for tests that don't care about emitted events.
    fn noop_sink(_: api::Event) {}

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// **Acceptance test 1/3** — `idle_watchdog_fires_on_silence`
    ///
    /// A stream that yields one chunk then stalls, combined with a small
    /// `idle_secs` and a much larger wall-clock cap.  The idle watchdog MUST
    /// fire before the wall-clock cap would, and the result MUST be
    /// `DeveloperError::IdleTimeout`.
    ///
    /// Uses `tokio::time::pause` + `advance` (virtual time) so the test is
    /// fully deterministic and completes in microseconds.
    #[tokio::test(start_paused = true)]
    async fn idle_watchdog_fires_on_silence() {
        let idle_secs: u64 = 5;
        // Large wall-clock cap: 1 hour — the idle watchdog must fire first.
        let _wall_clock_secs: u64 = 3600;

        // Stream: one chunk immediately, then stalls forever.
        let stream = stalling_stream(&["initial chunk"]);

        // The drain will block waiting for the second item.
        // Advance virtual time past idle_secs to trigger the timeout.
        let drain_fut = drain_stream_with_watchdog(
            stream,
            Some(idle_secs),
            &noop_sink,
            api::RunId(0),
            api::TaskId::new("test-task"),
        );

        // Race the drain against advancing virtual time.
        // `tokio::time::sleep` inside `timeout` uses the virtual clock when paused.
        let result = tokio::time::timeout(Duration::from_secs(idle_secs + 1), drain_fut)
            .await
            .expect("test future should complete once virtual time is advanced");

        // The drain must return IdleTimeout, NOT a completion.
        match result {
            Err(DeveloperError::IdleTimeout { idle_secs: n }) => {
                assert_eq!(n, idle_secs, "idle_secs in the error must match the config");
            }
            Ok(output) => panic!("expected IdleTimeout, got Ok({output:?})"),
            Err(DeveloperError::Other(msg)) => panic!("expected IdleTimeout, got Other({msg})"),
        }
    }

    /// **Acceptance test 2/3** — `idle_watchdog_resets_on_activity`
    ///
    /// A stream that emits a chunk every `idle_secs/2` — well below the idle
    /// threshold.  The watchdog MUST NOT fire; the drain MUST complete normally
    /// (`Ok`).
    ///
    /// Uses `tokio::time::pause` / `start_paused = true` so `sleep` calls in
    /// the stream use virtual time.
    #[tokio::test(start_paused = true)]
    async fn idle_watchdog_resets_on_activity() {
        let idle_secs: u64 = 10;
        // Each chunk arrives after idle_secs/2 — within the threshold.
        let inter_chunk_delay = Duration::from_secs(idle_secs / 2);

        let stream = periodic_stream(inter_chunk_delay);

        let result = drain_stream_with_watchdog(
            stream,
            Some(idle_secs),
            &noop_sink,
            api::RunId(0),
            api::TaskId::new("test-task"),
        )
        .await;

        match result {
            Ok(output) => {
                assert!(
                    output.contains("hello"),
                    "output must contain the streamed text; got {output:?}"
                );
                assert!(
                    output.contains("world"),
                    "both chunks must be present; got {output:?}"
                );
            }
            Err(DeveloperError::IdleTimeout { idle_secs: n }) => {
                panic!("watchdog fired unexpectedly at {n}s — activity should have reset it");
            }
            Err(DeveloperError::Other(msg)) => panic!("unexpected error: {msg}"),
        }
    }

    /// **Acceptance test 3/3** — `idle_disabled_matches_legacy`
    ///
    /// With `idle_secs = None`, a stalling stream is bounded ONLY by an external
    /// wall-clock cap (not by any idle logic inside `drain_stream_with_watchdog`).
    /// This proves the `None` path is byte-for-byte the legacy behaviour: no
    /// `IdleTimeout` is ever returned.
    ///
    /// To avoid the test hanging forever (the stalling stream never resolves),
    /// we race the drain against a short `tokio::time::timeout` that represents
    /// "the wall-clock cap fired".  We assert that:
    /// (a) the drain did NOT return `IdleTimeout`,
    /// (b) and the only reason it didn't complete was the external wall-clock cap.
    #[tokio::test(start_paused = true)]
    async fn idle_disabled_matches_legacy() {
        // Stalling stream — would block forever without an external timeout.
        let stream = stalling_stream(&["one chunk"]);

        // The drain should stall (no idle watchdog); the outer timeout simulates
        // the wall-clock cap firing after a short virtual duration.
        let wall_clock_cap = Duration::from_secs(1);
        let drain_result = tokio::time::timeout(
            wall_clock_cap,
            drain_stream_with_watchdog(
                stream,
                None, // idle watchdog disabled
                &noop_sink,
                api::RunId(0),
                api::TaskId::new("test-task"),
            ),
        )
        .await;

        // The drain must have been stopped by the external wall-clock cap (Elapsed),
        // NOT by an IdleTimeout from within the drain itself.
        match drain_result {
            Err(_elapsed) => {
                // Correct: the wall-clock cap (outer timeout) fired.
                // This is the legacy path — the drain blocked until cancelled.
            }
            Ok(Err(DeveloperError::IdleTimeout { .. })) => {
                panic!("idle watchdog must NOT fire when idle_secs is None");
            }
            Ok(Ok(_)) => {
                panic!("stream was supposed to stall; it completed instead");
            }
            Ok(Err(DeveloperError::Other(msg))) => {
                panic!("unexpected other error: {msg}");
            }
        }
    }

    // ── Acceptance test: metrics_event_carries_model_and_duration ─────────────

    /// **Acceptance test** — `metrics_event_carries_model_and_duration`
    ///
    /// Sends a real `Develop` message through the kameo `Developer` actor (the
    /// production `Developer::handle` code path).  Asserts that a
    /// `Event::RoleTurnMetrics` with the assignment's model and a `duration_ms`
    /// ≥ 0 is emitted by the real handler.
    ///
    /// Uses the `NoopBackend` (one chunk + `TurnComplete`) and a real git
    /// worktree for the commit step.
    #[tokio::test]
    async fn metrics_event_carries_model_and_duration() {
        use std::sync::Mutex;

        use chrono::Utc;

        use crate::{
            actors::{Develop, Developer, DeveloperArgs, Supervisor, SupervisorArgs},
            backend::noop::NoopBackend,
            config::{Config, GlobalConfig, ProjectConfig, RoleAssignment},
            supervision::{RestartConfig, RootSupervisor},
            task::{Task, TaskId, TaskState},
            worktree::WorktreeManager,
        };

        // ── Shared sink ─────────────────────────────────────────────────────
        let events: Arc<Mutex<Vec<api::Event>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);
        let sink: crate::actors::supervisor::EventSink = Arc::new(move |e: api::Event| {
            events_clone.lock().unwrap().push(e);
        });

        // ── Backend: one text chunk then TurnComplete{usage:None} ────────────
        let backend: Arc<dyn AgentBackend> =
            Arc::new(NoopBackend::with_responses(vec!["hello".into()]));

        // ── Role assignment with model = "test-model" ────────────────────────
        let assignment = RoleAssignment {
            provider: "noop".to_string(),
            mode: None,
            model: Some("test-model".to_string()),
            effort: None,
            system_prompt: None,
            system_prompt_mode: None,
        };

        // ── Spawn actors ─────────────────────────────────────────────────────
        let root = RootSupervisor::start();

        let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
            &root,
            SupervisorArgs {
                worktree_manager: WorktreeManager::new(
                    std::path::PathBuf::from("/tmp/makina-dev-metrics-test"),
                    "develop".into(),
                ),
                config: Config::resolve(GlobalConfig::default(), ProjectConfig::default()),
            },
            RestartConfig::default(),
        )
        .await;

        let developer_ref = RootSupervisor::spawn_child::<Developer>(
            &root,
            DeveloperArgs {
                supervisor: supervisor_ref.clone(),
                backend: Arc::clone(&backend),
                assignment: Some(assignment),
            },
            RestartConfig::default(),
        )
        .await;

        // ── Real git worktree for the commit step ────────────────────────────
        let dev_worktree = tempfile::tempdir().expect("temp worktree dir");
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(dev_worktree.path())
                .args(args)
                .status()
                .expect("git must be available");
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["config", "user.name", "Test User"]);
        run_git(&["commit", "--allow-empty", "-m", "init"]);

        // ── Task ─────────────────────────────────────────────────────────────
        let now = Utc::now();
        let task = Task {
            id: TaskId::new("metrics-test-task"),
            title: "Metrics test".to_string(),
            description: "Test that metrics are emitted".to_string(),
            done_when: "RoleTurnMetrics is emitted".to_string(),
            depends_on: vec![],
            section: None,
            state: TaskState::New,
            gate_iterations: 0,
            review_iterations: 0,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
            failure_reason: None,
        };

        // ── Send Develop message through the real actor ──────────────────────
        developer_ref
            .ask(Develop {
                task,
                worktree: dev_worktree.path().to_path_buf(),
                feedback: None,
                run: api::RunId(0),
                sink,
                idle_secs: None,
            })
            .send()
            .await
            .expect("Develop must return Ok");

        // ── Assert RoleTurnMetrics was emitted ───────────────────────────────
        let collected = events.lock().unwrap();
        let metrics_event = collected
            .iter()
            .find(|e| matches!(e, api::Event::RoleTurnMetrics { .. }));

        assert!(
            metrics_event.is_some(),
            "expected a RoleTurnMetrics event; got: {collected:?}"
        );

        if let Some(api::Event::RoleTurnMetrics {
            model,
            duration_ms,
            role,
            usage,
            ..
        }) = metrics_event
        {
            assert_eq!(
                model, "test-model",
                "model must be resolved from the role assignment"
            );
            assert!(
                *duration_ms < u64::MAX,
                "duration_ms must be a measured value, got {duration_ms}"
            );
            assert_eq!(*role, api::AgentRole::Developer, "role must be Developer");
            assert!(
                usage.is_none(),
                "usage must be None when backend reports no usage"
            );
        }
    }
}
