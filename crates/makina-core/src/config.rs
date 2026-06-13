//! Two-layer TOML configuration for Makina.
//!
//! Makina uses two configuration files merged at startup, with the project
//! layer winning over the global layer:
//!
//! - **Global** `~/.makina/config.toml` — machine/user-level settings:
//!   the agent backend command, the Planner model and call mechanism, default
//!   termination caps, and max concurrency.
//! - **Project** `makina.toml` — repository-level settings (committed to
//!   version control): gates (toolchain-specific shell commands), base branch,
//!   and optional per-project overrides of caps and concurrency.
//!
//! # Usage pattern
//!
//! ```rust,no_run
//! use makina_core::config::Config;
//!
//! # fn main() -> Result<(), makina_core::config::ConfigError> {
//! // Load with default real paths.
//! let config = Config::load_defaults()?;
//! println!("backend command: {}", config.backend.command);
//! # Ok(())
//! # }
//! ```
//!
//! For testing, use [`GlobalConfig::from_toml_str`] and
//! [`ProjectConfig::from_toml_str`] with in-memory TOML strings, then
//! [`Config::resolve`] and [`Config::validate`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use crate::paths;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can occur while loading, parsing, or validating Makina's
/// configuration.
///
/// The variants provide specific, actionable messages so that operators can
/// quickly identify and fix configuration problems.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A TOML source string (global or project config) could not be parsed.
    ///
    /// The wrapped message is from the `toml` crate and includes the line/col
    /// of the first parse error.
    #[error("TOML parse error in {file}: {message}")]
    Parse {
        /// Human-readable label for the file that failed (e.g. `"~/.makina/config.toml"`).
        file: String,
        /// The underlying `toml::de::Error` message.
        message: String,
    },

    /// A config file could not be read from disk.
    #[error("I/O error reading `{path}`: {message}")]
    Io {
        /// Path that could not be read.
        path: String,
        /// The underlying `std::io::Error` message.
        message: String,
    },

    /// The merged, resolved config failed semantic validation.
    ///
    /// `reason` is a precise, human-readable description of the violation
    /// (e.g. `"backend.command must not be empty"`).
    #[error("invalid config: {reason}")]
    Validation {
        /// Description of the specific validation rule that was violated.
        reason: String,
    },
}

// ── Config load paths ─────────────────────────────────────────────────────────

/// The resolved file-system paths checked by [`Config::load_defaults`].
///
/// Returned by [`Config::load_defaults_with_paths`] so that callers (e.g. the
/// binary entry point) can include path information in user-facing error
/// messages — naming which file was checked, whether it existed, and which
/// layer wins on merge.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// Resolved path for the global config (`~/.makina/config.toml`).
    /// `None` when `$HOME` is unset.
    pub global: Option<PathBuf>,
    /// Resolved path for the project config (`.makina/config.toml` or legacy
    /// `./makina.toml`).
    ///
    /// **Invariant:** always `Some`. `resolve_project_config_path` defaults to
    /// the preferred `.makina/config.toml` path even when neither config file
    /// exists, so this field is never `None`. The `Option` wrapper is retained
    /// to mirror `global` and to allow callers to treat both fields uniformly.
    pub project: Option<PathBuf>,
}

// ── Backend config ────────────────────────────────────────────────────────────

/// Configuration for the external agent backend CLI.
///
/// The backend is spawned as a subprocess.  `command` is the binary (or shell
/// command) and `args` are the arguments passed to it.
///
/// # Defaults
///
/// `command` defaults to an empty string; [`Config::validate`] will reject a
/// config whose `backend.command` is empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BackendConfig {
    /// The binary or command to spawn for agent sessions (e.g. `"acp-cli"`).
    ///
    /// Must be non-empty after merging; [`Config::validate`] enforces this.
    pub command: String,

    /// Arguments to pass to `command` when spawning agent sessions.
    pub args: Vec<String>,
}

// ── Provider and Role config ──────────────────────────────────────────────────

/// Configuration for a named ACP provider.
///
/// A provider is an external agent backend (ACP CLI) that can be referenced
/// by name and assigned to roles. Each provider has a command, optional
/// arguments, and optional environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// The unique name of this provider (e.g. `"default"`, `"grok"`, `"claude"`).
    pub name: String,

    /// The binary or command to spawn for agent sessions.
    ///
    /// Must be non-empty after validation.
    pub command: String,

    /// Arguments to pass to `command` when spawning agent sessions.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables to set when spawning the backend.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Assignment of a role to a provider, with optional mode/model/effort defaults.
///
/// Specifies which provider (ACP backend) a role uses and what default
/// mode/model/effort selections it should apply when opening a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleAssignment {
    /// The name of the provider this role is assigned to.
    ///
    /// Must reference a declared provider in `GlobalConfig::providers`.
    pub provider: String,

    /// Optional default mode ID to apply when opening a session.
    #[serde(default)]
    pub mode: Option<String>,

    /// Optional default model option value to apply when opening a session.
    #[serde(default)]
    pub model: Option<String>,

    /// Optional default effort (thought_level) option value to apply when opening a session.
    #[serde(default)]
    pub effort: Option<String>,
}

/// Role-to-provider assignments and defaults.
///
/// Specifies which provider each role (Planner, Developer, Reviewer) uses
/// and what default mode/model/effort selections apply to each.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RolesConfig {
    /// Optional assignment for the Planner role.
    #[serde(default)]
    pub planner: Option<RoleAssignment>,

    /// Optional assignment for the Developer role.
    #[serde(default)]
    pub developer: Option<RoleAssignment>,

    /// Optional assignment for the Reviewer role.
    #[serde(default)]
    pub reviewer: Option<RoleAssignment>,
}

// ── Planner config ────────────────────────────────────────────────────────────

/// Planner model and call mechanism configuration.
///
/// Configures which model identifier the Planner uses and how it makes model
/// calls.  Decision finalised in `planner-model-mechanism` (task 18):
/// `OneShotAgent` is the implemented MVP path; `DirectApi` is deferred.
///
/// See `docs/spec/planner-model-mechanism.md` for the full decision and rationale.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlannerConfig {
    /// The model identifier used by the Planner (e.g. `"gemini-2.0-flash"`).
    ///
    /// Passed through to the agent backend when relevant.  Defaults to an
    /// empty string (the backend uses its own default model).
    pub model: String,

    /// How the Planner calls the model.  See [`PlannerMechanism`].
    ///
    /// Defaults to [`PlannerMechanism::OneShotAgent`] — the implemented MVP
    /// path.  Setting `DirectApi` returns a clear "not supported in MVP" error.
    pub mechanism: PlannerMechanism,
}

/// The mechanism the Planner uses to call the model.
///
/// Decision finalised in task 18 (`planner-model-mechanism`):
/// `OneShotAgent` is **implemented**; `DirectApi` is **deferred** (not
/// implemented in the MVP — selecting it returns a clear error, not a silent
/// fallback).
///
/// Serde strings are stable: `"one-shot-agent"` and `"direct-api"`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlannerMechanism {
    /// **Implemented (MVP path).** The Planner spawns a one-shot agent session
    /// via the `AgentBackend` trait, sends a single prompt, and collects the
    /// JSON response.
    ///
    /// Inherits the **Zed auth model**: the agent CLI is pre-authenticated by
    /// the user; Makina holds no credentials.  This is the default.
    #[default]
    OneShotAgent,

    /// **Deferred — not implemented in the MVP.**  The Planner would call the
    /// model API directly without an ACP subprocess.
    ///
    /// Selecting this variant returns
    /// [`crate::interpreter::InterpretError::MechanismNotSupported`] with a
    /// clear message.  Implementing it would require Makina to manage an API
    /// key — the one credential exception explicitly deferred by the
    /// architecture.  See `docs/spec/planner-model-mechanism.md` §5.
    DirectApi,
}

// ── Caps config ───────────────────────────────────────────────────────────────

/// Termination caps applied to every task.
///
/// These caps protect against runaway tasks.  All values must be ≥ 1;
/// [`Config::validate`] enforces this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CapsConfig {
    /// Maximum number of gate iterations before a task is failed.
    ///
    /// A gate iteration is one round of: Developer finishes → gates run → at
    /// least one gate fails → Developer is given another attempt.
    pub gate_iterations: u32,

    /// Maximum number of Reviewer feedback cycles before a task is failed.
    ///
    /// A reviewer iteration is one round of: Reviewer requests changes →
    /// Developer addresses them.
    pub reviewer_iterations: u32,

    /// Per-task wall-clock limit in seconds.
    ///
    /// If a task is still in-progress after this many seconds it is forcibly
    /// failed.
    pub wall_clock_secs: u64,

    /// Optional idle timeout in seconds.
    ///
    /// If set, a task step will be aborted if no output is received for this
    /// many seconds. Must be at least 1 if specified.
    #[serde(default)]
    pub idle_secs: Option<u64>,
}

impl Default for CapsConfig {
    fn default() -> Self {
        Self {
            gate_iterations: 5,
            reviewer_iterations: 5,
            wall_clock_secs: 1800, // 30 minutes
            idle_secs: None,
        }
    }
}

// ── GlobalConfig ──────────────────────────────────────────────────────────────

/// Returns the default concurrency (3) for serde field-level default.
fn default_concurrency() -> usize {
    3
}

/// User/machine-level configuration, stored at `~/.makina/config.toml`.
///
/// All sections are optional in the TOML file; absent sections fall back to
/// [`Default`] implementations.  This means a completely empty global config
/// file is valid at parse time (though it may fail [`Config::validate`] if, for
/// example, `backend.command` remains empty and no project override supplies it).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    /// The ACP agent CLI to spawn for Developer and Reviewer sessions.
    ///
    /// **Legacy field (back-compat).** When `providers` is empty but `backend` is set,
    /// the backend is synthesized into a provider named `"default"` during resolve.
    pub backend: BackendConfig,

    /// Named ACP providers available for role assignment.
    ///
    /// If empty at resolve time and a legacy `[backend]` section exists,
    /// one default provider is synthesized from the backend config.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,

    /// Role-to-provider assignments and per-role mode/model/effort defaults.
    #[serde(default)]
    pub roles: RolesConfig,

    /// Planner model and mechanism.  See [`PlannerConfig`].
    ///
    /// Defaults to `OneShotAgent` (implemented MVP path).
    pub planner: PlannerConfig,

    /// Termination caps applied to all tasks unless overridden by the project.
    pub caps: CapsConfig,

    /// Maximum number of tasks that may run concurrently.
    ///
    /// Defaults to `3`.  Must be ≥ 1 after merging.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            backend: BackendConfig::default(),
            providers: Vec::new(),
            roles: RolesConfig::default(),
            planner: PlannerConfig::default(),
            caps: CapsConfig::default(),
            concurrency: 3,
        }
    }
}

impl GlobalConfig {
    /// Parse a `GlobalConfig` from a TOML string.
    ///
    /// `source_label` is used only in error messages (e.g. `"~/.makina/config.toml"`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] if the TOML is malformed or contains
    /// type mismatches.
    pub fn from_toml_str(toml: &str, source_label: &str) -> Result<Self, ConfigError> {
        toml::from_str(toml).map_err(|e| ConfigError::Parse {
            file: source_label.to_string(),
            message: e.to_string(),
        })
    }
}

// ── ProjectConfig ─────────────────────────────────────────────────────────────

/// A single gate: a shell command that must exit 0 for a task to pass.
///
/// Gates are toolchain-specific and therefore belong in the project config
/// (`makina.toml`), not the global config.  They cannot be defaulted globally
/// because the commands differ per repository/language.
///
/// # Example
///
/// ```toml
/// [[gates]]
/// name    = "tests"
/// command = "cargo test --workspace"
///
/// [[gates]]
/// name    = "lint"
/// command = "cargo clippy --all-targets -- -D warnings"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct GateConfig {
    /// Short human-readable name for the gate (e.g. `"tests"`, `"lint"`).
    ///
    /// Must be non-empty; [`Config::validate`] enforces this.
    pub name: String,

    /// Shell command that must exit 0.  Run in the task's working directory.
    ///
    /// Must be non-empty; [`Config::validate`] enforces this.
    pub command: String,

    /// Optional Docker image to run the gate command inside.
    ///
    /// When absent, the gate runs directly on the host.
    #[serde(default)]
    pub image: Option<String>,
}

/// Optional per-field overrides of [`CapsConfig`] that a project can set.
///
/// Only the fields present in `makina.toml` override the global values; `None`
/// fields leave the global value unchanged.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CapsOverride {
    /// Override [`CapsConfig::gate_iterations`] if `Some`.
    pub gate_iterations: Option<u32>,

    /// Override [`CapsConfig::reviewer_iterations`] if `Some`.
    pub reviewer_iterations: Option<u32>,

    /// Override [`CapsConfig::wall_clock_secs`] if `Some`.
    pub wall_clock_secs: Option<u64>,

    /// Override [`CapsConfig::idle_secs`] if present in the TOML.
    ///
    /// When `Some`, this value overrides the global `idle_secs`.
    /// The inner `Option<u64>` allows specifying `idle_secs = None` explicitly
    /// to disable idle timeout for a project.
    pub idle_secs: Option<Option<u64>>,
}

/// Project-level configuration, stored at `makina.toml` in the repo root.
///
/// This file is typically committed to version control.  It specifies the
/// gates (toolchain-specific pass/fail commands), the base branch, and
/// optional overrides of global termination caps and concurrency.
///
/// All fields are optional in the TOML; absent fields use [`Default`].
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ProjectConfig {
    /// Gates that must pass for a task to be considered done.
    ///
    /// Evaluated in order; the first failure stops gate evaluation.
    pub gates: Vec<GateConfig>,

    /// The base branch for worktrees and pull requests.
    ///
    /// Defaults to `"develop"` during [`Config::resolve`] if left empty.
    pub base_branch: String,

    /// Optional per-field overrides of the global [`CapsConfig`].
    pub caps: Option<CapsOverride>,

    /// Optional override of the global `concurrency` setting.
    pub concurrency: Option<usize>,
}

impl ProjectConfig {
    /// Parse a `ProjectConfig` from a TOML string.
    ///
    /// `source_label` is used only in error messages (e.g. `"makina.toml"`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] if the TOML is malformed or contains
    /// type mismatches.
    pub fn from_toml_str(toml: &str, source_label: &str) -> Result<Self, ConfigError> {
        toml::from_str(toml).map_err(|e| ConfigError::Parse {
            file: source_label.to_string(),
            message: e.to_string(),
        })
    }
}

// ── Config (resolved/merged) ──────────────────────────────────────────────────

/// The resolved, merged configuration used at runtime.
///
/// This is the type the rest of the system uses.  Obtain it via
/// [`Config::load`] (which reads real files) or by composing
/// [`GlobalConfig::from_toml_str`] + [`ProjectConfig::from_toml_str`] +
/// [`Config::resolve`] + [`Config::validate`] (testable without I/O).
#[derive(Debug, Clone)]
pub struct Config {
    /// The agent backend CLI command and arguments (legacy field; use `providers` instead).
    pub backend: BackendConfig,

    /// Named ACP providers available for role assignment.
    pub providers: Vec<ProviderConfig>,

    /// Role-to-provider assignments and per-role mode/model/effort defaults.
    pub roles: RolesConfig,

    /// Planner configuration (model + mechanism).  See [`PlannerConfig`].
    pub planner: PlannerConfig,

    /// Effective termination caps (global defaults merged with project overrides).
    pub caps: CapsConfig,

    /// Effective maximum task concurrency.
    pub concurrency: usize,

    /// Gates that must exit 0 for a task to pass.  From the project layer only.
    pub gates: Vec<GateConfig>,

    /// Base branch for worktrees and pull requests.  From the project layer.
    pub base_branch: String,
}

impl Config {
    /// Merge a `GlobalConfig` and a `ProjectConfig` into a resolved `Config`.
    ///
    /// The **project layer wins**: any `Some` field in the project's `caps`
    /// override replaces the corresponding global value; `concurrency` is
    /// similarly overridden if present.  `gates` and `base_branch` always come
    /// from the project layer.
    ///
    /// **Back-compat:** if `providers` is empty but a legacy `[backend]` exists,
    /// a single provider named `"default"` is synthesized from the backend config,
    /// and any role not explicitly assigned is assigned to `"default"`.
    ///
    /// This function does **not** validate the result; call [`Config::validate`]
    /// after `resolve` to surface semantic errors.
    pub fn resolve(global: GlobalConfig, project: ProjectConfig) -> Self {
        // Merge caps: start with global, apply project overrides field-by-field.
        let caps = if let Some(ref ov) = project.caps {
            CapsConfig {
                gate_iterations: ov.gate_iterations.unwrap_or(global.caps.gate_iterations),
                reviewer_iterations: ov
                    .reviewer_iterations
                    .unwrap_or(global.caps.reviewer_iterations),
                wall_clock_secs: ov.wall_clock_secs.unwrap_or(global.caps.wall_clock_secs),
                idle_secs: ov.idle_secs.unwrap_or(global.caps.idle_secs),
            }
        } else {
            global.caps.clone()
        };

        // Project concurrency wins if present.
        let concurrency = project.concurrency.unwrap_or(global.concurrency);

        // base_branch: use project value if non-empty, else default.
        let base_branch = if project.base_branch.is_empty() {
            "develop".to_string()
        } else {
            project.base_branch
        };

        // ── Provider and role resolution with back-compat ──────────────────────

        // If providers is empty but backend.command is set, synthesize a default provider.
        let mut providers = global.providers.clone();
        let mut roles = global.roles.clone();

        if providers.is_empty() && !global.backend.command.is_empty() {
            // Synthesize a "default" provider from the legacy backend config.
            providers.push(ProviderConfig {
                name: "default".to_string(),
                command: global.backend.command.clone(),
                args: global.backend.args.clone(),
                env: BTreeMap::new(),
            });

            // Assign any role not explicitly set to "default".
            if roles.planner.is_none() {
                roles.planner = Some(RoleAssignment {
                    provider: "default".to_string(),
                    mode: None,
                    model: None,
                    effort: None,
                });
            }
            if roles.developer.is_none() {
                roles.developer = Some(RoleAssignment {
                    provider: "default".to_string(),
                    mode: None,
                    model: None,
                    effort: None,
                });
            }
            if roles.reviewer.is_none() {
                roles.reviewer = Some(RoleAssignment {
                    provider: "default".to_string(),
                    mode: None,
                    model: None,
                    effort: None,
                });
            }
        }

        Self {
            backend: global.backend,
            providers,
            roles,
            planner: global.planner,
            caps,
            concurrency,
            gates: project.gates,
            base_branch,
        }
    }

    /// Validate the resolved config for semantic correctness.
    ///
    /// Checks performed:
    ///
    /// - `backend.command` is non-empty (legacy field).
    /// - All provider commands are non-empty.
    /// - All provider names are unique.
    /// - All `RoleAssignment.provider` names reference declared providers.
    /// - `caps.gate_iterations` ≥ 1.
    /// - `caps.reviewer_iterations` ≥ 1.
    /// - `caps.wall_clock_secs` ≥ 1.
    /// - `concurrency` ≥ 1.
    /// - Each gate has a non-empty `name`.
    /// - Each gate has a non-empty `command`.
    /// - `base_branch` is non-empty.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] with a precise `reason` string on
    /// the first violation found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Validate providers.
        let mut seen_names = std::collections::HashSet::new();

        for provider in &self.providers {
            // Check for non-empty command.
            if provider.command.is_empty() {
                return Err(ConfigError::Validation {
                    reason: format!("provider {:?}: command must not be empty", provider.name),
                });
            }

            // Check for unique names.
            if !seen_names.insert(provider.name.clone()) {
                return Err(ConfigError::Validation {
                    reason: format!("duplicate provider name: {:?}", provider.name),
                });
            }
        }

        // Validate role assignments reference declared providers.
        let provider_names: std::collections::HashSet<_> =
            self.providers.iter().map(|p| p.name.clone()).collect();

        for (role_name, assignment) in [
            ("planner", &self.roles.planner),
            ("developer", &self.roles.developer),
            ("reviewer", &self.roles.reviewer),
        ] {
            if let Some(assignment) = assignment
                && !provider_names.contains(&assignment.provider)
            {
                let provider_list = if self.providers.is_empty() {
                    "no [[providers]] are defined".to_string()
                } else {
                    let names: Vec<String> =
                        self.providers.iter().map(|p| p.name.clone()).collect();
                    format!("defined providers: [{}]", names.join(", "))
                };
                return Err(ConfigError::Validation {
                    reason: format!(
                        "role '{}' references unknown provider {:?} — {}",
                        role_name, assignment.provider, provider_list
                    ),
                });
            }
        }

        // Legacy backend.command check (kept for back-compat).
        if self.backend.command.is_empty() && self.providers.is_empty() {
            return Err(ConfigError::Validation {
                reason: "backend.command must not be empty".to_string(),
            });
        }

        if self.caps.gate_iterations == 0 {
            return Err(ConfigError::Validation {
                reason: "caps.gate_iterations must be at least 1".to_string(),
            });
        }

        if self.caps.reviewer_iterations == 0 {
            return Err(ConfigError::Validation {
                reason: "caps.reviewer_iterations must be at least 1".to_string(),
            });
        }

        if self.caps.wall_clock_secs == 0 {
            return Err(ConfigError::Validation {
                reason: "caps.wall_clock_secs must be at least 1".to_string(),
            });
        }

        if let Some(idle_secs) = self.caps.idle_secs {
            if idle_secs == 0 {
                return Err(ConfigError::Validation {
                    reason: "caps.idle_secs must be at least 1".to_string(),
                });
            }
            if idle_secs >= self.caps.wall_clock_secs {
                warn!(
                    "caps.idle_secs ({}) is >= wall_clock_secs ({}), idle watchdog will never trigger",
                    idle_secs, self.caps.wall_clock_secs
                );
            }
        }

        if self.concurrency == 0 {
            return Err(ConfigError::Validation {
                reason: "concurrency must be at least 1".to_string(),
            });
        }

        if self.base_branch.is_empty() {
            return Err(ConfigError::Validation {
                reason: "base_branch must not be empty".to_string(),
            });
        }

        for (i, gate) in self.gates.iter().enumerate() {
            if gate.name.is_empty() {
                return Err(ConfigError::Validation {
                    reason: format!("gates[{i}].name must not be empty"),
                });
            }
            if gate.command.is_empty() {
                return Err(ConfigError::Validation {
                    reason: format!(
                        "gates[{i}].command must not be empty (gate: {:?})",
                        gate.name
                    ),
                });
            }
        }

        Ok(())
    }

    /// Load config from explicit file paths, parse, resolve, and validate.
    ///
    /// Each path is optional:
    /// - `None` → that layer uses its [`Default`].
    /// - `Some(path)` where the file does **not exist** → that layer uses its
    ///   [`Default`] (missing file is not an error).
    /// - `Some(path)` where the file **exists** → parsed from TOML; I/O or
    ///   parse errors are returned as [`ConfigError`].
    ///
    /// The project layer wins on merge; the result is validated before
    /// returning.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Io`] — file exists but could not be read.
    /// - [`ConfigError::Parse`] — file was read but TOML is invalid.
    /// - [`ConfigError::Validation`] — merged config fails semantic validation.
    pub fn load(
        global_path: Option<&Path>,
        project_path: Option<&Path>,
    ) -> Result<Config, ConfigError> {
        Config::load_with_labels(global_path, None, project_path, None)
    }

    /// Load config with explicit source labels for error messages.
    ///
    /// This is the internal implementation that supports both `load()` (for tests)
    /// and `load_defaults()` (which provides human-friendly labels).
    ///
    /// If `global_label` or `project_label` is `None`, the path's `display()`
    /// is used as a fallback label.
    fn load_with_labels(
        global_path: Option<&Path>,
        global_label: Option<&str>,
        project_path: Option<&Path>,
        project_label: Option<&str>,
    ) -> Result<Config, ConfigError> {
        let global = match global_path {
            None => GlobalConfig::default(),
            Some(path) => {
                if path.exists() {
                    let toml_str = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
                        path: path.display().to_string(),
                        message: e.to_string(),
                    })?;
                    let default_label;
                    let label = if let Some(lbl) = global_label {
                        lbl
                    } else {
                        default_label = path.display().to_string();
                        &default_label
                    };
                    GlobalConfig::from_toml_str(&toml_str, label)?
                } else {
                    GlobalConfig::default()
                }
            }
        };

        let project = match project_path {
            None => ProjectConfig::default(),
            Some(path) => {
                if path.exists() {
                    let toml_str = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
                        path: path.display().to_string(),
                        message: e.to_string(),
                    })?;
                    let default_label;
                    let label = if let Some(lbl) = project_label {
                        lbl
                    } else {
                        default_label = path.display().to_string();
                        &default_label
                    };
                    ProjectConfig::from_toml_str(&toml_str, label)?
                } else {
                    ProjectConfig::default()
                }
            }
        };

        let config = Config::resolve(global, project);
        config.validate()?;
        Ok(config)
    }

    /// Convenience wrapper that resolves the real default paths and calls
    /// [`Config::load_with_labels`] with human-friendly labels.
    ///
    /// - Global:  `~/.makina/config.toml`
    /// - Project: resolved against the current working directory with the
    ///   following precedence:
    ///   1. `.makina/config.toml` (preferred)
    ///   2. legacy `./makina.toml` (deprecated; emits a `warn!` when chosen)
    ///
    /// Prefer this in production entry points.  In tests, use [`Config::load`]
    /// with explicit paths so tests don't depend on the operator's home
    /// directory.
    ///
    /// # Errors
    ///
    /// Same as [`Config::load`].
    pub fn load_defaults() -> Result<Config, ConfigError> {
        let (result, _paths) = Self::load_defaults_with_paths();
        result
    }

    /// Like [`Config::load_defaults`] but returns `(Result<Config, ConfigError>,
    /// ConfigPaths)` — the resolved paths are **always** available, even when
    /// loading fails.  This lets the binary entry point name the files that were
    /// checked in its error message without re-deriving the paths independently.
    ///
    /// # Return value
    ///
    /// - `.0` — the load result, which may be an error.
    /// - `.1` — the [`ConfigPaths`] that were resolved before the load attempt.
    ///   These are valid regardless of whether `.0` is `Ok` or `Err`.
    pub fn load_defaults_with_paths() -> (Result<Config, ConfigError>, ConfigPaths) {
        let global_path = home_dir().map(|h| h.join(".makina").join("config.toml"));
        let repo_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let (project_path, _legacy) = resolve_project_config_path(&repo_root);

        let paths = ConfigPaths {
            global: global_path.clone(),
            project: project_path.clone(),
        };

        let result = Config::load_with_labels(
            global_path.as_deref(),
            Some("global (~/.makina/config.toml)"),
            project_path.as_deref(),
            Some("project (.makina/config.toml)"),
        );

        (result, paths)
    }
}

/// Resolve the project config path under `repo_root`, applying the precedence
/// `.makina/config.toml` (preferred) → legacy `./makina.toml` (deprecated).
///
/// Returns the chosen path (always `Some`, defaulting to the preferred path
/// even when neither exists, so [`Config::load`] sees a non-existent preferred
/// path and falls back to defaults) together with a `bool` indicating whether
/// the legacy `./makina.toml` was chosen. Choosing the legacy path emits a
/// `warn!`.
fn resolve_project_config_path(repo_root: &Path) -> (Option<PathBuf>, bool) {
    let primary = paths::config_file(repo_root);
    let legacy = repo_root.join("makina.toml");

    if primary.exists() {
        (Some(primary), false)
    } else if legacy.exists() {
        warn!(
            path = %legacy.display(),
            "loading deprecated ./makina.toml; move it to .makina/config.toml"
        );
        (Some(legacy), true)
    } else {
        (Some(primary), false)
    }
}

/// Resolve the user's home directory via the `HOME` environment variable.
///
/// Returns `None` if `HOME` is unset or not valid UTF-8 (e.g. in some
/// container environments).  In that case [`Config::load_defaults`] skips the
/// global config file and uses defaults for the global layer.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Config loading, merging, and validation tests.
    //!
    //! All required acceptance-criterion tests use in-memory TOML strings; no
    //! home-directory lookups occur in this module.

    use super::*;

    // ── Sample TOML strings ───────────────────────────────────────────────────

    /// A complete global config.  Includes all sections.
    const GLOBAL_TOML: &str = r#"
        concurrency = 4

        [backend]
        command = "acp-cli"
        args    = ["--verbose"]

        [planner]
        model     = "claude-opus-4-5"
        mechanism = "one-shot-agent"

        [caps]
        gate_iterations     = 8
        reviewer_iterations = 6
        wall_clock_secs     = 3600
    "#;

    /// A project config that overrides concurrency, one cap, adds gates, and
    /// sets a non-default base_branch.
    const PROJECT_TOML: &str = r#"
        base_branch = "main"
        concurrency = 2

        [[gates]]
        name    = "build"
        command = "cargo build --workspace"

        [[gates]]
        name    = "test"
        command = "cargo test --workspace"

        [caps]
        gate_iterations = 10
    "#;

    // ── Merge-correctness test ────────────────────────────────────────────────

    /// **Acceptance criterion — merge correctness**
    ///
    /// Given a global config and a project config where the project overrides
    /// `concurrency`, one cap (`gate_iterations`), adds gates, and sets a
    /// non-default `base_branch`, the resolved `Config` must:
    ///
    /// - Use the project value for overridden fields.
    /// - Retain the global value for fields not overridden by the project.
    #[test]
    fn merge_project_wins_for_overridden_fields() {
        let global =
            GlobalConfig::from_toml_str(GLOBAL_TOML, "global").expect("global TOML is valid");
        let project =
            ProjectConfig::from_toml_str(PROJECT_TOML, "project").expect("project TOML is valid");

        let config = Config::resolve(global, project);

        // ── Project-layer fields ──────────────────────────────────────────────

        // base_branch: project sets "main".
        assert_eq!(
            config.base_branch, "main",
            "base_branch should be overridden by project"
        );

        // concurrency: project sets 2 (global had 4).
        assert_eq!(
            config.concurrency, 2,
            "concurrency should be overridden by project"
        );

        // gate_iterations: project caps override sets 10 (global had 8).
        assert_eq!(
            config.caps.gate_iterations, 10,
            "caps.gate_iterations should be overridden by project"
        );

        // gates: come from project.
        assert_eq!(
            config.gates.len(),
            2,
            "should have 2 gates from the project layer"
        );
        assert_eq!(config.gates[0].name, "build");
        assert_eq!(config.gates[0].command, "cargo build --workspace");
        assert_eq!(config.gates[1].name, "test");
        assert_eq!(config.gates[1].command, "cargo test --workspace");

        // ── Global-layer fields (not overridden) ──────────────────────────────

        // reviewer_iterations: project did NOT override → keep global value 6.
        assert_eq!(
            config.caps.reviewer_iterations, 6,
            "caps.reviewer_iterations should come from global config"
        );

        // wall_clock_secs: project did NOT override → keep global value 3600.
        assert_eq!(
            config.caps.wall_clock_secs, 3600,
            "caps.wall_clock_secs should come from global config"
        );

        // backend: comes from global.
        assert_eq!(
            config.backend.command, "acp-cli",
            "backend.command should come from global config"
        );
        assert_eq!(config.backend.args, vec!["--verbose"]);

        // planner: comes from global.
        assert_eq!(config.planner.model, "claude-opus-4-5");
        assert_eq!(config.planner.mechanism, PlannerMechanism::OneShotAgent);

        // Validate should pass for this fully-resolved config.
        config
            .validate()
            .expect("fully-resolved sample config should be valid");
    }

    // ── Invalid-config tests ──────────────────────────────────────────────────

    /// **Acceptance criterion — invalid config fails clearly (a): validation failure**
    ///
    /// A config with `concurrency = 0` fails [`Config::validate`] with
    /// [`ConfigError::Validation`] and a message mentioning "concurrency".
    #[test]
    fn validation_fails_for_zero_concurrency() {
        let global = GlobalConfig::from_toml_str(
            r#"
            concurrency = 0

            [backend]
            command = "acp-cli"
            "#,
            "test-global",
        )
        .expect("TOML itself is valid");

        let project = ProjectConfig::default();
        let config = Config::resolve(global, project);

        let err = config
            .validate()
            .expect_err("concurrency = 0 should fail validation");

        assert!(
            matches!(err, ConfigError::Validation { .. }),
            "error should be ConfigError::Validation, got: {err:?}"
        );
        assert!(
            err.to_string().contains("concurrency"),
            "error message should mention 'concurrency', got: {err}"
        );
    }

    /// **Acceptance criterion — invalid config fails clearly (a, second case):**
    /// empty `backend.command` fails validation with a clear message.
    #[test]
    fn validation_fails_for_empty_backend_command() {
        // Global with explicitly empty command.
        let global = GlobalConfig::from_toml_str(
            r#"
            concurrency = 1

            [backend]
            command = ""
            "#,
            "test-global",
        )
        .expect("TOML itself is valid");

        let project = ProjectConfig::default();
        let config = Config::resolve(global, project);

        let err = config
            .validate()
            .expect_err("empty backend.command should fail validation");

        assert!(
            matches!(err, ConfigError::Validation { .. }),
            "error should be ConfigError::Validation, got: {err:?}"
        );
        assert!(
            err.to_string().contains("backend.command"),
            "error message should mention 'backend.command', got: {err}"
        );
    }

    /// **Acceptance criterion — invalid config fails clearly (b): malformed TOML**
    ///
    /// Passing a syntactically invalid TOML string to
    /// [`GlobalConfig::from_toml_str`] returns [`ConfigError::Parse`].
    #[test]
    fn parse_error_for_malformed_global_toml() {
        let bad_toml = "this is not valid = = toml!!!";

        let err = GlobalConfig::from_toml_str(bad_toml, "test-global")
            .expect_err("malformed TOML should return an error");

        assert!(
            matches!(err, ConfigError::Parse { .. }),
            "error should be ConfigError::Parse, got: {err:?}"
        );
        // The error message should identify the source.
        assert!(
            err.to_string().contains("test-global"),
            "error message should contain the source label, got: {err}"
        );
    }

    /// Malformed project TOML also returns [`ConfigError::Parse`].
    #[test]
    fn parse_error_for_malformed_project_toml() {
        let bad_toml = "[gates\nname = broken";

        let err = ProjectConfig::from_toml_str(bad_toml, "makina.toml")
            .expect_err("malformed TOML should return an error");

        assert!(
            matches!(err, ConfigError::Parse { .. }),
            "error should be ConfigError::Parse"
        );
        assert!(
            err.to_string().contains("makina.toml"),
            "error should name the source file"
        );
    }

    /// An empty gate command fails validation with a message naming the gate.
    #[test]
    fn validation_fails_for_gate_with_empty_command() {
        let global = GlobalConfig::from_toml_str(
            r#"
            concurrency = 1
            [backend]
            command = "acp-cli"
            "#,
            "test-global",
        )
        .expect("valid");

        let project = ProjectConfig::from_toml_str(
            r#"
            [[gates]]
            name    = "my-gate"
            command = ""
            "#,
            "test-project",
        )
        .expect("valid TOML");

        let config = Config::resolve(global, project);
        let err = config
            .validate()
            .expect_err("gate with empty command should fail");

        assert!(
            matches!(err, ConfigError::Validation { .. }),
            "should be a validation error"
        );
        assert!(
            err.to_string().contains("command"),
            "error should mention 'command'"
        );
    }

    // ── Default-value tests ───────────────────────────────────────────────────

    /// Defaults from both layers produce sane initial values.
    #[test]
    fn default_caps_are_within_expected_range() {
        let caps = CapsConfig::default();
        assert_eq!(caps.gate_iterations, 5);
        assert_eq!(caps.reviewer_iterations, 5);
        assert_eq!(caps.wall_clock_secs, 1800);
    }

    /// `idle_secs` validation: Some(0) fails, Some(30) succeeds, None succeeds.
    #[test]
    fn idle_cap_validates() {
        // Test 1: idle_secs = Some(0) should fail validation
        let global = GlobalConfig::from_toml_str(
            r#"
            [backend]
            command = "acp-cli"
            [caps]
            idle_secs = 0
            "#,
            "test-global",
        )
        .expect("valid TOML");
        let project = ProjectConfig::from_toml_str("", "test-project").expect("empty project");
        let config = Config::resolve(global, project);
        let err = config
            .validate()
            .expect_err("idle_secs = 0 should fail validation");
        assert_eq!(
            err.to_string(),
            "invalid config: caps.idle_secs must be at least 1"
        );

        // Test 2: idle_secs = Some(30) should succeed
        let global = GlobalConfig::from_toml_str(
            r#"
            [backend]
            command = "acp-cli"
            [caps]
            idle_secs = 30
            "#,
            "test-global",
        )
        .expect("valid TOML");
        let project = ProjectConfig::from_toml_str("", "test-project").expect("empty project");
        let config = Config::resolve(global, project);
        config.validate().expect("idle_secs = 30 should validate");

        // Test 3: idle_secs = None (default) should succeed
        let global = GlobalConfig::from_toml_str(
            r#"
            [backend]
            command = "acp-cli"
            "#,
            "test-global",
        )
        .expect("valid TOML");
        let project = ProjectConfig::from_toml_str("", "test-project").expect("empty project");
        let config = Config::resolve(global, project);
        config.validate().expect("idle_secs = None should validate");
        assert_eq!(
            config.caps.idle_secs, None,
            "idle_secs should default to None"
        );
    }

    /// Global default concurrency is 3.
    #[test]
    fn default_concurrency_is_three() {
        // An empty global TOML parses to defaults.
        let global = GlobalConfig::from_toml_str("", "empty").expect("empty TOML is valid");
        assert_eq!(global.concurrency, 3);
    }

    /// Project default base_branch falls back to "develop" during resolve.
    #[test]
    fn default_base_branch_is_develop() {
        let global = GlobalConfig::from_toml_str(
            r#"
            [backend]
            command = "acp-cli"
            "#,
            "g",
        )
        .expect("valid");
        let project = ProjectConfig::from_toml_str("", "p").expect("empty project TOML is valid");

        let config = Config::resolve(global, project);
        assert_eq!(
            config.base_branch, "develop",
            "empty project base_branch should fall back to 'develop'"
        );
    }

    // ── load() with temp files ────────────────────────────────────────────────

    /// **Nice-to-have:** `Config::load` with a missing global path falls back
    /// to global defaults and uses the project file.
    #[test]
    fn load_missing_global_falls_back_to_defaults() {
        use std::io::Write;

        // Write a valid project config to a temp file.
        let mut project_file = tempfile::NamedTempFile::new().expect("should create temp file");
        write!(
            project_file,
            r#"
            base_branch = "feature"

            [[gates]]
            name    = "check"
            command = "cargo check"

            [caps]
            gate_iterations = 3
            "#
        )
        .expect("write project TOML");

        // Point global_path at a file that definitely does not exist.
        let nonexistent_global = std::path::Path::new("/tmp/__makina_does_not_exist_config.toml");

        let result = Config::load(Some(nonexistent_global), Some(project_file.path()));
        // Should fail validation because backend.command is empty (global default).
        // This confirms the missing-global fallback occurred without error, and
        // the validation error is the only problem.
        match result {
            Err(ConfigError::Validation { reason }) => {
                assert!(
                    reason.contains("backend.command"),
                    "validation should fail on empty backend.command, got: {reason}"
                );
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    /// `Config::load` with both paths missing returns validation error (empty
    /// backend command from all-defaults).
    #[test]
    fn load_both_missing_returns_defaults_then_validation_error() {
        let nonexistent_global = std::path::Path::new("/tmp/__makina_no_global.toml");
        let nonexistent_project = std::path::Path::new("/tmp/__makina_no_project.toml");

        let result = Config::load(Some(nonexistent_global), Some(nonexistent_project));
        match result {
            Err(ConfigError::Validation { reason }) => {
                assert!(reason.contains("backend.command"), "{reason}");
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    // ── Project config path resolution ────────────────────────────────────────

    /// `.makina/config.toml` is preferred over the legacy `./makina.toml`.
    #[test]
    fn resolve_project_config_path_prefers_makina_dir() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let repo_root = dir.path();

        let primary = paths::config_file(repo_root);
        std::fs::create_dir_all(primary.parent().unwrap()).expect("create .makina dir");
        std::fs::write(&primary, "").expect("write .makina/config.toml");

        let (path, legacy) = resolve_project_config_path(repo_root);

        assert_eq!(path.as_deref(), Some(primary.as_path()));
        assert!(
            !legacy,
            "legacy should be false when .makina/config.toml exists"
        );
    }

    /// When only `./makina.toml` exists, it is chosen and flagged as legacy
    /// (exercising the deprecation-warning branch).
    #[test]
    fn resolve_project_config_path_falls_back_to_legacy() {
        let dir = tempfile::tempdir().expect("should create temp dir");
        let repo_root = dir.path();

        let legacy_path = repo_root.join("makina.toml");
        std::fs::write(&legacy_path, "").expect("write ./makina.toml");

        let (path, legacy) = resolve_project_config_path(repo_root);

        assert_eq!(path.as_deref(), Some(legacy_path.as_path()));
        assert!(
            legacy,
            "legacy should be true when only ./makina.toml exists"
        );
    }

    // ── Provider and role config tests ────────────────────────────────────────

    /// **Acceptance criterion — legacy backend becomes default provider**
    ///
    /// Given a global config with only `[backend]` (no `providers` section),
    /// after resolve, a `ProviderConfig` named `"default"` should be synthesized
    /// from the backend command/args, and all roles should be assigned to it.
    #[test]
    fn legacy_backend_becomes_default_provider() {
        let global = GlobalConfig::from_toml_str(
            r#"
            [backend]
            command = "acp-cli"
            args = ["--verbose"]
            "#,
            "test-global",
        )
        .expect("valid");

        let project = ProjectConfig::default();
        let config = Config::resolve(global, project);

        // Should have exactly one provider: "default".
        assert_eq!(
            config.providers.len(),
            1,
            "should have one synthesized provider"
        );
        assert_eq!(config.providers[0].name, "default");
        assert_eq!(config.providers[0].command, "acp-cli");
        assert_eq!(config.providers[0].args, vec!["--verbose"]);

        // All three roles should be assigned to "default".
        assert!(config.roles.planner.is_some(), "planner should be assigned");
        assert_eq!(config.roles.planner.as_ref().unwrap().provider, "default");

        assert!(
            config.roles.developer.is_some(),
            "developer should be assigned"
        );
        assert_eq!(config.roles.developer.as_ref().unwrap().provider, "default");

        assert!(
            config.roles.reviewer.is_some(),
            "reviewer should be assigned"
        );
        assert_eq!(config.roles.reviewer.as_ref().unwrap().provider, "default");

        // Validation should pass.
        config
            .validate()
            .expect("synthesized default provider should pass validation");
    }

    /// **Acceptance criterion — role assignment resolves provider**
    ///
    /// Given a global config with two providers ("a" and "b") and developer
    /// assigned to provider "b", the resolved config should have both providers
    /// and the developer role should reference "b".
    #[test]
    fn role_assignment_resolves_provider() {
        let global = GlobalConfig::from_toml_str(
            r#"
            [[providers]]
            name = "a"
            command = "provider-a"

            [[providers]]
            name = "b"
            command = "provider-b"

            [roles.developer]
            provider = "b"
            mode = "code"
            model = "grok-2"
            effort = "high"
            "#,
            "test-global",
        )
        .expect("valid");

        let project = ProjectConfig::default();
        let config = Config::resolve(global, project);

        // Should have both providers.
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.providers[0].name, "a");
        assert_eq!(config.providers[1].name, "b");

        // Developer should be assigned to "b".
        assert!(config.roles.developer.is_some());
        let dev_assignment = config.roles.developer.as_ref().unwrap();
        assert_eq!(dev_assignment.provider, "b");
        assert_eq!(dev_assignment.mode, Some("code".to_string()));
        assert_eq!(dev_assignment.model, Some("grok-2".to_string()));
        assert_eq!(dev_assignment.effort, Some("high".to_string()));

        // Validation should pass.
        config
            .validate()
            .expect("config with valid provider references should pass validation");
    }

    /// **Acceptance criterion — unknown provider rejected**
    ///
    /// Given a config where roles.reviewer.provider references a provider named
    /// "nope" that does not exist, validate() should return a Validation error
    /// mentioning "unknown provider".
    #[test]
    fn unknown_provider_rejected() {
        let global = GlobalConfig::from_toml_str(
            r#"
            [[providers]]
            name = "a"
            command = "provider-a"

            [roles.reviewer]
            provider = "nope"
            "#,
            "test-global",
        )
        .expect("valid TOML");

        let project = ProjectConfig::default();
        let config = Config::resolve(global, project);

        let err = config
            .validate()
            .expect_err("reviewer referencing unknown provider 'nope' should fail validation");

        assert!(
            matches!(err, ConfigError::Validation { .. }),
            "should be a validation error"
        );
        assert!(
            err.to_string().contains("unknown provider"),
            "error should mention 'unknown provider', got: {err}"
        );
    }

    /// **Acceptance criterion — enrich-config-errors (test 1):**
    /// The unknown-provider `reason` includes the defined provider names.
    ///
    /// Given a config with `roles.developer.provider="nope"` and
    /// `providers=[a, default]`, the validation error reason should contain
    /// the bad provider name "nope" AND the valid provider names "a" and "default".
    #[test]
    fn config_error_lists_defined_providers() {
        let global = GlobalConfig::from_toml_str(
            r#"
            [[providers]]
            name = "a"
            command = "provider-a"

            [[providers]]
            name = "default"
            command = "default-provider"

            [roles.developer]
            provider = "nope"
            "#,
            "test-global",
        )
        .expect("valid TOML");

        let project = ProjectConfig::default();
        let config = Config::resolve(global, project);

        let err = config
            .validate()
            .expect_err("developer referencing unknown provider 'nope' should fail validation");

        let error_msg = err.to_string();
        assert!(
            error_msg.contains("nope"),
            "error should mention the bad provider 'nope', got: {error_msg}"
        );
        assert!(
            error_msg.contains("a"),
            "error should mention the defined provider 'a', got: {error_msg}"
        );
        assert!(
            error_msg.contains("default"),
            "error should mention the defined provider 'default', got: {error_msg}"
        );
    }

    /// **Acceptance criterion — enrich-config-errors (test 2a):**
    /// Parse errors name the project file.
    ///
    /// Given a malformed project TOML, the parse error should contain the
    /// project-specific label "project (.makina/config.toml)".
    #[test]
    fn parse_error_names_source_file() {
        let bad_toml = "[gates\nname = broken";

        let err = ProjectConfig::from_toml_str(bad_toml, "project (.makina/config.toml)")
            .expect_err("malformed TOML should return an error");

        let error_msg = err.to_string();
        assert!(
            error_msg.contains("project (.makina/config.toml)"),
            "error should contain the project-specific label, got: {error_msg}"
        );
    }

    /// **Acceptance criterion — enrich-config-errors (test 2b):**
    /// Parse errors name the global file.
    ///
    /// Given a malformed global TOML, the parse error should contain the
    /// global-specific label "global (~/.makina/config.toml)".
    #[test]
    fn parse_error_names_global_source_file() {
        let bad_toml = "this is not valid = = toml!!!";

        let err = GlobalConfig::from_toml_str(bad_toml, "global (~/.makina/config.toml)")
            .expect_err("malformed TOML should return an error");

        let error_msg = err.to_string();
        assert!(
            error_msg.contains("global (~/.makina/config.toml)"),
            "error should contain the global-specific label, got: {error_msg}"
        );
    }
}
