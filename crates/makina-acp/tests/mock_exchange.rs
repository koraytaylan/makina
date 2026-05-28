//! Deterministic full-exchange test against an in-memory mock ACP agent.
//!
//! This is the task's **required** test: it drives [`makina_acp::AcpClient`]
//! through the complete protocol — `initialize` → `session/new` →
//! `session/prompt` → streamed `session/update` chunks → turn complete — over a
//! [`tokio::io::duplex`] pipe, with no subprocess. It asserts the handshake ran
//! and the streamed response assembles correctly.

mod common;

use common::{MockBehavior, spawn_mock_agent};
use futures::StreamExt;
use makina_acp::{AcpClient, AcpResponseChunk, StopReason};

/// Drain a prompt stream into (assembled text, stop reason), asserting the
/// stream ends with exactly one `TurnComplete`.
async fn drain(stream: makina_acp::PromptStream<'_>) -> (String, StopReason) {
    let mut text = String::new();
    let mut stop = None;
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item.expect("no error item expected in a clean turn") {
            AcpResponseChunk::Text(t) => text.push_str(&t),
            AcpResponseChunk::TurnComplete(reason) => {
                assert!(stop.is_none(), "TurnComplete must appear exactly once");
                stop = Some(reason);
            }
        }
    }
    (text, stop.expect("stream must yield a TurnComplete"))
}

#[tokio::test]
async fn full_handshake_and_streamed_prompt_response() {
    let behavior = MockBehavior {
        session_id: "sess-xyz".into(),
        chunks: vec!["The ".into(), "answer ".into(), "is ".into(), "42.".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 0,
    };
    let (reader, writer, mock) = spawn_mock_agent(behavior);

    // connect() performs initialize + session/new against the mock.
    let mut client = AcpClient::with_transport(reader, writer, "/tmp/repo")
        .await
        .expect("handshake should succeed");

    // Handshake results are visible.
    assert_eq!(client.session_id(), "sess-xyz");
    assert_eq!(client.protocol_version(), 1);
    assert_eq!(
        client.agent_info().map(|i| i.name.as_str()),
        Some("mock-acp-agent")
    );

    // Run the turn and collect the streamed response.
    let stream = client
        .prompt("What is the answer?")
        .expect("prompt accepted");
    let (text, stop) = drain(stream).await;

    assert_eq!(
        text, "The answer is 42.",
        "chunks assemble in arrival order"
    );
    assert_eq!(stop, StopReason::EndTurn);

    client.shutdown().await.expect("shutdown is clean");
    // Drop the client so its write half closes, letting the mock's read loop
    // hit EOF and the task finish (otherwise `mock.await` would block).
    drop(client);
    mock.await.expect("mock agent task completes");
}

#[tokio::test]
async fn non_text_updates_are_ignored_during_turn() {
    // The mock injects tool-call updates before the text; the client must skip
    // them and still assemble only the assistant text.
    let behavior = MockBehavior {
        session_id: "sess-noise".into(),
        chunks: vec!["clean ".into(), "text".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 3,
    };
    let (reader, writer, mock) = spawn_mock_agent(behavior);

    let mut client = AcpClient::with_transport(reader, writer, "/tmp/repo")
        .await
        .unwrap();

    let stream = client.prompt("go").unwrap();
    let (text, stop) = drain(stream).await;

    assert_eq!(text, "clean text");
    assert_eq!(stop, StopReason::EndTurn);

    client.shutdown().await.unwrap();
    drop(client);
    mock.await.unwrap();
}

#[tokio::test]
async fn two_sequential_turns_on_one_session() {
    // A mock that answers any number of prompts (chunks reused each turn).
    let behavior = MockBehavior {
        session_id: "sess-multi".into(),
        chunks: vec!["pong".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 0,
    };
    let (reader, writer, mock) = spawn_mock_agent(behavior);

    let mut client = AcpClient::with_transport(reader, writer, "/tmp/repo")
        .await
        .unwrap();

    // First turn.
    {
        let stream = client.prompt("ping 1").unwrap();
        let (text, _) = drain(stream).await;
        assert_eq!(text, "pong");
    }
    // Second turn on the same session/transport.
    {
        let stream = client.prompt("ping 2").unwrap();
        let (text, _) = drain(stream).await;
        assert_eq!(text, "pong");
    }

    client.shutdown().await.unwrap();
    drop(client);
    mock.await.unwrap();
}
