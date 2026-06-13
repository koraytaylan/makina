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
///
/// The rich side-channel variants (thoughts / tool calls) are intentionally
/// ignored here so the many text-only tests keep their simple contract; tests
/// that care about the rich chunks use [`drain_all`].
async fn drain(stream: makina_acp::PromptStream<'_>) -> (String, StopReason) {
    let mut text = String::new();
    let mut stop = None;
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item.expect("no error item expected in a clean turn") {
            AcpResponseChunk::Text(t) => text.push_str(&t),
            AcpResponseChunk::Thought(_)
            | AcpResponseChunk::ToolCall { .. }
            | AcpResponseChunk::ToolCallUpdate { .. }
            | AcpResponseChunk::CurrentModeUpdate { .. } => {}
            AcpResponseChunk::TurnComplete(reason) => {
                assert!(stop.is_none(), "TurnComplete must appear exactly once");
                stop = Some(reason);
            }
        }
    }
    (text, stop.expect("stream must yield a TurnComplete"))
}

/// Drain a prompt stream into (every chunk in arrival order, stop reason),
/// asserting the stream ends with exactly one `TurnComplete`. The returned Vec
/// excludes the terminal `TurnComplete`; the stop reason is returned separately.
async fn drain_all(stream: makina_acp::PromptStream<'_>) -> (Vec<AcpResponseChunk>, StopReason) {
    let mut chunks = Vec::new();
    let mut stop = None;
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item.expect("no error item expected in a clean turn") {
            AcpResponseChunk::TurnComplete(reason) => {
                assert!(stop.is_none(), "TurnComplete must appear exactly once");
                stop = Some(reason);
            }
            other => chunks.push(other),
        }
    }
    (chunks, stop.expect("stream must yield a TurnComplete"))
}

#[tokio::test]
async fn full_handshake_and_streamed_prompt_response() {
    let behavior = MockBehavior {
        session_id: "sess-xyz".into(),
        chunks: vec!["The ".into(), "answer ".into(), "is ".into(), "42.".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 0,
        ..MockBehavior::default()
    };
    let (reader, writer, mock) = spawn_mock_agent(behavior);

    // connect() performs initialize + session/new against the mock.
    let mut client = AcpClient::with_transport(reader, writer, "/tmp/repo", None, None)
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
async fn non_message_updates_are_emitted_as_rich_chunks() {
    // The mock injects a thought, a tool_call, and a tool_call_update (plus some
    // `leading_noise_updates` tool_calls) before the text. Those non-message
    // updates are now DELIVERED as rich side-channel chunks — not silently
    // dropped — while the assembled assistant text stays clean.
    let behavior = MockBehavior {
        session_id: "sess-noise".into(),
        chunks: vec!["clean ".into(), "text".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 3,
        inject_thoughts_and_tools: true,
        ..MockBehavior::default()
    };
    let (reader, writer, mock) = spawn_mock_agent(behavior);

    let mut client = AcpClient::with_transport(reader, writer, "/tmp/repo", None, None)
        .await
        .unwrap();

    let stream = client.prompt("go").unwrap();
    let (chunks, stop) = drain_all(stream).await;

    // The rich variants must be present.
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, AcpResponseChunk::Thought(_))),
        "expected at least one Thought chunk, got {chunks:?}"
    );
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, AcpResponseChunk::ToolCall { .. })),
        "expected at least one ToolCall chunk, got {chunks:?}"
    );
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, AcpResponseChunk::ToolCallUpdate { .. })),
        "expected at least one ToolCallUpdate chunk, got {chunks:?}"
    );

    // The mock injects every non-message update (thought + tool calls) BEFORE the
    // text, so every rich side-channel chunk must arrive at a lower index than the
    // first Text chunk in arrival order.
    let first_text_idx = chunks
        .iter()
        .position(|c| matches!(c, AcpResponseChunk::Text(_)))
        .expect("expected at least one Text chunk");
    for (idx, chunk) in chunks.iter().enumerate() {
        if matches!(
            chunk,
            AcpResponseChunk::Thought(_)
                | AcpResponseChunk::ToolCall { .. }
                | AcpResponseChunk::ToolCallUpdate { .. }
        ) {
            assert!(
                idx < first_text_idx,
                "rich chunk at index {idx} ({chunk:?}) must arrive before the \
                 first Text chunk at index {first_text_idx}, got {chunks:?}"
            );
        }
    }

    // Despite the side-channel noise, the assembled assistant text is intact.
    let text: String = chunks
        .iter()
        .filter_map(|c| match c {
            AcpResponseChunk::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "clean text");
    assert_eq!(stop, StopReason::EndTurn);

    client.shutdown().await.unwrap();
    drop(client);
    mock.await.unwrap();
}

#[tokio::test]
async fn thought_and_tool_events_are_delivered() {
    // The mock injects, in this exact order before the text:
    //   Thought("thinking…"), ToolCall(rich-tc-1, pending), ToolCallUpdate(completed)
    // then the assistant text chunks. Assert the rich variants arrive
    // interleaved with text in that precise arrival order.
    let behavior = MockBehavior {
        session_id: "sess-rich".into(),
        chunks: vec!["done".into()],
        stop_reason: "end_turn".into(),
        leading_noise_updates: 0,
        inject_thoughts_and_tools: true,
        ..MockBehavior::default()
    };
    let (reader, writer, mock) = spawn_mock_agent(behavior);

    let mut client = AcpClient::with_transport(reader, writer, "/tmp/repo", None, None)
        .await
        .unwrap();

    let stream = client.prompt("go").unwrap();
    let (chunks, stop) = drain_all(stream).await;

    assert_eq!(
        chunks,
        vec![
            AcpResponseChunk::Thought("thinking…".into()),
            AcpResponseChunk::ToolCall {
                id: "rich-tc-1".into(),
                title: "running tests".into(),
                kind: Some("execute".into()),
                status: "pending".into(),
                detail: None,
            },
            AcpResponseChunk::ToolCallUpdate {
                id: "rich-tc-1".into(),
                status: Some("completed".into()),
                title: None,
                detail: None,
            },
            AcpResponseChunk::Text("done".into()),
        ],
        "rich chunks must arrive interleaved with text in injection order"
    );
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
        ..MockBehavior::default()
    };
    let (reader, writer, mock) = spawn_mock_agent(behavior);

    let mut client = AcpClient::with_transport(reader, writer, "/tmp/repo", None, None)
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
