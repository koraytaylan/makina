//! Task-15 acceptance: a prompt round-trips through `makina-acp` **behind the
//! `makina_core::backend` trait**.
//!
//! These tests build an [`AcpSession`](makina_acp::AcpSession) from an
//! [`AcpClient`](makina_acp::AcpClient) connected to the in-memory mock ACP agent
//! (`tests/common`) — no subprocess — and then exercise everything through the
//! `AgentBackend` / `AgentSession` **trait objects** (`Box<dyn …>`), proving the
//! mapping rather than the concrete types.
//!
//! Coverage:
//! * round-trip: `prompt` → drain `ResponseStream` → `TextChunk`s assemble to the
//!   expected text and the LAST item is `TurnComplete`; then `terminate`;
//! * contract — post-terminate `prompt` returns `BackendError::Terminated`;
//! * contract — `terminate` is idempotent;
//! * contract — a mid-turn error surfaces `BackendError::Transport` (never a
//!   false `TurnComplete`);
//! * lifecycle — dropping the `ResponseStream` early does not leak/hang (the
//!   client is reclaimed, so the next call still works);
//! * system prompt — prepended to the first turn only.

mod common;

use std::sync::{Arc, Mutex};

use common::{MockBehavior, spawn_mock_agent, spawn_mock_agent_recording};
use futures::StreamExt;
use makina_acp::AcpClient;
use makina_acp::backend::AcpSession;
use makina_core::backend::{AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// ── helpers ──────────────────────────────────────────────────────────────────

/// Connect an [`AcpClient`] to a mock agent and return it wrapped as a
/// `Box<dyn AgentSession>` so tests only touch the trait surface.
async fn session_over_mock(behavior: MockBehavior, system_prompt: &str) -> Box<dyn AgentSession> {
    let (reader, writer, _mock) = spawn_mock_agent(behavior);
    let client = AcpClient::with_transport(reader, writer, "/tmp/repo")
        .await
        .expect("handshake should succeed");
    Box::new(AcpSession::from_client(client, system_prompt))
}

/// Drain a [`ResponseStream`] into the concatenated `TextChunk` text, asserting
/// the final item is exactly one `TurnComplete` and no item is an error.
async fn drain_ok(stream: ResponseStream) -> (String, /* turn_complete_count */ usize) {
    let mut text = String::new();
    let mut completes = 0;
    let mut saw_complete_last = false;
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item.expect("no error item expected on a clean turn") {
            ResponseEvent::TextChunk { text: chunk } => {
                assert!(!saw_complete_last, "no chunk may follow TurnComplete");
                text.push_str(&chunk);
            }
            ResponseEvent::TurnComplete => {
                completes += 1;
                saw_complete_last = true;
            }
        }
    }
    assert!(
        saw_complete_last,
        "the stream MUST end with TurnComplete as its final item"
    );
    (text, completes)
}

// ── round-trip behind the trait ───────────────────────────────────────────────

#[tokio::test]
async fn prompt_round_trips_through_the_trait() {
    let behavior = MockBehavior {
        session_id: "sess-trait".into(),
        chunks: vec!["The ".into(), "answer ".into(), "is ".into(), "42.".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 0,
        ..MockBehavior::default()
    };
    // Drive everything through the trait object.
    let mut session = session_over_mock(behavior, "").await;

    let stream = session
        .prompt(Prompt::new("What is the answer?"))
        .await
        .expect("prompt should be accepted");
    let (text, completes) = drain_ok(stream).await;

    assert_eq!(
        text, "The answer is 42.",
        "chunks assemble in arrival order"
    );
    assert_eq!(completes, 1, "exactly one TurnComplete");

    // Terminate via the trait; idempotent second call.
    session.terminate().await.expect("terminate ok");
    session.terminate().await.expect("terminate idempotent");
}

#[tokio::test]
async fn non_text_updates_are_skipped_behind_the_trait() {
    // Leading tool-call noise updates must not leak into the mapped stream.
    let behavior = MockBehavior {
        session_id: "sess-noise".into(),
        chunks: vec!["clean ".into(), "text".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 3,
        ..MockBehavior::default()
    };
    let mut session = session_over_mock(behavior, "").await;

    let stream = session.prompt(Prompt::new("go")).await.unwrap();
    let (text, completes) = drain_ok(stream).await;

    assert_eq!(text, "clean text");
    assert_eq!(completes, 1);
    session.terminate().await.unwrap();
}

#[tokio::test]
async fn two_sequential_turns_through_the_trait() {
    // The mock answers every prompt with the same chunks; the bridge must hand
    // the client back between turns so the second prompt succeeds.
    let behavior = MockBehavior {
        session_id: "sess-multi".into(),
        chunks: vec!["pong".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 0,
        ..MockBehavior::default()
    };
    let mut session = session_over_mock(behavior, "").await;

    let (t1, c1) = drain_ok(session.prompt(Prompt::new("ping 1")).await.unwrap()).await;
    assert_eq!(t1, "pong");
    assert_eq!(c1, 1);

    let (t2, c2) = drain_ok(session.prompt(Prompt::new("ping 2")).await.unwrap()).await;
    assert_eq!(t2, "pong");
    assert_eq!(c2, 1);

    session.terminate().await.unwrap();
}

// ── contract: post-terminate prompt → Terminated ──────────────────────────────

#[tokio::test]
async fn prompt_after_terminate_returns_terminated() {
    let behavior = MockBehavior::default();
    let mut session = session_over_mock(behavior, "").await;

    session.terminate().await.expect("terminate ok");

    // The Ok variant (a boxed stream) is not Debug, so match rather than
    // `assert!(matches!(.., "{result:?}"))`.
    match session.prompt(Prompt::new("too late")).await {
        Err(BackendError::Terminated) => {}
        Err(other) => panic!("prompt after terminate must return Terminated, got {other:?}"),
        Ok(_) => panic!("prompt after terminate must NOT return a stream"),
    }
}

// ── contract: terminate is idempotent (even without any prompt) ────────────────

#[tokio::test]
async fn terminate_is_idempotent_without_a_prompt() {
    let behavior = MockBehavior::default();
    let mut session = session_over_mock(behavior, "").await;

    session.terminate().await.expect("first terminate ok");
    session.terminate().await.expect("second terminate ok");
    session.terminate().await.expect("third terminate ok");
}

// ── contract: mid-turn error surfaces Transport, not a false TurnComplete ──────

#[tokio::test]
async fn mid_turn_disconnect_surfaces_transport_error_no_false_turn_complete() {
    // A hand-rolled peer that completes the handshake, streams one chunk, then
    // closes stdout WITHOUT sending the prompt result — exactly the mid-turn
    // disconnect task 13 tests at the client level, here asserted through the
    // trait's mapped BackendError.
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
        // session/prompt: one chunk, then EOF (no result).
        let _ = lines.next_line().await.unwrap();
        peer_write
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"partial\"}}}}\n",
            )
            .await
            .unwrap();
        peer_write.flush().await.unwrap();
        peer_write.shutdown().await.unwrap();
        while let Ok(Some(_)) = lines.next_line().await {}
    });

    let client = AcpClient::with_transport(client_read, client_write, "/tmp")
        .await
        .expect("handshake ok");
    let mut session: Box<dyn AgentSession> = Box::new(AcpSession::from_client(client, ""));

    let mut stream = session
        .prompt(Prompt::new("go"))
        .await
        .expect("prompt accepted");

    let mut saw_text = false;
    let mut saw_transport_err = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(ResponseEvent::TextChunk { text }) => {
                assert_eq!(text, "partial");
                saw_text = true;
            }
            Ok(ResponseEvent::TurnComplete) => {
                panic!("must NOT emit TurnComplete after a mid-turn disconnect");
            }
            Err(BackendError::Transport { .. }) => saw_transport_err = true,
            Err(other) => panic!("expected Transport error, got {other:?}"),
        }
    }
    assert!(saw_text, "the partial chunk should stream through");
    assert!(
        saw_transport_err,
        "a Transport error item should terminate the turn"
    );

    // The session is still usable for terminate (client reclaimed even on error).
    drop(stream);
    session.terminate().await.expect("terminate after error ok");
    peer.await.unwrap();
}

// ── lifecycle: early drop of the ResponseStream doesn't leak/hang ──────────────

#[tokio::test]
async fn early_drop_of_response_stream_reclaims_client() {
    // Many chunks so there is plenty left to stream when we drop early.
    let behavior = MockBehavior {
        session_id: "sess-drop".into(),
        chunks: (0..100).map(|i| format!("chunk-{i} ")).collect(),
        stop_reason: "end_turn".into(),
        leading_noise_updates: 0,
        ..MockBehavior::default()
    };
    let mut session = session_over_mock(behavior, "").await;

    // Take only the first item, then drop the stream mid-turn.
    {
        let mut stream = session.prompt(Prompt::new("first")).await.unwrap();
        let first = stream.next().await;
        assert!(
            matches!(first, Some(Ok(ResponseEvent::TextChunk { .. }))),
            "expected a first chunk, got {first:?}"
        );
        // `stream` dropped here → forwarding task observes the closed receiver and
        // returns the client.
    }

    // The client must have been reclaimed: terminate completes promptly (no hang)
    // and is idempotent. (A hang here would fail the test by timing out the
    // whole suite; reclaim_client awaiting the worker's oneshot proves liveness.)
    session
        .terminate()
        .await
        .expect("terminate after early drop ok");
    session.terminate().await.expect("idempotent");
}

#[tokio::test]
async fn prompt_after_early_drop_still_works() {
    // Prove the client is genuinely reclaimed (not just that terminate works):
    // a SECOND prompt after an early drop must round-trip.
    let behavior = MockBehavior {
        session_id: "sess-drop2".into(),
        chunks: vec!["a ".into(), "b ".into(), "c".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 0,
        ..MockBehavior::default()
    };
    let mut session = session_over_mock(behavior, "").await;

    {
        let mut stream = session.prompt(Prompt::new("turn-1")).await.unwrap();
        let _first = stream.next().await; // consume one, then drop
    }

    // Second turn on the same (reclaimed) client.
    let stream = session.prompt(Prompt::new("turn-2")).await.unwrap();
    let (text, completes) = drain_ok(stream).await;
    assert_eq!(text, "a b c");
    assert_eq!(completes, 1);

    session.terminate().await.unwrap();
}

// ── system prompt: prepended to the first turn only ────────────────────────────

#[tokio::test]
async fn system_prompt_is_prepended_to_first_turn_only() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let behavior = MockBehavior {
        session_id: "sess-sys".into(),
        chunks: vec!["ok".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 0,
        ..MockBehavior::default()
    };
    let (reader, writer, _mock) = spawn_mock_agent_recording(behavior, Arc::clone(&log));
    let client = AcpClient::with_transport(reader, writer, "/tmp/repo")
        .await
        .unwrap();
    let mut session: Box<dyn AgentSession> =
        Box::new(AcpSession::from_client(client, "You are a developer."));

    // Two turns; the system prompt must appear only in the first.
    let (_t1, _c1) = drain_ok(session.prompt(Prompt::new("do task A")).await.unwrap()).await;
    let (_t2, _c2) = drain_ok(session.prompt(Prompt::new("do task B")).await.unwrap()).await;
    session.terminate().await.unwrap();

    let sent = log.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "two prompts were sent");
    assert_eq!(
        sent[0], "You are a developer.\n\ndo task A",
        "first turn must carry the system-prompt prelude"
    );
    assert_eq!(
        sent[1], "do task B",
        "later turns must be sent verbatim (no repeated system prompt)"
    );
}

#[tokio::test]
async fn empty_system_prompt_adds_no_prelude() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let behavior = MockBehavior {
        session_id: "sess-nosys".into(),
        chunks: vec!["ok".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 0,
        ..MockBehavior::default()
    };
    let (reader, writer, _mock) = spawn_mock_agent_recording(behavior, Arc::clone(&log));
    let client = AcpClient::with_transport(reader, writer, "/tmp/repo")
        .await
        .unwrap();
    let mut session: Box<dyn AgentSession> = Box::new(AcpSession::from_client(client, ""));

    let _ = drain_ok(session.prompt(Prompt::new("just the task")).await.unwrap()).await;
    session.terminate().await.unwrap();

    let sent = log.lock().unwrap().clone();
    assert_eq!(sent, vec!["just the task"], "no stray leading blank line");
}
