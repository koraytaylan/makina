//! Integration tests for the **Developer-side gate-iteration loop** (task 22).
//!
//! # Acceptance criteria ("done when")
//!
//! 1. **Passing gates advance to review → done** — a gate that always exits `0`
//!    lets the task reach `Done` (gates pass → review → approve).
//! 2. **Gate failure loops, then passes** — a gate that fails the first time then
//!    passes (a deterministic counter-file gate) still reaches `Done`, with
//!    `gate_iterations` incremented and the gate-failure output relayed to the
//!    Developer on the retry (verified via `recorded_prompts()`).
//! 3. **Cap moves the task to failed** — an always-failing gate with a small
//!    `caps.gate_iterations` drives the task to `Failed` (GateCapReached) and the
//!    worktree is torn down (no leak).
//!
//! # Test-strategy compliance (see `docs/spec/testing-strategy.md`)
//!
//! - Backend is always `NoopBackend` — no real agent CLI, no model call.
//! - The "gates" here are trivial deterministic shell builtins run in the temp
//!   worktree (`true`, `false`, a counter-increment) — NOT the env-dependent,
//!   slow toolchain commands (`cargo test`, `clippy`) the strategy forbids.  The
//!   task's done-when criteria explicitly require real `sh -c` gates run in the
//!   worktree; these builtins are fast, side-effect-confined, and deterministic.
//! - Determinism via `ask`/await — no arbitrary sleeps.
//! - Each test uses a fresh temporary git repo (`tempfile`); the real repo is
//!   never touched.  The temp-repo setup mirrors `tests/develop_review_loop.rs`.

use std::process::Command;
use std::sync::Arc;

use chrono::Utc;

use makina_core::actors::{
    RunReadyTasks, SetSpokes, SetTaskGraph, Supervisor, SupervisorArgs, TaskGraphSnapshot,
};
use makina_core::api::FailureKind;
use makina_core::backend::AgentBackend;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{BackendConfig, CapsConfig, Config, GateConfig, PlannerConfig};
use makina_core::supervision::{RestartConfig, RootSupervisor};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

// ── Temp-repo helper (mirrors tests/develop_review_loop.rs) ──────────────────────

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

// ── Config builder ───────────────────────────────────────────────────────────────

/// Build a resolved [`Config`] with the given `gates` and `gate_iterations` cap.
///
/// The backend command is a non-empty placeholder (the tests use `NoopBackend`,
/// so it is never actually spawned) and `base_branch` is `develop` to match the
/// temp repo.
fn config_with_gates(gates: Vec<GateConfig>, gate_iterations: u32) -> Config {
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
        caps: CapsConfig {
            gate_iterations,
            reviewer_iterations: 5,
            wall_clock_secs: 1800,
            idle_secs: None,
        },
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

/// Convenience: a single discovered gate `{name, command}`.
///
/// A `GateConfig` flagged as discovered (provenance only — execution treats it
/// identically to a configured gate). Uses the `source` field plan 0025 adds.
fn discovered_gate(name: &str, command: &str) -> GateConfig {
    GateConfig {
        name: name.to_string(),
        command: command.to_string(),
        image: None,
        source: Some("discovered".to_string()),
    }
}

// ── Task builder ───────────────────────────────────────────────────────────────

/// Build a `New` task with the given `id`.
fn task(id: &str) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: format!("Task {id}"),
        description: format!("Implement {id}."),
        done_when: format!("{id} is done"),
        depends_on: vec![],
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

// ── Actor-tree builder ───────────────────────────────────────────────────────────

/// Spawn the actor tree over `repo_root` with the given `backend` and `config`,
/// wire the concurrency deps into the hub via `SetSpokes`, and return
/// `(root, supervisor_ref)`.
///
/// Under task 24 the hub spawns a Developer/Reviewer pair **per task** itself, so
/// the helper only wires the means to do so (root ref, hub ref, shared backend);
/// it no longer pre-spawns a shared spoke pair.
async fn build_actor_tree(
    repo_root: std::path::PathBuf,
    backend: Arc<dyn AgentBackend>,
    config: Config,
) -> (
    kameo::actor::ActorRef<RootSupervisor>,
    kameo::actor::ActorRef<Supervisor>,
) {
    let root = RootSupervisor::start();

    let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
        &root,
        SupervisorArgs {
            worktree_manager: WorktreeManager::new(repo_root, "develop".into()),
            config,
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

// ── Test 1: passing gates advance to review → done ───────────────────────────────

/// **Done-when (1)** — a gate that always exits `0` lets the task pass gates,
/// advance to review, get approved, and reach `Done`.
#[tokio::test]
async fn passing_gates_advance_to_review_and_done() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // 1st prompt (developer) → dev output; 2nd (reviewer) → approve.
    let backend = NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);

    // A single gate that always passes.
    let config = config_with_gates(vec![gate("true", "true")], 5);

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config,
    )
    .await;

    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "gate-pass".into(),
            tasks: vec![task("build-thing")],
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
        vec![(TaskId::new("build-thing"), TaskState::Done)],
        "passing gates should let the task reach Done"
    );

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let t = snapshot
        .get(&TaskId::new("build-thing"))
        .expect("task present");
    assert_eq!(t.state, TaskState::Done);
    assert_eq!(
        t.gate_iterations, 0,
        "gates passed on the first try, so no gate iterations were counted"
    );

    // Worktree torn down after approval.
    assert!(
        !repo_root
            .join(".makina")
            .join("worktrees")
            .join("build-thing")
            .exists(),
        "worktree must be gone after the run"
    );

    root.kill();
}

// ── Test 2: gate failure loops, then passes ──────────────────────────────────────

/// **Done-when (2)** — a deterministic counter-file gate fails on the first run
/// (count reaches 1, `< 2`) and passes on the second (count reaches 2, `>= 2`).
///
/// Asserts:
/// - the task still reaches `Done`;
/// - `gate_iterations == 1` (exactly one gate failure);
/// - the Developer was re-dispatched WITH the gate-failure feedback — the retry
///   developer prompt contains the gate name and output (proves the loop + the
///   feedback path).
#[tokio::test]
async fn gate_failure_loops_then_passes_and_relays_feedback() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // Response cycle (one per prompt, in order):
    //   1. developer  → dev output (attempt 1)  [then gate FAILS]
    //   2. developer  → dev output (attempt 2)  [then gate PASSES]
    //   3. reviewer   → approve
    let backend = NoopBackend::with_responses(vec![
        "First attempt.".into(),
        "Second attempt after gate failure.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);
    let backend_probe = backend.clone();

    // Deterministic gate: increment a counter file in the worktree; fail until
    // it reaches 2.  Run #1: count=1 → `[ 1 -ge 2 ]` is false → exit 1 (fail).
    // Run #2: count=2 → `[ 2 -ge 2 ]` is true → exit 0 (pass).
    let counter_gate = gate(
        "counter",
        "count=$(cat .gatecount 2>/dev/null || echo 0); \
         count=$((count+1)); echo $count > .gatecount; \
         echo \"gate attempt $count\"; [ $count -ge 2 ]",
    );
    let config = config_with_gates(vec![counter_gate], 5);

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config,
    )
    .await;

    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "gate-retry".into(),
            tasks: vec![task("fix-thing")],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks must drive the loop without a hard error");

    // Despite the first gate failure, the task ends Done.
    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("fix-thing"), TaskState::Done)],
        "the task should reach Done after the gate fails once then passes"
    );

    // Exactly one gate failure was counted.
    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let t = snapshot
        .get(&TaskId::new("fix-thing"))
        .expect("task present");
    assert_eq!(t.state, TaskState::Done);
    assert_eq!(
        t.gate_iterations, 1,
        "exactly one gate failure should bump gate_iterations to 1"
    );

    // Three prompts: dev1, dev2(retry with gate feedback), review(approve).
    let prompts = backend_probe.recorded_prompts();
    assert_eq!(
        prompts.len(),
        3,
        "expected 3 prompts (2 dev + 1 review); got {prompts:?}"
    );

    // The retry developer prompt (2nd overall) must carry the gate-failure
    // feedback: the gate name and its captured output.
    assert!(
        prompts[1].contains("counter"),
        "the developer retry prompt must name the failing gate; got: {:?}",
        prompts[1]
    );
    assert!(
        prompts[1].contains("gate attempt 1"),
        "the developer retry prompt must include the gate's output; got: {:?}",
        prompts[1]
    );
    // The first developer prompt preceded the gate run, so it must NOT contain
    // the gate feedback.
    assert!(
        !prompts[0].contains("Gate `counter` failed"),
        "the first developer prompt must not contain gate feedback; got: {:?}",
        prompts[0]
    );

    assert!(
        !repo_root
            .join(".makina")
            .join("worktrees")
            .join("fix-thing")
            .exists(),
        "worktree must be gone after the run"
    );

    root.kill();
}

// ── Test 3: discovered gate failure loops back to developer ─────────────────────

/// **Discovered gate test (1)** — a discovered gate that fails on the first run
/// then passes (a deterministic counter-file gate) still reaches `Done`, with
/// `gate_iterations` incremented and the gate-failure output relayed to the
/// Developer on the retry (verified via `recorded_prompts()`).
///
/// This test proves that discovered gates participate identically to configured
/// gates in the loop: the failure loops back to the Developer via the existing
/// self-loop + feedback path.
#[tokio::test]
async fn discovered_gate_failure_loops_back_to_developer() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // Response cycle (one per prompt, in order):
    //   1. developer  → dev output (attempt 1)  [then gate FAILS]
    //   2. developer  → dev output (attempt 2)  [then gate PASSES]
    //   3. reviewer   → approve
    let backend = NoopBackend::with_responses(vec![
        "First attempt.".into(),
        "Second attempt after gate failure.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);
    let backend_probe = backend.clone();

    // Build a config with a configured gate (passing) and a discovered counter
    // gate that fails once then passes.
    let counter_gate = discovered_gate(
        "discovered-check",
        "count=$(cat .gatecount 2>/dev/null || echo 0); \
         count=$((count+1)); echo $count > .gatecount; \
         echo \"discovered gate attempt $count\"; [ $count -ge 2 ]",
    );
    let config = config_with_gates(vec![gate("configured", "true"), counter_gate], 5);

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config,
    )
    .await;

    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "discovered-gate-loop".into(),
            tasks: vec![task("test-task")],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks must drive the loop without a hard error");

    // Despite the discovered gate failure, the task ends Done.
    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("test-task"), TaskState::Done)],
        "the task should reach Done after the discovered gate fails once then passes"
    );

    // Exactly one gate failure was counted.
    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let t = snapshot
        .get(&TaskId::new("test-task"))
        .expect("task present");
    assert_eq!(t.state, TaskState::Done);
    assert_eq!(
        t.gate_iterations, 1,
        "exactly one gate failure (discovered) should bump gate_iterations to 1"
    );

    // Three prompts: dev1, dev2(retry with gate feedback), review(approve).
    let prompts = backend_probe.recorded_prompts();
    assert_eq!(
        prompts.len(),
        3,
        "expected 3 prompts (2 dev + 1 review); got {prompts:?}"
    );

    // The retry developer prompt (2nd overall) must carry the DISCOVERED gate-failure
    // feedback: the gate name and its captured output.
    assert!(
        prompts[1].contains("discovered-check"),
        "the developer retry prompt must name the failing discovered gate; got: {:?}",
        prompts[1]
    );
    assert!(
        prompts[1].contains("discovered gate attempt 1"),
        "the developer retry prompt must include the discovered gate's output; got: {:?}",
        prompts[1]
    );
    // The first developer prompt preceded the gate run, so it must NOT contain
    // the gate feedback.
    assert!(
        !prompts[0].contains("Gate `discovered-check` failed"),
        "the first developer prompt must not contain gate feedback; got: {:?}",
        prompts[0]
    );

    assert!(
        !repo_root
            .join(".makina")
            .join("worktrees")
            .join("test-task")
            .exists(),
        "worktree must be gone after the run"
    );

    root.kill();
}

// ── Test 4: passing gates proceed to reviewer ────────────────────────────────────

/// **Discovered gate test (2)** — a merged config with both a configured gate
/// (passing) and a discovered gate (passing) allows the task to pass all gates,
/// advance to review, get approved, and reach `Done` with `gate_iterations == 0`.
///
/// This test proves that passing discovered gates do not block the path to review.
#[tokio::test]
async fn passing_gates_proceed_to_reviewer() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // 1st prompt (developer) → dev output; 2nd (reviewer) → approve.
    let backend = NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]);
    let backend_probe = backend.clone();

    // Both a configured gate and a discovered gate that always pass.
    let config = config_with_gates(
        vec![
            gate("configured", "true"),
            discovered_gate("discovered-check", "true"),
        ],
        5,
    );

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config,
    )
    .await;

    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "discovered-pass".into(),
            tasks: vec![task("passing-task")],
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
        vec![(TaskId::new("passing-task"), TaskState::Done)],
        "passing gates (both configured and discovered) should let the task reach Done"
    );

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let t = snapshot
        .get(&TaskId::new("passing-task"))
        .expect("task present");
    assert_eq!(t.state, TaskState::Done);
    assert_eq!(
        t.gate_iterations, 0,
        "all gates passed on the first try, so no gate iterations were counted"
    );

    // Two prompts: dev and review. The reviewer must be reached (the reviewer
    // prompt contains JSON verdict text).
    let prompts = backend_probe.recorded_prompts();
    assert_eq!(
        prompts.len(),
        2,
        "expected 2 prompts (1 dev + 1 review); got {prompts:?}"
    );
    assert!(
        prompts[1]
            .to_lowercase()
            .contains("respond with only the json verdict"),
        "the reviewer prompt must be reached; got: {:?}",
        prompts[1]
    );

    assert!(
        !repo_root
            .join(".makina")
            .join("worktrees")
            .join("passing-task")
            .exists(),
        "worktree must be gone after the run"
    );

    root.kill();
}

// ── Test 5: cap moves the task to failed ─────────────────────────────────────────

/// **Discovered gate test (3)** — an always-failing **discovered** gate with
/// `caps.gate_iterations = 3` drives the task to `Failed` (GateCapReached) after
/// the cap is hit, the worktree is torn down, and the task's `failure_reason.kind`
/// is `FailureKind::GateCap`.
///
/// This test proves that discovered gates hit the shared cap and classify
/// `GateCap` identically to configured gates, and that the Reviewer is never
/// reached when the cap exhausts.
#[tokio::test]
async fn gate_cap_classifies_gatecap() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // The developer always "succeeds" (produces output); the gate always fails,
    // so the loop is driven purely by the gate cap.  NoopBackend cycles, so a
    // single dev response covers every re-dispatch.
    let backend = NoopBackend::with_responses(vec!["dev attempt".into()]);
    let backend_probe = backend.clone();

    let cap = 3u32;
    // A single always-failing discovered gate.
    let config = config_with_gates(vec![discovered_gate("always-fail", "false")], cap);

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config,
    )
    .await;

    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "discovered-gate-cap".into(),
            tasks: vec![task("doomed-task")],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks must drive the loop without a hard error");

    // The always-failing discovered gate drives the task to Failed via
    // GateCapReached.
    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("doomed-task"), TaskState::Failed)],
        "an always-failing discovered gate should drive the task to Failed at the cap"
    );

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let t = snapshot
        .get(&TaskId::new("doomed-task"))
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

    // The failure_reason must be set with kind == GateCap.
    assert!(
        t.failure_reason.is_some(),
        "failure_reason must be set when gate cap is reached"
    );
    assert_eq!(
        t.failure_reason.as_ref().unwrap().kind,
        FailureKind::GateCap,
        "failure_reason.kind must be GateCap"
    );

    // The Developer was dispatched exactly `cap` times (one per gate iteration),
    // and the Reviewer was NEVER reached (the task failed before review).
    let prompts = backend_probe.recorded_prompts();
    assert_eq!(
        prompts.len(),
        cap as usize,
        "developer dispatched once per gate iteration, reviewer never reached; got {prompts:?}"
    );
    assert!(
        prompts.iter().all(|p| !p
            .to_lowercase()
            .contains("respond with only the json verdict")),
        "the reviewer must never be prompted when the gate cap fails the task"
    );

    // The worktree was torn down on the cap failure (no leak).
    assert!(
        !repo_root
            .join(".makina")
            .join("worktrees")
            .join("doomed-task")
            .exists(),
        "worktree must be torn down when the gate cap fails the task"
    );

    root.kill();
}

// ── Test 6: cap moves the task to failed ─────────────────────────────────────────

/// **Done-when (3)** — an always-failing gate with `caps.gate_iterations = 3`
/// drives the task to `Failed` (GateCapReached) after the cap is hit, and the
/// worktree is torn down (no leak).
#[tokio::test]
async fn always_failing_gate_hits_cap_and_fails_task() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    // The developer always "succeeds" (produces output); the gate always fails,
    // so the loop is driven purely by the gate cap.  NoopBackend cycles, so a
    // single dev response covers every re-dispatch.
    let backend = NoopBackend::with_responses(vec!["dev attempt".into()]);
    let backend_probe = backend.clone();

    let cap = 3u32;
    let config = config_with_gates(vec![gate("false", "false")], cap);

    let (root, supervisor_ref) = build_actor_tree(
        repo_root.clone(),
        Arc::new(backend) as Arc<dyn AgentBackend>,
        config,
    )
    .await;

    supervisor_ref
        .ask(SetTaskGraph(TaskGraph {
            slug: "gate-cap".into(),
            tasks: vec![task("doomed-thing")],
        }))
        .send()
        .await
        .expect("SetTaskGraph must be accepted");

    let report = supervisor_ref
        .ask(RunReadyTasks)
        .send()
        .await
        .expect("RunReadyTasks must drive the loop without a hard error");

    // The always-failing gate drives the task to Failed via GateCapReached.
    assert_eq!(
        report.outcomes,
        vec![(TaskId::new("doomed-thing"), TaskState::Failed)],
        "an always-failing gate should drive the task to Failed at the cap"
    );

    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("snapshot ask must not fail")
        .expect("graph should be Some");
    let t = snapshot
        .get(&TaskId::new("doomed-thing"))
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

    // The Developer was dispatched exactly `cap` times (one per gate iteration),
    // and the Reviewer was NEVER reached (the task failed before review).
    let prompts = backend_probe.recorded_prompts();
    assert_eq!(
        prompts.len(),
        cap as usize,
        "developer dispatched once per gate iteration, reviewer never reached; got {prompts:?}"
    );
    assert!(
        prompts.iter().all(|p| !p
            .to_lowercase()
            .contains("respond with only the json verdict")),
        "the reviewer must never be prompted when the gate cap fails the task"
    );

    // The worktree was torn down on the cap failure (no leak).
    assert!(
        !repo_root
            .join(".makina")
            .join("worktrees")
            .join("doomed-thing")
            .exists(),
        "worktree must be torn down when the gate cap fails the task"
    );

    root.kill();
}
