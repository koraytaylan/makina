//! Integration test: Normalizer Repairs Malformed TASKS.md
//!
//! Acceptance criterion (plan 0031, task integration-normalizer-malformed-tasks):
//!
//! A malformed TASKS.md is detected, the normalizer repairs it from the
//! SCOPE.md + ARCHITECTURE.md brief, the repaired file is written to disk,
//! and the run opens successfully.
//!
//! # Test coverage
//!
//! 1. **Malformed TASKS.md detection and repair** — Create a plan dir with:
//!    - Valid SCOPE.md and ARCHITECTURE.md
//!    - Malformed TASKS.md (e.g., task heading before section heading)
//!
//!    Issue OpenRun on that plan dir; the normalizer should be invoked,
//!    repair the TASKS.md, write it back, and re-interpret it successfully.
//! 2. **Repaired file is persisted** — Assert the written TASKS.md contains
//!    well-formed markdown that can be re-interpreted deterministically.
//! 3. **Run opens successfully after repair** — The run transitions to the
//!    open-runs list, and the graph is registered with tasks from the
//!    normalized TASKS.md.
//!
//! # Test-strategy compliance
//!
//! - Backend is a `TestBackend` that returns a valid TASKS.md response for
//!   the normalizer's prompt.
//! - Each test uses a fresh temporary git repo and plan dir.
//! - No arbitrary sleeps.

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;

use makina_core::api::{Api, Command as ApiCommand, CommandOutcome, RunStatus};
use makina_core::backend::{
    AgentBackend, AgentSession, BackendError, Prompt, ResponseEvent, ResponseStream, SessionConfig,
};
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::StructuredTextInterpreter;
use makina_core::orchestrator::CoreApi;
use makina_core::test_support::setup_temp_repo;
use makina_core::worktree::WorktreeManager;

// ── Test backend that returns a valid TASKS.md for normalization ────────────

/// A mock backend for tests that returns a well-formed TASKS.md
/// when the normalizer invokes it.
struct TestBackend {
    /// The response to return for normalization requests.
    response: String,
}

impl TestBackend {
    fn new(response: String) -> Self {
        TestBackend { response }
    }
}

#[async_trait::async_trait]
impl AgentBackend for TestBackend {
    async fn spawn(&self, _config: SessionConfig) -> Result<Box<dyn AgentSession>, BackendError> {
        Ok(Box::new(TestSession {
            response: self.response.clone(),
        }))
    }
}

/// A mock session that returns the pre-configured response.
struct TestSession {
    response: String,
}

#[async_trait::async_trait]
impl AgentSession for TestSession {
    async fn prompt(&mut self, _prompt: Prompt) -> Result<ResponseStream, BackendError> {
        let response = self.response.clone();
        let stream = futures::stream::iter(vec![
            Ok(ResponseEvent::TextChunk { text: response }),
            Ok(ResponseEvent::TurnComplete { usage: None }),
        ])
        .boxed();
        Ok(stream)
    }

    async fn terminate(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
}

// ── Temp-repo and plan-dir helpers ───────────────────────────────────────────

/// Create a minimal git repository in a fresh tempdir on a `develop` branch.
/// Build a `CoreApi` over the deterministic interpreter + the test backend +
/// a temp-repo `WorktreeManager` + a no-gate `Config`.
fn build_api(repo_root: PathBuf, backend: Arc<dyn AgentBackend>) -> CoreApi {
    let interpreter = Arc::new(EdgeInferrer::new(
        Arc::new(StructuredTextInterpreter::new()),
    ));
    let wm = WorktreeManager::new(repo_root, "develop".into());
    let config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    CoreApi::new(interpreter, backend, wm, config)
}

// ── Test 1: Normalizer repairs malformed TASKS.md ──────────────────────────

/// **Acceptance (integration-normalizer-malformed-tasks):**
///
/// Sequence:
/// 1. Create a temp plan directory with:
///    - Valid SCOPE.md ("# Scope\n\nA test plan.")
///    - Valid ARCHITECTURE.md ("# Architecture\n\nOne workstream.")
///    - **Malformed** TASKS.md with a task heading BEFORE a section heading
///      (violates Makina convention; interpreter will reject it).
/// 2. Issue OpenRun on that TASKS.md.
/// 3. The normalizer detects the parse error, invokes the planner (mocked to
///    return a well-formed TASKS.md), writes the repaired version, and re-interprets it.
/// 4. Assert:
///    - The run opens successfully (transitions to open-runs list).
///    - The on-disk TASKS.md is the normalized (repaired) version.
///    - The registered graph contains tasks from the normalized TASKS.md.
#[tokio::test]
async fn normalizer_repairs_malformed_tasks() {
    // 1. Setup: Create a temp git repo and a plan directory.
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let plan_dir = tempfile::tempdir().expect("create plan tempdir");
    let plan_path = plan_dir.path();

    // Write valid SCOPE.md
    let scope = r#"# Scope

A test plan to verify normalizer integration.

## Why this plan

The normalizer should detect and repair malformed TASKS.md.

## In scope

Verify that a malformed but plan-convention-compliant dir is normalized.

## Out of scope

Non-plan dirs (without SCOPE.md + ARCHITECTURE.md).
"#;
    std::fs::write(plan_path.join("SCOPE.md"), scope).expect("write SCOPE.md");

    // Write valid ARCHITECTURE.md
    let arch = r#"# Architecture

## One workstream

All changes are in one workstream for simplicity.

**Edits:**

Add a single task that does nothing.
"#;
    // Note: ARCHITECTURE.md must exist for is_plan_convention_dir to return true
    std::fs::write(plan_path.join("ARCHITECTURE.md"), arch).expect("write ARCHITECTURE.md");

    // Write **malformed** TASKS.md (task heading before section heading).
    // This violates the Makina convention: sections must come before tasks.
    let malformed_tasks = r#"# Test Task List

This is a malformed task list.

### malformed-task — A task that comes before its section

This is a task defined before any section header, which violates convention.

- **Depends on:** —
- **Done when:** nothing.

## 0001 — The Workstream

The workstream header comes **after** the task, which is wrong.
"#;
    let tasks_path = plan_path.join("TASKS.md");
    std::fs::write(&tasks_path, malformed_tasks).expect("write malformed TASKS.md");

    // Prepare a well-formed TASKS.md to be returned by the mock normalizer.
    // Must be a valid Makina task list with proper structure:
    // # Title
    // ## NNNN – Workstream Name
    // ### task-id – Task Title
    let repaired_tasks = r#"# Test Task List

A test plan to verify normalizer integration.

---

## 0001 — The Workstream

### malformed-task — A task that comes before its section

This is a task defined within its section, now in the correct order.

- **Depends on:** —
- **Done when:** verify the task was normalized.
"#;

    // 2. Create API with the test backend that returns the repaired TASKS.md.
    let backend = Arc::new(TestBackend::new(repaired_tasks.to_string()));
    let api = build_api(repo_root.clone(), backend);

    // 3. Issue OpenRun on the malformed TASKS.md.
    let outcome = api
        .execute(ApiCommand::OpenRun {
            task_list_path: tasks_path.clone(),
        })
        .await
        .expect("OpenRun must succeed (normalizer repairs the TASKS.md)");

    // 4. Verify the run was opened.
    let run_id = match outcome {
        CommandOutcome::RunOpened { run } => run,
        other => panic!(
            "expected RunOpened, got {other:?}: may indicate normalizer or re-interpretation failed"
        ),
    };

    // 5. Verify the on-disk TASKS.md is now the repaired version.
    let written_tasks = std::fs::read_to_string(&tasks_path).expect("read written TASKS.md");
    assert!(
        written_tasks.contains("## 0001"),
        "written TASKS.md should have section header; got:\n{}",
        written_tasks
    );
    assert!(
        written_tasks.contains("### malformed-task"),
        "written TASKS.md should have task header; got:\n{}",
        written_tasks
    );
    // The malformed version had the task BEFORE the section; the repaired version
    // should have the section BEFORE the task (or in the correct order).
    // We can verify this by checking that "## 0001" comes before "### malformed-task"
    // in the text.
    let section_pos = written_tasks
        .find("## 0001")
        .expect("section header must exist");
    let task_pos = written_tasks
        .find("### malformed-task")
        .expect("task header must exist");
    assert!(
        section_pos < task_pos,
        "in repaired TASKS.md, section header (line {}) must come before task header (line {})",
        section_pos,
        task_pos
    );

    // 6. Verify the run is registered and accessible.
    let view = api
        .run(run_id)
        .await
        .expect("run(id) must return the registered run");

    // 7. Verify the run has the tasks from the repaired TASKS.md.
    assert!(
        !view.tasks.is_empty(),
        "repaired graph must have at least one task"
    );
    assert_eq!(
        view.status,
        RunStatus::Pending,
        "freshly opened run should be Pending"
    );

    // Find the malformed-task and verify it exists.
    let malformed_task = view
        .tasks
        .iter()
        .find(|t| t.id.0 == "malformed-task")
        .expect("malformed-task must be in the repaired graph");
    assert_eq!(
        malformed_task.title, "A task that comes before its section",
        "task title should match the repaired TASKS.md"
    );
}
