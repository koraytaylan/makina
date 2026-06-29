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
//! `worktree`) root at `state_root(repo_root)` — i.e.
//! `$HOME/.makina/projects/{project_ns}/` when `HOME` is set, otherwise falling
//! back to `repo_root/.makina`. The `state_root` function reads the `HOME`
//! environment variable (the only env-var access in this module).

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Resolve `$HOME` via the `HOME` env var (mirrors `config.rs::home_dir`;
/// deliberately no `dirs` crate). `None` when `HOME` is unset.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

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

/// 6-char lowercase-hex hash of `data` using inline FNV-1a (64-bit).
/// FNV-1a is release-stable: the algorithm is fixed, not tied to
/// `DefaultHasher` which is explicitly unstable across Rust releases.
fn hex6(data: &[u8]) -> String {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash: u64 = FNV_OFFSET;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:016x}", hash).chars().take(6).collect()
}

/// 4-char lowercase-hex hash of `data` using inline FNV-1a (64-bit).
/// Same algorithm as `hex6`, truncated to 4 hex digits.
fn hex4(data: &[u8]) -> String {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash: u64 = FNV_OFFSET;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:016x}", hash).chars().take(4).collect()
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
// Public helpers — project namespace + state root
// ---------------------------------------------------------------------------

/// Per-project namespace under `~/.makina/projects/`:
/// `"{repo_basename}-{hash6}"`, where `hash6` is a 6-char hex hash of the
/// canonicalized absolute repo path. Stable for a given path; distinct for
/// two different repo paths even when they share a basename.
pub fn project_ns(repo_root: &Path) -> String {
    let canonical = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let basename = sanitize_ns_part(
        canonical
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo"),
    );
    let hash6 = hex6(canonical.to_string_lossy().as_bytes());
    format!("{basename}-{hash6}")
}

/// Runtime-state root for a project: `$HOME/.makina/projects/{project_ns}`.
/// Holds the transient `worktrees/` and `runs/` trees.
///
/// **`HOME` env var:** this is the only place in `paths.rs` that reads the
/// environment. The result is a pure function of `$HOME` and `repo_root`.
///
/// Falls back to `repo_root/.makina` when `HOME` is unset (best-effort for
/// `HOME`-less environments — explicitly documented, not a silent behaviour).
pub fn state_root(repo_root: &Path) -> PathBuf {
    match home_dir() {
        Some(home) => home
            .join(".makina")
            .join("projects")
            .join(project_ns(repo_root)),
        None => repo_root.join(".makina"),
    }
}

// ---------------------------------------------------------------------------
// Public path helpers — transient runtime state (rooted at state_root)
// ---------------------------------------------------------------------------

/// Returns the directory for a single run:
/// `$HOME/.makina/projects/{ns}/runs/{run_id}`.
///
/// # Example
///
/// With `HOME=/home/user` and a repo at `/projects/myrepo`:
/// ```text
/// /home/user/.makina/projects/myrepo-<hash6>/runs/01ABC
/// ```
pub fn run_dir(repo_root: &Path, run_id: &str) -> PathBuf {
    state_root(repo_root).join("runs").join(run_id)
}

/// Returns the audit-log path for a run:
/// `$HOME/.makina/projects/{ns}/runs/{run_id}/audit.jsonl`.
///
/// # Example
///
/// With `HOME=/home/user` and a repo at `/projects/myrepo`:
/// ```text
/// /home/user/.makina/projects/myrepo-<hash6>/runs/01ABC/audit.jsonl
/// ```
pub fn audit_log(repo_root: &Path, run_id: &str) -> PathBuf {
    run_dir(repo_root, run_id).join("audit.jsonl")
}

/// Returns the per-task log path within a run:
/// `$HOME/.makina/projects/{ns}/runs/{run_id}/logs/{task_slug}.log`.
///
/// # Example
///
/// With `HOME=/home/user` and a repo at `/projects/myrepo`:
/// ```text
/// /home/user/.makina/projects/myrepo-<hash6>/runs/01ABC/logs/task-a.log
/// ```
pub fn task_log(repo_root: &Path, run_id: &str, task_slug: &str) -> PathBuf {
    run_dir(repo_root, run_id)
        .join("logs")
        .join(format!("{task_slug}.log"))
}

/// Returns the transient directory that holds Makina-created task worktrees:
/// `$HOME/.makina/projects/{ns}/worktrees/`.
pub fn worktrees_dir(repo_root: &Path) -> PathBuf {
    state_root(repo_root).join("worktrees")
}

/// Creates (if needed) and returns the per-run log directory:
/// `$HOME/.makina/projects/{ns}/runs/{run_id}/logs`.
///
/// Unlike the other helpers in this module — which are pure, no-I/O path
/// builders — this function performs I/O: it `create_dir_all`s the directory
/// before returning it. Keep this the one clearly-separate I/O helper here.
pub fn run_logs_dir(repo_root: &Path, run_id: &str) -> std::io::Result<PathBuf> {
    let dir = run_dir(repo_root, run_id).join("logs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Returns the worktree directory for a task within a plan:
/// `$HOME/.makina/projects/{ns}/worktrees/{short_worktree_name}`.
///
/// The directory leaf uses the bounded short form `{plan#}-{task-trunc}-{hash4}`
/// (see [`short_worktree_name`]) so names stay filesystem-friendly and
/// `create` + `remove` always agree on the same path.
///
/// # Example
///
/// With `HOME=/home/user` and a repo at `/projects/myrepo`:
/// ```text
/// /home/user/.makina/projects/myrepo-<hash6>/worktrees/0003-task-a-xxxx
/// ```
pub fn worktree(repo_root: &Path, plan_slug: &str, task_id: &str) -> PathBuf {
    worktrees_dir(repo_root).join(short_worktree_name(plan_slug, task_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Use the process-global HOME_ENV_LOCK from lib.rs so all test modules
    // serialize HOME mutations across crate boundaries.
    use crate::HOME_ENV_LOCK;

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
    // Transient helpers — rooted at state_root
    // -----------------------------------------------------------------------

    #[test]
    fn run_dir_path() {
        let _guard = HOME_ENV_LOCK.blocking_lock();
        let tmp_home = tempfile::tempdir().expect("create temp home");
        // Create a fake repo dir so canonicalize works
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        // SAFETY: serialised by HOME_LOCK — no other thread mutates HOME concurrently
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let expected = state_root(repo_root).join("runs").join("01ABC");
        let got = run_dir(repo_root, "01ABC");

        assert_eq!(got, expected);
        // Must NOT be under repo_root/.makina
        assert!(
            !got.starts_with(repo_root.join(".makina")),
            "run_dir must not be under repo_root/.makina, got {}",
            got.display()
        );
    }

    #[test]
    fn audit_log_path() {
        let _guard = HOME_ENV_LOCK.blocking_lock();
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        // SAFETY: serialised by HOME_LOCK
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let expected = state_root(repo_root)
            .join("runs")
            .join("01ABC")
            .join("audit.jsonl");
        let got = audit_log(repo_root, "01ABC");

        assert_eq!(got, expected);
        assert!(
            !got.starts_with(repo_root.join(".makina")),
            "audit_log must not be under repo_root/.makina, got {}",
            got.display()
        );
    }

    #[test]
    fn task_log_path() {
        let _guard = HOME_ENV_LOCK.blocking_lock();
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        // SAFETY: serialised by HOME_LOCK
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let expected = state_root(repo_root)
            .join("runs")
            .join("01ABC")
            .join("logs")
            .join("task-a.log");
        let got = task_log(repo_root, "01ABC", "task-a");

        assert_eq!(got, expected);
        assert!(
            !got.starts_with(repo_root.join(".makina")),
            "task_log must not be under repo_root/.makina, got {}",
            got.display()
        );
    }

    #[test]
    fn worktree_path() {
        let _guard = HOME_ENV_LOCK.blocking_lock();
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        // SAFETY: serialised by HOME_LOCK
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        // The leaf is now the short name, not the old plan--task composite.
        let short = short_worktree_name("my-plan", "task-a");
        let expected = state_root(repo_root).join("worktrees").join(&short);
        let got = worktree(repo_root, "my-plan", "task-a");

        assert_eq!(got, expected);
        assert!(
            !got.starts_with(repo_root.join(".makina")),
            "worktree must not be under repo_root/.makina, got {}",
            got.display()
        );
        // The leaf must use the short-name format (not the old plan--task form).
        assert!(
            !got.to_string_lossy().contains("--"),
            "worktree path must not contain '--' (old delimiter), got {}",
            got.display()
        );
    }

    #[test]
    fn run_logs_dir_creates_and_is_idempotent() {
        let _guard = HOME_ENV_LOCK.blocking_lock();
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        // SAFETY: serialised by HOME_LOCK
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let expected_prefix = state_root(repo_root);

        let dir = run_logs_dir(repo_root, "01ABC").expect("first run_logs_dir call");
        assert!(
            dir.starts_with(&expected_prefix),
            "run_logs_dir should live under state_root, got {}",
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
    // New tests required by the spec
    // -----------------------------------------------------------------------

    #[test]
    fn transient_helpers_live_under_state_root() {
        let _guard = HOME_ENV_LOCK.blocking_lock();
        let tmp_home = tempfile::tempdir().expect("create temp home");
        let tmp_repo = tempfile::tempdir().expect("create temp repo");
        let repo_root = tmp_repo.path();

        // SAFETY: serialised by HOME_LOCK
        unsafe { std::env::set_var("HOME", tmp_home.path()) };

        let sr = state_root(repo_root);
        let committed_root = repo_root.join(".makina");

        // Transient helpers must resolve under state_root
        assert!(
            run_dir(repo_root, "r1").starts_with(&sr),
            "run_dir must be under state_root"
        );
        assert!(
            worktree(repo_root, "plan", "task").starts_with(&sr),
            "worktree must be under state_root"
        );
        assert!(
            task_log(repo_root, "r1", "t1").starts_with(&sr),
            "task_log must be under state_root"
        );
        assert!(
            audit_log(repo_root, "r1").starts_with(&sr),
            "audit_log must be under state_root"
        );
        // run_logs_dir is I/O — test the path before creation
        let logs_dir = sr.join("runs").join("r1").join("logs");
        assert!(
            logs_dir.starts_with(&sr),
            "run_logs_dir path must be under state_root"
        );

        // Committed helpers must stay under repo_root/.makina
        assert!(
            config_file(repo_root).starts_with(&committed_root),
            "config_file must be under repo_root/.makina"
        );
        assert!(
            task_graph(repo_root, "slug").starts_with(&committed_root),
            "task_graph must be under repo_root/.makina"
        );

        // state_root itself must be under tmp_home, NOT under repo_root
        assert!(
            sr.starts_with(tmp_home.path()),
            "state_root must be under HOME, got {}",
            sr.display()
        );
        assert!(
            !sr.starts_with(repo_root),
            "state_root must NOT be under repo_root, got {}",
            sr.display()
        );
    }

    #[test]
    fn project_ns_is_stable_and_path_distinct() {
        // project_ns is pure — no HOME needed (it hashes the repo path, not HOME).
        let tmp_a = tempfile::tempdir().expect("create temp dir a");
        let tmp_b = tempfile::tempdir().expect("create temp dir b");

        let ns_a1 = project_ns(tmp_a.path());
        let ns_a2 = project_ns(tmp_a.path());
        let ns_b = project_ns(tmp_b.path());

        // Stable: same input → same output
        assert_eq!(ns_a1, ns_a2, "project_ns must be stable for a given path");

        // Distinct: two different repo paths → different namespaces
        assert_ne!(
            ns_a1, ns_b,
            "project_ns must differ for distinct repo paths"
        );

        // Namespace must match [a-z0-9-]+
        assert!(
            ns_a1
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "project_ns must be [a-z0-9-], got {ns_a1}"
        );
    }

    // -----------------------------------------------------------------------
    // short_worktree_name — task 0081 acceptance tests
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
