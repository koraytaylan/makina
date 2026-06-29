//! **The end-to-end trial (task 33 — `e2e-run`).**
//!
//! This is the culmination of the whole build: it drives the *exact* API the
//! TUI drives — [`makina_core::orchestrator::CoreApi`] — wired to a **real
//! authenticated ACP agent** ([`makina_acp::AcpBackend`], `gemini --acp` by
//! default) over a real git repository, and watches the full loop run:
//!
//! ```text
//!   OpenRun(dogfood-tasks.md)          → Planner (deterministic interpreter)
//!   StartRun                           → Supervisor scheduler
//!     per task:  git worktree off develop
//!                Developer (agent edits files in the worktree)
//!                gates  (cargo test / clippy / fmt, in the worktree)
//!                Reviewer (agent emits an approve/reject verdict)
//!                squash-merge task/{id} → develop   (on approve)
//! ```
//!
//! The **done-when** for the task: *at least one task reaches `Done` and lands
//! on `develop`, driven from the TUI.* Because the TUI is pure presentation over
//! `Arc<dyn Api>`, driving `CoreApi` directly here is the faithful automated
//! proof of "driven from the TUI"; the interactive TUI run is the manual
//! equivalent (documented in `docs/trial/e2e-run.md`).
//!
//! # SAFETY — never touch the live repo
//!
//! The loop creates worktrees + branches and **squash-merges into `develop`**.
//! This test therefore operates entirely on a **temporary `git clone`** of the
//! live repository (a local clone gets all of `develop`); the live working repo
//! is never used as the `repo_root`. The clone lives under
//! [`tempfile::tempdir`] and is removed when the test ends.
//!
//! # Why `#[ignore]`
//!
//! It needs a real, authenticated agent CLI, makes real model calls, compiles
//! the whole workspace inside each gate pass, and mutates a (throwaway) repo —
//! it is slow and environment-dependent. The normal `cargo test` stays green and
//! fast without it. Run it manually:
//!
//! ```bash
//! MAKINA_ACP_CMD=gemini MAKINA_ACP_ARGS=--acp \
//!     cargo test -p makina --test e2e -- --ignored --nocapture
//! ```
//!
//! `MAKINA_ACP_CMD` is the agent program (default `gemini`); `MAKINA_ACP_ARGS`
//! is a comma-separated arg list (default `--acp`). The agent must already be
//! authenticated — Makina inherits the environment and holds no credentials
//! (the Zed auth model).
//!
//! ## Permission handling — the ACP gateway
//!
//! Default-mode agents (gemini without `--yolo`) send a
//! `session/request_permission` server→client request before editing files.
//! Makina's permission gateway handles this transparently: the ACP transport
//! intercepts the `session/request_permission` message, a `WorktreePolicy`
//! auto-allows any tool call rooted inside the per-task worktree (selecting the
//! *allow-once* option), replies to the agent, and records an `AuditEntry` to
//! the Supervisor-owned `JsonlAuditSink` (`.tasks/{slug}/audit.jsonl`). The
//! agent receives its approval and continues normally.
//!
//! The deterministic asserting gate for this behaviour is the in-crate test
//! `gateway_threading_audit_sink_flows_through_backend_command_to_transport`
//! in `crates/makina-acp/src/backend.rs` (`#[cfg(test)] mod tests`), with the
//! companion integration test
//! `interleaved_permission_request_completes_turn_and_records_audit_entry` in
//! `crates/makina-acp/tests/backend_trait.rs`. Those tests prove that a
//! `session/request_permission` is answered and the turn completes, all without
//! `--yolo`. The `--yolo` bypass is no longer needed or used.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;

use makina_acp::AcpBackend;
use makina_core::api::{
    AgentRole, Api, Command, CommandOutcome, Event, ExchangeEvent, RunStatus, TaskState,
};
use makina_core::backend::AgentBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::StructuredTextInterpreter;
use makina_core::orchestrator::CoreApi;
use makina_core::worktree::WorktreeManager;

/// The base branch the trial runs against (and squash-merges into).
const BASE_BRANCH: &str = "develop";

/// Overall wall-clock budget for the whole run observed from the test side.
///
/// Real model turns + cold `cargo` compiles in each gate pass are *minutes*.
/// This bounds the test so a wedged agent cannot hang CI forever; it is well
/// above the per-task `caps.wall_clock_secs` so the engine's own caps fire
/// first under normal conditions.
const OBSERVE_DEADLINE: Duration = Duration::from_secs(20 * 60);

// ── git helpers (operate on the temp clone only) ────────────────────────────────

/// Run `git -C {repo} {args}`, asserting it exits 0 (panics with stderr).
fn git(repo: &Path, args: &[&str]) {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} in {repo:?} failed (code {:?}):\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Run `git -C {repo} {args}` and return trimmed stdout (asserting exit 0).
fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} in {repo:?} failed (code {:?}):\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Number of commits reachable from `develop` in the clone.
fn develop_commit_count(repo: &Path) -> usize {
    git_stdout(repo, &["rev-list", "--count", BASE_BRANCH])
        .parse()
        .expect("commit count is a number")
}

// ── env / setup ─────────────────────────────────────────────────────────────────

/// Resolve the agent program + args from the environment, defaulting to
/// `gemini --acp` (the verified, signed-in CLI used throughout the build).
fn agent_program_and_args() -> (String, Vec<String>) {
    let program = std::env::var("MAKINA_ACP_CMD").unwrap_or_else(|_| "gemini".to_string());
    let args: Vec<String> = std::env::var("MAKINA_ACP_ARGS")
        .map(|s| s.split(',').map(|a| a.trim().to_string()).collect())
        .unwrap_or_else(|_| vec!["--acp".to_string()]);
    (program, args)
}

/// The live repository root: the workspace this test crate lives in.
///
/// `CARGO_MANIFEST_DIR` is `<live>/crates/makina`; the repo root is two levels
/// up. We only ever *read* from here (as the `git clone` source); we never make
/// it the `repo_root` of the engine.
fn live_repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent() // <live>/crates
        .and_then(Path::parent) // <live>
        .expect("repo root is two levels above the crate manifest")
        .to_path_buf()
}

/// `git clone` the live repo into a fresh tempdir, check out `develop`, and give
/// the clone a local commit identity (so worktree commits + the squash-merge can
/// commit). Returns the tempdir (keep it alive) and the clone path.
///
/// A *local* clone of a repo whose current branch is `develop` checks out
/// `develop` and carries its full history — so the dogfood task list committed on
/// `develop` is present in the clone.
fn clone_live_repo() -> (tempfile::TempDir, PathBuf) {
    let live = live_repo_root();
    let dir = tempfile::tempdir().expect("create tempdir for the clone");
    let clone = dir.path().join("makina-clone");

    // Clone (local, no hardlinks across the tempfs boundary issues — plain copy).
    let output = StdCommand::new("git")
        .args(["clone", "--quiet"])
        .arg(&live)
        .arg(&clone)
        .output()
        .expect("failed to spawn git clone");
    assert!(
        output.status.success(),
        "git clone of the live repo failed:\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    // Make sure we are on develop (clone checks out origin HEAD; be explicit).
    let current = git_stdout(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]);
    if current != BASE_BRANCH {
        // Create a local develop tracking origin/develop and switch to it.
        git(&clone, &["checkout", "-B", BASE_BRANCH, "origin/develop"]);
    }

    // A hermetic commit identity for the clone (worktree commit + squash-merge).
    git(&clone, &["config", "user.email", "e2e@makina.test"]);
    git(&clone, &["config", "user.name", "Makina E2E"]);
    // Don't sign throwaway commits even if the user's global config enables it.
    git(&clone, &["config", "commit.gpgsign", "false"]);

    (dir, clone)
}

/// Build the trial [`Config`]: the three project gates + `develop` + generous
/// caps (real model + cold compiles are slow). This mirrors the committed
/// `makina.toml` but is constructed explicitly so the test does not depend on
/// CWD-relative config discovery.
fn trial_config() -> Config {
    let project = ProjectConfig::from_toml_str(
        r#"
        base_branch = "develop"
        concurrency = 2

        [caps]
        gate_iterations     = 5
        reviewer_iterations = 3
        wall_clock_secs     = 1200

        [[gates]]
        name    = "test"
        command = "cargo test"

        [[gates]]
        name    = "clippy"
        command = "cargo clippy -- -D warnings"

        [[gates]]
        name    = "fmt"
        command = "cargo fmt --check"
        "#,
        "trial-makina.toml",
    )
    .expect("trial project config must parse");

    // A global layer carrying the backend command so `validate()` passes (the
    // engine itself uses the injected `AcpBackend`, not `config.backend`, but a
    // resolved Config still must be valid).
    let (program, args) = agent_program_and_args();
    let global = GlobalConfig::from_toml_str(
        &format!(
            r#"
            [backend]
            command = "{program}"
            args    = [{args}]
            "#,
            args = args
                .iter()
                .map(|a| format!("{a:?}"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        "trial-global",
    )
    .expect("trial global config must parse");

    let config = Config::resolve(global, project);
    config.validate().expect("trial config must validate");
    config
}

// ── The test ───────────────────────────────────────────────────────────────────

/// **Run the full loop end to end through the real CoreApi + a real agent.**
///
/// Drives a temp clone of this repo through Planner → Supervisor → Developer +
/// gates → Reviewer → squash-merge, and asserts at least one dogfood task reaches
/// `Done` AND a corresponding squashed `task(...)` commit lands on the clone's
/// `develop`, with the new `util.rs` content present on `develop`.
///
/// A multi-thread runtime so the background scheduler + the per-task actor tree +
/// the event-collector + the poll loop all make progress concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "e2e: needs a real authenticated ACP agent (gemini --acp) and is slow; run manually with --ignored"]
async fn full_loop_lands_a_task_on_develop() {
    // ── 1. Temp clone (NEVER the live repo) ───────────────────────────────────
    let (_clone_dir, clone) = clone_live_repo();
    eprintln!("[e2e] cloned live repo → {}", clone.display());

    let dogfood = clone.join("docs/trial/dogfood-tasks.md");
    assert!(
        dogfood.exists(),
        "dogfood task list must be present in the clone at {}",
        dogfood.display()
    );

    let develop_before = develop_commit_count(&clone);
    let head_before = git_stdout(&clone, &["rev-parse", BASE_BRANCH]);
    eprintln!("[e2e] develop before run: {head_before} ({develop_before} commits)");

    // ── 2. Wire CoreApi exactly as the TUI does — but with a REAL agent ───────
    let (program, args) = agent_program_and_args();
    eprintln!("[e2e] agent backend: {program} {args:?}");
    let backend: Arc<dyn AgentBackend> = Arc::new(AcpBackend::new(program, args));

    // Deterministic planner (the dogfood list is well-formed structured text —
    // no model call needed for planning), same as the TUI's `main.rs`.
    let interpreter = Arc::new(EdgeInferrer::new(
        Arc::new(StructuredTextInterpreter::new()),
    ));

    // Worktree manager + config rooted at the CLONE.
    let wm = WorktreeManager::new(clone.clone(), BASE_BRANCH.to_string());
    let config = trial_config();

    let api: Arc<dyn Api> = Arc::new(CoreApi::new(interpreter, backend, wm, config));

    // ── 3. Observe the live event stream (logged for visibility) ──────────────
    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let mut stream = api.subscribe();
        let sink = Arc::clone(&events);
        tokio::spawn(async move {
            while let Some(ev) = stream.next().await {
                log_event(&ev);
                sink.lock().unwrap().push(ev);
            }
        });
    }

    // ── 4. OpenRun → StartRun (the two commands the TUI issues) ───────────────
    let run = match api
        .execute(Command::OpenRun {
            task_list_path: dogfood.clone(),
        })
        .await
        .expect("OpenRun must succeed for the dogfood list")
    {
        CommandOutcome::RunOpened { run } => run,
        other => panic!("expected RunOpened, got {other:?}"),
    };
    eprintln!("[e2e] opened run {run}");

    // Snapshot the interpreted plan.
    let opened = api.run(run).await.expect("opened run must be queryable");
    eprintln!(
        "[e2e] planned {} task(s): {:?}",
        opened.tasks.len(),
        opened
            .tasks
            .iter()
            .map(|t| (
                t.id.0.clone(),
                t.depends_on.iter().map(|d| d.0.clone()).collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>()
    );
    assert!(!opened.tasks.is_empty(), "the plan must contain tasks");

    api.execute(Command::StartRun { run })
        .await
        .expect("StartRun must be accepted");
    eprintln!("[e2e] started run {run}; observing the loop (deadline {OBSERVE_DEADLINE:?})…");

    // ── 5. Observe until at least one task is Done, or the deadline elapses ────
    //
    // We do NOT require ALL tasks to finish (the done-when is "at least one task
    // reaches done and lands on develop"). We stop as soon as one task is Done,
    // or when the run reaches a terminal aggregate status, or on timeout.
    let deadline = tokio::time::Instant::now() + OBSERVE_DEADLINE;
    // Holds the latest view each poll, so it reflects the run state when the loop
    // exits (a task observed Done / terminal aggregate status / timeout).
    let final_view = loop {
        let view = api.run(run).await.expect("run must remain queryable");

        let any_done = view.tasks.iter().any(|t| t.state == TaskState::Done);
        if any_done {
            eprintln!("[e2e] a task reached Done — stopping observation");
            break view;
        }

        // Terminal aggregate status with no Done task → the run cannot make a task
        // land; stop and let the assertions report the partial progress.
        if matches!(view.status, RunStatus::Completed | RunStatus::Failed) {
            eprintln!(
                "[e2e] run reached terminal status {:?} with no Done task — stopping",
                view.status
            );
            break view;
        }

        if tokio::time::Instant::now() >= deadline {
            eprintln!("[e2e] OBSERVE_DEADLINE elapsed — stopping observation");
            break view;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    // ── 6. Report what happened (state + event-log highlights) ────────────────
    report_state(&final_view);
    report_event_highlights(&events.lock().unwrap());

    // Give a moment for the squash-merge + worktree teardown of the just-Done
    // task to settle on `develop` (the FSM merges before marking Done, so by the
    // time we observe Done the commit is already on develop — this is belt-and-
    // braces for the filesystem/git index).
    let done_task = final_view
        .tasks
        .iter()
        .find(|t| t.state == TaskState::Done)
        .cloned();

    // ── 7. Assert the done-when: a task is Done AND landed on develop ─────────
    let done_task = done_task.unwrap_or_else(|| {
        panic!(
            "DONE-WHEN NOT MET: no task reached Done within {OBSERVE_DEADLINE:?}.\n\
             Final task states: {:?}\n\
             See the event log above for how far the loop got (interpret/worktree/\
             developer/gates/review/merge).",
            final_view
                .tasks
                .iter()
                .map(|t| (t.id.0.clone(), format!("{:?}", t.state)))
                .collect::<Vec<_>>()
        )
    });
    eprintln!("[e2e] Done task: {}", done_task.id.0);

    // A squashed commit landed on develop: develop gained at least one commit and
    // its subject references a task.
    let develop_after = develop_commit_count(&clone);
    assert!(
        develop_after > develop_before,
        "develop must gain at least one squashed commit (before={develop_before}, after={develop_after})"
    );

    let develop_log = git_stdout(&clone, &["log", BASE_BRANCH, "--pretty=%s"]);
    eprintln!("[e2e] develop log after run:\n{develop_log}");
    let landed_subject = git_stdout(
        &clone,
        &["log", BASE_BRANCH, "-1", "--pretty=%s", "--grep=task("],
    );
    assert!(
        develop_log.contains("task("),
        "develop must carry a squash commit with a `task(...)` subject; log:\n{develop_log}"
    );
    eprintln!("[e2e] landed squash commit subject: {landed_subject:?}");

    // The dogfood work specifically adds `crates/makina-core/src/util.rs`. Assert
    // the file content is present on develop (proves real agent edits landed, not
    // just an empty commit). Read it out of the develop tree via `git show`.
    let util_on_develop = git_stdout(
        &clone,
        &[
            "show",
            &format!("{BASE_BRANCH}:crates/makina-core/src/util.rs"),
        ],
    );
    eprintln!(
        "[e2e] util.rs on develop ({} bytes):\n{}",
        util_on_develop.len(),
        util_on_develop
    );
    assert!(
        !util_on_develop.trim().is_empty(),
        "the new util.rs must exist and be non-empty on develop"
    );
    // The format-duration task (or kebab-validate) must have added a real fn.
    assert!(
        util_on_develop.contains("fn format_duration")
            || util_on_develop.contains("fn is_valid_kebab_id"),
        "util.rs on develop must contain a dogfood function; got:\n{util_on_develop}"
    );

    eprintln!(
        "[e2e] DONE-WHEN MET: task `{}` reached Done and landed on develop \
         (develop {develop_before} → {develop_after} commits).",
        done_task.id.0
    );

    // The temp clone is dropped here (tempdir cleanup); the live repo was never
    // used as the repo_root, so it is untouched by this run.
}

// ── logging helpers ─────────────────────────────────────────────────────────────

/// Log one engine event compactly (so the run is observable with `--nocapture`).
fn log_event(ev: &Event) {
    match ev {
        Event::RunOpened { run, .. } => eprintln!("  · RunOpened {run}"),
        Event::RunStatusChanged { run, status } => {
            eprintln!("  · RunStatusChanged {run} → {status:?}")
        }
        Event::TaskStateChanged { task, state, .. } => {
            eprintln!("  · TaskStateChanged {} → {state:?}", task.0)
        }
        Event::TaskIterationsUpdated {
            task,
            gate_iterations,
            review_iterations,
            ..
        } => eprintln!(
            "  · TaskIterations {} gate={gate_iterations} review={review_iterations}",
            task.0
        ),
        Event::SessionCapabilities { task, role, .. } => {
            eprintln!("  · SessionCapabilities {role:?}[{}]", task.0)
        }
        Event::CurrentModeUpdate { task, role, .. } => {
            eprintln!("  · CurrentModeUpdate {role:?}[{}]", task.0)
        }
        Event::AgentExchange {
            task, role, event, ..
        } => match event {
            ExchangeEvent::PromptSent { text } => {
                eprintln!("  · {role:?}[{}] ← prompt ({} chars)", task.0, text.len())
            }
            ExchangeEvent::ResponseChunk { text } => {
                // Keep chunk logging terse; chunks can be frequent.
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let preview: String = trimmed.chars().take(80).collect();
                    eprintln!("  · {role:?}[{}] → {preview}", task.0);
                }
            }
            ExchangeEvent::ThoughtChunk { text } => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    let preview: String = trimmed.chars().take(80).collect();
                    eprintln!("  · {role:?}[{}] ⟂ thought: {preview}", task.0);
                }
            }
            ExchangeEvent::ToolCall {
                id, title, status, ..
            } => eprintln!("  · {role:?}[{}] ⚙ tool {id} '{title}' [{status}]", task.0),
            ExchangeEvent::ToolCallUpdate { id, status, .. } => eprintln!(
                "  · {role:?}[{}] ⚙ tool {id} update [{}]",
                task.0,
                status.as_deref().unwrap_or("-")
            ),
            ExchangeEvent::TurnComplete => {
                eprintln!("  · {role:?}[{}] → [turn complete]", task.0)
            }
        },
        Event::TaskIdle {
            task, idle_secs, ..
        } => {
            eprintln!("  · TaskIdle {}[idle timeout: {idle_secs}s]", task.0)
        }
        Event::TaskRetried { task, .. } => {
            eprintln!("  · TaskRetried {}", task.0)
        }
        Event::RoleTurnMetrics {
            role,
            task,
            model,
            duration_ms,
            ..
        } => {
            eprintln!(
                "  · RoleTurnMetrics {role:?}[{}] model={model} duration={duration_ms}ms",
                task.0
            )
        }
        Event::ProjectDiscovered {
            gate_count,
            scanned_files,
        } => {
            eprintln!(
                "  · ProjectDiscovered gates={} scanned_files={}",
                gate_count, scanned_files
            )
        }
        Event::PlanOperation {
            plan_slug,
            phase,
            message,
            ..
        } => {
            eprintln!("  · PlanOperation {plan_slug} {phase:?}: {message}")
        }
        Event::RunIntegrationBranchLeft { run, branch } => {
            eprintln!("  · RunIntegrationBranchLeft {run} → {branch}")
        }
    }
}

/// Print the final per-task state table.
fn report_state(view: &makina_core::api::RunView) {
    eprintln!("[e2e] ── final run state ────────────────────────────────────");
    eprintln!("[e2e] run status: {:?}", view.status);
    for t in &view.tasks {
        eprintln!(
            "[e2e]   task {:<16} state={:<12?} gate_iters={} review_iters={}",
            t.id.0, t.state, t.gate_iterations, t.review_iterations
        );
    }
}

/// Summarise the captured event stream (counts + the stage milestones reached).
fn report_event_highlights(events: &[Event]) {
    let mut state_changes = 0usize;
    let mut prompts = 0usize;
    let mut chunks = 0usize;
    let mut turns = 0usize;
    let mut thoughts = 0usize;
    let mut tool_calls = 0usize;
    let mut tool_updates = 0usize;
    let mut saw_dev = false;
    let mut saw_reviewer = false;
    let mut reached: Vec<String> = Vec::new();

    for ev in events {
        match ev {
            Event::TaskStateChanged { state, .. } => {
                state_changes += 1;
                let label = format!("{state:?}");
                if !reached.contains(&label) {
                    reached.push(label);
                }
            }
            Event::AgentExchange { role, event, .. } => {
                match role {
                    AgentRole::Developer => saw_dev = true,
                    AgentRole::Reviewer => saw_reviewer = true,
                }
                match event {
                    ExchangeEvent::PromptSent { .. } => prompts += 1,
                    ExchangeEvent::ResponseChunk { .. } => chunks += 1,
                    ExchangeEvent::ThoughtChunk { .. } => thoughts += 1,
                    ExchangeEvent::ToolCall { .. } => tool_calls += 1,
                    ExchangeEvent::ToolCallUpdate { .. } => tool_updates += 1,
                    ExchangeEvent::TurnComplete => turns += 1,
                }
            }
            _ => {}
        }
    }

    eprintln!("[e2e] ── event-log highlights ──────────────────────────────");
    eprintln!("[e2e]   total events: {}", events.len());
    eprintln!("[e2e]   task state-changes: {state_changes} (reached: {reached:?})");
    eprintln!("[e2e]   agent prompts: {prompts}, response chunks: {chunks}, turns: {turns}");
    eprintln!(
        "[e2e]   thoughts: {thoughts}, tool calls: {tool_calls}, tool updates: {tool_updates}"
    );
    eprintln!("[e2e]   developer engaged: {saw_dev}, reviewer engaged: {saw_reviewer}");
}
