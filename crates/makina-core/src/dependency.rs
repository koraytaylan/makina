//! Cross-cutting dependency inference for task graphs.
//!
//! # Overview
//!
//! [`EdgeInferrer`] is a decorator over any [`TaskListInterpreter`] that adds
//! inferred dependency edges to the resulting [`TaskGraph`].  It calls the inner
//! interpreter, then post-processes the graph with [`infer_edges`] to serialize
//! tasks that touch the same files or areas.
//!
//! This is Makina's primary defence against same-file merge conflicts: overlapping
//! tasks are serialized at planning time so their branches never collide.
//!
//! # Area-extraction heuristic
//!
//! The area of a task is the set of **backtick-delimited code spans** found in
//! its text (`title` + `description` + `done_when`).  The structured-text
//! convention already wraps crate names, file names, and identifiers in
//! backticks (e.g. `` `makina-core` ``, `` `backend.rs` ``, `` `AgentBackend` ``),
//! so these spans are a reliable, zero-cost proxy for "what does this task touch".
//!
//! Spans are normalized (trimmed + lowercased) before comparison.  A task with no
//! backtick spans has an empty area set and will never be linked by inference
//! (explicit `Depends on` edges are unaffected).
//!
//! **Conservative bias** — over-serializing (an extra edge) is safe because it
//! only delays a task; under-serializing risks a merge conflict when two tasks
//! modify the same file simultaneously.  The heuristic therefore errs toward
//! adding an edge whenever areas overlap.
//!
//! # Edge-inference rule
//!
//! For every pair of tasks (A, B) that share at least one area:
//! - If A appears before B in `tasks` order, add B → A (B depends on A).
//! - Edges are only added, never removed.  All explicit `Depends on` edges are
//!   preserved.
//! - Duplicate edges are suppressed (no edge is added if one already exists).
//! - Self-edges are never added.
//!
//! Because edges always run from a later task to an earlier one (in authored
//! order), the resulting graph is acyclic by construction: any cycle would
//! require an edge from an earlier task to a later one, which this rule never
//! produces.
//!
//! # Future work
//!
//! The backtick heuristic is the **deterministic baseline** (no model, no NLP).
//! Task 18 (`planner-model-mechanism`) may compose a model-backed interpreter
//! *before* this decorator, improving area extraction precision while keeping
//! the serialization rule here unchanged.  Alternatively, a future task may
//! replace the heuristic inside `infer_edges` with a model call that returns
//! per-task file lists — the [`EdgeInferrer`] wrapper and the
//! [`TaskListInterpreter`] seam remain stable either way.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use crate::interpreter::{InterpretError, TaskListInterpreter};
use crate::task::{TaskGraph, TaskId};

// ── EdgeInferrer ──────────────────────────────────────────────────────────────

/// A decorator over [`TaskListInterpreter`] that adds cross-cutting dependency
/// edges to the returned [`TaskGraph`].
///
/// Construct with [`EdgeInferrer::new`], passing any existing interpreter.
/// The resulting `EdgeInferrer` satisfies `TaskListInterpreter` itself, so it
/// can be injected wherever an interpreter is expected (e.g. [`PlannerArgs`]).
///
/// # Example
///
/// ```rust,ignore
/// use std::sync::Arc;
/// use makina_core::interpreter::StructuredTextInterpreter;
/// use makina_core::dependency::EdgeInferrer;
///
/// let interpreter = EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new()));
/// // `interpreter` now wraps the deterministic parser with inferred edges.
/// ```
///
/// [`PlannerArgs`]: crate::actors::planner::PlannerArgs
pub struct EdgeInferrer {
    inner: Arc<dyn TaskListInterpreter>,
}

impl EdgeInferrer {
    /// Wrap an existing interpreter with cross-cutting dependency inference.
    pub fn new(inner: Arc<dyn TaskListInterpreter>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl TaskListInterpreter for EdgeInferrer {
    /// Interpret the task list via the inner interpreter, then add inferred
    /// edges for tasks that share areas (backtick spans), then re-validate.
    async fn interpret(&self, slug: &str, source_text: &str) -> Result<TaskGraph, InterpretError> {
        let mut graph = self.inner.interpret(slug, source_text).await?;
        infer_edges(&mut graph);
        // Validate that the augmented graph is still structurally sound.
        // (The acyclicity guarantee means this should always pass, but we run it
        // defensively to catch any bug in the inference logic early.)
        graph.validate()?;
        Ok(graph)
    }
}

// ── Area extraction ───────────────────────────────────────────────────────────

/// Extract the set of areas from a block of text.
///
/// An "area" is any backtick-delimited span found in `text`, normalized to a
/// trimmed, lowercase string.  Spans that are empty after trimming are dropped.
///
/// # Example
///
/// ```ignore
/// let areas = extract_areas("Add `AgentBackend` to `makina-core`.");
/// // → {"agentbackend", "makina-core"}
/// ```
fn extract_areas(text: &str) -> HashSet<String> {
    let mut areas = HashSet::new();
    let mut chars = text.char_indices().peekable();

    while let Some((i, ch)) = chars.next() {
        if ch == '`' {
            // Consume until the closing backtick or end-of-string.
            let start = i + 1; // byte offset after the opening backtick
            let mut end = start;
            for (j, c) in chars.by_ref() {
                if c == '`' {
                    end = j;
                    break;
                }
                end = j + c.len_utf8();
            }
            let span = text[start..end].trim().to_lowercase();
            if !span.is_empty() {
                areas.insert(span);
            }
        }
    }

    areas
}

/// Combine all text fields of a task into one string for area extraction.
fn task_text(task: &crate::task::Task) -> String {
    format!("{} {} {}", task.title, task.description, task.done_when)
}

// ── Edge inference ────────────────────────────────────────────────────────────

/// Add inferred dependency edges to `graph` for tasks that share at least one
/// area (backtick span).
///
/// See the [module documentation](self) for the full rule and conservative-bias
/// rationale.
///
/// # Acyclicity guarantee
///
/// Edges are added in the form "later task depends on earlier task" (where
/// "earlier" and "later" refer to position in `graph.tasks`).  This total order
/// means a cycle is impossible: any cycle would require an edge pointing from an
/// earlier index to a later one, which this function never produces.
pub fn infer_edges(graph: &mut TaskGraph) {
    let n = graph.tasks.len();
    if n < 2 {
        return;
    }

    // Compute areas for every task up front (index → set of area strings).
    let areas: Vec<HashSet<String>> = graph
        .tasks
        .iter()
        .map(|t| extract_areas(&task_text(t)))
        .collect();

    // Build a quick lookup: TaskId → index in `graph.tasks`.
    let index_of: HashMap<&TaskId, usize> = graph
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| (&t.id, i))
        .collect();

    // For each pair (i < j), if they share an area, add j → i (j depends on i).
    // We collect additions here to avoid borrowing `graph.tasks` mutably while
    // we read it.
    let mut additions: Vec<(usize, TaskId)> = Vec::new(); // (task_index, dep_to_add)

    for i in 0..n {
        for j in (i + 1)..n {
            // Skip if they share no areas.
            if areas[i].is_disjoint(&areas[j]) {
                continue;
            }

            let earlier_id = graph.tasks[i].id.clone();

            // Skip if the edge j → i already exists (explicit or previously inferred).
            let already_exists = graph.tasks[j]
                .depends_on
                .iter()
                .any(|dep| dep == &earlier_id);

            if !already_exists {
                // Also verify the "earlier" task is indeed in the graph (it is, since
                // we built `index_of` from the same `tasks` slice, but be defensive).
                if index_of.contains_key(&earlier_id) {
                    additions.push((j, earlier_id));
                }
            }
        }
    }

    // Apply the collected additions.
    for (task_idx, dep_id) in additions {
        graph.tasks[task_idx].depends_on.push(dep_id);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::StructuredTextInterpreter;
    use std::sync::Arc;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Minimal well-formed structured-text task list with `n` tasks under one
    /// section, given a closure that produces each task's body.
    ///
    /// `tasks` is a slice of `(id, title, description, done_when)`.
    fn build_source(tasks: &[(&str, &str, &str, &str)]) -> String {
        let mut src =
            String::from("# Test Project — Task List\n\nPreamble.\n\n---\n\n## 0001 — Section\n\n");
        let mut first = true;
        for (id, title, description, done_when) in tasks {
            if !first {
                src.push('\n');
            }
            first = false;
            src.push_str(&format!("### {id} — {title}\n"));
            src.push_str(description);
            src.push('\n');
            src.push_str("- **Depends on:** —\n");
            src.push_str(&format!("- **Done when:** {done_when}\n"));
        }
        src
    }

    /// Same as `build_source` but allows a custom `depends_on` line per task.
    fn build_source_with_deps(tasks: &[(&str, &str, &str, &str, &str)]) -> String {
        let mut src =
            String::from("# Test Project — Task List\n\nPreamble.\n\n---\n\n## 0001 — Section\n\n");
        let mut first = true;
        for (id, title, description, done_when, deps) in tasks {
            if !first {
                src.push('\n');
            }
            first = false;
            src.push_str(&format!("### {id} — {title}\n"));
            src.push_str(description);
            src.push('\n');
            src.push_str(&format!("- **Depends on:** {deps}\n"));
            src.push_str(&format!("- **Done when:** {done_when}\n"));
        }
        src
    }

    // ── Acceptance test: overlapping areas produce a linking edge ─────────────

    /// Two tasks that both mention the same backtick span (`` `makina-core` ``)
    /// must come out linked: the LATER task (task-b, index 1) must depend on
    /// the EARLIER task (task-a, index 0).
    #[tokio::test]
    async fn overlapping_areas_produce_inferred_edge() {
        let source = build_source(&[
            (
                "task-a",
                "First task",
                "Work on the `makina-core` crate and extend its public API.",
                "`makina-core` builds successfully.",
            ),
            (
                "task-b",
                "Second task",
                "Add tests to the `makina-core` crate for the new API.",
                "All `makina-core` tests pass.",
            ),
        ]);

        let interpreter = EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new()));
        let graph = interpreter
            .interpret("test", &source)
            .await
            .expect("interpretation must succeed");

        // task-b (index 1) must depend on task-a (index 0).
        let task_b = graph
            .tasks
            .iter()
            .find(|t| t.id.0 == "task-b")
            .expect("task-b must be in graph");

        assert!(
            task_b.depends_on.contains(&TaskId::new("task-a")),
            "task-b must depend on task-a (inferred via shared `makina-core` area); \
             actual depends_on: {:?}",
            task_b.depends_on
        );

        // Direction check: the EARLIER task (task-a) must NOT depend on task-b.
        let task_a = graph
            .tasks
            .iter()
            .find(|t| t.id.0 == "task-a")
            .expect("task-a must be in graph");
        assert!(
            !task_a.depends_on.contains(&TaskId::new("task-b")),
            "task-a must NOT depend on task-b (would create forward edge); \
             actual depends_on: {:?}",
            task_a.depends_on
        );
    }

    // ── No spurious edges for disjoint areas ──────────────────────────────────

    /// Tasks that share NO backtick areas must NOT be linked by inference.
    #[tokio::test]
    async fn no_shared_area_produces_no_inferred_edge() {
        let source = build_source(&[
            (
                "task-x",
                "Database task",
                "Set up the `postgres` database schema.",
                "The `postgres` migration runs without errors.",
            ),
            (
                "task-y",
                "Frontend task",
                "Build the `ui-components` library for the web app.",
                "All `ui-components` stories render without errors.",
            ),
        ]);

        let interpreter = EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new()));
        let graph = interpreter
            .interpret("test", &source)
            .await
            .expect("interpretation must succeed");

        // Neither task should depend on the other.
        let task_x = graph.tasks.iter().find(|t| t.id.0 == "task-x").unwrap();
        let task_y = graph.tasks.iter().find(|t| t.id.0 == "task-y").unwrap();

        assert!(
            task_x.depends_on.is_empty(),
            "task-x should have no inferred deps; got: {:?}",
            task_x.depends_on
        );
        assert!(
            task_y.depends_on.is_empty(),
            "task-y should have no inferred deps; got: {:?}",
            task_y.depends_on
        );
    }

    // ── Explicit edges are preserved and not duplicated ───────────────────────

    /// When two tasks share an area AND have an explicit `Depends on` edge,
    /// the explicit edge must be preserved and must NOT be duplicated.
    #[tokio::test]
    async fn explicit_edge_preserved_and_not_duplicated() {
        // task-b explicitly depends on task-a; they also share `backend.rs`.
        let source = build_source_with_deps(&[
            (
                "task-a",
                "First task",
                "Implement the `backend.rs` module.",
                "`backend.rs` compiles and tests pass.",
                "—",
            ),
            (
                "task-b",
                "Second task",
                "Refactor the `backend.rs` module.",
                "`backend.rs` refactor is complete.",
                "task-a", // explicit edge
            ),
        ]);

        let interpreter = EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new()));
        let graph = interpreter
            .interpret("test", &source)
            .await
            .expect("interpretation must succeed");

        let task_b = graph.tasks.iter().find(|t| t.id.0 == "task-b").unwrap();

        // Exactly one edge to task-a (the explicit one; inference must not add a second).
        let dep_count = task_b.depends_on.iter().filter(|d| d.0 == "task-a").count();
        assert_eq!(
            dep_count, 1,
            "task-b must depend on task-a exactly once (not duplicated); \
             actual depends_on: {:?}",
            task_b.depends_on
        );
    }

    // ── Validate passes and graph is acyclic for 3-task overlapping list ──────

    /// A 3-task list where all tasks share a common area should still produce
    /// a valid, acyclic graph: task-c → task-b → task-a (chained by inference).
    ///
    /// `TaskGraph::validate()` must pass, confirming all edges resolve and ids
    /// are unique.  Acyclicity is verified by checking there are no back-edges
    /// (i.e. no earlier-indexed task depends on a later-indexed task).
    #[tokio::test]
    async fn three_task_overlapping_graph_is_valid_and_acyclic() {
        let source = build_source(&[
            (
                "task-1",
                "First task",
                "Create the `lib.rs` entry point for the crate.",
                "`lib.rs` is present and the crate compiles.",
            ),
            (
                "task-2",
                "Second task",
                "Add error types in `lib.rs`.",
                "Error types in `lib.rs` have doc-tests that pass.",
            ),
            (
                "task-3",
                "Third task",
                "Expose the public API via `lib.rs`.",
                "All public items in `lib.rs` have rustdoc comments.",
            ),
        ]);

        let interpreter = EdgeInferrer::new(Arc::new(StructuredTextInterpreter::new()));
        let graph = interpreter
            .interpret("test", &source)
            .await
            .expect("interpretation must succeed");

        // validate() checks unique ids and all edges resolve.
        graph.validate().expect("graph must pass validate()");

        // Build a position index for acyclicity check.
        let pos: HashMap<&str, usize> = graph
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id.0.as_str(), i))
            .collect();

        // For every edge (A depends on B), B must have a LOWER index than A.
        for task in &graph.tasks {
            let task_idx = pos[task.id.0.as_str()];
            for dep in &task.depends_on {
                let dep_idx = pos[dep.0.as_str()];
                assert!(
                    dep_idx < task_idx,
                    "edge from `{}` (idx {}) to `{}` (idx {}) is a forward edge — acyclicity broken",
                    task.id,
                    task_idx,
                    dep,
                    dep_idx
                );
            }
        }

        // task-2 must depend on task-1 (shared `lib.rs`).
        let task_2 = graph.tasks.iter().find(|t| t.id.0 == "task-2").unwrap();
        assert!(
            task_2.depends_on.contains(&TaskId::new("task-1")),
            "task-2 must depend on task-1; got: {:?}",
            task_2.depends_on
        );

        // task-3 must depend on task-1 (shared `lib.rs`).
        let task_3 = graph.tasks.iter().find(|t| t.id.0 == "task-3").unwrap();
        assert!(
            task_3.depends_on.contains(&TaskId::new("task-1")),
            "task-3 must depend on task-1; got: {:?}",
            task_3.depends_on
        );
    }

    // ── Unit tests for extract_areas ──────────────────────────────────────────

    #[test]
    fn extract_areas_finds_backtick_spans() {
        let areas = extract_areas("Work on `makina-core` and `backend.rs` today.");
        assert!(areas.contains("makina-core"), "must find makina-core");
        assert!(areas.contains("backend.rs"), "must find backend.rs");
        assert_eq!(areas.len(), 2, "exactly two spans");
    }

    #[test]
    fn extract_areas_normalizes_to_lowercase() {
        let areas = extract_areas("See `AgentBackend` and `TASKS.md`.");
        assert!(areas.contains("agentbackend"), "must be lowercased");
        assert!(areas.contains("tasks.md"), "must be lowercased");
    }

    #[test]
    fn extract_areas_empty_spans_are_dropped() {
        let areas = extract_areas("Look at `` (empty) and `real-thing`.");
        assert!(!areas.contains(""), "empty span must be dropped");
        assert!(areas.contains("real-thing"));
    }

    #[test]
    fn extract_areas_no_backticks_returns_empty() {
        let areas = extract_areas("No code spans at all in this sentence.");
        assert!(areas.is_empty());
    }

    // ── infer_edges unit test (without async/interpreter) ────────────────────

    /// Verify `infer_edges` directly on a hand-built `TaskGraph`.
    #[test]
    fn infer_edges_links_overlapping_tasks_and_skips_disjoint() {
        use crate::task::{Task, TaskState};
        use chrono::Utc;

        let now = Utc::now();
        let make_task = |id: &str, description: &str| Task {
            id: TaskId::new(id),
            title: id.to_string(),
            description: description.to_string(),
            done_when: "done.".to_string(),
            depends_on: vec![],
            section: None,
            state: TaskState::New,
            gate_iterations: 0,
            review_iterations: 0,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        };

        let mut graph = TaskGraph {
            slug: "test".to_string(),
            tasks: vec![
                make_task("alpha", "Modifies `shared.rs` heavily."),
                make_task("beta", "Also touches `shared.rs` for cleanup."),
                make_task("gamma", "Only touches `other.rs`, nothing shared."),
            ],
        };

        infer_edges(&mut graph);

        // beta (idx 1) must depend on alpha (idx 0) — shared `shared.rs`.
        let beta = graph.tasks.iter().find(|t| t.id.0 == "beta").unwrap();
        assert!(beta.depends_on.contains(&TaskId::new("alpha")));

        // gamma must have no inferred deps (no shared areas with alpha or beta).
        let gamma = graph.tasks.iter().find(|t| t.id.0 == "gamma").unwrap();
        assert!(gamma.depends_on.is_empty());

        // alpha must not depend on beta (no back-edge).
        let alpha = graph.tasks.iter().find(|t| t.id.0 == "alpha").unwrap();
        assert!(!alpha.depends_on.contains(&TaskId::new("beta")));

        graph.validate().expect("graph must still validate");
    }
}
