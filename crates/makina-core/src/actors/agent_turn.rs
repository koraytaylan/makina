//! Shared helper for draining agent response streams.
//!
//! The Developer and Reviewer actors each drain an [`ResponseStream`] in an identical
//! pattern: idle-timeout watchdog, side-channel forwarding of `ThoughtChunk`/`ToolCall`
//! events, and `RoleTurnMetrics` emission. This module extracts that ~130-line loop
//! into a single `drain_agent_turn` helper; the two actors differ only in the role
//! and the role-specific error enum, so the helper returns a shared [`DrainError`]
//! that each actor maps into its own `DeveloperError`/`ReviewerError`.
//!
//! [`ResponseStream`]: crate::backend::ResponseStream

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;

use crate::api;
use crate::backend::{AgentSession, ResponseEvent, ResponseStream};
use crate::config::RoleAssignment;
use crate::roles::current_model_from;

// ── Drain error type ─────────────────────────────────────────────────────────

/// Shared error type returned by [`drain_agent_turn`].
///
/// The helper is parameterized by the idle timeout and usage; it returns a
/// shared error enum that each actor (Developer/Reviewer) maps into its own
/// role-specific error type (`DeveloperError`/`ReviewerError`) at the call site.
#[derive(Debug)]
pub enum DrainError {
    /// The idle watchdog fired: no agent output for the configured duration.
    ///
    /// Carries the configured threshold so callers can surface it in error messages.
    IdleTimeout { idle_secs: u64 },
    /// The response stream reported a transport error mid-turn.
    Stream(String),
    /// The response stream ended unexpectedly (closed without `TurnComplete`).
    EndedUnexpectedly,
}

impl std::fmt::Display for DrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DrainError::IdleTimeout { idle_secs } => {
                write!(f, "no agent output for {idle_secs}s")
            }
            DrainError::Stream(msg) => write!(f, "stream error: {msg}"),
            DrainError::EndedUnexpectedly => write!(f, "stream ended unexpectedly"),
        }
    }
}

// ── Drain helper ─────────────────────────────────────────────────────────────

/// Drain an agent response stream until `TurnComplete`, accumulating output.
///
/// Handles the idle timeout watchdog, forwards chunks as live `ResponseChunk`
/// events to the sink, accumulates the turn's text from `TextChunk` events only
/// (side channels like `ThoughtChunk` and `ToolCall` are forwarded but not
/// accumulated), and emits a `RoleTurnMetrics` event on successful completion.
///
/// This is the single source of truth for the stream-draining pattern shared by
/// the Developer and Reviewer actors; the two roles differ only in `role` and
/// the role-specific error mapping at the call site.
///
/// # Arguments
///
/// - `session`: The active agent session (holds capabilities for model inference).
/// - `events`: The response stream to drain until `TurnComplete`.
/// - `role`: The actor's role (`Developer` or `Reviewer`); used in emitted events.
/// - `task_id`: The task's ID; used in emitted events.
/// - `idle_secs`: Optional timeout in seconds. If `Some(n)`, each `events.next()`
///   is wrapped in a `tokio::time::timeout`; if the timeout elapses, the session
///   is terminated and `DrainError::IdleTimeout` is returned. If `None`, no timeout
///   is applied.
/// - `sink`: The event sink (callback) for publishing side-channel and metrics events.
/// - `run`: The run ID; used in emitted events.
/// - `turn_start`: The start time of the turn; used to compute metrics duration.
/// - `assignment`: Optional role assignment with a model override; if present and
///   contains a model, that model is used in metrics; otherwise the model is inferred
///   from the session's capabilities.
///
/// # Returns
///
/// On success: `(String, Option<api::UsageStats>)` — the accumulated response text
/// and the usage reported by the backend (the `usage` field of `ResponseEvent::TurnComplete`).
///
/// On error: `DrainError::IdleTimeout`, `DrainError::Stream`, or `DrainError::EndedUnexpectedly`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drain_agent_turn<S: AgentSession + ?Sized>(
    session: &mut S,
    events: &mut ResponseStream,
    role: api::AgentRole,
    task_id: api::TaskId,
    idle_secs: Option<u64>,
    sink: &Arc<dyn Fn(api::Event) + Send + Sync>,
    run: api::RunId,
    turn_start: Instant,
    assignment: Option<&RoleAssignment>,
) -> Result<(String, Option<api::UsageStats>), DrainError> {
    let mut output = String::new();

    loop {
        let item = match idle_secs {
            Some(idle) => {
                let timeout_duration = Duration::from_secs(idle);
                match tokio::time::timeout(timeout_duration, events.next()).await {
                    Ok(item) => item,
                    Err(_elapsed) => {
                        // Idle timeout fired: no output for idle_secs.
                        let _ = events;
                        let _ = session.terminate().await;
                        sink(api::Event::TaskIdle {
                            run,
                            task: task_id,
                            idle_secs: idle,
                        });
                        return Err(DrainError::IdleTimeout { idle_secs: idle });
                    }
                }
            }
            None => events.next().await,
        };

        match item {
            Some(Ok(ResponseEvent::TextChunk { text })) => {
                sink(api::Event::AgentExchange {
                    run,
                    task: task_id.clone(),
                    role: role.clone(),
                    event: api::ExchangeEvent::ResponseChunk { text: text.clone() },
                });
                output.push_str(&text);
            }
            // Thought and tool events are side-channel only: they are
            // forwarded to the live `AgentExchange` stream for observability
            // but MUST NOT contribute to `output` (the final answer text is
            // built solely from `TextChunk`/`ResponseChunk`).
            Some(Ok(ResponseEvent::ThoughtChunk { text })) => {
                sink(api::Event::AgentExchange {
                    run,
                    task: task_id.clone(),
                    role: role.clone(),
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
                sink(api::Event::AgentExchange {
                    run,
                    task: task_id.clone(),
                    role: role.clone(),
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
                sink(api::Event::AgentExchange {
                    run,
                    task: task_id.clone(),
                    role: role.clone(),
                    event: api::ExchangeEvent::ToolCallUpdate {
                        id,
                        status,
                        title,
                        content: detail,
                    },
                });
            }
            Some(Ok(ResponseEvent::CurrentModeUpdate { current_mode_id })) => {
                sink(api::Event::CurrentModeUpdate {
                    run,
                    task: task_id.clone(),
                    role: role.clone(),
                    current_mode_id,
                });
            }
            Some(Ok(ResponseEvent::TurnComplete { usage })) => {
                sink(api::Event::AgentExchange {
                    run,
                    task: task_id.clone(),
                    role: role.clone(),
                    event: api::ExchangeEvent::TurnComplete,
                });
                let model = assignment
                    .as_ref()
                    .and_then(|a| a.model.clone())
                    .or_else(|| current_model_from(session.capabilities().as_ref()))
                    .unwrap_or_else(|| "(default)".to_string());
                sink(api::Event::RoleTurnMetrics {
                    run,
                    task: task_id,
                    role,
                    model,
                    duration_ms: turn_start.elapsed().as_millis() as u64,
                    usage: usage.clone(),
                });
                return Ok((output, usage));
            }
            Some(Err(e)) => {
                let _ = events;
                let _ = session.terminate().await;
                return Err(DrainError::Stream(e.to_string()));
            }
            None => {
                let _ = events;
                let _ = session.terminate().await;
                return Err(DrainError::EndedUnexpectedly);
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{AgentBackend, noop::NoopBackend};

    /// Verify that text chunks are forwarded to the sink and accumulated into output.
    #[tokio::test]
    async fn test_drain_agent_turn_forwards_text_chunks() {
        // NoopBackend splits responses on newlines, so use a response with
        // a newline to produce multiple TextChunks from a single prompt.
        let backend = NoopBackend::with_responses(vec!["hello\nworld".into()]);

        let mut session = backend
            .spawn(crate::backend::SessionConfig {
                working_dir: std::path::PathBuf::from("/tmp"),
                system_prompt: "test".into(),
                mode: None,
                model: None,
                effort: None,
                extra: None,
                task_id: None,
                run_id: String::new(),
            })
            .await
            .expect("spawn must succeed");

        let mut stream = session
            .prompt(crate::backend::Prompt::new("test prompt"))
            .await
            .expect("prompt must succeed");

        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: Arc<dyn Fn(api::Event) + Send + Sync> = Arc::new({
            let events = Arc::clone(&events);
            move |e: api::Event| {
                events.lock().unwrap().push(e);
            }
        });

        let (output, _usage) = drain_agent_turn(
            &mut *session,
            &mut stream,
            api::AgentRole::Developer,
            api::TaskId::new("test-task"),
            None,
            &sink,
            api::RunId(1),
            Instant::now(),
            None,
        )
        .await
        .expect("drain should succeed");

        // ── Output accumulation ──────────────────────────────────────────────
        // NoopBackend splits on newlines using str::lines() which strips the
        // newlines, producing two chunks: "hello" and "world" concatenated directly
        assert_eq!(output, "helloworld", "output should accumulate text chunks");

        // ── Forwarding to sink ───────────────────────────────────────────────
        let captured = events.lock().unwrap();
        let has_text_chunks = captured.iter().any(|e| {
            matches!(
                e,
                api::Event::AgentExchange {
                    event: api::ExchangeEvent::ResponseChunk { text },
                    ..
                } if !text.is_empty()
            )
        });
        assert!(
            has_text_chunks,
            "sink should see ResponseChunk events forwarded"
        );
    }

    /// Verify that `RoleTurnMetrics` is emitted with the correct role and timing.
    #[tokio::test]
    async fn test_drain_agent_turn_emits_metrics() {
        let backend = NoopBackend::with_responses(vec!["output".into()]);

        let mut session = backend
            .spawn(crate::backend::SessionConfig {
                working_dir: std::path::PathBuf::from("/tmp"),
                system_prompt: "test".into(),
                mode: None,
                model: None,
                effort: None,
                extra: None,
                task_id: None,
                run_id: String::new(),
            })
            .await
            .expect("spawn must succeed");

        let mut stream = session
            .prompt(crate::backend::Prompt::new("test prompt"))
            .await
            .expect("prompt must succeed");

        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: Arc<dyn Fn(api::Event) + Send + Sync> = Arc::new({
            let events = Arc::clone(&events);
            move |e: api::Event| {
                events.lock().unwrap().push(e);
            }
        });

        let turn_start = Instant::now();
        drain_agent_turn(
            &mut *session,
            &mut stream,
            api::AgentRole::Reviewer,
            api::TaskId::new("test-task"),
            None,
            &sink,
            api::RunId(2),
            turn_start,
            None,
        )
        .await
        .expect("drain should succeed");

        let captured = events.lock().unwrap();
        let has_metrics = captured.iter().any(|e| {
            matches!(
                e,
                api::Event::RoleTurnMetrics {
                    role: api::AgentRole::Reviewer,
                    ..
                }
            )
        });
        assert!(
            has_metrics,
            "sink should see RoleTurnMetrics with correct role"
        );
    }

    /// Verify that a scripted stream (TextChunk followed by auto-appended TurnComplete)
    /// completes successfully and returns Ok.
    ///
    /// NoopBackend::scripted always appends TurnComplete if missing, so the drain
    /// succeeds normally. This test confirms drain_agent_turn can be called with a
    /// scripted stream and returns Ok with the accumulated text.
    #[tokio::test]
    async fn test_drain_agent_turn_scripted_events_succeed() {
        let backend = NoopBackend::scripted(vec![ResponseEvent::TextChunk {
            text: "scripted text".into(),
        }]);

        let mut session = backend
            .spawn(crate::backend::SessionConfig {
                working_dir: std::path::PathBuf::from("/tmp"),
                system_prompt: "test".into(),
                mode: None,
                model: None,
                effort: None,
                extra: None,
                task_id: None,
                run_id: String::new(),
            })
            .await
            .expect("spawn must succeed");

        let mut stream = session
            .prompt(crate::backend::Prompt::new("test prompt"))
            .await
            .expect("prompt must succeed");

        let sink: Arc<dyn Fn(api::Event) + Send + Sync> = Arc::new(|_: api::Event| {});

        // The scripted backend appends TurnComplete, so this should succeed.
        let result = drain_agent_turn(
            &mut *session,
            &mut stream,
            api::AgentRole::Developer,
            api::TaskId::new("test-task"),
            None,
            &sink,
            api::RunId(3),
            Instant::now(),
            None,
        )
        .await;

        let (output, _usage) = result.expect("drain should succeed with scripted events");
        assert_eq!(
            output, "scripted text",
            "accumulated text matches scripted chunk"
        );
    }

    /// Verify that the idle timeout watchdog fires, terminates the session, emits
    /// `TaskIdle`, and returns `DrainError::IdleTimeout`.
    ///
    /// Uses `tokio::time::pause()` + `tokio::time::advance()` to advance the clock
    /// without real waiting. The stream is a `futures::stream::pending()` that
    /// never yields, so the timeout is the only thing that can resolve the future.
    #[tokio::test]
    async fn test_drain_agent_turn_handles_idle_timeout() {
        use crate::backend::BackendError;
        use futures::stream;

        // Pause the Tokio clock so we can advance time deterministically.
        tokio::time::pause();

        // Spawn a NoopSession to serve as the session argument (for terminate()).
        let backend = NoopBackend::new();
        let mut session = backend
            .spawn(crate::backend::SessionConfig {
                working_dir: std::path::PathBuf::from("/tmp"),
                system_prompt: "test".into(),
                mode: None,
                model: None,
                effort: None,
                extra: None,
                task_id: None,
                run_id: String::new(),
            })
            .await
            .expect("spawn must succeed");

        // A stream that never resolves — so only the timeout can end the poll.
        let mut pending_stream: ResponseStream =
            Box::pin(stream::pending::<Result<ResponseEvent, BackendError>>());

        let emitted_events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: Arc<dyn Fn(api::Event) + Send + Sync> = Arc::new({
            let emitted_events = Arc::clone(&emitted_events);
            move |e: api::Event| {
                emitted_events.lock().unwrap().push(e);
            }
        });

        // Drive drain_agent_turn with a 1-second idle timeout in a background task,
        // then advance time by 2 seconds so the timeout fires.
        let task_id = api::TaskId::new("idle-test-task");
        let run = api::RunId(99);
        let idle_secs: u64 = 1;

        let drain_fut = drain_agent_turn(
            &mut *session,
            &mut pending_stream,
            api::AgentRole::Developer,
            task_id.clone(),
            Some(idle_secs),
            &sink,
            run,
            Instant::now(),
            None,
        );

        // Advance clock past the idle threshold so the timeout fires.
        let result = {
            let advance = tokio::time::advance(Duration::from_secs(idle_secs + 1));
            tokio::join!(drain_fut, advance).0
        };

        // ── Assert DrainError::IdleTimeout is returned ────────────────────────
        match result {
            Err(DrainError::IdleTimeout {
                idle_secs: reported,
            }) => {
                assert_eq!(
                    reported, idle_secs,
                    "IdleTimeout must carry the configured threshold"
                );
            }
            other => panic!(
                "expected DrainError::IdleTimeout, got: {:?}",
                other.map(|_| "(ok)").unwrap_or("(other err)")
            ),
        }

        // ── Assert TaskIdle was emitted ────────────────────────────────────────
        let captured = emitted_events.lock().unwrap();
        let has_task_idle = captured.iter().any(|e| {
            matches!(
                e,
                api::Event::TaskIdle {
                    run: r,
                    task: t,
                    idle_secs: s,
                } if *r == run && *t == task_id && *s == idle_secs
            )
        });
        assert!(
            has_task_idle,
            "drain_agent_turn must emit TaskIdle when the watchdog fires; got: {captured:?}"
        );
    }
}
