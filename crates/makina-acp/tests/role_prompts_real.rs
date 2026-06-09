//! `#[ignore]`d integration test: Developer and Reviewer role prompts against a
//! real ACP CLI.
//!
//! This is the task-19 acceptance criterion — **"the backend produces dev output
//! and review verdicts from the respective prompts"** — exercised against a real
//! authenticated agent (e.g. `gemini --acp`).
//!
//! The test is `#[ignore]`d so CI without a CLI or without a signed-in agent
//! stays green.  Deterministic proof lives in `makina-core`'s unit tests (using
//! `NoopBackend`).
//!
//! # Running this test
//!
//! On a machine where the ACP CLI is installed and authenticated:
//!
//! ```bash
//! # Google Gemini CLI (must be signed in via `gemini auth login`):
//! MAKINA_ACP_CMD=gemini MAKINA_ACP_ARGS=--acp \
//!     cargo test -p makina-acp --test role_prompts_real -- --ignored --nocapture
//!
//! # Zed's Claude Code ACP adapter via npx:
//! MAKINA_ACP_CMD=npx \
//! MAKINA_ACP_ARGS='-y,@zed-industries/claude-code-acp@latest' \
//!     cargo test -p makina-acp --test role_prompts_real -- --ignored --nocapture
//! ```
//!
//! `MAKINA_ACP_CMD` is the program; `MAKINA_ACP_ARGS` is a comma-separated arg
//! list.  Auth path: the CLI must be pre-authenticated (Zed model — Makina
//! inherits the parent environment and holds no credentials).
//!
//! # What is asserted
//!
//! **Developer role**:
//! - A session spawned with `session_config_for(Role::Developer, cwd)` and
//!   prompted with a trivial implementation task returns non-empty output.
//!
//! **Reviewer role**:
//! - A session spawned with `session_config_for(Role::Reviewer, cwd)` and
//!   prompted with a trivial review scenario produces output that
//!   `parse_review_verdict` successfully parses as `Approve` or `Reject`.
//!
//! No byte-exact response content is asserted; real model output is
//! non-deterministic.

use std::time::Duration;

use futures::StreamExt;
use makina_acp::AcpBackend;
use makina_core::backend::{AgentBackend, Prompt, ResponseEvent};
use makina_core::roles::{ReviewVerdict, Role, parse_review_verdict, session_config_for};

/// Read the agent program + args from the environment (shared by both tests).
fn cli_program_and_args() -> (String, Vec<String>) {
    let program = std::env::var("MAKINA_ACP_CMD").unwrap_or_else(|_| {
        panic!(
            "set MAKINA_ACP_CMD to the agent program (e.g. `gemini`); \
             see module-level documentation for the full command."
        )
    });
    let args: Vec<String> = std::env::var("MAKINA_ACP_ARGS")
        .map(|s| s.split(',').map(|a| a.trim().to_string()).collect())
        .unwrap_or_default();
    (program, args)
}

/// Drain a session stream into a `String`, collecting `TextChunk` text until
/// `TurnComplete`.  Times out after 120 seconds to prevent hanging CI.
async fn collect_response(
    session: &mut Box<dyn makina_core::backend::AgentSession>,
    prompt_text: &str,
) -> String {
    let stream = session
        .prompt(Prompt::new(prompt_text))
        .await
        .expect("prompt must succeed");

    let mut collected = String::new();
    let turn = async {
        let mut events = stream;
        while let Some(item) = events.next().await {
            match item.expect("no transport error during real turn") {
                ResponseEvent::TextChunk { text } => {
                    eprint!("{text}");
                    collected.push_str(&text);
                }
                // Side-channel events do not contribute to the collected answer.
                ResponseEvent::ThoughtChunk { .. }
                | ResponseEvent::ToolCall { .. }
                | ResponseEvent::ToolCallUpdate { .. } => {}
                ResponseEvent::TurnComplete => {
                    eprintln!("\n[turn complete]");
                    break;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(120), turn)
        .await
        .expect("real turn timed out after 120 s");

    collected
}

/// **Real-backend acceptance test (a): Developer role produces dev output.**
///
/// Spawns a session with the `Developer` system prompt, gives it a trivial
/// implementation task, drains the response stream, and asserts non-empty
/// developer output is returned.
#[tokio::test]
#[ignore = "requires a real, authenticated ACP CLI; run manually with MAKINA_ACP_CMD set"]
async fn developer_role_produces_dev_output() {
    let (program, args) = cli_program_and_args();
    eprintln!("developer_role_produces_dev_output: using ACP CLI `{program}` args={args:?}");

    let cwd = std::env::current_dir().unwrap();
    let backend = AcpBackend::new(program, args);
    let config = session_config_for(Role::Developer, cwd, None);

    eprintln!(
        "Spawning Developer session with system_prompt prefix: {:?}…",
        &config.system_prompt[..config.system_prompt.len().min(60)]
    );

    let mut session = backend
        .spawn(config)
        .await
        .expect("AcpBackend::spawn (Developer) must succeed");

    let prompt = "Describe in one sentence what you would do to implement a Rust function \
                  that adds two integers. Do not write any code.";

    eprintln!("Sending Developer prompt: {prompt:?}");
    let dev_output = collect_response(&mut session, prompt).await;
    session.terminate().await.expect("terminate must succeed");

    eprintln!(
        "Developer output ({} chars): {:?}",
        dev_output.len(),
        &dev_output[..dev_output.len().min(200)]
    );

    assert!(
        !dev_output.trim().is_empty(),
        "Developer role must produce non-empty output"
    );
}

/// **Real-backend acceptance test (b): Reviewer role produces a parseable verdict.**
///
/// Spawns a session with the `Reviewer` system prompt, gives it a trivial
/// review scenario (a minimal passing implementation), drains the response
/// stream, and asserts that `parse_review_verdict` produces either
/// `ReviewVerdict::Approve` or `ReviewVerdict::Reject` without error.
#[tokio::test]
#[ignore = "requires a real, authenticated ACP CLI; run manually with MAKINA_ACP_CMD set"]
async fn reviewer_role_produces_parseable_verdict() {
    let (program, args) = cli_program_and_args();
    eprintln!("reviewer_role_produces_parseable_verdict: using ACP CLI `{program}` args={args:?}");

    let cwd = std::env::current_dir().unwrap();
    let backend = AcpBackend::new(program, args);
    let config = session_config_for(Role::Reviewer, cwd, None);

    eprintln!(
        "Spawning Reviewer session with system_prompt prefix: {:?}…",
        &config.system_prompt[..config.system_prompt.len().min(60)]
    );

    let mut session = backend
        .spawn(config)
        .await
        .expect("AcpBackend::spawn (Reviewer) must succeed");

    // Give the Reviewer a trivially correct implementation to review.
    let prompt = "\
Task: Implement a Rust function `add(a: i32, b: i32) -> i32` that returns the sum of two integers.
Done when: the function compiles and returns the correct sum.

Implementation found in working directory:
```rust
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
```

Please review this implementation against the task criterion and emit your verdict JSON.";

    eprintln!("Sending Reviewer prompt (scenario: trivially correct add function)");
    let raw_output = collect_response(&mut session, prompt).await;
    session.terminate().await.expect("terminate must succeed");

    eprintln!(
        "Reviewer raw output ({} chars): {:?}",
        raw_output.len(),
        &raw_output[..raw_output.len().min(400)]
    );

    assert!(
        !raw_output.trim().is_empty(),
        "Reviewer role must produce non-empty output"
    );

    let verdict = parse_review_verdict(&raw_output).unwrap_or_else(|e| {
        panic!(
            "parse_review_verdict must succeed on real Reviewer output.\n\
             Error: {e}\n\
             Raw output: {raw_output:?}"
        )
    });

    eprintln!("Parsed verdict: {verdict:?}");

    // Any well-formed verdict is acceptable — the point is that the JSON
    // contract was honoured.  For a trivially correct implementation we expect
    // Approve, but the model is non-deterministic.
    match &verdict {
        ReviewVerdict::Approve => {
            eprintln!("Reviewer approved (expected for a correct add function).");
        }
        ReviewVerdict::Reject { feedback } => {
            eprintln!("Reviewer rejected with feedback: {feedback:?}");
            // A rejection is allowed — the model may be strict — but feedback
            // must be non-empty (the parser already enforces this via the
            // default message, so this is belt-and-suspenders).
            assert!(!feedback.is_empty(), "Reject feedback must not be empty");
        }
    }
}
