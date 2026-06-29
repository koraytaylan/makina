//! Integration tests for the direct planner turn.

use std::io::Write as _;
use std::sync::Arc;

use makina_core::actors::{InterpretTaskList, interpret_task_list};
use makina_core::interpreter::StructuredTextInterpreter;
use tempfile::NamedTempFile;

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

#[tokio::test]
async fn planner_interprets_file_into_task_graph() {
    let mut tmp = NamedTempFile::with_suffix(".md").expect("should create temp file");
    tmp.write_all(SAMPLE_TASK_LIST.as_bytes())
        .expect("should write sample task list");

    let graph = interpret_task_list(
        Arc::new(StructuredTextInterpreter::new()),
        InterpretTaskList {
            path: tmp.path().to_path_buf(),
        },
    )
    .await
    .expect("InterpretTaskList should succeed");

    assert_eq!(
        graph.tasks.len(),
        4,
        "graph should contain exactly 4 tasks; got {}",
        graph.tasks.len()
    );
    graph
        .validate()
        .expect("delivered graph must pass TaskGraph::validate()");

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
    assert_eq!(add_ci.depends_on, vec![TaskId::new("scaffold")]);

    let core_logic = graph
        .tasks
        .iter()
        .find(|t| t.id == TaskId::new("core-logic"))
        .expect("core-logic task must be present");
    assert_eq!(
        core_logic.depends_on,
        vec![TaskId::new("scaffold"), TaskId::new("add-ci")]
    );
    assert_eq!(core_logic.section.as_deref(), Some("0002"));

    let docs = graph
        .tasks
        .iter()
        .find(|t| t.id == TaskId::new("docs"))
        .expect("docs task must be present");
    assert_eq!(docs.depends_on, vec![TaskId::new("core-logic")]);

    for task in &graph.tasks {
        assert_eq!(task.state, TaskState::New);
        assert_eq!(task.gate_iterations, 0);
        assert_eq!(task.review_iterations, 0);
        assert!(task.started_at.is_none());
        assert!(task.finished_at.is_none());
    }
}

#[tokio::test]
async fn planner_returns_error_for_missing_file() {
    let result = interpret_task_list(
        Arc::new(StructuredTextInterpreter::new()),
        InterpretTaskList {
            path: std::path::PathBuf::from("/nonexistent/path/to/tasks.md"),
        },
    )
    .await;

    let err = result.expect_err("missing file should produce Err");
    assert!(
        err.contains("failed to read")
            || err.contains("No such file")
            || err.contains("nonexistent"),
        "error should mention the read failure; got: {err}"
    );
}
