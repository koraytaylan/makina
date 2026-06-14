//! Role definitions, system prompts, and verdict parsing for the Developer and
//! Reviewer agent roles.
//!
//! # Domain vs view
//!
//! [`Role`] is the **domain enum** used by the orchestrator to select a system
//! prompt and build a [`SessionConfig`].  [`crate::api::AgentRole`] is the
//! **view mirror** used by the TUI-facing `api` module — it mirrors the same
//! concepts but is defined independently so that `api` does not depend on core
//! orchestration types.  There is no `From` conversion between them here;
//! task 21 (`develop-review-loop`) will add that mapping if needed.
//!
//! # System prompts
//!
//! [`DEVELOPER_SYSTEM_PROMPT`] and [`REVIEWER_SYSTEM_PROMPT`] are the
//! role-specific instructions sent to the agent backend at session spawn time
//! via [`SessionConfig::system_prompt`].  They are concise MVP prompts:
//!
//! - The Developer prompt tells the agent it is implementing a single task in
//!   its current working directory (a git worktree) and should make focused,
//!   correct code changes.
//! - The Reviewer prompt tells the agent to review the work and **emit a
//!   structured JSON verdict** that Makina can parse via
//!   [`parse_review_verdict`].
//!
//! # Reviewer verdict contract
//!
//! The Reviewer must output a single JSON object — optionally wrapped in a
//! ` ```json … ``` ` code fence and/or surrounded by prose:
//!
//! ```json
//! {"verdict":"approve"}
//! ```
//! or
//! ```json
//! {"verdict":"reject","feedback":"<actionable reason>"}
//! ```
//!
//! [`parse_review_verdict`] extracts the outermost `{ … }` JSON object,
//! strips fences and prose, and maps the result to [`ReviewVerdict`].
//!
//! # Usage by orchestrator
//!
//! ```rust,ignore
//! // Developer session
//! let assignment = config.roles.developer.clone();
//! let session_cfg = session_config_for(Role::Developer, worktree_path, assignment);
//! let mut session = backend.spawn(session_cfg).await?;
//!
//! // Reviewer session
//! let assignment = config.roles.reviewer.clone();
//! let session_cfg = session_config_for(Role::Reviewer, worktree_path, assignment);
//! let mut session = backend.spawn(session_cfg).await?;
//! // … collect response …
//! let verdict = parse_review_verdict(&response_text)?;
//! ```

use std::path::PathBuf;

use thiserror::Error;

use crate::api;
use crate::backend::SessionConfig;
use crate::config::RoleAssignment;

// ── Role ──────────────────────────────────────────────────────────────────────

/// The domain role of an agent session.
///
/// Determines which system prompt is injected when a session is spawned.
/// The same [`crate::backend::AgentBackend`] implementation serves both roles;
/// the role is encoded entirely in [`SessionConfig::system_prompt`].
///
/// # Relation to `api::AgentRole`
///
/// [`crate::api::AgentRole`] is the **view mirror** of this enum — defined
/// independently in the `api` module so the TUI layer does not depend on
/// orchestration internals.  The two enums are structurally identical but are
/// intentionally NOT coupled (no `From` conversion in this module).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The agent that implements a task: makes code changes inside the
    /// worktree, keeps changes focused, and aims for correct, compiling output.
    Developer,
    /// The agent that reviews the Developer's output: evaluates correctness
    /// against the task's intent and emits a machine-readable
    /// [`ReviewVerdict`].
    Reviewer,
}

// ── System prompts ─────────────────────────────────────────────────────────────

/// System prompt for the **Developer** role.
///
/// Instructs the agent that it is implementing a single well-scoped task
/// inside the current working directory (a git worktree).  Keeps the
/// instructions concise and MVP-appropriate: make focused, correct changes;
/// Makina runs gates separately.
pub const DEVELOPER_SYSTEM_PROMPT: &str = "\
You are the Developer agent in Makina, a multi-agent software-factory \
orchestrator. Your job is to implement a single well-scoped task inside the \
current working directory (a git worktree dedicated to this task).

Guidelines:
- Read the task description carefully and make only the changes required to \
satisfy it.
- Keep changes focused: do not refactor unrelated code or add features beyond \
the task scope.
- Aim for correct, compiling output. Makina runs quality gates separately, so \
you do not need to run them yourself, but your code should be correct.
- If the task is already done, say so clearly and make no changes.";

/// System prompt for the **Reviewer** role.
///
/// Instructs the agent to review the work already done in the working
/// directory against the task's intent and emit a **structured verdict**.
///
/// # Output contract
///
/// The Reviewer MUST output a single JSON object — and nothing else (prose and
/// ` ```json ``` ` fences are tolerated by the parser):
///
/// ```json
/// {"verdict":"approve"}
/// ```
/// or
/// ```json
/// {"verdict":"reject","feedback":"<actionable reason>"}
/// ```
///
/// `"approve"` means the implementation satisfies the task's `done_when`
/// criterion.  `"reject"` means it does not; `feedback` must be a concrete,
/// actionable description of what needs to change.
pub const REVIEWER_SYSTEM_PROMPT: &str = "\
You are the Reviewer agent in Makina, a multi-agent software-factory \
orchestrator. Your job is to review the work done in the current working \
directory (a git worktree) against the task's acceptance criterion.

Evaluate whether the implementation satisfies the task's intent and \
`done_when` condition.

You MUST respond with ONLY a single JSON verdict object — no prose before or \
after it (a ```json code fence is acceptable but optional):

  {\"verdict\":\"approve\"}

  OR

  {\"verdict\":\"reject\",\"feedback\":\"<actionable reason>\"}

Rules:
- Use \"approve\" if the implementation fully satisfies the task criterion.
- Use \"reject\" if it does not; provide a concrete, actionable `feedback` \
string explaining what must change.
- Output ONLY the JSON object (or a single ```json fence containing it). \
No other text.";

// ── Convenience functions ─────────────────────────────────────────────────────

/// Return the system prompt for the given [`Role`].
///
/// # Example
/// ```
/// use makina_core::roles::{Role, system_prompt_for, DEVELOPER_SYSTEM_PROMPT};
/// assert_eq!(system_prompt_for(Role::Developer), DEVELOPER_SYSTEM_PROMPT);
/// ```
pub fn system_prompt_for(role: Role) -> &'static str {
    match role {
        Role::Developer => DEVELOPER_SYSTEM_PROMPT,
        Role::Reviewer => REVIEWER_SYSTEM_PROMPT,
    }
}

/// Combine a built-in role prompt with optional custom instructions.
///
/// # Arguments
/// - `builtin`: the base system prompt (e.g., `DEVELOPER_SYSTEM_PROMPT`)
/// - `custom`: optional project-specific instructions
/// - `mode`: how to combine them — `Some("replace")` substitutes `custom` for
///   `builtin`; `None` or any other value appends (default)
///
/// # Returns
/// - If `custom` is `None`, returns `builtin` as-is.
/// - If `mode == Some("replace")`, returns `custom`.
/// - Otherwise, returns `builtin` with `custom` appended (separated by two newlines).
fn combine_prompt(builtin: &str, custom: Option<&str>, mode: Option<&str>) -> String {
    match custom {
        None => builtin.to_string(),
        Some(c) => match mode {
            Some("replace") => c.to_string(),
            _ => format!("{builtin}\n\n{c}"), // append (default)
        },
    }
}

/// Return the effective system prompt for a role, combining its built-in constant
/// with any project-specific instructions.
///
/// If no assignment is provided, returns the role's built-in prompt as-is.
/// Otherwise, applies the `system_prompt_mode` semantics (default is append).
pub fn effective_system_prompt(role: Role, assignment: Option<&RoleAssignment>) -> String {
    let builtin = system_prompt_for(role);
    match assignment {
        None => builtin.to_string(),
        Some(a) => combine_prompt(
            builtin,
            a.system_prompt.as_deref(),
            a.system_prompt_mode.as_deref(),
        ),
    }
}

/// Build a [`SessionConfig`] for the given [`Role`], working directory, and role assignment.
///
/// Sets `system_prompt` to the role-appropriate constant (optionally combined with
/// project-specific instructions) and carries the `mode`, `model`, and `effort`
/// defaults from the role's assignment. The orchestrator calls this when spawning
/// a session for a role.
///
/// # Example
/// ```
/// use std::path::PathBuf;
/// use makina_core::roles::{Role, session_config_for, REVIEWER_SYSTEM_PROMPT};
/// use makina_core::config::RoleAssignment;
/// let assignment = RoleAssignment {
///     provider: "default".to_string(),
///     mode: None,
///     model: None,
///     effort: None,
///     system_prompt: None,
///     system_prompt_mode: None,
/// };
/// let cfg = session_config_for(Role::Reviewer, PathBuf::from("/tmp/worktree"), Some(assignment));
/// assert_eq!(cfg.system_prompt, REVIEWER_SYSTEM_PROMPT);
/// assert_eq!(cfg.working_dir, PathBuf::from("/tmp/worktree"));
/// ```
pub fn session_config_for(
    role: Role,
    working_dir: PathBuf,
    assignment: Option<RoleAssignment>,
) -> SessionConfig {
    let system_prompt = effective_system_prompt(role, assignment.as_ref());
    let (mode, model, effort) = assignment
        .map(|a| (a.mode, a.model, a.effort))
        .unwrap_or((None, None, None));

    SessionConfig {
        working_dir,
        system_prompt,
        mode,
        model,
        effort,
        extra: None,
        task_id: None,
    }
}

/// Return the current model name from a session's discovered capabilities, if any.
///
/// Scans `caps.config_options` for the first entry with `category == "model"`
/// and returns its `current_value` as a `String`.  Returns `None` if capabilities
/// are absent, no model option is found, or the current value is not a string.
///
/// Used by the Developer and Reviewer actors to resolve the model name for the
/// `Event::RoleTurnMetrics` emission when the role assignment did not specify one.
pub fn current_model_from(caps: Option<&api::SessionCapabilities>) -> Option<String> {
    let caps = caps?;
    caps.config_options
        .iter()
        .find(|opt| opt.category == "model")
        .and_then(|opt| opt.current_value.as_ref())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

// ── ReviewVerdict ─────────────────────────────────────────────────────────────

/// The Reviewer's verdict on a Developer's output.
///
/// Produced by [`parse_review_verdict`] from the Reviewer agent's model
/// output.  The Supervisor (task 21) uses this verdict to drive the FSM
/// ([`crate::state_machine::transition`] with `ReviewerApproved` or
/// `ReviewerRejected`).
///
/// This type was previously defined in [`crate::actors::reviewer`]; it has
/// been moved here because verdict parsing is a role-prompt concern.
/// [`crate::actors::reviewer`] re-exports it from here.
#[derive(Debug, Clone, PartialEq)]
pub enum ReviewVerdict {
    /// The output meets the acceptance criteria; the task should advance to Done.
    Approve,

    /// The output does not meet the acceptance criteria; the Developer should
    /// iterate.  The `feedback` string will be passed to the Developer in the
    /// next iteration (task 21).
    Reject {
        /// Human-readable, actionable explanation of what must change.
        feedback: String,
    },
}

// ── VerdictParseError ─────────────────────────────────────────────────────────

/// Errors returned by [`parse_review_verdict`].
#[derive(Debug, Error)]
pub enum VerdictParseError {
    /// No JSON object (`{ … }`) was found in the model output.
    ///
    /// The Reviewer prompt specifies a strict output contract; if no JSON is
    /// present the model deviated from the contract.
    #[error("no JSON object found in reviewer output")]
    NoJsonObject,

    /// The JSON object was found but `serde_json` could not deserialize it.
    ///
    /// The inner `serde_json::Error` describes the specific field mismatch.
    #[error("failed to deserialize verdict JSON: {0}")]
    SerdeError(#[from] serde_json::Error),

    /// The `verdict` field had an unrecognised value (neither `"approve"` nor
    /// `"reject"`, case-insensitive).
    #[error("unknown verdict value: {0:?}; expected \"approve\" or \"reject\"")]
    UnknownVerdict(String),
}

// ── parse_review_verdict ──────────────────────────────────────────────────────

/// Parse a Reviewer agent's model output into a [`ReviewVerdict`].
///
/// # Input contract
///
/// The input is the raw text emitted by the Reviewer agent.  It SHOULD be (or
/// contain) a JSON object matching the output contract described in
/// [`REVIEWER_SYSTEM_PROMPT`]:
///
/// ```json
/// {"verdict":"approve"}
/// ```
/// or
/// ```json
/// {"verdict":"reject","feedback":"<reason>"}
/// ```
///
/// The parser is **tolerant**: it strips leading/trailing prose and
/// ` ```json … ``` ` fences before locating the outermost `{ … }` JSON object
/// (same approach as [`crate::json::extract_json_object`]).
///
/// # Case tolerance
///
/// `"Approve"`, `"APPROVE"`, `"approve"` are all accepted.  Same for
/// `"reject"`.
///
/// # Missing `feedback` on reject
///
/// If `verdict` is `"reject"` but `feedback` is absent or empty, the parser
/// substitutes a default message: `"(no feedback provided)"`.  This is the
/// least-surprising behaviour: a `Reject` with no feedback is still actionable
/// (the Developer knows the work was rejected) and does not require the caller
/// to handle a spurious error just because the model omitted the field.
///
/// # Errors
///
/// Returns [`VerdictParseError`] if:
/// - No JSON object is found in `model_output` → [`VerdictParseError::NoJsonObject`].
/// - `serde_json` fails to parse the extracted JSON → [`VerdictParseError::SerdeError`].
/// - The `verdict` field value is not `"approve"` or `"reject"` →
///   [`VerdictParseError::UnknownVerdict`].
pub fn parse_review_verdict(model_output: &str) -> Result<ReviewVerdict, VerdictParseError> {
    // Step 1: extract the outermost { … } JSON object, tolerating fences and prose.
    let json_str =
        crate::json::extract_json_object(model_output).ok_or(VerdictParseError::NoJsonObject)?;

    // Step 2: deserialize into a loosely-typed map to allow case-insensitive
    // verdict matching and optional fields.
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json_str)?;

    // Step 3: extract and normalize the verdict field.
    let verdict_raw = map
        .get("verdict")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let verdict_lower = verdict_raw.to_ascii_lowercase();

    match verdict_lower.as_str() {
        "approve" => Ok(ReviewVerdict::Approve),
        "reject" => {
            // Extract feedback; default to a canned message if absent/empty.
            let feedback = map
                .get("feedback")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("(no feedback provided)")
                .to_string();
            Ok(ReviewVerdict::Reject { feedback })
        }
        other => Err(VerdictParseError::UnknownVerdict(other.to_string())),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use futures::StreamExt;

    use super::*;
    use crate::backend::noop::NoopBackend;
    use crate::backend::{AgentBackend, Prompt, ResponseEvent};

    // ── effective_system_prompt (combine_prompt) ──────────────────────────────

    #[test]
    fn append_extends_builtin() {
        // When system_prompt is provided and mode is not ("append" is default),
        // effective_system_prompt should return builtin + "\n\n" + custom.
        let assignment = RoleAssignment {
            provider: "default".to_string(),
            mode: None,
            model: None,
            effort: None,
            system_prompt: Some("X".to_string()),
            system_prompt_mode: None,
        };

        let result = effective_system_prompt(Role::Developer, Some(&assignment));
        let expected = format!("{}\n\nX", DEVELOPER_SYSTEM_PROMPT);
        assert_eq!(
            result, expected,
            "append mode should combine builtin and custom with newlines"
        );
    }

    #[test]
    fn replace_overrides() {
        // When system_prompt_mode is explicitly "replace", it should return
        // exactly the custom prompt with no builtin prefix.
        let assignment = RoleAssignment {
            provider: "default".to_string(),
            mode: None,
            model: None,
            effort: None,
            system_prompt: Some("X".to_string()),
            system_prompt_mode: Some("replace".to_string()),
        };

        let result = effective_system_prompt(Role::Developer, Some(&assignment));
        assert_eq!(
            result, "X",
            "replace mode should return only the custom prompt"
        );
    }

    #[test]
    fn absent_uses_builtin() {
        // When system_prompt is None, effective_system_prompt should return
        // the role's builtin prompt as-is.
        let assignment = RoleAssignment {
            provider: "default".to_string(),
            mode: None,
            model: None,
            effort: None,
            system_prompt: None,
            system_prompt_mode: None,
        };

        let result = effective_system_prompt(Role::Developer, Some(&assignment));
        assert_eq!(
            result,
            DEVELOPER_SYSTEM_PROMPT.to_string(),
            "absent custom prompt should use builtin"
        );

        // Also test with None assignment directly.
        let result_none = effective_system_prompt(Role::Reviewer, None);
        assert_eq!(
            result_none,
            REVIEWER_SYSTEM_PROMPT.to_string(),
            "None assignment should use builtin"
        );
    }

    // ── system_prompt_for / session_config_for ────────────────────────────────

    #[test]
    fn system_prompt_for_developer_returns_dev_const() {
        assert_eq!(
            system_prompt_for(Role::Developer),
            DEVELOPER_SYSTEM_PROMPT,
            "system_prompt_for(Developer) must return DEVELOPER_SYSTEM_PROMPT"
        );
    }

    #[test]
    fn system_prompt_for_reviewer_returns_reviewer_const() {
        assert_eq!(
            system_prompt_for(Role::Reviewer),
            REVIEWER_SYSTEM_PROMPT,
            "system_prompt_for(Reviewer) must return REVIEWER_SYSTEM_PROMPT"
        );
    }

    #[test]
    fn developer_and_reviewer_prompts_are_distinct() {
        assert_ne!(
            DEVELOPER_SYSTEM_PROMPT, REVIEWER_SYSTEM_PROMPT,
            "Developer and Reviewer prompts must not be identical"
        );
    }

    #[test]
    fn session_config_for_developer_sets_correct_prompt_and_dir() {
        let dir = PathBuf::from("/repo/worktree/task-42");
        let cfg = session_config_for(Role::Developer, dir.clone(), None);
        assert_eq!(cfg.system_prompt, DEVELOPER_SYSTEM_PROMPT);
        assert_eq!(cfg.working_dir, dir);
        assert!(cfg.extra.is_none());
        assert!(cfg.mode.is_none());
        assert!(cfg.model.is_none());
        assert!(cfg.effort.is_none());
    }

    #[test]
    fn session_config_for_reviewer_sets_correct_prompt_and_dir() {
        let dir = PathBuf::from("/repo/worktree/task-42-review");
        let cfg = session_config_for(Role::Reviewer, dir.clone(), None);
        assert_eq!(cfg.system_prompt, REVIEWER_SYSTEM_PROMPT);
        assert_eq!(cfg.working_dir, dir);
        assert!(cfg.extra.is_none());
        assert!(cfg.mode.is_none());
        assert!(cfg.model.is_none());
        assert!(cfg.effort.is_none());
    }

    // ── parse_review_verdict: happy paths ─────────────────────────────────────

    #[test]
    fn parse_bare_approve_json() {
        let input = r#"{"verdict":"approve"}"#;
        let v = parse_review_verdict(input).expect("bare approve must parse");
        assert_eq!(v, ReviewVerdict::Approve);
    }

    #[test]
    fn parse_bare_reject_json_with_feedback() {
        let input = r#"{"verdict":"reject","feedback":"Missing unit tests."}"#;
        let v = parse_review_verdict(input).expect("bare reject must parse");
        assert_eq!(
            v,
            ReviewVerdict::Reject {
                feedback: "Missing unit tests.".to_string()
            }
        );
    }

    #[test]
    fn parse_fenced_approve() {
        let input = "```json\n{\"verdict\":\"approve\"}\n```";
        let v = parse_review_verdict(input).expect("fenced approve must parse");
        assert_eq!(v, ReviewVerdict::Approve);
    }

    #[test]
    fn parse_fenced_reject_with_feedback() {
        let input = "Here is my review:\n```json\n{\"verdict\":\"reject\",\"feedback\":\"Fix the bug.\"}\n```\nPlease address the feedback.";
        let v = parse_review_verdict(input).expect("prose-wrapped fenced reject must parse");
        assert_eq!(
            v,
            ReviewVerdict::Reject {
                feedback: "Fix the bug.".to_string()
            }
        );
    }

    #[test]
    fn parse_prose_wrapped_approve() {
        let input =
            "After reviewing the code I conclude that {\"verdict\":\"approve\"} is appropriate.";
        let v = parse_review_verdict(input).expect("prose-wrapped approve must parse");
        assert_eq!(v, ReviewVerdict::Approve);
    }

    #[test]
    fn parse_case_insensitive_approve() {
        // "Approve" with capital A.
        let input = r#"{"verdict":"Approve"}"#;
        let v = parse_review_verdict(input).expect("case-insensitive Approve must parse");
        assert_eq!(v, ReviewVerdict::Approve);
    }

    #[test]
    fn parse_case_insensitive_reject() {
        let input = r#"{"verdict":"REJECT","feedback":"Needs more work."}"#;
        let v = parse_review_verdict(input).expect("case-insensitive REJECT must parse");
        assert_eq!(
            v,
            ReviewVerdict::Reject {
                feedback: "Needs more work.".to_string()
            }
        );
    }

    // ── parse_review_verdict: missing-feedback reject ─────────────────────────

    /// Per the documented rule: a `reject` with no `feedback` field (or an empty
    /// one) substitutes a default message rather than erroring.
    #[test]
    fn parse_reject_without_feedback_uses_default_message() {
        let input = r#"{"verdict":"reject"}"#;
        let v = parse_review_verdict(input).expect("reject without feedback must not error");
        match v {
            ReviewVerdict::Reject { feedback } => {
                assert!(
                    !feedback.is_empty(),
                    "default feedback must not be empty; got: {feedback:?}"
                );
            }
            ReviewVerdict::Approve => panic!("expected Reject, got Approve"),
        }
    }

    #[test]
    fn parse_reject_with_empty_feedback_uses_default_message() {
        let input = r#"{"verdict":"reject","feedback":""}"#;
        let v = parse_review_verdict(input).expect("reject with empty feedback must not error");
        match v {
            ReviewVerdict::Reject { feedback } => {
                assert!(
                    !feedback.is_empty(),
                    "default feedback must not be empty; got: {feedback:?}"
                );
            }
            ReviewVerdict::Approve => panic!("expected Reject, got Approve"),
        }
    }

    // ── parse_review_verdict: error cases ─────────────────────────────────────

    #[test]
    fn parse_no_json_returns_no_json_object_error() {
        let input = "The work looks good to me.";
        let err = parse_review_verdict(input).expect_err("no JSON must return an error");
        assert!(
            matches!(err, VerdictParseError::NoJsonObject),
            "expected NoJsonObject, got: {err:?}"
        );
    }

    #[test]
    fn parse_malformed_json_returns_serde_error() {
        // Braces are balanced so extract_json_object succeeds, but serde_json
        // fails because the content is not valid JSON.
        let input = r#"{"verdict": "approve" BROKEN_KEY_WITHOUT_COLON "value"}"#;
        let err = parse_review_verdict(input).expect_err("malformed JSON must return an error");
        assert!(
            matches!(err, VerdictParseError::SerdeError(_)),
            "expected SerdeError, got: {err:?}"
        );
    }

    #[test]
    fn parse_unknown_verdict_value_returns_unknown_verdict_error() {
        let input = r#"{"verdict":"maybe"}"#;
        let err = parse_review_verdict(input).expect_err("unknown verdict must return an error");
        match &err {
            VerdictParseError::UnknownVerdict(v) => {
                assert_eq!(
                    v, "maybe",
                    "unknown verdict message should contain the bad value"
                );
            }
            other => panic!("expected UnknownVerdict, got: {other:?}"),
        }
    }

    // ── Developer plumbing test via NoopBackend ───────────────────────────────

    /// Proves that `session_config_for(Developer, dir)` flows through the
    /// backend session API end-to-end.  NoopBackend ignores the system prompt
    /// (by design) — the point is to prove the plumbing, not prompt-driven
    /// behaviour (the real proof is the `#[ignore]`d ACP test in makina-acp).
    #[tokio::test]
    async fn developer_session_config_flows_through_noop_backend() {
        let backend = Arc::new(NoopBackend::with_responses(vec![
            "fn main() { println!(\"hello\"); }".to_string(),
        ]));

        let config =
            session_config_for(Role::Developer, PathBuf::from("/tmp/worktree/task-1"), None);
        let mut session = backend
            .spawn(config)
            .await
            .expect("NoopBackend::spawn must succeed");

        let mut stream = session
            .prompt(Prompt::new("Implement a function that adds two integers."))
            .await
            .expect("prompt must succeed");

        let mut dev_output = String::new();
        while let Some(item) = stream.next().await {
            match item.expect("no error in noop stream") {
                ResponseEvent::TextChunk { text } => dev_output.push_str(&text),
                // Side-channel events do not contribute to the developer output.
                ResponseEvent::ThoughtChunk { .. }
                | ResponseEvent::ToolCall { .. }
                | ResponseEvent::ToolCallUpdate { .. }
                | ResponseEvent::CurrentModeUpdate { .. } => {}
                ResponseEvent::TurnComplete { .. } => break,
            }
        }
        drop(stream);
        session.terminate().await.expect("terminate must succeed");

        assert!(
            !dev_output.is_empty(),
            "developer output must be non-empty; got: {:?}",
            dev_output
        );
    }

    // ── ReviewVerdict: basic derives ──────────────────────────────────────────

    #[test]
    fn review_verdict_debug_and_clone() {
        let a = ReviewVerdict::Approve;
        let b = a.clone();
        assert_eq!(a, b);
        assert!(format!("{a:?}").contains("Approve"));

        let r = ReviewVerdict::Reject {
            feedback: "try again".to_string(),
        };
        let r2 = r.clone();
        assert_eq!(r, r2);
        assert!(format!("{r:?}").contains("Reject"));
    }

    // ── SessionConfig with role assignments ───────────────────────────────────

    #[test]
    fn session_config_for_carries_role_assignments() {
        // Proves that session_config_for correctly threads role assignment
        // (mode, model, effort) into the SessionConfig.
        let assignment = RoleAssignment {
            provider: "grok".to_string(),
            mode: Some("code".to_string()),
            model: Some("claude-opus".to_string()),
            effort: Some("high".to_string()),
            system_prompt: None,
            system_prompt_mode: None,
        };

        let cfg = session_config_for(
            Role::Developer,
            PathBuf::from("/tmp/worktree"),
            Some(assignment),
        );

        assert_eq!(cfg.mode, Some("code".to_string()));
        assert_eq!(cfg.model, Some("claude-opus".to_string()));
        assert_eq!(cfg.effort, Some("high".to_string()));
        assert_eq!(cfg.system_prompt, DEVELOPER_SYSTEM_PROMPT);
    }

    #[test]
    fn session_config_for_none_assignment_has_no_selections() {
        // Proves that when no assignment is provided, selections are None.
        let cfg = session_config_for(Role::Reviewer, PathBuf::from("/tmp/worktree"), None);

        assert_eq!(cfg.mode, None);
        assert_eq!(cfg.model, None);
        assert_eq!(cfg.effort, None);
        assert_eq!(cfg.system_prompt, REVIEWER_SYSTEM_PROMPT);
    }
}
