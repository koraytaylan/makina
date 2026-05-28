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

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, ReadHalf,
    WriteHalf,
};
use tokio::task::JoinHandle;

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
    /// before the text chunks, to prove the client ignores them.
    pub leading_noise_updates: usize,
}

impl Default for MockBehavior {
    fn default() -> Self {
        Self {
            session_id: "mock-session-1".to_string(),
            chunks: vec!["Hello".into(), ", ".into(), "world!".into()],
            stop_reason: "end_turn".to_string(),
            leading_noise_updates: 0,
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
    let handle = tokio::spawn(run_mock(peer_read, peer_write, behavior));
    (client_read, client_write, handle)
}

/// The mock agent's protocol loop.
async fn run_mock<R, W>(reader: R, mut writer: W, behavior: MockBehavior)
where
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
                            "authMethods": [],
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
                send(
                    &mut writer,
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "stopReason": behavior.stop_reason }
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
