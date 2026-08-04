//! Coordinator-owned rendering of plan and root status documents.

use crate::plan::{AuthoredTaskStatus, GitObjectId, PlanDocument, PlanIntegrationState};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StatusEditError {
    #[error("status document is missing the unique `{0}` anchor")]
    MissingAnchor(&'static str),
    #[error("status document contains duplicate `{0}` anchors")]
    DuplicateAnchor(&'static str),
    #[error("root status has {count} rows for plan {number}; expected exactly one")]
    RootCardinality { number: String, count: usize },
    #[error(
        "root status already has a different row for plan {number}\nexpected: {expected}\nactual: {actual}"
    )]
    RootMismatch {
        number: String,
        expected: String,
        actual: String,
    },
    #[error("exception record for task {0} is missing or ambiguous")]
    ExceptionCardinality(String),
    #[error("exception record is not a bounded single line")]
    InvalidException,
    #[error("rendered status would not load back: {0}")]
    UnreadableRender(String),
}

fn validate_exception_part(value: &str) -> Result<(), StatusEditError> {
    if value.is_empty() || value.len() > 240 || value.contains(['\n', '\r', '\0', ';']) {
        return Err(StatusEditError::InvalidException);
    }
    Ok(())
}

/// Add one active blocker record to the fixed Exceptions field.
pub fn append_blocker_exception(
    body: &str,
    task: &str,
    reason: &str,
) -> Result<String, StatusEditError> {
    validate_exception_part(task)?;
    validate_exception_part(reason)?;
    let prefix = "- **Exceptions:** ";
    let mut lines = body.lines().map(str::to_owned).collect::<Vec<_>>();
    let hits = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with(prefix))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let [index] = hits.as_slice() else {
        return Err(if hits.is_empty() {
            StatusEditError::MissingAnchor(prefix)
        } else {
            StatusEditError::DuplicateAnchor(prefix)
        });
    };
    let current = lines[*index]
        .strip_prefix(prefix)
        .expect("matched prefix")
        .trim_end_matches('.');
    let record = format!("{task} — {}", reason.trim_end_matches('.'));
    let value = if current == "—" {
        record
    } else {
        format!("{current}; {record}")
    };
    lines[*index] = format!("{prefix}{value}.");
    let mut out = lines.join("\n");
    if body.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Mark the unique active record for `task` resolved without deleting history.
pub fn resolve_exception(
    body: &str,
    task: &str,
    resolution: &str,
) -> Result<String, StatusEditError> {
    validate_exception_part(task)?;
    validate_exception_part(resolution)?;
    let prefix = "- **Exceptions:** ";
    let mut lines = body.lines().map(str::to_owned).collect::<Vec<_>>();
    let hits = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with(prefix))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let [index] = hits.as_slice() else {
        return Err(if hits.is_empty() {
            StatusEditError::MissingAnchor(prefix)
        } else {
            StatusEditError::DuplicateAnchor(prefix)
        });
    };
    let value = lines[*index]
        .strip_prefix(prefix)
        .expect("matched prefix")
        .trim_end_matches('.');
    let mut records = value
        .split(';')
        .map(str::trim)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let target = format!("{task} — ");
    let matches = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.starts_with(&target) && !record.contains(" [resolved: "))
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let [record_index] = matches.as_slice() else {
        return Err(StatusEditError::ExceptionCardinality(task.to_owned()));
    };
    records[*record_index].push_str(&format!(
        " [resolved: {}]",
        resolution.trim_end_matches('.')
    ));
    lines[*index] = format!("{prefix}{}.", records.join("; "));
    let mut out = lines.join("\n");
    if body.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusTransition {
    pub integration_state: PlanIntegrationState,
    pub run: Option<String>,
    pub validation_base: Option<GitObjectId>,
    pub mode: Option<String>,
    pub final_oid: Option<GitObjectId>,
    pub display_status: String,
    pub last_updated: String,
}

/// The two canonical spellings of a final merge mode, returned together.
///
/// `.0` is the commit-trailer vocabulary (`Makina-Final-Mode`, lowercase-kebab);
/// `.1` is the `STATUS.md` `Integration` vocabulary, which is the *only*
/// spelling the plan loader accepts. They are handed out as a pair so a caller
/// cannot write one vocabulary into the other's slot — writing the trailer
/// spelling into STATUS produced an `invalid integration mode` that made the
/// finalized plan unreadable.
pub fn final_mode_names(mode: crate::config::FinalMerge) -> (&'static str, &'static str) {
    match mode {
        crate::config::FinalMerge::Squash => ("squash", "Squash"),
        crate::config::FinalMerge::MergeCommit => ("merge-commit", "MergeCommit"),
        crate::config::FinalMerge::Stage => ("stage", "Stage"),
        crate::config::FinalMerge::Manual => ("manual", "Manual"),
    }
}

/// The documented root/heading badge for an integration state.
///
/// Mirrors the display-precedence table in the plan-0048 architecture note. Use
/// it instead of an ad-hoc string so a writer cannot invent a badge the status
/// validator rejects as lifecycle drift.
pub fn display_badge(state: PlanIntegrationState) -> &'static str {
    match state {
        PlanIntegrationState::Planned => "📋 Planned",
        PlanIntegrationState::Assembling => "🚧 In progress",
        PlanIntegrationState::AwaitingIntegration => "⏳ Awaiting integration",
        PlanIntegrationState::FinalizationPending => "🔄 Finalizing",
        PlanIntegrationState::IntegrationBlocked => "⛔ Blocked",
        PlanIntegrationState::Complete => "✅ Complete",
    }
}

fn state_name(state: PlanIntegrationState) -> &'static str {
    match state {
        PlanIntegrationState::Planned => "planned",
        PlanIntegrationState::Assembling => "assembling",
        PlanIntegrationState::AwaitingIntegration => "awaiting-integration",
        PlanIntegrationState::FinalizationPending => "finalization-pending",
        PlanIntegrationState::IntegrationBlocked => "integration-blocked",
        PlanIntegrationState::Complete => "complete",
    }
}

fn replace_unique_line(
    body: &mut String,
    prefix: &'static str,
    replacement: String,
) -> Result<(), StatusEditError> {
    let matches = body.lines().filter(|line| line.starts_with(prefix)).count();
    if matches == 0 {
        return Err(StatusEditError::MissingAnchor(prefix));
    }
    if matches != 1 {
        return Err(StatusEditError::DuplicateAnchor(prefix));
    }
    let trailing = body.ends_with('\n');
    *body = body
        .lines()
        .map(|line| {
            if line.starts_with(prefix) {
                replacement.as_str()
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if trailing {
        body.push('\n');
    }
    Ok(())
}

/// Render coordinator fields while preserving all authored narrative bytes.
pub fn render_plan_status(
    plan: &PlanDocument,
    transition: &StatusTransition,
) -> Result<String, StatusEditError> {
    let mut body = plan.status.source.body.clone();
    let done = plan
        .tasks
        .iter()
        .filter(|t| t.frontmatter.status == AuthoredTaskStatus::Done)
        .count();
    let blocked = plan
        .tasks
        .iter()
        .filter(|t| t.frontmatter.status == AuthoredTaskStatus::Blocked)
        .count();
    let dropped = plan
        .tasks
        .iter()
        .filter(|t| t.frontmatter.status == AuthoredTaskStatus::Dropped)
        .count();
    replace_unique_line(
        &mut body,
        "# Plan ",
        format!(
            "# Plan {} — {} — {}",
            plan.key.number, plan.title, transition.display_status
        ),
    )?;
    replace_unique_line(
        &mut body,
        "- **Status:** ",
        format!(
            "- **Status:** {}.",
            transition.display_status.trim_end_matches('.')
        ),
    )?;
    replace_unique_line(
        &mut body,
        "- **Progress:** ",
        format!(
            "- **Progress:** {done}/{} tasks done; {blocked} blocked; {dropped} dropped.",
            plan.tasks.len()
        ),
    )?;
    let optional = |value: Option<&str>| value.map_or_else(|| "—".to_owned(), |v| format!("`{v}`"));
    let integration = format!(
        "- **Integration:** `{}`; run {}; base `{}` @ `{}`; validation base {}; mode {}; final integration {}.",
        state_name(transition.integration_state),
        optional(transition.run.as_deref()),
        plan.status.base_name,
        plan.status.base_oid,
        optional(transition.validation_base.as_ref().map(GitObjectId::as_str)),
        optional(transition.mode.as_deref()),
        optional(transition.final_oid.as_ref().map(GitObjectId::as_str))
    );
    replace_unique_line(&mut body, "- **Integration:** ", integration)?;
    replace_unique_line(&mut body, "_Last updated:", transition.last_updated.clone())?;
    // Fail here, not one task later. A rendered status the loader rejects is
    // committed to the plan ref and only explodes when the *next* durable claim
    // re-reads it, by which point the reported error names the reader rather
    // than the writer that produced it.
    crate::plan::verify_rendered_status(plan, &body)
        .map_err(|diagnostics| StatusEditError::UnreadableRender(diagnostics.join("; ")))?;
    Ok(body)
}

pub fn expected_root_row(plan: &PlanDocument) -> String {
    let progress = if plan.status.dropped == 0 {
        format!("{}/{}", plan.status.done, plan.status.total)
    } else {
        format!(
            "{} + {} / {}",
            plan.status.done, plan.status.dropped, plan.status.total
        )
    };
    format!(
        "| {} | {} | {} | {} | {} | [status]({}/STATUS.md) |",
        plan.key.number,
        plan.title,
        plan.status.display_status,
        progress,
        plan.status.outcome,
        plan.key.relative_dir.file_name().unwrap().to_string_lossy()
    )
}

/// How a root roll-up edit treats an absent row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootEdit {
    /// Insert an absent row; reuse one exact row; reject a divergent one.
    Register,
    /// Require exactly one existing row and rewrite it.
    Update,
    /// Insert an absent row, otherwise rewrite the unique existing one.
    Upsert,
}

/// Registration inserts an absent row and idempotently reuses one exact row.
pub fn register_root_row(root: &str, plan: &PlanDocument) -> Result<String, StatusEditError> {
    edit_root(root, plan, RootEdit::Register)
}

/// A lifecycle transition against a board that already carries the row.
///
/// Use this for boards read from the plan ref: registration guarantees the row
/// is there, so its absence is real corruption and must not be papered over.
pub fn update_root_row(root: &str, plan: &PlanDocument) -> Result<String, StatusEditError> {
    edit_root(root, plan, RootEdit::Update)
}

/// A lifecycle transition against a board read from the **base** branch.
///
/// Registration can only add a missing row to the plan ref — it must never move
/// the base branch — so a plan registered against a board that did not already
/// list it has its row on the plan ref and nowhere else. Finalization overlays
/// onto the current base board, so requiring the row there stranded such a plan
/// in `awaiting-integration` forever, with Phase P failing on every attempt.
/// Inserting the row is the same thing registration already decided to do.
/// Duplicate rows remain an error in every mode.
pub fn upsert_root_row(root: &str, plan: &PlanDocument) -> Result<String, StatusEditError> {
    edit_root(root, plan, RootEdit::Upsert)
}

/// Append one bounded coordinator disposition record without rewriting history.
pub fn append_disposition_exception(body: &str, record: &str) -> Result<String, StatusEditError> {
    const ANCHOR: &str = "## Exceptions";
    let count = body.lines().filter(|line| *line == ANCHOR).count();
    if count == 0 {
        return Err(StatusEditError::MissingAnchor(ANCHOR));
    }
    if count != 1 {
        return Err(StatusEditError::DuplicateAnchor(ANCHOR));
    }
    let offset = body.find(ANCHOR).expect("checked anchor") + ANCHOR.len();
    let mut out = body.to_owned();
    out.insert_str(offset, &format!("\n\n- {record}"));
    Ok(out)
}

fn edit_root(root: &str, plan: &PlanDocument, mode: RootEdit) -> Result<String, StatusEditError> {
    let prefix = format!("| {} |", plan.key.number);
    let positions = root
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with(&prefix))
        .collect::<Vec<_>>();
    let expected = expected_root_row(plan);
    let inserts = matches!(mode, RootEdit::Register | RootEdit::Upsert);
    match positions.as_slice() {
        [] if inserts => {
            let mut out = root.trim_end_matches('\n').to_owned();
            out.push('\n');
            out.push_str(&expected);
            out.push('\n');
            Ok(out)
        }
        [] => Err(StatusEditError::RootCardinality {
            number: plan.key.number.clone(),
            count: 0,
        }),
        [(_, actual)] if mode == RootEdit::Register && *actual == expected => Ok(root.to_owned()),
        [(_, actual)] if mode == RootEdit::Register => Err(StatusEditError::RootMismatch {
            number: plan.key.number.clone(),
            expected,
            actual: (*actual).to_owned(),
        }),
        [(index, _)] => {
            let trailing = root.ends_with('\n');
            let mut lines = root.lines().map(str::to_owned).collect::<Vec<_>>();
            lines[*index] = expected;
            let mut out = lines.join("\n");
            if trailing {
                out.push('\n');
            }
            Ok(out)
        }
        many => Err(StatusEditError::RootCardinality {
            number: plan.key.number.clone(),
            count: many.len(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocker_history_is_resolved_without_erasure() {
        let source = "# Plan 0048\n\n- **Exceptions:** task-a — timed out.\n";
        let resolved = resolve_exception(source, "task-a", "retry run-2").unwrap();
        assert!(resolved.contains("task-a — timed out [resolved: retry run-2]"));
        assert!(!resolved.contains("**Exceptions:** —"));
    }

    #[test]
    fn blocker_records_are_bounded_and_unique() {
        let source = "- **Exceptions:** —.\n";
        let blocked = append_blocker_exception(source, "task-a", "review failed").unwrap();
        assert_eq!(blocked, "- **Exceptions:** task-a — review failed.\n");
        assert!(resolve_exception(&blocked, "task-b", "retry").is_err());
        assert!(append_blocker_exception(source, "task-a", &"x".repeat(241)).is_err());
    }
}
