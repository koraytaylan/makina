//! `NoopBackend` — deterministic in-process test backend.
//!
//! A production-available (NOT `#[cfg(test)]`) implementation of
//! [`AgentBackend`] that never spawns a subprocess or makes network calls.
//! It is the standard test double for orchestration and integration tests
//! (e.g. task 12 — testing harness, task 21 — develop-review-loop) that need
//! a real backend without wiring up an actual agent CLI.
//!
//! # Features
//!
//! - **Canned responses** — configure the text each prompt should return.
//!   By default every prompt yields a single `TextChunk { text: "noop response"
//!   }` followed by `TurnComplete`. Customise via
//!   [`NoopBackend::with_responses`].
//!
//! - **Response cycling** — when the configured response list is exhausted the
//!   backend cycles back to the first entry rather than erroring.  If the list
//!   is empty the default `"noop response"` is used instead.
//!
//! - **Prompt recording** — every prompt text is appended to a shared
//!   `Arc<Mutex<Vec<String>>>` that callers can inspect via
//!   [`NoopBackend::recorded_prompts`].  All sessions spawned from the same
//!   backend share the same recorder.
//!
//! - **Contract compliance** — the three trait contracts are always honoured:
//!   1. Every response stream ends with `TurnComplete` as the final event.
//!   2. [`NoopSession::terminate`] is idempotent; double-terminate returns
//!      `Ok(())`.
//!   3. [`NoopSession::prompt`] on a terminated session returns
//!      `BackendError::Terminated` without producing a stream.
//!
//! # Multi-chunk responses
//!
//! Pass a response string containing `\n` characters to get one `TextChunk`
//! per line, enabling streaming-consumer tests (task 30) that need to observe
//! multiple `TextChunk` events.  Single-line strings produce one `TextChunk`
//! followed by `TurnComplete` (the common case).
//!
//! # Example
//!
//! ```no_run
//! use std::path::PathBuf;
//! use makina_core::backend::{AgentBackend, Prompt, SessionConfig};
//! use makina_core::backend::noop::NoopBackend;
//! use futures::StreamExt;
//!
//! # async fn example() {
//! let backend = NoopBackend::with_responses(vec!["hello".into(), "world".into()]);
//!
//! let config = SessionConfig {
//!     working_dir: PathBuf::from("/tmp"),
//!     system_prompt: "test".into(),
//!     extra: None,
//! };
//!
//! let mut session = backend.spawn(config).await.unwrap();
//! let stream = session.prompt(Prompt::new("ping")).await.unwrap();
//! let events: Vec<_> = stream.collect().await;
//! // events = [Ok(TextChunk { text: "hello" }), Ok(TurnComplete)]
//!
//! assert_eq!(backend.recorded_prompts(), vec!["ping"]);
//! # }
//! ```

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;

use crate::backend::{
    AgentBackend, AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream, SessionConfig,
};

// ── NoopBackend ───────────────────────────────────────────────────────────────

/// Deterministic in-process backend for testing.
///
/// See the [module-level documentation](self) for a full description of
/// capabilities and usage.
#[derive(Clone)]
pub struct NoopBackend {
    /// Canned responses cycled in order across all prompts.
    ///
    /// Empty means "use default response for every prompt".
    responses: Vec<String>,

    /// Shared prompt recorder.  All sessions spawned from this backend append
    /// to the same list so that the creating test can read back what the actors
    /// actually sent.
    recorder: Arc<Mutex<Vec<String>>>,

    /// Shared atomic counter for cycling through `responses`.
    response_index: Arc<Mutex<usize>>,

    /// Scripted events for the rich-path test support (see [`NoopBackend::scripted`]).
    ///
    /// When `Some`, every prompt emits exactly this sequence (with a
    /// `TurnComplete` appended if the script does not already end with one),
    /// bypassing the `responses`/`next_response` text path entirely.  `None`
    /// preserves the default canned-text behaviour.
    scripted_events: Option<Vec<ResponseEvent>>,
}

impl NoopBackend {
    /// Create a backend that returns `"noop response"` for every prompt.
    pub fn new() -> Self {
        Self {
            responses: Vec::new(),
            recorder: Arc::new(Mutex::new(Vec::new())),
            response_index: Arc::new(Mutex::new(0)),
            scripted_events: None,
        }
    }

    /// Create a backend that cycles through `responses` (one entry per prompt).
    ///
    /// If `responses` is empty this behaves identically to [`NoopBackend::new`].
    ///
    /// **Cycling behaviour**: when all entries have been used the index wraps
    /// back to zero, so a test with N prompts but fewer configured responses
    /// will never see an error just because the list ran out.
    pub fn with_responses(responses: Vec<String>) -> Self {
        Self {
            responses,
            recorder: Arc::new(Mutex::new(Vec::new())),
            response_index: Arc::new(Mutex::new(0)),
            scripted_events: None,
        }
    }

    /// Build a backend whose every turn emits exactly the given scripted events
    /// (followed by `TurnComplete` if the script does not already end with one).
    ///
    /// This is test-support for the *rich* response path: it lets a test make a
    /// Noop session emit `ThoughtChunk`/`ToolCall`/`ToolCallUpdate` side-channel
    /// events interleaved with `TextChunk`s, which the default canned-text path
    /// cannot produce.  Prompts are still recorded into [`recorded_prompts`].
    ///
    /// [`recorded_prompts`]: NoopBackend::recorded_prompts
    pub fn scripted(events: Vec<ResponseEvent>) -> Self {
        Self {
            responses: Vec::new(),
            recorder: Arc::new(Mutex::new(Vec::new())),
            response_index: Arc::new(Mutex::new(0)),
            scripted_events: Some(events),
        }
    }

    /// Return the full list of prompt texts received so far (in order).
    ///
    /// Acquires the internal mutex; do not call from within an async context
    /// that might already hold it (there is no re-entrant locking).
    pub fn recorded_prompts(&self) -> Vec<String> {
        self.recorder
            .lock()
            .expect("noop recorder mutex poisoned")
            .clone()
    }

    /// Pick the next canned response text and advance the cycle index.
    fn next_response(&self) -> String {
        if self.responses.is_empty() {
            return "noop response".to_string();
        }
        let mut idx = self
            .response_index
            .lock()
            .expect("noop response_index mutex poisoned");
        let text = self.responses[*idx % self.responses.len()].clone();
        *idx = (*idx + 1) % self.responses.len();
        text
    }
}

impl Default for NoopBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentBackend for NoopBackend {
    /// Spawn a new `NoopSession` that shares this backend's recorder and
    /// response state.
    ///
    /// `config.working_dir` and `config.system_prompt` are accepted but not
    /// acted upon — this backend has no subprocess or network socket to
    /// configure.
    async fn spawn(&self, _config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError> {
        Ok(Box::new(NoopSession {
            terminated: false,
            recorder: Arc::clone(&self.recorder),
            backend: self.clone(),
        }))
    }
}

// ── NoopSession ───────────────────────────────────────────────────────────────

/// A single session created by [`NoopBackend`].
///
/// Records each prompt text and returns canned responses supplied at backend
/// construction time.  Upholds all three `AgentSession` contracts:
///
/// * Response stream always ends with `TurnComplete`.
/// * `terminate` is idempotent.
/// * `prompt` on a terminated session returns `BackendError::Terminated`.
pub struct NoopSession {
    terminated: bool,
    recorder: Arc<Mutex<Vec<String>>>,
    /// Clone of the originating backend, used to cycle responses and share
    /// the same `response_index` as sibling sessions.
    backend: NoopBackend,
}

#[async_trait]
impl AgentSession for NoopSession {
    /// Record the prompt then return a canned response stream.
    ///
    /// # Contract
    ///
    /// Returns `BackendError::Terminated` immediately if the session has been
    /// terminated.  Otherwise: if the backend was built with
    /// [`NoopBackend::scripted`], emits exactly the scripted events (appending a
    /// `TurnComplete` if absent); otherwise produces zero or more `TextChunk`
    /// events followed by exactly one `TurnComplete`.
    async fn prompt(&mut self, prompt: Prompt) -> Result<ResponseStream, BackendError> {
        if self.terminated {
            return Err(BackendError::Terminated);
        }

        // Record the prompt text.
        self.recorder
            .lock()
            .expect("noop recorder mutex poisoned")
            .push(prompt.text.clone());

        // Rich-path test support: if the backend was built with `scripted`,
        // emit exactly those events (ensuring a trailing TurnComplete) and skip
        // the canned-text path entirely.
        if let Some(script) = &self.backend.scripted_events {
            let mut events: Vec<Result<ResponseEvent, BackendError>> =
                script.iter().cloned().map(Ok).collect();
            if !matches!(events.last(), Some(Ok(ResponseEvent::TurnComplete))) {
                events.push(Ok(ResponseEvent::TurnComplete));
            }
            return Ok(Box::pin(stream::iter(events)));
        }

        // Retrieve the next canned response.
        let response_text = self.backend.next_response();

        // Split on newlines to produce one TextChunk per line (multi-chunk
        // support for streaming-consumer tests).  A single-line response
        // produces exactly one TextChunk followed by TurnComplete.
        let mut events: Vec<Result<ResponseEvent, BackendError>> = response_text
            .lines()
            .map(|line| {
                Ok(ResponseEvent::TextChunk {
                    text: line.to_string(),
                })
            })
            .collect();

        // Guarantee: stream ends with TurnComplete (contract §1).
        events.push(Ok(ResponseEvent::TurnComplete));

        Ok(Box::pin(stream::iter(events)))
    }

    /// Release the session.
    ///
    /// # Contract
    ///
    /// Idempotent: a second (or Nth) call returns `Ok(())` without error.
    async fn terminate(&mut self) -> Result<(), BackendError> {
        // Set the flag regardless of its current value (idempotent).
        self.terminated = true;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Unit tests for [`NoopBackend`] and [`NoopSession`], plus the required
    //! "actor drives backend without a real CLI" integration test using kameo.

    use std::path::PathBuf;

    use futures::StreamExt;
    use kameo::{
        actor::{ActorRef, Spawn},
        message::{Context, Message},
    };

    use super::*;
    use crate::backend::{AgentBackend, BackendError, Prompt, ResponseEvent, SessionConfig};

    // ── helpers ──────────────────────────────────────────────────────────────

    fn test_config() -> SessionConfig {
        SessionConfig {
            working_dir: PathBuf::from("/tmp/noop-test"),
            system_prompt: "You are a noop agent.".to_string(),
            extra: None,
        }
    }

    /// Drain a `ResponseStream` into a `Vec<ResponseEvent>`, panicking on any
    /// error item.
    async fn drain_ok(stream: ResponseStream) -> Vec<ResponseEvent> {
        stream
            .map(|r| r.expect("unexpected Err item in noop stream"))
            .collect()
            .await
    }

    // ── contract 1: TurnComplete is always the final stream event ─────────────

    #[tokio::test]
    async fn stream_ends_with_turn_complete_default() {
        let backend = NoopBackend::new();
        let mut session = backend.spawn(test_config()).await.unwrap();

        let stream = session.prompt(Prompt::new("hi")).await.unwrap();
        let events = drain_ok(stream).await;

        assert!(!events.is_empty(), "stream must not be empty");
        assert!(
            matches!(events.last().unwrap(), ResponseEvent::TurnComplete),
            "last event must be TurnComplete, got: {:?}",
            events.last()
        );
    }

    #[tokio::test]
    async fn stream_ends_with_turn_complete_custom_response() {
        let backend = NoopBackend::with_responses(vec!["custom text".into()]);
        let mut session = backend.spawn(test_config()).await.unwrap();

        let stream = session.prompt(Prompt::new("q")).await.unwrap();
        let events = drain_ok(stream).await;

        assert!(
            matches!(events.last().unwrap(), ResponseEvent::TurnComplete),
            "last event must be TurnComplete"
        );
    }

    #[tokio::test]
    async fn multiline_response_produces_multiple_text_chunks_then_turn_complete() {
        let backend = NoopBackend::with_responses(vec!["line one\nline two\nline three".into()]);
        let mut session = backend.spawn(test_config()).await.unwrap();

        let stream = session.prompt(Prompt::new("q")).await.unwrap();
        let events = drain_ok(stream).await;

        // 3 TextChunk events + 1 TurnComplete
        assert_eq!(events.len(), 4, "expected 3 chunks + TurnComplete");
        assert!(matches!(&events[0], ResponseEvent::TextChunk { text } if text == "line one"));
        assert!(matches!(&events[1], ResponseEvent::TextChunk { text } if text == "line two"));
        assert!(matches!(&events[2], ResponseEvent::TextChunk { text } if text == "line three"));
        assert!(matches!(&events[3], ResponseEvent::TurnComplete));
    }

    // ── rich path: scripted() emits side-channel events ───────────────────────

    #[tokio::test]
    async fn scripted_emits_thought_and_tool_events_then_appends_turn_complete() {
        // Script omits the trailing TurnComplete on purpose to prove it is
        // appended automatically.
        let backend = NoopBackend::scripted(vec![
            ResponseEvent::ThoughtChunk {
                text: "planning".into(),
            },
            ResponseEvent::ToolCall {
                id: "tc-1".into(),
                title: "run tests".into(),
                kind: Some("execute".into()),
                status: "pending".into(),
            },
            ResponseEvent::TextChunk {
                text: "the answer".into(),
            },
            ResponseEvent::ToolCallUpdate {
                id: "tc-1".into(),
                status: Some("completed".into()),
                title: None,
            },
        ]);
        let mut session = backend.spawn(test_config()).await.unwrap();

        let stream = session.prompt(Prompt::new("go")).await.unwrap();
        let events = drain_ok(stream).await;

        assert_eq!(events.len(), 5, "4 scripted events + appended TurnComplete");
        assert!(matches!(&events[0], ResponseEvent::ThoughtChunk { text } if text == "planning"));
        assert!(matches!(&events[1], ResponseEvent::ToolCall { id, .. } if id == "tc-1"));
        assert!(matches!(&events[2], ResponseEvent::TextChunk { text } if text == "the answer"));
        assert!(matches!(&events[3], ResponseEvent::ToolCallUpdate { id, .. } if id == "tc-1"));
        assert!(matches!(&events[4], ResponseEvent::TurnComplete));

        // The prompt was still recorded.
        assert_eq!(backend.recorded_prompts(), vec!["go"]);
    }

    // ── contract 2: terminate is idempotent ───────────────────────────────────

    #[tokio::test]
    async fn terminate_is_idempotent() {
        let backend = NoopBackend::new();
        let mut session = backend.spawn(test_config()).await.unwrap();

        // First terminate
        session.terminate().await.unwrap();
        // Second terminate must also succeed.
        session.terminate().await.unwrap();
        // Third for good measure.
        session.terminate().await.unwrap();
    }

    // ── contract 3: prompt on terminated session → BackendError::Terminated ──

    #[tokio::test]
    async fn prompt_after_terminate_returns_terminated_error() {
        let backend = NoopBackend::new();
        let mut session = backend.spawn(test_config()).await.unwrap();
        session.terminate().await.unwrap();

        let result = session.prompt(Prompt::new("too late")).await;
        assert!(
            matches!(result, Err(BackendError::Terminated)),
            "expected BackendError::Terminated"
        );
    }

    // ── with_responses: cycling and recording ─────────────────────────────────

    #[tokio::test]
    async fn with_responses_cycles_and_records_prompts() {
        let backend = NoopBackend::with_responses(vec!["alpha".into(), "beta".into()]);
        let mut session = backend.spawn(test_config()).await.unwrap();

        // Prompt 0 → "alpha"
        let events = drain_ok(session.prompt(Prompt::new("p0")).await.unwrap()).await;
        assert!(matches!(&events[0], ResponseEvent::TextChunk { text } if text == "alpha"));

        // Prompt 1 → "beta"
        let events = drain_ok(session.prompt(Prompt::new("p1")).await.unwrap()).await;
        assert!(matches!(&events[0], ResponseEvent::TextChunk { text } if text == "beta"));

        // Prompt 2 → cycles back to "alpha"
        let events = drain_ok(session.prompt(Prompt::new("p2")).await.unwrap()).await;
        assert!(matches!(&events[0], ResponseEvent::TextChunk { text } if text == "alpha"));

        // All three prompts were recorded.
        let recorded = backend.recorded_prompts();
        assert_eq!(recorded, vec!["p0", "p1", "p2"]);
    }

    #[tokio::test]
    async fn empty_responses_uses_default_text() {
        let backend = NoopBackend::with_responses(vec![]);
        let mut session = backend.spawn(test_config()).await.unwrap();

        let events = drain_ok(session.prompt(Prompt::new("x")).await.unwrap()).await;
        assert!(
            matches!(&events[0], ResponseEvent::TextChunk { text } if text == "noop response"),
            "expected default 'noop response' text, got: {:?}",
            events[0]
        );
    }

    #[tokio::test]
    async fn multiple_sessions_share_recorder_and_response_cycle() {
        let backend = NoopBackend::with_responses(vec!["r0".into(), "r1".into()]);
        let mut s1 = backend.spawn(test_config()).await.unwrap();
        let mut s2 = backend.spawn(test_config()).await.unwrap();

        let e1 = drain_ok(s1.prompt(Prompt::new("from-s1")).await.unwrap()).await;
        let e2 = drain_ok(s2.prompt(Prompt::new("from-s2")).await.unwrap()).await;

        // Responses should be handed out across sessions in round-robin order.
        assert!(matches!(&e1[0], ResponseEvent::TextChunk { text } if text == "r0"));
        assert!(matches!(&e2[0], ResponseEvent::TextChunk { text } if text == "r1"));

        // Both prompts appear in the shared recorder.
        let recorded = backend.recorded_prompts();
        assert_eq!(recorded, vec!["from-s1", "from-s2"]);
    }

    // ── actor-drives-backend test ─────────────────────────────────────────────
    //
    // Required test: "a test drives an actor through the backend without a real CLI".
    //
    // A tiny kameo actor (`BackendActor`) holds a `Box<dyn AgentBackend>`.  On
    // receiving a `RunPrompt` message it:
    //   1. Spawns a session from the backend.
    //   2. Sends the prompt text from the message.
    //   3. Drains the `ResponseStream`, collecting `TextChunk` text until
    //      `TurnComplete`.
    //   4. Terminates the session.
    //   5. Returns the concatenated response text.
    //
    // The test then asserts:
    //   a. The canned response came back correctly.
    //   b. `recorded_prompts()` on the shared backend shows the exact prompt the
    //      actor sent.

    struct BackendActor {
        backend: Box<dyn AgentBackend>,
    }

    impl kameo::actor::Actor for BackendActor {
        type Args = Box<dyn AgentBackend>;
        type Error = std::convert::Infallible;

        async fn on_start(
            args: Self::Args,
            _actor_ref: ActorRef<Self>,
        ) -> Result<Self, Self::Error> {
            Ok(BackendActor { backend: args })
        }
    }

    /// Message: run a single prompt through the backend and return the response.
    struct RunPrompt {
        text: String,
    }

    impl Message<RunPrompt> for BackendActor {
        type Reply = Result<String, String>;

        async fn handle(
            &mut self,
            msg: RunPrompt,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            // 1. Spawn a session.
            let config = SessionConfig {
                working_dir: PathBuf::from("/tmp/actor-test"),
                system_prompt: "noop".to_string(),
                extra: None,
            };
            let mut session = self
                .backend
                .spawn(config)
                .await
                .map_err(|e| e.to_string())?;

            // 2. Send the prompt.
            let stream = session
                .prompt(Prompt::new(msg.text))
                .await
                .map_err(|e| e.to_string())?;

            // 3. Drain the stream, collecting TextChunk text until TurnComplete.
            let mut collected = String::new();
            let mut events = stream;
            while let Some(item) = events.next().await {
                match item.map_err(|e| e.to_string())? {
                    ResponseEvent::TextChunk { text } => {
                        if !collected.is_empty() {
                            collected.push('\n');
                        }
                        collected.push_str(&text);
                    }
                    // Side-channel events do not contribute to the collected
                    // answer text.
                    ResponseEvent::ThoughtChunk { .. }
                    | ResponseEvent::ToolCall { .. }
                    | ResponseEvent::ToolCallUpdate { .. } => {}
                    ResponseEvent::TurnComplete => break,
                }
            }

            // 4. Terminate the session.
            session.terminate().await.map_err(|e| e.to_string())?;

            // 5. Return collected response text.
            Ok(collected)
        }
    }

    #[tokio::test]
    async fn actor_drives_backend_without_real_cli() {
        // Build a NoopBackend with a specific canned response and keep a clone
        // for later assertions (shared Arc recorder).
        let backend = NoopBackend::with_responses(vec!["the answer".into()]);
        let backend_clone = backend.clone();

        // Spawn the actor, injecting the backend as a trait object.
        let actor_ref = BackendActor::spawn(Box::new(backend) as Box<dyn AgentBackend>);

        // Send the RunPrompt message and await the reply.
        // ask().await returns Result<Reply::Ok, SendError<M, Reply::Error>>.
        // With type Reply = Result<String, String>, Reply::Ok = String.
        // So the outer Result wraps a SendError; the inner Result<String,String>
        // is the handler's return value. We must send it via the kameo Reply
        // machinery: for Result<T,E> kameo unwraps it and surfaces E as a
        // HandlerError(SendError).  A clean success → Ok(String).
        let response = actor_ref
            .ask(RunPrompt {
                text: "what is the question?".to_string(),
            })
            .await
            .expect("actor ask failed (send error)");

        // (a) Assert the canned response came back.
        assert_eq!(
            response, "the answer",
            "actor should have received the canned response"
        );

        // (b) Assert the recorder captured the prompt the actor sent.
        let recorded = backend_clone.recorded_prompts();
        assert_eq!(
            recorded,
            vec!["what is the question?"],
            "recorded_prompts should show exactly the prompt the actor sent"
        );
    }
}
