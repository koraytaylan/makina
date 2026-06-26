//! Typed errors for the ACP client.
//!
//! Every fallible operation on [`AcpClient`](crate::AcpClient) and its session
//! returns an [`AcpError`]. The variants separate the failure *domains* that a
//! caller (notably task 15, which maps this client onto
//! `makina_core::backend::BackendError`) needs to distinguish:
//!
//! | `AcpError`           | maps naturally to `BackendError` |
//! |----------------------|----------------------------------|
//! | [`Spawn`]            | `Spawn`                          |
//! | [`Transport`]        | `Transport`                      |
//! | [`Protocol`]         | `Transport`                      |
//! | [`Rpc`]              | `Transport`                      |
//! | [`AgentExited`]      | `Transport`                      |
//! | [`TurnTimeout`]      | `Transport`                      |
//! | [`Closed`]           | `Terminated`                     |
//!
//! [`Spawn`]: AcpError::Spawn
//! [`Transport`]: AcpError::Transport
//! [`Protocol`]: AcpError::Protocol
//! [`Rpc`]: AcpError::Rpc
//! [`AgentExited`]: AcpError::AgentExited
//! [`TurnTimeout`]: AcpError::TurnTimeout
//! [`Closed`]: AcpError::Closed

use crate::protocol::JsonRpcError;

/// An error from any ACP client operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AcpError {
    /// The agent subprocess could not be spawned (binary missing, permission
    /// denied, bad working directory, failure to capture stdio, …).
    #[error("failed to spawn ACP agent process: {0}")]
    Spawn(String),

    /// A transport-level I/O failure on the stdio pipes (broken pipe, read/write
    /// error). The connection is unusable after this; callers should close it.
    #[error("ACP transport I/O error: {0}")]
    Transport(String),

    /// A message could not be (de)serialised or violated the protocol shape
    /// (malformed JSON on a line, a response with neither result nor error,
    /// an unexpected message, …).
    #[error("ACP protocol error: {0}")]
    Protocol(String),

    /// The agent returned a JSON-RPC error object in reply to one of our
    /// requests (e.g. `initialize`/`session/new`/`session/prompt` rejected).
    #[error("agent returned an error: {0}")]
    Rpc(#[from] JsonRpcError),

    /// The agent process exited (or its stdout closed) before completing the
    /// expected exchange. Carries any captured stderr tail for diagnostics.
    #[error("ACP agent exited unexpectedly{}{}",
        if .status.is_empty() { String::new() } else { format!(" ({})", .status) },
        if .stderr.is_empty() { String::new() } else { format!(": {}", .stderr) })]
    AgentExited {
        /// Description of the exit status, if known (e.g. `"exit code 1"`).
        status: String,
        /// Tail of the agent's stderr, captured for diagnostics.
        stderr: String,
    },

    /// The client (or session) has been closed/terminated and can no longer be
    /// used. Operations after [`AcpClient::shutdown`](crate::AcpClient::shutdown)
    /// return this.
    #[error("ACP client is closed")]
    Closed,

    /// A bounded control RPC exceeded its deadline without a matching response.
    #[error("agent turn timed out after {secs}s")]
    TurnTimeout { secs: u64 },
}

impl AcpError {
    /// Wrap a [`std::io::Error`] as a [`AcpError::Transport`].
    pub(crate) fn transport(err: impl std::fmt::Display) -> Self {
        AcpError::Transport(err.to_string())
    }

    /// Wrap a serialisation/shape problem as a [`AcpError::Protocol`].
    pub(crate) fn protocol(msg: impl std::fmt::Display) -> Self {
        AcpError::Protocol(msg.to_string())
    }
}

/// Convenience result alias for ACP client operations.
pub type Result<T> = std::result::Result<T, AcpError>;
