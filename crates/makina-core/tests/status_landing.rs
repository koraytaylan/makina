use std::{fs, path::Path, process::Command};

use makina_core::landing::{
    DispositionIdentity, OwnedWrite, StatusLandingIdentity, TransactionFailpoint,
    commit_task_claim_with_failpoint, commit_task_disposition, commit_task_status,
    commit_task_status_with_failpoint,
};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().into()
}

#[tokio::test]
async fn phase_b_failpoint_matrix_restores_handled_failures_and_retains_cas_candidate() {
    for point in [
        TransactionFailpoint::Render,
        TransactionFailpoint::Replace,
        TransactionFailpoint::Validate,
        TransactionFailpoint::Commit,
        TransactionFailpoint::Cas,
    ] {
        let repo = repository();
        fs::write(repo.path().join("unrelated"), "operator\n").unwrap();
        let old = git(repo.path(), &["rev-parse", "refs/heads/plan/0048-X"]);
        let identity = StatusLandingIdentity {
            plan: "0048-X".into(),
            task: "x".into(),
            run: "01AAAAAAAAAAAAAAAAAAAAAAAA".into(),
            landing: old.clone(),
        };
        let writes = vec![
            OwnedWrite {
                path: "docs/plans/STATUS.md".into(),
                bytes: b"root B\n".to_vec(),
            },
            OwnedWrite {
                path: "docs/plans/0048-X/STATUS.md".into(),
                bytes: b"status B\n".to_vec(),
            },
            OwnedWrite {
                path: "docs/plans/0048-X/tasks/0101-x.md".into(),
                bytes: b"task B\n".to_vec(),
            },
        ];
        let error = commit_task_status_with_failpoint(
            repo.path(),
            "refs/heads/plan/0048-X",
            &old,
            &writes,
            &identity,
            point,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("injected transaction failure"));
        assert_eq!(
            old,
            git(repo.path(), &["rev-parse", "refs/heads/plan/0048-X"])
        );
        assert_eq!(
            fs::read_to_string(repo.path().join("unrelated")).unwrap(),
            "operator\n"
        );
        if point != TransactionFailpoint::Cas {
            assert_eq!(
                fs::read_to_string(repo.path().join("docs/plans/STATUS.md")).unwrap(),
                "root\n"
            );
            assert_eq!(git(repo.path(), &["diff", "--cached", "--name-only"]), "");
        } else {
            assert_ne!(git(repo.path(), &["rev-parse", "HEAD"]), old);
        }
    }
}

#[tokio::test]
async fn claim_failpoint_matrix_never_publishes_partial_status() {
    for point in [
        TransactionFailpoint::Render,
        TransactionFailpoint::Replace,
        TransactionFailpoint::Validate,
        TransactionFailpoint::Commit,
        TransactionFailpoint::Cas,
    ] {
        let repo = repository();
        let old = git(repo.path(), &["rev-parse", "refs/heads/plan/0048-X"]);
        let identity = StatusLandingIdentity {
            plan: "0048-X".into(),
            task: "x".into(),
            run: "01AAAAAAAAAAAAAAAAAAAAAAAA".into(),
            landing: old.clone(),
        };
        let writes = [OwnedWrite {
            path: "docs/plans/0048-X/tasks/0101-x.md".into(),
            bytes: b"in-progress\n".to_vec(),
        }];
        commit_task_claim_with_failpoint(
            repo.path(),
            "refs/heads/plan/0048-X",
            &old,
            &writes,
            &identity,
            point,
        )
        .await
        .unwrap_err();
        assert_eq!(
            old,
            git(repo.path(), &["rev-parse", "refs/heads/plan/0048-X"])
        );
        if point != TransactionFailpoint::Cas {
            assert_eq!(
                fs::read_to_string(repo.path().join(&writes[0].path)).unwrap(),
                "task\n"
            );
        }
    }
}

#[tokio::test]
async fn disposition_is_cas_guarded_and_records_digest_chain() {
    let repo = repository();
    let old = git(repo.path(), &["rev-parse", "refs/heads/plan/0048-X"]);
    let identity = DispositionIdentity {
        plan: "0048-X".into(),
        task: "x".into(),
        run: "01AAAAAAAAAAAAAAAAAAAAAAAA".into(),
        action: "ungate".into(),
        previous_source_digest: "old-source".into(),
        source_digest: "new-source".into(),
        previous_plan_digest: "old-plan".into(),
        plan_digest: "new-plan".into(),
    };
    let oid = commit_task_disposition(
        repo.path(),
        "refs/heads/plan/0048-X",
        &old,
        &[OwnedWrite {
            path: "docs/plans/0048-X/tasks/0101-x.md".into(),
            bytes: b"ungated\n".to_vec(),
        }],
        &identity,
    )
    .await
    .unwrap();
    let message = git(repo.path(), &["show", "-s", "--format=%B", &oid]);
    for trailer in [
        "Makina-Phase: task-disposition",
        "Makina-Disposition: ungate",
        "Makina-Previous-Source-Digest: old-source",
        "Makina-Source-Digest: new-source",
        "Makina-Previous-Plan-Digest: old-plan",
        "Makina-Plan-Digest: new-plan",
    ] {
        assert!(
            message.lines().any(|line| line == trailer),
            "missing {trailer}"
        );
    }
    let error =
        commit_task_disposition(repo.path(), "refs/heads/plan/0048-X", &old, &[], &identity)
            .await
            .unwrap_err();
    assert!(error.to_string().contains("ref moved"));
}

fn repository() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Test"]);
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    fs::create_dir_all(repo.path().join("docs/plans/0048-X/tasks")).unwrap();
    fs::write(repo.path().join("docs/plans/STATUS.md"), "root\n").unwrap();
    fs::write(repo.path().join("docs/plans/0048-X/STATUS.md"), "status\n").unwrap();
    fs::write(
        repo.path().join("docs/plans/0048-X/tasks/0101-x.md"),
        "task\n",
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "base"]);
    git(repo.path(), &["branch", "plan/0048-X"]);
    repo
}

#[tokio::test]
async fn phase_b_is_exact_idempotent_and_cas_guarded() {
    let repo = repository();
    let old = git(repo.path(), &["rev-parse", "refs/heads/plan/0048-X"]);
    let identity = StatusLandingIdentity {
        plan: "0048-X".into(),
        task: "x".into(),
        run: "01AAAAAAAAAAAAAAAAAAAAAAAA".into(),
        landing: old.clone(),
    };
    let writes = vec![
        OwnedWrite {
            path: "docs/plans/STATUS.md".into(),
            bytes: b"root B\n".to_vec(),
        },
        OwnedWrite {
            path: "docs/plans/0048-X/STATUS.md".into(),
            bytes: b"status B\n".to_vec(),
        },
        OwnedWrite {
            path: "docs/plans/0048-X/tasks/0101-x.md".into(),
            bytes: b"task B\n".to_vec(),
        },
    ];
    let b = commit_task_status(
        repo.path(),
        "refs/heads/plan/0048-X",
        &old,
        &writes,
        &identity,
    )
    .await
    .unwrap();
    assert_eq!(
        b,
        commit_task_status(
            repo.path(),
            "refs/heads/plan/0048-X",
            &old,
            &writes,
            &identity
        )
        .await
        .unwrap()
    );
    let message = git(repo.path(), &["show", "-s", "--format=%B", &b]);
    for trailer in [
        "Makina-Phase: task-status",
        "Makina-Plan: 0048-X",
        "Makina-Task: x",
        "Makina-Run: 01AAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        assert!(message.lines().any(|line| line == trailer));
    }
    assert!(
        message
            .lines()
            .any(|line| line == format!("Makina-Landing: {old}"))
    );
}

#[tokio::test]
async fn phase_b_rejects_unowned_paths_before_mutation() {
    let repo = repository();
    let old = git(repo.path(), &["rev-parse", "refs/heads/plan/0048-X"]);
    let identity = StatusLandingIdentity {
        plan: "0048-X".into(),
        task: "x".into(),
        run: "01AAAAAAAAAAAAAAAAAAAAAAAA".into(),
        landing: old.clone(),
    };
    let error = commit_task_status(
        repo.path(),
        "refs/heads/plan/0048-X",
        &old,
        &[OwnedWrite {
            path: "src/lib.rs".into(),
            bytes: vec![],
        }],
        &identity,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("not coordinator-owned"));
    assert_eq!(
        old,
        git(repo.path(), &["rev-parse", "refs/heads/plan/0048-X"])
    );
}
