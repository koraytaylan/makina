//! LLM-driven project-discovery pass.
//!
//! Runs one model pass over a repository's manifests and prose files, then
//! deterministically parses the model's JSON into a [`DiscoveryResult`] —
//! proposed gate commands and per-role constraint instructions.
//!
//! # Design
//!
//! This mirrors the one-shot shape of [`crate::interpreter::ModelInterpreter`]:
//! one session is spawned, one prompt is sent, the text stream is drained to
//! [`crate::backend::ResponseEvent::TurnComplete`], and the session is
//! terminated.  The deterministic seam is [`parse_discovery_result`], which can
//! be unit-tested without a real model.

use std::collections::BTreeMap;
use std::path::Path;

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::backend::{AgentBackend, BackendError, Prompt, ResponseEvent, SessionConfig};

// ── Result types ───────────────────────────────────────────────────────────────

/// A single gate discovered by the LLM pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredGate {
    /// Human-readable name for the gate (e.g. `"tests"`, `"lint"`).
    pub name: String,
    /// Shell command that MUST exit 0 for the gate to pass.
    pub command: String,
}

/// The parsed result of a project-discovery pass.
///
/// Produced by [`parse_discovery_result`] and returned by [`discover_project`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DiscoveryResult {
    /// Gate commands proposed for this project.
    #[serde(default)]
    pub gates: Vec<DiscoveredGate>,
    /// Role-specific constraint prose, keyed by role name
    /// (`"developer"`, `"reviewer"`, `"planner"`).
    #[serde(default)]
    pub role_constraints: BTreeMap<String, String>,
}

// ── Repo context ───────────────────────────────────────────────────────────────

/// The textual context gathered from a repo for the discovery prompt, plus
/// the list of files actually read (used to populate the `[discovery]` stamp).
pub struct RepoContext {
    /// Concatenated, labelled file contents for the discovery prompt.
    pub text: String,
    /// Relative filenames (from `repo_root`) that were found and read.
    pub scanned_files: Vec<String>,
}

/// Maximum bytes read per file during repo scanning.
const FILE_BYTE_CAP: usize = 8_192;

/// Gather repository context for the discovery prompt.
///
/// Reads a bounded set of files — manifests (`Cargo.toml`, `package.json`,
/// `pyproject.toml`, `go.mod`, `Makefile`) and prose (`README*`,
/// `CONTRIBUTING*`, `AGENTS*`) — from `repo_root`.  Each file is truncated to
/// [`FILE_BYTE_CAP`] bytes and labelled with its filename.
///
/// Pure + IO-light: only existence checks and file reads.
pub fn gather_repo_context(repo_root: &Path) -> RepoContext {
    // Fixed manifest candidates (exact names).
    let manifest_names = [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "Makefile",
    ];

    // Glob-style prefix candidates (we scan the directory for matches).
    let prose_prefixes = ["README", "CONTRIBUTING", "AGENTS"];

    let mut text = String::new();
    let mut scanned_files = Vec::new();

    // Read exact-named manifests.
    for name in &manifest_names {
        let path = repo_root.join(name);
        if path.is_file()
            && let Ok(contents) = std::fs::read(&path)
        {
            let truncated = truncate_bytes(&contents, FILE_BYTE_CAP);
            text.push_str(&format!("=== {name} ===\n{truncated}\n\n"));
            scanned_files.push(name.to_string());
        }
    }

    // Scan the root directory for prose files matching known prefixes.
    if let Ok(entries) = std::fs::read_dir(repo_root) {
        let mut prose_entries: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let file_name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_file() && prose_prefixes.iter().any(|p| file_name.starts_with(p)) {
                    Some(file_name)
                } else {
                    None
                }
            })
            .collect();
        prose_entries.sort();

        for name in prose_entries {
            let path = repo_root.join(&name);
            if let Ok(contents) = std::fs::read(&path) {
                let truncated = truncate_bytes(&contents, FILE_BYTE_CAP);
                text.push_str(&format!("=== {name} ===\n{truncated}\n\n"));
                scanned_files.push(name);
            }
        }
    }

    RepoContext {
        text,
        scanned_files,
    }
}

/// Truncate raw bytes to `cap` and convert to UTF-8 (lossy).
fn truncate_bytes(bytes: &[u8], cap: usize) -> String {
    let slice = if bytes.len() > cap {
        &bytes[..cap]
    } else {
        bytes
    };
    String::from_utf8_lossy(slice).into_owned()
}

// ── System prompt ──────────────────────────────────────────────────────────────

/// System prompt for the project-discovery agent.
///
/// Instructs the model to emit ONLY a JSON object matching [`DiscoveryResult`] —
/// no prose, no markdown fences.  Mirrors the strictness of
/// [`crate::interpreter::PLANNER_SYSTEM_PROMPT`].
pub const DISCOVERY_SYSTEM_PROMPT: &str = "\
You are the Project-Discovery agent in Makina, a multi-agent software-factory \
orchestrator. Your sole function is to inspect a repository's files and propose \
quality gates and per-role constraint instructions.

Output ONLY a JSON object — no prose, no markdown fences, no explanation. The \
schema is:

{
  \"gates\": [
    { \"name\": \"<short name>\", \"command\": \"<shell command that must exit 0>\" }
  ],
  \"role_constraints\": {
    \"developer\": \"<short prose constraint for the Developer role>\",
    \"reviewer\": \"<short prose constraint for the Reviewer role>\",
    \"planner\": \"<short prose constraint for the Planner role>\"
  }
}

Rules:
- `gates` lists quality-gate commands that should run after every Developer turn \
(e.g. test runner, linter, formatter check). Omit a gate if no such tool is \
present.
- `role_constraints` values are short prose instructions appended to each role's \
built-in system prompt. Omit a role key if you have no specific constraint.
- Output ONLY the JSON object. No surrounding text. No code fences.";

// ── Error type ─────────────────────────────────────────────────────────────────

/// Errors returned by [`parse_discovery_result`] and [`discover_project`].
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// No JSON object (`{ … }`) was found in the model output.
    #[error("no JSON object found in discovery model output")]
    NoJsonObject,

    /// The JSON object was found but `serde_json` could not deserialize it into
    /// [`DiscoveryResult`].
    #[error("failed to deserialize discovery JSON: {0}")]
    Deserialize(#[from] serde_json::Error),

    /// A backend error occurred while spawning the session or prompting.
    #[error("backend error during discovery: {0}")]
    Backend(#[from] BackendError),
}

// ── Parser ─────────────────────────────────────────────────────────────────────

/// Parse raw model output into a [`DiscoveryResult`].
///
/// Uses [`crate::json::extract_json_object`] to strip prose/fences before
/// handing the extracted JSON to `serde_json`.
///
/// # Errors
///
/// - [`DiscoveryError::NoJsonObject`] — no `{ … }` found in `raw`.
/// - [`DiscoveryError::Deserialize`] — JSON found but not a valid `DiscoveryResult`.
pub fn parse_discovery_result(raw: &str) -> Result<DiscoveryResult, DiscoveryError> {
    let json = crate::json::extract_json_object(raw).ok_or(DiscoveryError::NoJsonObject)?;
    serde_json::from_str(json).map_err(DiscoveryError::Deserialize)
}

// ── Discovery pass ─────────────────────────────────────────────────────────────

/// Run one model pass over the repository and return the parsed [`DiscoveryResult`]
/// together with the list of scanned files.
///
/// Mirrors [`crate::interpreter::ModelInterpreter::interpret`]:
/// 1. Gather repo context via [`gather_repo_context`].
/// 2. Spawn a session with [`DISCOVERY_SYSTEM_PROMPT`].
/// 3. Send one prompt embedding the repo context.
/// 4. Drain [`ResponseEvent::TextChunk`] events until [`ResponseEvent::TurnComplete`].
/// 5. Terminate the session.
/// 6. Parse the accumulated text via [`parse_discovery_result`].
///
/// # Errors
///
/// Returns [`DiscoveryError::Backend`] on session or transport failures,
/// [`DiscoveryError::NoJsonObject`] or [`DiscoveryError::Deserialize`] on parse
/// failures.
pub async fn discover_project(
    backend: &dyn AgentBackend,
    repo_root: &Path,
) -> Result<(DiscoveryResult, Vec<String>), DiscoveryError> {
    let ctx = gather_repo_context(repo_root);

    let cfg = SessionConfig {
        working_dir: repo_root.to_path_buf(),
        system_prompt: DISCOVERY_SYSTEM_PROMPT.to_string(),
        mode: None,
        model: None,
        effort: None,
        extra: None,
        task_id: None,
        run_id: String::new(),
    };
    let mut session = backend.spawn(cfg).await?;

    let prompt_text = format!(
        "Inspect this repository and output ONLY the discovery JSON:\n\n{}",
        ctx.text
    );
    let mut stream = session.prompt(Prompt::new(prompt_text)).await?;

    let mut raw = String::new();
    while let Some(item) = stream.next().await {
        match item? {
            ResponseEvent::TextChunk { text } => raw.push_str(&text),
            ResponseEvent::TurnComplete { .. } => break,
            // Side-channel events are ignored.
            ResponseEvent::ThoughtChunk { .. }
            | ResponseEvent::ToolCall { .. }
            | ResponseEvent::ToolCallUpdate { .. }
            | ResponseEvent::CurrentModeUpdate { .. } => {}
        }
    }
    drop(stream);
    let _ = session.terminate().await;

    Ok((parse_discovery_result(&raw)?, ctx.scanned_files))
}

// ── Apply discovery to config ──────────────────────────────────────────────────

/// Apply a [`DiscoveryResult`] to project and role configurations.
///
/// Updates `project` and `roles` in place:
/// 1. Removes any existing `source == Some("discovered")` gates from `project.gates`.
/// 2. Appends the new discovered gates with `source = Some("discovered")`.
/// 3. For each role constraint in `result.role_constraints`, folds it into that
///    role's `system_prompt` via append (the default from task 0073).
/// 4. Sets the `[discovery]` stamp with the current RFC3339 timestamp and scanned files.
///
/// Existing manual gates (without a source) are preserved. Unset roles are created
/// with a default `RoleAssignment` to hold the constraint.
pub fn apply_discovery(
    project: &mut crate::config::ProjectConfigWrite,
    roles: &mut crate::config::RolesConfig,
    result: &DiscoveryResult,
    now_rfc3339: &str,
    scanned: &[String],
) {
    // Remove any previously discovered gates (source == "discovered").
    project
        .gates
        .retain(|g| g.source.as_deref() != Some("discovered"));

    // Add the newly discovered gates.
    for gate in &result.gates {
        project.gates.push(crate::config::GateConfig {
            name: gate.name.clone(),
            command: gate.command.clone(),
            image: None,
            source: Some("discovered".to_string()),
        });
    }

    // Fold role constraints into role assignments (append mode).
    for (role_name, constraint) in &result.role_constraints {
        // Get or create a RoleAssignment for this role.
        let assignment = match role_name.as_str() {
            "developer" => {
                if roles.developer.is_none() {
                    roles.developer = Some(crate::config::RoleAssignment::default());
                }
                roles.developer.as_mut().unwrap()
            }
            "reviewer" => {
                if roles.reviewer.is_none() {
                    roles.reviewer = Some(crate::config::RoleAssignment::default());
                }
                roles.reviewer.as_mut().unwrap()
            }
            "planner" => {
                if roles.planner.is_none() {
                    roles.planner = Some(crate::config::RoleAssignment::default());
                }
                roles.planner.as_mut().unwrap()
            }
            _ => continue, // Ignore unknown roles.
        };

        // Combine the constraint with any existing system_prompt via append.
        let combined = if let Some(ref existing) = assignment.system_prompt {
            format!("{}\n\n{}", existing, constraint)
        } else {
            constraint.clone()
        };
        assignment.system_prompt = Some(combined);
        // Ensure mode is not set (defaults to append).
        assignment.system_prompt_mode = None;
    }

    // Set the discovery stamp.
    project.discovery = Some(crate::config::DiscoveryStamp {
        last_run: now_rfc3339.to_string(),
        scanned_files: scanned.to_vec(),
    });
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{AgentBackend, AgentSession, BackendError, ResponseStream, SessionConfig};
    use async_trait::async_trait;
    use futures::stream;

    // ── Stub backend ──────────────────────────────────────────────────────────

    /// A stub backend whose session yields a fixed JSON text chunk then TurnComplete.
    struct FixedJsonBackend {
        json: String,
    }

    struct FixedJsonSession {
        json: String,
        terminated: bool,
    }

    #[async_trait]
    impl AgentBackend for FixedJsonBackend {
        async fn spawn(
            &self,
            _config: SessionConfig,
        ) -> Result<Box<dyn AgentSession>, BackendError> {
            Ok(Box::new(FixedJsonSession {
                json: self.json.clone(),
                terminated: false,
            }))
        }
    }

    #[async_trait]
    impl AgentSession for FixedJsonSession {
        async fn prompt(&mut self, _prompt: Prompt) -> Result<ResponseStream, BackendError> {
            if self.terminated {
                return Err(BackendError::Terminated);
            }
            let json = self.json.clone();
            let events: Vec<Result<ResponseEvent, BackendError>> = vec![
                Ok(ResponseEvent::TextChunk { text: json }),
                Ok(ResponseEvent::TurnComplete { usage: None }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }

        async fn terminate(&mut self) -> Result<(), BackendError> {
            self.terminated = true;
            Ok(())
        }
    }

    // ── parse_discovery_result tests ──────────────────────────────────────────

    #[test]
    fn parse_discovery_result_from_json() {
        let json = r#"{"gates":[{"name":"tests","command":"cargo test"}],"role_constraints":{"developer":"use workspace lints"}}"#;
        let result = parse_discovery_result(json).expect("should parse valid JSON");
        assert_eq!(result.gates.len(), 1);
        assert_eq!(result.gates[0].name, "tests");
        assert_eq!(result.gates[0].command, "cargo test");
        assert_eq!(
            result.role_constraints.get("developer").map(|s| s.as_str()),
            Some("use workspace lints")
        );
    }

    #[test]
    fn malformed_response_errors_cleanly() {
        // No JSON at all → NoJsonObject
        let err = parse_discovery_result("no json here").unwrap_err();
        assert!(
            matches!(err, DiscoveryError::NoJsonObject),
            "expected NoJsonObject, got: {err}"
        );

        // Valid JSON but wrong shape (gates is an integer, not an array) → Deserialize
        let err = parse_discovery_result(r#"{"gates":3}"#).unwrap_err();
        assert!(
            matches!(err, DiscoveryError::Deserialize(_)),
            "expected Deserialize error for bad shape, got: {err}"
        );
    }

    // ── discover_project (stub backend) ──────────────────────────────────────

    #[tokio::test]
    async fn discover_project_uses_backend_stub() {
        // Prepare a temp directory to act as repo root.
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path();

        // Write a minimal Cargo.toml so gather_repo_context has something to scan.
        std::fs::write(
            repo_root.join("Cargo.toml"),
            "[package]\nname = \"stub\"\nversion = \"0.1.0\"\n",
        )
        .expect("write Cargo.toml");

        let fixed_json = r#"{"gates":[{"name":"build","command":"cargo build"}],"role_constraints":{"reviewer":"be strict"}}"#;
        let backend = FixedJsonBackend {
            json: fixed_json.to_string(),
        };

        let (result, scanned) = discover_project(&backend, repo_root)
            .await
            .expect("discover_project should succeed");

        // Result matches the stub output.
        assert_eq!(result.gates.len(), 1);
        assert_eq!(result.gates[0].name, "build");
        assert_eq!(result.gates[0].command, "cargo build");
        assert_eq!(
            result.role_constraints.get("reviewer").map(|s| s.as_str()),
            Some("be strict")
        );

        // scanned_files reflects gather_repo_context (at least Cargo.toml).
        assert!(
            scanned.contains(&"Cargo.toml".to_string()),
            "scanned_files should include Cargo.toml, got: {scanned:?}"
        );
    }

    // ── gather_repo_context ───────────────────────────────────────────────────

    #[test]
    fn gather_repo_context_reads_manifests_and_prose() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"t\"\n").unwrap();
        std::fs::write(root.join("README.md"), "# Test repo\n").unwrap();

        let ctx = gather_repo_context(root);
        assert!(ctx.scanned_files.contains(&"Cargo.toml".to_string()));
        assert!(ctx.scanned_files.contains(&"README.md".to_string()));
        assert!(ctx.text.contains("=== Cargo.toml ==="));
        assert!(ctx.text.contains("=== README.md ==="));
    }

    #[test]
    fn gather_repo_context_empty_dir_returns_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ctx = gather_repo_context(tmp.path());
        assert!(ctx.scanned_files.is_empty());
        assert!(ctx.text.is_empty() || ctx.text.trim().is_empty());
    }

    #[test]
    fn gather_repo_context_truncates_large_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // Write more than FILE_BYTE_CAP bytes.
        let large_content = "x".repeat(FILE_BYTE_CAP * 2);
        std::fs::write(root.join("README.md"), &large_content).unwrap();

        let ctx = gather_repo_context(root);
        // The text block for README.md should not exceed cap + overhead.
        assert!(ctx.text.len() < large_content.len() + 100);
    }

    // ── Task 0075 acceptance criterion tests ─────────────────────────────────

    /// A stub backend that panics if `spawn` is called.
    /// Used to verify that discovery is skipped when the repo is already stamped.
    struct PanicBackend;

    #[async_trait]
    impl AgentBackend for PanicBackend {
        async fn spawn(
            &self,
            _config: SessionConfig,
        ) -> Result<Box<dyn AgentSession>, BackendError> {
            panic!("PanicBackend::spawn called — discovery should not have run");
        }
    }

    /// Helper: simulate the "first-open auto-run" path:
    /// read project config, if not stamped → run discovery + apply + write.
    async fn auto_run_discovery_if_needed(repo_root: &std::path::Path, backend: &dyn AgentBackend) {
        use crate::config::{ProjectConfig, ProjectConfigWrite, RolesConfig};
        use crate::paths::config_file;

        let config_path = config_file(repo_root);

        // Skip if no config file exists (matches orchestrator behaviour).
        if !config_path.exists() {
            return;
        }

        // Read the current project config.
        let project_config: ProjectConfig = match tokio::fs::read_to_string(&config_path).await {
            Ok(s) => match ProjectConfig::from_toml_str(&s, "project") {
                Ok(cfg) => cfg,
                Err(_) => return,
            },
            Err(_) => return,
        };

        // Skip if already stamped.
        if project_config.discovery.is_some() {
            return;
        }

        // Run discovery.
        let (result, scanned_files) = match discover_project(backend, repo_root).await {
            Ok(r) => r,
            Err(_) => return, // Non-fatal.
        };

        let mut write_config =
            ProjectConfigWrite::from_project_and_roles(project_config, RolesConfig::default());
        let mut roles = RolesConfig::default();

        let now = "2026-01-01T00:00:00Z".to_string();
        apply_discovery(&mut write_config, &mut roles, &result, &now, &scanned_files);

        let _ = crate::config::write_project_config(repo_root, |cfg| {
            *cfg = write_config;
        })
        .await;
    }

    /// First open of an un-stamped repo: discovery runs and writes discovered gates
    /// + a `[discovery]` stamp to the config file.
    #[tokio::test]
    async fn first_open_runs_and_writes_discovery() {
        use crate::config::ProjectConfig;
        use crate::paths::config_file;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path();

        // Write a minimal project config with NO [discovery] stamp.
        let config_dir = repo_root.join(".makina");
        std::fs::create_dir_all(&config_dir).expect("create .makina dir");
        std::fs::write(
            config_dir.join("config.toml"),
            r#"base_branch = "main"

[[gates]]
name = "manual"
command = "echo ok"
"#,
        )
        .expect("write initial config");

        // Write a Cargo.toml so the repo context scanner has something to read.
        std::fs::write(repo_root.join("Cargo.toml"), "[package]\nname=\"stub\"\n")
            .expect("write Cargo.toml");

        let fixed_json = r#"{"gates":[{"name":"tests","command":"cargo test"}],"role_constraints":{"developer":"use workspace lints"}}"#;
        let backend = FixedJsonBackend {
            json: fixed_json.to_string(),
        };

        // Run the auto-discovery path.
        auto_run_discovery_if_needed(repo_root, &backend).await;

        // Read back the config and verify the discovery results were written.
        let config_str = std::fs::read_to_string(config_file(repo_root)).expect("read config");
        let project: ProjectConfig =
            ProjectConfig::from_toml_str(&config_str, "project").expect("parse config");

        // The [discovery] stamp must be present.
        let stamp = project.discovery.expect("discovery stamp must be present");
        assert_eq!(stamp.last_run, "2026-01-01T00:00:00Z");
        assert!(
            stamp.scanned_files.contains(&"Cargo.toml".to_string()),
            "scanned_files must contain Cargo.toml, got: {:?}",
            stamp.scanned_files
        );

        // The discovered gate must be present in the config.
        let has_discovered_gate = project
            .gates
            .iter()
            .any(|g| g.name == "tests" && g.source.as_deref() == Some("discovered"));
        assert!(
            has_discovered_gate,
            "discovered gate 'tests' must be in config, gates: {:?}",
            project.gates
        );
    }

    /// Second open of a stamped repo: discovery is skipped entirely; the stub backend
    /// is NOT spawned.
    #[tokio::test]
    async fn second_open_skips_when_stamped() {
        use crate::config::ProjectConfig;
        use crate::paths::config_file;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path();

        // Write a config that already has a [discovery] stamp.
        let config_dir = repo_root.join(".makina");
        std::fs::create_dir_all(&config_dir).expect("create .makina dir");
        std::fs::write(
            config_dir.join("config.toml"),
            r#"base_branch = "main"

[[gates]]
name = "tests"
command = "cargo test"
source = "discovered"

[discovery]
last_run = "2025-12-31T00:00:00Z"
scanned_files = ["Cargo.toml"]
"#,
        )
        .expect("write stamped config");

        // PanicBackend will panic if spawned — verifying backend is NOT called.
        let backend = PanicBackend;

        // This must complete without panicking (discovery should be skipped).
        auto_run_discovery_if_needed(repo_root, &backend).await;

        // Verify the config is unchanged (stamp still has original last_run).
        let config_str = std::fs::read_to_string(config_file(repo_root)).expect("read config");
        let project: ProjectConfig =
            ProjectConfig::from_toml_str(&config_str, "project").expect("parse config");

        let stamp = project
            .discovery
            .expect("discovery stamp must still be present");
        assert_eq!(
            stamp.last_run, "2025-12-31T00:00:00Z",
            "stamp must not be overwritten when already present"
        );
    }

    /// Force re-run with DiscoverProject: even when stamped, discovery re-runs,
    /// discovered gates are replaced (not duplicated), and last_run is updated.
    #[tokio::test]
    async fn force_rerun_overwrites() {
        use crate::config::{ProjectConfig, ProjectConfigWrite, RolesConfig};
        use crate::paths::config_file;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_root = tmp.path();

        // Write a config with an existing discovered gate + stamp.
        let config_dir = repo_root.join(".makina");
        std::fs::create_dir_all(&config_dir).expect("create .makina dir");
        std::fs::write(
            config_dir.join("config.toml"),
            r#"base_branch = "main"

[[gates]]
name = "old-gate"
command = "echo old"
source = "discovered"

[discovery]
last_run = "2025-12-31T00:00:00Z"
scanned_files = ["old.toml"]
"#,
        )
        .expect("write initial stamped config");

        // New discovery returns a different gate.
        let new_json =
            r#"{"gates":[{"name":"new-gate","command":"cargo test"}],"role_constraints":{}}"#;
        let backend = FixedJsonBackend {
            json: new_json.to_string(),
        };

        // Simulate force re-run (ignores the stamp, always runs).
        let project_config: ProjectConfig = {
            let s = tokio::fs::read_to_string(config_file(repo_root))
                .await
                .expect("read config");
            ProjectConfig::from_toml_str(&s, "project").expect("parse config")
        };

        let (result, scanned_files) = discover_project(&backend, repo_root)
            .await
            .expect("discovery should succeed");

        let mut write_config =
            ProjectConfigWrite::from_project_and_roles(project_config, RolesConfig::default());
        let mut roles = RolesConfig::default();

        let new_now = "2026-06-01T12:00:00Z";
        apply_discovery(
            &mut write_config,
            &mut roles,
            &result,
            new_now,
            &scanned_files,
        );

        crate::config::write_project_config(repo_root, |cfg| {
            *cfg = write_config;
        })
        .await
        .expect("write should succeed");

        // Read back and verify.
        let config_str = std::fs::read_to_string(config_file(repo_root)).expect("read config");
        let project: ProjectConfig =
            ProjectConfig::from_toml_str(&config_str, "project").expect("parse config");

        // The old discovered gate must be gone; the new one present.
        let old_present = project.gates.iter().any(|g| g.name == "old-gate");
        assert!(!old_present, "old-gate must be replaced by force re-run");

        let new_present = project
            .gates
            .iter()
            .any(|g| g.name == "new-gate" && g.source.as_deref() == Some("discovered"));
        assert!(new_present, "new-gate must be present after force re-run");

        // The stamp must be updated.
        let stamp = project.discovery.expect("discovery stamp must be present");
        assert_eq!(stamp.last_run, new_now, "last_run must be updated");
    }

    /// write_project_config preserves manual gates alongside discovered ones.
    /// A manual gate (no source) must survive a discovery write.
    #[test]
    fn writer_preserves_manual_gates() {
        use crate::config::{GateConfig, ProjectConfigWrite, RolesConfig};

        // Start with a ProjectConfigWrite that has a manual gate.
        let mut write_config = ProjectConfigWrite {
            gates: vec![GateConfig {
                name: "manual".to_string(),
                command: "echo ok".to_string(),
                image: None,
                source: None, // Manual gate — no source.
            }],
            base_branch: "main".to_string(),
            caps: None,
            concurrency: None,
            discovery: None,
            roles: RolesConfig::default(),
            merge: None,
        };
        let mut roles = RolesConfig::default();

        // Simulate a discovery result with one discovered gate.
        let result = DiscoveryResult {
            gates: vec![DiscoveredGate {
                name: "tests".to_string(),
                command: "cargo test".to_string(),
            }],
            role_constraints: std::collections::BTreeMap::new(),
        };

        apply_discovery(
            &mut write_config,
            &mut roles,
            &result,
            "2026-01-01T00:00:00Z",
            &["Cargo.toml".to_string()],
        );

        // The manual gate must still be present.
        let manual_present = write_config
            .gates
            .iter()
            .any(|g| g.name == "manual" && g.source.is_none());
        assert!(
            manual_present,
            "manual gate must be preserved, gates: {:?}",
            write_config.gates
        );

        // The discovered gate must also be present.
        let discovered_present = write_config
            .gates
            .iter()
            .any(|g| g.name == "tests" && g.source.as_deref() == Some("discovered"));
        assert!(
            discovered_present,
            "discovered gate must be present, gates: {:?}",
            write_config.gates
        );
    }
}
