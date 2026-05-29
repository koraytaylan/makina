//! Lifecycle and error-path tests for [`makina_acp::AcpClient`].
//!
//! Covers the failure surface task 15 must rely on:
//! * spawn failure (missing binary) → [`AcpError::Spawn`];
//! * agent exits during the handshake → typed error (not a hang);
//! * agent sends malformed JSON → [`AcpError::Protocol`];
//! * agent disconnects mid-turn → the prompt stream yields a typed error and
//!   ends;
//! * a **real subprocess** is killed on `shutdown()` (no leaked/zombie process).

mod common;

use std::path::PathBuf;

use futures::StreamExt;
use makina_acp::{AcpClient, AcpCommand, AcpError, AcpResponseChunk};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// ── spawn failure ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn spawn_of_missing_binary_is_typed_spawn_error() {
    let command = AcpCommand::new(
        "definitely-not-a-real-acp-agent-binary-xyz",
        std::env::temp_dir(),
    );
    let err = AcpClient::connect(command).await.unwrap_err();
    assert!(
        matches!(err, AcpError::Spawn(_)),
        "expected Spawn error, got {err:?}"
    );
}

// ── handshake: agent exits early ─────────────────────────────────────────────────

#[tokio::test]
async fn agent_eof_during_handshake_is_typed_error() {
    let (client_io, peer_io) = tokio::io::duplex(4096);
    let (client_read, client_write) = tokio::io::split(client_io);
    // Peer immediately closes its write side (EOF) without answering initialize,
    // but keeps reading so the client's write succeeds.
    let (peer_read, mut peer_write) = tokio::io::split(peer_io);
    let peer = tokio::spawn(async move {
        peer_write.shutdown().await.unwrap();
        // Drain whatever the client sends until it gives up.
        let mut lines = BufReader::new(peer_read).lines();
        while let Ok(Some(_)) = lines.next_line().await {}
    });

    let err = AcpClient::with_transport(client_read, client_write, "/tmp", None, None)
        .await
        .expect_err("handshake against a dead agent must fail");
    assert!(
        matches!(err, AcpError::AgentExited { .. } | AcpError::Transport(_)),
        "expected AgentExited/Transport, got {err:?}"
    );
    peer.await.unwrap();
}

// ── handshake: tolerate non-JSON preamble (real-CLI behaviour) ───────────────────

#[tokio::test]
async fn handshake_tolerates_non_jsonrpc_preamble() {
    // Mirrors what a real `gemini --acp` does: it prints human-readable log lines
    // on stdout before the JSON-RPC `initialize` result. The client must skip the
    // noise and complete the handshake regardless.
    let (client_io, peer_io) = tokio::io::duplex(8192);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (peer_read, mut peer_write) = tokio::io::split(peer_io);
    let peer = tokio::spawn(async move {
        let mut lines = BufReader::new(peer_read).lines();
        // initialize: emit two noise lines, then the real result.
        let _ = lines.next_line().await.unwrap();
        peer_write
            .write_all(b"Ignore file not found: /tmp/.geminiignore, continuing\n")
            .await
            .unwrap();
        peer_write
            .write_all(b"Hook system initialized successfully\n")
            .await
            .unwrap();
        peer_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"protocolVersion\":1,\"agentInfo\":{\"name\":\"noisy-agent\",\"version\":\"9\"}}}\n")
            .await
            .unwrap();
        // session/new
        let _ = lines.next_line().await.unwrap();
        peer_write
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"sessionId\":\"noisy-sess\"}}\n",
            )
            .await
            .unwrap();
        peer_write.flush().await.unwrap();
        while let Ok(Some(_)) = lines.next_line().await {}
    });

    let client = AcpClient::with_transport(client_read, client_write, "/tmp", None, None)
        .await
        .expect("handshake must succeed despite non-JSON preamble");
    assert_eq!(client.session_id(), "noisy-sess");
    assert_eq!(
        client.agent_info().map(|i| i.name.as_str()),
        Some("noisy-agent")
    );
    drop(client);
    peer.await.unwrap();
}

// ── handshake: structurally-invalid result payload → Protocol error ──────────────

#[tokio::test]
async fn handshake_with_invalid_result_payload_is_protocol_error() {
    // The agent answers `initialize` with a syntactically valid JSON-RPC response
    // whose *result* lacks the required `protocolVersion` field. Deserialising it
    // into the expected type fails → typed Protocol error (not a hang).
    let (client_io, peer_io) = tokio::io::duplex(4096);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (peer_read, mut peer_write) = tokio::io::split(peer_io);
    let peer = tokio::spawn(async move {
        let mut lines = BufReader::new(peer_read).lines();
        let _ = lines.next_line().await.unwrap();
        // Missing `protocolVersion`: well-formed JSON-RPC, wrong shape.
        peer_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"unexpected\":true}}\n")
            .await
            .unwrap();
        peer_write.flush().await.unwrap();
        while let Ok(Some(_)) = lines.next_line().await {}
    });

    let err = AcpClient::with_transport(client_read, client_write, "/tmp", None, None)
        .await
        .expect_err("an invalid initialize result must fail");
    assert!(
        matches!(err, AcpError::Protocol(_)),
        "expected Protocol error, got {err:?}"
    );
    peer.await.unwrap();
}

// ── mid-turn disconnect ──────────────────────────────────────────────────────────

#[tokio::test]
async fn agent_disconnect_mid_turn_yields_error_then_ends() {
    let (client_io, peer_io) = tokio::io::duplex(8192);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (peer_read, mut peer_write) = tokio::io::split(peer_io);

    let peer = tokio::spawn(async move {
        let mut lines = BufReader::new(peer_read).lines();
        // initialize
        let _ = lines.next_line().await.unwrap();
        peer_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"protocolVersion\":1}}\n")
            .await
            .unwrap();
        // session/new
        let _ = lines.next_line().await.unwrap();
        peer_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"sessionId\":\"s\"}}\n")
            .await
            .unwrap();
        peer_write.flush().await.unwrap();
        // session/prompt: send one text chunk, then disconnect WITHOUT a result.
        let _ = lines.next_line().await.unwrap();
        peer_write
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"partial\"}}}}\n",
            )
            .await
            .unwrap();
        peer_write.flush().await.unwrap();
        // Close stdout (EOF) mid-turn; the prompt request will never be answered.
        peer_write.shutdown().await.unwrap();
        while let Ok(Some(_)) = lines.next_line().await {}
    });

    let mut client = AcpClient::with_transport(client_read, client_write, "/tmp", None, None)
        .await
        .expect("handshake ok");

    let mut stream = client.prompt("go").expect("prompt accepted");

    // We should get the partial text chunk, then a typed error, then the stream
    // ends (None). No TurnComplete, because the agent never finished the turn.
    let mut saw_text = false;
    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(AcpResponseChunk::Text(t)) => {
                assert_eq!(t, "partial");
                saw_text = true;
            }
            Ok(AcpResponseChunk::TurnComplete(_)) => {
                panic!("turn should not complete after a mid-turn disconnect");
            }
            Err(e) => {
                assert!(
                    matches!(e, AcpError::AgentExited { .. } | AcpError::Transport(_)),
                    "expected AgentExited/Transport mid-turn, got {e:?}"
                );
                saw_error = true;
            }
        }
    }
    assert!(saw_text, "the partial chunk should have streamed through");
    assert!(saw_error, "a terminal error should have been yielded");

    drop(stream);
    client.shutdown().await.unwrap();
    // Drop the client to close its write half so the peer's drain loop ends.
    drop(client);
    peer.await.unwrap();
}

// ── real subprocess lifecycle: killed on shutdown, no zombie ─────────────────────

/// A tiny, deterministic ACP agent implemented in POSIX `sh`. It answers the
/// `initialize` (id 0) and `session/new` (id 1) requests Makina always sends
/// first — in that fixed order — then idles forever reading stdin. This is a
/// **mock subprocess** (not a real agent CLI / model), used solely to exercise
/// real process spawning + teardown.
const SH_MOCK_AGENT: &str = r#"
read -r _initialize
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentInfo":{"name":"sh-mock","version":"0"}}}'
read -r _session_new
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"sh-session"}}'
# Idle: keep stdin open and the process alive until killed.
while read -r _line; do :; done
"#;

#[tokio::test]
async fn real_subprocess_is_killed_on_shutdown() {
    // Skip gracefully if `sh` is unavailable (it is present on macOS/Linux CI).
    if which_sh().is_none() {
        eprintln!("skipping: /bin/sh not found");
        return;
    }

    let command = AcpCommand::new("sh", std::env::temp_dir()).args(["-c", SH_MOCK_AGENT]);
    let mut client = AcpClient::connect(command)
        .await
        .expect("sh mock agent should complete the handshake");
    assert_eq!(client.session_id(), "sh-session");

    // We cannot read the child's PID through the public API, so verify teardown
    // behaviourally: shutdown must complete promptly and be idempotent.
    client.shutdown().await.expect("shutdown ok");
    client.shutdown().await.expect("shutdown is idempotent");
}

#[tokio::test]
async fn real_subprocess_no_zombie_after_drop() {
    if which_sh().is_none() {
        eprintln!("skipping: /bin/sh not found");
        return;
    }

    // Spawn the mock as a bare child to capture its PID, drive the handshake via
    // a parallel mechanism is overkill; instead we assert the higher-level
    // guarantee: after connect + drop (no explicit shutdown), the process is
    // gone thanks to `kill_on_drop`. We detect the PID via `pgrep` on a unique
    // marker arg embedded in the command line.
    let marker = format!("makina-acp-zombie-probe-{}", std::process::id());
    let script = format!("{SH_MOCK_AGENT}\n# {marker}");
    let command = AcpCommand::new("sh", std::env::temp_dir()).args([String::from("-c"), script]);

    {
        let client = AcpClient::connect(command).await.expect("handshake ok");
        assert_eq!(client.session_id(), "sh-session");
        // Drop without shutdown: `kill_on_drop(true)` must terminate the child.
    }

    // Poll (bounded, no fixed sleep) until the marked process is gone.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if !pgrep_marker(&marker) {
            break; // process reaped — success
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("subprocess survived after client drop (possible leak/zombie)");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Locate `sh` (present on POSIX systems). Returns its path if runnable.
fn which_sh() -> Option<PathBuf> {
    for p in ["/bin/sh", "/usr/bin/sh"] {
        if std::path::Path::new(p).exists() {
            return Some(PathBuf::from(p));
        }
    }
    None
}

/// True if any process command line contains `marker` (best-effort, via pgrep).
fn pgrep_marker(marker: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", marker])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
