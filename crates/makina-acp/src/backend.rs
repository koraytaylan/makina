//! The ACP adapter: `makina_core::backend::AgentBackend` over [`AcpClient`].
//!
//! This module (task 15 — `acp-backend-impl`) maps the high-level
//! [`AcpClient`](crate::AcpClient) onto the generic agent-backend trait in
//! `makina-core`, so the rest of Makina drives ACP agents through
//! `Box<dyn AgentBackend>` / `Box<dyn AgentSession>` without knowing the
//! protocol.
//!
//! # The borrowing-to-owned stream bridge
//!
//! The central challenge is that [`AcpClient::prompt`](crate::AcpClient::prompt)
//! returns a [`PromptStream<'a>`](crate::PromptStream) that **borrows** the
//! client, whereas [`AgentSession::prompt`] must hand back an **owned**
//! `ResponseStream` (`Pin<Box<dyn Stream + Send + 'static>>`). A borrowing stream
//! cannot be boxed into a `'static` one.
//!
//! We bridge with a moved client + two channels:
//!
//! ```text
//!  prompt(p):
//!    reclaim client (drain previous turn) ─┐
//!                                          ▼
//!    move client into tokio::spawn ── drives PromptStream ──► mpsc::Sender
//!                                          │                       │
//!                                          │  on turn end / error  ▼
//!                                          └── drop PromptStream  ResponseStream
//!                                              return client via oneshot   (the caller)
//! ```
//!
//! * The session **owns** the client as `Option<AcpClient>`; `None` represents a
//!   terminated session, and `Some` lets us move it in/out of the worker task.
//! * `prompt()` opens a [`tokio::sync::mpsc`] channel and moves the client into a
//!   spawned task. That task calls `client.prompt(text)`, drives the
//!   `PromptStream` to completion, maps each [`AcpResponseChunk`] to a
//!   [`ResponseEvent`], and forwards it through the mpsc sender. When the turn
//!   ends (or the receiver is dropped, or an error occurs) the task **drops the
//!   `PromptStream`** (releasing the borrow) and returns the [`AcpClient`] to the
//!   session via a [`tokio::sync::oneshot`] channel.
//! * The next `prompt()` / `terminate()` reclaims the client from that oneshot
//!   first, which both upholds the "drain/finish the previous turn before the
//!   next prompt" contract and guarantees the client is never leaked.
//! * If the consumer drops the `ResponseStream` early, the next mpsc `send`
//!   fails; the worker stops immediately and still returns the client.
//! * The bridged stream **always** ends with [`ResponseEvent::TurnComplete`] as
//!   its final item on a successful turn; a mid-turn error is surfaced as a
//!   [`BackendError`] item (never a false `TurnComplete`).
//!
//! # System prompt
//!
//! The minimal ACP client has no first-class system-prompt slot, so for the MVP
//! the session **prepends `SessionConfig.system_prompt` to the text of the first
//! prompt only** (subsequent turns are sent verbatim). The hook
//! ([`AcpSession::compose_first_turn`]) is isolated so task 19 (`role-prompts`)
//! can refine role-specific prompting without touching the bridge.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use makina_core::api;
use makina_core::backend::{
    AgentBackend, AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream, SessionConfig,
};
use makina_core::governance::{AuditSink, NoopAuditSink};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use crate::client::{AcpClient, AcpCommand, AcpResponseChunk};
use crate::error::AcpError;

/// Buffer size for the mpsc channel that carries mapped [`ResponseEvent`]s from
/// the per-turn worker task to the consumer's [`ResponseStream`].
///
/// Bounded so a slow consumer applies backpressure to the worker (rather than
/// letting unbounded chunks accumulate); large enough that bursty chunk delivery
/// rarely blocks the worker between consumer polls.
const CHANNEL_CAPACITY: usize = 64;

// ── Error mapping ──────────────────────────────────────────────────────────────

/// Map an [`AcpError`] onto the generic [`BackendError`], per the task-13 handoff
/// table:
///
/// | `AcpError`                              | `BackendError` |
/// |-----------------------------------------|----------------|
/// | `Spawn`                                 | `Spawn`        |
/// | `Transport` / `Protocol` / `Rpc` / `AgentExited` | `Transport` |
/// | `Closed`                                | `Terminated`   |
fn map_error(err: AcpError) -> BackendError {
    match err {
        AcpError::Spawn(reason) => BackendError::Spawn { reason },
        AcpError::Closed => BackendError::Terminated,
        // All remaining variants are transport-domain failures: the session is
        // broken and the caller should `terminate`.
        other => BackendError::Transport {
            reason: other.to_string(),
        },
    }
}

// ── AcpBackend ───────────────────────────────────────────────────────────────

/// An [`AgentBackend`] that drives ACP-compatible agent CLIs via [`AcpClient`].
///
/// Construct it with the agent program + args to spawn (e.g. from
/// `makina_core::config::BackendConfig`). Each [`spawn`](AcpBackend::spawn) builds
/// an [`AcpCommand`] — program/args from the backend, `working_dir` from the
/// per-session [`SessionConfig`] — connects an [`AcpClient`] (inheriting the
/// environment, per the Zed auth model), and wraps it in an [`AcpSession`].
///
/// `AcpBackend` is cheap to clone-by-reference (`Send + Sync`) and may back many
/// concurrent sessions; sessions are fully independent.
///
/// The **audit sink** is run-level: one `Arc<dyn AuditSink>` shared across all
/// sessions spawned by this backend.  Inject it via
/// [`AcpBackend::with_audit_sink`]; the default is [`NoopAuditSink`].
///
/// The **permission policy** is session-scoped (it depends on `working_dir`).
/// By default a [`crate::permission::WorktreePolicy`] is built from the session's
/// `working_dir` at connect time.  The policy flows through [`AcpCommand`], so
/// per-session overrides are possible via
/// [`AcpCommand::with_policy`] when constructing a command directly.
pub struct AcpBackend {
    /// The agent program to execute (e.g. `gemini`, `npx`).
    program: PathBuf,
    /// Arguments passed to the program (e.g. `["--acp"]`).
    args: Vec<String>,
    /// Extra environment variables layered on top of the inherited environment
    /// (Zed auth model — credentials are never injected here).
    env: Vec<(String, String)>,
    /// Run-level audit sink, shared by all sessions.
    audit_sink: Arc<dyn AuditSink>,
}

// Manual Clone: Arc<dyn AuditSink> is Clone but not derived without a bound.
impl Clone for AcpBackend {
    fn clone(&self) -> Self {
        Self {
            program: self.program.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            audit_sink: Arc::clone(&self.audit_sink),
        }
    }
}

// Manual Debug: Arc<dyn AuditSink> may not be Debug.
// `finish_non_exhaustive` signals that `audit_sink` is intentionally omitted.
impl std::fmt::Debug for AcpBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpBackend")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &self.env)
            .finish_non_exhaustive()
    }
}

impl AcpBackend {
    /// Build a backend that spawns `program` with `args` for each session.
    ///
    /// The environment is inherited from the parent process (the agent CLI is
    /// expected to be already authenticated); use [`AcpBackend::env`] to layer
    /// extra variables on top.
    ///
    /// Orchestration typically builds this from
    /// `makina_core::config::BackendConfig { command, args }`; raw program/args
    /// are taken here to avoid coupling `makina-acp` to the core config type.
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            env: Vec::new(),
            audit_sink: Arc::new(NoopAuditSink),
        }
    }

    /// Layer one extra environment variable onto every spawned session
    /// (added on top of the inherited environment, never replacing it).
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Set the audit sink that records every permission decision across all
    /// sessions spawned by this backend.  The same `Arc` is shared across all
    /// sessions (clone-by-reference), so a capturing test sink can be read back
    /// after the backend is used.
    #[must_use]
    pub fn with_audit_sink(mut self, sink: Arc<dyn AuditSink>) -> Self {
        self.audit_sink = sink;
        self
    }

    /// Build the [`AcpCommand`] for a session in `working_dir`.
    ///
    /// The command carries the backend's audit sink and no policy override (the
    /// default [`crate::permission::WorktreePolicy`] will be built from
    /// `working_dir` at connect time).
    fn command_for(&self, working_dir: PathBuf) -> AcpCommand {
        let mut command = AcpCommand::new(self.program.clone(), working_dir)
            .args(self.args.clone())
            .with_audit_sink(Arc::clone(&self.audit_sink));
        for (key, value) in &self.env {
            command = command.env(key.clone(), value.clone());
        }
        command
    }

    /// Test-only seam: connect a client over `reader`/`writer` using the policy
    /// and audit sink that `spawn` would thread through for `working_dir`.
    ///
    /// This is the acceptance-test entry point for `gateway-threading`: it calls
    /// `command_for(working_dir)` (the same path as `spawn`) and then calls
    /// `AcpClient::with_transport` with the command's `effective_policy()` and
    /// `audit_sink`, so the test's capturing sink reaches the transport through
    /// the real `AcpBackend → command_for → AcpCommand` wiring rather than being
    /// injected directly.
    #[cfg(test)]
    pub(crate) async fn spawn_with_transport<R, W>(
        &self,
        reader: R,
        writer: W,
        working_dir: PathBuf,
    ) -> crate::error::Result<AcpClient>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let command = self.command_for(working_dir);
        // Use the single shared derivation (same path as spawn_transport) so the
        // test seam and the production path always agree on which policy/sink to
        // thread through (see `AcpCommand::transport_inputs`).
        let (policy, sink) = command.transport_inputs();
        AcpClient::with_transport(
            reader,
            writer,
            &command.working_dir,
            Some(policy),
            Some(sink),
        )
        .await
    }
}

#[async_trait]
impl AgentBackend for AcpBackend {
    /// Connect a new ACP session.
    ///
    /// Builds an [`AcpCommand`] (program/args from this backend, `working_dir`
    /// from `config`), runs [`AcpClient::connect`] (spawn + `initialize` +
    /// `session/new`), and wraps the connected client in an [`AcpSession`].
    /// `config.system_prompt` is stored and prepended to the first prompt.
    ///
    /// After the session is created, if the role assignment specifies a `mode`,
    /// `model`, or `effort` and the agent advertises those capabilities, they are
    /// applied via `set_mode` / `set_config_option` (silently skipped if not
    /// advertised).
    ///
    /// Connect failures are mapped to [`BackendError`] (`Spawn` for spawn
    /// failures, `Transport` for handshake/protocol failures).
    async fn spawn(&self, config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError> {
        let command = self.command_for(config.working_dir);
        let mut client = AcpClient::connect(command).await.map_err(map_error)?;

        // Apply role assignments (mode, model, effort) if advertised.
        if let Some(mode) = &config.mode
            && let Some(modes) = client.modes()
            && modes.available_modes.iter().any(|m| &m.id == mode)
        {
            client.set_mode(mode).await.map_err(map_error)?;
        }

        if let Some(model) = &config.model {
            // Find the first option whose category is "model" — its *id* is the
            // option identifier; `model` is the desired *value* to set on it.
            let model_option_id = client
                .config_options()
                .iter()
                .find(|o| o.category == "model")
                .map(|o| o.id.clone());
            if let Some(option_id) = model_option_id {
                client
                    .set_config_option(&option_id, serde_json::Value::String(model.clone()))
                    .await
                    .map_err(map_error)?;
            }
        }

        if let Some(effort) = &config.effort {
            // Find the first option whose category is "thought_level" — its *id* is
            // the option identifier; `effort` is the desired *value* to set on it.
            let effort_option_id = client
                .config_options()
                .iter()
                .find(|o| o.category == "thought_level")
                .map(|o| o.id.clone());
            if let Some(option_id) = effort_option_id {
                client
                    .set_config_option(&option_id, serde_json::Value::String(effort.clone()))
                    .await
                    .map_err(map_error)?;
            }
        }

        Ok(Box::new(AcpSession::from_client(
            client,
            config.system_prompt,
        )))
    }
}

// ── Capability snapshot helper ────────────────────────────────────────────────

/// Build an [`api::SessionCapabilities`] snapshot from a connected
/// [`AcpClient`], mapping the ACP protocol types to the API view types.
///
/// Returns `None` if the client advertises neither modes nor config options
/// (i.e. the agent did not include them in `session/new`).
fn build_capabilities(client: &AcpClient) -> Option<api::SessionCapabilities> {
    let modes = client.modes().map(|m| api::SessionModes {
        current_mode_id: m.current_mode_id.clone(),
        available_modes: m
            .available_modes
            .iter()
            .map(|mode| api::SessionModeView {
                id: mode.id.clone(),
                name: mode.name.clone(),
                description: mode.description.clone(),
            })
            .collect(),
    });

    let config_options: Vec<api::ConfigOptionView> = client
        .config_options()
        .iter()
        .map(|opt| api::ConfigOptionView {
            id: opt.id.clone(),
            name: opt.name.clone(),
            category: opt.category.clone(),
            kind: opt.kind.clone(),
            current_value: opt.current_value.clone(),
            options: opt
                .options
                .iter()
                .map(|choice| api::ConfigOptionChoiceView {
                    value: choice.value.clone(),
                    name: choice.name.clone(),
                    description: choice.description.clone(),
                })
                .collect(),
        })
        .collect();

    if modes.is_none() && config_options.is_empty() {
        None
    } else {
        Some(api::SessionCapabilities {
            modes,
            config_options,
        })
    }
}

// ── AcpSession ───────────────────────────────────────────────────────────────

/// A single ACP agent session behind the [`AgentSession`] trait.
///
/// Owns the [`AcpClient`] as `Option<AcpClient>` so it can be moved into a
/// per-turn worker task and reclaimed afterwards; `None` means the session has
/// been terminated. See the [module docs](self) for the full bridge design.
pub struct AcpSession {
    /// The connected client when the session is live and idle, `None` once
    /// terminated. While a turn is in flight this is `None` and the client lives
    /// in the worker task; it is reclaimed via [`pending_return`](Self::pending_return).
    client: Option<AcpClient>,
    /// Channel on which the in-flight turn's worker returns the client when the
    /// turn ends (success, error, or early consumer drop). Reclaimed before the
    /// next `prompt`/`terminate`.
    pending_return: Option<oneshot::Receiver<AcpClient>>,
    /// The session/role prompt, prepended to the first prompt's text. `take`n on
    /// first use so later turns are sent verbatim.
    system_prompt: Option<String>,
    /// Capabilities snapshot taken from the client at construction time (after
    /// `session/new` handshake). `None` if the agent did not advertise any.
    capabilities: Option<api::SessionCapabilities>,
}

impl AcpSession {
    /// Wrap an already-connected [`AcpClient`] as a trait session, recording the
    /// `system_prompt` to prepend to the first turn.
    ///
    /// This is the seam used by tests: build an [`AcpClient`] via
    /// [`AcpClient::with_transport`] against an in-memory mock agent (no
    /// subprocess), wrap it here, and exercise the [`AgentSession`] trait
    /// methods. It is also how [`AcpBackend::spawn`] constructs the session.
    ///
    /// Snapshots the client's advertised capabilities at construction time so
    /// that callers can query them via [`AgentSession::capabilities`] without
    /// needing access to the client directly.
    pub fn from_client(client: AcpClient, system_prompt: impl Into<String>) -> Self {
        let system_prompt = system_prompt.into();
        let capabilities = build_capabilities(&client);
        Self {
            client: Some(client),
            pending_return: None,
            // An empty system prompt is treated as "no prelude" so we never send
            // a stray leading blank line.
            system_prompt: (!system_prompt.is_empty()).then_some(system_prompt),
            capabilities,
        }
    }

    /// Reclaim the client after a turn: if a previous `prompt` is still in
    /// flight, await the worker's `oneshot` (which fires the moment that turn
    /// ends — success, error, or early consumer drop) and restore the client.
    ///
    /// Upholds the trait's "drain/finish the previous turn before the next
    /// prompt" contract and guarantees the client is never leaked. If the worker
    /// dropped the sender without returning the client (only possible if the
    /// worker task itself was cancelled), the session transitions to terminated.
    async fn reclaim_client(&mut self) {
        if let Some(rx) = self.pending_return.take() {
            match rx.await {
                Ok(client) => self.client = Some(client),
                // Worker vanished without returning the client (e.g. task abort):
                // treat the session as terminated rather than hang.
                Err(_) => self.client = None,
            }
        }
    }

    /// Compose the text actually sent to the agent for this turn.
    ///
    /// On the first turn the stored system prompt (if any) is prepended,
    /// separated by a blank line, then cleared so later turns send `text`
    /// verbatim. This is the documented MVP system-prompt handling and the
    /// single hook task 19 (`role-prompts`) should refine.
    fn compose_first_turn(&mut self, text: String) -> String {
        match self.system_prompt.take() {
            Some(prelude) => format!("{prelude}\n\n{text}"),
            None => text,
        }
    }
}

#[async_trait]
impl AgentSession for AcpSession {
    /// Send `prompt` to the agent and return an owned stream of response events.
    ///
    /// Reclaims the client from any previous in-flight turn, then moves it into a
    /// worker task that drives one ACP turn and forwards mapped events over an
    /// mpsc channel (see the [module docs](self)). The returned stream yields
    /// zero or more [`ResponseEvent::TextChunk`] then exactly one
    /// [`ResponseEvent::TurnComplete`]; a mid-turn failure yields a
    /// [`BackendError::Transport`] item instead of a false `TurnComplete`, after
    /// which the stream ends.
    ///
    /// Returns [`BackendError::Terminated`] immediately (no stream) if the
    /// session has been terminated.
    async fn prompt(&mut self, prompt: Prompt) -> Result<ResponseStream, BackendError> {
        // Finish/drain the previous turn so we own the client again.
        self.reclaim_client().await;

        let Some(mut client) = self.client.take() else {
            return Err(BackendError::Terminated);
        };

        // Apply the first-turn system-prompt prelude (no-op on later turns).
        let text = self.compose_first_turn(prompt.text);

        let (event_tx, event_rx) =
            mpsc::channel::<Result<ResponseEvent, BackendError>>(CHANNEL_CAPACITY);
        let (return_tx, return_rx) = oneshot::channel::<AcpClient>();
        self.pending_return = Some(return_rx);

        tokio::spawn(async move {
            // Drive one ACP turn. Inside `run_turn`, `client.prompt` borrows
            // `client` to build the `PromptStream`; that borrow ends when
            // `run_turn` returns (its stream local is dropped), so the client is
            // free to move again here.
            run_turn(&mut client, text, &event_tx).await;
            // Return the (no-longer-borrowed) client so the session can reuse or
            // shut it down. If the session was dropped meanwhile the send fails
            // and the client is dropped here — its own `Drop` tears the
            // subprocess down, so nothing leaks.
            let _ = return_tx.send(client);
        });

        Ok(Box::pin(ReceiverStream::new(event_rx)))
    }

    /// Terminate the session, shutting down the underlying client.
    ///
    /// Reclaims the client from any in-flight turn first (so an early-dropped or
    /// still-running turn is wound down cleanly), then calls
    /// [`AcpClient::shutdown`]. Idempotent: once terminated the client is `None`
    /// and subsequent calls return `Ok(())`.
    async fn terminate(&mut self) -> Result<(), BackendError> {
        // Reclaim the client from any pending turn so we can shut it down (and so
        // a still-running worker is wound down rather than leaked).
        self.reclaim_client().await;

        match self.client.take() {
            Some(mut client) => {
                // `AcpClient::shutdown` is itself idempotent and maps to Ok even
                // if the subprocess is already gone; surface any error typed.
                client.shutdown().await.map_err(map_error)
            }
            // Already terminated — idempotent success.
            None => Ok(()),
        }
    }

    /// Return the capabilities snapshot taken from the ACP client at session
    /// construction time (after `session/new`).  `None` if the agent did not
    /// advertise any modes or config options.
    fn capabilities(&self) -> Option<api::SessionCapabilities> {
        self.capabilities.clone()
    }
}

// ── Per-turn worker ────────────────────────────────────────────────────────────

/// Drive one ACP prompt turn to completion, forwarding mapped [`ResponseEvent`]s
/// into `event_tx`.
///
/// Guarantees the trait stream contract:
/// * `AcpResponseChunk::Text(s)` → `ResponseEvent::TextChunk { text: s }`;
/// * `AcpResponseChunk::Thought(s)` → `ResponseEvent::ThoughtChunk { text: s }`;
/// * `AcpResponseChunk::ToolCall { .. }` → `ResponseEvent::ToolCall { .. }`
///   (1:1 field mapping);
/// * `AcpResponseChunk::ToolCallUpdate { .. }` → `ResponseEvent::ToolCallUpdate
///   { .. }` (1:1 field mapping);
/// * `AcpResponseChunk::TurnComplete(_)` → `ResponseEvent::TurnComplete`, sent as
///   the final item on a clean turn;
/// * any [`AcpError`] (including a failure to even start the turn) → a single
///   [`BackendError`] item (never followed by a `TurnComplete`).
///
/// The `Thought`/`ToolCall`/`ToolCallUpdate` chunks are forwarded as
/// side-channel `ResponseEvent`s; consumers that only want the final answer text
/// accumulate `TextChunk` and ignore the rest.
///
/// Stops early — without error — if a `send` fails, which means the consumer
/// dropped the [`ResponseStream`]. In every case the borrowed `PromptStream` is
/// dropped before this function returns, freeing the client to be moved back.
async fn run_turn(
    client: &mut AcpClient,
    text: String,
    event_tx: &mpsc::Sender<Result<ResponseEvent, BackendError>>,
) {
    use futures::StreamExt;

    // Starting the turn can fail synchronously (e.g. the client is already
    // closed, or the transport's reader task already ended). Surface that as a
    // single terminal error item — not a TurnComplete.
    let mut stream = match client.prompt(text) {
        Ok(stream) => stream,
        Err(err) => {
            let _ = event_tx.send(Err(map_error(err))).await;
            return;
        }
    };

    while let Some(item) = stream.next().await {
        let event = match item {
            Ok(AcpResponseChunk::Text(text)) => Ok(ResponseEvent::TextChunk { text }),
            // Rich side-channel chunks map 1:1 to their ResponseEvent twins.
            Ok(AcpResponseChunk::Thought(text)) => Ok(ResponseEvent::ThoughtChunk { text }),
            Ok(AcpResponseChunk::ToolCall {
                id,
                title,
                kind,
                status,
                detail,
            }) => Ok(ResponseEvent::ToolCall {
                id,
                title,
                kind,
                status,
                detail,
            }),
            Ok(AcpResponseChunk::ToolCallUpdate {
                id,
                status,
                title,
                detail,
            }) => Ok(ResponseEvent::ToolCallUpdate {
                id,
                status,
                title,
                detail,
            }),
            Ok(AcpResponseChunk::CurrentModeUpdate { current_mode_id }) => {
                Ok(ResponseEvent::CurrentModeUpdate { current_mode_id })
            }
            Ok(AcpResponseChunk::TurnComplete(_reason)) => {
                // Final item on a clean turn. Forward it; whether or not the
                // consumer is still listening, the turn is over. The ACP
                // `stop_reason` is intentionally dropped — `ResponseEvent::TurnComplete`
                // carries no reason in the MVP.
                //
                // Usage is mapped to `None` here (MVP fallback): the ACP
                // `PromptResult.usage` field is parsed by the protocol layer but
                // not yet threaded through `AcpResponseChunk::TurnComplete`
                // (would require changing `client.rs`).  Behaviour is identical —
                // usage simply never lights up until that threading is added.
                let _ = event_tx
                    .send(Ok(ResponseEvent::TurnComplete { usage: None }))
                    .await;
                return;
            }
            // Mid-turn failure: surface a typed error item and stop. The ACP
            // `PromptStream` itself ends after an error, but we return eagerly so
            // we never emit a TurnComplete after it.
            Err(err) => Err(map_error(err)),
        };

        let is_err = event.is_err();
        // A send failure means the consumer dropped the ResponseStream; stop
        // forwarding (the turn's borrow is released when `stream` drops on
        // return). On a forwarded error item we are also done.
        if event_tx.send(event).await.is_err() || is_err {
            return;
        }
    }
    // NOTE: if the ACP stream ends without a TurnComplete (truncated/misbehaving
    // peer), the consumer's ResponseStream simply ends without one. Pre-existing
    // behavior; a future change could send BackendError here to distinguish a
    // clean end-of-turn from a truncated stream.
    //
    // `stream` is dropped here (function return), releasing the &mut borrow of
    // `client` so the caller can move it back over the oneshot.
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Unit tests for the error mapping and the spawn-failure path. The full
    //! round-trip-behind-the-trait proof lives in `tests/backend_trait.rs` (it
    //! needs the in-memory mock agent from `tests/common`).

    use super::*;
    use crate::protocol::JsonRpcError;

    #[test]
    fn error_mapping_follows_the_handoff_table() {
        assert!(matches!(
            map_error(AcpError::Spawn("x".into())),
            BackendError::Spawn { .. }
        ));
        assert!(matches!(
            map_error(AcpError::Transport("x".into())),
            BackendError::Transport { .. }
        ));
        assert!(matches!(
            map_error(AcpError::Protocol("x".into())),
            BackendError::Transport { .. }
        ));
        assert!(matches!(
            map_error(AcpError::AgentExited {
                status: "exit 1".into(),
                stderr: String::new()
            }),
            BackendError::Transport { .. }
        ));
        assert!(matches!(
            map_error(AcpError::Rpc(JsonRpcError {
                code: -32000,
                message: "boom".into(),
                data: None,
            })),
            BackendError::Transport { .. }
        ));
        assert!(matches!(
            map_error(AcpError::Closed),
            BackendError::Terminated
        ));
    }

    #[test]
    fn spawn_error_reason_is_preserved() {
        // The human-readable reason must survive the mapping (Spawn → Spawn).
        let BackendError::Spawn { reason } = map_error(AcpError::Spawn("binary missing".into()))
        else {
            panic!("expected Spawn");
        };
        assert_eq!(reason, "binary missing");
    }

    #[tokio::test]
    async fn spawn_of_missing_binary_is_backend_spawn_error() {
        // `AcpBackend::spawn` on a nonexistent program → BackendError::Spawn.
        let backend = AcpBackend::new(
            "definitely-not-a-real-acp-agent-binary-xyz",
            vec!["--acp".into()],
        );
        let config = SessionConfig {
            working_dir: std::env::temp_dir(),
            system_prompt: "test".into(),
            mode: None,
            model: None,
            effort: None,
            extra: None,
        };
        // The Ok variant (`Box<dyn AgentSession>`) is not Debug, so match rather
        // than `unwrap_err()`.
        match backend.spawn(config).await {
            Err(BackendError::Spawn { .. }) => {}
            Err(other) => panic!("expected Spawn, got {other:?}"),
            Ok(_) => panic!("spawning a missing binary must not succeed"),
        }
    }

    #[test]
    fn command_for_carries_program_args_env_and_working_dir() {
        let backend = AcpBackend::new("gemini", vec!["--acp".into(), "--quiet".into()])
            .env("FOO", "bar")
            .env("BAZ", "qux");
        let command = backend.command_for(PathBuf::from("/work/dir"));

        assert_eq!(command.program, PathBuf::from("gemini"));
        assert_eq!(command.args, vec!["--acp".to_string(), "--quiet".into()]);
        assert_eq!(command.working_dir, PathBuf::from("/work/dir"));
        assert_eq!(
            command.env,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn compose_first_turn_prepends_only_once() {
        // We can't build a real AcpClient without a transport here, so test the
        // prompt-composition logic directly on a session whose client is None
        // (compose_first_turn does not touch the client).
        let mut session = AcpSession {
            client: None,
            pending_return: None,
            system_prompt: Some("SYS".to_string()),
            capabilities: None,
        };
        assert_eq!(session.compose_first_turn("turn1".into()), "SYS\n\nturn1");
        // The prompt is consumed; later turns are verbatim.
        assert_eq!(session.compose_first_turn("turn2".into()), "turn2");
    }

    #[test]
    fn compose_first_turn_without_system_prompt_is_verbatim() {
        let mut session = AcpSession {
            client: None,
            pending_return: None,
            system_prompt: None,
            capabilities: None,
        };
        assert_eq!(session.compose_first_turn("hello".into()), "hello");
    }

    /// Gateway-threading unit test (task `gateway-threading`).
    ///
    /// Proves that the `AcpBackend::with_audit_sink` → `command_for` →
    /// `AcpCommand.audit_sink` injection seam reaches the transport:
    ///
    /// 1. A capturing sink is injected via `AcpBackend::with_audit_sink`.
    /// 2. `command_for` carries the *exact same* `Arc` (`ptr_eq`).
    /// 3. `spawn_with_transport` builds a client using the command's policy and
    ///    sink (the same path as real `spawn`, minus the subprocess).
    /// 4. A mock peer sends `session/request_permission` mid-turn.
    /// 5. After the turn, the capturing sink holds exactly one `Allow` entry.
    ///
    /// This is an in-crate test so it can reach the private `command_for`.
    #[tokio::test]
    async fn gateway_threading_audit_sink_flows_through_backend_command_to_transport() {
        use futures::StreamExt as _;
        use makina_core::backend::AgentSession;
        use makina_core::governance::{AuditDecision, AuditEntry, AuditSink};
        use serde_json::json;
        use std::sync::Mutex;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        // ── sentinel id for the injected permission request ─────────────────
        // Distinct from the normal request id sequence (0, 1, 2 …) so the mock
        // peer and the transport both refer to the same well-known value.
        // NOTE: this is a deliberate twin of the same sentinel in
        // `tests/common/mod.rs`; the in-crate test cannot reach that module.
        const PERMISSION_REQUEST_ID: u64 = 9999;

        // ── capturing sink ──────────────────────────────────────────────────
        // Deliberate twin of `CapturingAuditSink` in `tests/backend_trait.rs`:
        // the in-crate test cannot use `tests/common/` so it defines its own.
        #[derive(Clone, Default)]
        struct CapturingSink {
            entries: Arc<Mutex<Vec<AuditEntry>>>,
        }
        impl AuditSink for CapturingSink {
            fn record(&self, entry: AuditEntry) {
                self.entries.lock().unwrap().push(entry);
            }
        }

        let sink = CapturingSink::default();
        let entries_handle = Arc::clone(&sink.entries);
        let sink_arc: Arc<dyn AuditSink> = Arc::new(sink);

        // ── backend with injected sink ──────────────────────────────────────
        let worktree = std::env::temp_dir().join("makina-backend-unit-perm-test");
        // Best-effort: ensure the directory exists so WorktreePolicy can stat it
        // if it ever needs to, keeping the test deterministic regardless.
        let _ = std::fs::create_dir_all(&worktree);
        let backend =
            AcpBackend::new("echo", vec!["--acp".into()]).with_audit_sink(Arc::clone(&sink_arc));

        // Prove the Arc flows into the command (same pointer, not a copy).
        let command = backend.command_for(worktree.clone());
        assert!(
            Arc::ptr_eq(&sink_arc, &command.audit_sink),
            "command_for must carry the exact Arc injected into the backend"
        );
        drop(command); // we'll use spawn_with_transport below

        // ── mock peer ───────────────────────────────────────────────────────
        let session_id = "sess-gw-unit";
        let (client_io, peer_io) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (peer_read, mut peer_write) = tokio::io::split(peer_io);

        let peer = tokio::spawn(async move {
            let mut lines = BufReader::new(peer_read).lines();

            macro_rules! send {
                ($v:expr) => {{
                    let mut bytes = serde_json::to_vec(&$v).unwrap();
                    bytes.push(b'\n');
                    peer_write.write_all(&bytes).await.unwrap();
                    peer_write.flush().await.unwrap();
                }};
            }

            // initialize
            let _ = lines.next_line().await.unwrap();
            send!(json!({
                "jsonrpc": "2.0", "id": 0,
                "result": { "protocolVersion": 1, "agentCapabilities": {},
                            "authMethods": [], "agentInfo": { "name": "mock", "version": "0" } }
            }));

            // session/new
            let _ = lines.next_line().await.unwrap();
            send!(json!({ "jsonrpc": "2.0", "id": 1,
                          "result": { "sessionId": session_id } }));

            // session/prompt: read it
            let _ = lines.next_line().await.unwrap();

            // inject permission request before the text chunk
            send!(json!({
                "jsonrpc": "2.0", "id": PERMISSION_REQUEST_ID,
                "method": "session/request_permission",
                "params": {
                    "sessionId": session_id,
                    "options": [
                        { "optionId": "proceed_always", "name": "Always", "kind": "allow_always" },
                        { "optionId": "proceed_once",   "name": "Allow",  "kind": "allow_once"  }
                    ],
                    "toolCall": {
                        "toolCallId": "write_file__unit_test_1",
                        "title": "Write file"
                    }
                }
            }));

            // read the client's permission response
            if let Ok(Some(resp_line)) = lines.next_line().await {
                let resp: serde_json::Value = serde_json::from_str(resp_line.trim()).unwrap();
                assert_eq!(
                    resp["id"], PERMISSION_REQUEST_ID,
                    "permission response id mismatch"
                );
            }

            // send one text chunk and complete
            send!(json!({
                "jsonrpc": "2.0", "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": { "sessionUpdate": "agent_message_chunk",
                                "content": { "type": "text", "text": "unit work done" } }
                }
            }));
            send!(json!({ "jsonrpc": "2.0", "id": 2,
                          "result": { "stopReason": "end_turn" } }));
        });

        // ── connect through the backend seam, not bare with_transport ───────
        let client = backend
            .spawn_with_transport(client_read, client_write, worktree)
            .await
            .expect("spawn_with_transport handshake must succeed");

        // ── drive one full turn through the trait ───────────────────────────
        let mut session: Box<dyn AgentSession> = Box::new(AcpSession::from_client(client, ""));
        let stream = session
            .prompt(makina_core::backend::Prompt::new("do the unit work"))
            .await
            .expect("prompt accepted");

        let mut text = String::new();
        let mut completes = 0usize;
        let mut stream = stream;
        while let Some(item) = stream.next().await {
            match item.expect("no error expected") {
                makina_core::backend::ResponseEvent::TextChunk { text: chunk } => {
                    text.push_str(&chunk)
                }
                // Side-channel events do not contribute to the assembled answer.
                makina_core::backend::ResponseEvent::ThoughtChunk { .. }
                | makina_core::backend::ResponseEvent::ToolCall { .. }
                | makina_core::backend::ResponseEvent::ToolCallUpdate { .. }
                | makina_core::backend::ResponseEvent::CurrentModeUpdate { .. } => {}
                makina_core::backend::ResponseEvent::TurnComplete { .. } => completes += 1,
            }
        }
        assert_eq!(text, "unit work done");
        assert_eq!(completes, 1);
        session.terminate().await.expect("terminate ok");
        peer.await.expect("mock peer task panicked");

        // ── audit assertion: the backend-threaded sink recorded the entry ───
        let recorded = entries_handle.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "exactly one AuditEntry must be recorded through the backend→command path"
        );
        assert_eq!(
            recorded[0].decision,
            AuditDecision::Allow,
            "WorktreePolicy auto-allows inside the worktree"
        );
        assert_eq!(
            recorded[0].option_id.as_deref(),
            Some("proceed_once"),
            "WorktreePolicy must select the allow_once option"
        );
    }
}
