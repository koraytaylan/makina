//! Pure path-building helpers for the `.makina/` workspace layout.
//!
//! Every function here is a pure string/path join: no I/O, no validation, no
//! logic. Each parameter is treated as an opaque string and is appended
//! verbatim. Callers are responsible for ensuring the inputs are well-formed.

use std::path::{Path, PathBuf};

/// Returns the path of the Makina config file:
/// `{repo_root}/.makina/config.toml`.
///
/// # Example
///
/// ```
/// # use std::path::Path;
/// # use makina_core::paths::config_file;
/// let p = config_file(Path::new("/repo"));
/// assert_eq!(p, std::path::PathBuf::from("/repo/.makina/config.toml"));
/// ```
pub fn config_file(repo_root: &Path) -> PathBuf {
    repo_root.join(".makina").join("config.toml")
}

/// Returns the path of a persisted task-graph artifact:
/// `{repo_root}/.makina/tasks/{slug}.json`.
///
/// # Example
///
/// ```
/// # use std::path::Path;
/// # use makina_core::paths::task_graph;
/// let p = task_graph(Path::new("/repo"), "my-feature");
/// assert_eq!(p, std::path::PathBuf::from("/repo/.makina/tasks/my-feature.json"));
/// ```
pub fn task_graph(repo_root: &Path, slug: &str) -> PathBuf {
    repo_root
        .join(".makina")
        .join("tasks")
        .join(format!("{slug}.json"))
}

/// Returns the directory for a single run:
/// `{repo_root}/.makina/runs/{run_id}`.
///
/// # Example
///
/// ```
/// # use std::path::Path;
/// # use makina_core::paths::run_dir;
/// let p = run_dir(Path::new("/repo"), "01ABC");
/// assert_eq!(p, std::path::PathBuf::from("/repo/.makina/runs/01ABC"));
/// ```
pub fn run_dir(repo_root: &Path, run_id: &str) -> PathBuf {
    repo_root.join(".makina").join("runs").join(run_id)
}

/// Returns the audit-log path for a run:
/// `{repo_root}/.makina/runs/{run_id}/audit.jsonl`.
///
/// # Example
///
/// ```
/// # use std::path::Path;
/// # use makina_core::paths::audit_log;
/// let p = audit_log(Path::new("/repo"), "01ABC");
/// assert_eq!(p, std::path::PathBuf::from("/repo/.makina/runs/01ABC/audit.jsonl"));
/// ```
pub fn audit_log(repo_root: &Path, run_id: &str) -> PathBuf {
    run_dir(repo_root, run_id).join("audit.jsonl")
}

/// Returns the per-task log path within a run:
/// `{repo_root}/.makina/runs/{run_id}/logs/{task_slug}.log`.
///
/// # Example
///
/// ```
/// # use std::path::Path;
/// # use makina_core::paths::task_log;
/// let p = task_log(Path::new("/repo"), "01ABC", "task-a");
/// assert_eq!(p, std::path::PathBuf::from("/repo/.makina/runs/01ABC/logs/task-a.log"));
/// ```
pub fn task_log(repo_root: &Path, run_id: &str, task_slug: &str) -> PathBuf {
    run_dir(repo_root, run_id)
        .join("logs")
        .join(format!("{task_slug}.log"))
}

/// Creates (if needed) and returns the per-run log directory:
/// `{repo_root}/.makina/runs/{run_id}/logs`.
///
/// Unlike the other helpers in this module — which are pure, no-I/O path
/// builders ([`run_dir`], [`audit_log`], [`task_log`]) — this function performs
/// I/O: it `create_dir_all`s the directory before returning it. Keep this the
/// one clearly-separate I/O helper here.
pub fn run_logs_dir(repo_root: &Path, run_id: &str) -> std::io::Result<PathBuf> {
    let dir = run_dir(repo_root, run_id).join("logs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Returns the worktree directory for a task:
/// `{repo_root}/.makina/worktrees/{task_id}`.
///
/// # Example
///
/// ```
/// # use std::path::Path;
/// # use makina_core::paths::worktree;
/// let p = worktree(Path::new("/repo"), "task-a");
/// assert_eq!(p, std::path::PathBuf::from("/repo/.makina/worktrees/task-a"));
/// ```
pub fn worktree(repo_root: &Path, task_id: &str) -> PathBuf {
    repo_root.join(".makina").join("worktrees").join(task_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_path() {
        assert_eq!(
            config_file(Path::new("/repo")),
            PathBuf::from("/repo/.makina/config.toml")
        );
    }

    #[test]
    fn task_graph_path() {
        assert_eq!(
            task_graph(Path::new("/repo"), "my-feature"),
            PathBuf::from("/repo/.makina/tasks/my-feature.json")
        );
    }

    #[test]
    fn run_dir_path() {
        assert_eq!(
            run_dir(Path::new("/repo"), "01ABC"),
            PathBuf::from("/repo/.makina/runs/01ABC")
        );
    }

    #[test]
    fn audit_log_path() {
        assert_eq!(
            audit_log(Path::new("/repo"), "01ABC"),
            PathBuf::from("/repo/.makina/runs/01ABC/audit.jsonl")
        );
    }

    #[test]
    fn task_log_path() {
        assert_eq!(
            task_log(Path::new("/repo"), "01ABC", "task-a"),
            PathBuf::from("/repo/.makina/runs/01ABC/logs/task-a.log")
        );
    }

    #[test]
    fn worktree_path() {
        assert_eq!(
            worktree(Path::new("/repo"), "task-a"),
            PathBuf::from("/repo/.makina/worktrees/task-a")
        );
    }

    #[test]
    fn run_logs_dir_creates_and_is_idempotent() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let repo_root = tmp.path();
        let run_id = "01ABC";

        let dir = run_logs_dir(repo_root, run_id).expect("first run_logs_dir call");
        assert!(
            dir.ends_with(".makina/runs/01ABC/logs"),
            "returned path should end with .makina/runs/{run_id}/logs, got {}",
            dir.display()
        );
        assert!(dir.is_dir(), "directory should exist after the call");

        // Second call on the same path must also succeed (idempotent).
        let dir2 = run_logs_dir(repo_root, run_id).expect("second run_logs_dir call");
        assert_eq!(dir, dir2);
    }
}
