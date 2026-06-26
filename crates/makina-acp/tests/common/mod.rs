//! Shared test scaffolding: an in-memory **mock ACP agent**.
//!
//! The mock speaks the same newline-delimited JSON-RPC 2.0 dialect a real ACP
//! CLI does, over the peer end of a [`tokio::io::duplex`] pipe. It lets the
//! integration tests exercise [`makina_acp::AcpClient`]'s full handshake +
//! prompt/response exchange **deterministically and without a subprocess**, per
//! the project testing strategy (no real agent CLIs in automated tests).
//!
//! Each method is answered the way the official `agent-client-protocol-schema`
//! defines it (protocol v1):
//! * `initialize` → `{ protocolVersion: 1, agentInfo: {…} }`
//! * `session/new` → `{ sessionId: "<id>" }`
//! * `session/prompt` → a sequence of `session/update` notifications carrying
//!   `agent_message_chunk` text, followed by the prompt result with a stop
//!   reason.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, ReadHalf,
    WriteHalf,
};
use tokio::task::JoinHandle;

/// Shared, thread-safe recorder of the prompt texts the mock agent received.
///
/// Cloning shares the same underlying buffer (it is an `Arc`), so a test can
/// hand one clone to [`spawn_mock_agent_recording`] and keep another to read
/// back what the client sent. Used by the task-15 backend tests to verify the
/// system-prompt prepend end-to-end through the trait.
pub type PromptLog = Arc<Mutex<Vec<String>>>;

/// How the mock agent should behave for the single `session/prompt` it answers.
#[derive(Clone)]
pub struct MockBehavior {
    /// The session id returned from `session/new`.
    pub session_id: String,
    /// Text chunks emitted as `agent_message_chunk` `session/update`s, in order.
    pub chunks: Vec<String>,
    /// The `stopReason` string returned in the prompt result (e.g. `"end_turn"`).
    pub stop_reason: String,
    /// If set, the mock injects this many non-text updates (a tool-call update)
    /// before the text chunks. Since `tool_call` is now a modelled update kind,
    /// the client delivers each of these as an [`makina_acp::AcpResponseChunk::ToolCall`]
    /// side-channel chunk rather than ignoring it.
    pub leading_noise_updates: usize,
    /// If `true`, the mock injects — before the text chunks, in this exact order —
    /// one `agent_thought_chunk` (with text), one `tool_call` (toolCallId + title +
    /// status `"pending"`), and one `tool_call_update` (same toolCallId, status
    /// `"completed"`). Lets tests assert the rich Thought / ToolCall / ToolCallUpdate
    /// chunks are delivered interleaved with text.
    pub inject_thoughts_and_tools: bool,
    /// Authentication methods advertised in the `initialize` response.
    ///
    /// Each entry is a `{ "type": "…", … }` JSON object, mirroring the ACP wire
    /// format. Defaults to a single `oauth` method so the Zed-model test can
    /// assert that `AcpClient::auth_methods()` returns a non-empty slice.
    pub auth_methods: Vec<Value>,
    /// If `Some`, the mock emits one `session/request_permission` inbound request
    /// **before** the first text chunk, waits to read the client's response, then
    /// continues the turn normally.  The value is the `toolCallId` to use in the
    /// permission request, with a single `allow_once` option offered.
    pub inject_permission_request: Option<String>,
    /// Optional usage counts to include in the prompt result (e.g. `{ "inputTokens": 42, "outputTokens": 100 }`).
    pub usage: Option<Value>,
}

impl Default for MockBehavior {
    fn default() -> Self {
        Self {
            session_id: "mock-session-1".to_string(),
            chunks: vec!["Hello".into(), ", ".into(), "world!".into()],
            stop_reason: "end_turn".to_string(),
            leading_noise_updates: 0,
            inject_thoughts_and_tools: false,
            // Default: advertise one auth method so tests can assert observability.
            auth_methods: vec![json!({ "type": "oauth" })],
            inject_permission_request: None,
            usage: None,
        }
    }
}

/// Spawn a mock ACP agent on the peer end of a fresh duplex pipe.
///
/// Returns the two client-side halves (to hand to
/// [`makina_acp::AcpClient::with_transport`]) plus the join handle of the mock
/// task so the test can await its clean completion.
pub fn spawn_mock_agent(
    behavior: MockBehavior,
) -> (
    ReadHalf<DuplexStream>,
    WriteHalf<DuplexStream>,
    JoinHandle<()>,
) {
    let (client_io, peer_io) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (peer_read, peer_write) = tokio::io::split(peer_io);
    let handle = tokio::spawn(run_mock(peer_read, peer_write, behavior, None));
    (client_read, client_write, handle)
}

/// Like [`spawn_mock_agent`], but every `session/prompt`'s text content block is
/// appended (in order) to `log`.
///
/// Lets a test assert *what* the client actually sent — e.g. that the task-15
/// backend prepends the system prompt only to the first turn.
pub fn spawn_mock_agent_recording(
    behavior: MockBehavior,
    log: PromptLog,
) -> (
    ReadHalf<DuplexStream>,
    WriteHalf<DuplexStream>,
    JoinHandle<()>,
) {
    let (client_io, peer_io) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (peer_read, peer_write) = tokio::io::split(peer_io);
    let handle = tokio::spawn(run_mock(peer_read, peer_write, behavior, Some(log)));
    (client_read, client_write, handle)
}

/// The mock agent's protocol loop. When `prompt_log` is `Some`, each
/// `session/prompt`'s concatenated text content is recorded into it.
async fn run_mock<R, W>(
    reader: R,
    mut writer: W,
    behavior: MockBehavior,
    prompt_log: Option<PromptLog>,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = serde_json::from_str(trimmed).expect("mock: client sent invalid JSON");
        let id = req["id"].clone();
        let method = req["method"].as_str().unwrap_or_default().to_string();

        match method.as_str() {
            "initialize" => {
                send(
                    &mut writer,
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": 1,
                            "agentCapabilities": {},
                            "authMethods": behavior.auth_methods,
                            "agentInfo": { "name": "mock-acp-agent", "version": "0.0.1" }
                        }
                    }),
                )
                .await;
            }
            "session/new" => {
                send(
                    &mut writer,
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "sessionId": behavior.session_id }
                    }),
                )
                .await;
            }
            "session/prompt" => {
                let session_id = req["params"]["sessionId"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();

                // Record the sent prompt text (concatenation of text blocks) if
                // a log was supplied.
                if let Some(log) = &prompt_log {
                    let mut sent = String::new();
                    if let Some(blocks) = req["params"]["prompt"].as_array() {
                        for block in blocks {
                            if block["type"] == "text"
                                && let Some(t) = block["text"].as_str()
                            {
                                sent.push_str(t);
                            }
                        }
                    }
                    log.lock().expect("mock prompt log poisoned").push(sent);
                }

                // Optional leading non-text updates the client must ignore.
                for n in 0..behavior.leading_noise_updates {
                    send(
                        &mut writer,
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": format!("tc-{n}"),
                                    "title": "noise"
                                }
                            }
                        }),
                    )
                    .await;
                }

                // Optional rich side-channel updates: one thought, one tool_call,
                // and one matching tool_call_update, in that exact order, before
                // the text chunks. Lets the rich-drain tests assert arrival order.
                if behavior.inject_thoughts_and_tools {
                    send(
                        &mut writer,
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "agent_thought_chunk",
                                    "content": { "type": "text", "text": "thinking…" }
                                }
                            }
                        }),
                    )
                    .await;
                    send(
                        &mut writer,
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": "rich-tc-1",
                                    "title": "running tests",
                                    "kind": "execute",
                                    "status": "pending"
                                }
                            }
                        }),
                    )
                    .await;
                    send(
                        &mut writer,
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "tool_call_update",
                                    "toolCallId": "rich-tc-1",
                                    "status": "completed"
                                }
                            }
                        }),
                    )
                    .await;
                }

                // Optional interleaved permission request (inbound from agent).
                // The mock sends a `session/request_permission` request, then
                // reads back exactly one response before continuing.
                if let Some(ref tool_call_id) = behavior.inject_permission_request {
                    // Sentinel id: distinct from the normal request sequence (0, 1, 2 …)
                    // so both sides can unambiguously identify the permission response.
                    // NOTE: mirrored as PERMISSION_REQUEST_ID in `src/backend.rs` tests.
                    const PERMISSION_REQUEST_ID: u64 = 9999;
                    send(
                        &mut writer,
                        json!({
                            "jsonrpc": "2.0",
                            "id": PERMISSION_REQUEST_ID,
                            "method": "session/request_permission",
                            "params": {
                                "sessionId": session_id,
                                "options": [
                                    {
                                        "optionId": "proceed_always",
                                        "name": "Always",
                                        "kind": "allow_always"
                                    },
                                    {
                                        "optionId": "proceed_once",
                                        "name": "Allow",
                                        "kind": "allow_once"
                                    }
                                ],
                                "toolCall": {
                                    "toolCallId": tool_call_id,
                                    "title": format!("Executing {tool_call_id}")
                                }
                            }
                        }),
                    )
                    .await;
                    // Read the client's response to the permission request.
                    // The transport reader task handles this independently, so
                    // we just drain it here to keep the mock in sync.
                    if let Ok(Some(resp_line)) = lines.next_line().await {
                        let resp: Value = serde_json::from_str(resp_line.trim())
                            .expect("mock: client sent invalid JSON for perm response");
                        // Validate the response has our id; the actual outcome is
                        // determined by the injected policy on the client side.
                        assert_eq!(
                            resp["id"], PERMISSION_REQUEST_ID,
                            "mock: expected permission response with id {PERMISSION_REQUEST_ID}"
                        );
                    }
                }

                // Stream the assistant text as agent_message_chunk updates.
                for chunk in &behavior.chunks {
                    send(
                        &mut writer,
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": chunk }
                                }
                            }
                        }),
                    )
                    .await;
                }

                // Finish the turn.
                let mut result = json!({ "stopReason": behavior.stop_reason });
                if let Some(usage) = &behavior.usage {
                    result["usage"] = usage.clone();
                }
                send(
                    &mut writer,
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result
                    }),
                )
                .await;
            }
            "session/cancel" => {
                // Notification — no reply. The mock simply records nothing.
            }
            other => {
                // Unknown method: reply with a JSON-RPC method-not-found error.
                send(
                    &mut writer,
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("method not found: {other}") }
                    }),
                )
                .await;
            }
        }
    }
}

/// Write one JSON value as a newline-terminated line.
async fn send<W: AsyncWrite + Unpin>(writer: &mut W, value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    writer.write_all(&bytes).await.expect("mock: write failed");
    writer.flush().await.expect("mock: flush failed");
}
