//! Phase A must publish the plan ref so Phase B's expected-old CAS can land.
//!
//! The transactional run drives Phase A/B inside the run's **integration
//! workspace**, which the claim/status landings leave on a DETACHED HEAD (they
//! `checkout --detach {expected_old}`, commit, then CAS the plan ref). A Phase A
//! that only commits on `HEAD` therefore leaves `refs/heads/plan/{slug}` behind
//! the landing commit, and Phase B — which passes the landing OID as its
//! `expected_old` — fails with `RefMoved`, stranding the task in `InReview`.

use std::path::Path;
use std::process::Command;

use makina_core::landing::{OwnedWrite, StatusLandingIdentity, commit_task_status};
use makina_core::merge::{LandingEvidenceStatus, MergeOutcome, SquashMerger, TaskLandingIdentity};

const PLAN: &str = "0001-todo-core";
const TASK: &str = "add-task-toggle";
const RUN: &str = "01KZ2A19SW75EHAG4P7960T0YF";

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().into()
}

fn task_identity() -> TaskLandingIdentity {
    TaskLandingIdentity {
        plan: PLAN.into(),
        task: TASK.into(),
        run: RUN.into(),
    }
}

fn status_identity(landing: &str) -> StatusLandingIdentity {
    StatusLandingIdentity {
        plan: PLAN.into(),
        task: TASK.into(),
        run: RUN.into(),
        landing: landing.into(),
    }
}

fn status_writes() -> Vec<OwnedWrite> {
    vec![
        OwnedWrite {
            path: format!("docs/plans/{PLAN}/tasks/0101-add-task-toggle.md").into(),
            bytes: b"status: done\n".to_vec(),
        },
        OwnedWrite {
            path: format!("docs/plans/{PLAN}/STATUS.md").into(),
            bytes: b"done: 1\n".to_vec(),
        },
    ]
}

/// Build the exact production topology: a repo whose plan branch is only ever
/// reached through a detached integration workspace, plus a task branch forked
/// from the plan tip.  Returns `(repo, integration workspace, plan branch)`.
fn transactional_repo() -> (tempfile::TempDir, std::path::PathBuf, String) {
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path().to_owned();
    git(&root, &["init", "-q", "-b", "develop", "."]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(root.join(format!("docs/plans/{PLAN}/tasks"))).unwrap();
    std::fs::write(root.join("docs/plans/STATUS.md"), "plans\n").unwrap();
    std::fs::write(
        root.join(format!("docs/plans/{PLAN}/STATUS.md")),
        "done: 0\n",
    )
    .unwrap();
    std::fs::write(
        root.join(format!("docs/plans/{PLAN}/tasks/0101-add-task-toggle.md")),
        "status: todo\n",
    )
    .unwrap();
    std::fs::write(root.join("src.txt"), "base\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "scaffold"]);

    let plan_branch = format!("plan/{PLAN}");
    git(&root, &["branch", &plan_branch]);

    // The task worktree forks from the plan branch and lands one commit.
    let task_branch = format!("task/{PLAN}-{TASK}");
    let task_tree = root.join("task-worktree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            &task_branch,
            task_tree.to_str().unwrap(),
            &plan_branch,
        ],
    );
    std::fs::write(task_tree.join("src.txt"), "base\ntoggle\n").unwrap();
    git(&task_tree, &["add", "."]);
    git(&task_tree, &["commit", "-m", "implement toggle"]);

    // The run's integration workspace: created detached, then left detached at
    // the plan tip by the claim landing (`commit_task_claim` → `checkout
    // --detach {expected_old}`).
    let integration = root.join("integration");
    let plan_tip = git(&root, &["rev-parse", &plan_branch]);
    git(
        &root,
        &[
            "worktree",
            "add",
            "--detach",
            integration.to_str().unwrap(),
            &plan_tip,
        ],
    );

    (repo, integration, plan_branch)
}

#[tokio::test]
async fn phase_a_publishes_the_plan_ref_from_a_detached_integration_workspace() {
    let (repo, integration, plan_branch) = transactional_repo();
    let plan_tip_before = git(repo.path(), &["rev-parse", &plan_branch]);
    let merger = SquashMerger::new(integration.clone(), plan_branch.clone());

    let MergeOutcome::Merged { oid } = merger
        .squash_merge_with_evidence(
            &format!("task/{PLAN}-{TASK}"),
            "task(add-task-toggle): Add A Task Toggle Method",
            &task_identity(),
        )
        .await
        .unwrap()
    else {
        panic!("expected a clean squash landing");
    };

    assert_ne!(
        oid.as_str(),
        plan_tip_before,
        "Phase A must create a commit"
    );
    assert_eq!(
        git(repo.path(), &["rev-parse", &plan_branch]),
        oid.as_str(),
        "Phase A must publish the landing commit on the plan ref, not just on HEAD",
    );
    assert_eq!(
        merger
            .verify_task_landing_oid(oid.as_str(), &task_identity())
            .await
            .unwrap(),
        LandingEvidenceStatus::Verified(oid.clone()),
        "the landing must be reachable from the plan ref for recovery",
    );
}

#[tokio::test]
async fn phase_b_lands_after_phase_a_in_a_detached_integration_workspace() {
    let (_repo, integration, plan_branch) = transactional_repo();
    let merger = SquashMerger::new(integration.clone(), plan_branch.clone());
    let MergeOutcome::Merged { oid } = merger
        .squash_merge_with_evidence(
            &format!("task/{PLAN}-{TASK}"),
            "task(add-task-toggle): Add A Task Toggle Method",
            &task_identity(),
        )
        .await
        .unwrap()
    else {
        panic!("expected a clean squash landing");
    };

    // Exactly what `DriverContext::commit_phase_b` does after Phase A.
    let phase_b = commit_task_status(
        &integration,
        &format!("refs/heads/{plan_branch}"),
        oid.as_str(),
        &status_writes(),
        &status_identity(oid.as_str()),
    )
    .await;

    let phase_b = phase_b.expect("Phase B must land on top of the published Phase A commit");
    assert_eq!(
        git(
            &integration,
            &["rev-parse", &format!("refs/heads/{plan_branch}")]
        ),
        phase_b,
        "Phase B publishes the status commit on the plan ref",
    );
    assert_eq!(
        git(&integration, &["rev-parse", &format!("{phase_b}^")]),
        oid.as_str(),
        "the status commit is a child of the Phase A landing",
    );
}

/// Phase A is idempotent across a response-loss retry: the second call must
/// recognize the published evidence instead of landing a duplicate commit.
#[tokio::test]
async fn phase_a_retry_reuses_the_published_landing() {
    let (_repo, integration, plan_branch) = transactional_repo();
    let merger = SquashMerger::new(integration.clone(), plan_branch.clone());
    let branch = format!("task/{PLAN}-{TASK}");
    let MergeOutcome::Merged { oid: first } = merger
        .squash_merge_with_evidence(&branch, "task(add-task-toggle): toggle", &task_identity())
        .await
        .unwrap()
    else {
        panic!("expected a clean squash landing");
    };

    let MergeOutcome::Merged { oid: second } = merger
        .squash_merge_with_evidence(&branch, "task(add-task-toggle): toggle", &task_identity())
        .await
        .unwrap()
    else {
        panic!("expected the retry to reuse the landing");
    };

    assert_eq!(
        first, second,
        "a Phase A retry must not duplicate a landing"
    );
    assert_eq!(
        git(
            &integration,
            &["rev-parse", &format!("refs/heads/{plan_branch}")]
        ),
        second.as_str(),
    );
}
