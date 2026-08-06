//! Integration coverage for the `makina run` subcommand's non-agent behavior.
//!
//! The orchestration `makina run` performs is already covered end to end by
//! `makina-core`'s `sequential_plan_end_to_end`, which drives the same
//! `RegisterPlan → OpenPlan → StartRun` sequence against a `NoopBackend`. What
//! is *not* covered there is the binary's own contract: argument handling, the
//! guard rails that must fire before any agent is spawned, and the exit codes a
//! script or CI job branches on.
//!
//! These tests deliberately exercise only paths that reject before dispatching a
//! turn, so they never spawn an agent CLI or make a model call.

use std::path::Path;
use std::process::Command;

/// Path to the `makina` binary built for this test run.
///
/// Cargo builds the binary for integration tests but does not export its path,
/// so derive it from the test executable's own location (`target/{profile}/deps/
/// {test}` → `target/{profile}/makina`).
fn makina_bin() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test executable path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("makina")
}

fn run(cwd: &Path, args: &[&str]) -> (i32, String) {
    let output = Command::new(makina_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("makina binary runs");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().unwrap_or(-1), text)
}

/// A missing plan directory is a usage error, not a run attempt.
#[test]
fn run_without_a_plan_dir_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let (code, text) = run(dir.path(), &["run"]);
    assert_eq!(code, 2, "usage errors exit 2; got {text}");
    assert!(text.contains("plan-dir"), "{text}");
}

/// A second positional argument is rejected rather than silently ignored.
#[test]
fn run_rejects_unexpected_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let (code, text) = run(dir.path(), &["run", "docs/plans/0001-x", "extra"]);
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("unexpected"), "{text}");
}

/// Outside a Git repository there is nothing to run, and the message says so.
#[test]
fn run_outside_a_git_repository_reports_the_missing_repo() {
    let dir = tempfile::tempdir().unwrap();
    let (code, text) = run(dir.path(), &["run", "docs/plans/0001-sample"]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("not inside a Git repository"), "{text}");
}

/// A path that is not a plan bundle fails before any agent is spawned.
#[test]
fn run_rejects_a_directory_that_is_not_a_plan_bundle() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        &["init", "-q", "-b", "develop"][..],
        &["config", "user.email", "test@example.invalid"][..],
        &["config", "user.name", "Headless Test"][..],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::create_dir_all(dir.path().join("docs/plans/0001-Absent")).unwrap();
    std::fs::write(dir.path().join("seed"), "seed\n").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["-c", "commit.gpgsign=false", "commit", "-qm", "seed"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );

    let (code, text) = run(dir.path(), &["run", "docs/plans/0001-Absent"]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("not a plan bundle") || text.contains("not loader-valid"),
        "the failure must name the bundle problem; got {text}",
    );
}

/// `--help` documents the subcommand, so operators can discover it.
#[test]
fn help_lists_the_run_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let (code, text) = run(dir.path(), &["--help"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("run <plan-dir>"), "{text}");
}
