//! Pure path-building helpers for the `.makina/` workspace layout.
//!
//! Most functions here are pure string/path joins: no I/O, no validation, no
//! logic. Each parameter is treated as an opaque string and is appended
//! verbatim. Callers are responsible for ensuring the inputs are well-formed.
//!
//! **Committed helpers** (`config_file`, `task_graph`) root at
//! `repo_root/.makina/` — these are versioned artifacts.
//!
//! **Transient helpers** (`run_dir`, `audit_log`, `task_log`, `run_logs_dir`,
//! `worktree`, `worktrees_dir`, `checkpoint_dir`) also root at
//! `repo_root/.makina/` — they hold runtime state (runs, worktrees,
//! checkpoints) that is gitignored. Co-locating transient state with the
//! repository eliminates the `$HOME` dependency and stale-state-survives-repo-
//! deletion bugs of the previous external layout.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Sanitise a string to `[a-z0-9-]`: lowercases, keeps `a-z 0-9 -`, maps
/// everything else to `-`, then trims leading and trailing `-`.
fn sanitize_ns_part(s: &str) -> String {
    let lowered: String = s
        .chars()
        .map(|c| {
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphanumeric() || lc == '-' {
                lc
            } else {
                '-'
            }
        })
        .collect();
    lowered.trim_matches('-').to_string()
}

/// 4-char lowercase-hex hash of `data` using inline FNV-1a (64-bit).
/// FNV-1a is release-stable: the algorithm is fixed, not tied to
/// `DefaultHasher` which is explicitly unstable across Rust releases.
fn hex4(data: &[u8]) -> String {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash: u64 = FNV_OFFSET;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}").chars().take(4).collect()
}

/// Extract the leading run of ASCII digits from `s`.
/// Returns the digits as a string; if `s` has no leading digits, returns the
/// sanitized first-4-character head instead so the result is never empty.
fn leading_digits(s: &str) -> String {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        return digits;
    }
    // Fallback: sanitize the first 4 chars of `s`.
    let head: String = s.chars().take(4).collect();
    let sanitized = sanitize_ns_part(&head);
    if sanitized.is_empty() {
        "plan".to_string()
    } else {
        sanitized
    }
}

/// Truncate `s` to `max_len` characters and trim trailing `-`.
fn truncate_trim(s: &str, max_len: usize) -> &str {
    let end = s
        .char_indices()
        .nth(max_len)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    s[..end].trim_end_matches('-')
}

// ---------------------------------------------------------------------------
// Public path helpers — committed artifacts (stay under repo_root/.makina)
// ---------------------------------------------------------------------------

/// Maximum length of the task portion in [`short_worktree_name`].
const TASK_TRUNC: usize = 20;

/// Bounded, unique, `[a-z0-9-]`-valid worktree dir/branch leaf:
/// `"{plan#}-{task-trunc}-{hash4}"`, e.g. `"0016-sidebar-tree-nav-a1b2"`.
///
/// - `plan#` — the leading run of ASCII digits in `plan_slug`
///   (e.g. `"0016"` from `"0016-sidebar-tree"`); if `plan_slug` has no leading
///   digits, use the sanitised `plan_slug` head as a fallback so the name is
///   never empty.
/// - `task-trunc` — `task_id` sanitised to `[a-z0-9-]` and truncated to
///   [`TASK_TRUNC`] chars, trimming any trailing `-`.
/// - `hash4` — 4-char hex hash of `"{plan_slug}--{task_id}"`, so two tasks
///   that truncate to the same prefix still differ.
///
/// Deterministic in `(plan_slug, task_id)`, so `create` and `remove` agree.
pub fn short_worktree_name(plan_slug: &str, task_id: &str) -> String {
    let plan_num = leading_digits(plan_slug);
    let raw_task = sanitize_ns_part(task_id);
    let task = truncate_trim(&raw_task, TASK_TRUNC);
    let composite = format!("{plan_slug}--{task_id}");
    let hash4 = hex4(composite.as_bytes());
    format!("{plan_num}-{task}-{hash4}")
}

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

// ---------------------------------------------------------------------------
// Public helpers — state root
// ---------------------------------------------------------------------------

/// Runtime-state root for a project: `{repo_root}/.makina/`.
///
/// Holds both committed artifacts (`config.toml`, `tasks/`) and transient
/// runtime state (`runs/`, `worktrees/`, `checkpoints/`). Transient
/// directories are listed in `.makina/.gitignore` so they never appear in
/// `git status` or get committed accidentally.
///
/// Infallible: no `$HOME` dependency, no containment check. The state root
/// is always the `.makina` directory inside the repository.
pub fn state_root(repo_root: &Path) -> std::io::Result<PathBuf> {
    Ok(repo_root.join(".makina"))
}

// ---------------------------------------------------------------------------
// Public path helpers — transient runtime state (rooted at state_root)
// ---------------------------------------------------------------------------

/// Returns the directory for a single run:
/// `{repo_root}/.makina/runs/{run_id}`.
///
/// # Example
///
/// ```text
/// /projects/myrepo/.makina/runs/01ABC
/// ```
pub fn run_dir(repo_root: &Path, run_id: &str) -> std::io::Result<PathBuf> {
    Ok(state_root(repo_root)?.join("runs").join(run_id))
}

/// Returns the audit-log path for a run:
/// `{repo_root}/.makina/runs/{run_id}/audit.jsonl`.
///
/// # Example
///
/// ```text
/// /projects/myrepo/.makina/runs/01ABC/audit.jsonl
/// ```
pub fn audit_log(repo_root: &Path, run_id: &str) -> std::io::Result<PathBuf> {
    Ok(run_dir(repo_root, run_id)?.join("audit.jsonl"))
}

/// Returns the per-task log path within a run:
/// `{repo_root}/.makina/runs/{run_id}/logs/{task_slug}.log`.
///
/// # Example
///
/// ```text
/// /projects/myrepo/.makina/runs/01ABC/logs/task-a.log
/// ```
pub fn task_log(repo_root: &Path, run_id: &str, task_slug: &str) -> std::io::Result<PathBuf> {
    Ok(run_dir(repo_root, run_id)?
        .join("logs")
        .join(format!("{task_slug}.log")))
}

/// Returns the transient directory that holds Makina-created task worktrees:
/// `{repo_root}/.makina/worktrees/`.
pub fn worktrees_dir(repo_root: &Path) -> std::io::Result<PathBuf> {
    Ok(state_root(repo_root)?.join("worktrees"))
}

/// Creates (if needed) and returns the per-run log directory:
/// `{repo_root}/.makina/runs/{run_id}/logs`.
///
/// Unlike the other helpers in this module — which are pure, no-I/O path
/// builders — this function performs I/O: it `create_dir_all`s the directory
/// before returning it. Keep this the one clearly-separate I/O helper here.
pub fn run_logs_dir(repo_root: &Path, run_id: &str) -> std::io::Result<PathBuf> {
    let dir = run_dir(repo_root, run_id)?.join("logs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Returns the worktree directory for a task within a plan:
/// `{repo_root}/.makina/worktrees/{short_worktree_name}`.
///
/// The directory leaf uses the bounded short form `{plan#}-{task-trunc}-{hash4}`
/// (see [`short_worktree_name`]) so names stay filesystem-friendly and
/// `create` + `remove` always agree on the same path.
///
/// # Example
///
/// ```text
/// /projects/myrepo/.makina/worktrees/0003-task-a-xxxx
/// ```
pub fn worktree(repo_root: &Path, plan_slug: &str, task_id: &str) -> std::io::Result<PathBuf> {
    Ok(worktrees_dir(repo_root)?.join(short_worktree_name(plan_slug, task_id)))
}

/// Returns the directory holding plan checkpoints:
/// `{repo_root}/.makina/checkpoints/`.
pub fn checkpoints_dir(repo_root: &Path) -> std::io::Result<PathBuf> {
    Ok(state_root(repo_root)?.join("checkpoints"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Committed-artifact helpers — always under repo_root/.makina
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // Transient helpers — rooted at state_root (repo_root/.makina)
    // -----------------------------------------------------------------------

    #[test]
    fn run_dir_path() {
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        let expected = repo_root.join(".makina").join("runs").join("01ABC");
        let got = run_dir(repo_root, "01ABC").unwrap();

        assert_eq!(got, expected);
    }

    #[test]
    fn audit_log_path() {
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        let expected = repo_root
            .join(".makina")
            .join("runs")
            .join("01ABC")
            .join("audit.jsonl");
        let got = audit_log(repo_root, "01ABC").unwrap();

        assert_eq!(got, expected);
    }

    #[test]
    fn task_log_path() {
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        let expected = repo_root
            .join(".makina")
            .join("runs")
            .join("01ABC")
            .join("logs")
            .join("task-a.log");
        let got = task_log(repo_root, "01ABC", "task-a").unwrap();

        assert_eq!(got, expected);
    }

    #[test]
    fn worktree_path() {
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        // The leaf is the short name, not the old plan--task composite.
        let short = short_worktree_name("my-plan", "task-a");
        let expected = repo_root.join(".makina").join("worktrees").join(&short);
        let got = worktree(repo_root, "my-plan", "task-a").unwrap();

        assert_eq!(got, expected);
        // The leaf must use the short-name format (not the old plan--task form).
        assert!(
            !got.to_string_lossy().contains("--"),
            "worktree path must not contain '--' (old delimiter), got {}",
            got.display()
        );
    }

    #[test]
    fn run_logs_dir_creates_and_is_idempotent() {
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        let expected_prefix = repo_root.join(".makina");

        let dir = run_logs_dir(repo_root, "01ABC").expect("first run_logs_dir call");
        assert!(
            dir.starts_with(&expected_prefix),
            "run_logs_dir should live under repo/.makina, got {}",
            dir.display()
        );
        assert!(
            dir.ends_with("runs/01ABC/logs"),
            "returned path should end with runs/01ABC/logs, got {}",
            dir.display()
        );
        assert!(dir.is_dir(), "directory should exist after the call");

        // Second call on the same path must also succeed (idempotent).
        let dir2 = run_logs_dir(repo_root, "01ABC").expect("second run_logs_dir call");
        assert_eq!(dir, dir2);
    }

    // -----------------------------------------------------------------------
    // State root + all helpers live under repo_root/.makina
    // -----------------------------------------------------------------------

    #[test]
    fn transient_helpers_live_under_state_root() {
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        let sr = state_root(repo_root).unwrap();
        let committed_root = repo_root.join(".makina");

        // state_root IS repo_root/.makina now.
        assert_eq!(sr, committed_root, "state_root must be repo_root/.makina");

        // Transient helpers must resolve under state_root (= repo_root/.makina)
        assert!(
            run_dir(repo_root, "r1").unwrap().starts_with(&sr),
            "run_dir must be under state_root"
        );
        assert!(
            worktree(repo_root, "plan", "task")
                .unwrap()
                .starts_with(&sr),
            "worktree must be under state_root"
        );
        assert!(
            task_log(repo_root, "r1", "t1").unwrap().starts_with(&sr),
            "task_log must be under state_root"
        );
        assert!(
            audit_log(repo_root, "r1").unwrap().starts_with(&sr),
            "audit_log must be under state_root"
        );

        // Committed helpers must also stay under repo_root/.makina
        assert!(
            config_file(repo_root).starts_with(&committed_root),
            "config_file must be under repo_root/.makina"
        );
        assert!(
            task_graph(repo_root, "slug").starts_with(&committed_root),
            "task_graph must be under repo_root/.makina"
        );
    }

    // -----------------------------------------------------------------------
    // short_worktree_name — acceptance tests
    // -----------------------------------------------------------------------

    /// The short worktree name must:
    /// - contain only `[a-z0-9-]`
    /// - not start or end with `-`
    /// - start with the plan number (e.g. `"0016-"`)
    /// - stay within a reasonable bound
    #[test]
    fn short_worktree_name_is_bounded_and_valid() {
        let name = short_worktree_name("0016-sidebar-tree", "navigation-panel-item");

        // Must start with plan number
        assert!(
            name.starts_with("0016-"),
            "name must start with plan number, got {name}"
        );

        // Rough upper bound: 4 (plan) + 1 + 20 (task) + 1 + 4 (hash) = 30 chars
        assert!(
            name.len() <= 30,
            "name should be bounded, got {} chars: {name}",
            name.len()
        );

        // Must only contain [a-z0-9-]
        for c in name.chars() {
            assert!(
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-',
                "invalid char '{c}' in name '{name}'"
            );
        }

        // Must not have leading or trailing dashes
        assert!(!name.starts_with('-'), "name must not start with dash");
        assert!(!name.ends_with('-'), "name must not end with dash");
    }

    /// The short worktree name must be:
    /// - deterministic: same `(plan_slug, task_id)` → same name
    /// - distinct: different task IDs under one plan → different names (even if
    ///   truncated heads collide, the `hash4` suffix disambiguates)
    #[test]
    fn short_worktree_name_is_deterministic_and_distinct() {
        // Same inputs → same output (deterministic)
        let n1 = short_worktree_name("0016-sidebar-tree", "navigation-panel");
        let n2 = short_worktree_name("0016-sidebar-tree", "navigation-panel");
        assert_eq!(n1, n2, "same inputs must produce the same name");

        // Different task IDs → different names
        let na = short_worktree_name("0016-sidebar-tree", "task-a");
        let nb = short_worktree_name("0016-sidebar-tree", "task-b");
        assert_ne!(na, nb, "distinct task IDs must produce distinct names");

        // No leading digits → fallback, but still non-empty
        let nfd = short_worktree_name("custom-plan", "task-id");
        assert!(!nfd.is_empty(), "fallback name must not be empty");
        assert!(
            nfd.starts_with("cust"),
            "fallback name should use sanitised plan head: {nfd}"
        );
    }
}
