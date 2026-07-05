//! Hermetic git test helpers shared across the workspace test suites.
//! Neutralizes global/system git config on every spawned git AND writes a
//! repo-local `commit.gpgsign=false` shield, so engine-spawned git (which must
//! honour real user config and is never env-neutralized) is shielded by the
//! repo itself even on a host with `commit.gpgsign = true`.
use std::path::Path;
use std::process::{Command, Output};

/// Run `git <args>` in `dir` with global/system config neutralized; assert success.
pub fn run_git(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Temp git repo on `develop` with one initial commit, hermetic against host
/// git config. Keep the returned TempDir alive.
pub fn setup_temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path();
    init_git_repo_with_identity(path);
    dir
}

/// Initialize a git repo at `path` on a `develop` branch with one initial
/// commit and hermetic identity. Hermetic against host git config.
pub fn init_git_repo_with_identity(path: &Path) {
    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["config", "commit.gpgsign", "false"]);
    run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);
    let head = run_git(path, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let current = String::from_utf8_lossy(&head.stdout).trim().to_string();
    if current != "develop" {
        run_git(path, &["branch", "-m", &current, "develop"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[should_panic(expected = "failed")]
    fn run_git_panics_on_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["definitely-not-a-git-subcommand"]);
    }
    #[test]
    fn setup_temp_repo_is_on_develop_with_initial_commit() {
        let repo = setup_temp_repo();
        let br = run_git(repo.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(String::from_utf8_lossy(&br.stdout).trim(), "develop");
        let log = run_git(repo.path(), &["log", "--oneline"]);
        assert!(String::from_utf8_lossy(&log.stdout).contains("Initial commit"));
    }
    #[test]
    fn temp_repo_commits_despite_poisoned_global_config() {
        // Poison THIS child git's global config with gpgsign=true (no signing key
        // configured). The repo-local commit.gpgsign=false shield must still win.
        let poison = tempfile::tempdir().unwrap();
        let cfg = poison.path().join("gitconfig");
        std::fs::write(&cfg, "[commit]\n\tgpgsign = true\n").unwrap();
        let repo = setup_temp_repo();
        let out = Command::new("git")
            .args(["commit", "--allow-empty", "-m", "second"])
            .current_dir(repo.path())
            .env("GIT_CONFIG_GLOBAL", &cfg)
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "local commit.gpgsign=false must shield against poisoned global: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
