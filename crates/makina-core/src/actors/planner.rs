//! Planner turn — interprets a task-list file into a [`TaskGraph`].
//!
//! The planner is now a direct async helper rather than an actor spoke. It reads
//! a structured task-list document, derives the graph slug from the file stem,
//! and delegates parsing to an injected [`TaskListInterpreter`].
//!
//! [`TaskGraph`]: crate::task::TaskGraph

use std::path::PathBuf;
use std::sync::Arc;

use crate::interpreter::TaskListInterpreter;
use crate::task::TaskGraph;

/// Instruct the planner to interpret the task-list document at `path`.
pub struct InterpretTaskList {
    /// Path to the task-list document (for example, a `.tasks/` Markdown file).
    pub path: PathBuf,
}

/// Reply returned by [`interpret_task_list`].
pub type InterpretTaskListAck = Result<TaskGraph, String>;

/// Interpret one task-list file with the supplied interpreter.
pub async fn interpret_task_list(
    interpreter: Arc<dyn TaskListInterpreter>,
    msg: InterpretTaskList,
) -> InterpretTaskListAck {
    let slug = msg
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("tasks")
        .to_string();

    let text = tokio::fs::read_to_string(&msg.path)
        .await
        .map_err(|e| format!("failed to read `{}`: {e}", msg.path.display()))?;

    interpreter
        .interpret(&slug, &text)
        .await
        .map_err(|e| format!("interpretation failed: {e}"))
}
