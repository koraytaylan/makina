//! Developer role turn — executes a single task in a worktree.
//!
//! # Role
//!
//! The Developer turn receives a task assignment, works on it inside a git
//! worktree by driving the injected [`AgentBackend`], commits the result, and
//! returns the collected agent output to the scheduler.
//!
//! # Backend injection
//!
//! The agent backend is injected as `Arc<dyn AgentBackend>`. Tests inject
//! [`NoopBackend`](crate::backend::noop::NoopBackend); production injects the ACP
//! backend. The Developer turn never knows which concrete backend it is driving.
//!
//! # The develop turn (task 21)
//!
//! [`develop`] does the following:
//! 1. Builds a [`SessionConfig`] via [`session_config_for(Role::Developer, …)`].
//! 2. Spawns a session on the backend with the task's worktree as the working dir.
//! 3. Sends a single prompt describing the task (title/description/`done_when`,
//!    plus any reviewer feedback on a retry).
//! 4. Drains the [`ResponseStream`], concatenating the agent's text output.
//! 5. Terminates the session.
//! 6. Commits the worktree's changes to `task/{id}` (`git add -A` + `git commit
//!    --allow-empty`) so task 23's squash-merge has the work to land on
//!    `develop`.
//!
//! [`session_config_for(Role::Developer, …)`]: crate::roles::session_config_for
//! [`SessionConfig`]: crate::backend::SessionConfig
//! [`ResponseStream`]: crate::backend::ResponseStream

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::api;
use crate::backend::{AgentBackend, Prompt};
use crate::config::RoleAssignment;
use crate::roles::{Role, session_config_for};
use crate::task::Task;

use super::agent_turn::{DrainError, drain_agent_turn};
use super::supervisor::EventSink;

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

/// Run one Developer turn against `backend`.
pub async fn develop(
    backend: Arc<dyn AgentBackend>,
    assignment: Option<RoleAssignment>,
    msg: Develop,
) -> DevelopAck {
    // 1. Build a developer session config rooted at the task's worktree,
    //    carrying the role assignment (mode/model/effort) from `self.assignment`.
    let config = {
        let mut c = session_config_for(Role::Developer, msg.worktree.clone(), assignment.clone());
        c.task_id = Some(msg.task.id.0.clone());
        c
    };

    // 2. Spawn a session on the injected backend.
    let mut session = backend
        .spawn(config)
        .await
        .map_err(|e| DeveloperError::Other(format!("developer backend spawn failed: {e}")))?;

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
    let mut events = stream;
    let (output, _usage) = drain_agent_turn(
        &mut *session,
        &mut events,
        api::AgentRole::Developer,
        task_id.clone(),
        msg.idle_secs,
        &msg.sink,
        msg.run,
        turn_start,
        assignment.as_ref(),
    )
    .await
    .map_err(|err| match err {
        DrainError::IdleTimeout { idle_secs } => DeveloperError::IdleTimeout { idle_secs },
        DrainError::Stream(e) => DeveloperError::Other(format!("developer stream error: {e}")),
        DrainError::EndedUnexpectedly => {
            DeveloperError::Other("developer stream ended unexpectedly".to_string())
        }
    })?;

    // 5. Terminate the session (idempotent — also called on error paths above).
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
        // Deliberately not "a reviewer rejected this": work is also returned
        // after the reviewer *approved* it, when the branch touched paths the
        // task does not own. Naming the reviewer there sent the agent looking
        // for quality problems in work that had just been approved, and it
        // spent every remaining round defending it. The feedback says who is
        // asking and what for; the preamble only has to be true.
        prompt.push_str(&format!(
            "\nThis is a revision. The previous attempt was returned with the \
             following feedback — address it specifically:\n{feedback}\n"
        ));
    }

    prompt
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AgentBackend;

    // ── Acceptance test: metrics_event_carries_model_and_duration ─────────────

    /// **Acceptance test** — `metrics_event_carries_model_and_duration`
    ///
    /// Runs a real [`develop`] turn and asserts that a
    /// `Event::RoleTurnMetrics` with the assignment's model and a `duration_ms`
    /// is emitted by the real handler.
    ///
    /// Uses the `NoopBackend` (one chunk + `TurnComplete`) and a real git
    /// worktree for the commit step.
    #[tokio::test]
    async fn metrics_event_carries_model_and_duration() {
        use std::sync::Mutex;

        use chrono::Utc;

        use crate::{
            backend::noop::NoopBackend,
            config::RoleAssignment,
            task::{Task, TaskId, TaskState},
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

        // ── Real git worktree for the commit step ────────────────────────────
        let dev_worktree = tempfile::tempdir().expect("temp worktree dir");
        crate::test_support::init_git_repo_with_identity(dev_worktree.path());

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

        // ── Run the real Developer turn ──────────────────────────────────────
        develop(
            Arc::clone(&backend),
            Some(assignment),
            Develop {
                task,
                worktree: dev_worktree.path().to_path_buf(),
                feedback: None,
                run: api::RunId(0),
                sink,
                idle_secs: None,
            },
        )
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
