//! **Plan 0006 acceptance — end-to-end exchange observability.**
//!
//! This is the final acceptance test for the "exchange thoughts and tools"
//! plan.  It proves that the *rich* side-channel response events introduced by
//! the plan — `ThoughtChunk`, `ToolCall`, `ToolCallUpdate` — survive the WHOLE
//! stack and land, correctly shaped, in the TUI's per-task exchange log:
//!
//! ```text
//!   NoopBackend::scripted([Text, Thought, Thought, ToolCall, Update, Update, Text])
//!     → real Developer turn (forwards each ResponseEvent as an ExchangeEvent)
//!       → real CoreApi event stream (Event::AgentExchange)
//!         → real App::update(AppEvent::ApiEvent(..))  (the binary's path for AgentExchange)
//!           → App::exchange_logs[(RunId, task)]  (ExchangeLog)
//! ```
//!
//! It deliberately drives the **production** `CoreApi` orchestrator (the same
//! type `makina`'s `main.rs` constructs) wired to the production `NoopBackend`,
//! and feeds the resulting live `Event`s into a real [`makina::app::App`] via the
//! same `AppEvent::ApiEvent` wrapper the IO loop in `makina::event` uses for
//! `AgentExchange` events (the binary additionally remaps `RunOpened`→`RunLoaded`,
//! which is irrelevant to the exchange events under test).  There
//! is no shortcut: every event observed here was produced by the real
//! role-turn forwarding code over the real broadcast event stream.
//!
//! # Why this would have FAILED before the plan
//!
//! Before plan 0006 none of the load-bearing pieces existed:
//! `ResponseEvent::{ThoughtChunk,ToolCall,ToolCallUpdate}`,
//! `ExchangeEvent::{ThoughtChunk,ToolCall,ToolCallUpdate}`,
//! `NoopBackend::scripted`, and the `ExchangeLog::{append_thought,start_tool,
//! update_tool}` handling in `App`.  A test scripting those events and asserting
//! the thought text + the upserted-to-`completed` tool entry could not even
//! compile, let alone pass.
//!
//! # Where the run stops
//!
//! With a scripted backend EVERY turn (developer *and* reviewer) emits the same
//! rich sequence whose answer text is `"Working on it. Done."` — which is not a
//! parseable review verdict, so the run never reaches `Done`.  That is fine: the
//! observability target is the **Developer** turn, which happens first.  We
//! consume the live stream until we have seen the Developer's `TurnComplete`
//! `AgentExchange` for the task (bounded by a wall-clock deadline so the test can
//! never hang), then assert on the accumulated log.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;

use makina::app::{App, AppEvent, ExchangeContent};
use makina_core::api::{
    AgentRole, Api, Command, CommandOutcome, Event, ExchangeEvent, RunView, TaskId,
};
use makina_core::backend::noop::NoopBackend;
use makina_core::backend::{AgentBackend, ResponseEvent};
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::StructuredTextInterpreter;
use makina_core::orchestrator::CoreApi;
use makina_core::worktree::WorktreeManager;

/// A single-task structured-text task list: the smallest graph that drives one
/// Developer turn.  Mirrors the `ONE_TASK_LIST` shape used by the orchestrator's
/// own execution tests.
const ONE_TASK_LIST: &str = r#"# Solo — Task List

A one-task list used to exercise the full exchange-observability path.

---

## 0001 — Foundation

### solo-task — Implement the solo task
Do the thing in `lib.rs`.
- **Depends on:** —
- **Done when:** The solo task is implemented, the code works, and tests pass.
"#;

/// The scripted rich turn every prompt emits (the plan-0006 acceptance fixture):
/// interleaved text + two thought chunks + a tool call mutated by two updates to
/// a final `completed` status + trailing text.  `NoopBackend::scripted` appends
/// the terminating `TurnComplete` automatically.
fn rich_turn() -> Vec<ResponseEvent> {
    vec![
        ResponseEvent::TextChunk {
            text: "Working on it. ".into(),
        },
        ResponseEvent::ThoughtChunk {
            text: "I need to read the trait first.".into(),
        },
        ResponseEvent::ThoughtChunk {
            text: "Now I'll edit it.".into(),
        },
        ResponseEvent::ToolCall {
            id: "tc-1".into(),
            title: "Edit src/lib.rs".into(),
            kind: Some("edit".into()),
            status: "pending".into(),
            detail: None,
        },
        ResponseEvent::ToolCallUpdate {
            id: "tc-1".into(),
            status: Some("in_progress".into()),
            title: None,
            detail: None,
        },
        ResponseEvent::ToolCallUpdate {
            id: "tc-1".into(),
            status: Some("completed".into()),
            title: None,
            detail: None,
        },
        ResponseEvent::TextChunk {
            text: "Done.".into(),
        },
    ]
}

/// Build a `Config` with NO gates so the develop→review loop advances straight
/// through (the same no-gate config the orchestrator integration tests use).
fn no_gate_config() -> Config {
    Config::resolve(GlobalConfig::default(), ProjectConfig::default())
}

/// Create a minimal git repo on a `develop` branch in a fresh tempdir so the
/// `WorktreeManager` can create per-task worktrees off it.
fn setup_temp_repo() -> tempfile::TempDir {
    use std::process::Command as StdCommand;

    fn run_git(path: &std::path::Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
        assert!(status.success(), "git {args:?} failed");
    }

    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path();
    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["config", "commit.gpgsign", "false"]);
    run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);
    let branch = String::from_utf8(
        StdCommand::new("git")
            .args(["-C", &path.to_string_lossy()])
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("git rev-parse HEAD")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();
    if branch != "develop" {
        run_git(path, &["branch", "-m", &branch, "develop"]);
    }
    dir
}

/// Write `contents` to a markdown file in a fresh tempdir; return both (keep the
/// dir alive for the test's lifetime).
fn write_task_list(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let path = dir.path().join("solo-feature.md");
    std::fs::write(&path, contents).expect("write task list");
    (dir, path)
}

/// **Plan-0006 acceptance: a full Developer turn's thoughts AND tool call surface
/// in the App exchange log, with the tool upserted to its final status and no
/// side-channel text leaking into the answer.**
///
/// Drives the production `CoreApi` (deterministic interpreter + scripted
/// `NoopBackend` + temp-repo `WorktreeManager` + no-gate `Config`) with a
/// single-task graph, consumes the live `Event` stream into a real `App` (via the
/// `AppEvent::ApiEvent` path the binary uses for `AgentExchange` events), and asserts the per-task
/// `ExchangeLog`.
///
/// Multi-thread runtime: the background scheduler + per-task driver futures all
/// make progress concurrently while this test polls/consumes the event stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_turn_with_thoughts_and_tools_surfaces_in_exchange_log() {
    // ── 1. Wire the production CoreApi with a scripted NoopBackend ─────────────
    let interpreter = Arc::new(EdgeInferrer::new(
        Arc::new(StructuredTextInterpreter::new()),
    ));
    let backend: Arc<dyn AgentBackend> = Arc::new(NoopBackend::scripted(rich_turn()));
    let repo = setup_temp_repo();
    let _home_guard = makina_core::HOME_ENV_LOCK.lock().await;
    let temp_home = tempfile::tempdir().expect("create temp HOME");
    // SAFETY: serialized by HOME_ENV_LOCK for the duration of this async test.
    unsafe { std::env::set_var("HOME", temp_home.path()) };
    let wm = WorktreeManager::new(repo.path().to_path_buf(), "develop".into());
    let api: Arc<dyn Api> = Arc::new(CoreApi::new(interpreter, backend, wm, no_gate_config()));

    // ── 2. A real App consuming events the exact way the binary does ───────────
    // No initial runs are seeded: AgentExchange handling keys the log by the
    // composite (RunId, TaskId) to prevent cross-run stale data; the log is
    // built purely from the live events flowing through the real stack.
    let mut app = App::new(
        Arc::clone(&api),
        Vec::<RunView>::new(),
        std::path::PathBuf::from("."),
    );

    // Subscribe BEFORE starting so no events are missed.
    let mut stream = api.subscribe();

    // ── 3. OpenRun → StartRun (the two commands the TUI issues) ────────────────
    let (_list_dir, list_path) = write_task_list(ONE_TASK_LIST);
    let run = match api
        .execute(Command::OpenRun {
            task_list_path: list_path,
        })
        .await
        .expect("OpenRun must succeed for the single-task list")
    {
        CommandOutcome::RunOpened { run } => run,
        other => panic!("expected RunOpened, got {other:?}"),
    };

    api.execute(Command::StartRun { run })
        .await
        .expect("StartRun must be accepted");

    // ── 4. Consume the live stream into the App until the Developer turn ends ───
    //
    // Bounded by a wall-clock deadline so a wedged run can never hang the test.
    // We stop as soon as we have applied the Developer's `TurnComplete`
    // AgentExchange for our task — by then the prompt, both thought chunks, the
    // tool call, both tool updates and both text chunks have all been applied.
    let task_id = TaskId::new("solo-task");
    let deadline = Duration::from_secs(30);

    let saw_dev_turn_complete = tokio::time::timeout(deadline, async {
        while let Some(ev) = stream.next().await {
            let is_dev_turn_complete = matches!(
                &ev,
                Event::AgentExchange {
                    task,
                    role: AgentRole::Developer,
                    event: ExchangeEvent::TurnComplete,
                    ..
                } if *task == task_id
            );

            // Feed the event into the App exactly as the IO loop does.
            app.update(AppEvent::ApiEvent(ev));

            if is_dev_turn_complete {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);

    assert!(
        saw_dev_turn_complete,
        "did not observe the Developer's TurnComplete for {task_id:?} within {deadline:?}; \
         exchange_logs = {:?}",
        app.exchange_logs.keys().collect::<Vec<_>>()
    );

    // ── 5. Assert on the accumulated per-task exchange log ─────────────────────
    // The log is keyed by (RunId, TaskId) — the composite key that prevents
    // cross-run stale data when two runs share the same task slug.
    let log = app
        .exchange_logs
        .get(&(run, task_id.clone()))
        .expect("the solo-task exchange log must exist (keyed by (RunId, TaskId))");

    // (a) The prompt entry is present (Developer role).
    let prompt = log
        .entries
        .iter()
        .find(|e| e.is_prompt())
        .expect("a prompt entry must be present");
    assert_eq!(
        prompt.role,
        AgentRole::Developer,
        "the prompt is the Developer's"
    );
    assert!(
        !prompt.text().is_empty(),
        "the prompt entry must carry the prompt text"
    );

    // (b) At least one Thought entry is present and carries the scripted
    //     reasoning (the two consecutive thought chunks coalesce into one entry).
    let thoughts: Vec<&str> = log
        .entries
        .iter()
        .filter_map(|e| match &e.content {
            ExchangeContent::Thought { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !thoughts.is_empty(),
        "at least one Thought entry must be present; entries = {:?}",
        log.entries
    );
    let all_thought_text = thoughts.join("");
    assert!(
        all_thought_text.contains("I need to read the trait first."),
        "the thought text must contain the scripted reasoning; got {all_thought_text:?}"
    );
    assert!(
        all_thought_text.contains("Now I'll edit it."),
        "both scripted thought chunks must be present; got {all_thought_text:?}"
    );

    // (c) Exactly ONE Tool entry for id "tc-1", and its status is the FINAL
    //     "completed" — proving the two ToolCallUpdates mutated it in place rather
    //     than appending new entries.
    let tool_entries: Vec<(&str, &str)> = log
        .entries
        .iter()
        .filter_map(|e| match &e.content {
            ExchangeContent::Tool { id, status, .. } => Some((id.as_str(), status.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_entries.len(),
        1,
        "exactly one Tool entry must exist (upsert by id, not append); got {tool_entries:?}"
    );
    assert_eq!(tool_entries[0].0, "tc-1", "the tool entry is keyed tc-1");
    assert_eq!(
        tool_entries[0].1, "completed",
        "the tool status must be the FINAL 'completed' after the two in-place updates"
    );

    // (d) After response segmentation, the scripted turn's two TextChunks appear
    //     as TWO separate Response entries (one before the thought/tool, one after).
    //     Collect all of them and assert their texts are exactly the scripted chunks —
    //     no thought/tool text must leak into any response entry.
    let response_texts: Vec<&str> = log
        .entries
        .iter()
        .filter_map(|e| match &e.content {
            ExchangeContent::Response { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !response_texts.is_empty(),
        "at least one Response entry must be present; entries = {:?}",
        log.entries
    );
    let all_response_text = response_texts.join("");
    assert_eq!(
        all_response_text, "Working on it. Done.",
        "all Response segments concatenated must equal the TextChunks (no thought/tool leakage); \
         segments = {response_texts:?}"
    );

    // Keep the temp repo alive until here (worktrees were created off it).
    drop(repo);
}
