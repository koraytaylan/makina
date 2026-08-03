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
                AcpResponseChunk::CurrentModeUpdate { .. } => {}
                // Rich side-channel chunks (thoughts/tool calls): this test only
                // cares about the assistant text, so ignore them.
                AcpResponseChunk::Thought(_)
                | AcpResponseChunk::ToolCall { .. }
                | AcpResponseChunk::ToolCallUpdate { .. } => {}
                AcpResponseChunk::TurnComplete { stop_reason, .. } => {
                    eprintln!("\n[turn complete: {stop_reason:?}]");
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
        mode: None,
        model: None,
        effort: None,
        extra: None,
        task_id: None,
        run_id: String::new(),
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
                ResponseEvent::CurrentModeUpdate { .. } => {}
                // Side-channel events do not contribute to the collected answer.
                ResponseEvent::ThoughtChunk { .. }
                | ResponseEvent::ToolCall { .. }
                | ResponseEvent::ToolCallUpdate { .. } => {}
                ResponseEvent::TurnComplete { .. } => {
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

/// Empirical probe for the `verify-permission-trigger` task.
///
/// This `#[ignore]` test exists per the task's "Done when" criterion: it records
/// (in its source) the observed trigger condition and the exact concrete
/// `session/request_permission` request params + response shape captured against
/// a real `gemini --acp` (v0.44.1, default approval mode).
///
/// Re-run manually if the agent behavior must be re-sampled:
///   MAKINA_ACP_CMD=gemini MAKINA_ACP_ARGS=--acp \
///     cargo test -p makina-acp --test real_cli probe_permission_trigger -- --ignored --nocapture
///
/// The data below decides only the `ClientCapabilities` literal (the rest of the
/// gateway design is unaffected).
#[test]
#[ignore = "records empirical capture from real gemini --acp; see source for data and conclusion"]
fn probe_permission_trigger_captured_data() {
    // === Written note for verify-permission-trigger ===
    //
    // Empirical determination (2026-04-22, macOS, gemini 0.44.1):
    //
    // * With the current empty `clientCapabilities: {}` the agent **does emit**
    //   `session/request_permission` (before any file write under default
    //   "Prompts for approval" mode). This is the normal path, not an edge case.
    //
    // * Advertising an `fs` capability:
    //     "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true } }
    //   does **not** suppress the permission request. The identical
    //   `session/request_permission` still arrives; no `fs/read_text_file` or
    //   `fs/write_text_file` server→client requests were observed for the write
    //   attempt. The agent still performed the write via its internal tool after
    //   the permission reply was supplied.
    //
    // Conclusion: empty `ClientCapabilities` (the unit struct that serializes to
    // `{}`) is sufficient and the correct choice. Advertising `fs` would not
    // eliminate permission prompts and would require implementing the fs/* RPCs
    // (out of scope). Keep the literal unchanged.
    //
    // --- Exact captured `session/request_permission` params (one live sample) ---
    //
    // {
    //   "sessionId": "45efbc32-fa78-40ca-9637-5ea06d4c48e3",
    //   "options": [
    //     { "optionId": "proceed_always", "name": "Allow for this session", "kind": "allow_always" },
    //     { "optionId": "proceed_once",  "name": "Allow",                   "kind": "allow_once" },
    //     { "optionId": "cancel",        "name": "Reject",                  "kind": "reject_once" }
    //   ],
    //   "toolCall": {
    //     "toolCallId": "write_file__write_file_1780041520414_0",
    //     "status": "pending",
    //     "title": "Writing to fs-probe.txt",
    //     "content": [
    //       {
    //         "type": "diff",
    //         "path": "/.../fs-probe.txt",
    //         "oldText": "",
    //         "newText": "PROBE-FS-TEST",
    //         "_meta": { "kind": "add" }
    //       }
    //     ],
    //     "locations": [ { "path": "/.../fs-probe.txt" } ],
    //     "kind": "edit"
    //   }
    // }
    //
    // Unknown/extra fields under `toolCall` (content, locations, _meta, and any
    // future tool-specific keys) must be preserved by the deserializer (use a
    // flattened map or #[serde(flatten)] + unknown container) in the follow-up
    // `acp-permission-types` task.
    //
    // --- Expected response shape (sent as the JSON-RPC result for the request id) ---
    //
    // Allow (select one of the offered options):
    //   { "outcome": { "outcome": "selected", "optionId": "proceed_once" } }
    //
    // Cancel / reject:
    //   { "outcome": { "outcome": "cancelled" } }
    //
    // (The full reply is the normal JSON-RPC response envelope:
    //   {"jsonrpc":"2.0","id":<request-id>,"result":<shape-above>}
    //  This shape was confirmed live in the probe: after the reply the agent
    //  emitted `tool_call_update: completed` for the write and finished the turn
    //  with `stopReason: "end_turn"`.)
    //
    // The option kinds observed are "allow_always" | "allow_once" | "reject_once".
    // The selected optionId must be one of the `optionId` strings from the
    // request (e.g. "proceed_once").
    //
    // This record, plus the two real runs (empty vs. fs) that produced it, fully
    // satisfies the task. No production code change was required; the
    // `ClientCapabilities` literal stays the empty unit struct.

    eprintln!(
        "verify-permission-trigger probe data recorded in source (see `cargo test -- --nocapture` output or the test body comment)."
    );
    eprintln!(
        "Conclusion: keep empty ClientCapabilities; permission requests arrive regardless of fs advertisement."
    );
}

/// **Model selection reaches the agent** — `AcpBackend::spawn` applies
/// `SessionConfig::model` via `session/set_config_option`.  A wrong param name
/// there makes a conforming agent reject the request with JSON-RPC `-32602`,
/// which fails the whole spawn and therefore the whole run — the failure this
/// test guards against was exactly that (`optionId` instead of `configId`).
///
/// This test spawns and terminates a session; it never sends a prompt, so it
/// costs no model tokens.
///
/// ```bash
/// MAKINA_ACP_CMD=opencode MAKINA_ACP_ARGS=acp MAKINA_ACP_MODEL='<provider/model>' \
///     cargo test -p makina-acp --test real_cli -- --ignored --nocapture \
///     real_cli_applies_the_configured_model
/// ```
#[tokio::test]
#[ignore = "requires a real, authenticated ACP CLI; run manually with MAKINA_ACP_CMD set"]
async fn real_cli_applies_the_configured_model() {
    let (program, args) = cli_program_and_args();
    let model = std::env::var("MAKINA_ACP_MODEL")
        .expect("set MAKINA_ACP_MODEL to a model value the agent advertises");

    let backend = AcpBackend::new(&program, args);
    let config = SessionConfig {
        working_dir: std::env::current_dir().unwrap(),
        system_prompt: "You are a test harness. Do nothing.".to_string(),
        mode: None,
        model: Some(model.clone()),
        effort: None,
        extra: None,
        task_id: None,
        run_id: String::new(),
    };

    eprintln!("spawning session on {program} with model {model}");
    // The assertion IS the spawn: applying the model option is part of `spawn`,
    // so a rejected `session/set_config_option` surfaces here as an `Err`.
    let mut session = backend
        .spawn(config)
        .await
        .expect("spawn must apply the configured model without the agent rejecting it");

    if let Some(capabilities) = session.capabilities() {
        eprintln!("agent capabilities: {capabilities:?}");
    }
    session.terminate().await.expect("terminate");
}
