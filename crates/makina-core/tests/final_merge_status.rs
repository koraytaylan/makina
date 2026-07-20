use std::{fs, path::Path, process::Command};

use makina_core::landing::{
    FinalizationIdentity, OwnedWrite, commit_final_integration, commit_finalization_completion,
    commit_finalization_prepared, verify_manual_final_integration,
};

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().into()
}

fn repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "develop"]);
    git(
        repo.path(),
        &["config", "user.email", "final@example.invalid"],
    );
    git(repo.path(), &["config", "user.name", "Final Test"]);
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
async fn p_f_c_are_cas_linked_and_completion_records_f() {
    for (mode, merge_commit) in [("squash", false), ("stage", false), ("merge-commit", true)] {
        let repo = repo();
        let base = git(repo.path(), &["rev-parse", "develop"]);
        let identity = FinalizationIdentity {
            plan: "0048-X".into(),
            run: "01FINAL".into(),
            mode: mode.into(),
            expected_base: base.clone(),
        };
        let p = commit_finalization_prepared(
            repo.path(),
            "refs/heads/plan/0048-X",
            "refs/heads/develop",
            &base,
            &base,
            &[OwnedWrite {
                path: "docs/plans/0048-X/STATUS.md".into(),
                bytes: b"pending\n".to_vec(),
            }],
            &identity,
            true,
        )
        .await
        .unwrap();
        git(repo.path(), &["checkout", "--detach", &base]);
        let f = commit_final_integration(
            repo.path(),
            "refs/heads/develop",
            &base,
            &p,
            &identity,
            merge_commit,
            &[("x".into(), base.clone())],
        )
        .await
        .unwrap();
        let c = commit_finalization_completion(
            repo.path(),
            "refs/heads/develop",
            &f,
            &[OwnedWrite {
                path: "docs/plans/0048-X/STATUS.md".into(),
                bytes: format!("complete {f}\n").into_bytes(),
            }],
            &identity,
        )
        .await
        .unwrap();
        assert_eq!(git(repo.path(), &["rev-parse", "develop"]), c);
        let c_message = git(repo.path(), &["show", "-s", "--format=%B", &c]);
        assert!(c_message.contains(&format!("Makina-Final-Commit: {f}")));
        assert_eq!(
            git(
                repo.path(),
                &["show", &format!("{c}:docs/plans/0048-X/STATUS.md")]
            ),
            format!("complete {f}")
        );
        assert_eq!(
            commit_finalization_completion(repo.path(), "refs/heads/develop", &f, &[], &identity)
                .await
                .unwrap(),
            c
        );
    }
}

#[tokio::test]
async fn manual_requires_exact_base_tip_tree_parents_and_trailers() {
    let repo = repo();
    let base = git(repo.path(), &["rev-parse", "develop"]);
    let identity = FinalizationIdentity {
        plan: "0048-X".into(),
        run: "01FINAL".into(),
        mode: "manual".into(),
        expected_base: base.clone(),
    };
    let p = commit_finalization_prepared(
        repo.path(),
        "refs/heads/plan/0048-X",
        "refs/heads/develop",
        &base,
        &base,
        &[OwnedWrite {
            path: "docs/plans/0048-X/STATUS.md".into(),
            bytes: b"pending\n".to_vec(),
        }],
        &identity,
        true,
    )
    .await
    .unwrap();
    git(repo.path(), &["checkout", "--detach", &base]);
    let tree = git(repo.path(), &["rev-parse", &format!("{p}^{{tree}}")]);
    let message = format!(
        "manual final\n\nMakina-Phase: final-integration\nMakina-Final-Mode: manual\nMakina-Plan: 0048-X\nMakina-Run: 01FINAL\nMakina-Plan-Tip: {p}"
    );
    let f = git(
        repo.path(),
        &["commit-tree", &tree, "-p", &base, "-m", &message],
    );
    git(
        repo.path(),
        &["update-ref", "refs/heads/develop", &f, &base],
    );
    assert_eq!(
        verify_manual_final_integration(
            repo.path(),
            "refs/heads/develop",
            &base,
            &p,
            &f,
            &identity
        )
        .await
        .unwrap(),
        f
    );
}

#[tokio::test]
async fn reprepare_creates_p2_without_rewriting_p() {
    let repo = repo();
    let base = git(repo.path(), &["rev-parse", "develop"]);
    let identity = FinalizationIdentity {
        plan: "0048-X".into(),
        run: "01FINAL".into(),
        mode: "squash".into(),
        expected_base: base.clone(),
    };
    let p = commit_finalization_prepared(
        repo.path(),
        "refs/heads/plan/0048-X",
        "refs/heads/develop",
        &base,
        &base,
        &[OwnedWrite {
            path: "docs/plans/0048-X/STATUS.md".into(),
            bytes: b"pending one\n".to_vec(),
        }],
        &identity,
        true,
    )
    .await
    .unwrap();
    git(repo.path(), &["checkout", "develop"]);
    fs::write(repo.path().join("advance.txt"), "advance\n").unwrap();
    git(repo.path(), &["add", "advance.txt"]);
    git(repo.path(), &["commit", "-qm", "advance"]);
    let base2 = git(repo.path(), &["rev-parse", "develop"]);
    let identity2 = FinalizationIdentity {
        expected_base: base2.clone(),
        ..identity
    };
    let p2 = commit_finalization_prepared(
        repo.path(),
        "refs/heads/plan/0048-X",
        "refs/heads/develop",
        &p,
        &base2,
        &[OwnedWrite {
            path: "docs/plans/0048-X/STATUS.md".into(),
            bytes: b"pending two\n".to_vec(),
        }],
        &identity2,
        false,
    )
    .await
    .unwrap();
    assert_ne!(p, p2);
    assert_eq!(git(repo.path(), &["rev-parse", &format!("{p2}^")]), p);
    assert_eq!(git(repo.path(), &["rev-parse", "plan/0048-X"]), p2);
}
