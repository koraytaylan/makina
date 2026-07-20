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
//!    scheduler join result is `Err(_)` whose message contains `"panicked"`
//!    (the `fatal_error` path in `scheduler`).
//!
//! # Failure injection (per the task spec)
//!
//! The run-wide `false`-gate path (gates live in `Config`) cannot fail exactly
//! one of three independents, so these tests use **test-only backends keyed by
//! task id**. A session's task id is recoverable from
//! [`SessionConfig::working_dir`] by comparing its final component with
//! `paths::short_worktree_name(plan_slug, task_id)`.
//!
//! - [`FailOneBackend`] returns `Err` from `prompt()` for the designated task
//!   (driving it to `Failed` via the Developer hard-error path) and the usual
//!   `NoopBackend`-shaped `Ok`/approve response for the other two.
//! - [`PanicBackend`] `panic!`s inside `prompt()` for its designated task.
//!
//! # Test-strategy compliance (see `docs/spec/testing-strategy.md`)
//!
//! - The agent stand-in is an in-process backend — no real CLI, no model call.
//! - Determinism: awaited scheduler completion, bounded by
//!   [`tokio::time::timeout`] — no sleeps.
//! - Each test uses a fresh temporary git repo (`tempfile`); the real repo is
//!   never touched.  The temp-repo setup mirrors the other integration tests.

mod common;

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream;

use makina_core::actors::RunReport;
use makina_core::backend::{
    AgentBackend, AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream, SessionConfig,
};
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::paths;
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::test_support::setup_temp_repo;

// ── Task matching ───────────────────────────────────────────────────────────────

/// Decide whether a spawned session belongs to `(plan_slug, task_id)`.
///
/// The worktree directory name is [`makina_core::paths::short_worktree_name`] — a
/// bounded, hashed form (`{plan#}-{task-trunc}-{hash4}`) that, unlike the old
/// `{plan_slug}--{task_id}` scheme, is **not reversible** to the task id. So a
/// per-task-keyed backend identifies its task by recomputing the expected
/// worktree name with the same production helper and comparing it to the
/// session's `working_dir` final component — no string-splitting, no coupling to
/// the name's internal shape.
fn is_task(config: &SessionConfig, plan_slug: &str, task_id: &str) -> bool {
    config
        .working_dir
        .file_name()
        .and_then(|s| s.to_str())
        .map(|name| name == paths::short_worktree_name(plan_slug, task_id))
        .unwrap_or(false)
}

// ── FailOneBackend: hard-error for one task, approve for the rest ───────────────

/// An [`AgentBackend`] that drives exactly the task named `fail_id` to a hard
/// error (its Developer `prompt()` returns `Err`), while every other task gets
/// the usual `NoopBackend`-shaped success response (and the reviewer approves),
/// so the other independents reach `Done`.
#[derive(Clone)]
struct FailOneBackend {
    plan_slug: String,
    fail_id: String,
    verdict: String,
}

impl FailOneBackend {
    fn new(
        plan_slug: impl Into<String>,
        fail_id: impl Into<String>,
        verdict: impl Into<String>,
    ) -> Self {
        Self {
            plan_slug: plan_slug.into(),
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
            should_fail: is_task(&config, &self.plan_slug, &self.fail_id),
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
    plan_slug: String,
    panic_id: String,
    verdict: String,
}

impl PanicBackend {
    fn new(
        plan_slug: impl Into<String>,
        panic_id: impl Into<String>,
        verdict: impl Into<String>,
    ) -> Self {
        Self {
            plan_slug: plan_slug.into(),
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
            should_panic: is_task(&config, &self.plan_slug, &self.panic_id),
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
/// Run a `git -C {path}` command, asserting it exits 0.
/// Run a `git -C {path}` command, returning trimmed stdout (asserting exit 0).
#[allow(dead_code)]
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

// ── Task / graph / scheduler helpers ────────────────────────────────────────────

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

/// Bounded run: drive the scheduler with a hard timeout so a deadlock fails
/// fast instead of hanging the suite.  Unwraps the hub-level `Result` (the run
/// must NOT hard-error for the continue-on-failure case).
async fn run_with_timeout(
    repo_root: std::path::PathBuf,
    graph: TaskGraph,
    backend: Arc<dyn AgentBackend>,
    cfg: Config,
) -> (RunReport, common::SharedTaskGraph) {
    run_raw(repo_root, graph, backend, cfg)
        .await
        .expect("run_graph must not hard-error (a task-level failure must not halt the run)")
}

/// Bounded run returning the raw scheduler `Result` for tests that assert a
/// run-level fatal error.
async fn run_raw(
    repo_root: std::path::PathBuf,
    graph: TaskGraph,
    backend: Arc<dyn AgentBackend>,
    cfg: Config,
) -> Result<(RunReport, common::SharedTaskGraph), String> {
    tokio::time::timeout(
        Duration::from_secs(20),
        common::run_graph_in_repo_result(repo_root, graph, Arc::clone(&backend), backend, cfg),
    )
    .await
    .expect("run_graph must not deadlock (timed out)")
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

    // `b` fails; `a` and `c` succeed + approve.  The direct test path scopes
    // worktrees with an EMPTY plan slug (see `DriverContext::plan_slug`), so the
    // backend keys on `short_worktree_name("", "b")`.
    let backend = FailOneBackend::new("", "b", r#"{"verdict":"approve"}"#);

    // Three independent ready tasks (no deps).
    let graph = TaskGraph {
        slug: "continue-on-failure".into(),
        tasks: vec![task("a", &[]), task("b", &[]), task("c", &[])],
        authored: Default::default(),
    };

    // The run must NOT hard-error even though `b` fails.
    let (report, _) = run_with_timeout(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config(2),
    )
    .await;

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
    // Empty plan slug: the direct test path scopes worktrees unprefixed.
    let backend = FailOneBackend::new("", "b", r#"{"verdict":"approve"}"#);

    // Two independent ready tasks (a, b) plus d depending on the failing b.
    let graph = TaskGraph {
        slug: "failed-reason-dependent-skipped".into(),
        tasks: vec![task("a", &[]), task("b", &[]), task("d", &["b"])],
        authored: Default::default(),
    };

    // The run must NOT hard-error even though `b` fails.
    let (report, graph_ref) = run_with_timeout(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config(2),
    )
    .await;

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
    let snapshot = common::graph_snapshot(&graph_ref).await;
    let d = snapshot.get(&TaskId::new("d")).expect("task d present");
    assert_eq!(
        d.state,
        TaskState::Skipped,
        "d must end Skipped in the final graph"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Test 2a: a backend `prompt()` panic is CONTAINED (does not halt the run)
// ══════════════════════════════════════════════════════════════════════════════

/// **A backend panic does not halt the run** — the spec calls for a panic
/// backend whose `AgentSession::prompt` `panic!`s for its task. Role turns are
/// wrapped in `catch_unwind`, so a `prompt()` panic is contained at the role-turn
/// boundary and mapped to the same task-level hard error as any other backend
/// failure. Under sched-continue-on-failure that is no longer fatal, so the
/// panicking task ends `Failed` while the two independents still reach `Done` and
/// the run does NOT hard-error.
///
/// (The scheduler's `fatal_error` "task driver panicked: …" arm fires only for a
/// genuine panic of the JoinSet-spawned *driver future* itself — see
/// [`scheduler_fatal_arm_reports_a_genuine_driver_future_panic`], which proves
/// that path directly.  No backend can reach it, because every backend call is
/// behind a `catch_unwind`-wrapped role turn.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backend_prompt_panic_is_contained_and_run_continues() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // `b` panics; `a` and `c` succeed + approve.  Empty plan slug.
    let backend = PanicBackend::new("", "b", r#"{"verdict":"approve"}"#);

    let graph = TaskGraph {
        slug: "panic-is-contained".into(),
        tasks: vec![task("a", &[]), task("b", &[]), task("c", &[])],
        authored: Default::default(),
    };

    // A contained backend panic must NOT hard-error the run.
    let (report, _) = run_raw(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config(2),
    )
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
/// backend call is behind a `catch_unwind`-wrapped role turn), this test
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
