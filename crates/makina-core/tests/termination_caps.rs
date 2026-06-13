//! Integration tests for **termination caps** (task 25) — each of the three caps
//! independently drives a task to terminal `Failed`.
//!
//! # Acceptance criterion
//!
//! Task 25 "done when": *each cap independently drives a task to `failed`.*
//! These tests build the real actor tree (`RootSupervisor` → `Supervisor` hub +
//! per-task `Developer`/`Reviewer` spokes) over a temporary git repo, drive a
//! `TaskGraph` through `RunReadyTasks`, and assert the task ends `Failed` for:
//!
//! 1. **Gate cap** — an always-failing gate (`{name:"false",command:"false"}`)
//!    with a small `caps.gate_iterations` → `Failed` (GateCapReached) after the
//!    cap, worktree torn down.
//! 2. **Reviewer cap** — `NoopBackend` always rejects + a small
//!    `caps.reviewer_iterations` (2) → `Failed` (ReviewCapReached) after exactly
//!    `reviewer_iterations` rejects, worktree torn down, Developer re-dispatched
//!    the expected number of times.
//! 3. **Wall-clock cap** — an instrumented slow backend whose Developer turn
//!    reliably exceeds a 1s `caps.wall_clock_secs` → `Failed`
//!    (WallClockCapReached), resources cleaned up (no worktree/branch leak).
//!
//! # Test-strategy compliance (see `docs/spec/testing-strategy.md`)
//!
//! - The agent stand-in is always an in-process backend (`NoopBackend` or the
//!   `SlowBackend` wrapper below) — no real CLI, no model call.
//! - The gate-cap "gate" is the trivial deterministic shell builtin `false` run
//!   in the temp worktree (NOT the env-dependent toolchain gates the strategy
//!   forbids).
//! - Determinism: caps 1 & 2 are `ask`-driven (no sleeps); cap 3 uses a backend
//!   that sleeps WELL past the 1s cap (so the timeout always fires) plus a bounded
//!   poll for the worktree teardown — never a fixed sleep to "wait for" success.
//! - Each test uses a fresh temporary git repo (`tempfile`); the real repo is
//!   never touched.  The temp-repo setup mirrors the other integration tests.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream;

use makina_core::actors::{
    RunReadyTasks, SetSpokes, SetTaskGraph, Supervisor, SupervisorArgs, TaskGraphSnapshot,
};
use makina_core::backend::noop::NoopBackend;
use makina_core::backend::{
    AgentBackend, AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream, SessionConfig,
};
use makina_core::config::{BackendConfig, CapsConfig, Config, GateConfig, PlannerConfig};
use makina_core::supervision::{RestartConfig, RootSupervisor};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

// ── Temp-repo helpers (mirror the other integration tests) ───────────────────────

/// Create a minimal git repository in a new temporary directory, on a `develop`
/// branch with one initial commit (so `git worktree add -b … develop` works).
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

/// Return true if `branch` exists in the repo at `path`.
fn branch_exists(path: &std::path::Path, branch: &str) -> bool {
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy()])
        .args(["branch", "--list", branch])
        .output()
        .expect("git branch --list");
    !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

// ── Config / task / actor-tree builders ──────────────────────────────────────────

/// Build a resolved [`Config`] with explicit caps and gates.
///
/// The backend command is a non-empty placeholder (the tests use in-process
/// backends, so it is never spawned) and `base_branch` is `develop` to match the
/// temp repo.
fn config(gates: Vec<GateConfig>, caps: CapsConfig) -> Config {
    use makina_core::config::{ProviderConfig, RolesConfig};

    Config {
        backend: BackendConfig {
            command: "noop".into(),
            args: vec![],
        },
        providers: vec![ProviderConfig {
            name: "default".into(),
            command: "noop".into(),
            args: vec![],
            env: Default::default(),
        }],
        roles: RolesConfig::default(),
        planner: PlannerConfig::default(),
        caps,
        concurrency: 1,
        gates,
        base_branch: "develop".into(),
    }
}

/// Convenience: a single gate `{name, command}`.
fn gate(name: &str, command: &str) -> GateConfig {
    GateConfig {
        name: name.to_string(),
        command: command.to_string(),
        image: None,
        source: None,
    }
}

/// Build a `New` task with the given `id`.
fn task(id: &str) -> Task {
    task_with_deps(id, &[])
}

/// Build a `New` task with the given `id` and `depends_on` prerequisites
/// (mirrors the `task(id, &[deps])` helper style from `concurrency.rs`).
fn task_with_deps(id: &str, deps: &[&str]) -> Task {
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

/// Poll (bounded) until the worktree directory for `task_id` is gone.
///
/// On the wall-clock-timeout path the per-task `DriverGuard` schedules a
/// **detached** best-effort worktree removal (it cannot `.await` in `Drop`), so
/// teardown completes shortly *after* the run returns.  This polls a short
/// deadline rather than using a fixed sleep (per the testing-strategy).
async fn assert_worktree_gone(repo_root: &std::path::Path, task_id: &str) {
    // The ask path uses an empty plan_slug, so the plan-scoped worktree dir name
    // is `--{task_id}`.
    let worktree_path = repo_root
        .join(".makina")
        .join("worktrees")
        .join(format!("--{task_id}"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !worktree_path.exists() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "worktree {worktree_path:?} was not torn down within the deadline \
                 (wall-clock cap must clean up via DriverGuard)"
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Cap 1: gate-iteration cap → Failed
// ══════════════════════════════════════════════════════════════════════════════

/// **Gate cap → Failed** — an always-failing gate (`false`) with
/// `caps.gate_iterations = 2` drives the task to `Failed` (GateCapReached) at the
/// cap, and the worktree is torn down (no leak).
#[tokio::test]
async fn gate_cap_drives_task_to_failed() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // The developer always "succeeds" (produces output); the gate always fails,
    // so the loop is driven purely by the gate cap.  NoopBackend cycles, so a
    // single dev response covers every re-dispatch.
    let backend = NoopBackend::with_responses(vec!["dev attempt".into()]);
    let backend_probe = backend.clone();

    let cap = 2u32;
    let cfg = config(
        vec![gate("false", "false")],
        CapsConfig {
            gate_iterations: cap,
            reviewer_iterations: 5,
            wall_clock_secs: 1800,
            idle_secs: None,
        },
    );

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        cfg,
    )
    .await;

    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "gate-cap".into(),
            tasks: vec![task("doomed-gate")],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks must drive the loop without a hard error");

    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("doomed-gate"), TaskState::Failed)],
        "an always-failing gate must drive the task to Failed at the cap"
    );

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let t = snapshot
        .get(&TaskId::new("doomed-gate"))
        .expect("task present");
    assert_eq!(t.state, TaskState::Failed, "task must end Failed");
    assert_eq!(
        t.gate_iterations, cap,
        "gate_iterations should equal the cap when GateCapReached fires"
    );
    assert!(
        t.finished_at.is_some(),
        "finished_at must be stamped on terminal failure"
    );

    // The Developer was dispatched exactly `cap` times (one per gate iteration);
    // the Reviewer was never reached.
    let prompts = backend_probe.recorded_prompts();
    assert_eq!(
        prompts.len(),
        cap as usize,
        "developer dispatched once per gate iteration, reviewer never reached; got {prompts:?}"
    );

    // Worktree + branch torn down on the cap failure (no leak). The ask path uses
    // an empty plan_slug ⇒ `--doomed-gate` / `task/--doomed-gate`.
    assert!(
        !repo_root
            .join(".makina")
            .join("worktrees")
            .join("--doomed-gate")
            .exists(),
        "worktree must be torn down when the gate cap fails the task"
    );
    assert!(
        !branch_exists(&repo_root, "task/--doomed-gate"),
        "branch must be torn down when the gate cap fails the task"
    );

    root.kill();
}

// ══════════════════════════════════════════════════════════════════════════════
// Cap 2: reviewer-iteration cap → Failed
// ══════════════════════════════════════════════════════════════════════════════

/// **Reviewer cap → Failed** — the reviewer ALWAYS rejects and
/// `caps.reviewer_iterations = 2`, so the task fails via `ReviewCapReached` after
/// exactly 2 rejections.
///
/// Asserts:
/// - the task ends `Failed` (ReviewCapReached), NOT looping forever;
/// - `review_iterations == 2` (== the cap) — the final cap-reaching rejection is
///   counted;
/// - the Developer was re-dispatched the expected number of times (2: the initial
///   turn + one re-work after the first reject) and the Reviewer ran twice;
/// - the worktree is torn down (no leak).
#[tokio::test]
async fn reviewer_cap_drives_task_to_failed() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // No gates → develop advances straight to review every turn.  The reviewer
    // always rejects; NoopBackend cycles, so [dev, reject] covers every round.
    let backend = NoopBackend::with_responses(vec![
        "dev attempt".into(),
        r#"{"verdict":"reject","feedback":"still not good enough"}"#.into(),
    ]);
    let backend_probe = backend.clone();

    let cap = 2u32;
    let cfg = config(
        vec![],
        CapsConfig {
            gate_iterations: 5,
            reviewer_iterations: cap,
            wall_clock_secs: 1800,
            idle_secs: None,
        },
    );

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        cfg,
    )
    .await;

    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "reviewer-cap".into(),
            tasks: vec![task("doomed-review")],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks must drive the loop without a hard error");

    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("doomed-review"), TaskState::Failed)],
        "an always-rejecting reviewer must drive the task to Failed at the cap"
    );

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let t = snapshot
        .get(&TaskId::new("doomed-review"))
        .expect("task present");
    assert_eq!(t.state, TaskState::Failed, "task must end Failed");
    assert_eq!(
        t.review_iterations, cap,
        "review_iterations should equal the cap when ReviewCapReached fires"
    );
    assert!(
        t.finished_at.is_some(),
        "finished_at must be stamped on terminal failure"
    );

    // Prompts in order: dev1, review1(reject), dev2, review2(reject → cap).
    // The cap-reaching rejection does NOT trigger another develop turn, so the
    // Developer ran exactly `cap` times and the Reviewer exactly `cap` times.
    //
    // Distinguish the two by their distinctive prompt prefixes: the Developer
    // prompt opens "Implement the following task…"; the Reviewer prompt opens
    // "Review the work done…".  (A Developer RETRY prompt also mentions "a
    // reviewer rejected …" in its feedback body, so we must match on the
    // leading verb, not a substring search for "review".)
    let prompts = backend_probe.recorded_prompts();
    assert_eq!(
        prompts.len(),
        (cap as usize) * 2,
        "expected {} prompts ({cap} dev + {cap} review); got {prompts:?}",
        (cap as usize) * 2
    );
    let dev_prompts = prompts
        .iter()
        .filter(|p| p.starts_with("Implement the following task"))
        .count();
    let review_prompts = prompts
        .iter()
        .filter(|p| p.starts_with("Review the work done"))
        .count();
    assert_eq!(
        dev_prompts, cap as usize,
        "Developer must be dispatched exactly {cap} times; got {dev_prompts} (prompts: {prompts:?})"
    );
    assert_eq!(
        review_prompts, cap as usize,
        "Reviewer must run exactly {cap} times; got {review_prompts}"
    );

    // Worktree + branch torn down on the cap failure (no leak). The ask path uses
    // an empty plan_slug ⇒ `--doomed-review` / `task/--doomed-review`.
    assert!(
        !repo_root
            .join(".makina")
            .join("worktrees")
            .join("--doomed-review")
            .exists(),
        "worktree must be torn down when the reviewer cap fails the task"
    );
    assert!(
        !branch_exists(&repo_root, "task/--doomed-review"),
        "branch must be torn down when the reviewer cap fails the task"
    );

    root.kill();
}

// ══════════════════════════════════════════════════════════════════════════════
// Cap 3: wall-clock cap → Failed
// ══════════════════════════════════════════════════════════════════════════════

/// An [`AgentBackend`] whose every prompt sleeps `delay` before responding.
///
/// Used to make a task's Developer turn reliably exceed the 1s wall-clock cap so
/// the scheduler's per-task `tokio::time::timeout` fires deterministically.  It
/// otherwise mirrors the `NoopBackend` response shape (one `TextChunk` then
/// `TurnComplete`).  A `prompts` counter records how many prompts were *started*
/// (so the test can assert the Reviewer was never reached).
#[derive(Clone)]
struct SlowBackend {
    delay: Duration,
    prompts_started: Arc<AtomicUsize>,
}

impl SlowBackend {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            prompts_started: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn prompts_started(&self) -> usize {
        self.prompts_started.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AgentBackend for SlowBackend {
    async fn spawn(&self, _config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError> {
        Ok(Box::new(SlowSession {
            terminated: false,
            delay: self.delay,
            prompts_started: Arc::clone(&self.prompts_started),
        }))
    }
}

struct SlowSession {
    terminated: bool,
    delay: Duration,
    prompts_started: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentSession for SlowSession {
    async fn prompt(&mut self, _prompt: Prompt) -> Result<ResponseStream, BackendError> {
        if self.terminated {
            return Err(BackendError::Terminated);
        }
        self.prompts_started.fetch_add(1, Ordering::SeqCst);

        // Sleep WELL past the wall-clock cap.  This is the awaited point the
        // scheduler's timeout cancels — it is NOT a "wait for success" sleep; the
        // whole purpose is that the cap fires DURING this await.
        tokio::time::sleep(self.delay).await;

        let events: Vec<Result<ResponseEvent, BackendError>> = vec![
            Ok(ResponseEvent::TextChunk {
                text: "developer output".to_string(),
            }),
            Ok(ResponseEvent::TurnComplete { usage: None }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    async fn terminate(&mut self) -> Result<(), BackendError> {
        self.terminated = true;
        Ok(())
    }
}

/// **Wall-clock cap → Failed** — a backend whose Developer turn sleeps 10s while
/// `caps.wall_clock_secs = 1`, so the scheduler's per-task timeout fires during
/// the Developer turn and drives the task to `Failed` (WallClockCapReached).
///
/// Asserts:
/// - the task ends `Failed`;
/// - the Reviewer was never reached (only the first/Developer prompt ever
///   started);
/// - resources are cleaned up — neither the worktree dir nor the `task/{id}`
///   branch leak (teardown via the cancelled driver's `DriverGuard`).
///
/// The test is fast (≈1s, the cap) and deterministic: the 10s backend delay
/// always exceeds the 1s cap by a wide margin, so the timeout always wins.  A
/// per-test outer timeout turns any regression (cap not firing) into a fast
/// failure instead of a 10s hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wall_clock_cap_drives_task_to_failed() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // Developer turn sleeps 10s; the cap is 1s, so the cap always fires first.
    let backend = SlowBackend::new(Duration::from_secs(10));
    let backend_probe = backend.clone();

    let cfg = config(
        vec![],
        CapsConfig {
            gate_iterations: 5,
            reviewer_iterations: 5,
            wall_clock_secs: 1, // tiny per-task deadline
            idle_secs: None,
        },
    );

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        cfg,
    )
    .await;

    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "wall-clock-cap".into(),
            tasks: vec![task("doomed-slow")],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    // Outer timeout: the run must complete near the 1s cap.  If the wall-clock
    // cap regressed, the backend's 10s sleep would otherwise hang the suite —
    // this turns that into a fast, clear failure.
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        supervisor_ref.ask(RunReadyTasks).send(),
    )
    .await
    .expect("RunReadyTasks must complete near the 1s wall-clock cap (did the cap fire?)")
    .expect("RunReadyTasks must drive the loop without a hard error");

    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("doomed-slow"), TaskState::Failed)],
        "the wall-clock cap must drive the task to Failed"
    );

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let t = snapshot
        .get(&TaskId::new("doomed-slow"))
        .expect("task present");
    assert_eq!(t.state, TaskState::Failed, "task must end Failed");
    assert!(
        t.finished_at.is_some(),
        "finished_at must be stamped on terminal failure"
    );

    // Only the Developer turn ever started (the cap fired during it); the Reviewer
    // was never reached.
    assert_eq!(
        backend_probe.prompts_started(),
        1,
        "exactly one prompt (the Developer turn) should have started before the cap fired; got {}",
        backend_probe.prompts_started()
    );

    // Resources cleaned up via the cancelled driver's DriverGuard (best-effort,
    // detached — so poll a bounded deadline rather than asserting immediately).
    assert_worktree_gone(&repo_root, "doomed-slow").await;
    // Branch teardown rides along with the same `remove` call; once the worktree
    // dir is gone the branch is too (remove() removes both).
    // Empty plan_slug on the ask path ⇒ `task/--doomed-slow`.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !branch_exists(&repo_root, "task/--doomed-slow") {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("branch task/--doomed-slow leaked after the wall-clock cap fired");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    root.kill();
}

// ══════════════════════════════════════════════════════════════════════════════
// Transitive skip: a failed task's dependents → Skipped
// ══════════════════════════════════════════════════════════════════════════════

/// **Dependents of a failed task are Skipped** — when task `A` reaches terminal
/// `Failed` (here via the always-failing `false` gate at `gate_iterations = 2`),
/// the scheduler transitively moves every task that (transitively) depends on `A`
/// to `Skipped` so they do not dangle non-terminal forever (a non-`Done` dep
/// never unlocks them).
///
/// Graph (concurrency 2): `A = []`, `B = [A]`, `C = [A]`, `D = [B]`.  Only `A`
/// is ready; it fails at the gate cap.  `B`/`C` depend on `A` and `D` depends on
/// `B`, so the reverse-edge BFS skips all three.
///
/// Asserts: `A` is `Failed`; `B`, `C`, `D` each reach `Skipped` (with
/// `finished_at` stamped) in the final graph; and each appears as `Skipped` in
/// `report.outcomes`.
#[tokio::test]
async fn dependents_of_failed_task_are_skipped() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // The developer always "succeeds" (produces output); the `false` gate always
    // fails, so `A` is driven to `Failed` purely by the gate cap.
    let backend = NoopBackend::with_responses(vec!["dev attempt".into()]);

    let mut cfg = config(
        vec![gate("false", "false")],
        CapsConfig {
            gate_iterations: 2,
            reviewer_iterations: 5,
            wall_clock_secs: 1800,
            idle_secs: None,
        },
    );
    cfg.concurrency = 2;

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        cfg,
    )
    .await;

    // a = [], b = [a], c = [a], d = [b].
    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "skip-dependents".into(),
            tasks: vec![
                task_with_deps("a", &[]),
                task_with_deps("b", &["a"]),
                task_with_deps("c", &["a"]),
                task_with_deps("d", &["b"]),
            ],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks must drive the loop without a hard error");

    // `a` failed; `b`, `c`, `d` were transitively skipped.
    assert!(
        report
            .outcomes
            .contains(&(TaskId::new("a"), TaskState::Failed)),
        "a must be Failed in outcomes; got {:?}",
        report.outcomes
    );
    for id in ["b", "c", "d"] {
        assert!(
            report
                .outcomes
                .contains(&(TaskId::new(id), TaskState::Skipped)),
            "dependent {id} must appear as Skipped in outcomes; got {:?}",
            report.outcomes
        );
    }

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");

    let a = snapshot.get(&TaskId::new("a")).expect("a present");
    assert_eq!(a.state, TaskState::Failed, "a must end Failed");

    for id in ["b", "c", "d"] {
        let t = snapshot.get(&TaskId::new(id)).expect("dependent present");
        assert_eq!(
            t.state,
            TaskState::Skipped,
            "dependent {id} must end Skipped"
        );
        assert!(
            t.finished_at.is_some(),
            "dependent {id} must have finished_at stamped when Skipped"
        );
    }

    root.kill();
}
