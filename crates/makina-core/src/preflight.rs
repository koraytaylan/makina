//! Provider binary preflight checks.
//!
//! Before launching a run, probe each provider's command to detect missing
//! binaries early, surfacing a non-fatal warning instead of a mid-task failure.
//!
//! The probing is **pure filesystem + env**: it walks `$PATH` to resolve
//! commands (or stats direct paths) without spawning any process.

use std::path::{Path, PathBuf};

use crate::config::Config;

/// Result of probing a provider's command for presence on the system.
///
/// Each probe resolves the provider's command name against `$PATH` (or validates
/// an absolute path), capturing whether the binary is present and any diagnostic
/// notes.
#[derive(Debug, Clone)]
pub struct ProviderProbe {
    /// The provider's configured name (e.g. `"default"`, `"grok"`).
    pub provider: String,

    /// The first whitespace token of the provider's `command` field
    /// (e.g. `"acp-cli"` from `"acp-cli --verbose"`).
    pub command: String,

    /// Resolved path to the binary if found, or `None` if not found on `$PATH`
    /// or if an absolute path does not exist.
    pub resolved: Option<PathBuf>,

    /// Optional diagnostic message (e.g. `"$PATH is empty"`, `"is a directory"`).
    pub note: Option<String>,
}

/// Probe each provider's command for presence on the filesystem.
///
/// Resolves each provider's command's first whitespace token:
/// - If the token contains `/`, stat it directly as an absolute/relative path.
/// - Otherwise, walk `$PATH` entries (split by `:` on Unix, `;` on Windows)
///   and check for an executable file with that name.
///
/// Returns a `Vec<ProviderProbe>` with one entry per provider in the config,
/// capturing the resolution result and any diagnostic notes. No process is ever
/// spawned (spawning an unauthenticated agent can itself hang — see plan 0015).
///
/// # Example
///
/// ```rust,no_run
/// use makina_core::config::Config;
/// use makina_core::preflight::probe_providers;
///
/// let config = Config::load_defaults()?;
/// let probes = probe_providers(&config);
/// for probe in probes {
///     if probe.resolved.is_none() {
///         println!("⚠ provider \"{}\" command '{}' not found", probe.provider, probe.command);
///     }
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn probe_providers(cfg: &Config) -> Vec<ProviderProbe> {
    // Read $PATH once here and thread it into the helper.  This keeps
    // `resolve_in_path` pure (no hidden global reads) so tests can inject any
    // PATH they want without touching the process environment.
    let path_env = std::env::var("PATH").unwrap_or_default();
    probe_providers_with_path(cfg, &path_env)
}

/// Like [`probe_providers`] but resolves commands against an explicit `path_env`
/// string instead of reading `$PATH` from the process environment.
///
/// This is the inner, testable variant.  Production code should call
/// [`probe_providers`], which reads `$PATH` once and delegates here.
pub fn probe_providers_with_path(cfg: &Config, path_env: &str) -> Vec<ProviderProbe> {
    cfg.providers
        .iter()
        .map(|provider| {
            let command_str = provider.command.clone();
            let first_token = command_str
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();

            if first_token.is_empty() {
                return ProviderProbe {
                    provider: provider.name.clone(),
                    command: first_token,
                    resolved: None,
                    note: Some("command is empty".to_string()),
                };
            }

            // If the token contains `/`, treat it as a path (absolute or relative).
            if first_token.contains('/') {
                let path = PathBuf::from(&first_token);
                let resolved = if is_executable(&path) {
                    Some(path.clone())
                } else {
                    None
                };
                let note = if resolved.is_none() {
                    if path.is_dir() {
                        Some("is a directory".to_string())
                    } else {
                        Some("file does not exist or is not executable".to_string())
                    }
                } else {
                    None
                };
                ProviderProbe {
                    provider: provider.name.clone(),
                    command: first_token,
                    resolved,
                    note,
                }
            } else {
                // Search the provided path_env for the executable.
                resolve_in_path(&first_token, &provider.name, path_env)
            }
        })
        .collect()
}

/// Resolve a command name against the given `path_env` string.
///
/// Splits `path_env` (`:` on Unix, `;` on Windows) and checks each directory
/// for an executable file with the given name.  Returns a `ProviderProbe`
/// with the result and optional diagnostic notes.
///
/// Accepting an explicit `path_env` (rather than reading `$PATH` directly)
/// keeps the function testable without mutating the process-wide environment.
fn resolve_in_path(command: &str, provider: &str, path_env: &str) -> ProviderProbe {
    // Empty PATH is a notable case.
    if path_env.is_empty() {
        return ProviderProbe {
            provider: provider.to_string(),
            command: command.to_string(),
            resolved: None,
            note: Some("$PATH is empty".to_string()),
        };
    }

    let separator = if cfg!(windows) { ";" } else { ":" };
    let paths = path_env.split(separator);

    for path_dir in paths {
        if path_dir.is_empty() {
            continue;
        }
        let candidate = PathBuf::from(path_dir).join(command);
        if is_executable(&candidate) {
            return ProviderProbe {
                provider: provider.to_string(),
                command: command.to_string(),
                resolved: Some(candidate),
                note: None,
            };
        }
    }

    // Not found in any PATH entry.
    ProviderProbe {
        provider: provider.to_string(),
        command: command.to_string(),
        resolved: None,
        note: None,
    }
}

/// Check if a path points to an executable file.
///
/// Returns `true` if the path exists, is a regular file, and has execute permissions.
/// Returns `false` otherwise (not found, is a directory, not executable, etc.).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        if let Ok(metadata) = fs::metadata(path)
            && metadata.is_file()
        {
            let mode = metadata.permissions().mode();
            // Check if any execute bit is set (user, group, or other).
            return (mode & 0o111) != 0;
        }
        false
    }

    #[cfg(windows)]
    {
        // On Windows, just check if the file exists; execute permission is implicit
        // for .exe and other executable extensions.
        std::path::Path::new(path).is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GlobalConfig;
    use std::fs;
    use std::io::Write;

    #[test]
    fn probe_reports_missing_binary() {
        // Create a config with a provider whose command does not exist.
        let global = GlobalConfig {
            providers: vec![crate::config::ProviderConfig {
                name: "test".to_string(),
                command: "definitely-not-real-xyz".to_string(),
                args: vec![],
                env: std::collections::BTreeMap::new(),
            }],
            ..Default::default()
        };

        let project = crate::config::ProjectConfig::default();
        let config = Config::resolve(global, project);

        let probes = probe_providers(&config);
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].provider, "test");
        assert_eq!(probes[0].command, "definitely-not-real-xyz");
        assert!(probes[0].resolved.is_none());
    }

    #[test]
    fn probe_resolves_command_on_synthetic_path() {
        // Create a temporary directory with a fake executable.
        let tmpdir = tempfile::tempdir().expect("create temp dir");
        let tmpdir_path = tmpdir.path();

        // Create an executable "foo" in the temp directory.
        let exe_path = tmpdir_path.join("foo");
        let mut f = fs::File::create(&exe_path).expect("create executable");
        f.write_all(b"#!/bin/sh\necho test\n")
            .expect("write to executable");

        // Make it executable on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&exe_path, fs::Permissions::from_mode(0o755))
                .expect("set permissions");
        }

        // Create a config with a provider command "foo --acp".
        let global = GlobalConfig {
            providers: vec![crate::config::ProviderConfig {
                name: "test".to_string(),
                command: "foo --acp".to_string(),
                args: vec![],
                env: std::collections::BTreeMap::new(),
            }],
            ..Default::default()
        };

        let project = crate::config::ProjectConfig::default();
        let config = Config::resolve(global, project);

        // Inject the synthetic PATH directly — no global env mutation needed.
        let synthetic_path = tmpdir_path.display().to_string();
        let probes = probe_providers_with_path(&config, &synthetic_path);

        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].provider, "test");
        assert_eq!(probes[0].command, "foo");
        assert!(
            probes[0].resolved.is_some(),
            "probe should resolve 'foo' in synthetic PATH"
        );
        let resolved = probes[0].resolved.as_ref().unwrap();
        assert!(
            resolved.ends_with("foo"),
            "resolved path should end with 'foo'"
        );
        assert_eq!(resolved.parent(), Some(tmpdir_path));
    }

    #[test]
    fn probe_handles_absolute_path() {
        // Use an absolute path that exists (e.g., /bin/sh on Unix).
        let existing_exe = if cfg!(unix) {
            "/bin/sh"
        } else {
            "C:\\Windows\\System32\\cmd.exe"
        };

        if !PathBuf::from(existing_exe).exists() {
            // Skip test if the reference binary doesn't exist on this system.
            return;
        }

        let global = GlobalConfig {
            providers: vec![crate::config::ProviderConfig {
                name: "test".to_string(),
                command: existing_exe.to_string(),
                args: vec![],
                env: std::collections::BTreeMap::new(),
            }],
            ..Default::default()
        };

        let project = crate::config::ProjectConfig::default();
        let config = Config::resolve(global, project);

        let probes = probe_providers(&config);
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].provider, "test");
        assert_eq!(probes[0].command, existing_exe);
        assert!(probes[0].resolved.is_some());
    }

    #[test]
    fn probe_detects_directory() {
        let tmpdir = tempfile::tempdir().expect("create temp dir");
        let tmpdir_path = tmpdir.path();

        let global = GlobalConfig {
            providers: vec![crate::config::ProviderConfig {
                name: "test".to_string(),
                command: tmpdir_path.display().to_string(),
                args: vec![],
                env: std::collections::BTreeMap::new(),
            }],
            ..Default::default()
        };

        let project = crate::config::ProjectConfig::default();
        let config = Config::resolve(global, project);

        let probes = probe_providers(&config);
        assert_eq!(probes.len(), 1);
        assert!(probes[0].resolved.is_none());
        assert_eq!(probes[0].note, Some("is a directory".to_string()));
    }

    #[test]
    fn probe_handles_command_with_args() {
        let tmpdir = tempfile::tempdir().expect("create temp dir");
        let tmpdir_path = tmpdir.path();

        // Create an executable "myagent" in the temp directory.
        let exe_path = tmpdir_path.join("myagent");
        let mut f = fs::File::create(&exe_path).expect("create executable");
        f.write_all(b"#!/bin/sh\necho test\n")
            .expect("write to executable");

        // Make it executable on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&exe_path, fs::Permissions::from_mode(0o755))
                .expect("set permissions");
        }

        let global = GlobalConfig {
            providers: vec![crate::config::ProviderConfig {
                name: "test".to_string(),
                command: "myagent --auth-proxy --verbose --model=claude".to_string(),
                args: vec![],
                env: std::collections::BTreeMap::new(),
            }],
            ..Default::default()
        };

        let project = crate::config::ProjectConfig::default();
        let config = Config::resolve(global, project);

        // Inject the synthetic PATH directly — no global env mutation needed.
        let synthetic_path = tmpdir_path.display().to_string();
        let probes = probe_providers_with_path(&config, &synthetic_path);

        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].provider, "test");
        assert_eq!(probes[0].command, "myagent");
        assert!(probes[0].resolved.is_some());
    }
}
