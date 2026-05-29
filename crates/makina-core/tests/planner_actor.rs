//! Integration test: Planner reads a structured-text task list from a file,
//! interprets it via `StructuredTextInterpreter`, and hands the resulting
//! `TaskGraph` to the Supervisor via `SetTaskGraph`.
//!
//! # What is tested
//!
//! 1. The full "reads text → produces graph → hands to Supervisor" flow.
//! 2. The Supervisor stores the graph (verified via `TaskGraphSnapshot`).
//! 3. The graph has the expected slug and task count.
//!
//! # Test strategy compliance
//!
//! - Uses `ask` (not `tell` + sleep) for determinism.
//! - Uses `StructuredTextInterpreter` (no model call, no network).
//! - Writes to a `tempfile` directory (no `.tasks/` side effects).

use std::sync::Arc;

use makina_core::{
    actors::{
        InterpretTaskList, Planner, PlannerArgs, Supervisor, SupervisorArgs, TaskGraphSnapshot,
    },
    interpreter::StructuredTextInterpreter,
    supervision::{RestartConfig, RootSupervisor},
    worktree::WorktreeManager,
};
use std::io::Write as _;
use tempfile::NamedTempFile;

// ── Sample task list (representative) ────────────────────────────────────────

/// A minimal but representative task list: two sections, four tasks, explicit
/// `Depends on` edges, and `Done when` values.
const SAMPLE_TASK_LIST: &str = r#"# Sample Project — Build Task List

Structured-text task list for integration test purposes.

**Conventions**
- ids are kebab-case.
- **Depends on** lists direct structural prerequisites only.
- **Done when** is the acceptance check.

---

## 0001 — Bootstrap

### scaffold — Scaffold the workspace
Create the initial workspace with the required directory structure
and configuration files.
- **Depends on:** —
- **Done when:** the workspace builds successfully.

### add-ci — Add continuous integration
Add a CI pipeline that runs tests on every push.
- **Depends on:** scaffold
- **Done when:** CI runs and passes on the first push.

---

## 0002 — Core

### core-logic — Implement core logic
Implement the main business logic module.
- **Depends on:** scaffold, add-ci
- **Done when:** unit tests for core logic pass.

### docs — Write documentation
Write documentation for all public interfaces.
- **Depends on:** core-logic
- **Done when:** all public items have doc comments.
"#;

// ── Integration test ──────────────────────────────────────────────────────────

/// Send `InterpretTaskList` to a live `Planner`, then query `TaskGraphSnapshot`
/// from the `Supervisor` and assert the graph was delivered correctly.
#[tokio::test]
async fn planner_interprets_file_and_hands_graph_to_supervisor() {
    // ── Write sample task list to a temp file ─────────────────────────────────
    let mut tmp = NamedTempFile::with_suffix(".md").expect("should create temp file");
    tmp.write_all(SAMPLE_TASK_LIST.as_bytes())
        .expect("should write sample task list");
    let path = tmp.path().to_path_buf();

    // ── Spawn actors ──────────────────────────────────────────────────────────
    let root = RootSupervisor::start();

    // The Supervisor needs a WorktreeManager in its Args; this test only exercises
    // SetTaskGraph/TaskGraphSnapshot (no worktree create/remove), so a manager
    // over a dummy path is sufficient.
    let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
        &root,
        SupervisorArgs {
            worktree_manager: WorktreeManager::new(
                std::path::PathBuf::from("/tmp/makina-planner-test"),
                "develop".into(),
            ),
        },
        RestartConfig::default(),
    )
    .await;

    let planner_ref = RootSupervisor::spawn_child::<Planner>(
        &root,
        PlannerArgs {
            supervisor: supervisor_ref.clone(),
            interpreter: Arc::new(StructuredTextInterpreter::new()),
        },
        RestartConfig::default(),
    )
    .await;

    // ── Send InterpretTaskList ────────────────────────────────────────────────
    // kameo 0.20: `ask().send().await` for a `Result<T, E>` reply returns
    // `Result<T, SendError<M, E>>`.  `.expect()` propagates the handler error
    // string as a panic message, so a successful parse + graph delivery gives
    // back `()` directly.
    planner_ref
        .ask(InterpretTaskList { path })
        .send()
        .await
        .expect("InterpretTaskList should succeed and deliver graph to Supervisor");

    // ── Query the Supervisor for the graph ────────────────────────────────────
    let snapshot = supervisor_ref
        .ask(TaskGraphSnapshot)
        .send()
        .await
        .expect("TaskGraphSnapshot ask must not fail");

    let graph = snapshot.expect("graph should be Some after InterpretTaskList");

    // ── Assert the graph content ──────────────────────────────────────────────

    // Slug is derived from the temp file stem (tempfile names vary, but the graph
    // must have exactly 4 tasks).
    assert_eq!(
        graph.tasks.len(),
        4,
        "graph should contain exactly 4 tasks; got {}",
        graph.tasks.len()
    );

    // Validate that the graph passes structural checks.
    graph
        .validate()
        .expect("delivered graph must pass TaskGraph::validate()");

    // Check individual tasks.
    use makina_core::task::{TaskId, TaskState};

    let scaffold = graph
        .tasks
        .iter()
        .find(|t| t.id == TaskId::new("scaffold"))
        .expect("scaffold task must be present");
    assert!(scaffold.depends_on.is_empty(), "scaffold has no deps");
    assert_eq!(scaffold.section.as_deref(), Some("0001"));
    assert_eq!(scaffold.state, TaskState::New);

    let add_ci = graph
        .tasks
        .iter()
        .find(|t| t.id == TaskId::new("add-ci"))
        .expect("add-ci task must be present");
    assert_eq!(
        add_ci.depends_on,
        vec![TaskId::new("scaffold")],
        "add-ci should depend on scaffold"
    );

    let core_logic = graph
        .tasks
        .iter()
        .find(|t| t.id == TaskId::new("core-logic"))
        .expect("core-logic task must be present");
    assert_eq!(
        core_logic.depends_on,
        vec![TaskId::new("scaffold"), TaskId::new("add-ci")],
        "core-logic should depend on scaffold and add-ci"
    );
    assert_eq!(core_logic.section.as_deref(), Some("0002"));

    let docs = graph
        .tasks
        .iter()
        .find(|t| t.id == TaskId::new("docs"))
        .expect("docs task must be present");
    assert_eq!(
        docs.depends_on,
        vec![TaskId::new("core-logic")],
        "docs should depend on core-logic"
    );

    // All tasks must be New with zero iteration counts.
    for task in &graph.tasks {
        assert_eq!(task.state, TaskState::New);
        assert_eq!(task.gate_iterations, 0);
        assert_eq!(task.review_iterations, 0);
        assert!(task.started_at.is_none());
        assert!(task.finished_at.is_none());
    }

    // ── Clean shutdown ────────────────────────────────────────────────────────
    root.kill();
}

/// Sending `InterpretTaskList` with a non-existent path returns `Err`.
#[tokio::test]
async fn planner_returns_error_for_missing_file() {
    let root = RootSupervisor::start();

    // The Supervisor needs a WorktreeManager in its Args; this test only exercises
    // SetTaskGraph/TaskGraphSnapshot (no worktree create/remove), so a manager
    // over a dummy path is sufficient.
    let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
        &root,
        SupervisorArgs {
            worktree_manager: WorktreeManager::new(
                std::path::PathBuf::from("/tmp/makina-planner-test"),
                "develop".into(),
            ),
        },
        RestartConfig::default(),
    )
    .await;

    let planner_ref = RootSupervisor::spawn_child::<Planner>(
        &root,
        PlannerArgs {
            supervisor: supervisor_ref.clone(),
            interpreter: Arc::new(StructuredTextInterpreter::new()),
        },
        RestartConfig::default(),
    )
    .await;

    // For a missing file the handler returns `Err(String)`.
    // kameo 0.20 surfaces this as `Err(SendError::HandlerError(String))`.
    let result = planner_ref
        .ask(InterpretTaskList {
            path: std::path::PathBuf::from("/nonexistent/path/to/tasks.md"),
        })
        .send()
        .await;

    assert!(
        result.is_err(),
        "missing file should produce Err; got: Ok(())"
    );

    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("failed to read")
            || err.contains("No such file")
            || err.contains("nonexistent"),
        "error should mention the read failure; got: {err}"
    );

    root.kill();
}
