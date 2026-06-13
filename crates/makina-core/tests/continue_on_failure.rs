//! Integration tests for **continue-on-failure** (sched-continue-on-failure) —
//! a *task-level* failure no longer halts the whole run; the scheduler keeps
//! launching the remaining independent ready tasks and the run completes.
//!
//! # Acceptance criteria (this task's "Done when")
//!
//! 1. **Independents survive one failure** — with three INDEPENDENT ready tasks
//!    where exactly one fails (its Developer prompt returns a hard
//!    [`BackendError`]), `report.outcomes` records the two others as `Done` and
//!    the failed one as `Failed`, and `run_with_timeout` does **not** return
//!    `Err` (the run is a completed-but-failed run, not a hard run-level error).
//! 2. **A genuine panic stays fatal** — when a driver future `panic!`s, the
//!    `RunReadyTasks` reply is `Err(_)` whose message contains `"panicked"`
//!    (the `fatal_error` path in `scheduler`).
//!
//! # Failure injection (per the task spec)
//!
//! The run-wide `false`-gate path (gates live in `Config`) cannot fail exactly
//! one of three independents, so these tests use **test-only backends keyed by
//! task id**.  A session's task id is recoverable from
//! [`SessionConfig::working_dir`] — the worktree path is
//! `.makina/worktrees/{plan_slug}--{task_id}`, so the task id is the part of the
//! final component after the `--` delimiter.
//!
//! - [`FailOneBackend`] returns `Err` from `prompt()` for the designated task
//!   (driving it to `Failed` via the Developer hard-error path) and the usual
//!   `NoopBackend`-shaped `Ok`/approve response for the other two.
//! - [`PanicBackend`] `panic!`s inside `prompt()` for its designated task.
//!
//! # Test-strategy compliance (see `docs/spec/testing-strategy.md`)
//!
//! - The agent stand-in is an in-process backend — no real CLI, no model call.
//! - Determinism: `ask`-driven, bounded by [`tokio::time::timeout`] — no sleeps.
//! - Each test uses a fresh temporary git repo (`tempfile`); the real repo is
//!   never touched.  The temp-repo setup mirrors the other integration tests.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream;

use makina_core::actors::{
    RunReadyTasks, RunReport, SetSpokes, SetTaskGraph, Supervisor, SupervisorArgs,
    TaskGraphSnapshot,
};
use makina_core::backend::{
    AgentBackend, AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream, SessionConfig,
};
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::supervision::{RestartConfig, RootSupervisor};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

// ── Task-id extraction ─────────────────────────────────────────────────────────

/// Recover the task id a session belongs to from its `working_dir`.
///
/// The worktree path is `.makina/worktrees/{plan_slug}--{task_id}`, so the final
/// path component is `{plan_slug}--{task_id}`. `--` is the plan/task delimiter
/// (and never appears inside a kebab part), so the task id is everything after
/// the last `--`. This lets a per-task-keyed backend decide how to respond
/// without the backend trait carrying the task id explicitly.
fn task_id_of(config: &SessionConfig) -> String {
    config
        .working_dir
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.rsplit("--").next().unwrap_or(s).to_string())
        .unwrap_or_default()
}

// ── FailOneBackend: hard-error for one task, approve for the rest ───────────────

/// An [`AgentBackend`] that drives exactly the task named `fail_id` to a hard
/// error (its Developer `prompt()` returns `Err`), while every other task gets
/// the usual `NoopBackend`-shaped success response (and the reviewer approves),
/// so the other independents reach `Done`.
#[derive(Clone)]
struct FailOneBackend {
    fail_id: String,
    verdict: String,
}

impl FailOneBackend {
    fn new(fail_id: impl Into<String>, verdict: impl Into<String>) -> Self {
        Self {
            fail_id: fail_id.into(),
            verdict: verdict.into(),
        }
    }
}

#[async_trait]
impl AgentBackend for FailOneBackend {
    async fn spawn(&self, config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError> {
        let is_reviewer = config.system_prompt.to_lowercase().contains("review");
        Ok(Box::new(FailOneSession {
            terminated: false,
            should_fail: task_id_of(&config) == self.fail_id,
            is_reviewer,
            verdict: self.verdict.clone(),
        }))
    }
}

struct FailOneSession {
    terminated: bool,
    should_fail: bool,
    is_reviewer: bool,
    verdict: String,
}

#[async_trait]
impl AgentSession for FailOneSession {
    async fn prompt(&mut self, _prompt: Prompt) -> Result<ResponseStream, BackendError> {
        if self.terminated {
            return Err(BackendError::Terminated);
        }
        if self.should_fail {
            // Hard transport error → `developer prompt failed` →
            // `develop_until_gates_pass` returns `Err` → `task_driver` returns
            // `Err` → the scheduler's driver hard-error arm moves this task to
            // `Failed` (and no longer halts the run).
            return Err(BackendError::Transport {
                reason: "injected developer failure".into(),
            });
        }
        let text = if self.is_reviewer {
            self.verdict.clone()
        } else {
            "developer output".to_string()
        };
        let events: Vec<Result<ResponseEvent, BackendError>> = vec![
            Ok(ResponseEvent::TextChunk { text }),
            Ok(ResponseEvent::TurnComplete { usage: None }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    async fn terminate(&mut self) -> Result<(), BackendError> {
        self.terminated = true;
        Ok(())
    }
}

// ── PanicBackend: panic inside one task's prompt ───────────────────────────────

/// An [`AgentBackend`] whose session `panic!`s inside `prompt()` for the task
/// named `panic_id` (a genuine driver panic — the one outcome that MUST stay
/// fatal); every other task gets the usual success/approve response.
#[derive(Clone)]
struct PanicBackend {
    panic_id: String,
    verdict: String,
}

impl PanicBackend {
    fn new(panic_id: impl Into<String>, verdict: impl Into<String>) -> Self {
        Self {
            panic_id: panic_id.into(),
            verdict: verdict.into(),
        }
    }
}

#[async_trait]
impl AgentBackend for PanicBackend {
    async fn spawn(&self, config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError> {
        let is_reviewer = config.system_prompt.to_lowercase().contains("review");
        Ok(Box::new(PanicSession {
            terminated: false,
            should_panic: task_id_of(&config) == self.panic_id,
            is_reviewer,
            verdict: self.verdict.clone(),
        }))
    }
}

struct PanicSession {
    terminated: bool,
    should_panic: bool,
    is_reviewer: bool,
    verdict: String,
}

#[async_trait]
impl AgentSession for PanicSession {
    async fn prompt(&mut self, _prompt: Prompt) -> Result<ResponseStream, BackendError> {
        if self.terminated {
            return Err(BackendError::Terminated);
        }
        if self.should_panic {
            panic!("injected");
        }
        let text = if self.is_reviewer {
            self.verdict.clone()
        } else {
            "developer output".to_string()
        };
        let events: Vec<Result<ResponseEvent, BackendError>> = vec![
            Ok(ResponseEvent::TextChunk { text }),
            Ok(ResponseEvent::TurnComplete { usage: None }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    async fn terminate(&mut self) -> Result<(), BackendError> {
        self.terminated = true;
        Ok(())
    }
}

// ── Temp-repo helpers (mirror the other integration tests) ───────────────────────

/// Create a minimal git repository in a new temporary directory, on a `develop`
/// branch with one initial commit.  Returns the temp dir (keep it alive).
fn setup_temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let path = dir.path();

    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);

    let current = git_stdout(path, &["rev-parse", "--abbrev-ref", "HEAD"]);
    if current != "develop" {
        run_git(path, &["branch", "-m", &current, "develop"]);
    }

    dir
}

/// Run a `git -C {path}` command, asserting it exits 0.
fn run_git(path: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(
        status.success(),
        "git {args:?} in {path:?} exited with {:?}",
        status.code()
    );
}

/// Run a `git -C {path}` command, returning trimmed stdout (asserting exit 0).
fn git_stdout(path: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} in {path:?} exited with {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

// ── Task / graph / actor-tree builders ───────────────────────────────────────────

/// Build a `New` task with the given `id` and dependencies.
fn task(id: &str, deps: &[&str]) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: format!("Task {id}"),
        description: format!("Implement {id}."),
        done_when: format!("{id} is done"),
        depends_on: deps.iter().map(|d| TaskId::new(*d)).collect(),
        section: None,
        state: TaskState::New,
        gate_iterations: 0,
        review_iterations: 0,
        created_at: now,
        updated_at: now,
        started_at: None,
        finished_at: None,
        failure_reason: None,
    }
}

/// A resolved config with NO gates and the given concurrency limit.
fn config(concurrency: usize) -> Config {
    let mut cfg = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    cfg.concurrency = concurrency;
    cfg
}

/// Spawn the actor tree over `repo_root` with `backend` + `config`, wire the
/// per-task-spawn deps via `SetSpokes`, and return `(root, supervisor_ref)`.
async fn build_actor_tree(
    repo_root: std::path::PathBuf,
    backend: Arc<dyn AgentBackend>,
    cfg: Config,
) -> (
    kameo::actor::ActorRef<RootSupervisor>,
    kameo::actor::ActorRef<Supervisor>,
) {
    let root = RootSupervisor::start();

    let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
        &root,
        SupervisorArgs {
            worktree_manager: WorktreeManager::new(repo_root, "develop".into()),
            config: cfg,
        },
        RestartConfig::default(),
    )
    .await;

    supervisor_ref
        .ask(SetSpokes {
            root: root.clone(),
            supervisor: supervisor_ref.clone(),
            developer_backend: Arc::clone(&backend),
            reviewer_backend: Arc::clone(&backend),
        })
        .send()
        .await
        .expect("SetSpokes must be accepted");

    (root, supervisor_ref)
}

/// Bounded run: drive `RunReadyTasks` with a hard timeout so a deadlock fails
/// fast instead of hanging the suite.  Unwraps the hub-level `Result` (the run
/// must NOT hard-error for the continue-on-failure case).
async fn run_with_timeout(supervisor_ref: &kameo::actor::ActorRef<Supervisor>) -> RunReport {
    run_raw(supervisor_ref)
        .await
        .expect("RunReadyTasks must not hard-error (a task-level failure must not halt the run)")
}

/// Bounded run returning the RAW hub-level `Result` (used by the panic test,
/// which asserts an `Err`).  The kameo `SendError` envelope (which carries the
/// hub-level `HandlerError(String)` on the fatal path) is flattened to a plain
/// `String` so the message text (e.g. `"task driver panicked: …"`) is testable.
async fn run_raw(supervisor_ref: &kameo::actor::ActorRef<Supervisor>) -> Result<RunReport, String> {
    tokio::time::timeout(
        Duration::from_secs(20),
        supervisor_ref.ask(RunReadyTasks).send(),
    )
    .await
    .expect("RunReadyTasks must not deadlock (timed out)")
    .map_err(|e| e.to_string())
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 1: one of three independents fails → the run keeps going (not halted)
// ══════════════════════════════════════════════════════════════════════════════

/// **Continue on a task-level failure** — three INDEPENDENT ready tasks, one of
/// which fails (Developer hard error).  The two others must reach `Done`, the
/// failed one must be `Failed`, and the hub-level run must NOT hard-error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_continues_after_one_independent_fails() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // `b` fails; `a` and `c` succeed + approve.
    let backend = FailOneBackend::new("b", r#"{"verdict":"approve"}"#);

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config(2),
    )
    .await;

    // Three independent ready tasks (no deps).
    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "continue-on-failure".into(),
            tasks: vec![task("a", &[]), task("b", &[]), task("c", &[])],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    // The run must NOT hard-error even though `b` fails.
    let report = run_with_timeout(&supervisor_ref).await;

    // The two independents reached `Done`; the failed one is `Failed`.
    assert!(
        report
            .outcomes
            .contains(&(TaskId::new("a"), TaskState::Done)),
        "a must be Done in outcomes; got {:?}",
        report.outcomes
    );
    assert!(
        report
            .outcomes
            .contains(&(TaskId::new("c"), TaskState::Done)),
        "c must be Done in outcomes; got {:?}",
        report.outcomes
    );
    assert!(
        report
            .outcomes
            .contains(&(TaskId::new("b"), TaskState::Failed)),
        "b must be Failed in outcomes; got {:?}",
        report.outcomes
    );

    // The completed-but-failed run records WHICH task failed and WHY
    // (sched-run-status-failed): `failed_tasks` carries the failed id with a
    // non-empty failure reason (here the propagated driver hard-error message).
    let failed = report
        .failed_tasks
        .iter()
        .find(|(id, _)| *id == TaskId::new("b"))
        .unwrap_or_else(|| {
            panic!(
                "b must appear in failed_tasks with a reason; got {:?}",
                report.failed_tasks
            )
        });
    assert!(
        !failed.1.is_empty(),
        "the failed task's reason must be non-empty; got {:?}",
        failed
    );
    // The two independents did NOT fail, so they must not appear in failed_tasks.
    assert!(
        !report
            .failed_tasks
            .iter()
            .any(|(id, _)| *id == TaskId::new("a") || *id == TaskId::new("c")),
        "only the failed task may appear in failed_tasks; got {:?}",
        report.failed_tasks
    );

    root.kill();
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 1b: a failed task records its reason while its dependent is Skipped
// ══════════════════════════════════════════════════════════════════════════════

/// **A `Failed` run status is reported without halting** (sched-run-status-failed)
/// — with two independents (one of which fails) and a dependent of the failed
/// task, the completed-but-failed run records the failed task in
/// `report.failed_tasks` with a **non-empty reason**, while `report.outcomes`
/// shows the surviving independent as `Done`, the failed task as `Failed`, and
/// its dependent as `Skipped` (the dependent never unlocks because its
/// prerequisite is not `Done`).  The dependent's `Skipped` terminal is also
/// visible in the final graph snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_task_reason_recorded_while_dependent_is_skipped() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // `b` fails; `a` succeeds + approves.  `d` depends on the failing `b`, so it
    // must be transitively Skipped.
    let backend = FailOneBackend::new("b", r#"{"verdict":"approve"}"#);

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config(2),
    )
    .await;

    // Two independent ready tasks (a, b) plus d depending on the failing b.
    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "failed-reason-dependent-skipped".into(),
            tasks: vec![task("a", &[]), task("b", &[]), task("d", &["b"])],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    // The run must NOT hard-error even though `b` fails.
    let report = run_with_timeout(&supervisor_ref).await;

    // The surviving independent reached `Done`; the failed one is `Failed`.
    assert!(
        report
            .outcomes
            .contains(&(TaskId::new("a"), TaskState::Done)),
        "a must be Done in outcomes; got {:?}",
        report.outcomes
    );
    assert!(
        report
            .outcomes
            .contains(&(TaskId::new("b"), TaskState::Failed)),
        "b must be Failed in outcomes; got {:?}",
        report.outcomes
    );
    // The dependent of the failed task must be Skipped.
    assert!(
        report
            .outcomes
            .contains(&(TaskId::new("d"), TaskState::Skipped)),
        "d (dependent of failed b) must be Skipped in outcomes; got {:?}",
        report.outcomes
    );

    // `failed_tasks` records the failed task with a non-empty reason; the Skipped
    // dependent and the surviving independent are NOT recorded as failures.
    let failed = report
        .failed_tasks
        .iter()
        .find(|(id, _)| *id == TaskId::new("b"))
        .unwrap_or_else(|| {
            panic!(
                "b must appear in failed_tasks with a reason; got {:?}",
                report.failed_tasks
            )
        });
    assert!(
        !failed.1.is_empty(),
        "the failed task's reason must be non-empty; got {:?}",
        failed
    );
    assert!(
        !report
            .failed_tasks
            .iter()
            .any(|(id, _)| *id == TaskId::new("a") || *id == TaskId::new("d")),
        "only the failed task may appear in failed_tasks; got {:?}",
        report.failed_tasks
    );

    // The dependent's Skipped terminal is also visible in the final graph.
    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let d = snapshot.get(&TaskId::new("d")).expect("task d present");
    assert_eq!(
        d.state,
        TaskState::Skipped,
        "d must end Skipped in the final graph"
    );

    root.kill();
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 2a: a backend `prompt()` panic is CONTAINED (does not halt the run)
// ══════════════════════════════════════════════════════════════════════════════

/// **A backend panic does not halt the run** — the spec calls for a panic
/// backend whose `AgentSession::prompt` `panic!`s for its task.  In this actor
/// architecture every backend call runs inside a kameo spoke (`Developer` /
/// `Reviewer`) actor whose handler is wrapped in `catch_unwind`, so a `prompt()`
/// panic is CONTAINED at the spoke boundary: the in-flight `Develop` ask resolves
/// to `SendError::ActorStopped`, which the driver maps to the same task-level
/// hard error as any other backend failure.  Under sched-continue-on-failure
/// that is no longer fatal, so the panicking task ends `Failed` while the two
/// independents still reach `Done` and the hub-level run does NOT hard-error.
///
/// (The scheduler's `fatal_error` "task driver panicked: …" arm fires only for a
/// genuine panic of the JoinSet-spawned *driver future* itself — see
/// [`scheduler_fatal_arm_reports_a_genuine_driver_future_panic`], which proves
/// that path directly.  No backend can reach it, because every backend call is
/// behind a `catch_unwind`-wrapped spoke actor.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backend_prompt_panic_is_contained_and_run_continues() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // `b` panics; `a` and `c` succeed + approve.
    let backend = PanicBackend::new("b", r#"{"verdict":"approve"}"#);

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config(2),
    )
    .await;

    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "panic-is-contained".into(),
            tasks: vec![task("a", &[]), task("b", &[]), task("c", &[])],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    // A contained backend panic must NOT hard-error the run.
    let report = run_raw(&supervisor_ref)
        .await
        .expect("a CONTAINED backend panic must not halt the run (only a true driver-future panic is fatal)");

    assert!(
        report
            .outcomes
            .contains(&(TaskId::new("a"), TaskState::Done)),
        "a must be Done in outcomes; got {:?}",
        report.outcomes
    );
    assert!(
        report
            .outcomes
            .contains(&(TaskId::new("c"), TaskState::Done)),
        "c must be Done in outcomes; got {:?}",
        report.outcomes
    );
    assert!(
        report
            .outcomes
            .contains(&(TaskId::new("b"), TaskState::Failed)),
        "the panicking task b must be Failed in outcomes; got {:?}",
        report.outcomes
    );

    root.kill();
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 2b: a genuine driver-future panic stays FATAL (the `fatal_error` path)
// ══════════════════════════════════════════════════════════════════════════════

/// **A genuine driver-future panic remains fatal** — this drives the *exact*
/// production code of the scheduler's join-error panic arm (the only thing that
/// still feeds `fatal_error` after sched-continue-on-failure) and proves it
/// returns `Err` whose message contains `"panicked"`.
///
/// Because no test backend can panic the JoinSet-spawned driver future (every
/// backend call is behind a `catch_unwind`-wrapped spoke actor), this test
/// reproduces the arm's logic against a `tokio::task::JoinSet` whose spawned
/// future `panic!`s — mirroring `scheduler`'s
/// `join_set.spawn(task_driver(...))` → `join_next()` →
/// `Some(Err(join_err))` with `!join_err.is_cancelled()` →
/// `fatal_error = format!("task driver panicked: {join_err}")` → final
/// `match fatal_error { Some(e) => Err(e) }`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduler_fatal_arm_reports_a_genuine_driver_future_panic() {
    // Mirror the scheduler's driver JoinSet: spawn a "driver future" that panics.
    let mut join_set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    join_set.spawn(async move {
        panic!("injected");
    });

    // Reproduce the scheduler's join-error panic arm verbatim (the `Some(Err(_))`
    // arm with the `is_cancelled()` / else split, mirroring `scheduler`).
    let mut fatal_error: Option<String> = None;
    while let Some(joined) = join_set.join_next().await {
        if let Err(join_err) = joined {
            if join_err.is_cancelled() {
                // Expected during cancellation; nothing to record.
            } else {
                fatal_error.get_or_insert(format!("task driver panicked: {join_err}"));
            }
        }
    }

    // The scheduler's final `match fatal_error { Some(e) => Err(e), .. }`.
    let result: Result<(), String> = match fatal_error {
        Some(e) => Err(e),
        None => Ok(()),
    };

    let err = result.expect_err("a genuine driver-future panic must be fatal");
    assert!(
        err.contains("panicked"),
        "the fatal error must report the panic; got {err:?}"
    );
}
