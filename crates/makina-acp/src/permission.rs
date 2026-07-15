//! Permission policy seam for ACP `session/request_permission` requests.
//!
//! This module (task `worktree-permission-policy`) defines the pluggable
//! decision interface and the default implementation: a fail-closed,
//! worktree-scoped policy that selects the offered `allow_once` option only
//! when every declared tool location resolves inside the assigned worktree.
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

/// The default policy: auto-allow (picking the offered `allow_once` option for
/// least privilege) only when the request's working directory and every
/// declared tool location resolve inside the per-task worktree this policy was
/// constructed with.
///
/// Construction: `WorktreePolicy::new(session_config.working_dir)` — the same
/// value passed to `AcpBackend::spawn`.
///
/// Missing, empty, or malformed `locations` metadata is denied. This is still
/// a protocol permission boundary rather than an OS sandbox: a compromised or
/// non-conforming subprocess could perform filesystem access without asking.
#[derive(Debug, Clone)]
pub struct WorktreePolicy {
    worktree: PathBuf,
}

/// Resolve a declared location component-by-component beneath `worktree`.
/// Existing components are canonicalized so symlink escapes are caught; a
/// not-yet-created suffix is retained lexically so creating a new in-worktree
/// file remains possible.
fn resolve_under(target: &str, worktree: &Path) -> Result<PathBuf, String> {
    if target.trim().is_empty() {
        return Err("location path must not be empty".into());
    }

    let canonical_worktree = std::fs::canonicalize(worktree)
        .map_err(|err| format!("configured worktree cannot be canonicalized: {err}"))?;
    if !canonical_worktree.is_dir() {
        return Err("configured worktree is not a directory".into());
    }

    let target_path = Path::new(target);
    let relative = if target_path.is_absolute() {
        // Accept both the canonical spelling and the spelling supplied when the
        // policy was constructed (which may itself pass through a symlink).
        target_path
            .strip_prefix(&canonical_worktree)
            .or_else(|_| target_path.strip_prefix(worktree))
            .map_err(|_| "absolute location is outside the configured worktree".to_string())?
    } else {
        target_path
    };

    let mut resolved = canonical_worktree.clone();
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if resolved == canonical_worktree {
                    return Err("location traverses above the configured worktree".into());
                }
                resolved.pop();
            }
            std::path::Component::Normal(part) => {
                resolved.push(part);
                match std::fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        resolved = std::fs::canonicalize(&resolved).map_err(|err| {
                            format!("location contains an unresolvable filesystem entry: {err}")
                        })?;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        // New path component. Later `..` components are still
                        // handled above, and any return to existing territory is
                        // canonicalized on the next normal component.
                    }
                    Err(err) => {
                        return Err(format!("location cannot be inspected: {err}"));
                    }
                }
            }
            // `relative` cannot legitimately contain an absolute-path prefix.
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err("location has an invalid path prefix".into());
            }
        }

        if !resolved.starts_with(&canonical_worktree) {
            return Err("location resolves outside the configured worktree".into());
        }
    }

    Ok(resolved)
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
        let canonical_worktree = std::fs::canonicalize(&self.worktree);
        let canonical_working_dir = std::fs::canonicalize(ctx.working_dir);
        if canonical_worktree.as_ref().ok() == canonical_working_dir.as_ref().ok()
            && canonical_worktree.is_ok()
        {
            let Some(locations_value) = ctx.tool_call.extra.get("locations") else {
                return PermissionDecision {
                    allow: false,
                    option_id: None,
                    reason: "tool call is missing required locations metadata".into(),
                };
            };
            let Some(locations) = locations_value.as_array() else {
                return PermissionDecision {
                    allow: false,
                    option_id: None,
                    reason: "tool call locations metadata must be an array".into(),
                };
            };
            if locations.is_empty() {
                return PermissionDecision {
                    allow: false,
                    option_id: None,
                    reason: "tool call locations metadata must not be empty".into(),
                };
            }

            for (index, location) in locations.iter().enumerate() {
                let Some(path) = location.get("path").and_then(|value| value.as_str()) else {
                    return PermissionDecision {
                        allow: false,
                        option_id: None,
                        reason: format!("tool call location {index} must contain a string path"),
                    };
                };
                if let Err(reason) = resolve_under(path, &self.worktree) {
                    tracing::warn!(
                        path,
                        worktree = ?self.worktree,
                        %reason,
                        "tool-call location denied"
                    );
                    return PermissionDecision {
                        allow: false,
                        option_id: None,
                        reason: format!("tool call location {index} is unsafe: {reason}"),
                    };
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

        let reason = match (canonical_worktree, canonical_working_dir) {
            (Err(err), _) => format!("configured worktree cannot be canonicalized: {err}"),
            (_, Err(err)) => format!("working_dir cannot be canonicalized: {err}"),
            (Ok(configured), Ok(actual)) => {
                format!("working_dir {actual:?} is not the configured worktree {configured:?}")
            }
        };
        PermissionDecision {
            allow: false,
            option_id: None,
            reason,
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
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "locations".into(),
            serde_json::json!([{ "path": "example.txt" }]),
        );
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
                extra,
            },
        }
    }

    #[test]
    fn worktree_policy_returns_allow_with_allow_once_id_and_nonempty_reason() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().to_path_buf();
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
        let worktree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let policy = WorktreePolicy::new(worktree.path());
        let params = sample_params_with_three_options();

        let ctx = PermissionRequestContext {
            session_id: &params.session_id,
            working_dir: other.path(),
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
        let temp = tempfile::tempdir().unwrap();
        // This would fail to compile if PermissionPolicy were not object-safe.
        let policy: Arc<dyn PermissionPolicy> = Arc::new(WorktreePolicy::new(temp.path()));

        // Exercise it through the trait object to prove dynamic dispatch works.
        let params = sample_params_with_three_options();
        let ctx = PermissionRequestContext {
            session_id: &params.session_id,
            working_dir: temp.path(),
            tool_call: &params.tool_call,
            options: &params.options,
        };
        let d = policy.decide(&ctx);
        assert!(d.allow);
    }

    #[test]
    fn worktree_policy_no_allow_once_option_yields_deny() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().to_path_buf();
        let policy = WorktreePolicy::new(worktree.clone());

        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "locations".into(),
            serde_json::json!([{ "path": "file.txt" }]),
        );

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
        assert!(!decision.allow);
        assert!(decision.option_id.is_none());
        assert!(!decision.reason.is_empty());
    }

    #[test]
    fn test_worktree_policy_denies_paths_outside_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().to_path_buf();
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
        assert!(decision.reason.contains("outside the configured worktree"));
    }

    #[test]
    fn test_worktree_policy_allows_paths_inside_worktree() {
        use std::fs;
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let temp_dir = temp.path().to_path_buf();

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
    }

    #[test]
    fn test_worktree_policy_denies_missing_locations() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().to_path_buf();
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
        assert!(!decision.allow, "missing locations must fail closed");
        assert!(decision.reason.contains("missing required locations"));
    }

    #[test]
    fn worktree_policy_denies_empty_or_malformed_locations() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().to_path_buf();
        let policy = WorktreePolicy::new(&worktree);

        for (locations, expected_reason) in [
            (serde_json::Value::Null, "must be an array"),
            (serde_json::json!("file.txt"), "must be an array"),
            (serde_json::json!({"path": "file.txt"}), "must be an array"),
            (serde_json::json!([]), "must not be empty"),
            (serde_json::json!([{}]), "must contain a string path"),
            (
                serde_json::json!([{"path": 42}]),
                "must contain a string path",
            ),
            (serde_json::json!([{"path": ""}]), "path must not be empty"),
        ] {
            let mut params = sample_params_with_three_options();
            params.tool_call.extra.insert("locations".into(), locations);
            let decision = policy.decide(&PermissionRequestContext {
                session_id: &params.session_id,
                working_dir: &worktree,
                tool_call: &params.tool_call,
                options: &params.options,
            });
            assert!(!decision.allow, "malformed metadata must fail closed");
            assert!(
                decision.reason.contains(expected_reason),
                "expected {expected_reason:?} in {:?}",
                decision.reason
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn worktree_policy_denies_symlink_escape_and_allows_internal_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let internal = temp.path().join("internal");
        std::fs::create_dir(&internal).unwrap();
        symlink(outside.path(), temp.path().join("escape")).unwrap();
        symlink(&internal, temp.path().join("inside-link")).unwrap();
        let policy = WorktreePolicy::new(temp.path());

        for (path, allowed) in [("escape/new.txt", false), ("inside-link/new.txt", true)] {
            let mut params = sample_params_with_three_options();
            params
                .tool_call
                .extra
                .insert("locations".into(), serde_json::json!([{ "path": path }]));
            let decision = policy.decide(&PermissionRequestContext {
                session_id: &params.session_id,
                working_dir: temp.path(),
                tool_call: &params.tool_call,
                options: &params.options,
            });
            assert_eq!(decision.allow, allowed, "unexpected decision for {path}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn worktree_policy_denies_dangling_symlink_location() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        symlink("missing-target", temp.path().join("dangling")).unwrap();
        let policy = WorktreePolicy::new(temp.path());
        let mut params = sample_params_with_three_options();
        params.tool_call.extra.insert(
            "locations".into(),
            serde_json::json!([{ "path": "dangling/file.txt" }]),
        );

        let decision = policy.decide(&PermissionRequestContext {
            session_id: &params.session_id,
            working_dir: temp.path(),
            tool_call: &params.tool_call,
            options: &params.options,
        });
        assert!(!decision.allow);
        assert!(decision.reason.contains("unresolvable filesystem entry"));
    }
}
