//! Shared audit types for the governance action gateway and ledger.
//!
//! This module defines the canonical [`AuditEntry`] shape (serde `Serialize`)
//! that is appended as one JSON object per line to `.tasks/{slug}/audit.jsonl`,
//! and the [`AuditSink`] trait that decouples producers of audit records
//! (the ACP transport in `makina-acp`) from the writer (Supervisor-owned file
//! appender in a later task).
//!
//! The no-op default sink is provided for tests and for backends that run
//! without an audit ledger.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Supporting types ──────────────────────────────────────────────────────────

/// Identity and description of the tool call that triggered a permission
/// decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRef {
    /// Logical tool name (e.g. `"write_file"`, `"run_terminal_cmd"`).
    pub name: String,
    /// Optional semantic kind supplied by the agent (e.g. `"edit"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The ACP `toolCallId` that uniquely identifies this request.
    pub id: String,
    /// Human-readable title of the operation (e.g. `"Writing to fs-probe.txt"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Outcome of an audited permission decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    /// The requested action was permitted.
    Allow,
    /// The requested action was denied or cancelled.
    Deny,
}

/// Policy that produced the decision together with its rationale.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyInfo {
    /// Stable name of the policy (e.g. `"WorktreePolicy"`).
    pub name: String,
    /// Human-readable explanation for why the decision was made.
    pub reason: String,
}

// ── AuditEntry ────────────────────────────────────────────────────────────────

/// A single immutable record of a governance decision.
///
/// Every field is serialized; optional fields are omitted when `None` so the
/// JSONL stays compact and stable for diff review.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// When the decision was made (RFC3339 UTC).
    pub timestamp: DateTime<Utc>,
    /// Identifier of the containing run (e.g. `"run-7"` or the orchestrator `RunId`).
    pub run_id: String,
    /// Task the audited action belongs to, when known (kebab-case slug).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// ACP session id from the permission request (when the entry comes from
    /// the gateway).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The tool call that required approval.
    pub tool: ToolRef,
    /// The high-level outcome chosen by the policy.
    pub decision: AuditDecision,
    /// The concrete `optionId` that was selected and echoed to the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
    /// Which policy produced the decision and why.
    pub policy: PolicyInfo,
    /// The working directory of the agent session at the time of the decision.
    pub working_dir: PathBuf,
}

// ── AuditSink seam ────────────────────────────────────────────────────────────

/// Destination for [`AuditEntry`] records.
///
/// The trait is deliberately synchronous and minimal so it can be used on the
/// hot path inside the ACP transport reader loop without adding an await point
/// for every permission prompt.
///
/// Object-safe so it can be stored as `Arc<dyn AuditSink>`.
pub trait AuditSink: Send + Sync {
    /// Append / observe one audit record.
    ///
    /// Implementations should be non-panicking; failures (e.g. disk full) are
    /// logged by the concrete sink but must not abort the caller.
    fn record(&self, entry: AuditEntry);
}

/// No-op implementation of [`AuditSink`].
///
/// Used as the default in tests, in unit-test backends, and anywhere an audit
/// ledger is not required.  All records are silently discarded.
#[derive(Clone, Debug, Default)]
pub struct NoopAuditSink;

impl AuditSink for NoopAuditSink {
    fn record(&self, _entry: AuditEntry) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn fixed_ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
            .single()
            .expect("valid timestamp")
    }

    fn sample_entry() -> AuditEntry {
        AuditEntry {
            timestamp: fixed_ts(),
            run_id: "run-42".to_string(),
            task_id: Some("audit-entry-type".to_string()),
            session_id: Some("sess-7f3a".to_string()),
            tool: ToolRef {
                name: "write_file".to_string(),
                kind: Some("edit".to_string()),
                id: "write_file__write_file_1780041520414_0".to_string(),
                title: Some("Writing to fs-probe.txt".to_string()),
            },
            decision: AuditDecision::Allow,
            option_id: Some("proceed_once".to_string()),
            policy: PolicyInfo {
                name: "WorktreePolicy".to_string(),
                reason: "working dir is the task worktree".to_string(),
            },
            working_dir: PathBuf::from("/tmp/makina/.worktrees/audit-entry-type"),
        }
    }

    #[test]
    fn audit_entry_round_trips_to_stable_single_line_json() {
        let original = sample_entry();

        let json =
            serde_json::to_string(&original).expect("AuditEntry must serialize without error");

        // Must be single-line (no pretty whitespace) for JSONL appends.
        assert!(
            !json.contains('\n'),
            "serialization must be compact single-line"
        );
        assert!(
            json.starts_with('{') && json.ends_with('}'),
            "must be a JSON object"
        );

        // Exact stable shape with the fixed sample data (field order = struct order).
        let expected = "{\"timestamp\":\"2026-06-01T12:00:00Z\",\"run_id\":\"run-42\",\"task_id\":\"audit-entry-type\",\"session_id\":\"sess-7f3a\",\"tool\":{\"name\":\"write_file\",\"kind\":\"edit\",\"id\":\"write_file__write_file_1780041520414_0\",\"title\":\"Writing to fs-probe.txt\"},\"decision\":\"allow\",\"option_id\":\"proceed_once\",\"policy\":{\"name\":\"WorktreePolicy\",\"reason\":\"working dir is the task worktree\"},\"working_dir\":\"/tmp/makina/.worktrees/audit-entry-type\"}";

        assert_eq!(
            json, expected,
            "AuditEntry JSON shape must be deterministic and stable"
        );

        let round_tripped: AuditEntry =
            serde_json::from_str(&json).expect("JSON must deserialize back to AuditEntry");

        assert_eq!(
            original, round_tripped,
            "round-tripped AuditEntry must equal the original"
        );
    }

    #[test]
    fn noop_sink_accepts_record() {
        let sink: NoopAuditSink = Default::default();
        let entry = sample_entry();

        // Calling record must be accepted (no panic, no effect).
        sink.record(entry.clone());

        // Also exercise the object-safe dyn form used for dependency injection.
        let dyn_sink: std::sync::Arc<dyn AuditSink> = std::sync::Arc::new(NoopAuditSink);
        dyn_sink.record(entry);
    }
}
