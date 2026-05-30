//! Integration test for **supervisor-audit-writer** — verifies that
//! `task_driver` calls `ctx.audit_registry.register(working_dir, run_uid,
//! run_id, slug, task_id)` on the `run_graph` code path.
//!
//! # Strategy
//!
//! 1. Build a `SpyAuditRegistry` — a lightweight `AuditRegistry` impl that
//!    records every `register(working_dir, run_uid, run_id, slug, task_id)`
//!    call into an `Arc<Mutex<Vec<RegisterCall>>>`.
//! 2. Drive a single task through `run_graph` (the orchestrator's real path)
//!    with `NoopBackend` and a temp repo.  Pass the spy + a known slug.
//! 3. Assert the spy captured a `register` call for the dispatched task with
//!    the correct `working_dir` (`repo_root/.makina/worktrees/{task_id}`), `run_id`
//!    (`"run:42"` for `RunId(42)`), slug, and task id.
//!
//! # Note
//!
//! `register` fires **after worktree creation** and **before** the Developer is
//! dispatched.  A task that reaches at least `InProgress` will have registered.
//! The `NoopBackend` here drives the task all the way to `Done`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use chrono::Utc;

use makina_core::actors::{RunControl, run_graph};
use makina_core::api::RunId;
use makina_core::audit::AuditRegistry;
use makina_core::backend::AgentBackend;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

// ── SpyAuditRegistry ────────────────────────────────────────────────────────────

/// One captured `register(…)` call.
#[derive(Debug, Clone)]
struct RegisterCall {
    working_dir: PathBuf,
    run_uid: String,
    run_id: String,
    slug: String,
    task_id: String,
}

/// Test spy: records every `register` call for later inspection.
#[derive(Clone, Default)]
struct SpyAuditRegistry {
    calls: Arc<Mutex<Vec<RegisterCall>>>,
}

impl SpyAuditRegistry {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Snapshot all calls recorded so far.
    fn recorded(&self) -> Vec<RegisterCall> {
        self.calls
            .lock()
            .expect("spy mutex must not be poisoned")
            .clone()
    }
}

impl AuditRegistry for SpyAuditRegistry {
    fn register(
        &self,
        working_dir: PathBuf,
        run_uid: String,
        run_id: String,
        slug: String,
        task_id: String,
    ) {
        self.calls
            .lock()
            .expect("spy mutex must not be poisoned")
            .push(RegisterCall {
                working_dir,
                run_uid,
                run_id,
                slug,
                task_id,
            });
    }
}

// ── Temp-repo helpers ────────────────────────────────────────────────────────────

fn setup_temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let path = dir.path();

    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);

    // Ensure the branch is named `develop` regardless of init.defaultBranch.
    let current_branch = String::from_utf8(
        Command::new("git")
            .args(["-C", &path.to_string_lossy()])
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("git rev-parse HEAD")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();

    if current_branch != "develop" {
        run_git(path, &["branch", "-m", &current_branch, "develop"]);
    }

    dir
}

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

fn task(id: &str) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: format!("Task {id}"),
        description: format!("Implement {id}."),
        done_when: format!("{id} is implemented"),
        depends_on: vec![],
        section: None,
        state: TaskState::New,
        gate_iterations: 0,
        review_iterations: 0,
        created_at: now,
        updated_at: now,
        started_at: None,
        finished_at: None,
    }
}

// ── Acceptance test ──────────────────────────────────────────────────────────────

/// `task_driver` must call `audit_registry.register(working_dir, run_uid,
/// run_id, slug, task_id)` on the `run_graph` code path, before dispatching the
/// Developer.
///
/// This test:
/// 1. Passes a `SpyAuditRegistry` and the slug `"audit-spy-slug"` to `run_graph`.
/// 2. Lets the task run to `Done` with `NoopBackend`.
/// 3. Asserts the spy captured exactly one `register` call with:
///    - `working_dir == repo_root/.makina/worktrees/audit-task`
///    - `run_uid == "audit-spy-run-uid"`
///    - `run_id == "run:42"` (from `RunId(42)`)
///    - `slug == "audit-spy-slug"`
///    - `task_id == "audit-task"`
#[tokio::test]
async fn run_graph_calls_audit_registry_register_on_dispatch() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let task_id_str = "audit-task";
    let slug = "audit-spy-slug";
    let run_uid = "audit-spy-run-uid";
    let run_id = RunId(42);

    // NoopBackend: developer responds with any text, reviewer approves.
    let backend: Arc<dyn AgentBackend> = Arc::new(NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]));

    // Spy registry — we'll clone the inner Arc before handing it off.
    let spy = SpyAuditRegistry::new();
    let spy_probe = spy.clone(); // retains access to the same `calls` Arc

    // TaskGraph with one task.
    let graph = Arc::new(tokio::sync::Mutex::new(TaskGraph {
        slug: slug.into(),
        tasks: vec![task(task_id_str)],
    }));

    let worktree_manager = WorktreeManager::new(repo_root.clone(), "develop".into());

    let config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());

    // Build a RunControl that uses `run_id` but is otherwise silent.
    let control = RunControl {
        run: run_id,
        sink: Arc::new(|_| {}),
        pause: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel: tokio_util::sync::CancellationToken::new(),
    };

    // Drive the graph through the orchestrator's real path.
    let report = run_graph(
        Arc::clone(&graph),
        worktree_manager,
        config,
        backend,
        control,
        Arc::new(spy),
        slug.to_string(),
        run_uid.to_string(),
    )
    .await
    .expect("run_graph must not error");

    // Task must reach Done so we know the driver ran past the register call.
    assert_eq!(
        report.outcomes,
        vec![(TaskId::new(task_id_str), TaskState::Done)],
        "task must reach Done via run_graph"
    );

    // ── Assert: register was called exactly once with the correct args ───────────
    let calls = spy_probe.recorded();
    assert_eq!(
        calls.len(),
        1,
        "audit_registry.register must be called exactly once (one task dispatched); got {calls:#?}"
    );

    let call = &calls[0];

    let expected_working_dir = repo_root
        .join(".makina")
        .join("worktrees")
        .join(task_id_str);
    assert_eq!(
        call.working_dir, expected_working_dir,
        "register: working_dir must be repo_root/.makina/worktrees/{task_id_str}"
    );

    assert_eq!(
        call.run_uid, run_uid,
        "register: run_uid must match the run_uid passed to run_graph"
    );

    assert_eq!(
        call.run_id,
        run_id.to_string(), // "run:42"
        "register: run_id must match RunId(42).to_string() == \"run:42\""
    );

    assert_eq!(
        call.slug, slug,
        "register: slug must match the graph slug passed to run_graph"
    );

    assert_eq!(
        call.task_id, task_id_str,
        "register: task_id must match the dispatched task's id"
    );
}
