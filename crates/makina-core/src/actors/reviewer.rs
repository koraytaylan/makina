//! Reviewer role turn — evaluates a Developer's output.
//!
//! # Role
//!
//! The Reviewer turn receives a review assignment, evaluates the Developer's
//! changes by driving the injected [`AgentBackend`], parses the structured
//! verdict, and returns it to the scheduler.
//!
//! # Backend injection
//!
//! The agent backend is injected as `Arc<dyn AgentBackend>`. Tests inject
//! [`NoopBackend`](crate::backend::noop::NoopBackend) configured to return a
//! verdict JSON; production injects the ACP backend.
//!
//! # The review turn (task 21)
//!
//! [`review`] does the following:
//! 1. Builds a [`SessionConfig`] via [`session_config_for(Role::Reviewer, …)`].
//! 2. Spawns a session on the backend with the task's worktree as the working dir.
//! 3. Sends a prompt asking for a review of the task's work (per the
//!    [`REVIEWER_SYSTEM_PROMPT`](crate::roles::REVIEWER_SYSTEM_PROMPT) contract).
//! 4. Drains the [`ResponseStream`], then [`parse_review_verdict`]s the output.
//! 5. Terminates the session and returns the [`ReviewVerdict`].
//!
//! [`session_config_for(Role::Reviewer, …)`]: crate::roles::session_config_for
//! [`SessionConfig`]: crate::backend::SessionConfig
//! [`ResponseStream`]: crate::backend::ResponseStream
//! [`parse_review_verdict`]: crate::roles::parse_review_verdict

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::api;
use crate::backend::{AgentBackend, Prompt};
use crate::config::RoleAssignment;
use crate::roles::{Role, parse_review_verdict, session_config_for};
use crate::task::Task;

use super::agent_turn::{DrainError, drain_agent_turn};
use super::supervisor::EventSink;

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
/// This alias mirrors [`DevelopAck`](super::developer::DevelopAck).
pub type ReviewReply = Result<ReviewVerdict, ReviewerError>;

/// Run one Reviewer turn against `backend`.
pub async fn review(
    backend: Arc<dyn AgentBackend>,
    assignment: Option<RoleAssignment>,
    msg: Review,
) -> ReviewReply {
    // 1. Build a reviewer session config rooted at the task's worktree,
    //    carrying the role assignment (mode/model/effort) from `self.assignment`.
    let config = {
        let mut c = session_config_for(Role::Reviewer, msg.worktree.clone(), assignment.clone());
        c.task_id = Some(msg.task.id.0.clone());
        c
    };

    // 2. Spawn a session on the injected backend.
    let mut session = backend
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

    // 3. Prompt for a structured review verdict, restating the contract if the
    //    reply does not parse.
    //
    //    A verdict is one small JSON object, and a model that has just written
    //    a paragraph of review prose sometimes writes it in the dialect it was
    //    thinking in — single-quoted keys, an unquoted `verdict`, a trailing
    //    comma. That used to end the task and the whole run on the spot, as a
    //    hard error, after the reviewer had already approved two rounds of the
    //    same task: the one thing the model got wrong was the punctuation of
    //    its answer, and nothing asked it again. The planner has had a repair
    //    round for exactly this since it was first written; the reviewer had
    //    none.
    let mut prompt_text = build_review_prompt(&msg.task);
    let mut repairs = 0usize;
    let verdict = loop {
        // Publish the outgoing prompt as the Reviewer "user turn" (task 31).
        (msg.sink)(api::Event::AgentExchange {
            run: msg.run,
            task: task_id.clone(),
            role: api::AgentRole::Reviewer,
            event: api::ExchangeEvent::PromptSent {
                text: prompt_text.clone(),
            },
        });

        let turn_start = Instant::now();
        let stream = match session.prompt(Prompt::new(prompt_text.clone())).await {
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
        let mut events = stream;
        let (output, _usage) = drain_agent_turn(
            &mut *session,
            &mut events,
            api::AgentRole::Reviewer,
            task_id.clone(),
            msg.idle_secs,
            &msg.sink,
            msg.run,
            turn_start,
            assignment.as_ref(),
        )
        .await
        .map_err(|err| match err {
            DrainError::IdleTimeout { idle_secs } => ReviewerError::IdleTimeout { idle_secs },
            DrainError::Stream(e) => ReviewerError::Other(format!("reviewer stream error: {e}")),
            DrainError::EndedUnexpectedly => {
                ReviewerError::Other("reviewer stream ended unexpectedly".to_string())
            }
        })?;

        match parse_review_verdict(&output) {
            Ok(verdict) => break verdict,
            Err(error) if repairs < MAX_VERDICT_REPAIRS => {
                repairs += 1;
                tracing::warn!(
                    task = %msg.task.id,
                    attempt = repairs,
                    %error,
                    "reviewer verdict did not parse; asking again",
                );
                prompt_text = verdict_repair_prompt(&error);
            }
            Err(error) => {
                let _ = session.terminate().await;
                return Err(ReviewerError::Other(format!(
                    "failed to parse review verdict after {} attempts: {error}; last reply was: {}",
                    repairs + 1,
                    excerpt(&output),
                )));
            }
        }
    };

    // 5. Terminate the session (idempotent).
    let _ = session.terminate().await;

    Ok(verdict)
}

/// How many times a reviewer is asked again for a verdict that parses.
///
/// Enough for a model that mangled its punctuation to correct itself, few
/// enough that one which cannot follow the contract at all is not paid for
/// repeatedly before the task is failed.
const MAX_VERDICT_REPAIRS: usize = 2;

/// Longest excerpt of an unusable reply carried into the failure message.
const VERDICT_EXCERPT_CAP: usize = 400;

/// The tail of `output`, bounded, so the failure says what the reviewer
/// actually replied instead of only that it could not be read.
fn excerpt(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return "(nothing)".to_owned();
    }
    match trimmed.char_indices().nth(VERDICT_EXCERPT_CAP) {
        None => trimmed.to_owned(),
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
    }
}

/// Re-ask for the verdict, naming what was wrong with the last reply.
///
/// Restates the whole contract rather than only the fault: the reply that
/// failed is the model's own most recent context, and telling it to "fix the
/// JSON" without showing the shape again tends to produce another dialect of
/// the same mistake.
fn verdict_repair_prompt(error: &crate::roles::VerdictParseError) -> String {
    format!(
        "That reply could not be read as a verdict: {error}.\n\n\
         Reply with the verdict object and nothing else — no prose, no code \
         fence, no trailing comma. Keys and string values are double-quoted:\n\n\
         {{\"verdict\":\"approve\"}}\n\n\
         or\n\n\
         {{\"verdict\":\"reject\",\"feedback\":\"<what must change>\"}}\n\n\
         Your judgement of the work is unchanged; only its formatting was \
         unreadable. Send that judgement again in exactly this shape."
    )
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
