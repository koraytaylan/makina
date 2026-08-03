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
//!   `update` whose `sessionUpdate` discriminator selects the update kind. Text
//!   chunks (`agent_message_chunk`, `agent_thought_chunk`, `user_message_chunk`)
//!   carry a `{ "type": "text", "text": … }` block; tool-call lifecycle updates
//!   (`tool_call`, `tool_call_update`) are now modelled too. Unmodelled kinds
//!   (plans, mode changes, …) collapse into `SessionUpdate::Other`.
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
/// `session/set_mode` — change the agent's operating mode (client → agent).
pub const METHOD_SESSION_SET_MODE: &str = "session/set_mode";
/// `session/set_config_option` — set a configuration option (client → agent).
pub const METHOD_SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
/// `session/update` — streamed turn update (agent → client notification).
pub const METHOD_SESSION_UPDATE: &str = "session/update";
/// `session/cancel` — cancel the in-flight turn (client → agent notification).
pub const METHOD_SESSION_CANCEL: &str = "session/cancel";
/// `session/request_permission` — agent→client request for tool-call approval
/// (e.g. before a file write under default approval mode).
pub const METHOD_SESSION_REQUEST_PERMISSION: &str = "session/request_permission";

/// The protocol version Makina speaks (ACP v1).
pub const PROTOCOL_VERSION: u16 = 1;

// ── JSON-RPC 2.0 envelope ───────────────────────────────────────────────────────

/// A JSON-RPC 2.0 request/response id: a string OR a number (per the spec).
/// Untagged so `7` parses as `Number` and `"perm-1"` as `String`. An explicit
/// JSON `null` id still deserializes to `Option::None` (serde `Option`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id (the form Makina issues for its own requests).
    Number(u64),
    /// String id (some agents use these for server→client requests).
    String(String),
}

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
    pub id: Option<RequestId>,
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
        match (&self.id, self.method.as_deref()) {
            // Our correlated responses always carry the numeric id we issued.
            (Some(RequestId::Number(id)), None) => IncomingKind::Response { id: *id },
            // A notification has a method and no id.
            (None, Some(_)) => IncomingKind::Notification,
            // A server→client request has an id (string OR number) and a method.
            (Some(_), Some(_)) => IncomingKind::Request,
            // Anything else (incl. a bare string-id "response" we never issued).
            _ => IncomingKind::Malformed,
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

/// An outgoing JSON-RPC 2.0 **response** (reply to an inbound server→client
/// request such as `session/request_permission`).
///
/// Unlike requests, responses have no `method`; they echo the request `id` and
/// carry a `result` (permission decisions are always success results per the
/// ACP shape; error replies use the error branch of [`IncomingMessage`]).
#[derive(Debug, Clone, Serialize)]
pub struct OutgoingResponse<R: Serialize> {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// The id of the request this response answers.
    pub id: RequestId,
    /// Success result payload.
    pub result: R,
}

impl<R: Serialize> OutgoingResponse<R> {
    /// Construct a response with the JSON-RPC `2.0` tag pre-filled.
    pub fn new(id: RequestId, result: R) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result,
        }
    }
}

/// An outgoing JSON-RPC 2.0 **error response** (reply to an inbound
/// server→client request we do not implement). Sending this ensures the
/// peer's request does not hang forever.
#[derive(Debug, Clone, Serialize)]
pub struct OutgoingErrorResponse {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// The id of the request this error answers.
    pub id: RequestId,
    /// The JSON-RPC error payload.
    pub error: JsonRpcError,
}

impl OutgoingErrorResponse {
    /// Construct an error response with the JSON-RPC `2.0` tag pre-filled.
    pub fn new(id: RequestId, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            error,
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
    /// Authentication methods advertised by the agent.
    ///
    /// Per the Zed model, Makina **never calls the ACP `authenticate` method** —
    /// the CLI must already be signed in via its own flow before Makina spawns
    /// it. This field is retained for observability: it lets callers log what
    /// auth the agent uses and detect misconfigured (un-authenticated) agents
    /// early. An empty list means the agent requires no auth or is already
    /// authenticated.
    #[serde(default)]
    pub auth_methods: Vec<AuthMethod>,
}

/// An authentication method advertised in the `initialize` response.
///
/// The ACP spec allows an extensible set of auth method descriptors. Makina
/// surfaces the raw `type` string (camelCase, e.g. `"oauth"`, `"apiKey"`) and
/// preserves the rest of the fields as unstructured JSON for forward
/// compatibility. Callers MUST NOT use this to drive an auth flow; it is for
/// logging and diagnostics only.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthMethod {
    /// The auth method type identifier (e.g. `"oauth"`, `"apiKey"`).
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Any additional fields in the auth-method descriptor — preserved opaquely.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
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

/// A mode the agent can operate in (e.g., "code", "analyze", "explain").
#[derive(Debug, Clone, Deserialize)]
pub struct SessionMode {
    /// The mode's stable identifier.
    pub id: String,
    /// Human-readable name of the mode.
    pub name: String,
    /// Optional description of what this mode does.
    #[serde(default)]
    pub description: Option<String>,
}

/// The current mode and available modes for a session.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModeState {
    /// The id of the mode currently active.
    pub current_mode_id: String,
    /// All modes the agent supports.
    pub available_modes: Vec<SessionMode>,
}

/// Deserialize a JSON array the schema marks `nullish`, mapping an explicit
/// `null` **and** an omitted key to an empty `Vec`.
///
/// `#[serde(default)]` alone covers only the *omitted* case; an explicit `null`
/// still fails with `invalid type: null, expected a sequence`.  For a field on
/// the `session/new` result that failure rejects the entire result, so the
/// spawn fails and the task hard-errors — a `null` array would take down the
/// run.  Pair this with `#[serde(default)]` so both spellings are accepted.
fn nullable_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// A choice for a config option (e.g., a model variant).
#[derive(Debug, Clone, Deserialize)]
pub struct ConfigOptionChoice {
    /// The stable value identifier for this choice.
    pub value: String,
    /// Human-readable name of the choice.
    pub name: String,
    /// Optional description of what this choice does.
    #[serde(default)]
    pub description: Option<String>,
    /// Human-readable label of the group this choice was advertised under, when
    /// the agent groups its choices (e.g. models by provider); `None` for a flat
    /// choice list.
    ///
    /// Never read from the choice object itself — the schema's choice shape has
    /// no `group` field.  [`flattened_choices`] fills this in from the enclosing
    /// group while flattening, so the grouping the agent expressed is preserved
    /// rather than discarded.
    #[serde(skip)]
    pub group: Option<String>,
}

/// A group of choices, for agents that advertise their choices grouped (e.g.
/// models by provider) rather than as one flat list.
///
/// Not public: groups are flattened into [`ConfigOptionChoice`]s (each carrying
/// its group label) by [`flattened_choices`], so consumers only ever deal with
/// a flat list.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigOptionChoiceGroup {
    /// Stable group identifier.
    ///
    /// Not read after parsing — it is declared because it is what distinguishes
    /// the grouped shape from the flat one in [`flattened_choices`]'s untagged
    /// union.  Dropping it would let a flat choice satisfy this variant.
    #[expect(dead_code, reason = "required for the untagged shape discrimination")]
    group: String,
    /// Human-readable group label — preserved onto each flattened choice.
    name: String,
    /// The choices in this group.
    #[serde(default, deserialize_with = "nullable_vec")]
    options: Vec<ConfigOptionChoice>,
}

/// Deserialize a select option's `options`, which the schema models as a union
/// of **either** a flat choice array **or** an array of groups:
/// `union([array(Choice), array(Group)])`.
///
/// A group carries no `value` of its own, so modelling only the flat half made
/// a grouped list fail with `missing field \`value\`` — rejecting the whole
/// `session/new` result and failing the run.  Groups are flattened into the
/// choice list with their label preserved on each choice; no `value` is
/// invented for the group itself.  `null`/absent yields an empty list.
fn flattened_choices<'de, D>(deserializer: D) -> Result<Vec<ConfigOptionChoice>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    /// The two shapes `options` may take on the wire.  `Flat` is tried first;
    /// a group object has no `value`, so it can only match `Grouped`.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ChoiceList {
        Flat(Vec<ConfigOptionChoice>),
        Grouped(Vec<ConfigOptionChoiceGroup>),
    }

    Ok(match Option::<ChoiceList>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(ChoiceList::Flat(choices)) => choices,
        Some(ChoiceList::Grouped(groups)) => groups
            .into_iter()
            .flat_map(|group| {
                let label = group.name;
                group.options.into_iter().map(move |mut choice| {
                    choice.group = Some(label.clone());
                    choice
                })
            })
            .collect(),
    })
}

/// A configuration option the agent supports (e.g., model, reasoning effort).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    /// The stable identifier of this option.
    pub id: String,
    /// Human-readable name of the option.
    pub name: String,
    /// The category of this option (e.g., "model", "thought_level", "model_config").
    ///
    /// **Optional in the schema** — an agent may advertise an option with no
    /// category at all (an agent-specific option with no well-known meaning).
    /// This MUST stay `Option`: a required field here fails deserialization of
    /// the whole `session/new` result, which fails the session spawn and hard-
    /// errors the task — one uncategorized option would take down the run.
    /// `None` is preserved rather than defaulted to `""`, so a lookup for a
    /// well-known category never matches an option that declared none.
    #[serde(default)]
    pub category: Option<String>,
    /// The type of this option (e.g., "select", "boolean").
    #[serde(rename = "type")]
    pub kind: String,
    /// The current value of this option (if set).
    #[serde(default)]
    pub current_value: Option<serde_json::Value>,
    /// Available choices for this option (if it's a select type).
    ///
    /// Accepts both wire shapes the schema allows — a flat choice array or an
    /// array of groups — flattened into one list; see [`flattened_choices`].
    #[serde(default, deserialize_with = "flattened_choices")]
    pub options: Vec<ConfigOptionChoice>,
    /// Extra/unknown fields to preserve forward compatibility.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// `session/new` result (agent → client).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    /// The opaque session identifier the agent assigned.
    pub session_id: String,
    /// Optional mode state (if the agent advertises modes).
    #[serde(default)]
    pub modes: Option<SessionModeState>,
    /// Optional config options the agent advertises (model, effort, etc).
    ///
    /// The schema marks this array `nullish`, so an agent with nothing to
    /// advertise may send `"configOptions": null` just as legitimately as
    /// omitting the key; both must yield an empty list rather than failing the
    /// whole result.  See [`nullable_vec`].
    #[serde(default, deserialize_with = "nullable_vec")]
    pub config_options: Vec<ConfigOption>,
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
    /// Optional token usage, when the agent reports it.
    /// Absent for agents that don't emit it (the common case).
    #[serde(default)]
    pub usage: Option<TurnUsage>,
}

/// Token usage reported by the agent in a `session/prompt` result.
///
/// Both fields are optional because a backend may report one count without
/// the other. `#[serde(default)]` ensures missing fields parse as `None`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage {
    /// Prompt/input tokens consumed by the turn, if reported.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Completion/output tokens produced by the turn, if reported.
    #[serde(default)]
    pub output_tokens: Option<u64>,
}

/// `session/cancel` params (client → agent notification).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    /// Session whose in-flight turn should be cancelled.
    pub session_id: String,
}

/// `session/set_mode` params (client → agent request).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModeParams {
    /// The session to change the mode for.
    pub session_id: String,
    /// The mode id to switch to.
    pub mode_id: String,
}

/// `session/set_config_option` params (client → agent request).
///
/// The option identifier is `configId` on the wire — **not** `optionId`.  It
/// matches the `id` of a [`ConfigOption`] advertised in the `session/new`
/// result.  Sending `optionId` makes a spec-conforming agent reject the request
/// with JSON-RPC `-32602 Invalid params`, which fails the whole session spawn.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionParams {
    /// The session this option applies to.
    pub session_id: String,
    /// The option id to set — the `id` of an advertised [`ConfigOption`].
    pub config_id: String,
    /// The new value for this option.
    pub value: serde_json::Value,
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
/// chunk variant wraps a flattened [`ContentChunk`]; tool-call lifecycle updates
/// wrap [`ToolCall`] / [`ToolCallUpdate`]. Remaining update kinds Makina does not
/// consume (plans, …) collapse into [`SessionUpdate::Other`].
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// A chunk of the agent's assistant message — the text Makina collects.
    AgentMessageChunk(ContentChunk),
    /// A chunk of the agent's "thinking"/reasoning stream (ignored).
    AgentThoughtChunk(ContentChunk),
    /// A chunk echoing the user message (ignored).
    UserMessageChunk(ContentChunk),
    /// The agent initiated a tool call (`toolCallId`, `title`, `kind`, `status`,
    /// `content`, …). Shares the wire shape of the permission [`ToolCall`].
    ToolCall(ToolCall),
    /// Incremental status/result update for a previously-announced tool call.
    ToolCallUpdate(ToolCallUpdate),
    /// The agent changed to a different mode.
    #[serde(rename = "current_mode_update")]
    CurrentModeUpdate {
        /// The new current mode id.
        #[serde(rename = "currentModeId")]
        current_mode_id: String,
    },
    /// Any other update kind (plan, …) — ignored.
    #[serde(other)]
    Other,
}

/// A streamed content chunk (`{ "content": ContentBlock }`).
#[derive(Debug, Clone, Deserialize)]
pub struct ContentChunk {
    /// The content carried by this chunk.
    pub content: ContentBlock,
}

// ── ACP permission flow (server→client `session/request_permission`) ────────────

/// `session/request_permission` params (agent → client request).
///
/// The agent emits this before performing a privileged action (write, terminal
/// command, …) when running in a mode that requires user approval. Makina's
/// policy engine will pick one of the offered options (or cancel) and reply via
/// [`OutgoingResponse`]<[`PermissionResponse`]>.
///
/// Unknown fields under `toolCall` are preserved via a flattened map so that
/// future schema additions or agent-specific data survive deserialization.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    /// The session the permission request belongs to.
    pub session_id: String,
    /// The choices the user (or policy) may pick from.
    pub options: Vec<PermissionOption>,
    /// Description of the tool call that is pending approval.
    pub tool_call: ToolCall,
}

/// A single choice offered in a permission request.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    /// Stable id to echo back when selecting this option.
    pub option_id: String,
    /// Human label shown to the user.
    pub name: String,
    /// Semantic kind (drives icon / policy defaulting).
    pub kind: PermissionOptionKind,
}

/// Discriminator for a [`PermissionOption`].
///
/// Matches the values observed from real agents (`gemini --acp`) and the
/// `agent-client-protocol-schema` definition. Unknown future kinds map to
/// `Other` rather than failing the request.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    #[serde(other)]
    Other,
}

/// Partial view of the `toolCall` object inside [`RequestPermissionParams`].
///
/// Only the identity and common status fields are modelled directly. Every
/// other key that appears under `toolCall` in the wire payload (content,
/// locations, _meta, raw_input, kind-specific data, …) is retained verbatim
/// inside `extra` via `#[serde(flatten)]`. This satisfies the requirement that
/// an unknown tool-call field from a real payload must survive the round-trip.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    /// Every field not explicitly named above is captured here.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Partial view of a `tool_call_update` session update.
///
/// The wire shape is identical to the [`ToolCall`] announcement — both carry
/// `toolCallId`, optional lifecycle fields (`status`/`title`/`kind`), and an
/// open `extra` map for everything else. Kept as an alias so the two stay in
/// lock-step; promote to its own struct if the schemas ever diverge.
pub type ToolCallUpdate = ToolCall;

/// Response returned to the agent for a `session/request_permission` request.
///
/// Serializes to the exact nested shape the ACP schema (and real agents)
/// expect:
///   `{ "outcome": { "outcome": "selected", "optionId": "..." } }`
/// or
///   `{ "outcome": { "outcome": "cancelled" } }`
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionResponse {
    /// The decision envelope (double-"outcome" shape is required by ACP).
    pub outcome: PermissionOutcome,
}

/// The inner outcome of a permission decision.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    /// The policy selected one of the `optionId`s offered in the request.
    #[serde(rename_all = "camelCase")]
    Selected { option_id: String },
    /// The prompt turn was cancelled (or the policy chose to reject).
    Cancelled,
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
        // `tool_call` and `tool_call_update` are now modelled explicitly, so this
        // must exercise a genuinely-unknown discriminator to still prove the
        // `#[serde(other)]` fallback for future/unmodelled update kinds.
        let v = serde_json::json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "some_future_kind",
                "entries": [ { "content": "step one", "priority": "high" } ]
            }
        });
        let n: SessionNotificationParams = serde_json::from_value(v).unwrap();
        assert!(matches!(n.update, SessionUpdate::Other));
    }

    #[test]
    fn session_update_parses_tool_call_update_and_preserves_extra() {
        // Realistic `session/update` notification carrying a `tool_call_update`.
        // Includes several fields beyond our minimal model (content, rawInput,
        // and a made-up futureField) that must survive in the flatten map.
        let v = serde_json::json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-42",
                "status": "completed",
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "exit 0" } }
                ],
                "rawInput": { "command": "cargo test" },
                "futureField": "must survive round-trip"
            }
        });
        let n: SessionNotificationParams = serde_json::from_value(v).unwrap();
        assert_eq!(n.session_id, "sess-1");
        match n.update {
            SessionUpdate::ToolCallUpdate(upd) => {
                assert_eq!(upd.tool_call_id, "tc-42");
                assert_eq!(upd.status.as_deref(), Some("completed"));
                // Unknown/extra fields must survive (the key requirement).
                assert!(
                    upd.extra.contains_key("content"),
                    "content array must be preserved"
                );
                assert!(
                    upd.extra.contains_key("rawInput"),
                    "rawInput must be preserved"
                );
                assert!(
                    upd.extra.get("futureField").and_then(|v| v.as_str())
                        == Some("must survive round-trip"),
                    "future unknown field must be retained in the flatten map"
                );
                // Known fields must NOT leak into extra.
                assert!(!upd.extra.contains_key("toolCallId"));
                assert!(!upd.extra.contains_key("status"));
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn session_update_tool_call_update_allows_absent_optional_fields() {
        // Only the required identity field is present; every `#[serde(default)]`
        // optional must default cleanly and the flatten map must stay empty.
        let v = serde_json::json!({
            "sessionId": "sess-1",
            "update": { "sessionUpdate": "tool_call_update", "toolCallId": "tc-9" }
        });
        let n: SessionNotificationParams = serde_json::from_value(v).unwrap();
        match n.update {
            SessionUpdate::ToolCallUpdate(upd) => {
                assert_eq!(upd.tool_call_id, "tc-9");
                assert!(upd.status.is_none());
                assert!(upd.title.is_none());
                assert!(upd.kind.is_none());
                assert!(upd.extra.is_empty());
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn session_update_parses_tool_call() {
        // A `tool_call` session-update mirrors the permission `ToolCall` shape.
        let v = serde_json::json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1",
                "title": "running tests",
                "kind": "execute",
                "status": "pending",
                "rawInput": { "command": "cargo test" }
            }
        });
        let n: SessionNotificationParams = serde_json::from_value(v).unwrap();
        match n.update {
            SessionUpdate::ToolCall(tc) => {
                assert_eq!(tc.tool_call_id, "tc-1");
                assert_eq!(tc.title.as_deref(), Some("running tests"));
                assert_eq!(tc.kind.as_deref(), Some("execute"));
                assert_eq!(tc.status.as_deref(), Some("pending"));
                assert!(tc.extra.contains_key("rawInput"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
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
        assert!(
            r.auth_methods.is_empty(),
            "empty authMethods should parse as empty vec"
        );
    }

    #[test]
    fn initialize_result_parses_auth_methods() {
        // Agents may advertise one or more auth methods; each carries at least a type.
        let v = serde_json::json!({
            "protocolVersion": 1,
            "authMethods": [
                { "type": "oauth", "authorizationUrl": "https://example.com/auth" },
                { "type": "apiKey" }
            ],
            "agentInfo": { "name": "test-agent", "version": "0.1.0" }
        });
        let r: InitializeResult = serde_json::from_value(v).unwrap();
        assert_eq!(r.auth_methods.len(), 2);
        assert_eq!(r.auth_methods[0].kind, "oauth");
        assert_eq!(
            r.auth_methods[0]
                .extra
                .get("authorizationUrl")
                .and_then(|v| v.as_str()),
            Some("https://example.com/auth")
        );
        assert_eq!(r.auth_methods[1].kind, "apiKey");
    }

    #[test]
    fn initialize_result_without_auth_methods_defaults_to_empty() {
        // Older agents may omit authMethods entirely; the default must be an empty vec.
        let v = serde_json::json!({
            "protocolVersion": 1
        });
        let r: InitializeResult = serde_json::from_value(v).unwrap();
        assert!(
            r.auth_methods.is_empty(),
            "missing authMethods field must default to an empty vec"
        );
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

    #[test]
    fn request_permission_params_deserializes_real_payload_and_preserves_unknown_tool_call_fields()
    {
        // Real payload captured from gemini --acp (see real_cli.rs probe_permission_trigger).
        // Includes several "unknown" (to our minimal model) fields under toolCall:
        // content, locations, _meta, and an extra futureUnknownField.
        let v = serde_json::json!({
            "sessionId": "45efbc32-fa78-40ca-9637-5ea06d4c48e3",
            "options": [
                { "optionId": "proceed_always", "name": "Allow for this session", "kind": "allow_always" },
                { "optionId": "proceed_once",  "name": "Allow",                   "kind": "allow_once" },
                { "optionId": "cancel",        "name": "Reject",                  "kind": "reject_once" }
            ],
            "toolCall": {
                "toolCallId": "write_file__write_file_1780041520414_0",
                "status": "pending",
                "title": "Writing to fs-probe.txt",
                "content": [
                    { "type": "diff", "path": "/tmp/fs-probe.txt", "oldText": "", "newText": "PROBE-FS-TEST" }
                ],
                "locations": [ { "path": "/tmp/fs-probe.txt" } ],
                "kind": "edit",
                "_meta": { "kind": "add" },
                "futureUnknownField": "must survive round-trip"
            }
        });

        let p: RequestPermissionParams = serde_json::from_value(v).unwrap();
        assert_eq!(p.session_id, "45efbc32-fa78-40ca-9637-5ea06d4c48e3");
        assert_eq!(p.options.len(), 3);
        assert_eq!(p.options[0].option_id, "proceed_always");
        assert_eq!(p.options[0].kind, PermissionOptionKind::AllowAlways);
        assert_eq!(p.options[1].option_id, "proceed_once");
        assert_eq!(p.options[1].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(p.options[2].option_id, "cancel");
        assert_eq!(p.options[2].kind, PermissionOptionKind::RejectOnce);

        // ToolCall identity fields
        assert_eq!(
            p.tool_call.tool_call_id,
            "write_file__write_file_1780041520414_0"
        );
        assert_eq!(
            p.tool_call.title.as_deref(),
            Some("Writing to fs-probe.txt")
        );
        assert_eq!(p.tool_call.status.as_deref(), Some("pending"));

        // Unknown/extra fields under toolCall must survive (the key requirement).
        assert!(
            p.tool_call.extra.contains_key("content"),
            "content array must be preserved"
        );
        assert!(
            p.tool_call.extra.contains_key("locations"),
            "locations must be preserved"
        );
        assert!(
            p.tool_call.extra.contains_key("_meta"),
            "_meta must be preserved"
        );
        assert!(
            p.tool_call
                .extra
                .get("futureUnknownField")
                .and_then(|v| v.as_str())
                == Some("must survive round-trip"),
            "future unknown field must be retained in the flatten map"
        );
        // Known fields must NOT leak into extra.
        assert!(!p.tool_call.extra.contains_key("toolCallId"));
        assert!(!p.tool_call.extra.contains_key("title"));
    }

    #[test]
    fn permission_response_serializes_to_exact_acp_outcome_shape() {
        // Allow path — selected option.
        let allow = PermissionResponse {
            outcome: PermissionOutcome::Selected {
                option_id: "proceed_once".into(),
            },
        };
        let v = serde_json::to_value(&allow).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": "proceed_once" }
            })
        );

        // Cancel / reject path.
        let cancel = PermissionResponse {
            outcome: PermissionOutcome::Cancelled,
        };
        let v = serde_json::to_value(&cancel).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "outcome": { "outcome": "cancelled" }
            })
        );
    }

    #[test]
    fn outgoing_response_envelope_serializes_with_id_and_result() {
        // Sanity that the new envelope produces a well-formed JSON-RPC response.
        let resp = OutgoingResponse::new(
            RequestId::Number(42),
            PermissionResponse {
                outcome: PermissionOutcome::Selected {
                    option_id: "allow".into(),
                },
            },
        );
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 42);
        assert_eq!(v["result"]["outcome"]["outcome"], "selected");
        assert_eq!(v["result"]["outcome"]["optionId"], "allow");
    }

    #[test]
    fn incoming_string_id_request_classifies_as_request() {
        let msg: IncomingMessage = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0", "id": "perm-1", "method": "session/request_permission"
        }))
        .expect("a string-id request must decode");
        assert_eq!(msg.id, Some(RequestId::String("perm-1".to_string())));
        assert_eq!(msg.classify(), IncomingKind::Request);
    }

    #[test]
    fn session_new_parses_modes() {
        // A session/new result with modes → NewSessionResult.modes is Some with
        // 2 available_modes.
        let v = serde_json::json!({
            "sessionId": "sess-with-modes",
            "modes": {
                "currentModeId": "code",
                "availableModes": [
                    {
                        "id": "code",
                        "name": "Code Mode",
                        "description": "Write and debug code"
                    },
                    {
                        "id": "analyze",
                        "name": "Analyze Mode",
                        "description": "Analyze existing code"
                    }
                ]
            }
        });
        let result: NewSessionResult = serde_json::from_value(v).unwrap();
        assert_eq!(result.session_id, "sess-with-modes");
        assert!(result.modes.is_some());
        let modes = result.modes.unwrap();
        assert_eq!(modes.current_mode_id, "code");
        assert_eq!(modes.available_modes.len(), 2);
        assert_eq!(modes.available_modes[0].id, "code");
        assert_eq!(modes.available_modes[0].name, "Code Mode");
        assert_eq!(
            modes.available_modes[0].description.as_deref(),
            Some("Write and debug code")
        );
        assert_eq!(modes.available_modes[1].id, "analyze");
    }

    #[test]
    fn set_mode_serializes() {
        // SetModeParams → {"sessionId":…,"modeId":…}
        let params = SetModeParams {
            session_id: "sess-123".into(),
            mode_id: "analyze".into(),
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["sessionId"], "sess-123");
        assert_eq!(v["modeId"], "analyze");
    }

    #[test]
    fn current_mode_update_is_not_other() {
        // A current_mode_update notification parses to CurrentModeUpdate, not
        // Other.
        let v = serde_json::json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "current_mode_update",
                "currentModeId": "analyze"
            }
        });
        let n: SessionNotificationParams = serde_json::from_value(v).unwrap();
        assert_eq!(n.session_id, "sess-1");
        match n.update {
            SessionUpdate::CurrentModeUpdate { current_mode_id } => {
                assert_eq!(current_mode_id, "analyze");
            }
            other => panic!(
                "expected CurrentModeUpdate, got {other:?}; the serde rename may be incorrect"
            ),
        }
    }

    #[test]
    fn session_new_parses_config_options_and_preserves_unknown() {
        // A session/new result with config options including unknown categories
        // should parse successfully and preserve unknown fields.
        let v = serde_json::json!({
            "sessionId": "sess-with-options",
            "configOptions": [
                {
                    "id": "model_opt_1",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "grok-1",
                    "options": [
                        {
                            "value": "grok-1",
                            "name": "Grok 1",
                            "description": "Older model"
                        },
                        {
                            "value": "grok-2",
                            "name": "Grok 2",
                            "description": "Newer model"
                        }
                    ]
                },
                {
                    "id": "effort_opt_1",
                    "name": "Reasoning Effort",
                    "category": "thought_level",
                    "type": "select",
                    "currentValue": "medium",
                    "options": [
                        {
                            "value": "low",
                            "name": "Low"
                        },
                        {
                            "value": "medium",
                            "name": "Medium"
                        },
                        {
                            "value": "high",
                            "name": "High"
                        }
                    ]
                },
                {
                    "id": "unknown_opt",
                    "name": "Future Option",
                    "category": "unknown_category",
                    "type": "select",
                    "currentValue": "value1",
                    "futureField": "preserved"
                }
            ]
        });
        let result: NewSessionResult = serde_json::from_value(v).unwrap();
        assert_eq!(result.session_id, "sess-with-options");
        assert_eq!(result.config_options.len(), 3);

        // Check first option (model)
        assert_eq!(result.config_options[0].id, "model_opt_1");
        assert_eq!(result.config_options[0].name, "Model");
        assert_eq!(result.config_options[0].category.as_deref(), Some("model"));
        assert_eq!(result.config_options[0].kind, "select");
        assert_eq!(
            result.config_options[0]
                .current_value
                .as_ref()
                .and_then(|v| v.as_str()),
            Some("grok-1")
        );
        assert_eq!(result.config_options[0].options.len(), 2);
        assert_eq!(result.config_options[0].options[0].value, "grok-1");
        assert_eq!(result.config_options[0].options[0].name, "Grok 1");

        // Check second option (effort)
        assert_eq!(
            result.config_options[1].category.as_deref(),
            Some("thought_level")
        );
        assert_eq!(result.config_options[1].options.len(), 3);

        // Check that unknown category is preserved
        assert_eq!(
            result.config_options[2].category.as_deref(),
            Some("unknown_category")
        );
        assert!(
            result.config_options[2].extra.contains_key("futureField"),
            "unknown fields must be preserved in extra"
        );
        assert_eq!(
            result.config_options[2]
                .extra
                .get("futureField")
                .and_then(|v| v.as_str()),
            Some("preserved")
        );
    }

    /// A `session/new` result whose config option omits `category` must still
    /// deserialize — the ACP schema marks `category` optional.
    ///
    /// A required `category` here fails the whole `NewSessionResult` parse, so
    /// `AcpClient::connect` returns `invalid session/new result: …`,
    /// `AgentBackend::spawn` returns `Err`, and the task hard-errors with
    /// `developer backend spawn failed` — one uncategorized option advertised by
    /// the agent would take down the entire run.  The uncategorized option must
    /// also stay **invisible** to the well-known-category lookups (`model` /
    /// `thought_level`) rather than being defaulted into one of them.
    #[test]
    fn session_new_parses_config_option_without_category() {
        let v = serde_json::json!({
            "sessionId": "sess-no-category",
            "configOptions": [
                {
                    "id": "agent_specific_opt",
                    "name": "Agent Specific",
                    "type": "boolean",
                    "currentValue": true
                },
                {
                    "id": "model_opt",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "options": [{ "value": "m-1", "name": "Model 1" }]
                }
            ]
        });

        let result: NewSessionResult =
            serde_json::from_value(v).expect("a config option without `category` must parse");
        assert_eq!(result.config_options.len(), 2);

        // The uncategorized option parses with `category: None` — NOT `""`.
        assert_eq!(result.config_options[0].id, "agent_specific_opt");
        assert_eq!(
            result.config_options[0].category, None,
            "a missing `category` must stay absent, not be defaulted to a string"
        );
        assert_eq!(result.config_options[0].kind, "boolean");

        // The categorized option is unaffected.
        assert_eq!(result.config_options[1].category.as_deref(), Some("model"));

        // An uncategorized option must never be picked up by a category lookup —
        // this is the comparison the backend/client use to select model/effort.
        let uncategorized_matches_model = result
            .config_options
            .iter()
            .filter(|o| o.category.as_deref() == Some("model"))
            .count();
        assert_eq!(
            uncategorized_matches_model, 1,
            "only the genuinely `model`-category option may match a model lookup"
        );
    }

    /// A `session/new` result whose config option sends `category: null` must
    /// parse the same way as an omitted one — the schema is `nullish`, so an
    /// agent may send either.
    #[test]
    fn session_new_parses_config_option_with_null_category() {
        let v = serde_json::json!({
            "sessionId": "sess-null-category",
            "configOptions": [
                {
                    "id": "opt",
                    "name": "Option",
                    "category": null,
                    "type": "select"
                }
            ]
        });

        let result: NewSessionResult =
            serde_json::from_value(v).expect("a config option with `category: null` must parse");
        assert_eq!(result.config_options[0].category, None);
    }

    /// `configOptions: null` must parse as an empty list, exactly like omitting
    /// the key — the schema marks the array `nullish`.
    ///
    /// `#[serde(default)]` alone covers only omission; an explicit `null` failed
    /// with `invalid type: null, expected a sequence`, rejecting the whole
    /// `session/new` result -> `AcpClient::connect` Err -> `spawn` Err -> the
    /// task hard-errors and the run fails.  Any agent that serializes an empty
    /// `Option<Vec<_>>` without skip-if-none sends exactly this.
    #[test]
    fn session_new_parses_null_config_options() {
        // `modes: null` is covered too: it already worked (Option absorbs null),
        // and is asserted here so the two stay consistent.
        let v = serde_json::json!({
            "sessionId": "sess-null-options",
            "configOptions": null,
            "modes": null
        });

        let result: NewSessionResult =
            serde_json::from_value(v).expect("`configOptions: null` must parse as an empty list");
        assert!(
            result.config_options.is_empty(),
            "a null config-option array must yield an empty list, not fail the parse"
        );
        assert!(result.modes.is_none());

        // Omitting the key entirely must behave identically.
        let omitted: NewSessionResult =
            serde_json::from_value(serde_json::json!({ "sessionId": "s" }))
                .expect("an omitted `configOptions` must still parse");
        assert!(omitted.config_options.is_empty());
    }

    /// A select option may advertise its choices **grouped** (e.g. models by
    /// provider) instead of as one flat array — the schema's `options` is
    /// `union([array(Choice), array(Group)])` and a group carries no `value`.
    ///
    /// Modelling only the flat half failed a grouped list with
    /// `missing field \`value\``, rejecting the whole `session/new` result and
    /// failing the run.  Groups must flatten into the choice list with their
    /// label preserved, and no `value` invented for the group itself.
    #[test]
    fn session_new_parses_grouped_config_option_choices() {
        let v = serde_json::json!({
            "sessionId": "sess-grouped",
            "configOptions": [
                {
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "openai/gpt-5",
                    "options": [
                        {
                            "group": "openai",
                            "name": "OpenAI",
                            "options": [
                                { "value": "openai/gpt-5", "name": "GPT-5" },
                                { "value": "openai/gpt-5-mini", "name": "GPT-5 mini" }
                            ]
                        },
                        {
                            "group": "anthropic",
                            "name": "Anthropic",
                            "options": [
                                { "value": "anthropic/opus", "name": "Opus" }
                            ]
                        }
                    ]
                }
            ]
        });

        let result: NewSessionResult =
            serde_json::from_value(v).expect("a grouped choice list must parse");

        let opt = &result.config_options[0];
        assert_eq!(opt.category.as_deref(), Some("model"));

        // Groups are flattened: three real choices, no synthetic group entry.
        let values: Vec<&str> = opt.options.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(
            values,
            vec!["openai/gpt-5", "openai/gpt-5-mini", "anthropic/opus"],
            "every grouped choice must appear once, in order, with its own value"
        );

        // The group label rides along on each choice rather than being dropped.
        let groups: Vec<Option<&str>> = opt.options.iter().map(|c| c.group.as_deref()).collect();
        assert_eq!(
            groups,
            vec![Some("OpenAI"), Some("OpenAI"), Some("Anthropic")],
            "each flattened choice must carry its group's label"
        );
    }

    /// The flat choice shape must keep working unchanged alongside the grouped
    /// one, and `options: null` must yield an empty list.
    ///
    /// `ConfigOption` carries a `#[serde(flatten)] extra` map, and `flatten`
    /// routes deserialization through serde's buffered `Content` path — this
    /// pins that `deserialize_with` on a sibling field still works there, and
    /// that a consumed field is not also swallowed into `extra`.
    #[test]
    fn config_option_choices_flat_shape_and_null_survive_the_flatten_path() {
        let v = serde_json::json!({
            "sessionId": "sess-flat",
            "configOptions": [
                {
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "options": [
                        { "value": "m-1", "name": "Model 1", "description": "the first" }
                    ],
                    "futureField": "preserved"
                },
                {
                    "id": "toggle",
                    "name": "Toggle",
                    "type": "boolean",
                    "options": null
                }
            ]
        });

        let result: NewSessionResult =
            serde_json::from_value(v).expect("the flat shape must still parse");

        let flat = &result.config_options[0];
        assert_eq!(flat.options.len(), 1);
        assert_eq!(flat.options[0].value, "m-1");
        assert_eq!(flat.options[0].description.as_deref(), Some("the first"));
        assert_eq!(
            flat.options[0].group, None,
            "a flat choice has no group label"
        );

        // The flatten map still captures unknown fields, and does NOT also
        // capture the fields the named fields consumed.
        assert_eq!(
            flat.extra.get("futureField").and_then(|v| v.as_str()),
            Some("preserved")
        );
        for consumed in ["id", "name", "category", "type", "options"] {
            assert!(
                !flat.extra.contains_key(consumed),
                "`{consumed}` is a named field and must not also land in `extra`"
            );
        }

        // `options: null` on the boolean option yields an empty list.
        assert!(result.config_options[1].options.is_empty());
    }

    #[test]
    fn set_config_option_serializes() {
        // SetConfigOptionParams should serialize to the correct JSON-RPC shape.
        // The option identifier is `configId` — an agent that validates its
        // params rejects `optionId` with -32602, failing the session spawn.
        let params = SetConfigOptionParams {
            session_id: "sess-456".into(),
            config_id: "model_opt_1".into(),
            value: serde_json::json!("grok-2"),
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["sessionId"], "sess-456");
        assert_eq!(v["configId"], "model_opt_1");
        assert_eq!(v["value"], "grok-2");
        assert!(
            v.get("optionId").is_none(),
            "`optionId` is not the wire name; agents reject it with -32602"
        );

        // Test with a non-string value
        let params2 = SetConfigOptionParams {
            session_id: "sess-789".into(),
            config_id: "effort_opt_1".into(),
            value: serde_json::json!(42),
        };
        let v2 = serde_json::to_value(&params2).unwrap();
        assert_eq!(v2["sessionId"], "sess-789");
        assert_eq!(v2["configId"], "effort_opt_1");
        assert_eq!(v2["value"], 42);
    }

    /// A `PromptResult` JSON without a `usage` field must deserialize to
    /// `usage: None` (the serde default).  This verifies that the ACP backend
    /// never invents usage counts when the agent omits the field (no estimation).
    #[test]
    fn usage_is_none_when_backend_omits_it() {
        // JSON that a real ACP agent would send when it does not report usage.
        let json = serde_json::json!({
            "stopReason": "end_turn"
        });
        let result: PromptResult =
            serde_json::from_value(json).expect("PromptResult must deserialize without usage");
        assert!(
            result.usage.is_none(),
            "usage must be None when the backend omits the field"
        );

        // Also verify that `TurnUsage` fields default to None individually.
        let json_partial = serde_json::json!({
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 50
            }
        });
        let result2: PromptResult = serde_json::from_value(json_partial)
            .expect("PromptResult must deserialize with partial usage");
        let usage = result2.usage.as_ref().expect("usage must be Some");
        assert_eq!(usage.input_tokens, Some(50));
        assert!(
            usage.output_tokens.is_none(),
            "output_tokens must be None when not reported"
        );
    }
}
