//! Permission policy seam for ACP `session/request_permission` requests.
//!
//! This module (task `worktree-permission-policy`) defines the pluggable
//! decision interface and the MVP default implementation: a worktree-scoped
//! auto-allow policy that selects the offered `allow_once` option when the
//! session is running inside its assigned per-task git worktree.
//!
//! The trait is object-safe (`Arc<dyn PermissionPolicy>`) so it can be
//! injected into the transport / client without monomorphising the whole
//! reader loop.

use std::path::{Path, PathBuf};

use crate::protocol::{PermissionOption, PermissionOptionKind, ToolCall};

/// Outcome of a policy decision for a permission request.
///
/// When `allow` is true the caller replies with a `PermissionOutcome::Selected`
/// using the supplied `option_id`.  When false the caller typically replies
/// `PermissionOutcome::Cancelled` (or selects a reject option if one is
/// distinguishable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    /// `true` → allow the tool call (select the `option_id`).
    pub allow: bool,
    /// The `optionId` to echo back when `allow` is true. `None` for deny paths.
    pub option_id: Option<String>,
    /// Human-readable justification (recorded in the audit ledger).
    pub reason: String,
}

/// Borrowed view of a `session/request_permission` request plus the session's
/// working directory (the per-task worktree path).
///
/// Policies receive this context rather than the raw wire type so future
/// extensions (e.g. richer tool metadata) do not force changes to every
/// policy implementation.
#[derive(Debug)]
pub struct PermissionRequestContext<'a> {
    /// The ACP session identifier.
    pub session_id: &'a str,
    /// The working directory in which the agent session is executing.
    /// For task work the Supervisor sets this to the per-task git worktree.
    pub working_dir: &'a Path,
    /// The pending tool call that requires approval.
    pub tool_call: &'a ToolCall,
    /// The choices the agent offered for this request (allow/reject variants).
    pub options: &'a [PermissionOption],
}

/// Synchronous, deterministic policy for `session/request_permission` requests.
///
/// Implementations must be `Send + Sync` because the policy is held behind
/// an `Arc` and invoked from the transport reader task.
///
/// Object-safety requirement (enforced by the `dyn` test): no `Self`-sized
/// bounds, no generic methods, and all methods take `&self`.
pub trait PermissionPolicy: Send + Sync {
    /// Decide whether to allow the requested operation and, if so, which of
    /// the offered `optionId`s to select.
    fn decide(&self, ctx: &PermissionRequestContext<'_>) -> PermissionDecision;

    /// Stable name used when emitting [`makina_core::governance::AuditEntry`]
    /// records (e.g. `"WorktreePolicy"`).
    fn name(&self) -> &str {
        "PermissionPolicy"
    }
}

/// The default MVP policy: auto-allow (picking the offered `allow_once`
/// option for least privilege) exactly when the request's `working_dir`
/// equals the per-task worktree this policy was constructed with.
///
/// Construction: `WorktreePolicy::new(session_config.working_dir)` — the same
/// value passed to `AcpBackend::spawn`.
///
/// **MVP limitation (documented):** this policy performs *no* validation that
/// the paths inside `tool_call` actually reside under the worktree.  It only
/// keys off the session working directory.  The audit ledger records every
/// auto-allow decision; sandboxed enforcement is future work.
#[derive(Debug, Clone)]
pub struct WorktreePolicy {
    worktree: PathBuf,
}

/// Check if a target path resolves to somewhere under the worktree.
///
/// This function resolves the target path via its parent directory (so a
/// not-yet-created file under the worktree is not falsely denied) and checks
/// if the result is contained within the worktree.
fn resolves_under(target: &str, worktree: &Path) -> bool {
    let target_path = Path::new(target);

    // Handle absolute paths and relative paths.
    let resolved_path = if target_path.is_absolute() {
        // For absolute paths, try to canonicalize the parent directory.
        match target_path.parent() {
            Some(parent) => {
                match std::fs::canonicalize(parent) {
                    Ok(canonical_parent) => {
                        canonical_parent.join(target_path.file_name().unwrap_or_default())
                    }
                    Err(_) => {
                        // Parent doesn't exist or can't be canonicalized.
                        // Fall back to lexical normalization.
                        normalize_path(target_path)
                    }
                }
            }
            None => target_path.to_path_buf(),
        }
    } else {
        // For relative paths, resolve relative to the worktree.
        let joined = worktree.join(target_path);
        match joined.parent() {
            Some(parent) => {
                match std::fs::canonicalize(parent) {
                    Ok(canonical_parent) => {
                        canonical_parent.join(joined.file_name().unwrap_or_default())
                    }
                    Err(_) => {
                        // Parent doesn't exist; use lexical normalization.
                        normalize_path(&joined)
                    }
                }
            }
            None => joined,
        }
    };

    // Check if the resolved path is under the worktree.
    // Canonicalize the worktree for comparison.
    let worktree_canonical = match std::fs::canonicalize(worktree) {
        Ok(canonical) => canonical,
        Err(_) => {
            // Worktree doesn't exist; use lexical normalization.
            normalize_path(worktree)
        }
    };

    // Check containment: resolved_path must start with worktree_canonical or be equal.
    resolved_path.starts_with(&worktree_canonical)
}

/// Normalize a path lexically without requiring it to exist.
///
/// Handles `.` and `..` components to produce a normalized absolute path.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::CurDir => {
                // Skip `.` components
            }
            std::path::Component::RootDir
            | std::path::Component::Normal(_)
            | std::path::Component::Prefix(_) => {
                components.push(component);
            }
        }
    }
    let mut result = PathBuf::new();
    for component in components {
        result.push(component);
    }
    result
}

impl WorktreePolicy {
    /// Build a policy that will auto-allow inside the supplied worktree path.
    pub fn new(worktree: impl Into<PathBuf>) -> Self {
        Self {
            worktree: worktree.into(),
        }
    }
}

impl PermissionPolicy for WorktreePolicy {
    fn decide(&self, ctx: &PermissionRequestContext<'_>) -> PermissionDecision {
        if ctx.working_dir == self.worktree {
            // Validate that any tool-call locations resolve under the worktree.
            if let Some(locations) = ctx
                .tool_call
                .extra
                .get("locations")
                .and_then(|v| v.as_array())
            {
                for loc in locations {
                    if let Some(path) = loc.get("path").and_then(|p| p.as_str())
                        && !resolves_under(path, &self.worktree)
                    {
                        tracing::warn!(
                            path = ?path,
                            worktree = ?self.worktree,
                            "tool-call path escapes worktree"
                        );
                        return PermissionDecision {
                            allow: false,
                            option_id: None,
                            reason: "path escapes worktree".into(),
                        };
                    }
                }
            }

            // Deterministically select the first offered allow-once option.
            // Order is the order the agent supplied; first match is stable.
            if let Some(opt) = ctx
                .options
                .iter()
                .find(|o| o.kind == PermissionOptionKind::AllowOnce)
            {
                return PermissionDecision {
                    allow: true,
                    option_id: Some(opt.option_id.clone()),
                    reason: format!(
                        "auto-allowed via WorktreePolicy (allow_once) for worktree {:?}",
                        self.worktree
                    ),
                };
            }

            // No allow_once offered — be explicit rather than silent deny.
            return PermissionDecision {
                allow: false,
                option_id: None,
                reason: "inside worktree but no allow_once option was offered".into(),
            };
        }

        PermissionDecision {
            allow: false,
            option_id: None,
            reason: format!(
                "working_dir {:?} is not the configured worktree {:?}",
                ctx.working_dir, self.worktree
            ),
        }
    }

    fn name(&self) -> &str {
        "WorktreePolicy"
    }
}

// A couple of free functions are intentionally not provided here; construction
// of a concrete policy and threading of the `Arc<dyn …>` happens in the
// gateway-threading task that wires `AcpCommand` / `Transport`.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::protocol::RequestPermissionParams;

    fn sample_params_with_three_options() -> RequestPermissionParams {
        RequestPermissionParams {
            session_id: "sess-worktree-1".into(),
            options: vec![
                PermissionOption {
                    option_id: "proceed_always".into(),
                    name: "Allow for this session".into(),
                    kind: PermissionOptionKind::AllowAlways,
                },
                PermissionOption {
                    option_id: "proceed_once".into(),
                    name: "Allow".into(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionOption {
                    option_id: "cancel".into(),
                    name: "Reject".into(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
            tool_call: ToolCall {
                tool_call_id: "write_file__example_1".into(),
                status: Some("pending".into()),
                title: Some("Writing example.txt".into()),
                kind: Some("edit".into()),
                extra: Default::default(),
            },
        }
    }

    #[test]
    fn worktree_policy_returns_allow_with_allow_once_id_and_nonempty_reason() {
        let worktree = PathBuf::from("/tmp/makina-worktrees/task-42");
        let policy = WorktreePolicy::new(worktree.clone());

        let params = sample_params_with_three_options();
        let ctx = PermissionRequestContext {
            session_id: &params.session_id,
            working_dir: &worktree,
            tool_call: &params.tool_call,
            options: &params.options,
        };

        let decision = policy.decide(&ctx);

        assert!(
            decision.allow,
            "expected allow inside the configured worktree"
        );
        assert_eq!(
            decision.option_id.as_deref(),
            Some("proceed_once"),
            "must deterministically select the allow-once option id"
        );
        assert!(
            !decision.reason.is_empty(),
            "reason must be non-empty for audit"
        );
        assert!(
            decision.reason.contains("WorktreePolicy"),
            "reason should mention the policy for traceability"
        );
    }

    #[test]
    fn worktree_policy_denies_outside_configured_worktree() {
        let policy = WorktreePolicy::new("/worktrees/task-real");
        let params = sample_params_with_three_options();
        let other_dir = PathBuf::from("/elsewhere");

        let ctx = PermissionRequestContext {
            session_id: &params.session_id,
            working_dir: &other_dir,
            tool_call: &params.tool_call,
            options: &params.options,
        };

        let decision = policy.decide(&ctx);
        assert!(!decision.allow);
        assert!(decision.option_id.is_none());
        assert!(!decision.reason.is_empty());
    }

    #[test]
    fn permission_policy_trait_is_object_safe_behind_arc() {
        // This would fail to compile if PermissionPolicy were not object-safe.
        let policy: Arc<dyn PermissionPolicy> =
            Arc::new(WorktreePolicy::new("/tmp/makina-worktrees/task-arc-test"));

        // Exercise it through the trait object to prove dynamic dispatch works.
        let params = sample_params_with_three_options();
        let worktree = PathBuf::from("/tmp/makina-worktrees/task-arc-test");
        let ctx = PermissionRequestContext {
            session_id: &params.session_id,
            working_dir: &worktree,
            tool_call: &params.tool_call,
            options: &params.options,
        };
        let d = policy.decide(&ctx);
        assert!(d.allow);
    }

    #[test]
    fn worktree_policy_no_allow_once_option_yields_deny() {
        let worktree = PathBuf::from("/tmp/wt");
        let policy = WorktreePolicy::new(worktree.clone());

        let params = RequestPermissionParams {
            session_id: "s".into(),
            options: vec![PermissionOption {
                option_id: "always".into(),
                name: "Always".into(),
                kind: PermissionOptionKind::AllowAlways,
            }],
            tool_call: ToolCall {
                tool_call_id: "t".into(),
                status: None,
                title: None,
                kind: None,
                extra: Default::default(),
            },
        };
        let ctx = PermissionRequestContext {
            session_id: &params.session_id,
            working_dir: &worktree,
            tool_call: &params.tool_call,
            options: &params.options,
        };

        let decision = policy.decide(&ctx);
        assert!(!decision.allow);
        assert!(decision.option_id.is_none());
        assert!(!decision.reason.is_empty());
    }

    #[test]
    fn test_worktree_policy_denies_paths_outside_worktree() {
        let worktree = PathBuf::from("/tmp/makina-worktrees/task-test");
        let policy = WorktreePolicy::new(worktree.clone());

        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "locations".into(),
            serde_json::json!([
                { "path": "/etc/passwd" }
            ]),
        );

        let params = RequestPermissionParams {
            session_id: "s".into(),
            options: vec![PermissionOption {
                option_id: "proceed_once".into(),
                name: "Allow".into(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            tool_call: ToolCall {
                tool_call_id: "t".into(),
                status: None,
                title: None,
                kind: None,
                extra,
            },
        };
        let ctx = PermissionRequestContext {
            session_id: &params.session_id,
            working_dir: &worktree,
            tool_call: &params.tool_call,
            options: &params.options,
        };

        let decision = policy.decide(&ctx);
        assert!(!decision.allow, "should deny path outside worktree");
        assert!(decision.option_id.is_none());
        assert_eq!(decision.reason, "path escapes worktree");
    }

    #[test]
    fn test_worktree_policy_allows_paths_inside_worktree() {
        use std::fs;
        use std::io::Write;

        // Create a temporary worktree directory for testing.
        let temp_dir = std::env::temp_dir().join("makina-test-worktree");
        let _ = fs::remove_dir_all(&temp_dir); // Clean up any previous test run
        fs::create_dir_all(&temp_dir).expect("failed to create temp worktree");

        let policy = WorktreePolicy::new(temp_dir.clone());

        // Create a file inside the worktree.
        let existing_file = temp_dir.join("existing.txt");
        let mut f = fs::File::create(&existing_file).expect("failed to create test file");
        f.write_all(b"test").expect("failed to write test file");

        // Also test a not-yet-created file under the worktree.
        let not_yet_created = temp_dir.join("not_yet_created.txt");

        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "locations".into(),
            serde_json::json!([
                { "path": existing_file.to_string_lossy().to_string() },
                { "path": not_yet_created.to_string_lossy().to_string() },
            ]),
        );

        let params = RequestPermissionParams {
            session_id: "s".into(),
            options: vec![PermissionOption {
                option_id: "proceed_once".into(),
                name: "Allow".into(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            tool_call: ToolCall {
                tool_call_id: "t".into(),
                status: None,
                title: None,
                kind: None,
                extra,
            },
        };
        let ctx = PermissionRequestContext {
            session_id: &params.session_id,
            working_dir: &temp_dir,
            tool_call: &params.tool_call,
            options: &params.options,
        };

        let decision = policy.decide(&ctx);
        assert!(decision.allow, "should allow paths inside worktree");
        assert_eq!(decision.option_id.as_deref(), Some("proceed_once"));

        // Clean up.
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_worktree_policy_allows_missing_locations() {
        let worktree = PathBuf::from("/tmp/makina-worktrees/task-test");
        let policy = WorktreePolicy::new(worktree.clone());

        let params = RequestPermissionParams {
            session_id: "s".into(),
            options: vec![PermissionOption {
                option_id: "proceed_once".into(),
                name: "Allow".into(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            tool_call: ToolCall {
                tool_call_id: "t".into(),
                status: None,
                title: None,
                kind: None,
                extra: Default::default(), // No "locations" key
            },
        };
        let ctx = PermissionRequestContext {
            session_id: &params.session_id,
            working_dir: &worktree,
            tool_call: &params.tool_call,
            options: &params.options,
        };

        let decision = policy.decide(&ctx);
        assert!(decision.allow, "should allow when locations are missing");
        assert_eq!(decision.option_id.as_deref(), Some("proceed_once"));
    }
}
