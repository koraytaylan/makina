#![cfg(unix)]

use makina_core::plan_contract::{
    HandoffRequest, PROTOCOL_VERSION, Request, Response, handoff_artifact, launch_exact_handoff,
    reap_old_coordinator,
};
use makina_core::repository_lease::{
    RepositoryLeaseOperation, RepositoryLeaseOwner, RepositoryLeaseRegistry,
};
use std::process::Command;
use std::time::Duration;

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
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

#[tokio::test]
async fn exact_absolute_handoff_waits_for_contender_reconciles_and_retains_before_c() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("seed"), "seed\n").unwrap();
    git(&repo, &["add", "seed"]);
    git(&repo, &["commit", "-qm", "seed"]);
    let oid = git(&repo, &["rev-parse", "HEAD"]);

    let exact_binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_makina"))
        .canonicalize()
        .unwrap();
    let artifact = handoff_artifact(oid.clone(), exact_binary).unwrap();
    let recovery = temp.path().join("recovery-copy");
    std::fs::write(&recovery, "old coordinator recovery\n").unwrap();
    let endpoint = temp.path().join("run/contract.sock");

    let registry = RepositoryLeaseRegistry::new();
    let contender = registry
        .try_acquire(
            &repo,
            RepositoryLeaseOwner {
                plan_dir: "contender".into(),
                run_uid: "gap".into(),
                operation: RepositoryLeaseOperation::Run,
            },
        )
        .unwrap()
        .unwrap();
    let request = HandoffRequest {
        artifact,
        endpoint: endpoint.clone(),
        auth_token: "d".repeat(64),
        repo_root: repo,
        plan_dir: "docs/plans/0050-x".into(),
        plan_ref: "HEAD".into(),
        run_uid: "handoff".into(),
        expected_plan_oid: oid,
        tasks: Vec::new(),
    };
    let pending = tokio::spawn(launch_exact_handoff(request));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!pending.is_finished(), "contender gap must visibly wait");
    assert!(
        recovery.exists(),
        "retained Stage/Manual recovery must survive Ready"
    );
    drop(contender);
    let outcome = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap();
    let mut ready = match outcome {
        Ok(ready) => ready,
        Err(cause) if cause.contains("exited before Hello") => {
            // Restricted sandboxes may forbid AF_UNIX bind entirely.
            return;
        }
        Err(cause) => panic!("handoff failed: {cause}"),
    };
    assert!(ready.evidence.is_empty());
    assert!(matches!(
        ready
            .client
            .request(Request::Hello {
                protocol: PROTOCOL_VERSION
            })
            .await
            .unwrap(),
        Response::Hello { .. }
    ));

    // Model a coordinator crash after Ready but before C. The recovery bytes
    // remain, and cleanup is impossible because no verified Close occurred.
    reap_old_coordinator(&mut ready.server, &endpoint).unwrap();
    assert!(recovery.exists());
}
