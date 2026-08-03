//! A task must always reach a terminal state, so a run can never stall.
//!
//! Two paths used to leave a task **active** after its driver gave up, which
//! reads to the operator as "the run halted for no reason": the task sits in
//! `InProgress`/`InReview` forever, its dependents are `Skipped`, and the run
//! finalizes `Failed` with no recorded reason anywhere.
//!
//! 1. **Review-acceptance footprint corrections were uncapped.**  A correction
//!    rewinds `InReview → InProgress` exactly like a reviewer rejection, but it
//!    was neither counted nor capped, so a footprint the agent cannot fix
//!    (build output, a generated lockfile) looped until the wall-clock cap.
//! 2. **A driver error could leave its task non-terminal.**  Any `?` between two
//!    transitions returned the error while the task was still active; the
//!    scheduler then skipped its dependents but dropped the reason, and the task
//!    was not even retryable (`retry_task` requires `Failed`).

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream;

use makina_core::backend::{
    AgentBackend, AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream, SessionConfig,
};
use makina_core::config::{
    BackendConfig, CapsConfig, Config, MergeConfig, PlannerConfig, ProviderConfig, RolesConfig,
};
use makina_core::plan::AuthoredTaskStatus;
use makina_core::task::{
    AuthoredRepoPattern, AuthoredSeedOutcome, AuthoredTaskMetadata, Task, TaskGraph, TaskId,
    TaskState,
};
use makina_core::test_support::setup_temp_repo;

// ── A developer that always writes OUTSIDE the authored footprint ──────────────

/// Writes `undeclared.txt` into every task worktree it is spawned for, so the
/// Developer's `git add -A && git commit` puts an undeclared path on the task
/// branch.  The reviewer stand-in always approves, so every loop iteration
/// reaches the review-acceptance footprint check and is returned for correction
/// — the exact shape of a footprint the agent cannot fix.
struct UndeclaredWriteBackend {
    response: String,
}

#[async_trait]
impl AgentBackend for UndeclaredWriteBackend {
    async fn spawn(&self, config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError> {
        std::fs::write(config.working_dir.join("undeclared.txt"), "undeclared\n").map_err(
            |error| BackendError::Spawn {
                reason: error.to_string(),
            },
        )?;
        Ok(Box::new(CannedSession {
            response: self.response.clone(),
        }))
    }
}

struct CannedSession {
    response: String,
}

#[async_trait]
impl AgentSession for CannedSession {
    async fn prompt(&mut self, _prompt: Prompt) -> Result<ResponseStream, BackendError> {
        let events: Vec<Result<ResponseEvent, BackendError>> = vec![
            Ok(ResponseEvent::TextChunk {
                text: self.response.clone(),
            }),
            Ok(ResponseEvent::TurnComplete { usage: None }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    async fn terminate(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
}

// ── Fixtures ──────────────────────────────────────────────────────────────────

fn config(caps: CapsConfig) -> Config {
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
        gates: vec![],
        base_branch: "develop".into(),
        merge: MergeConfig::default(),
        theme_name: "Ayu Dark".into(),
    }
}

fn task(id: &str, deps: &[&str]) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: format!("Task {id}"),
        description: format!("Implement {id}."),
        done_when: format!("{id} is done"),
        depends_on: deps.iter().map(|dep| TaskId::new(*dep)).collect(),
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

/// Authored metadata whose footprint declares ONLY `declared.txt`, so anything
/// else the developer commits is an undeclared change.
fn authored(touches: &[&str]) -> AuthoredTaskMetadata {
    AuthoredTaskMetadata {
        source_path: "docs/plans/0001-x/tasks/0101-t.md".into(),
        workstream: "0001".into(),
        kind: "task".into(),
        gated: false,
        touches: touches
            .iter()
            .map(|path| AuthoredRepoPattern::Path((*path).into()))
            .collect(),
        status: AuthoredTaskStatus::Planned,
        merged_as: None,
        seed: AuthoredSeedOutcome::Seeded(TaskState::New),
        collision_dependencies: vec![],
        branch_base_oid: None,
    }
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    let output = std::process::Command::new("git")
        .args(["-C", &repo.to_string_lossy()])
        .args(["branch", "--list", branch])
        .output()
        .expect("git branch --list");
    !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

// ══════════════════════════════════════════════════════════════════════════════

/// An unfixable footprint violation must exhaust the REVIEWER cap and drive the
/// task to terminal `Failed` — not loop until the wall clock kills the run.
#[tokio::test]
async fn unfixable_footprint_corrections_terminate_at_the_reviewer_cap() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();
    let cap = 3u32;

    let mut authored_map = BTreeMap::new();
    authored_map.insert(TaskId::new("undeclared"), authored(&["declared.txt"]));
    let graph = TaskGraph {
        slug: "footprint-cap".into(),
        tasks: vec![task("undeclared", &[]), task("dependent", &["undeclared"])],
        authored: authored_map,
    };

    let backend = Arc::new(UndeclaredWriteBackend {
        // The reviewer parses this as an approval, so every iteration reaches
        // the review-acceptance footprint check.
        response: "{\"verdict\":\"approve\"}".into(),
    }) as Arc<dyn AgentBackend>;

    let (report, graph_ref) = tokio::time::timeout(
        Duration::from_secs(60),
        common::run_graph_in_repo_result(
            repo_root.clone(),
            graph,
            Arc::clone(&backend),
            backend,
            config(CapsConfig {
                gate_iterations: 5,
                reviewer_iterations: cap,
                // Long enough that only the reviewer cap can end this task: if
                // the correction loop is uncapped the test times out instead of
                // passing by accident.
                wall_clock_secs: 1800,
                idle_secs: None,
            }),
        ),
    )
    .await
    .expect("the run must terminate on the reviewer cap, not spin until the wall clock")
    .expect("a capped task is a task-level failure, not a run-level error");

    let snapshot = common::graph_snapshot(&graph_ref).await;
    let failed = snapshot
        .get(&TaskId::new("undeclared"))
        .expect("task present");
    assert_eq!(
        failed.state,
        TaskState::Failed,
        "an unfixable footprint must end the task, not leave it active",
    );
    assert_eq!(
        failed.review_iterations, cap,
        "each correction is a rejection and must be counted against the reviewer cap",
    );
    assert!(
        failed.finished_at.is_some(),
        "a terminal task must carry finished_at",
    );
    let reason = failed
        .failure_reason
        .as_ref()
        .expect("the operator must be told why the task failed");
    assert!(
        reason.message.contains("footprint"),
        "the reason must name the footprint correction; got {reason:?}",
    );
    assert!(
        report
            .failed_tasks
            .iter()
            .any(|(id, _)| id == &TaskId::new("undeclared")),
        "the run report must record the failed task; got {:?}",
        report.failed_tasks,
    );
    assert_eq!(
        snapshot.get(&TaskId::new("dependent")).unwrap().state,
        TaskState::Skipped,
        "a dependent of a failed task is skipped, and the run still completes",
    );
    assert!(
        !branch_exists(&repo_root, &common::scheduler_task_branch("undeclared")),
        "the cap path must tear the task workspace down",
    );
}

/// A task that lands cleanly is unaffected by the correction accounting.
#[tokio::test]
async fn a_declared_footprint_still_lands_without_consuming_review_budget() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let mut authored_map = BTreeMap::new();
    authored_map.insert(TaskId::new("declared"), authored(&["undeclared.txt"]));
    let graph = TaskGraph {
        slug: "footprint-ok".into(),
        tasks: vec![task("declared", &[])],
        authored: authored_map,
    };

    let backend = Arc::new(UndeclaredWriteBackend {
        response: "{\"verdict\":\"approve\"}".into(),
    }) as Arc<dyn AgentBackend>;

    let (_report, graph_ref) = tokio::time::timeout(
        Duration::from_secs(60),
        common::run_graph_in_repo_result(
            repo_root.clone(),
            graph,
            Arc::clone(&backend),
            backend,
            config(CapsConfig {
                gate_iterations: 5,
                reviewer_iterations: 3,
                wall_clock_secs: 1800,
                idle_secs: None,
            }),
        ),
    )
    .await
    .expect("run must complete")
    .expect("run must not hard-error");

    let snapshot = common::graph_snapshot(&graph_ref).await;
    let done = snapshot
        .get(&TaskId::new("declared"))
        .expect("task present");
    assert_eq!(done.state, TaskState::Done, "the declared write must land");
    assert_eq!(
        done.review_iterations, 0,
        "a task that never needed a correction must not spend review budget",
    );
}
