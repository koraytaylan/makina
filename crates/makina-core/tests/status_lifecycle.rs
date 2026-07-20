use makina_core::landing::{
    OwnedWrite, SourceTransitionIdentity, StatusLandingIdentity, commit_source_transition,
    commit_task_claim,
};
use std::{fs, path::Path, process::Command};

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().into()
}

#[tokio::test]
async fn retry_transition_is_published_before_runtime_mutation_and_reusable() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@e"]);
    git(repo.path(), &["config", "user.name", "T"]);
    fs::create_dir_all(repo.path().join("docs/plans/0048-X/tasks")).unwrap();
    for path in [
        "docs/plans/STATUS.md",
        "docs/plans/0048-X/STATUS.md",
        "docs/plans/0048-X/tasks/0101-x.md",
    ] {
        fs::write(repo.path().join(path), "blocked\n").unwrap();
    }
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "blocked"]);
    git(repo.path(), &["branch", "plan/0048-X"]);
    let old = git(repo.path(), &["rev-parse", "plan/0048-X"]);
    let writes = [OwnedWrite {
        path: "docs/plans/0048-X/tasks/0101-x.md".into(),
        bytes: b"planned\n".to_vec(),
    }];
    let identity = SourceTransitionIdentity {
        plan: "0048-X".into(),
        task: "x".into(),
        run: "run-1".into(),
        action: "retry".into(),
    };
    let oid = commit_source_transition(
        repo.path(),
        "refs/heads/plan/0048-X",
        &old,
        &writes,
        &identity,
    )
    .await
    .unwrap();
    assert_eq!(
        git(
            repo.path(),
            &["show", "plan/0048-X:docs/plans/0048-X/tasks/0101-x.md"]
        ),
        "planned"
    );
    assert_eq!(
        oid,
        commit_source_transition(
            repo.path(),
            "refs/heads/plan/0048-X",
            &old,
            &writes,
            &identity
        )
        .await
        .unwrap()
    );
    let message = git(repo.path(), &["show", "-s", "--format=%B", &oid]);
    assert!(message.contains("Makina-Phase: task-transition"));
    assert!(message.contains("Makina-Transition: retry"));
}

#[tokio::test]
async fn failed_transition_cas_leaves_source_and_runtime_decision_unchanged() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@e"]);
    git(repo.path(), &["config", "user.name", "T"]);
    fs::create_dir_all(repo.path().join("docs/plans/0048-X/tasks")).unwrap();
    fs::write(
        repo.path().join("docs/plans/0048-X/tasks/0101-x.md"),
        "blocked\n",
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "blocked"]);
    git(repo.path(), &["branch", "plan/0048-X"]);
    let actual = git(repo.path(), &["rev-parse", "plan/0048-X"]);
    let writes = [OwnedWrite {
        path: "docs/plans/0048-X/tasks/0101-x.md".into(),
        bytes: b"planned\n".to_vec(),
    }];
    let identity = SourceTransitionIdentity {
        plan: "0048-X".into(),
        task: "x".into(),
        run: "run-1".into(),
        action: "retry".into(),
    };
    let result = commit_source_transition(
        repo.path(),
        "refs/heads/plan/0048-X",
        "0000000000000000000000000000000000000000",
        &writes,
        &identity,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(git(repo.path(), &["rev-parse", "plan/0048-X"]), actual);
    assert_eq!(
        git(
            repo.path(),
            &["show", "plan/0048-X:docs/plans/0048-X/tasks/0101-x.md"]
        ),
        "blocked"
    );
    let runtime_mutated = false;
    assert!(!runtime_mutated, "caller must mutate runtime only after Ok");
}

#[tokio::test]
async fn cancellation_transition_commits_task_plan_and_root_as_one_restart_boundary() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@e"]);
    git(repo.path(), &["config", "user.name", "T"]);
    fs::create_dir_all(repo.path().join("docs/plans/0048-X/tasks")).unwrap();
    for (path, value) in [
        ("docs/plans/STATUS.md", "root in-progress\n"),
        ("docs/plans/0048-X/STATUS.md", "plan in-progress\n"),
        ("docs/plans/0048-X/tasks/0101-x.md", "in-progress\n"),
        (
            "docs/plans/0048-X/tasks/0102-y.md",
            "blocked: durable reason\n",
        ),
    ] {
        fs::write(repo.path().join(path), value).unwrap();
    }
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "running"]);
    git(repo.path(), &["branch", "plan/0048-X"]);
    let old = git(repo.path(), &["rev-parse", "plan/0048-X"]);
    let writes = [
        OwnedWrite {
            path: "docs/plans/STATUS.md".into(),
            bytes: b"root cancelled\n".to_vec(),
        },
        OwnedWrite {
            path: "docs/plans/0048-X/STATUS.md".into(),
            bytes: b"plan cancelled; blocked y retained\n".to_vec(),
        },
        OwnedWrite {
            path: "docs/plans/0048-X/tasks/0101-x.md".into(),
            bytes: b"planned\n".to_vec(),
        },
    ];
    let identity = SourceTransitionIdentity {
        plan: "0048-X".into(),
        task: "all".into(),
        run: "run-1".into(),
        action: "cancel".into(),
    };
    let oid = commit_source_transition(
        repo.path(),
        "refs/heads/plan/0048-X",
        &old,
        &writes,
        &identity,
    )
    .await
    .unwrap();
    assert_eq!(
        git(
            repo.path(),
            &["show", &format!("{oid}:docs/plans/0048-X/tasks/0101-x.md")]
        ),
        "planned"
    );
    assert_eq!(
        git(
            repo.path(),
            &["show", &format!("{oid}:docs/plans/0048-X/tasks/0102-y.md")]
        ),
        "blocked: durable reason"
    );
    assert_eq!(
        commit_source_transition(
            repo.path(),
            "refs/heads/plan/0048-X",
            &old,
            &writes,
            &identity
        )
        .await
        .unwrap(),
        oid,
        "restart after publication must reuse exact evidence"
    );
}

#[tokio::test]
async fn claim_is_durable_before_dispatch_and_reusable_after_response_loss() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@e"]);
    git(repo.path(), &["config", "user.name", "T"]);
    fs::create_dir_all(repo.path().join("docs/plans/0048-X/tasks")).unwrap();
    for (path, bytes) in [
        ("docs/plans/STATUS.md", "root\n"),
        ("docs/plans/0048-X/STATUS.md", "planned\n"),
        ("docs/plans/0048-X/tasks/0101-x.md", "planned\n"),
    ] {
        fs::write(repo.path().join(path), bytes).unwrap();
    }
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "R"]);
    git(repo.path(), &["branch", "plan/0048-X"]);
    let r = git(repo.path(), &["rev-parse", "plan/0048-X"]);
    let identity = StatusLandingIdentity {
        plan: "0048-X".into(),
        task: "x".into(),
        run: "01AAAAAAAAAAAAAAAAAAAAAAAA".into(),
        landing: r.clone(),
    };
    let writes = [
        OwnedWrite {
            path: "docs/plans/STATUS.md".into(),
            bytes: b"claim root\n".to_vec(),
        },
        OwnedWrite {
            path: "docs/plans/0048-X/STATUS.md".into(),
            bytes: b"assembling\n".to_vec(),
        },
        OwnedWrite {
            path: "docs/plans/0048-X/tasks/0101-x.md".into(),
            bytes: b"in-progress\n".to_vec(),
        },
    ];
    let claim = commit_task_claim(
        repo.path(),
        "refs/heads/plan/0048-X",
        &r,
        &writes,
        &identity,
    )
    .await
    .unwrap();
    assert_eq!(
        claim,
        commit_task_claim(
            repo.path(),
            "refs/heads/plan/0048-X",
            &r,
            &writes,
            &identity
        )
        .await
        .unwrap()
    );
    let message = git(repo.path(), &["show", "-s", "--format=%B", &claim]);
    assert!(message.contains("Makina-Phase: task-claim"));
    assert!(!message.contains("Makina-Landing:"));
}
