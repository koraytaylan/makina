//! Newline-delimited JSON-RPC 2.0 transport over a generic byte stream.
//!
//! This module is the **testable core** of the client. It is deliberately
//! agnostic about *where* the bytes come from: it drives the protocol over any
//! [`AsyncRead`] + [`AsyncWrite`] pair. In production those are the child
//! process's stdout/stdin (see [`crate::client`]); in tests they are the two
//! ends of a [`tokio::io::duplex`] pipe with a mock ACP agent on the other side.
//! No subprocess is required to exercise the full request/response/notification
//! machinery.
//!
//! # Model
//!
//! * A background **reader task** owns the read half. For each newline-delimited
//!   line it decodes a JSON-RPC message and routes it:
//!   * **responses** → matched by `id` to the waiting caller via a `oneshot`;
//!   * **`session/update` notifications** → forwarded on an unbounded channel;
//!   * **other notifications / inbound requests** → ignored (Makina's MVP turn
//!     does not negotiate tools or permissions);
//!   * **malformed lines** → terminate the transport with a protocol error.
//! * The write half is guarded by a mutex so concurrent sends stay frame-aligned
//!   (each message is one line terminated by `\n`).
//! * When the reader ends (EOF or I/O error) it records a terminal status and
//!   wakes every pending request, so no caller blocks forever.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use makina_core::governance::{AuditDecision, AuditEntry, AuditSink, PolicyInfo, ToolRef};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::{AcpError, Result};
use crate::permission::{PermissionPolicy, PermissionRequestContext};
use crate::protocol::{
    IncomingKind, IncomingMessage, JsonRpcError, METHOD_SESSION_REQUEST_PERMISSION,
    OutgoingErrorResponse, OutgoingNotification, OutgoingRequest, OutgoingResponse,
    PermissionOutcome, PermissionResponse, RequestPermissionParams, SessionNotificationParams,
};

/// Why the reader task stopped — recorded so late callers get a precise error
/// instead of a generic "channel closed".
#[derive(Debug, Clone)]
enum ReaderEnd {
    /// The read side reached clean EOF (the agent closed stdout / exited).
    Eof,
    /// A transport I/O error occurred while reading.
    Io(String),
}

impl ReaderEnd {
    /// Turn the terminal reason into the error a pending/new request observes.
    fn to_error(&self) -> AcpError {
        match self {
            ReaderEnd::Eof => AcpError::AgentExited {
                status: String::new(),
                stderr: String::new(),
            },
            ReaderEnd::Io(msg) => AcpError::Transport(msg.clone()),
        }
    }
}

/// State shared between the public [`Transport`] handle and the reader task.
struct Shared {
    /// Outstanding requests awaiting a response, keyed by JSON-RPC id.
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value>>>>,
    /// Set once when the reader task stops; subsequent requests fail fast.
    ended: Mutex<Option<ReaderEnd>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            ended: Mutex::new(None),
        }
    }

    /// Record the terminal reason (first one wins) and wake all pending callers.
    fn shutdown(&self, reason: ReaderEnd) {
        {
            let mut ended = self.ended.lock().expect("ended mutex poisoned");
            if ended.is_none() {
                *ended = Some(reason.clone());
            }
        }
        let mut pending = self.pending.lock().expect("pending mutex poisoned");
        for (_, tx) in pending.drain() {
            // Receiver may already be gone; ignore send failures.
            let _ = tx.send(Err(reason.to_error()));
        }
    }

    /// The terminal error, if the reader has stopped.
    fn ended_error(&self) -> Option<AcpError> {
        self.ended
            .lock()
            .expect("ended mutex poisoned")
            .as_ref()
            .map(ReaderEnd::to_error)
    }
}

/// Inner state shared by every clone of a [`TransportSender`].
struct SenderInner<W> {
    /// Guarded write half; one line is written per message.
    writer: tokio::sync::Mutex<W>,
    /// Shared pending-request table + terminal status.
    shared: Arc<Shared>,
    /// Monotonic request-id source.
    next_id: Mutex<u64>,
    /// Injected policy used to answer `session/request_permission` requests.
    policy: Arc<dyn PermissionPolicy>,
    /// Session working directory (passed to policy context).
    working_dir: PathBuf,
    /// Sink for audit entries produced on permission decisions.
    audit_sink: Arc<dyn AuditSink>,
}

/// The **send side** of a connected transport: issue requests/notifications and
/// observe disconnection. Cheaply clonable (`Arc`-backed) and `'static`, so a
/// turn driver can own a clone while the [`Transport`] keeps polling
/// notifications — there is no aliasing borrow between the two halves.
pub struct TransportSender<W> {
    inner: Arc<SenderInner<W>>,
}

impl<W> Clone for TransportSender<W> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<W> TransportSender<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    /// Allocate the next request id.
    fn alloc_id(&self) -> u64 {
        let mut id = self.inner.next_id.lock().expect("next_id mutex poisoned");
        let current = *id;
        *id += 1;
        current
    }

    /// Send a JSON-RPC request and await its correlated response payload.
    ///
    /// Returns the raw `result` value on success, or a typed error if the agent
    /// replied with a JSON-RPC error, the connection dropped, or the payload was
    /// malformed.
    pub async fn send_request<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> Result<serde_json::Value> {
        // Fail fast if the reader already terminated.
        if let Some(err) = self.inner.shared.ended_error() {
            return Err(err);
        }

        let id = self.alloc_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self
                .inner
                .shared
                .pending
                .lock()
                .expect("pending mutex poisoned");
            pending.insert(id, tx);
        }

        let request = OutgoingRequest::new(id, method, params);
        if let Err(e) = self.write_message(&request).await {
            // Roll back the pending entry so it cannot leak.
            self.inner
                .shared
                .pending
                .lock()
                .expect("pending mutex poisoned")
                .remove(&id);
            return Err(e);
        }

        // Await the reader task routing our response (or a terminal wake-up).
        match rx.await {
            Ok(result) => result,
            // The sender was dropped without a value — only happens if the
            // reader task panicked. Surface whatever terminal reason we have.
            Err(_) => Err(self
                .inner
                .shared
                .ended_error()
                .unwrap_or_else(|| AcpError::Transport("reader task ended".into()))),
        }
    }

    /// Send a JSON-RPC notification (fire-and-forget; no response expected).
    pub async fn send_notification<P: Serialize>(&self, method: &str, params: P) -> Result<()> {
        if let Some(err) = self.inner.shared.ended_error() {
            return Err(err);
        }
        let notification = OutgoingNotification::new(method, params);
        self.write_message(&notification).await
    }

    /// Send a JSON-RPC success response to an inbound request (e.g. a
    /// `session/request_permission` we decided via the injected policy).
    pub async fn send_response<R: Serialize>(&self, id: u64, result: R) -> Result<()> {
        if let Some(err) = self.inner.shared.ended_error() {
            return Err(err);
        }
        let response = OutgoingResponse::new(id, result);
        self.write_message(&response).await
    }

    /// Send a JSON-RPC error response to an inbound request we do not support.
    /// This guarantees the peer never hangs waiting for a reply.
    pub async fn send_error_response(&self, id: u64, error: JsonRpcError) -> Result<()> {
        if let Some(err) = self.inner.shared.ended_error() {
            return Err(err);
        }
        let response = OutgoingErrorResponse::new(id, error);
        self.write_message(&response).await
    }

    /// The terminal error, if the agent has disconnected.
    pub fn ended_error(&self) -> Option<AcpError> {
        self.inner.shared.ended_error()
    }

    /// Serialise `msg` to one JSON line and write it (newline-terminated).
    async fn write_message<T: Serialize>(&self, msg: &T) -> Result<()> {
        let mut line = serde_json::to_vec(msg).map_err(AcpError::protocol)?;
        line.push(b'\n');
        // Async mutex: the write may await across the pipe, so a blocking
        // std::Mutex would risk holding a guard across an await point.
        let mut writer = self.inner.writer.lock().await;
        writer.write_all(&line).await.map_err(AcpError::transport)?;
        writer.flush().await.map_err(AcpError::transport)?;
        Ok(())
    }
}

/// A connected JSON-RPC transport: a [`TransportSender`] plus the inbound
/// `session/update` notification stream.
///
/// The owning [`crate::AcpClient`] holds this; aborting the background reader
/// task on drop releases the read half.
pub struct Transport<W> {
    /// The clonable send side.
    sender: TransportSender<W>,
    /// Inbound `session/update` notifications from the reader task.
    notifications: mpsc::UnboundedReceiver<SessionNotificationParams>,
    /// Handle to the background reader task (aborted on drop).
    reader_task: Option<JoinHandle<()>>,
}

impl<W> Transport<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    /// Start driving the protocol over `reader`/`writer`.
    ///
    /// The `policy`, `working_dir` and `audit_sink` are threaded to the reader
    /// so that inbound `session/request_permission` requests can be answered
    /// deterministically and audited without blocking the turn driver.
    pub fn new<R>(
        reader: R,
        writer: W,
        policy: Arc<dyn PermissionPolicy>,
        working_dir: PathBuf,
        audit_sink: Arc<dyn AuditSink>,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let shared = Arc::new(Shared::new());
        let (notif_tx, notif_rx) = mpsc::unbounded_channel();

        let sender = TransportSender {
            inner: Arc::new(SenderInner {
                writer: tokio::sync::Mutex::new(writer),
                shared: Arc::clone(&shared),
                next_id: Mutex::new(0),
                policy,
                working_dir,
                audit_sink,
            }),
        };

        // Give the reader a sender clone so it can answer inbound requests
        // (permission) and emit audit entries using the injected policy/sink.
        let reader_task = tokio::spawn(read_loop(
            reader,
            Arc::clone(&shared),
            notif_tx,
            sender.clone(),
        ));

        Self {
            sender,
            notifications: notif_rx,
            reader_task: Some(reader_task),
        }
    }

    /// Borrow the clonable send side.
    pub fn sender(&self) -> &TransportSender<W> {
        &self.sender
    }

    /// Send a JSON-RPC request and await its response (delegates to the sender).
    pub async fn send_request<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> Result<serde_json::Value> {
        self.sender.send_request(method, params).await
    }

    /// Send a JSON-RPC notification (delegates to the sender).
    pub async fn send_notification<P: Serialize>(&self, method: &str, params: P) -> Result<()> {
        self.sender.send_notification(method, params).await
    }

    /// Receive the next inbound `session/update` notification, or `None` if the
    /// agent disconnected (the reader task ended and drained the channel).
    pub async fn next_notification(&mut self) -> Option<SessionNotificationParams> {
        self.notifications.recv().await
    }

    /// Mutable access to the notification receiver (used by the turn driver to
    /// interleave chunk delivery with the prompt response).
    pub fn notifications_mut(&mut self) -> &mut mpsc::UnboundedReceiver<SessionNotificationParams> {
        &mut self.notifications
    }
}

impl<W> Drop for Transport<W> {
    fn drop(&mut self) {
        // Stop the reader task; its read half (and, in production, the child's
        // stdout) is released. The write half drops with the last sender clone.
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
    }
}

/// The background reader loop: decode newline-delimited JSON-RPC and route it.
async fn read_loop<R, W>(
    reader: R,
    shared: Arc<Shared>,
    notif_tx: mpsc::UnboundedSender<SessionNotificationParams>,
    sender: TransportSender<W>,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            // Clean EOF: the agent closed stdout.
            Ok(None) => {
                shared.shutdown(ReaderEnd::Eof);
                return;
            }
            Ok(Some(line)) => {
                let trimmed = line.trim();
                // Tolerate blank keep-alive lines some transports emit.
                if trimmed.is_empty() {
                    continue;
                }
                // Be lenient about non-JSON-RPC lines: real agent CLIs (e.g.
                // `gemini --acp`) print human-readable log preamble on stdout
                // *around* the protocol stream ("Hook registry initialized…").
                // Treating such a line as fatal would break a perfectly healthy
                // agent, so we skip lines that don't parse as JSON-RPC and surface
                // them as diagnostics instead. A genuinely broken/dead agent is
                // still caught: its pending requests are woken on EOF
                // (`AgentExited`).
                match serde_json::from_str::<IncomingMessage>(trimmed) {
                    Ok(message) => route_message(message, &shared, &notif_tx, &sender).await,
                    Err(_) => {
                        tracing::debug!(target: "acp", "ignoring non-JSON-RPC line: {trimmed}");
                    }
                }
            }
            Err(e) => {
                shared.shutdown(ReaderEnd::Io(e.to_string()));
                return;
            }
        }
    }
}

/// Route one decoded message to the waiting request or the notification channel.
async fn route_message<W>(
    message: IncomingMessage,
    shared: &Arc<Shared>,
    notif_tx: &mpsc::UnboundedSender<SessionNotificationParams>,
    sender: &TransportSender<W>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    match message.classify() {
        IncomingKind::Response { id } => {
            let waiter = shared
                .pending
                .lock()
                .expect("pending mutex poisoned")
                .remove(&id);
            if let Some(tx) = waiter {
                // A JSON-RPC response carries exactly one of result/error.
                let outcome = if let Some(err) = message.error {
                    Err(AcpError::Rpc(err))
                } else if let Some(result) = message.result {
                    Ok(result)
                } else {
                    // Neither field present: treat null-result as an empty object
                    // so methods whose result we ignore still succeed.
                    Ok(serde_json::Value::Null)
                };
                let _ = tx.send(outcome);
            }
            // Unknown id → stray response; ignore.
        }
        IncomingKind::Notification => {
            // Only `session/update` carries the streamed text we care about;
            // forward those and drop everything else. A `session/update` we
            // cannot parse is non-fatal — skip it rather than tearing down a
            // working turn.
            if message.method.as_deref() == Some(crate::protocol::METHOD_SESSION_UPDATE)
                && let Some(params) = message.params
                && let Ok(notif) = serde_json::from_value::<SessionNotificationParams>(params)
            {
                // If the receiver is gone the client stopped caring.
                let _ = notif_tx.send(notif);
            }
        }
        IncomingKind::Request => {
            let req_id = message.id.expect("Request always carries an id");
            let method = message.method.as_deref().unwrap_or("<unknown>");

            if method == METHOD_SESSION_REQUEST_PERMISSION {
                // Parse params, invoke policy (with session working dir),
                // write JSON-RPC success response, and emit audit entry.
                if let Some(params_val) = message.params {
                    match serde_json::from_value::<RequestPermissionParams>(params_val) {
                        Ok(params) => {
                            let ctx = PermissionRequestContext {
                                session_id: &params.session_id,
                                working_dir: &sender.inner.working_dir,
                                tool_call: &params.tool_call,
                                options: &params.options,
                            };
                            let decision = sender.inner.policy.decide(&ctx);

                            let perm_result = if decision.allow {
                                if let Some(ref oid) = decision.option_id {
                                    PermissionResponse {
                                        outcome: PermissionOutcome::Selected {
                                            option_id: oid.clone(),
                                        },
                                    }
                                } else {
                                    PermissionResponse {
                                        outcome: PermissionOutcome::Cancelled,
                                    }
                                }
                            } else {
                                PermissionResponse {
                                    outcome: PermissionOutcome::Cancelled,
                                }
                            };

                            let _ = sender.send_response(req_id, perm_result).await;

                            // Build and emit the audit entry to the injected sink.
                            let tool = ToolRef {
                                name: params
                                    .tool_call
                                    .kind
                                    .clone()
                                    .unwrap_or_else(|| "tool".to_string()),
                                kind: params.tool_call.kind.clone(),
                                id: params.tool_call.tool_call_id.clone(),
                                title: params.tool_call.title.clone(),
                            };
                            let audit_decision = if decision.allow {
                                AuditDecision::Allow
                            } else {
                                AuditDecision::Deny
                            };
                            let entry = AuditEntry {
                                timestamp: Utc::now(),
                                run_id: "acp-transport".to_string(),
                                task_id: None,
                                session_id: Some(params.session_id.clone()),
                                tool,
                                decision: audit_decision,
                                option_id: decision.option_id.clone(),
                                policy: PolicyInfo {
                                    name: sender.inner.policy.name().to_string(),
                                    reason: decision.reason.clone(),
                                },
                                working_dir: sender.inner.working_dir.clone(),
                            };
                            sender.inner.audit_sink.record(entry);
                        }
                        Err(e) => {
                            tracing::warn!(target: "acp", "malformed permission request params: {e}");
                            let _ = sender
                                .send_error_response(
                                    req_id,
                                    JsonRpcError {
                                        code: -32602,
                                        message: format!("invalid params: {e}"),
                                        data: None,
                                    },
                                )
                                .await;
                        }
                    }
                } else {
                    let _ = sender
                        .send_error_response(
                            req_id,
                            JsonRpcError {
                                code: -32602,
                                message: "missing params for request_permission".into(),
                                data: None,
                            },
                        )
                        .await;
                }
            } else {
                // Other inbound requests (tools, etc.): log and reply with
                // method-not-found so the peer's request never hangs.
                tracing::warn!(
                    target: "acp",
                    "inbound request for unsupported method {method}; replying with error so caller does not hang"
                );
                let _ = sender
                    .send_error_response(
                        req_id,
                        JsonRpcError {
                            code: -32601,
                            message: format!("method not found: {method}"),
                            data: None,
                        },
                    )
                    .await;
            }
        }
        // Valid JSON but not a usable JSON-RPC message (no id and no method) —
        // treat as benign noise and skip, consistent with the lenient line
        // handling in `read_loop`.
        IncomingKind::Malformed => {}
    }
}

#[cfg(test)]
mod tests {
    //! Transport-level tests over an in-memory duplex pipe (no subprocess).

    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use makina_core::governance::{AuditDecision, AuditEntry, AuditSink, NoopAuditSink};
    use serde_json::{Value, json};

    use crate::permission::WorktreePolicy;
    use crate::protocol::{METHOD_SESSION_UPDATE, SessionUpdate};

    /// Build a transport whose peer end is a raw duplex half we can script.
    fn duplex_transport() -> (
        Transport<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
        tokio::io::ReadHalf<tokio::io::DuplexStream>, // peer reads what client writes
        tokio::io::WriteHalf<tokio::io::DuplexStream>, // peer writes what client reads
    ) {
        // client_io: the client's view; peer_io: the other end.
        let (client_io, peer_io) = tokio::io::duplex(8 * 1024);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (peer_read, peer_write) = tokio::io::split(peer_io);
        let cwd = PathBuf::from("/test/duplex/workdir");
        let transport = Transport::new(
            client_read,
            client_write,
            Arc::new(WorktreePolicy::new(cwd.clone())),
            cwd,
            Arc::new(NoopAuditSink),
        );
        (transport, peer_read, peer_write)
    }

    async fn write_line<W: AsyncWrite + Unpin>(w: &mut W, line: &str) {
        w.write_all(line.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
        w.flush().await.unwrap();
    }

    async fn read_line<R: AsyncRead + Unpin>(r: &mut R) -> String {
        let mut lines = BufReader::new(r).lines();
        lines.next_line().await.unwrap().expect("expected a line")
    }

    #[tokio::test]
    async fn request_response_round_trip() {
        let (transport, mut peer_read, mut peer_write) = duplex_transport();

        // Peer: read the request, reply with a result echoing the id.
        let peer = tokio::spawn(async move {
            let req_line = read_line(&mut peer_read).await;
            let req: serde_json::Value = serde_json::from_str(&req_line).unwrap();
            assert_eq!(req["jsonrpc"], "2.0");
            assert_eq!(req["method"], "ping");
            let id = req["id"].as_u64().unwrap();
            let reply = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } });
            write_line(&mut peer_write, &reply.to_string()).await;
        });

        let result = transport
            .send_request("ping", serde_json::json!({ "v": 1 }))
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn rpc_error_response_is_typed() {
        let (transport, mut peer_read, mut peer_write) = duplex_transport();
        let peer = tokio::spawn(async move {
            let req_line = read_line(&mut peer_read).await;
            let req: serde_json::Value = serde_json::from_str(&req_line).unwrap();
            let id = req["id"].as_u64().unwrap();
            let reply = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "method not found" }
            });
            write_line(&mut peer_write, &reply.to_string()).await;
        });

        let err = transport.send_request("nope", ()).await.unwrap_err();
        match err {
            AcpError::Rpc(e) => {
                assert_eq!(e.code, -32601);
                assert_eq!(e.message, "method not found");
            }
            other => panic!("expected Rpc error, got {other:?}"),
        }
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn notifications_are_forwarded() {
        let (mut transport, mut _peer_read, mut peer_write) = duplex_transport();
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": METHOD_SESSION_UPDATE,
            "params": {
                "sessionId": "s1",
                "update": { "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "hi" } }
            }
        });
        write_line(&mut peer_write, &notif.to_string()).await;

        let received = transport.next_notification().await.expect("a notification");
        assert_eq!(received.session_id, "s1");
        match received.update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                assert_eq!(chunk.content.as_text(), Some("hi"));
            }
            other => panic!("expected AgentMessageChunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn eof_wakes_pending_request_with_agent_exited() {
        let (transport, peer_read, mut peer_write) = duplex_transport();
        // Shut down the peer's *write* direction so the client's read side hits
        // clean EOF (the "agent closed stdout" case), while keeping `peer_read`
        // alive so the client's write still succeeds. (With `tokio::io::split`,
        // dropping only the write half would NOT close the channel — the read
        // half keeps the stream alive — so we must `shutdown()` explicitly.)
        peer_write.shutdown().await.unwrap();

        let err = transport.send_request("ping", ()).await.unwrap_err();
        assert!(
            matches!(err, AcpError::AgentExited { .. }),
            "expected AgentExited after EOF, got {err:?}"
        );
        drop(peer_read);
    }

    #[tokio::test]
    async fn non_jsonrpc_lines_are_skipped_not_fatal() {
        // Real agent CLIs (e.g. `gemini --acp`) emit human-readable log lines on
        // stdout around the protocol. The reader must skip them and stay usable.
        let (transport, mut peer_read, mut peer_write) = duplex_transport();

        let peer = tokio::spawn(async move {
            // Two kinds of noise: plain text, and valid JSON that isn't a usable
            // JSON-RPC message (no id, no method).
            write_line(&mut peer_write, "Hook registry initialized with 0 entries").await;
            write_line(&mut peer_write, r#"{"jsonrpc":"2.0"}"#).await;
            // Now the real response to the client's request.
            let req_line = read_line(&mut peer_read).await;
            let req: serde_json::Value = serde_json::from_str(&req_line).unwrap();
            let id = req["id"].as_u64().unwrap();
            let reply = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } });
            write_line(&mut peer_write, &reply.to_string()).await;
        });

        // Despite the leading noise, the request/response still completes.
        let result = transport.send_request("ping", ()).await.unwrap();
        assert_eq!(result["ok"], true);
        peer.await.unwrap();
    }

    /// Capturing sink for the permission-response test; records every
    /// AuditEntry so the test can assert exactly one allow decision was emitted.
    #[derive(Clone, Default)]
    struct TestAuditSink {
        entries: Arc<Mutex<Vec<AuditEntry>>>,
    }

    impl AuditSink for TestAuditSink {
        fn record(&self, entry: AuditEntry) {
            self.entries.lock().unwrap().push(entry);
        }
    }

    #[tokio::test]
    async fn permission_request_mid_turn_is_answered_and_audited() {
        // In-memory duplex (no subprocess). Scripted peer sends a
        // session/request_permission while a prompt turn is in flight.
        let (client_io, peer_io) = tokio::io::duplex(8 * 1024);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (mut peer_read, mut peer_write) = tokio::io::split(peer_io);

        let worktree = PathBuf::from("/tmp/makina-test-worktree-perm");
        let policy: Arc<dyn PermissionPolicy> = Arc::new(WorktreePolicy::new(worktree.clone()));
        let sink = Arc::new(TestAuditSink::default());
        let entries = sink.entries.clone();

        let transport = Transport::new(
            client_read,
            client_write,
            policy,
            worktree.clone(),
            sink as Arc<dyn AuditSink>,
        );

        // Peer script: handshake, start prompt, inject one permission request,
        // verify the client replied with the allow-once selection, then finish turn.
        let peer = tokio::spawn(async move {
            // initialize
            let line = read_line(&mut peer_read).await;
            let req: Value = serde_json::from_str(&line).unwrap();
            let id = req["id"].as_u64().unwrap();
            write_line(
                &mut peer_write,
                &json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1}}).to_string(),
            )
            .await;

            // session/new
            let line = read_line(&mut peer_read).await;
            let req: Value = serde_json::from_str(&line).unwrap();
            let id = req["id"].as_u64().unwrap();
            write_line(
                &mut peer_write,
                &json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"sess-perm-test"}})
                    .to_string(),
            )
            .await;

            // session/prompt (keep pending)
            let line = read_line(&mut peer_read).await;
            let req: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["method"], "session/prompt");
            let prompt_id = req["id"].as_u64().unwrap();

            // Mid-turn: send inbound permission request
            let perm_id = 42u64;
            let perm_req = json!({
                "jsonrpc": "2.0",
                "id": perm_id,
                "method": "session/request_permission",
                "params": {
                    "sessionId": "sess-perm-test",
                    "options": [
                        {"optionId": "proceed_always", "name": "Always", "kind": "allow_always"},
                        {"optionId": "proceed_once",  "name": "Allow",  "kind": "allow_once"}
                    ],
                    "toolCall": {
                        "toolCallId": "write_file__42",
                        "title": "Writing probe.txt"
                    }
                }
            });
            write_line(&mut peer_write, &perm_req.to_string()).await;

            // Read the response the client (transport) wrote for the perm request
            let resp_line = read_line(&mut peer_read).await;
            let resp: Value = serde_json::from_str(&resp_line).unwrap();
            assert_eq!(resp["jsonrpc"], "2.0");
            assert_eq!(resp["id"], perm_id);
            let outcome = &resp["result"]["outcome"];
            assert_eq!(outcome["outcome"], "selected");
            assert_eq!(outcome["optionId"], "proceed_once"); // WorktreePolicy picks allow_once

            // Stream one text chunk (so turn has visible progress)
            let chunk = json!({
                "jsonrpc": "2.0",
                "method": METHOD_SESSION_UPDATE,
                "params": {
                    "sessionId": "sess-perm-test",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "done" }
                    }
                }
            });
            write_line(&mut peer_write, &chunk.to_string()).await;

            // Complete the original prompt
            let done = json!({
                "jsonrpc": "2.0",
                "id": prompt_id,
                "result": { "stopReason": "end_turn" }
            });
            write_line(&mut peer_write, &done.to_string()).await;
        });

        // Drive from client side using the transport directly (handshake + prompt).
        // The inbound perm request is handled by the background reader while this
        // prompt request is outstanding.
        let _init = transport
            .send_request(
                "initialize",
                json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"t","version":"0"}}),
            )
            .await
            .unwrap();

        let _new = transport
            .send_request("session/new", json!({"cwd": worktree, "mcpServers":[]}))
            .await
            .unwrap();

        let prompt_fut = transport.send_request(
            "session/prompt",
            json!({"sessionId":"sess-perm-test","prompt":[{"type":"text","text":"work"}]}),
        );

        let prompt_res = prompt_fut.await.unwrap();
        assert_eq!(prompt_res["stopReason"], "end_turn");

        peer.await.unwrap();

        // Exactly one audit entry with an allow decision.
        let captured = entries.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].decision, AuditDecision::Allow);
    }
}
