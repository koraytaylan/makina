//! The high-level ACP client: spawn → connect → prompt → teardown.
//!
//! [`AcpClient`] is the public surface task 15 (`acp-backend-impl`) wraps to
//! implement `makina_core::backend::AgentBackend` / `AgentSession`. It owns the
//! agent subprocess and a [`Transport`], drives the `initialize`/`session/new`
//! handshake, and turns a `session/prompt` into an incremental stream of
//! [`AcpResponseChunk`]s assembled from `session/update` notifications.
//!
//! # Lifecycle
//!
//! ```text
//!  AcpClient::connect(AcpCommand)        // spawn subprocess + initialize + session/new
//!        │
//!        ▼
//!  client.prompt("…") ──► PromptStream ──► [Text|Thought|ToolCall|…, …, TurnComplete]
//!        │
//!        ▼ client.shutdown()  (or drop)   // kill subprocess, abort reader task
//! ```
//!
//! # Testability
//!
//! The protocol logic is decoupled from process spawning. [`AcpClient::connect`]
//! spawns a real subprocess, but [`AcpClient::with_transport`] accepts any
//! [`Transport`] over a generic byte stream, so the entire handshake + prompt
//! exchange can be exercised over an in-memory [`tokio::io::duplex`] pipe with a
//! mock agent — no binary required (see the crate's `tests/`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use futures::Stream;
use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};

use std::sync::Arc;

use makina_core::governance::NoopAuditSink;

use crate::error::{AcpError, Result};
use crate::permission::WorktreePolicy;
use crate::protocol::{
    self, AuthMethod, ContentBlock, ContentChunk, Implementation, InitializeParams,
    InitializeResult, NewSessionParams, NewSessionResult, PromptParams, PromptResult,
    SessionUpdate, StopReason,
};
use crate::transport::Transport;

/// Boxed write half used by the production (subprocess) transport, so
/// [`AcpClient`] is not generic over the stream type.
type BoxedWriter = Pin<Box<dyn AsyncWrite + Send>>;

/// Default client identity reported to the agent during `initialize`.
const CLIENT_NAME: &str = "makina";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Command description ──────────────────────────────────────────────────────────

/// How to launch an ACP agent CLI as a subprocess.
///
/// Follows the **Zed authentication model**: Makina inherits the parent process
/// environment (so the CLI's existing sign-in is reused) and never injects model
/// credentials. Additional env vars set here are *added on top of* the inherited
/// environment, not a replacement.
#[derive(Clone)]
pub struct AcpCommand {
    /// The program to execute (e.g. `npx`, `claude-code-acp`, `gemini`).
    pub program: PathBuf,
    /// Arguments passed to the program.
    pub args: Vec<String>,
    /// Working directory for the subprocess and the session's `cwd`.
    pub working_dir: PathBuf,
    /// Extra environment variables layered on top of the inherited environment.
    pub env: Vec<(String, String)>,
    /// Optional policy override; when `None` a [`WorktreePolicy`] built from
    /// `working_dir` is used at connect time.
    pub(crate) policy: Option<Arc<dyn crate::permission::PermissionPolicy>>,
    /// Audit sink that receives one entry per permission decision.
    /// Defaults to [`makina_core::governance::NoopAuditSink`].
    pub(crate) audit_sink: Arc<dyn makina_core::governance::AuditSink>,
}

// Manual Debug: the Arc<dyn …> impls are not guaranteed Debug.
// `finish_non_exhaustive` signals that `audit_sink` (and the resolved policy)
// are intentionally omitted from the output.
impl std::fmt::Debug for AcpCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpCommand")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("working_dir", &self.working_dir)
            .field("env", &self.env)
            .field("has_policy_override", &self.policy.is_some())
            .finish_non_exhaustive()
    }
}

impl AcpCommand {
    /// Build a command for `program` running in `working_dir`.
    pub fn new(program: impl Into<PathBuf>, working_dir: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            working_dir: working_dir.into(),
            env: Vec::new(),
            policy: None,
            audit_sink: Arc::new(NoopAuditSink),
        }
    }

    /// Append CLI arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Add one extra environment variable (layered on the inherited env).
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Override the permission policy used for `session/request_permission`
    /// requests on this command's session.  When not called the default
    /// [`WorktreePolicy`] is built from `working_dir` at connect time.
    #[must_use]
    pub fn with_policy(mut self, policy: Arc<dyn crate::permission::PermissionPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Set the audit sink that records every permission decision for this
    /// command's session.
    #[must_use]
    pub fn with_audit_sink(mut self, sink: Arc<dyn makina_core::governance::AuditSink>) -> Self {
        self.audit_sink = sink;
        self
    }

    /// Resolve the effective policy: the override if set, otherwise a fresh
    /// [`WorktreePolicy`] scoped to `working_dir`.
    pub(crate) fn effective_policy(&self) -> Arc<dyn crate::permission::PermissionPolicy> {
        match &self.policy {
            Some(p) => Arc::clone(p),
            None => Arc::new(WorktreePolicy::new(self.working_dir.clone())),
        }
    }

    /// Return the `(policy, sink)` pair that any transport built from this
    /// command must use.
    ///
    /// This is the **single authoritative derivation** shared by both
    /// `spawn_transport` (production path) and `AcpBackend::spawn_with_transport`
    /// (test seam), so a change here propagates to both without divergence.
    pub(crate) fn transport_inputs(
        &self,
    ) -> (
        Arc<dyn crate::permission::PermissionPolicy>,
        Arc<dyn makina_core::governance::AuditSink>,
    ) {
        (self.effective_policy(), Arc::clone(&self.audit_sink))
    }
}

// ── Response chunk ───────────────────────────────────────────────────────────────

/// One item produced by a [`PromptStream`].
///
/// A turn yields, in arrival order, zero or more of the following — interleaved
/// as the agent emits them:
/// * [`AcpResponseChunk::Text`] — a fragment of the assistant message
///   (`agent_message_chunk`);
/// * [`AcpResponseChunk::Thought`] — a fragment of the agent's reasoning stream
///   (`agent_thought_chunk`);
/// * [`AcpResponseChunk::ToolCall`] — the agent announced a tool call;
/// * [`AcpResponseChunk::ToolCallUpdate`] — a status/result update for a
///   previously-announced tool call;
///
/// followed by exactly one terminal [`AcpResponseChunk::TurnComplete`]. The
/// non-text variants are a side channel: they do not contribute to the assembled
/// assistant text, but they are delivered (not dropped) so downstream consumers
/// can render thoughts and tool activity. This mirrors the shape of
/// `makina_core::backend::ResponseEvent` so task 15's adapter is a thin mapping.
///
/// Every field is a `String`/`Option<String>` so the enum stays `Clone + Eq`;
/// richer per-tool payloads (raw input, content, …) are intentionally not
/// carried here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpResponseChunk {
    /// A fragment of the agent's assistant message text (never empty).
    Text(String),
    /// A fragment of the agent's "thinking"/reasoning stream (never empty).
    Thought(String),
    /// The agent announced a tool call. `status` defaults to `"pending"` when the
    /// agent omits it; `kind` is the optional semantic category (e.g. `"execute"`).
    ToolCall {
        /// Stable id correlating this call with later [`AcpResponseChunk::ToolCallUpdate`]s.
        id: String,
        /// Human-readable title (empty when the agent omits it).
        title: String,
        /// Optional semantic kind (e.g. `"execute"`, `"edit"`).
        kind: Option<String>,
        /// Lifecycle status (`"pending"` when the agent omits it).
        status: String,
    },
    /// A status/result update for a previously-announced tool call.
    ToolCallUpdate {
        /// The id of the [`AcpResponseChunk::ToolCall`] this updates.
        id: String,
        /// Updated lifecycle status, if the update carried one.
        status: Option<String>,
        /// Updated title, if the update carried one.
        title: Option<String>,
    },
    /// The agent autonomously changed its operating mode (`current_mode_update`).
    ///
    /// Side channel: does not contribute to the assembled assistant text.
    CurrentModeUpdate {
        /// The id of the mode the agent switched to.
        current_mode_id: String,
    },
    /// The turn finished; carries the agent's stop reason.
    TurnComplete(StopReason),
}

// ── The client ───────────────────────────────────────────────────────────────────

/// A connected ACP client over a subprocess (or, in tests, any transport).
///
/// Holds the agent's child-process handle (if spawned) and the JSON-RPC
/// [`Transport`]. A session has been created during [`connect`](Self::connect);
/// call [`prompt`](Self::prompt) to run a turn and [`shutdown`](Self::shutdown)
/// (or drop) to tear everything down.
pub struct AcpClient {
    /// The agent subprocess, when this client owns one. `None` for transports
    /// injected directly in tests.
    child: Option<Child>,
    /// Process-group id of the spawned agent (its own pid, since it is the
    /// group leader via `process_group(0)`). `None` when no subprocess is owned
    /// (test transports) or the child's pid was already taken. Used to
    /// **group-kill** the agent and every descendant it forked, not just the
    /// direct child.
    pgid: Option<u32>,
    /// JSON-RPC transport over the agent's stdio.
    transport: Transport<BoxedWriter>,
    /// The session created at connect time.
    session_id: String,
    /// Negotiated protocol version reported by the agent.
    protocol_version: u16,
    /// Agent name/version from `initialize`, if provided.
    agent_info: Option<Implementation>,
    /// Authentication methods advertised by the agent in the `initialize` response.
    ///
    /// Retained for observability (Zed model: Makina never calls `authenticate`).
    /// An empty list means the agent requires no auth or is already authenticated.
    auth_methods: Vec<AuthMethod>,
    /// Session modes advertised by the agent (if supported).
    modes: Option<protocol::SessionModeState>,
    /// Config options advertised by the agent (e.g., model, effort).
    config_options: Vec<protocol::ConfigOption>,
    /// Set once [`shutdown`](Self::shutdown) has run, to make it idempotent.
    closed: bool,
}

// Manual `Debug` (the boxed writer in `Transport` is not `Debug`); prints the
// useful client state without exposing the transport internals.
impl std::fmt::Debug for AcpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpClient")
            .field("has_subprocess", &self.child.is_some())
            .field("pgid", &self.pgid)
            .field("session_id", &self.session_id)
            .field("protocol_version", &self.protocol_version)
            .field("agent_info", &self.agent_info)
            .field("auth_methods_count", &self.auth_methods.len())
            .field("modes_available", &self.modes.is_some())
            .field("config_options_count", &self.config_options.len())
            .field("closed", &self.closed)
            .finish()
    }
}

impl AcpClient {
    /// Spawn the agent described by `command`, perform the `initialize`
    /// handshake, and create a session.
    ///
    /// On success the returned client has an active session and is ready for
    /// [`prompt`](Self::prompt). Spawn failures surface as [`AcpError::Spawn`];
    /// handshake/transport failures as [`AcpError::Transport`] /
    /// [`AcpError::Protocol`] / [`AcpError::Rpc`].
    ///
    /// The subprocess inherits the parent environment (Zed auth model); any
    /// `command.env` entries are layered on top.
    pub async fn connect(command: AcpCommand) -> Result<Self> {
        let (child, pgid, transport) = spawn_transport(&command)?;
        // Build atop the generic constructor so spawn + protocol stay separable.
        let mut client = Self::from_parts(Some(child), pgid, transport);
        client.handshake(&command.working_dir).await?;
        Ok(client)
    }

    /// Build a client over an arbitrary [`Transport`] **and** run the
    /// `initialize` + `session/new` handshake against it.
    ///
    /// This is the seam used by tests: pass a transport built over a
    /// [`tokio::io::duplex`] pipe whose other end is a mock agent. No subprocess
    /// is involved, so the full protocol exchange is exercised deterministically.
    ///
    /// `policy` and `audit_sink` are injected into the transport so that
    /// `session/request_permission` requests are answered and audited exactly as
    /// they would be in production.  Pass `None` for `policy` to get the default
    /// [`WorktreePolicy`] scoped to `cwd`; pass `None` for `audit_sink` for a
    /// silent [`makina_core::governance::NoopAuditSink`].
    pub async fn with_transport<R, W>(
        reader: R,
        writer: W,
        cwd: impl AsRef<Path>,
        policy: Option<Arc<dyn crate::permission::PermissionPolicy>>,
        audit_sink: Option<Arc<dyn makina_core::governance::AuditSink>>,
    ) -> Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let cwd = cwd.as_ref().to_path_buf();
        let policy: Arc<dyn crate::permission::PermissionPolicy> =
            policy.unwrap_or_else(|| Arc::new(WorktreePolicy::new(cwd.clone())));
        let sink: Arc<dyn makina_core::governance::AuditSink> =
            audit_sink.unwrap_or_else(|| Arc::new(NoopAuditSink));
        let transport = Transport::new(
            reader,
            Box::pin(writer) as BoxedWriter,
            policy,
            cwd.clone(),
            sink,
        );
        let mut client = Self::from_parts(None, None, transport);
        client.handshake(&cwd).await?;
        Ok(client)
    }

    /// Assemble a not-yet-handshaked client from its parts.
    fn from_parts(
        child: Option<Child>,
        pgid: Option<u32>,
        transport: Transport<BoxedWriter>,
    ) -> Self {
        Self {
            child,
            pgid,
            transport,
            session_id: String::new(),
            protocol_version: 0,
            agent_info: None,
            auth_methods: Vec::new(),
            modes: None,
            config_options: Vec::new(),
            closed: false,
        }
    }

    /// Drive `initialize` then `session/new`, populating session state.
    async fn handshake(&mut self, cwd: &Path) -> Result<()> {
        // 1. initialize — capability negotiation.
        let init_params = InitializeParams {
            protocol_version: protocol::PROTOCOL_VERSION,
            client_capabilities: Default::default(),
            client_info: Implementation {
                name: CLIENT_NAME.to_string(),
                version: CLIENT_VERSION.to_string(),
            },
        };
        let init_value = self
            .transport
            .send_request(protocol::METHOD_INITIALIZE, &init_params)
            .await?;
        let init: InitializeResult = serde_json::from_value(init_value)
            .map_err(|e| AcpError::protocol(format!("invalid initialize result: {e}")))?;
        self.protocol_version = init.protocol_version;
        self.agent_info = init.agent_info;
        self.auth_methods = init.auth_methods;
        // Log advertised auth methods so operators can verify the Zed model is
        // working: a signed-in CLI typically advertises no methods or just notes
        // the mechanism it already used.
        if self.auth_methods.is_empty() {
            tracing::debug!(
                "ACP initialize: agent advertises no authMethods (already authenticated or auth-free)"
            );
        } else {
            let kinds: Vec<&str> = self.auth_methods.iter().map(|m| m.kind.as_str()).collect();
            tracing::debug!(?kinds, "ACP initialize: agent advertises authMethods");
        }

        // 2. session/new — create the session in the requested working dir.
        let new_session_params = NewSessionParams {
            cwd: cwd.to_path_buf(),
            mcp_servers: Vec::new(),
        };
        let session_value = self
            .transport
            .send_request(protocol::METHOD_SESSION_NEW, &new_session_params)
            .await?;
        let session: NewSessionResult = serde_json::from_value(session_value)
            .map_err(|e| AcpError::protocol(format!("invalid session/new result: {e}")))?;
        self.session_id = session.session_id;
        self.modes = session.modes;
        self.config_options = session.config_options;

        Ok(())
    }

    /// The session id assigned by the agent.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The protocol version the agent agreed to.
    pub fn protocol_version(&self) -> u16 {
        self.protocol_version
    }

    /// The agent's reported name/version, if it sent any.
    pub fn agent_info(&self) -> Option<&Implementation> {
        self.agent_info.as_ref()
    }

    /// The authentication methods the agent advertised in the `initialize` response.
    ///
    /// This is an observability accessor only. Per the **Zed auth-inheritance model**,
    /// Makina **never calls the ACP `authenticate` method** — the agent CLI is expected
    /// to already be signed in via its own flow (e.g. `gemini` oauth, `claude` login)
    /// before Makina spawns it. Makina then inherits the CLI's session via the inherited
    /// environment; no credentials are passed through Makina.
    ///
    /// An empty slice means the agent requires no explicit auth or is already
    /// authenticated. A non-empty slice is diagnostic information; operators should
    /// sign the CLI in via its own flow (e.g. `gemini auth login`) rather than
    /// expecting Makina to drive an auth flow.
    ///
    /// # Misconfiguration signal
    ///
    /// If the agent surfaces an error during `initialize` or `session/prompt` that
    /// suggests it is not authenticated, check that the CLI is signed in independently
    /// (`gemini auth login`, `claude` login, etc.) and re-run. The error will surface
    /// as [`AcpError::Rpc`] or [`AcpError::AgentExited`]; the agent's stderr
    /// (forwarded to `tracing` under the `acp_agent` target → the run/task log
    /// file and the TUI error pane) typically contains the human-readable reason.
    pub fn auth_methods(&self) -> &[crate::protocol::AuthMethod] {
        &self.auth_methods
    }

    /// Get the session modes advertised by the agent (if any).
    pub fn modes(&self) -> Option<&protocol::SessionModeState> {
        self.modes.as_ref()
    }

    /// Get the configuration options advertised by the agent (if any).
    pub fn config_options(&self) -> &[protocol::ConfigOption] {
        &self.config_options
    }

    /// Request the agent to switch to a different mode.
    ///
    /// Sends a `session/set_mode` request to the agent. Returns `Ok(())` if the
    /// request was sent successfully; the agent's response is handled asynchronously.
    pub async fn set_mode(&mut self, mode_id: &str) -> Result<()> {
        if self.closed {
            return Err(AcpError::Closed);
        }
        if let Some(err) = self.transport.sender().ended_error() {
            return Err(err);
        }

        let params = protocol::SetModeParams {
            session_id: self.session_id.clone(),
            mode_id: mode_id.to_string(),
        };

        let sender = self.transport.sender().clone();
        let _response: serde_json::Value = sender
            .send_request(protocol::METHOD_SESSION_SET_MODE, &params)
            .await?;

        Ok(())
    }

    /// Set a configuration option on the agent.
    ///
    /// Sends a `session/set_config_option` request to the agent. Returns `Ok(())`
    /// if the request was sent successfully; the agent's response is handled
    /// asynchronously. The `value` parameter is a JSON value that matches the
    /// option's expected type.
    pub async fn set_config_option(
        &mut self,
        option_id: &str,
        value: serde_json::Value,
    ) -> Result<()> {
        if self.closed {
            return Err(AcpError::Closed);
        }
        if let Some(err) = self.transport.sender().ended_error() {
            return Err(err);
        }

        let params = protocol::SetConfigOptionParams {
            session_id: self.session_id.clone(),
            option_id: option_id.to_string(),
            value,
        };

        let sender = self.transport.sender().clone();
        let _response: serde_json::Value = sender
            .send_request(protocol::METHOD_SESSION_SET_CONFIG_OPTION, &params)
            .await?;

        Ok(())
    }

    /// Convenience: set the agent's model by locating the `model`-category
    /// config option and applying it. Returns `Ok(false)` when the agent
    /// advertised no `model` option (nothing to set).
    pub async fn set_model(&mut self, model: &str) -> Result<bool> {
        self.set_option_in_category("model", serde_json::Value::String(model.to_string()))
            .await
    }

    /// Convenience: set the agent's reasoning effort by locating the
    /// `thought_level`-category config option. Returns `Ok(false)` when the
    /// agent advertised no such option.
    pub async fn set_effort(&mut self, effort: &str) -> Result<bool> {
        self.set_option_in_category(
            "thought_level",
            serde_json::Value::String(effort.to_string()),
        )
        .await
    }

    /// Locate the advertised config option whose `category` matches and set it.
    async fn set_option_in_category(
        &mut self,
        category: &str,
        value: serde_json::Value,
    ) -> Result<bool> {
        let option_id = self
            .config_options
            .iter()
            .find(|o| o.category == category)
            .map(|o| o.id.clone());
        match option_id {
            Some(id) => {
                self.set_config_option(&id, value).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Send `text` as a single user turn and return an incremental stream of
    /// the agent's response.
    ///
    /// The returned [`PromptStream`] borrows the client for the duration of the
    /// turn; drain it (to [`AcpResponseChunk::TurnComplete`] or an error) before
    /// issuing another prompt. Yields, in arrival order:
    /// * [`AcpResponseChunk::Text`] for each assistant `agent_message_chunk`;
    /// * [`AcpResponseChunk::Thought`] for each `agent_thought_chunk`;
    /// * [`AcpResponseChunk::ToolCall`] / [`AcpResponseChunk::ToolCallUpdate`]
    ///   for `tool_call` / `tool_call_update` lifecycle updates;
    /// * [`AcpResponseChunk::TurnComplete`] once the `session/prompt` response
    ///   arrives;
    /// * an [`AcpError`] if the transport breaks or the agent exits mid-turn,
    ///   after which the stream ends.
    pub fn prompt(&mut self, text: impl Into<String>) -> Result<PromptStream<'_>> {
        if self.closed {
            return Err(AcpError::Closed);
        }
        if let Some(err) = self.transport.sender().ended_error() {
            return Err(err);
        }

        let params = PromptParams {
            session_id: self.session_id.clone(),
            prompt: vec![ContentBlock::text(text)],
        };

        // The prompt request runs on a cloned (owned, 'static) sender so the
        // future does not borrow `self`; meanwhile we keep `&mut` access to the
        // notification receiver to interleave chunk delivery. No aliasing.
        let sender = self.transport.sender().clone();
        let response: BoxFuture<'static, Result<PromptResult>> = Box::pin(async move {
            let value = sender
                .send_request(protocol::METHOD_SESSION_PROMPT, &params)
                .await?;
            serde_json::from_value::<PromptResult>(value)
                .map_err(|e| AcpError::protocol(format!("invalid session/prompt result: {e}")))
        });

        Ok(PromptStream {
            transport: &mut self.transport,
            state: StreamState::Streaming(response),
            buffered: VecDeque::new(),
        })
    }

    /// Gracefully terminate the client: kill the agent subprocess (if any) and
    /// stop the reader task.
    ///
    /// Idempotent — calling it again returns `Ok(())`. The subprocess is also
    /// killed on drop, so calling `shutdown` is optional but lets callers await
    /// the kill and observe errors.
    pub async fn shutdown(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;

        if let Some(child) = self.child.as_mut() {
            // Group-kill the whole process group (the agent + every descendant
            // it forked), not just the direct child. SIGTERM first for a clean
            // exit, a short grace, then SIGKILL. Falls back to a direct-child
            // kill when there is no pgid or we're not on Unix.
            group_kill(self.pgid, child);
            // Give the group a moment to react to SIGTERM before escalating.
            tokio::time::sleep(KILL_GRACE).await;
            group_kill_force(self.pgid, child);
            // Reap so no zombie lingers. Ignore the status — we are tearing down.
            let _ = child.wait().await;
        }
        // We have reaped our own group; drop it from the process-wide reaper so
        // a later `kill_all_agents()` does not re-kill a (possibly recycled) pgid.
        if let Some(pgid) = self.pgid {
            crate::reaper::deregister(pgid as i32);
        }
        Ok(())
    }
}

/// Grace period between SIGTERM and SIGKILL when group-killing an agent.
const KILL_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// Send `SIGTERM` to the agent's process group (`pgid`), so the agent and every
/// descendant it forked receive it. Falls back to a direct-child kill when
/// `pgid` is `None` or on non-Unix. Best-effort: a group that already exited
/// yields a benign error we ignore.
fn group_kill(pgid: Option<u32>, child: &mut Child) {
    #[cfg(unix)]
    {
        if let Some(pgid) = pgid {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pgid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            return;
        }
    }
    let _ = pgid; // silence unused on non-Unix
    let _ = child.start_kill();
}

/// Escalate to `SIGKILL` on the agent's process group after the grace period.
/// Same fallback semantics as [`group_kill`].
fn group_kill_force(pgid: Option<u32>, child: &mut Child) {
    #[cfg(unix)]
    {
        if let Some(pgid) = pgid {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pgid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            return;
        }
    }
    let _ = pgid; // silence unused on non-Unix
    let _ = child.start_kill();
}

/// Best-effort synchronous teardown if the caller never called
/// [`AcpClient::shutdown`]. Tokio's `Child` is configured (below) to kill the
/// process when its handle drops, so this guarantees no leaked subprocess.
impl Drop for AcpClient {
    fn drop(&mut self) {
        // Group-kill the agent's process group so descendants it forked die too,
        // not just the direct child. `Drop` is synchronous, so we send `SIGKILL`
        // straight away (no grace period). `kill_on_drop(true)` on the `Child`
        // remains the last-resort direct-child backstop, and the `Transport`'s
        // own `Drop` aborts the reader task.
        if let Some(child) = self.child.as_mut() {
            group_kill_force(self.pgid, child);
        }
        // Deregister from the process-wide reaper after our own group-kill, so a
        // later `kill_all_agents()` does not re-kill a (possibly recycled) pgid.
        if let Some(pgid) = self.pgid {
            crate::reaper::deregister(pgid as i32);
        }
    }
}

// ── Subprocess spawning ──────────────────────────────────────────────────────────

/// Spawn the agent subprocess with piped stdio and wrap its stdout/stdin in a
/// [`Transport`]. Stderr is captured and forwarded line-by-line to this
/// process's stderr (useful for surfacing agent diagnostics / auth prompts).
fn spawn_transport(command: &AcpCommand) -> Result<(Child, Option<u32>, Transport<BoxedWriter>)> {
    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args)
        .current_dir(&command.working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Reap the process if the handle is dropped without an explicit kill —
        // this is the leak-prevention guarantee.
        .kill_on_drop(true);
    // Put the agent in its own fresh process group (pgid == its pid) so that it
    // becomes the group leader and every descendant it forks shares the group.
    // That lets us reap the whole tree with `killpg`, not just the direct child.
    #[cfg(unix)]
    cmd.process_group(0);
    // Zed auth model: inherit the parent environment; only *add* extras.
    for (key, value) in &command.env {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn().map_err(|e| {
        AcpError::Spawn(format!(
            "could not spawn `{}`: {e}",
            command.program.display()
        ))
    })?;

    // The agent is its own process-group leader, so its pgid equals its pid.
    let pgid = child.id();

    // Register the live process group in the process-wide reaper so a panic or
    // out-of-band signal can still reap it (see `crate::reaper`). The owning
    // client deregisters it after its own group-kill in `shutdown`/`Drop`.
    if let Some(pgid) = pgid {
        crate::reaper::register(pgid as i32);
    }

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| AcpError::Spawn("child stdin was not captured".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AcpError::Spawn("child stdout was not captured".into()))?;

    // Forward the agent's stderr into `tracing` (never raw stderr) so operators
    // can see auth/errors in the run/task log + TUI error pane without dumping
    // over the live frame. This task ends when stderr closes (process exit); it
    // holds no client state.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(forward_stderr(stderr));
    }

    let reader = Box::pin(stdout) as Pin<Box<dyn AsyncRead + Send>>;
    let writer = Box::pin(stdin) as BoxedWriter;
    let cwd = command.working_dir.clone();
    // Use the single shared derivation so the production and test paths are
    // always in sync (see `AcpCommand::transport_inputs`).
    let (policy, sink) = command.transport_inputs();
    let transport = Transport::new(reader, writer, policy, cwd, sink);
    Ok((child, pgid, transport))
}

/// Drain the child's stderr line-by-line into `tracing`.
///
/// Each line is emitted as a `tracing::info!` event under the `acp_agent`
/// target so plan-0003's subscriber routes it to the per-run/per-task log file
/// and the TUI error pane. It must **not** be written to this process's stderr:
/// the ACP backend runs while the TUI's ratatui frame is live, so a raw
/// `eprintln!` would dump over the alternate screen and corrupt the UI.
async fn forward_stderr(stderr: tokio::process::ChildStderr) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::info!(target: "acp_agent", "{line}");
    }
}

// ── Prompt stream ────────────────────────────────────────────────────────────────

/// State machine driving a single prompt turn to completion.
enum StreamState {
    /// Awaiting the `session/prompt` response while forwarding chunks. The
    /// boxed future is `'static` (built from a cloned sender), so it does not
    /// alias the borrowed transport.
    Streaming(BoxFuture<'static, Result<PromptResult>>),
    /// Response received; emit `TurnComplete` next, then finish.
    Completing(StopReason),
    /// Terminal: the stream has ended.
    Done,
}

/// An incremental stream of one prompt turn's response.
///
/// Implements [`futures::Stream`]; consume with `StreamExt` (`while let Some(..)
/// = stream.next().await`). Borrows the [`AcpClient`] until the turn ends. See
/// [`AcpClient::prompt`].
pub struct PromptStream<'a> {
    /// Borrowed transport — its notification receiver feeds text chunks.
    transport: &'a mut Transport<BoxedWriter>,
    /// Turn progress.
    state: StreamState,
    /// Response chunks observed after the prompt response resolved, queued to
    /// emit before `TurnComplete` (insurance against any chunk/response
    /// reordering). Holds the full rich variant set, not just text, so thoughts
    /// and tool-call updates that land late are not lost.
    buffered: VecDeque<AcpResponseChunk>,
}

impl PromptStream<'_> {
    /// Pull the assistant message text out of an `agent_message_chunk` update, if
    /// it carries non-empty text. Returns `None` for every other update kind.
    ///
    /// Kept as the single place that knows how to read an assistant text chunk;
    /// [`Self::classify_update`] reuses it for the [`AcpResponseChunk::Text`] case.
    fn extract_text(chunk: &ContentChunk) -> Option<String> {
        chunk
            .content
            .as_text()
            .map(str::to_string)
            .filter(|t| !t.is_empty())
    }

    /// Classify an already-delivered notification into the rich response chunk it
    /// should produce, if any.
    ///
    /// Assistant/thought text chunks become [`AcpResponseChunk::Text`] /
    /// [`AcpResponseChunk::Thought`] (empty text is dropped); tool-call lifecycle
    /// updates become [`AcpResponseChunk::ToolCall`] /
    /// [`AcpResponseChunk::ToolCallUpdate`]; autonomous mode switches become
    /// [`AcpResponseChunk::CurrentModeUpdate`]. User-message echoes and unmodelled
    /// kinds (`SessionUpdate::Other`) return `None` and are skipped.
    fn classify_update(update: SessionUpdate) -> Option<AcpResponseChunk> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                Self::extract_text(&chunk).map(AcpResponseChunk::Text)
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                Self::extract_text(&chunk).map(AcpResponseChunk::Thought)
            }
            SessionUpdate::ToolCall(tc) => Some(AcpResponseChunk::ToolCall {
                id: tc.tool_call_id,
                title: tc.title.unwrap_or_default(),
                kind: tc.kind,
                status: tc.status.unwrap_or_else(|| "pending".to_string()),
            }),
            SessionUpdate::ToolCallUpdate(u) => Some(AcpResponseChunk::ToolCallUpdate {
                id: u.tool_call_id,
                status: u.status,
                title: u.title,
            }),
            SessionUpdate::CurrentModeUpdate { current_mode_id } => {
                Some(AcpResponseChunk::CurrentModeUpdate { current_mode_id })
            }
            SessionUpdate::UserMessageChunk(_) | SessionUpdate::Other => None,
        }
    }
}

impl Stream for PromptStream<'_> {
    type Item = Result<AcpResponseChunk>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            // Emit any buffered post-response chunks before TurnComplete.
            if let Some(chunk) = this.buffered.pop_front() {
                return Poll::Ready(Some(Ok(chunk)));
            }

            match &mut this.state {
                StreamState::Streaming(response) => {
                    // 1. Prefer delivering a ready notification chunk first, so
                    //    text/thoughts/tools stream out incrementally as they arrive.
                    match this.transport.notifications_mut().poll_recv(cx) {
                        Poll::Ready(Some(notif)) => {
                            if let Some(chunk) = Self::classify_update(notif.update) {
                                return Poll::Ready(Some(Ok(chunk)));
                            }
                            // Update kind with no chunk (user echo / unmodelled):
                            // loop to check for more.
                            continue;
                        }
                        Poll::Ready(None) => {
                            // Notification channel closed (agent disconnected).
                            // Fall through to poll the response future, which will
                            // now resolve with a terminal error because the reader
                            // task called `shared.shutdown()` on EOF/error, waking it.
                        }
                        Poll::Pending => {}
                    }

                    // 2. Poll the prompt response. Only reached when no chunk is
                    //    immediately buffered.
                    match response.as_mut().poll(cx) {
                        Poll::Ready(Ok(result)) => {
                            // Drain any chunks that landed before the response so
                            // none are dropped (text AND rich side-channel kinds),
                            // then move to Completing.
                            while let Ok(notif) = this.transport.notifications_mut().try_recv() {
                                if let Some(chunk) = Self::classify_update(notif.update) {
                                    this.buffered.push_back(chunk);
                                }
                            }
                            this.state = StreamState::Completing(result.stop_reason);
                            continue;
                        }
                        Poll::Ready(Err(e)) => {
                            this.state = StreamState::Done;
                            return Poll::Ready(Some(Err(e)));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                StreamState::Completing(_) => {
                    // Take the stop reason and finish.
                    let StreamState::Completing(reason) =
                        std::mem::replace(&mut this.state, StreamState::Done)
                    else {
                        unreachable!("state checked in match arm")
                    };
                    return Poll::Ready(Some(Ok(AcpResponseChunk::TurnComplete(reason))));
                }
                StreamState::Done => return Poll::Ready(None),
            }
        }
    }
}

#[cfg(test)]
mod current_mode_tests {
    use super::*;

    #[test]
    fn classify_update_maps_current_mode_update() {
        // Regression: `current_mode_update` notifications used to be dropped
        // (classified to `None`). They must now surface as a `CurrentModeUpdate`
        // chunk so autonomous mode switches reach the TUI.
        let chunk = PromptStream::classify_update(SessionUpdate::CurrentModeUpdate {
            current_mode_id: "code".to_string(),
        });
        assert_eq!(
            chunk,
            Some(AcpResponseChunk::CurrentModeUpdate {
                current_mode_id: "code".to_string(),
            })
        );
    }
}
