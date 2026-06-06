//! ACP client for Makina.
//!
//! This crate speaks the [Agent Client Protocol (ACP)][acp] to an external,
//! already-authenticated agent CLI (e.g. `claude-code-acp`, Gemini's
//! `--experimental-acp`, or any Zed-compatible agent). It spawns the CLI as a
//! subprocess, performs the `initialize` handshake, opens a session, sends a
//! prompt, and streams the assistant's response back chunk-by-chunk — then tears
//! the subprocess down cleanly.
//!
//! [acp]: https://agentclientprotocol.com/
//!
//! # Scope
//!
//! This crate provides both the **low-level client** (Task 13 — `acp-client`,
//! [`AcpClient`]) and the **`makina-core` backend adapter** (Task 15 —
//! `acp-backend-impl`, [`AcpBackend`] / [`AcpSession`] in [`backend`]) that maps
//! the client onto `makina_core::backend::AgentBackend` / `AgentSession`.
//! Credential handling is deliberately **out of scope** (the Zed model: the CLI
//! is already signed in and Makina inherits its environment — see [`AcpCommand`]).
//!
//! # Protocol subset
//!
//! Only the methods needed for one text turn are implemented, over
//! **newline-delimited JSON-RPC 2.0**:
//!
//! | Method            | Direction        | Purpose                          |
//! |-------------------|------------------|----------------------------------|
//! | `initialize`      | client → agent   | capability negotiation           |
//! | `session/new`     | client → agent   | create a session in a `cwd`      |
//! | `session/prompt`  | client → agent   | send one user turn               |
//! | `session/update`  | agent → client   | streamed assistant text chunks   |
//! | `session/cancel`  | client → agent   | (available) cancel the turn      |
//!
//! The wire shapes mirror the official Zed `agent-client-protocol-schema` crate
//! (protocol v1); see [`protocol`] for the field-by-field mapping and citations.
//!
//! # Example (over a real CLI)
//!
//! ```no_run
//! use makina_acp::{AcpClient, AcpCommand, AcpResponseChunk};
//! use futures::StreamExt;
//!
//! # async fn run() -> Result<(), makina_acp::AcpError> {
//! // Spawn an ACP agent (already authenticated; env is inherited).
//! let command = AcpCommand::new("claude-code-acp", "/path/to/repo");
//! let mut client = AcpClient::connect(command).await?;
//!
//! let mut answer = String::new();
//! let mut stream = client.prompt("Write a haiku about Rust.")?;
//! while let Some(item) = stream.next().await {
//!     match item? {
//!         AcpResponseChunk::Text(t) => answer.push_str(&t),
//!         // Thoughts and tool-call activity arrive as a side channel.
//!         AcpResponseChunk::Thought(_)
//!         | AcpResponseChunk::ToolCall { .. }
//!         | AcpResponseChunk::ToolCallUpdate { .. } => {}
//!         AcpResponseChunk::TurnComplete(_reason) => break,
//!     }
//! }
//! drop(stream);
//!
//! client.shutdown().await?;
//! println!("{answer}");
//! # Ok(())
//! # }
//! ```
//!
//! # Testability
//!
//! Protocol logic is separated from process spawning: the [`transport`] layer
//! runs over any `AsyncRead + AsyncWrite`, and [`AcpClient::with_transport`]
//! connects over an injected stream. Tests drive the full exchange against a
//! mock agent over a [`tokio::io::duplex`] pipe — no real CLI required (see the
//! crate's integration tests).

pub mod backend;
pub mod client;
pub mod error;
pub mod permission;
pub mod protocol;
pub mod reaper;
pub mod transport;

pub use backend::{AcpBackend, AcpSession};
pub use client::{AcpClient, AcpCommand, AcpResponseChunk, PromptStream};
pub use error::{AcpError, Result};
pub use permission::{
    PermissionDecision, PermissionPolicy, PermissionRequestContext, WorktreePolicy,
};
pub use protocol::{AuthMethod, StopReason};
pub use reaper::kill_all_agents;
#[cfg(any(test, feature = "test-util"))]
#[doc(hidden)]
pub use reaper::{register_for_test, registered_pgids_for_test, set_kill_fn_for_test};
pub use transport::{Transport, TransportSender};
