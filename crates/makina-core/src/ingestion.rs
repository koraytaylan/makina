//! Ingestion issue and report types.
//!
//! Captures structured issues (blocking or warning) produced during task-graph
//! ingestion by the interpreter, validator, or qualifier passes.  The
//! [`IngestionReport`] is the single result object returned to callers (e.g.
//! `RunView`) so they can decide whether to proceed or surface problems.

use serde::{Deserialize, Serialize};

use crate::dependency::transitive_depends_on;
use crate::task::{TaskGraph, TaskId};

// ── Supporting enums ──────────────────────────────────────────────────────────

/// Severity of an ingestion issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    /// The ingestion cannot proceed; the run is blocked.
    Blocking,
    /// Non-fatal observation; ingestion may continue.
    Warning,
}

/// Source / stage that produced the issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSource {
    /// Produced by the interpreter (parse / model output).
    Interpreter,
    /// Produced by structural validation of the task graph.
    Validator,
    /// Produced by a qualifier / policy pass.
    Qualifier,
}

// ── Issue and Report ──────────────────────────────────────────────────────────

/// A single ingestion issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestionIssue {
    /// Task the issue relates to, when known.
    pub task_id: Option<TaskId>,
    /// Severity (blocking vs. warning).
    pub severity: IssueSeverity,
    /// Which stage produced the issue.
    pub source: IssueSource,
    /// Stable machine-readable code (e.g. "duplicate-task-id").
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Optional suggestion for remediation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Aggregate report of all issues found during ingestion of a task graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestionReport {
    /// All issues, in the order they were discovered.
    pub issues: Vec<IngestionIssue>,
}

impl IngestionReport {
    /// Returns true if the report contains any blocking issue.
    pub fn is_blocked(&self) -> bool {
        self.issues
            .iter()
            .any(|i| i.severity == IssueSeverity::Blocking)
    }

    /// Iterator over only the blocking issues (in discovery order).
    pub fn blocking(&self) -> impl Iterator<Item = &IngestionIssue> {
        self.issues
            .iter()
            .filter(|i| i.severity == IssueSeverity::Blocking)
    }

    /// Iterator over only the warning issues (in discovery order).
    pub fn warnings(&self) -> impl Iterator<Item = &IngestionIssue> {
        self.issues
            .iter()
            .filter(|i| i.severity == IssueSeverity::Warning)
    }

    /// True when the report contains zero issues.
    pub fn is_empty(&self) -> bool {
        self.issues.is_empty()
    }
}

impl Default for IngestionReport {
    /// Returns an empty report (no issues).  Used by `RunView` literals.
    fn default() -> Self {
        IngestionReport { issues: Vec::new() }
    }
}

/// Structural validator that collects *all* issues in the graph (non-fail-fast).
///
/// Returns every structural defect as an [`IngestionIssue`] with
/// `source = Validator` and `severity = Blocking`.  Unlike
/// [`crate::task::TaskGraph::validate`], this scans the entire graph and
/// reports duplicates, dangling references, empty `done_when`, self-deps,
/// and cycles (using the reachability primitive from [`crate::dependency`]).
///
/// This is additive: interpreters still rely on `TaskGraph::validate` to
/// construct a graph; this validator is used later to build an
/// [`IngestionReport`] for the review gate.
pub fn validate(graph: &TaskGraph) -> Vec<IngestionIssue> {
    let mut issues: Vec<IngestionIssue> = Vec::new();

    // 1. Duplicate task IDs (emit once per duplicated id, in discovery order).
    let mut seen: std::collections::HashSet<TaskId> = std::collections::HashSet::new();
    let mut reported_dups: std::collections::HashSet<TaskId> = std::collections::HashSet::new();
    for task in &graph.tasks {
        if !seen.insert(task.id.clone()) && reported_dups.insert(task.id.clone()) {
            issues.push(IngestionIssue {
                task_id: Some(task.id.clone()),
                severity: IssueSeverity::Blocking,
                source: IssueSource::Validator,
                code: "duplicate-task-id".to_string(),
                message: format!("duplicate task id: {}", task.id),
                suggestion: Some("ensure every task has a unique id".to_string()),
            });
        }
    }

    // 2–4. Per-task checks (empty done_when, self-dependency, dangling).
    for task in &graph.tasks {
        // empty done_when (whitespace-only counts as empty).
        if task.done_when.trim().is_empty() {
            issues.push(IngestionIssue {
                task_id: Some(task.id.clone()),
                severity: IssueSeverity::Blocking,
                source: IssueSource::Validator,
                code: "empty-done-when".to_string(),
                message: format!("task `{}` has empty `done_when`", task.id),
                suggestion: Some(
                    "provide a concrete acceptance criterion in `done_when`".to_string(),
                ),
            });
        }

        // self-dependency (lists own id).
        if task.depends_on.iter().any(|d| d == &task.id) {
            issues.push(IngestionIssue {
                task_id: Some(task.id.clone()),
                severity: IssueSeverity::Blocking,
                source: IssueSource::Validator,
                code: "self-dependency".to_string(),
                message: format!("task `{}` depends on itself", task.id),
                suggestion: Some("remove the self-reference from `depends_on`".to_string()),
            });
        }

        // dangling dependencies (reference names no task in graph).
        for dep in &task.depends_on {
            if graph.get(dep).is_none() {
                issues.push(IngestionIssue {
                    task_id: Some(task.id.clone()),
                    severity: IssueSeverity::Blocking,
                    source: IssueSource::Validator,
                    code: "dangling-dependency".to_string(),
                    message: format!("task `{}` depends on unknown task `{}`", task.id, dep),
                    suggestion: Some(
                        "remove the reference or add the missing task to the graph".to_string(),
                    ),
                });
            }
        }
    }

    // 5. Cycle detection reuses the existing reachability primitive.
    // A cycle exists if any task transitively depends on itself.
    let has_cycle = graph
        .tasks
        .iter()
        .any(|t| transitive_depends_on(graph, &t.id, &t.id));
    if has_cycle {
        issues.push(IngestionIssue {
            task_id: None,
            severity: IssueSeverity::Blocking,
            source: IssueSource::Validator,
            code: "dependency-cycle".to_string(),
            message: "the task graph contains a dependency cycle".to_string(),
            suggestion: Some(
                "break the cycle by removing one of the edges in the loop".to_string(),
            ),
        });
    }

    issues
}

// ── Tests (per task spec) ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Task, TaskGraph, TaskState};

    #[test]
    fn is_blocked_is_true_iff_a_blocking_issue_present() {
        // empty report → false
        let empty = IngestionReport::default();
        assert!(!empty.is_blocked());

        // one Warning → false
        let warning_only = IngestionReport {
            issues: vec![IngestionIssue {
                task_id: None,
                severity: IssueSeverity::Warning,
                source: IssueSource::Interpreter,
                code: "example-warning".to_string(),
                message: "just a note".to_string(),
                suggestion: None,
            }],
        };
        assert!(!warning_only.is_blocked());

        // one Blocking → true
        let blocking_only = IngestionReport {
            issues: vec![IngestionIssue {
                task_id: None,
                severity: IssueSeverity::Blocking,
                source: IssueSource::Validator,
                code: "duplicate-id".to_string(),
                message: "task id already exists".to_string(),
                suggestion: Some("rename the task".to_string()),
            }],
        };
        assert!(blocking_only.is_blocked());
    }

    #[test]
    fn blocking_and_warnings_partition_issues() {
        let blocking = IngestionIssue {
            task_id: Some(TaskId::new("t1")),
            severity: IssueSeverity::Blocking,
            source: IssueSource::Qualifier,
            code: "blocked".to_string(),
            message: "blocking".to_string(),
            suggestion: None,
        };
        let warning = IngestionIssue {
            task_id: Some(TaskId::new("t2")),
            severity: IssueSeverity::Warning,
            source: IssueSource::Interpreter,
            code: "warn".to_string(),
            message: "warning".to_string(),
            suggestion: None,
        };

        let report = IngestionReport {
            issues: vec![blocking, warning],
        };

        assert_eq!(report.blocking().count(), 1);
        assert_eq!(report.warnings().count(), 1);
    }

    // ── validate tests (per task spec) ────────────────────────────────────────

    use chrono::Utc;

    /// Helper matching the fixture style used in `dependency.rs` tests.
    fn make_task(id: &str, done_when: &str, depends_on: Vec<&str>) -> Task {
        let now = Utc::now();
        Task {
            id: TaskId::new(id),
            title: id.to_string(),
            description: format!("Task {}.", id),
            done_when: done_when.to_string(),
            depends_on: depends_on.into_iter().map(TaskId::new).collect(),
            section: None,
            state: TaskState::New,
            gate_iterations: 0,
            review_iterations: 0,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        }
    }

    fn make_graph(tasks: Vec<Task>) -> TaskGraph {
        TaskGraph {
            slug: "test".to_string(),
            tasks,
        }
    }

    #[test]
    fn validate_clean_graph_has_no_issues() {
        let t1 = make_task("task-a", "A is complete.", vec![]);
        let t2 = make_task("task-b", "B is complete.", vec!["task-a"]);
        let g = make_graph(vec![t1, t2]);
        let issues = validate(&g);
        assert!(
            issues.is_empty(),
            "well-formed 2-task graph must produce no issues"
        );
    }

    #[test]
    fn validate_flags_dangling_dependency() {
        let t = make_task("orphan", "done.", vec!["ghost-task"]);
        let g = make_graph(vec![t]);
        let issues = validate(&g);
        assert!(
            issues.iter().any(
                |i| i.code == "dangling-dependency" && i.task_id == Some(TaskId::new("orphan"))
            ),
            "must flag dangling-dependency with task_id=Some(orphan); got: {:?}",
            issues
        );
    }

    #[test]
    fn validate_flags_duplicate_task_id() {
        let t = make_task("dupe", "done.", vec![]);
        let g = make_graph(vec![t.clone(), t]);
        let issues = validate(&g);
        let dup_issues: Vec<_> = issues
            .iter()
            .filter(|i| i.code == "duplicate-task-id")
            .collect();
        assert_eq!(dup_issues.len(), 1, "exactly one issue per duplicate id");
        assert_eq!(dup_issues[0].task_id, Some(TaskId::new("dupe")));
    }

    #[test]
    fn validate_flags_empty_done_when() {
        let t = make_task("empty", "   ", vec![]);
        let g = make_graph(vec![t]);
        let issues = validate(&g);
        assert!(
            issues
                .iter()
                .any(|i| i.code == "empty-done-when" && i.task_id == Some(TaskId::new("empty"))),
            "must flag empty-done-when; got: {:?}",
            issues
        );
    }

    #[test]
    fn validate_flags_self_dependency() {
        let t = make_task("loop", "done.", vec!["loop"]);
        let g = make_graph(vec![t]);
        let issues = validate(&g);
        assert!(
            issues
                .iter()
                .any(|i| i.code == "self-dependency" && i.task_id == Some(TaskId::new("loop"))),
            "must flag self-dependency with task_id=Some(loop); got: {:?}",
            issues
        );
    }

    #[test]
    fn validate_flags_dependency_cycle() {
        let a = make_task("a", "done.", vec!["b"]);
        let b = make_task("b", "done.", vec!["a"]);
        let g = make_graph(vec![a, b]);
        let issues = validate(&g);
        assert!(
            issues
                .iter()
                .any(|i| i.code == "dependency-cycle" && i.task_id.is_none()),
            "must flag dependency-cycle at graph level (task_id=None); got: {:?}",
            issues
        );
    }
}
