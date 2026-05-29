//! `#[ignore]`d integration test: `ModelInterpreter` against a real ACP CLI.
//!
//! This is the acceptance criterion for task 18 (`planner-model-mechanism`):
//! **"the Planner makes a real model call"**.  It uses `AcpBackend` (which wraps
//! a real ACP CLI subprocess) to back a `ModelInterpreter` and asserts that a
//! small sample task list is interpreted into a valid `TaskGraph`.
//!
//! The test is `#[ignore]`d so CI without a CLI or without a signed-in agent
//! stays green.  Deterministic proof of `ModelInterpreter` lives in
//! `makina-core`'s unit tests (using `NoopBackend`).
//!
//! # Running this test
//!
//! On a machine where the ACP CLI is installed and authenticated:
//!
//! ```bash
//! # Google Gemini CLI (must be signed in via `gemini auth login`):
//! MAKINA_ACP_CMD=gemini MAKINA_ACP_ARGS=--acp \
//!     cargo test -p makina-acp --test model_interpreter_real -- --ignored --nocapture
//!
//! # Zed's Claude Code ACP adapter via npx:
//! MAKINA_ACP_CMD=npx \
//! MAKINA_ACP_ARGS='-y,@zed-industries/claude-code-acp@latest' \
//!     cargo test -p makina-acp --test model_interpreter_real -- --ignored --nocapture
//! ```
//!
//! `MAKINA_ACP_CMD` is the program; `MAKINA_ACP_ARGS` is a comma-separated arg
//! list.  Auth path: the CLI must be pre-authenticated (Zed model — Makina
//! inherits the parent environment and holds no credentials).
//!
//! # What is asserted
//!
//! - `interpret` returns `Ok(graph)` — no transport, parse, or validation error.
//! - `graph.slug` equals the supplied slug.
//! - `graph.tasks` is non-empty.
//! - `graph.validate()` passes — unique ids, no dangling edges.
//! - Each task has `state == "new"` and zero iteration counts.
//!
//! No byte-exact response content is asserted; real model output is
//! non-deterministic.

use std::sync::Arc;

use makina_acp::AcpBackend;
use makina_core::interpreter::{ModelInterpreter, TaskListInterpreter};
use makina_core::task::TaskState;

/// A small, self-contained task list for testing.  Deliberately tiny so the
/// model turn is cheap and fast.  Two tasks, one dependency edge.
const SAMPLE_TASK_LIST: &str = r#"# Sample Project — Task List

A minimal task list for testing the model-backed interpreter.

**Conventions**
- ids are kebab-case.

---

## 0001 — Bootstrap

### scaffold-repo — Scaffold the repository
Create the repository structure with initial configuration files.
- **Depends on:** —
- **Done when:** `git status` shows the repository is clean with the initial commit.

### add-readme — Add README
Write a brief README describing the project.
- **Depends on:** scaffold-repo
- **Done when:** `README.md` exists and describes the project purpose.
"#;

/// Helper: read agent program + args from environment variables.
fn cli_program_and_args() -> (String, Vec<String>) {
    let program = std::env::var("MAKINA_ACP_CMD").unwrap_or_else(|_| {
        panic!(
            "set MAKINA_ACP_CMD to the agent program (e.g. `gemini`); \
             see module-level documentation for the full command."
        )
    });
    let args: Vec<String> = std::env::var("MAKINA_ACP_ARGS")
        .map(|s| s.split(',').map(|a| a.trim().to_string()).collect())
        .unwrap_or_default();
    (program, args)
}

/// **Real-model-call acceptance test** for `ModelInterpreter`.
///
/// Builds a `ModelInterpreter` backed by `AcpBackend`, interprets a small
/// sample task list, and asserts the resulting `TaskGraph` is structurally
/// valid.
#[tokio::test]
#[ignore = "requires a real, authenticated ACP CLI; run manually with MAKINA_ACP_CMD set"]
async fn model_interpreter_real_model_call() {
    let (program, args) = cli_program_and_args();
    eprintln!("model_interpreter_real_model_call: using ACP CLI `{program}` args={args:?}");

    let cwd = std::env::current_dir().unwrap();
    let backend = Arc::new(AcpBackend::new(program, args));
    let interpreter = ModelInterpreter::new(backend as Arc<dyn makina_core::backend::AgentBackend>)
        .with_working_dir(cwd);

    eprintln!("Calling interpret() — this makes a real model call ...");
    let result = interpreter
        .interpret("sample-project", SAMPLE_TASK_LIST)
        .await;

    match &result {
        Ok(graph) => {
            eprintln!(
                "interpret() succeeded: slug={:?}, tasks={}",
                graph.slug,
                graph.tasks.len()
            );
            for task in &graph.tasks {
                eprintln!(
                    "  task {:?}: state={:?}, depends_on={:?}",
                    task.id.0, task.state, task.depends_on
                );
            }
        }
        Err(e) => eprintln!("interpret() failed: {e}"),
    }

    let graph = result.expect("ModelInterpreter::interpret must succeed with a real model");

    // Slug is preserved.
    assert_eq!(graph.slug, "sample-project", "slug must match the input");

    // At least one task was returned.
    assert!(
        !graph.tasks.is_empty(),
        "interpret must return at least one task"
    );

    // The graph must pass structural validation (unique ids, no dangling edges).
    graph
        .validate()
        .expect("the model-generated TaskGraph must pass validate()");

    // All tasks start as `new` with zero iteration counts.
    for task in &graph.tasks {
        assert_eq!(
            task.state,
            TaskState::New,
            "task {:?} must start in state `new`",
            task.id.0
        );
        assert_eq!(
            task.gate_iterations, 0,
            "task {:?} must have gate_iterations == 0",
            task.id.0
        );
        assert_eq!(
            task.review_iterations, 0,
            "task {:?} must have review_iterations == 0",
            task.id.0
        );
    }
}
