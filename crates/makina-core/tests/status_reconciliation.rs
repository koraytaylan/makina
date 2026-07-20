use makina_core::landing::{TaskEvidenceState, inspect_task_evidence};
use std::{fs, path::Path, process::Command};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn commit(repo: &Path, body: &str, value: &str) -> String {
    fs::write(repo.join("evidence"), value).unwrap();
    git(repo, &["add", "evidence"]);
    git(repo, &["commit", "-qm", body]);
    git(repo, &["rev-parse", "HEAD"])
}

fn repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@e"]);
    git(repo.path(), &["config", "user.name", "T"]);
    repo
}

#[tokio::test]
async fn registration_claim_landing_and_bookkeeping_are_distinct() {
    let repo = repo();
    commit(
        repo.path(),
        "R\n\nMakina-Phase: plan-registration\nMakina-Plan: 0048-X",
        "r",
    );
    assert_eq!(
        inspect_task_evidence(repo.path(), "HEAD", "0048-X", "x")
            .await
            .unwrap(),
        TaskEvidenceState::RegistrationOnly
    );
    commit(
        repo.path(),
        "claim\n\nMakina-Phase: task-claim\nMakina-Plan: 0048-X\nMakina-Task: x\nMakina-Run: run-1",
        "claim",
    );
    assert!(matches!(
        inspect_task_evidence(repo.path(), "HEAD", "0048-X", "x")
            .await
            .unwrap(),
        TaskEvidenceState::Claimed { .. }
    ));
    let landing = commit(
        repo.path(),
        "A\n\nMakina-Plan: 0048-X\nMakina-Task: x\nMakina-Run: run-1",
        "a",
    );
    assert_eq!(
        inspect_task_evidence(repo.path(), "HEAD", "0048-X", "x")
            .await
            .unwrap(),
        TaskEvidenceState::LandingPending {
            implementation_oid: landing.clone()
        }
    );
    let status = commit(
        repo.path(),
        &format!(
            "B\n\nMakina-Phase: task-status\nMakina-Plan: 0048-X\nMakina-Task: x\nMakina-Run: run-1\nMakina-Landing: {landing}"
        ),
        "b",
    );
    assert_eq!(
        inspect_task_evidence(repo.path(), "HEAD", "0048-X", "x")
            .await
            .unwrap(),
        TaskEvidenceState::Complete {
            implementation_oid: landing,
            status_oid: status
        }
    );
}

#[tokio::test]
async fn source_first_reset_supersedes_volatile_claim_without_erasing_it() {
    let repo = repo();
    commit(
        repo.path(),
        "claim\n\nMakina-Phase: task-claim\nMakina-Plan: 0048-X\nMakina-Task: x\nMakina-Run: run-1",
        "claim",
    );
    commit(
        repo.path(),
        "cancel\n\nMakina-Phase: task-transition\nMakina-Plan: 0048-X\nMakina-Task: all\nMakina-Run: run-1\nMakina-Transition: cancel\nMakina-Previous-Plan: old",
        "planned",
    );
    assert_eq!(
        inspect_task_evidence(repo.path(), "HEAD", "0048-X", "x")
            .await
            .unwrap(),
        TaskEvidenceState::RegistrationOnly
    );
    assert_eq!(git(repo.path(), &["log", "--format=%s"]), "cancel\nclaim");
}

#[tokio::test]
async fn bad_or_duplicate_b_evidence_fails_closed() {
    let repo = repo();
    let landing = commit(
        repo.path(),
        "A\n\nMakina-Plan: 0048-X\nMakina-Task: x\nMakina-Run: run-1",
        "a",
    );
    commit(
        repo.path(),
        "bad B\n\nMakina-Phase: task-status\nMakina-Plan: 0048-X\nMakina-Task: x\nMakina-Run: run-1\nMakina-Landing: wrong",
        "b",
    );
    assert!(
        inspect_task_evidence(repo.path(), "HEAD", "0048-X", "x")
            .await
            .is_err()
    );
    assert!(!landing.is_empty());
}

#[tokio::test]
async fn reset_archives_exact_plan_and_task_tips_idempotently() {
    let repo = repo();
    commit(repo.path(), "R", "r");
    git(repo.path(), &["branch", "plan/0048-X"]);
    let task_branch = format!(
        "task/{}",
        makina_core::paths::short_worktree_name("0048-X", "x")
    );
    git(repo.path(), &["branch", &task_branch]);
    let tip = git(repo.path(), &["rev-parse", "HEAD"]);
    let manager =
        makina_core::worktree::WorktreeManager::new(repo.path().to_path_buf(), "master".into());
    let refs = manager
        .archive_run_refs("0048-X", "run-1", &["x".into()])
        .await
        .unwrap();
    assert_eq!(refs.len(), 2);
    for recovery in &refs {
        assert_eq!(git(repo.path(), &["rev-parse", recovery]), tip);
    }
    assert_eq!(
        manager
            .archive_run_refs("0048-X", "run-1", &["x".into()])
            .await
            .unwrap(),
        refs
    );
    assert_eq!(
        git(repo.path(), &["rev-parse", "refs/heads/plan/0048-X"]),
        tip,
        "archival must retain active evidence until later verified cleanup"
    );
}

#[tokio::test]
async fn stale_claim_requires_exact_clean_branch_and_worktree() {
    let _guard = makina_core::HOME_ENV_LOCK.lock().await;
    let repo = repo();
    let state = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", state.path()) };
    let claim = commit(repo.path(), "claim", "claim");
    let task_branch = format!(
        "task/{}",
        makina_core::paths::short_worktree_name("0048-X", "x")
    );
    git(repo.path(), &["branch", &task_branch, &claim]);
    let manager =
        makina_core::worktree::WorktreeManager::new(repo.path().to_path_buf(), "master".into());
    manager
        .prove_stale_claim_clean("0048-X", "x", &claim)
        .await
        .unwrap();
    let path = makina_core::paths::worktree(repo.path(), "0048-X", "x").unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    git(
        repo.path(),
        &["worktree", "add", path.to_str().unwrap(), &task_branch],
    );
    fs::write(path.join("untracked.txt"), "recovery").unwrap();
    assert!(
        manager
            .prove_stale_claim_clean("0048-X", "x", &claim)
            .await
            .is_err()
    );
    fs::remove_file(path.join("untracked.txt")).unwrap();
    manager
        .prove_stale_claim_clean("0048-X", "x", &claim)
        .await
        .unwrap();
    fs::write(path.join("evidence"), "diverged").unwrap();
    git(&path, &["add", "evidence"]);
    git(&path, &["commit", "-qm", "diverged task branch"]);
    assert!(
        manager
            .prove_stale_claim_clean("0048-X", "x", &claim)
            .await
            .is_err(),
        "a clean worktree on a divergent task branch remains recovery evidence"
    );
    if let Some(home) = old_home {
        unsafe { std::env::set_var("HOME", home) }
    } else {
        unsafe { std::env::remove_var("HOME") }
    }
}
