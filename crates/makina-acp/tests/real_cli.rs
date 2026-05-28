//! `#[ignore]`d integration test against a **real, authenticated ACP CLI**.
//!
//! This is the task's literal acceptance criterion — "exchanges a prompt/response
//! with a real ACP CLI" — but it is `#[ignore]`d so CI without a CLI (or without
//! a signed-in agent) stays green. The deterministic proof of the protocol lives
//! in `mock_exchange.rs` / `lifecycle_errors.rs`; this test is for manual
//! verification on a machine that has an agent installed and authenticated.
//!
//! # Running it
//!
//! ```bash
//! # Google Gemini CLI (supports `--acp`; must be signed in):
//! MAKINA_ACP_CMD=gemini MAKINA_ACP_ARGS=--acp \
//!     cargo test -p makina-acp --test real_cli -- --ignored --nocapture
//!
//! # Zed's Claude Code ACP adapter via npx:
//! MAKINA_ACP_CMD=npx \
//! MAKINA_ACP_ARGS='-y,@zed-industries/claude-code-acp@latest' \
//!     cargo test -p makina-acp --test real_cli -- --ignored --nocapture
//! ```
//!
//! `MAKINA_ACP_CMD` is the program; `MAKINA_ACP_ARGS` is a comma-separated arg
//! list. The agent must already be authenticated (Zed model: Makina inherits the
//! environment and never handles credentials).

use std::time::Duration;

use futures::StreamExt;
use makina_acp::{AcpBackend, AcpClient, AcpCommand, AcpResponseChunk};
use makina_core::backend::{AgentBackend, Prompt, ResponseEvent, SessionConfig};

/// Read the agent program + args from the environment (shared by both tests).
fn cli_program_and_args() -> (String, Vec<String>) {
    let program = std::env::var("MAKINA_ACP_CMD").unwrap_or_else(|_| {
        panic!("set MAKINA_ACP_CMD to the agent program (e.g. `gemini`)");
    });
    let args: Vec<String> = std::env::var("MAKINA_ACP_ARGS")
        .map(|s| s.split(',').map(|a| a.trim().to_string()).collect())
        .unwrap_or_default();
    (program, args)
}

#[tokio::test]
#[ignore = "requires a real, authenticated ACP CLI; run manually with MAKINA_ACP_CMD set"]
async fn real_cli_prompt_response() {
    let (program, args) = cli_program_and_args();

    let cwd = std::env::current_dir().unwrap();
    let command = AcpCommand::new(&program, cwd).args(args);

    eprintln!("connecting to real ACP agent: {program}");
    let mut client = AcpClient::connect(command)
        .await
        .expect("connect (initialize + session/new) against real CLI");

    eprintln!(
        "connected: protocol v{}, agent = {:?}, session = {}",
        client.protocol_version(),
        client.agent_info(),
        client.session_id()
    );

    // A trivial, deterministic-ish prompt. We assert only that *some* text comes
    // back and the turn completes — real model output is not byte-stable.
    let prompt = "Reply with exactly the word: pong";
    let mut stream = client.prompt(prompt).expect("prompt accepted");

    let mut answer = String::new();
    let mut completed = false;
    // Bound the whole turn so a misbehaving agent can't hang the test forever.
    let turn = async {
        while let Some(item) = stream.next().await {
            match item.expect("no transport error during real turn") {
                AcpResponseChunk::Text(t) => {
                    eprint!("{t}");
                    answer.push_str(&t);
                }
                AcpResponseChunk::TurnComplete(reason) => {
                    eprintln!("\n[turn complete: {reason:?}]");
                    completed = true;
                    break;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(120), turn)
        .await
        .expect("real turn timed out");

    drop(stream);
    client.shutdown().await.expect("shutdown");

    assert!(completed, "the turn should reach TurnComplete");
    assert!(
        !answer.trim().is_empty(),
        "the agent should have produced some text"
    );
}

/// The task-15 equivalent: exercise `AcpBackend::spawn` + the `AgentBackend` /
/// `AgentSession` **trait** against a real CLI. Asserts the same liveness as the
/// raw-client test but through the mapped `ResponseEvent` stream, proving the
/// adapter end-to-end with a real subprocess. `#[ignore]`d for the same reason.
#[tokio::test]
#[ignore = "requires a real, authenticated ACP CLI; run manually with MAKINA_ACP_CMD set"]
async fn real_cli_prompt_response_through_the_backend_trait() {
    let (program, args) = cli_program_and_args();
    let cwd = std::env::current_dir().unwrap();

    let backend = AcpBackend::new(program, args);
    let config = SessionConfig {
        working_dir: cwd,
        system_prompt: "You are a terse assistant.".to_string(),
        extra: None,
    };

    let mut session = backend
        .spawn(config)
        .await
        .expect("AcpBackend::spawn against a real CLI");

    let mut stream = session
        .prompt(Prompt::new("Reply with exactly the word: pong"))
        .await
        .expect("prompt accepted");

    let mut answer = String::new();
    let mut completed = false;
    let turn = async {
        while let Some(item) = stream.next().await {
            match item.expect("no transport error during real turn") {
                ResponseEvent::TextChunk { text } => {
                    eprint!("{text}");
                    answer.push_str(&text);
                }
                ResponseEvent::TurnComplete => {
                    eprintln!("\n[turn complete]");
                    completed = true;
                    break;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(120), turn)
        .await
        .expect("real turn timed out");

    drop(stream);
    session.terminate().await.expect("terminate");

    assert!(completed, "the turn should reach TurnComplete");
    assert!(
        !answer.trim().is_empty(),
        "the agent should have produced some text"
    );
}
