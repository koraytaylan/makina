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

/// Scans a raw structured-text task list document and reports **all** convention
/// violations (as blocking `Interpreter` issues) instead of failing fast like
/// `parse_structured_text`. This is a pure line scanner (no graph construction,
/// no I/O) used on the deterministic offline ingest path.
pub fn lint_source(source_text: &str) -> Vec<IngestionIssue> {
    const SEP: &str = " \u{2014} ";
    let mut issues: Vec<IngestionIssue> = Vec::new();
    let mut seen_section = false;
    // (heading_line, task_id, has_depends, has_done) for in-progress task block
    let mut current_task: Option<(usize, Option<String>, bool, bool)> = None;

    /// Emit the two possible missing-field issues for a just-ended task block.
    fn push_missing(
        issues: &mut Vec<IngestionIssue>,
        heading_line: usize,
        task_id: Option<&String>,
        has_dep: bool,
        has_don: bool,
    ) {
        if !has_dep {
            let msg = task_id.map_or_else(
                || {
                    format!(
                        "parse error at line {heading_line}: task is missing the `- **Depends on:**` field"
                    )
                },
                |id| {
                    format!(
                        "parse error at line {heading_line}: task `{id}` is missing the `- **Depends on:**` field"
                    )
                },
            );
            issues.push(IngestionIssue {
                task_id: None,
                severity: IssueSeverity::Blocking,
                source: IssueSource::Interpreter,
                code: "task-missing-depends-on".to_string(),
                message: msg,
                suggestion: None,
            });
        }
        if !has_don {
            let msg = task_id.map_or_else(
                || {
                    format!(
                        "parse error at line {heading_line}: task is missing the `- **Done when:**` field"
                    )
                },
                |id| {
                    format!(
                        "parse error at line {heading_line}: task `{id}` is missing the `- **Done when:**` field"
                    )
                },
            );
            issues.push(IngestionIssue {
                task_id: None,
                severity: IssueSeverity::Blocking,
                source: IssueSource::Interpreter,
                code: "task-missing-done-when".to_string(),
                message: msg,
                suggestion: None,
            });
        }
    }

    for (idx, raw_line) in source_text.lines().enumerate() {
        let line_no = idx + 1; // 1-based

        // ── Section heading: `## NNNN — Title` (or malformed) ─────────────────
        if let Some(rest) = raw_line.strip_prefix("## ") {
            if let Some((prev_line, prev_id, has_dep, has_don)) = current_task.take() {
                push_missing(&mut issues, prev_line, prev_id.as_ref(), has_dep, has_don);
            }
            if !rest.contains(SEP) {
                issues.push(IngestionIssue {
                    task_id: None,
                    severity: IssueSeverity::Blocking,
                    source: IssueSource::Interpreter,
                    code: "heading-missing-em-dash".to_string(),
                    message: format!(
                        "parse error at line {line_no}: section heading is missing the ` — ` separator (space + U+2014 + space)"
                    ),
                    suggestion: None,
                });
            }
            seen_section = true;
            continue;
        }

        // ── Task heading: `### {id} — {title}` (or malformed) ────────────────
        if let Some(rest) = raw_line.strip_prefix("### ") {
            let has_sep = rest.contains(SEP);
            if !seen_section {
                issues.push(IngestionIssue {
                    task_id: None,
                    severity: IssueSeverity::Blocking,
                    source: IssueSource::Interpreter,
                    code: "task-before-section".to_string(),
                    message: format!(
                        "parse error at line {line_no}: task heading found before any section heading"
                    ),
                    suggestion: None,
                });
            }
            if !has_sep {
                issues.push(IngestionIssue {
                    task_id: None,
                    severity: IssueSeverity::Blocking,
                    source: IssueSource::Interpreter,
                    code: "heading-missing-em-dash".to_string(),
                    message: format!(
                        "parse error at line {line_no}: task heading is missing the ` — ` separator (space + U+2014 + space)"
                    ),
                    suggestion: None,
                });
            }
            // Commit previous task block before (possibly) starting a new one.
            if let Some((prev_line, prev_id, has_dep, has_don)) = current_task.take() {
                push_missing(&mut issues, prev_line, prev_id.as_ref(), has_dep, has_don);
            }
            if has_sep {
                let task_id = rest.split_once(SEP).map(|(id, _)| id.trim().to_string());
                current_task = Some((line_no, task_id, false, false));
            } else {
                current_task = None;
            }
            continue;
        }

        // ── Field lines (only track presence; do not validate content) ───────
        if raw_line.strip_prefix("- **Depends on:** ").is_some() {
            if let Some((_, _, has_dep, _)) = &mut current_task {
                *has_dep = true;
            }
            continue;
        }
        if raw_line.strip_prefix("- **Done when:** ").is_some() {
            if let Some((_, _, _, has_don)) = &mut current_task {
                *has_don = true;
            }
            continue;
        }

        // ── Separator commits current task (mirrors parser) ──────────────────
        if raw_line == "---" {
            if let Some((prev_line, prev_id, has_dep, has_don)) = current_task.take() {
                push_missing(&mut issues, prev_line, prev_id.as_ref(), has_dep, has_don);
            }
            continue;
        }

        // All other lines (preamble, descriptions, blank, continuations) ignored by lint.
    }

    // EOF: commit any final in-progress task.
    if let Some((prev_line, prev_id, has_dep, has_don)) = current_task.take() {
        push_missing(&mut issues, prev_line, prev_id.as_ref(), has_dep, has_don);
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

    // ── lint_source tests (per task spec) ─────────────────────────────────────

    fn assert_lint_basics(issue: &IngestionIssue, expected_code: &str) {
        assert_eq!(
            issue.severity,
            IssueSeverity::Blocking,
            "lint issues are always Blocking"
        );
        assert_eq!(
            issue.source,
            IssueSource::Interpreter,
            "lint_source produces Interpreter issues"
        );
        assert_eq!(issue.task_id, None, "lint_source sets task_id=None");
        assert_eq!(issue.code, expected_code);
        assert!(
            issue.suggestion.is_none(),
            "lint_source leaves suggestion=None"
        );
        assert!(
            issue.message.contains("parse error at line"),
            "message must embed 1-based line: {}",
            issue.message
        );
    }

    #[test]
    fn lint_source_clean_document_has_no_issues() {
        // Exact worked example from docs/spec/structured-text-convention.md §7
        let source = r#"# Example Project — Build Task List

Structured-text task list for Example Project.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch
  `task/{id}` and worktree `.worktrees/{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- The Planner adds further dependency edges automatically.
- **Done when** is the acceptance check.

---

## 0001 — Foundation

### init-repo — Initialise the repository
Create the Git repository, add `.gitignore`, and push an initial commit.
- **Depends on:** —
- **Done when:** `git log` shows the initial commit and `.gitignore` is
  present.

### add-ci — Add CI pipeline
Add a GitHub Actions workflow that runs `cargo test` on every push.
- **Depends on:** init-repo
- **Done when:** a push to `main` triggers the CI workflow and it passes.

---

## 0002 — Core Library

### core-lib — Create core library crate
Scaffold the `core` crate with a public API module and passing unit
tests.
- **Depends on:** init-repo, add-ci
- **Done when:** `cargo test -p core` passes and the public API module
  is documented.
"#;
        let issues = lint_source(source);
        assert!(
            issues.is_empty(),
            "worked example from spec §7 must yield empty vec; got: {:?}",
            issues
        );
    }

    #[test]
    fn lint_source_detects_task_before_section() {
        // Task heading appears before any ## section heading.
        let source = r#"# T

Preamble text.

---

### too-early — Task before section
Description here.
- **Depends on:** —
- **Done when:** it is done.

## 0001 — First Section

### ok — Valid task
Ok description.
- **Depends on:** —
- **Done when:** ok.
"#;
        let issues = lint_source(source);
        let before: Vec<_> = issues
            .iter()
            .filter(|i| i.code == "task-before-section")
            .collect();
        assert_eq!(before.len(), 1, "exactly one task-before-section");
        assert_lint_basics(before[0], "task-before-section");
        // The offending ### is on line 7 in this fixture
        assert!(
            before[0].message.contains("7"),
            "message must mention line 7: {}",
            before[0].message
        );
    }

    #[test]
    fn lint_source_detects_heading_missing_em_dash() {
        let source = r#"# T

Preamble.

---

## 0001 Missing dash here
Some text after bad section.

### good — Good task
Desc.
- **Depends on:** —
- **Done when:** done.

### bad-task  Also missing dash
More desc.
- **Depends on:** —
- **Done when:** also done.
"#;
        let issues = lint_source(source);
        let dashless: Vec<_> = issues
            .iter()
            .filter(|i| i.code == "heading-missing-em-dash")
            .collect();
        assert_eq!(dashless.len(), 2, "two headings lack the em-dash separator");
        assert_lint_basics(dashless[0], "heading-missing-em-dash");
        assert_lint_basics(dashless[1], "heading-missing-em-dash");
        // Lines: ## on 5, first ### good has dash, second ### on ~13 lacks
        assert!(
            dashless.iter().any(|i| i.message.contains("5")),
            "one must be the ## on line 5"
        );
        assert!(
            dashless.iter().any(|i| i.message.contains("task heading")),
            "one must be a task heading"
        );
    }

    #[test]
    fn lint_source_detects_task_missing_depends_on() {
        let source = r#"# T

Preamble.

---

## 0001 — Section

### no-dep — Task without depends line
This task only has a Done when.
- **Done when:** the work completes successfully.
"#;
        let issues = lint_source(source);
        let missing: Vec<_> = issues
            .iter()
            .filter(|i| i.code == "task-missing-depends-on")
            .collect();
        assert_eq!(missing.len(), 1);
        assert_lint_basics(missing[0], "task-missing-depends-on");
        // Heading for the bad task is line 7
        assert!(
            missing[0].message.contains("9"),
            "message should reference the task heading line 9: {}",
            missing[0].message
        );
        assert!(
            missing[0].message.contains("no-dep"),
            "message should mention the task id when known"
        );
    }

    #[test]
    fn lint_source_detects_task_missing_done_when() {
        let source = r#"# T

Preamble.

---

## 0001 — Section

### no-done — Task without done line
Has depends but no done when before EOF.
- **Depends on:** —
"#;
        let issues = lint_source(source);
        let missing: Vec<_> = issues
            .iter()
            .filter(|i| i.code == "task-missing-done-when")
            .collect();
        assert_eq!(missing.len(), 1);
        assert_lint_basics(missing[0], "task-missing-done-when");
        assert!(
            missing[0].message.contains("9"),
            "message should reference heading line: {}",
            missing[0].message
        );
        assert!(
            missing[0].message.contains("no-done"),
            "message should mention task id"
        );
    }

    #[test]
    fn lint_source_reports_all_problems_at_once() {
        // Fixture with one dash-less heading + one task missing Done when.
        // Must report BOTH (does not stop at first error).
        let source = r#"# T

Preamble.

---

## 0001  Dashless section heading

### good — Has both fields
Description.
- **Depends on:** —
- **Done when:** completes.

### incomplete — Missing done when
Desc.
- **Depends on:** —
"#; // EOF ends the last task without Done when
        let issues = lint_source(source);
        assert_eq!(
            issues.len(),
            2,
            "must collect both problems, not stop at first; got: {:?}",
            issues
        );
        assert!(
            issues.iter().any(|i| i.code == "heading-missing-em-dash"),
            "must include heading-missing-em-dash"
        );
        assert!(
            issues.iter().any(|i| i.code == "task-missing-done-when"),
            "must include task-missing-done-when"
        );
        for i in &issues {
            assert_lint_basics(i, &i.code);
        }
        // Discovery order: dashless ## is seen first (line 5), missing done is
        // reported on EOF for the task whose heading was line 11.
        assert!(
            issues[0].code == "heading-missing-em-dash",
            "first issue should be the dashless heading"
        );
    }
}
