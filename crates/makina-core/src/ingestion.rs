//! Ingestion issue and report types.
//!
//! Captures structured issues (blocking or warning) produced during task-graph
//! ingestion by the interpreter, validator, or qualifier passes.  The
//! [`IngestionReport`] is the single result object returned to callers (e.g.
//! `RunView`) so they can decide whether to proceed or surface problems.

use serde::{Deserialize, Serialize};

use crate::task::TaskId;

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

// ── Tests (per task spec) ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
}
