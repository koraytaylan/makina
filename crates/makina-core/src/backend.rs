//! Agent-backend trait and supporting types.
//!
//! This module defines the central seam between the Makina orchestrator and
//! external agent CLIs (e.g. an ACP-compatible subprocess).  Actors such as
//! `Developer` and `Reviewer` hold a `Box<dyn AgentBackend>` and interact with
//! agents exclusively through this interface; they never know whether the
//! backing implementation drives a subprocess, an in-process stub, or a
//! network socket.
//!
//! # Session lifecycle
//!
//! ```text
//!  AgentBackend::spawn(config)
//!        │
//!        ▼
//!  AgentSession ──► prompt(p) ──► ResponseStream ──► [TextChunk/ThoughtChunk/
//!        │                                            ToolCall/ToolCallUpdate…,
//!        │                                            TurnComplete]
//!        │
//!        ▼ terminate()
//!     (released)
//! ```
//!
//! A single `AgentSession` processes one prompt at a time (the orchestrator is
//! responsible for serialising calls if needed).  `terminate` is idempotent;
//! calling it on an already-terminated session MUST return `Ok(())`.

use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};

use crate::api;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can originate from any backend operation.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// The backend could not start the agent process / session.
    ///
    /// Carries a human-readable description of the root cause (e.g. binary
    /// not found, permission denied).
    #[error("failed to spawn agent session: {reason}")]
    Spawn { reason: String },

    /// A transport-level failure occurred while sending a prompt or receiving
    /// a response (e.g. the subprocess exited unexpectedly mid-stream).
    ///
    /// Carries a human-readable description; the session should be considered
    /// broken after this error and callers should call `terminate`.
    #[error("prompt/transport error: {reason}")]
    Transport { reason: String },

    /// The session has already been terminated (or was never successfully
    /// spawned) and cannot accept further prompts.
    ///
    /// Callers MUST NOT retry after receiving this variant; they should
    /// discard the session and spawn a new one if needed.
    #[error("agent session is terminated")]
    Terminated,
}

// ── Supporting types ──────────────────────────────────────────────────────────

/// Configuration supplied to [`AgentBackend::spawn`] to describe the new session.
///
/// # Forward-compatibility
///
/// New optional fields may be added in the future.  Implementers MUST ignore
/// fields they do not recognise (Rust's struct literal syntax naturally enforces
/// this through `..Default::default()` patterns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    /// The working directory in which the agent should operate.
    ///
    /// For subprocess backends this becomes the process's `cwd`.  Implementers
    /// MUST propagate this path rather than silently ignoring it.
    pub working_dir: PathBuf,

    /// The role/system prompt injected at the start of the session.
    ///
    /// This is the agent's "identity" for the entire session; it is sent once
    /// at spawn time (or equivalent) rather than repeated with every prompt.
    /// Implementers MUST pass it to the agent before the first user turn.
    pub system_prompt: String,

    /// Optional default mode ID to apply to the session (via `session/set_mode` if supported).
    ///
    /// The backend should apply this mode after `session/new` if the agent advertises it
    /// in its modes. Ignored if the agent does not support modes or does not advertise
    /// the requested mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,

    /// Optional default model option value to apply to the session.
    ///
    /// The backend should apply this option after `session/new` if the agent advertises it
    /// in its config options with category "model". Ignored if not advertised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Optional default effort (thought_level) option value to apply to the session.
    ///
    /// The backend should apply this option after `session/new` if the agent advertises it
    /// in its config options with category "thought_level". Ignored if not advertised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    /// Optional backend-specific settings serialised as a TOML value.
    ///
    /// Concrete backends may deserialise this into their own strongly-typed
    /// configuration.  A value of `None` means "use backend defaults".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<toml::Value>,
}

/// A single turn prompt sent to an [`AgentSession`].
///
/// # Future extension
///
/// The inner `text` field is the only content type for MVP.  Structured
/// content (images, file attachments) may be added as additional fields later
/// without breaking existing implementations because they would be optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    /// The plain-text prompt for this turn.
    pub text: String,
}

impl Prompt {
    /// Convenience constructor.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// A single event emitted from a [`ResponseStream`].
///
/// Implementers stream zero or more
/// [`TextChunk`](ResponseEvent::TextChunk) /
/// [`ThoughtChunk`](ResponseEvent::ThoughtChunk) /
/// [`ToolCall`](ResponseEvent::ToolCall) /
/// [`ToolCallUpdate`](ResponseEvent::ToolCallUpdate) events followed by exactly
/// one [`TurnComplete`](ResponseEvent::TurnComplete) to signal that the agent
/// has finished its turn.  The stream MUST then end (i.e. yield `None` as the
/// next `Poll`).
///
/// Only [`TextChunk`](ResponseEvent::TextChunk) contributes to the final answer
/// text — consumers wanting just the answer accumulate `TextChunk` and ignore
/// the rest (thoughts and tool activity are a side channel surfaced for live UI).
///
/// # Contracts for implementers
///
/// * `TextChunk` events MAY arrive in any granularity (one per token, one per
///   line, or even the entire response in one chunk).  The TUI accumulates
///   them in order.
/// * `ThoughtChunk`, `ToolCall`, and `ToolCallUpdate` MAY be emitted at any
///   point before the final `TurnComplete`.  They are informational only;
///   consumers that only want the final answer text MUST ignore them (or
///   accumulate `TextChunk` exclusively).
/// * `TurnComplete` MUST be the final event before the stream closes.  Sending
///   further events after `TurnComplete` is a protocol violation.
/// * If the agent errors mid-stream, implementers SHOULD yield an `Err` value
///   rather than a `TurnComplete`; the stream MUST still terminate after the
///   error (i.e. yield `None` next).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseEvent {
    /// A streamed chunk of the agent's text response.
    ///
    /// The TUI concatenates chunks in arrival order to build the full message.
    /// This is the ONLY variant that contributes to the final answer text.
    TextChunk {
        /// A fragment of the agent's response text.  Never empty.
        text: String,
    },

    /// A streamed chunk of the agent's "thinking"/reasoning stream.
    ///
    /// Side-channel only: consumers building the final answer ignore this.
    ThoughtChunk {
        /// A fragment of the agent's reasoning text.  Never empty.
        text: String,
    },

    /// The agent announced a tool call.
    ///
    /// Side-channel only: consumers building the final answer ignore this.
    ToolCall {
        /// Stable id correlating this call with later [`ResponseEvent::ToolCallUpdate`]s.
        id: String,
        /// Human-readable title (empty when the agent omits it).
        title: String,
        /// Optional semantic kind (e.g. `"execute"`, `"edit"`).
        kind: Option<String>,
        /// Lifecycle status (`"pending"` when the agent omits it).
        status: String,
    },

    /// A status/result update for a previously-announced tool call.
    ///
    /// Side-channel only: consumers building the final answer ignore this.
    ToolCallUpdate {
        /// The id of the [`ResponseEvent::ToolCall`] this updates.
        id: String,
        /// Updated lifecycle status, if the update carried one.
        status: Option<String>,
        /// Updated title, if the update carried one.
        title: Option<String>,
    },

    /// The agent autonomously changed its operating mode.
    ///
    /// Side-channel only: consumers building the final answer ignore this.
    CurrentModeUpdate {
        /// The id of the mode the agent switched to.
        current_mode_id: String,
    },

    /// The agent has finished generating its response for this turn.
    ///
    /// This event MUST be the last item before the stream closes.
    TurnComplete,
}

// ── Stream type alias ─────────────────────────────────────────────────────────

/// A boxed, owned stream of response events for one prompt turn.
///
/// Consumers drive the stream with standard `futures::StreamExt` combinators
/// or via a manual poll loop.  The stream yields zero or more
/// `TextChunk`/`ThoughtChunk`/`ToolCall`/`ToolCallUpdate` events followed by
/// exactly one [`ResponseEvent::TurnComplete`]; only `TextChunk` contributes to
/// the final answer text — consumers wanting just the answer accumulate
/// `TextChunk` and ignore the rest.  The stream ends (returns
/// `Poll::Ready(None)`) after [`ResponseEvent::TurnComplete`] is yielded or
/// after a [`BackendError`] is yielded.
pub type ResponseStream = Pin<Box<dyn Stream<Item = Result<ResponseEvent, BackendError>> + Send>>;

// ── Traits ────────────────────────────────────────────────────────────────────

/// Factory for agent sessions.
///
/// A single `AgentBackend` instance may be shared across actors (it is `Send +
/// Sync`) and used to create multiple concurrent sessions.  Implementers MUST
/// ensure that sessions are fully independent; one session failing MUST NOT
/// affect another.
///
/// # Object safety
///
/// `AgentBackend` is designed to be used as `Box<dyn AgentBackend>` in actor
/// state.  All methods are `async` via `#[async_trait]` to preserve object
/// safety.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Spawn a new agent session described by `config`.
    ///
    /// # Contracts
    ///
    /// * Implementers MUST apply `config.working_dir` as the session's working
    ///   directory.
    /// * Implementers MUST inject `config.system_prompt` before the first user
    ///   turn (the mechanism is backend-specific).
    /// * On success the returned session is ready to accept the first
    ///   [`AgentSession::prompt`] call.
    /// * On failure the function returns [`BackendError::Spawn`]; the caller
    ///   need not call `terminate` in this case.
    async fn spawn(&self, config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError>;
}

/// A single, stateful agent session.
///
/// Callers interact with the session by sending prompts and consuming the
/// resulting [`ResponseStream`].  Sessions are typically short-lived: one per
/// Developer/Reviewer task invocation.
///
/// # Concurrency
///
/// Sessions are NOT `Sync`; callers MUST NOT call `prompt` concurrently from
/// multiple tasks.  The orchestrator is responsible for ensuring serial access.
///
/// # Object safety
///
/// `AgentSession` is designed to be used as `Box<dyn AgentSession>` inside
/// actor state.  All methods are `async` via `#[async_trait]` to preserve
/// object safety.
#[async_trait]
pub trait AgentSession: Send {
    /// Send `prompt` to the agent and return a stream of response events.
    ///
    /// # Contracts
    ///
    /// * The returned stream MUST yield zero or more
    ///   `TextChunk`/`ThoughtChunk`/`ToolCall`/`ToolCallUpdate`
    ///   [`ResponseEvent`]s followed by exactly one
    ///   [`ResponseEvent::TurnComplete`] if the turn succeeds.  Implementers MAY
    ///   emit the `ThoughtChunk`/`ToolCall`/`ToolCallUpdate` side-channel
    ///   variants at any point before the final `TurnComplete`; consumers that
    ///   only want the final answer text should ignore them (or only accumulate
    ///   `TextChunk`).
    /// * The stream MUST terminate (return `Poll::Ready(None)`) immediately
    ///   after either `TurnComplete` or an `Err` item.
    /// * Calling `prompt` on a terminated session MUST return
    ///   [`BackendError::Terminated`] immediately (without returning a stream).
    /// * The caller MUST fully drain (or drop) the previous turn's stream
    ///   before calling `prompt` again; behaviour is undefined otherwise and
    ///   implementations are free to return an error or panic.
    async fn prompt(&mut self, prompt: Prompt) -> Result<ResponseStream, BackendError>;

    /// Terminate the session and release its resources.
    ///
    /// For subprocess backends this typically means sending a graceful shutdown
    /// signal and waiting briefly, then force-killing the process.
    ///
    /// # Contracts
    ///
    /// * `terminate` MUST be idempotent: calling it on an already-terminated
    ///   session MUST return `Ok(())` without panicking or returning an error.
    /// * After `terminate` returns, all resources held by the session (file
    ///   descriptors, child process handles, etc.) MUST be released.
    /// * Callers SHOULD call `terminate` even if a previous `prompt` returned
    ///   an error, to ensure resource cleanup.
    async fn terminate(&mut self) -> Result<(), BackendError>;

    /// Return the capabilities (modes, config options) discovered during the
    /// session handshake, if any.
    ///
    /// The default implementation returns `None`, which is correct for test
    /// backends and any backend whose agent does not advertise capabilities.
    /// ACP-backed sessions override this to surface the modes and config
    /// options received in the `session/new` response.
    ///
    /// Callers (Developer/Reviewer handlers) invoke this immediately after
    /// [`AgentBackend::spawn`] to emit an [`api::Event::SessionCapabilities`]
    /// event to the TUI.
    fn capabilities(&self) -> Option<api::SessionCapabilities> {
        None
    }
}

// ── Submodules ────────────────────────────────────────────────────────────────

pub mod noop;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! In-crate compile/contract proof for `AgentBackend` + `AgentSession`.
    //!
    //! This is NOT the `NoopBackend` (which lives in a later task); it is a
    //! minimal inline stub that proves:
    //!   1. The traits are object-safe (`Box<dyn …>` compiles).
    //!   2. A straightforward stub can implement both traits.
    //!   3. The stream contract (chunks followed by TurnComplete) works end-to-end.

    use super::*;
    use futures::StreamExt;
    use futures::stream;
    use std::path::PathBuf;

    // ── Stub backend ──────────────────────────────────────────────────────────

    /// A canned backend that always returns a fixed two-chunk response.
    struct StubBackend;

    #[async_trait]
    impl AgentBackend for StubBackend {
        async fn spawn(
            &self,
            _config: SessionConfig,
        ) -> Result<Box<dyn AgentSession>, BackendError> {
            Ok(Box::new(StubSession { terminated: false }))
        }
    }

    struct StubSession {
        terminated: bool,
    }

    #[async_trait]
    impl AgentSession for StubSession {
        async fn prompt(&mut self, prompt: Prompt) -> Result<ResponseStream, BackendError> {
            if self.terminated {
                return Err(BackendError::Terminated);
            }
            // Echo the prompt back as two chunks plus TurnComplete.
            let echo = prompt.text.clone();
            let events: Vec<Result<ResponseEvent, BackendError>> = vec![
                Ok(ResponseEvent::TextChunk {
                    text: format!("Echo: {echo}"),
                }),
                Ok(ResponseEvent::TextChunk {
                    text: " (done)".to_string(),
                }),
                Ok(ResponseEvent::TurnComplete),
            ];
            Ok(Box::pin(stream::iter(events)))
        }

        async fn terminate(&mut self) -> Result<(), BackendError> {
            // Idempotent: already terminated is still Ok.
            self.terminated = true;
            Ok(())
        }
    }

    // ── Helper: run via trait-objects ──────────────────────────────────────────

    /// Exercises the full lifecycle through `Box<dyn …>` to prove object-safety.
    async fn run_session(backend: &dyn AgentBackend) -> Vec<ResponseEvent> {
        let config = SessionConfig {
            working_dir: PathBuf::from("/tmp/stub"),
            system_prompt: "You are a stub agent.".to_string(),
            mode: None,
            model: None,
            effort: None,
            extra: None,
        };
        let mut session: Box<dyn AgentSession> = backend.spawn(config).await.unwrap();

        let prompt = Prompt::new("hello world");
        let stream = session.prompt(prompt).await.unwrap();

        let events: Vec<ResponseEvent> = stream
            .map(|r| r.expect("stream item should be Ok"))
            .collect()
            .await;

        session.terminate().await.unwrap();
        // Idempotency: second terminate must also succeed.
        session.terminate().await.unwrap();

        events
    }

    // ── Tests ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stub_streams_chunks_then_turn_complete() {
        let backend = StubBackend;
        let events = run_session(&backend).await;

        assert_eq!(events.len(), 3, "expected 2 chunks + TurnComplete");

        // First two events are TextChunks.
        assert!(
            matches!(&events[0], ResponseEvent::TextChunk { text } if text.starts_with("Echo:")),
            "first event should be an Echo TextChunk"
        );
        assert!(
            matches!(&events[1], ResponseEvent::TextChunk { text } if text.contains("done")),
            "second event should be the (done) TextChunk"
        );

        // Final event is TurnComplete.
        assert!(
            matches!(&events[2], ResponseEvent::TurnComplete),
            "last event should be TurnComplete"
        );
    }

    #[tokio::test]
    async fn terminated_session_returns_terminated_error() {
        let backend: Box<dyn AgentBackend> = Box::new(StubBackend);
        let config = SessionConfig {
            working_dir: PathBuf::from("/tmp/stub"),
            system_prompt: "stub".to_string(),
            mode: None,
            model: None,
            effort: None,
            extra: None,
        };
        let mut session = backend.spawn(config).await.unwrap();
        session.terminate().await.unwrap();

        let result = session.prompt(Prompt::new("after terminate")).await;
        assert!(
            matches!(result, Err(BackendError::Terminated)),
            "prompt after terminate should return Terminated"
        );
    }

    #[tokio::test]
    async fn session_config_round_trips_through_serde() {
        // Use toml (already a workspace dep) to exercise serde Serialize/Deserialize.
        let config = SessionConfig {
            working_dir: PathBuf::from("/repo/task-42"),
            system_prompt: "You are a developer.".to_string(),
            mode: None,
            model: None,
            effort: None,
            extra: None,
        };
        let serialised = toml::to_string(&config).expect("serialise config to TOML");
        let deserialised: SessionConfig =
            toml::from_str(&serialised).expect("deserialise config from TOML");
        assert_eq!(config.system_prompt, deserialised.system_prompt);
        assert_eq!(config.working_dir, deserialised.working_dir);
    }

    #[tokio::test]
    async fn text_accumulation_ignores_thought_and_tool_events() {
        // A stream that interleaves the side-channel variants (ThoughtChunk,
        // ToolCall, ToolCallUpdate) with the real answer text. A consumer that
        // wants only the final answer accumulates TextChunk and ignores the
        // rest — proving the new variants are a pure side channel.
        let events: Vec<Result<ResponseEvent, BackendError>> = vec![
            Ok(ResponseEvent::ThoughtChunk {
                text: "let me think".to_string(),
            }),
            Ok(ResponseEvent::TextChunk {
                text: "Hello".to_string(),
            }),
            Ok(ResponseEvent::ToolCall {
                id: "tc-1".to_string(),
                title: "run tests".to_string(),
                kind: Some("execute".to_string()),
                status: "pending".to_string(),
            }),
            Ok(ResponseEvent::TextChunk {
                text: ", world!".to_string(),
            }),
            Ok(ResponseEvent::ToolCallUpdate {
                id: "tc-1".to_string(),
                status: Some("completed".to_string()),
                title: None,
            }),
            Ok(ResponseEvent::TurnComplete),
        ];
        let stream: ResponseStream = Box::pin(stream::iter(events));

        // Drain, accumulating ONLY TextChunk into the answer string.
        let mut answer = String::new();
        let mut events = stream;
        while let Some(item) = events.next().await {
            match item.expect("no error item in this scripted stream") {
                ResponseEvent::TextChunk { text } => answer.push_str(&text),
                ResponseEvent::ThoughtChunk { .. }
                | ResponseEvent::ToolCall { .. }
                | ResponseEvent::ToolCallUpdate { .. }
                | ResponseEvent::CurrentModeUpdate { .. } => {
                    // Side-channel events do not contribute to the answer.
                }
                ResponseEvent::TurnComplete => break,
            }
        }

        assert_eq!(
            answer, "Hello, world!",
            "only TextChunk text should contribute to the accumulated answer"
        );
    }

    #[tokio::test]
    async fn response_event_derives_are_usable() {
        let chunk = ResponseEvent::TextChunk {
            text: "hello".to_string(),
        };
        let complete = ResponseEvent::TurnComplete;

        // Clone must compile and produce equal values.
        let chunk2 = chunk.clone();
        let complete2 = complete.clone();

        // Debug formatting must not panic.
        let dbg = format!("{chunk2:?} {complete2:?}");
        assert!(dbg.contains("TextChunk"), "Debug should mention TextChunk");
        assert!(
            dbg.contains("TurnComplete"),
            "Debug should mention TurnComplete"
        );

        // Serde round-trip through toml (workspace dep, no extras needed).
        // Note: toml serializes enums differently; we verify the enum is
        // serde-annotated correctly by confirming Serialize is callable.
        let _ser_chunk = toml::to_string(&chunk).expect("TextChunk should serialize");
        let _ser_complete = toml::to_string(&complete).expect("TurnComplete should serialize");
    }
}
