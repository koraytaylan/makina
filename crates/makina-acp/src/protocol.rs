//! ACP wire types and the JSON-RPC 2.0 envelope.
//!
//! Makina speaks a deliberately **minimal** subset of the Agent Client Protocol
//! (ACP) — exactly what is required for a spawn → initialize → session → prompt
//! → teardown exchange. The full protocol (tool calls, permission requests,
//! plans, MCP-over-ACP, modes, …) is intentionally **not** modelled here; the
//! orchestrator only needs to drive an agent through a single text turn and
//! collect the streamed text response.
//!
//! # Authoritative reference
//!
//! The wire shapes below were derived from the official Zed `agent-client-protocol`
//! Rust SDK — specifically the `agent-client-protocol-schema` crate (v0.13.x,
//! protocol version 1) and the published spec at <https://agentclientprotocol.com>.
//! The field names and serde renames match that schema exactly so a real ACP CLI
//! (e.g. `claude-code-acp`, Gemini's `--experimental-acp`) understands them:
//!
//! * **Framing**: newline-delimited JSON — one JSON-RPC 2.0 message per line.
//!   (Confirmed by the SDK's `stdio`/`AcpAgent` transports, which read with
//!   `BufReader::lines()` and write `line + "\n"`. ACP does **not** use
//!   `Content-Length` framing.)
//! * **`initialize`** — `protocolVersion` is an integer (`1` for V1),
//!   `clientCapabilities`, `clientInfo { name, version }`.
//! * **`session/new`** — `{ "cwd": <absolute path>, "mcpServers": [] }`.
//! * **`session/prompt`** — `{ "sessionId": <string>, "prompt": [ContentBlock] }`.
//! * **`session/update`** — agent→client notification carrying a tagged
//!   `update` whose `sessionUpdate` discriminator selects the chunk kind
//!   (`agent_message_chunk`, `agent_thought_chunk`, …); the chunk's `content`
//!   is a `{ "type": "text", "text": … }` block.
//!
//! Method-name constants (`initialize`, `session/new`, `session/prompt`,
//! `session/update`, `session/cancel`) are mirrored from the schema's
//! `*_METHOD_NAME` definitions.

use serde::{Deserialize, Serialize};

// ── JSON-RPC method names (mirrors agent-client-protocol-schema) ────────────────

/// `initialize` — capability-negotiation handshake (client → agent).
pub const METHOD_INITIALIZE: &str = "initialize";
/// `session/new` — create a new session (client → agent).
pub const METHOD_SESSION_NEW: &str = "session/new";
/// `session/prompt` — send one user turn (client → agent).
pub const METHOD_SESSION_PROMPT: &str = "session/prompt";
/// `session/update` — streamed turn update (agent → client notification).
pub const METHOD_SESSION_UPDATE: &str = "session/update";
/// `session/cancel` — cancel the in-flight turn (client → agent notification).
pub const METHOD_SESSION_CANCEL: &str = "session/cancel";

/// The protocol version Makina speaks (ACP v1).
pub const PROTOCOL_VERSION: u16 = 1;

// ── JSON-RPC 2.0 envelope ───────────────────────────────────────────────────────

/// A single line on the wire, decoded enough to route it.
///
/// ACP multiplexes three JSON-RPC message kinds over one byte stream:
/// responses (have an `id` + `result`/`error`), requests (have an `id` +
/// `method`), and notifications (have a `method`, no `id`). We only *send*
/// requests/notifications and only *receive* responses + `session/update`
/// notifications, but we decode the full shape so unexpected inbound requests
/// (e.g. a permission prompt) can be recognised rather than mis-parsed.
#[derive(Debug, Clone, Deserialize)]
pub struct IncomingMessage {
    /// JSON-RPC version tag; always `"2.0"`. Captured but not validated
    /// strictly (a non-conforming agent is surfaced elsewhere as a protocol
    /// error when the payload cannot be interpreted).
    #[serde(default)]
    pub jsonrpc: Option<String>,
    /// Request/response correlation id. Absent for notifications.
    #[serde(default)]
    pub id: Option<u64>,
    /// Method name. Present for requests and notifications, absent for responses.
    #[serde(default)]
    pub method: Option<String>,
    /// Method params (requests/notifications) — left as raw JSON.
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// Success payload (responses) — left as raw JSON.
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    /// Error payload (responses).
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

impl IncomingMessage {
    /// Classify this line into the routing categories the client cares about.
    pub fn classify(&self) -> IncomingKind {
        match (self.id, self.method.as_deref()) {
            // A response correlates by id and has no method.
            (Some(id), None) => IncomingKind::Response { id },
            // A notification has a method and no id.
            (None, Some(_)) => IncomingKind::Notification,
            // A server→client request has both an id and a method.
            (Some(_), Some(_)) => IncomingKind::Request,
            // Neither id nor method → unintelligible JSON-RPC.
            (None, None) => IncomingKind::Malformed,
        }
    }
}

/// The routing category of an [`IncomingMessage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncomingKind {
    /// A reply to one of our outgoing requests, correlated by `id`.
    Response {
        /// The correlation id this response answers.
        id: u64,
    },
    /// An agent→client notification (e.g. `session/update`).
    Notification,
    /// An agent→client request expecting a reply (e.g. a permission prompt).
    Request,
    /// A line that is neither a valid response, request, nor notification.
    Malformed,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code (per the JSON-RPC spec / ACP error codes).
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured error data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)?;
        if let Some(data) = &self.data {
            write!(f, " ({data})")?;
        }
        Ok(())
    }
}

// Lets `AcpError` carry a `JsonRpcError` via `#[from]` / `#[error(transparent)]`.
impl std::error::Error for JsonRpcError {}

/// An outgoing JSON-RPC 2.0 **request** (expects a correlated response).
#[derive(Debug, Clone, Serialize)]
pub struct OutgoingRequest<'a, P: Serialize> {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Correlation id chosen by the client.
    pub id: u64,
    /// Method name.
    pub method: &'a str,
    /// Method params.
    pub params: P,
}

impl<'a, P: Serialize> OutgoingRequest<'a, P> {
    /// Construct a request with the JSON-RPC `2.0` tag pre-filled.
    pub fn new(id: u64, method: &'a str, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// An outgoing JSON-RPC 2.0 **notification** (no id, no response).
#[derive(Debug, Clone, Serialize)]
pub struct OutgoingNotification<'a, P: Serialize> {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Method name.
    pub method: &'a str,
    /// Method params.
    pub params: P,
}

impl<'a, P: Serialize> OutgoingNotification<'a, P> {
    /// Construct a notification with the JSON-RPC `2.0` tag pre-filled.
    pub fn new(method: &'a str, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

// ── ACP method params / results ─────────────────────────────────────────────────

/// `initialize` params (client → agent).
///
/// Mirrors `agent_client_protocol_schema::InitializeRequest`
/// (`#[serde(rename_all = "camelCase")]`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Latest protocol version supported by this client (integer; `1`).
    pub protocol_version: u16,
    /// Client capabilities. Makina advertises none beyond the baseline, so this
    /// is an empty object — agents fill in their own capabilities in the reply.
    pub client_capabilities: ClientCapabilities,
    /// Client name/version, surfaced to the agent for diagnostics.
    pub client_info: Implementation,
}

/// Client capability advertisement. Empty for Makina's MVP (no filesystem or
/// terminal capabilities are offered to the agent).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {}

/// Implementation name/version metadata (`{ name, version }`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Implementation {
    /// Programmatic name of the implementation.
    pub name: String,
    /// Version string (e.g. `"0.1.0"`).
    pub version: String,
}

/// `initialize` result (agent → client).
///
/// Only the fields Makina inspects are modelled; unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// Protocol version the agent agreed to (or its latest).
    pub protocol_version: u16,
    /// Agent name/version, if provided.
    #[serde(default)]
    pub agent_info: Option<Implementation>,
}

/// `session/new` params (client → agent).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    /// Absolute working directory for the session.
    pub cwd: std::path::PathBuf,
    /// MCP servers to attach — always empty for Makina (no MCP-over-ACP).
    pub mcp_servers: Vec<serde_json::Value>,
}

/// `session/new` result (agent → client).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    /// The opaque session identifier the agent assigned.
    pub session_id: String,
}

/// `session/prompt` params (client → agent).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    /// Session this turn belongs to.
    pub session_id: String,
    /// The user message, as a list of content blocks. Makina sends exactly one
    /// text block.
    pub prompt: Vec<ContentBlock>,
}

/// `session/prompt` result (agent → client) — delivered once the turn ends.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    /// Why the agent stopped (`end_turn`, `max_tokens`, `cancelled`, …).
    pub stop_reason: StopReason,
}

/// `session/cancel` params (client → agent notification).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    /// Session whose in-flight turn should be cancelled.
    pub session_id: String,
}

/// Reason an agent stopped a prompt turn.
///
/// Mirrors `agent_client_protocol_schema::StopReason`
/// (`#[serde(rename_all = "snake_case")]`). Unknown future variants deserialize
/// into [`StopReason::Other`] rather than failing the turn.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The turn ended normally.
    EndTurn,
    /// The agent hit its token limit.
    MaxTokens,
    /// The agent hit its max-requests-per-turn limit.
    MaxTurnRequests,
    /// The agent refused to continue.
    Refusal,
    /// The turn was cancelled (via `session/cancel`).
    Cancelled,
    /// Any stop reason not known to this minimal client.
    #[serde(other)]
    Other,
}

/// A content block. Makina sends and recognises only text blocks; the tagged
/// representation (`{ "type": "text", "text": … }`) matches
/// `agent_client_protocol_schema::ContentBlock` (`#[serde(tag = "type")]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain or Markdown text.
    Text {
        /// The text payload.
        text: String,
    },
    /// Any non-text block (image, audio, resource, …). Captured opaquely so a
    /// mixed-content turn does not fail to parse; Makina ignores these.
    #[serde(other)]
    Other,
}

impl ContentBlock {
    /// Build a text content block.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }

    /// Borrow the text if this is a text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            ContentBlock::Other => None,
        }
    }
}

/// `session/update` notification params (agent → client).
///
/// Mirrors `agent_client_protocol_schema::SessionNotification`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotificationParams {
    /// Session this update pertains to.
    pub session_id: String,
    /// The actual update payload.
    pub update: SessionUpdate,
}

/// A streamed session update.
///
/// Mirrors `agent_client_protocol_schema::SessionUpdate`
/// (`#[serde(tag = "sessionUpdate", rename_all = "snake_case")]`). Each text
/// chunk variant wraps a flattened [`ContentChunk`]. Variants Makina does not
/// consume (tool calls, plans, …) collapse into [`SessionUpdate::Other`].
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// A chunk of the agent's assistant message — the text Makina collects.
    AgentMessageChunk(ContentChunk),
    /// A chunk of the agent's "thinking"/reasoning stream (ignored).
    AgentThoughtChunk(ContentChunk),
    /// A chunk echoing the user message (ignored).
    UserMessageChunk(ContentChunk),
    /// Any other update kind (tool call, plan, mode change, …) — ignored.
    #[serde(other)]
    Other,
}

/// A streamed content chunk (`{ "content": ContentBlock }`).
#[derive(Debug, Clone, Deserialize)]
pub struct ContentChunk {
    /// The content carried by this chunk.
    pub content: ContentBlock,
}

#[cfg(test)]
mod tests {
    //! Wire-format proofs: every shape we serialize must match the ACP schema's
    //! field names, and every shape we deserialize must accept the schema's
    //! output (including unknown fields and unknown enum variants).

    use super::*;

    #[test]
    fn initialize_params_serializes_with_camel_case() {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "makina".into(),
                version: "0.1.0".into(),
            },
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["protocolVersion"], 1);
        assert!(v["clientCapabilities"].is_object());
        assert_eq!(v["clientInfo"]["name"], "makina");
        assert_eq!(v["clientInfo"]["version"], "0.1.0");
    }

    #[test]
    fn new_session_params_uses_cwd_and_mcp_servers() {
        let params = NewSessionParams {
            cwd: std::path::PathBuf::from("/repo/task-1"),
            mcp_servers: vec![],
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["cwd"], "/repo/task-1");
        assert_eq!(v["mcpServers"], serde_json::json!([]));
    }

    #[test]
    fn prompt_params_serializes_text_content_block() {
        let params = PromptParams {
            session_id: "sess-1".into(),
            prompt: vec![ContentBlock::text("hello")],
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["sessionId"], "sess-1");
        assert_eq!(v["prompt"][0]["type"], "text");
        assert_eq!(v["prompt"][0]["text"], "hello");
    }

    #[test]
    fn prompt_result_deserializes_stop_reason() {
        let v = serde_json::json!({ "stopReason": "end_turn" });
        let r: PromptResult = serde_json::from_value(v).unwrap();
        assert_eq!(r.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn unknown_stop_reason_maps_to_other() {
        let v = serde_json::json!({ "stopReason": "some_future_reason" });
        let r: PromptResult = serde_json::from_value(v).unwrap();
        assert_eq!(r.stop_reason, StopReason::Other);
    }

    #[test]
    fn session_update_parses_agent_message_chunk() {
        // Exactly the shape a real agent emits over the wire.
        let v = serde_json::json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "partial answer" }
            }
        });
        let n: SessionNotificationParams = serde_json::from_value(v).unwrap();
        assert_eq!(n.session_id, "sess-1");
        match n.update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                assert_eq!(chunk.content.as_text(), Some("partial answer"));
            }
            other => panic!("expected AgentMessageChunk, got {other:?}"),
        }
    }

    #[test]
    fn session_update_ignores_unknown_update_kinds() {
        let v = serde_json::json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1",
                "title": "running tests"
            }
        });
        let n: SessionNotificationParams = serde_json::from_value(v).unwrap();
        assert!(matches!(n.update, SessionUpdate::Other));
    }

    #[test]
    fn initialize_result_ignores_unknown_fields() {
        // Agents send many fields we don't model; they must not break parsing.
        let v = serde_json::json!({
            "protocolVersion": 1,
            "agentCapabilities": { "promptCapabilities": { "image": true } },
            "authMethods": [],
            "agentInfo": { "name": "claude-code-acp", "version": "1.2.3" }
        });
        let r: InitializeResult = serde_json::from_value(v).unwrap();
        assert_eq!(r.protocol_version, 1);
        assert_eq!(r.agent_info.unwrap().name, "claude-code-acp");
    }

    #[test]
    fn incoming_message_classification() {
        let response: IncomingMessage =
            serde_json::from_value(serde_json::json!({ "jsonrpc": "2.0", "id": 7, "result": {} }))
                .unwrap();
        assert_eq!(response.classify(), IncomingKind::Response { id: 7 });

        let notification: IncomingMessage = serde_json::from_value(
            serde_json::json!({ "jsonrpc": "2.0", "method": "session/update", "params": {} }),
        )
        .unwrap();
        assert_eq!(notification.classify(), IncomingKind::Notification);

        let request: IncomingMessage = serde_json::from_value(
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "session/request_permission" }),
        )
        .unwrap();
        assert_eq!(request.classify(), IncomingKind::Request);

        let malformed: IncomingMessage =
            serde_json::from_value(serde_json::json!({ "jsonrpc": "2.0" })).unwrap();
        assert_eq!(malformed.classify(), IncomingKind::Malformed);
    }
}
