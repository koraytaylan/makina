//! Integration tests for **concurrency** (task 24) — running multiple tasks in
//! parallel up to `config.concurrency`, one Developer/Reviewer per task, with
//! merges into `develop` serialized.
//!
//! # Acceptance criterion
//!
//! Task 24 "done when": *independent tasks run in parallel up to the limit.*
//! These tests prove that **deterministically** (no timing-dependent sleeps):
//!
//! 1. **Parallel up to the limit** — with `concurrency = N` and `M > N`
//!    independent tasks, the max OBSERVED concurrent sessions == `N` (parallelism
//!    happened AND was capped at `N`), and every task reaches `Done`.
//! 2. **Cap enforced** — with `M ≫ N`, the max observed concurrency never exceeds
//!    `N`.
//! 3. **Dependencies still serialize** — `B` depends on `A`; `B`'s session never
//!    overlaps `A`'s and starts only after `A` finished.
//! 4. **Single Developer per task / no double-dispatch** — each task reaches a
//!    terminal state exactly once; each task is spawned the expected number of
//!    sessions (2: developer + reviewer) and no more.
//! 5. **Merge serialization** — under concurrency all approved tasks' commits
//!    land on `develop` (commit count == number of Done tasks) and `develop`
//!    stays clean (no corruption / interleaving).
//!
//! # How parallelism is proven deterministically (the barrier approach)
//!
//! Timing-based assertions are flaky, so we use an **instrumented backend**
//! ([`CountingBackend`]) that:
//!
//! - tracks the number of CONCURRENTLY-ACTIVE sessions (incremented when a
//!   session's first prompt begins, decremented on `terminate`) and records the
//!   MAX observed via an [`AtomicUsize`];
//! - optionally GATES each session's prompt on a [`tokio::sync::Barrier`] sized
//!   `N`, so exactly `N` sessions must be simultaneously active to proceed.
//!
//! With the semaphore allowing `N` drivers and a barrier requiring `N` parties,
//! the only way the run makes progress is for `N` sessions to be active at once —
//! making parallelism *observable*.  A per-test timeout
//! ([`tokio::time::timeout`]) turns any accidental deadlock (e.g. a cap bug that
//! lets fewer than `N` run) into a fast failure instead of a hang.
//!
//! Within one task the Developer and Reviewer sessions run sequentially (one
//! active at a time per driver), so the max observed concurrent sessions equals
//! the number of drivers running simultaneously — exactly the quantity under
//! test.
//!
//! # Test-strategy compliance (see `docs/spec/testing-strategy.md`)
//!
//! - The agent stand-in is an in-process backend (a counting wrapper around the
//!   `NoopBackend` semantics) — no real CLI, no model call, no gate command.
//! - Determinism via barrier + bounded `timeout` — no arbitrary sleeps.
//! - Each test uses a fresh temporary git repo (`tempfile`); the real repo is
//!   never touched.  The temp-repo setup mirrors the other integration tests.

mod common;

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream;

use makina_core::backend::{
    AgentBackend, AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream, SessionConfig,
};
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::test_support::setup_temp_repo;
use tokio::sync::Barrier;

// ── Instrumented backend ─────────────────────────────────────────────────────────

/// Shared instrumentation state for [`CountingBackend`].
struct CountingState {
    /// Number of sessions whose prompt is currently in-flight (active).
    active: AtomicUsize,
    /// The maximum value `active` ever reached (the peak observed concurrency).
    max_active: AtomicUsize,
    /// Total number of sessions spawned (one per Developer/Reviewer turn).
    spawned: AtomicUsize,
    /// Total number of prompts handled across all sessions.
    prompts: AtomicUsize,
    /// Optional barrier: when `Some(n)`, every session's prompt waits until `n`
    /// sessions are simultaneously waiting, forcing exactly `n` to be active at
    /// once.  `None` disables gating (used by the deps-serialize test, where the
    /// number of simultaneously-ready tasks varies).
    barrier: Option<Arc<Barrier>>,
    /// The verdict JSON the reviewer session should return.  Developer sessions
    /// return a fixed dev string.  A session is treated as a "reviewer" turn if
    /// its system prompt contains "review" (see [`CountingSession::prompt`]).
    verdict: String,
}

/// An [`AgentBackend`] that counts concurrently-active sessions, records the peak,
/// and (optionally) gates each prompt on a [`Barrier`] so concurrency is
/// deterministically observable.  It reproduces the `NoopBackend` response shape
/// (one `TextChunk` then `TurnComplete`) so the Developer/Reviewer actors behave
/// exactly as in the other integration tests.
#[derive(Clone)]
struct CountingBackend {
    state: Arc<CountingState>,
}

impl CountingBackend {
    /// Build a backend.  `barrier_parties = Some(n)` gates every prompt on an
    /// `n`-party barrier (forcing `n` simultaneously-active sessions);
    /// `None` disables gating.  `verdict` is the reviewer's verdict JSON.
    fn new(barrier_parties: Option<usize>, verdict: impl Into<String>) -> Self {
        let barrier = barrier_parties.map(|n| Arc::new(Barrier::new(n)));
        Self {
            state: Arc::new(CountingState {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                spawned: AtomicUsize::new(0),
                prompts: AtomicUsize::new(0),
                barrier,
                verdict: verdict.into(),
            }),
        }
    }

    fn max_observed(&self) -> usize {
        self.state.max_active.load(Ordering::SeqCst)
    }

    fn spawned(&self) -> usize {
        self.state.spawned.load(Ordering::SeqCst)
    }

    fn prompts(&self) -> usize {
        self.state.prompts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AgentBackend for CountingBackend {
    async fn spawn(&self, config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError> {
        self.state.spawned.fetch_add(1, Ordering::SeqCst);
        // A session is a "reviewer" turn iff its system prompt mentions review;
        // the Reviewer's role system prompt does (see crate::roles).
        let is_reviewer = config.system_prompt.to_lowercase().contains("review");
        Ok(Box::new(CountingSession {
            terminated: false,
            is_reviewer,
            state: Arc::clone(&self.state),
        }))
    }
}

/// A single session from [`CountingBackend`].
struct CountingSession {
    terminated: bool,
    is_reviewer: bool,
    state: Arc<CountingState>,
}

#[async_trait]
impl AgentSession for CountingSession {
    async fn prompt(&mut self, _prompt: Prompt) -> Result<ResponseStream, BackendError> {
        if self.terminated {
            return Err(BackendError::Terminated);
        }
        self.state.prompts.fetch_add(1, Ordering::SeqCst);

        // ── Mark this session ACTIVE and update the observed peak ──────────────
        let now_active = self.state.active.fetch_add(1, Ordering::SeqCst) + 1;
        // Monotonically raise the recorded max to `now_active`.
        self.state
            .max_active
            .fetch_max(now_active, Ordering::SeqCst);

        // ── Gate on the barrier (if configured) to FORCE simultaneity ──────────
        //
        // With an N-party barrier, this prompt blocks until N sessions are all
        // simultaneously active here — proving (deterministically, not via
        // timing) that N tasks ran in parallel.  The semaphore cap guarantees no
        // MORE than N reach here at once, so `max_active` settles to exactly N.
        if let Some(barrier) = &self.state.barrier {
            barrier.wait().await;
        }

        // ── Produce the response (NoopBackend-shaped) ──────────────────────────
        let text = if self.is_reviewer {
            self.state.verdict.clone()
        } else {
            "developer output".to_string()
        };
        let events: Vec<Result<ResponseEvent, BackendError>> = vec![
            Ok(ResponseEvent::TextChunk { text }),
            Ok(ResponseEvent::TurnComplete { usage: None }),
        ];

        // ── Session no longer active (prompt complete) ─────────────────────────
        self.state.active.fetch_sub(1, Ordering::SeqCst);

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

/// The number of commits reachable from `HEAD`.
fn commit_count(path: &std::path::Path) -> usize {
    git_stdout(path, &["rev-list", "--count", "HEAD"])
        .parse()
        .expect("commit count is a number")
}

/// `git status --porcelain` output (empty == clean working tree + index).
fn status_porcelain(path: &std::path::Path) -> String {
    git_stdout(path, &["status", "--porcelain"])
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
fn config_with_concurrency(concurrency: usize) -> Config {
    let mut cfg = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    cfg.concurrency = concurrency;
    cfg
}

/// Bounded run: drive the scheduler with a hard timeout so a deadlock fails
/// fast instead of hanging the suite.
async fn run_with_timeout(
    repo_root: std::path::PathBuf,
    graph: TaskGraph,
    backend: Arc<dyn AgentBackend>,
    config: Config,
) -> (makina_core::actors::RunReport, common::SharedTaskGraph) {
    tokio::time::timeout(
        Duration::from_secs(20),
        common::run_graph_in_repo_result(repo_root, graph, Arc::clone(&backend), backend, config),
    )
    .await
    .expect("run_graph must not deadlock (timed out)")
    .expect("run_graph must not hard-error")
}

// ── Test 1: parallel up to the limit (max observed == N) ─────────────────────────

/// **Done-when** — with `concurrency = N` and `M > N` INDEPENDENT tasks, exactly
/// `N` run at once (the `N`-party barrier proves at least `N` overlap; the
/// semaphore proves at most `N`), and all `M` tasks reach `Done`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_up_to_the_limit() {
    const N: usize = 2;
    const M: usize = 4;

    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // N-party barrier: every Developer/Reviewer prompt waits until N sessions are
    // simultaneously active, so progress REQUIRES N parallel drivers.
    let backend = CountingBackend::new(Some(N), r#"{"verdict":"approve"}"#);
    let probe = backend.clone();

    // M independent tasks (no deps).
    let tasks: Vec<Task> = (0..M).map(|i| task(&format!("task-{i}"), &[])).collect();
    let graph = TaskGraph {
        slug: "parallel-test".into(),
        tasks,
        authored: Default::default(),
    };

    let (report, graph_ref) = run_with_timeout(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config_with_concurrency(N),
    )
    .await;

    // All M tasks reached Done.
    assert_eq!(report.outcomes.len(), M, "every task must be reported once");
    for (_, state) in &report.outcomes {
        assert_eq!(*state, TaskState::Done, "every task must reach Done");
    }
    let snapshot = common::graph_snapshot(&graph_ref).await;
    for i in 0..M {
        assert_eq!(
            snapshot
                .get(&TaskId::new(format!("task-{i}")))
                .unwrap()
                .state,
            TaskState::Done,
            "task-{i} must be Done in the graph"
        );
    }

    // The CORE assertion: parallelism happened AND was capped at exactly N.
    // - >= N: the N-party barrier could only have released if N sessions were
    //   simultaneously active (otherwise the run would have timed out).
    // - <= N: the semaphore cap.  Together: max observed == N.
    assert_eq!(
        probe.max_observed(),
        N,
        "max observed concurrency must be exactly N (= {N}); got {}",
        probe.max_observed()
    );
}

// ── Test 2: cap enforced with M ≫ N (never exceeds N) ────────────────────────────

/// With many more tasks than the limit, the max observed concurrency must NEVER
/// exceed `N`.  (The barrier sized `N` also proves it reaches `N`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cap_enforced_with_many_tasks() {
    const N: usize = 3;
    const M: usize = 12;

    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let backend = CountingBackend::new(Some(N), r#"{"verdict":"approve"}"#);
    let probe = backend.clone();

    let tasks: Vec<Task> = (0..M).map(|i| task(&format!("t-{i:02}"), &[])).collect();
    let graph = TaskGraph {
        slug: "cap-test".into(),
        tasks,
        authored: Default::default(),
    };

    let (report, _) = run_with_timeout(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config_with_concurrency(N),
    )
    .await;

    assert_eq!(report.outcomes.len(), M, "all M tasks reported once");
    for (_, state) in &report.outcomes {
        assert_eq!(*state, TaskState::Done);
    }

    // Cap NEVER exceeded; and the N-party barrier proves it reached N.
    assert_eq!(
        probe.max_observed(),
        N,
        "max observed concurrency must equal the cap N (= {N}); got {}",
        probe.max_observed()
    );
}

// ── Test 3: dependencies still serialize (B never overlaps A) ─────────────────────

/// `B` depends on `A`.  With `concurrency = 2` (room to overlap if deps were
/// ignored) but NO barrier gating, `B`'s session must never overlap `A`'s — i.e.
/// the max observed concurrency is `1` (the two tasks ran strictly one after the
/// other because B could not start until A was `Done`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dependencies_still_serialize() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // No barrier (None): if we gated on a 2-party barrier, A alone could never
    // proceed (B isn't ready) → deadlock.  Instead we OBSERVE that A and B never
    // overlap via max_observed == 1.
    let backend = CountingBackend::new(None, r#"{"verdict":"approve"}"#);
    let probe = backend.clone();

    // B depends on A; B authored first to prove ordering is by readiness.
    let graph = TaskGraph {
        slug: "deps-test".into(),
        tasks: vec![task("task-b", &["task-a"]), task("task-a", &[])],
        authored: Default::default(),
    };
    graph.validate().expect("graph validates");

    let (report, _) = run_with_timeout(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config_with_concurrency(2), // room for 2, but the dep forbids overlap.
    )
    .await;

    // Both reached Done, and A completed before B (A is first in completion order
    // because B could not start until A finished).
    assert_eq!(report.outcomes.len(), 2, "both tasks reported");
    assert_eq!(
        report.outcomes[0],
        (TaskId::new("task-a"), TaskState::Done),
        "A must complete first (B blocked on A)"
    );
    assert_eq!(
        report.outcomes[1],
        (TaskId::new("task-b"), TaskState::Done),
        "B completes after A"
    );

    // The dependency forced strict serialization: A and B never overlapped, so at
    // most ONE session was ever active at a time.
    assert_eq!(
        probe.max_observed(),
        1,
        "a dependent task must not overlap its dependency (max concurrency 1); got {}",
        probe.max_observed()
    );
}

// ── Test 4: single Developer per task / no double-dispatch ───────────────────────

/// Each task is dispatched EXACTLY once: it appears once in the report, its
/// terminal state is recorded once, and the backend spawned exactly `2 * M`
/// sessions (one Developer + one Reviewer per task — no extra/duplicate
/// dispatch).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_dispatch_per_task() {
    const N: usize = 2;
    const M: usize = 5;

    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // No barrier: this test counts sessions/dispatches, not concurrency, so we
    // must not require simultaneity (M is odd; a 2-party barrier could strand the
    // final task).  Approve so each task runs exactly dev+review.
    let backend = CountingBackend::new(None, r#"{"verdict":"approve"}"#);
    let probe = backend.clone();

    let tasks: Vec<Task> = (0..M).map(|i| task(&format!("task-{i}"), &[])).collect();
    let graph = TaskGraph {
        slug: "dispatch-test".into(),
        tasks,
        authored: Default::default(),
    };

    let (report, _) = run_with_timeout(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config_with_concurrency(N),
    )
    .await;

    // Each task appears EXACTLY once in the report (no double-dispatch).
    assert_eq!(report.outcomes.len(), M, "exactly M outcomes");
    let mut ids: Vec<String> = report.outcomes.iter().map(|(id, _)| id.0.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        M,
        "every task id must appear exactly once (no dupes)"
    );
    for (_, state) in &report.outcomes {
        assert_eq!(*state, TaskState::Done);
    }

    // Exactly one Developer + one Reviewer session per task → 2 * M sessions and
    // 2 * M prompts.  More would mean a task was dispatched/worked more than once.
    assert_eq!(
        probe.spawned(),
        2 * M,
        "exactly 2 sessions (dev + review) per task; got {}",
        probe.spawned()
    );
    assert_eq!(
        probe.prompts(),
        2 * M,
        "exactly 2 prompts (dev + review) per task; got {}",
        probe.prompts()
    );
}

// ── Test 5: merge serialization (all commits land; develop clean) ────────────────

/// Under concurrency, every approved task's commit must land on `develop` (commit
/// count == number of Done tasks) and `develop` must be CLEAN afterwards — i.e.
/// the serialized merges did not race/corrupt the shared checkout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merges_into_develop_are_serialized_and_clean() {
    const N: usize = 3;
    const M: usize = 6;

    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // Barrier sized N to MAXIMIZE merge contention (N drivers reach the approve →
    // squash-merge step close together), stressing the develop merge lock.
    let backend = CountingBackend::new(Some(N), r#"{"verdict":"approve"}"#);

    let tasks: Vec<Task> = (0..M).map(|i| task(&format!("land-{i}"), &[])).collect();
    let graph = TaskGraph {
        slug: "merge-test".into(),
        tasks,
        authored: Default::default(),
    };

    let count_before = commit_count(&repo_root);

    let (report, _) = run_with_timeout(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config_with_concurrency(N),
    )
    .await;

    // All M tasks reached Done.
    assert_eq!(report.outcomes.len(), M);
    let done = report
        .outcomes
        .iter()
        .filter(|(_, s)| *s == TaskState::Done)
        .count();
    assert_eq!(done, M, "every task must reach Done");

    // EXACTLY one squashed commit per Done task landed on develop (serialized
    // merges — none lost to a race, none duplicated/corrupted).
    assert_eq!(
        commit_count(&repo_root),
        count_before + M,
        "develop must gain exactly one commit per Done task ({M})"
    );

    // develop is CLEAN (no half-staged squash / conflict markers left behind).
    // The `.makina/` directory is intentionally untracked (supervisor persistence
    // artifacts and worktree checkouts); filter it out so the merge-cleanliness
    // check stays meaningful.
    let dirty: String = status_porcelain(&repo_root)
        .lines()
        .filter(|line| !line.contains(".makina/"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        dirty.is_empty(),
        "develop must be clean after concurrent merges; status:\n{}",
        status_porcelain(&repo_root)
    );
}

// ── Test 6: per-driver start/end intervals are observable & overlap ───────────────

/// **Done-when** (`sched-parallelism-instrument`) — each driver's start/end
/// interval is observable via the already-existing `Task.started_at` /
/// `Task.finished_at` timestamps (stamped by `mark_started_locked` /
/// `mark_finished_locked`), read back through the shared graph snapshot.
///
/// With `concurrency = 2`, a 2-party barrier forces both drivers' prompts to be
/// simultaneously active, so the two tasks' `[started_at, finished_at]` intervals
/// MUST overlap.  We assert that overlap with strict inequalities (no weakening to
/// "non-None"):
///
/// ```text
/// a.started_at < b.finished_at && b.started_at < a.finished_at
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn driver_intervals_observable() {
    const N: usize = 2;

    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // 2-party barrier: both drivers' prompts must be simultaneously active to make
    // progress, guaranteeing their start/end intervals overlap deterministically.
    let backend = CountingBackend::new(Some(N), r#"{"verdict":"approve"}"#);
    let probe = backend.clone();

    // Two independent ready tasks (no deps) so both can run at once.
    let graph = TaskGraph {
        slug: "intervals-test".into(),
        tasks: vec![task("task-a", &[]), task("task-b", &[])],
        authored: Default::default(),
    };

    let (report, graph_ref) = run_with_timeout(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config_with_concurrency(N),
    )
    .await;

    // Both tasks reached Done.
    assert_eq!(report.outcomes.len(), 2, "both tasks must be reported");
    for (_, state) in &report.outcomes {
        assert_eq!(*state, TaskState::Done, "every task must reach Done");
    }

    // Cross-check: the 2-party barrier could only release if both drivers were
    // simultaneously active, so the peak observed concurrency is exactly N.
    assert_eq!(
        probe.max_observed(),
        N,
        "max observed concurrency must be exactly N (= {N}); got {}",
        probe.max_observed()
    );

    let snapshot = common::graph_snapshot(&graph_ref).await;

    let a = snapshot
        .get(&TaskId::new("task-a"))
        .expect("task-a in graph");
    let b = snapshot
        .get(&TaskId::new("task-b"))
        .expect("task-b in graph");

    let a_started = a.started_at.expect("task-a started_at stamped");
    let a_finished = a.finished_at.expect("task-a finished_at stamped");
    let b_started = b.started_at.expect("task-b started_at stamped");
    let b_finished = b.finished_at.expect("task-b finished_at stamped");

    // The intervals [a_started, a_finished] and [b_started, b_finished] OVERLAP.
    assert!(
        a_started < b_finished && b_started < a_finished,
        "driver intervals must overlap: a=[{a_started}, {a_finished}], b=[{b_started}, {b_finished}]"
    );
}

// ── Test 7: ≥2 drivers overlap at concurrency=2 (deterministic) ───────────────────

/// **Done-when** (`sched-parallelism-verify-test`) — prove real parallelism the
/// way the suite already does: via the deterministic barrier, NOT flaky
/// timestamp-only timing (the module doc above explicitly rejects timing-based
/// assertions and arbitrary sleeps).
///
/// Two INDEPENDENT ready tasks at `concurrency = 2` with a 2-party
/// `CountingBackend` barrier force simultaneity: progress is only possible once
/// both drivers' prompts are simultaneously active, so the peak observed
/// concurrency settles to exactly `2`.  We additionally read each driver's
/// `[started_at, finished_at]` interval (exposed by `sched-parallelism-instrument`
/// via the shared graph snapshot) and assert the two intervals intersect —
/// cross-checked against `max_observed() == 2` so the assertion stays
/// deterministic (the barrier, not timing, is what proves overlap).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drivers_overlap_under_concurrency_2() {
    const N: usize = 2;

    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // 2-party barrier: both drivers' prompts must be simultaneously active before
    // either can proceed, forcing deterministic overlap (no sleeps, no timing).
    let backend = CountingBackend::new(Some(N), r#"{"verdict":"approve"}"#);
    let probe = backend.clone();

    // Two independent ready tasks (no deps) so both can run at once.
    let graph = TaskGraph {
        slug: "overlap-test".into(),
        tasks: vec![task("task-a", &[]), task("task-b", &[])],
        authored: Default::default(),
    };

    let (report, graph_ref) = run_with_timeout(
        repo_root.clone(),
        graph,
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config_with_concurrency(N),
    )
    .await;

    // Both tasks reached Done.
    assert_eq!(report.outcomes.len(), N, "both tasks must be reported");
    for (_, state) in &report.outcomes {
        assert_eq!(*state, TaskState::Done, "every task must reach Done");
    }

    // The CORE assertion: the 2-party barrier could only have released if both
    // drivers were simultaneously active, so the peak observed concurrency is
    // exactly N — proving ≥2 drivers overlapped at concurrency=2.
    assert_eq!(
        probe.max_observed(),
        N,
        "max observed concurrency must be exactly N (= {N}); got {}",
        probe.max_observed()
    );

    // Cross-check via the `sched-parallelism-instrument` start/end intervals: read
    // each driver's [started_at, finished_at] and assert they intersect.  This is
    // anchored on `max_observed() == 2` above, so the overlap claim stays
    // deterministic (the barrier — not timing — is what forces it).
    let snapshot = common::graph_snapshot(&graph_ref).await;

    let a = snapshot
        .get(&TaskId::new("task-a"))
        .expect("task-a in graph");
    let b = snapshot
        .get(&TaskId::new("task-b"))
        .expect("task-b in graph");

    let a_started = a.started_at.expect("task-a started_at stamped");
    let a_finished = a.finished_at.expect("task-a finished_at stamped");
    let b_started = b.started_at.expect("task-b started_at stamped");
    let b_finished = b.finished_at.expect("task-b finished_at stamped");

    // The intervals [a_started, a_finished] and [b_started, b_finished] INTERSECT.
    assert!(
        a_started < b_finished && b_started < a_finished,
        "driver intervals must intersect: a=[{a_started}, {a_finished}], b=[{b_started}, {b_finished}]"
    );
}
