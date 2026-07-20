use std::path::Path;
use std::process::Command;

use makina_core::merge::{LandingEvidenceStatus, MergeOutcome, SquashMerger, TaskLandingIdentity};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().into()
}

fn repo(format: &str) -> Option<tempfile::TempDir> {
    let repo = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args([
            "init",
            "-q",
            "-b",
            "develop",
            &format!("--object-format={format}"),
        ])
        .current_dir(repo.path())
        .status()
        .unwrap();
    if !status.success() {
        return None;
    }
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Test"]);
    std::fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "base"]);
    Some(repo)
}

fn identity() -> TaskLandingIdentity {
    TaskLandingIdentity {
        plan: "0048-Per-Task-Plan-Documents-And-Transactional-Status".into(),
        task: "capture-task-landing-evidence".into(),
        run: "01LANDINGEVIDENCE".into(),
    }
}

#[tokio::test]
async fn phase_a_returns_full_commit_oid_and_exact_trailers_and_reuses_it() {
    for format in ["sha1", "sha256"] {
        let Some(repo) = repo(format) else { continue };
        git(repo.path(), &["checkout", "-b", "task/evidence"]);
        std::fs::write(repo.path().join("task.txt"), "task\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "task branch tip"]);
        let task_tip = git(repo.path(), &["rev-parse", "HEAD"]);
        git(repo.path(), &["checkout", "develop"]);
        let merger = SquashMerger::new(repo.path().to_owned(), "develop".into());
        let MergeOutcome::Merged { oid } = merger
            .squash_merge_with_evidence("task/evidence", "land task", &identity())
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(oid.as_str().len(), if format == "sha256" { 64 } else { 40 });
        assert_eq!(oid.as_str(), git(repo.path(), &["rev-parse", "develop"]));
        assert_ne!(oid.as_str(), task_tip);
        assert_eq!(
            merger
                .verify_task_landing_oid(oid.as_str(), &identity())
                .await
                .unwrap(),
            LandingEvidenceStatus::Verified(oid.clone())
        );
        let body = git(repo.path(), &["show", "-s", "--format=%B", oid.as_str()]);
        assert!(body.contains(&format!("Makina-Plan: {}", identity().plan)));
        assert!(body.contains(&format!("Makina-Task: {}", identity().task)));
        assert!(body.contains(&format!("Makina-Run: {}", identity().run)));
        let count = git(repo.path(), &["rev-list", "--count", "develop"]);
        let MergeOutcome::Merged { oid: reused } = merger
            .squash_merge_with_evidence("task/evidence", "ignored", &identity())
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(reused, oid);
        assert_eq!(git(repo.path(), &["rev-list", "--count", "develop"]), count);
    }
}

#[tokio::test]
async fn wrong_or_ambiguous_evidence_never_fabricates_an_oid() {
    let repo = repo("sha1").unwrap();
    let merger = SquashMerger::new(repo.path().to_owned(), "develop".into());
    assert_eq!(
        merger.find_task_landing(&identity()).await.unwrap(),
        LandingEvidenceStatus::Missing
    );
    assert_eq!(
        merger
            .verify_task_landing_oid("deadbeef", &identity())
            .await
            .unwrap(),
        LandingEvidenceStatus::Mismatched
    );
    let mut invalid = identity();
    invalid.run = "bad\nrun".into();
    assert!(merger.find_task_landing(&invalid).await.is_err());

    let message = format!(
        "manual A\n\nMakina-Plan: {}\nMakina-Task: {}\nMakina-Run: {}",
        identity().plan,
        identity().task,
        identity().run
    );
    git(repo.path(), &["commit", "--allow-empty", "-m", &message]);
    git(repo.path(), &["commit", "--allow-empty", "-m", &message]);
    assert_eq!(
        merger.find_task_landing(&identity()).await.unwrap(),
        LandingEvidenceStatus::Ambiguous
    );
}
