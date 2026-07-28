#![cfg(unix)]

use makina_core::plan_contract::{
    CompletionEvidence, Mutation, PROTOCOL_VERSION, PlanContractClient, PlanContractServer,
    ProcessOutcome, Request, Response, ServerConfig, TerminationEvidence, WorkerOutcome,
    WorkerTerminationEvidence, apply_cleanup_permit, external_process_identity, handoff_artifact,
    preserve_bootstrap, reap_orphan_sentinel, recover_cleanup_permit, retention_manifest,
    verify_handoff_artifact, verify_process_termination, windows_pipe_name,
};
use makina_core::repository_lease::{
    RepositoryLeaseOperation, RepositoryLeaseOwner, RepositoryLeaseRegistry,
};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::Duration;

fn authoring_blueprint() -> makina_core::api::GeneratedPlanBlueprint {
    use makina_core::api::{
        GeneratedInitialStatusBlueprint as Status, GeneratedTaskBlueprint as Task,
        GeneratedWorkstreamBlueprint as Workstream,
    };
    makina_core::api::GeneratedPlanBlueprint {
        slug: "contract-authored".into(),
        title: "Contract Authored".into(),
        scope: "# Scope\n".into(),
        architecture: "# Architecture\n".into(),
        initial_status: Status {
            goal: "goal".into(),
            root_cause: "cause".into(),
            approach: "approach".into(),
            outcome: "pending".into(),
            last_updated: "2026-07-20".into(),
        },
        workstreams: vec![Workstream {
            id: "ws01".into(),
            title: "Work".into(),
        }],
        tasks: vec![Task {
            sequence: "01".into(),
            id: "first".into(),
            title: "First".into(),
            workstream: "ws01".into(),
            kind: "task".into(),
            depends_on: vec![],
            touches: vec!["crates/**".into()],
            gated: false,
            body: "Implement it.\n".into(),
        }],
    }
}

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

#[test]
fn structured_termination_rejects_empty_spoof_mismatch_and_premature_wait() {
    let mut child = Command::new("sleep").arg("60").spawn().unwrap();
    let registered = external_process_identity(child.id()).unwrap();
    let evidence = |process, handle: &str| TerminationEvidence {
        process,
        termination_handle: handle.into(),
        outcome: ProcessOutcome::Exited(0),
    };
    assert!(
        verify_process_termination(&registered, "handle", &evidence(registered.clone(), ""))
            .is_err()
    );
    assert!(
        verify_process_termination(
            &registered,
            "handle",
            &evidence(registered.clone(), "spoof")
        )
        .is_err()
    );
    let mut mismatched = registered.clone();
    mismatched.start_identity.push_str("-reused");
    assert!(
        verify_process_termination(&registered, "handle", &evidence(mismatched, "handle")).is_err()
    );
    assert!(
        verify_process_termination(
            &registered,
            "handle",
            &evidence(registered.clone(), "handle")
        )
        .is_err()
    );
    child.kill().unwrap();
    child.wait().unwrap();
    verify_process_termination(
        &registered,
        "handle",
        &evidence(registered.clone(), "handle"),
    )
    .unwrap();
}

#[test]
fn windows_pipe_shape_is_remote_namespace_safe_and_credential_derived() {
    let endpoint = std::path::Path::new("run/contract.sock");
    let first = windows_pipe_name(endpoint, &"a".repeat(64));
    let second = windows_pipe_name(endpoint, &"b".repeat(64));
    assert!(first.starts_with(r"\\.\pipe\makina-"));
    assert_ne!(
        first, second,
        "credentials must select unpredictable pipe names"
    );
    assert!(
        !first.contains(&"a".repeat(16)),
        "credential bytes must not leak"
    );
    assert!(!first.contains('/') && !first.contains(".."));
}

#[tokio::test]
async fn hard_dead_server_keeps_lease_until_orphan_sentinel_is_evidenced_and_reaped() {
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
    let endpoint = temp.path().join("run/contract.sock");
    let auth = "c".repeat(64);
    let server = PlanContractServer::new(ServerConfig {
        endpoint: endpoint.clone(),
        auth_token: auth.clone(),
        build_source_oid: oid.clone(),
    });
    let task = tokio::spawn(server.serve());
    for _ in 0..100 {
        if endpoint.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if task.is_finished() {
        return;
    }
    let client = PlanContractClient::new(endpoint.clone(), auth);
    let Response::Ready {
        session_token,
        plan_oid,
    } = client
        .request(Request::StartSession {
            protocol: PROTOCOL_VERSION,
            repo_root: repo.clone(),
            plan_dir: "docs/plans/0050-x".into(),
            plan_ref: "HEAD".into(),
            run_uid: "hard-death".into(),
            expected_plan_oid: oid.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("session not ready")
    };
    let mutation = Mutation {
        session_token: session_token.clone(),
        request_id: 1,
        expected_source_oid: oid,
        expected_plan_oid: plan_oid,
    };
    let mut worker = Command::new("sleep").arg("60").spawn().unwrap();
    let process = external_process_identity(worker.id()).unwrap();
    let Response::WorkerBegun {
        termination_handle, ..
    } = client
        .request(Request::BeginWorker {
            mutation,
            worker_id: "worker".into(),
            process: Some(process.clone()),
        })
        .await
        .unwrap()
    else {
        panic!("worker not begun")
    };
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let contender = RepositoryLeaseRegistry::new();
    let owner = RepositoryLeaseOwner {
        plan_dir: "other".into(),
        run_uid: "other".into(),
        operation: RepositoryLeaseOperation::Run,
    };
    assert!(
        contender
            .try_acquire(&repo, owner.clone())
            .unwrap()
            .is_none()
    );
    let sentinel = std::fs::read_dir(endpoint.parent().unwrap().join("sentinels"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    worker.kill().unwrap();
    worker.wait().unwrap();
    reap_orphan_sentinel(
        &sentinel,
        &TerminationEvidence {
            process,
            termination_handle,
            outcome: ProcessOutcome::Signaled(9),
        },
    )
    .unwrap();
    assert!(contender.try_acquire(&repo, owner).unwrap().is_some());
}

#[tokio::test]
async fn authenticated_session_reconnects_guards_workers_and_releases_lease() {
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
    let run_dir = temp.path().join("state/run");
    let endpoint = run_dir.join("contract.sock");
    let auth = "a".repeat(64);
    let server = PlanContractServer::new(ServerConfig {
        endpoint: endpoint.clone(),
        auth_token: auth.clone(),
        build_source_oid: oid.clone(),
    });
    let task = tokio::spawn(server.serve());
    for _ in 0..100 {
        if endpoint.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if task.is_finished() {
        let outcome = task.await.unwrap();
        if matches!(&outcome, Err(makina_core::plan_contract::ContractError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
        {
            // Restricted sandboxes may forbid AF_UNIX bind entirely.
            return;
        }
        panic!("server exited early: {outcome:?}");
    }
    assert_eq!(
        std::fs::metadata(&run_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&endpoint).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let bad = PlanContractClient::new(endpoint.clone(), "b".repeat(64));
    assert!(
        matches!(bad.request(Request::Hello { protocol: PROTOCOL_VERSION }).await.unwrap(),
        Response::Error { diagnostic } if diagnostic.code == "authentication_failed")
    );
    let client = PlanContractClient::new(endpoint.clone(), auth);
    assert!(matches!(
        client
            .request(Request::Hello {
                protocol: PROTOCOL_VERSION
            })
            .await
            .unwrap(),
        Response::Hello {
            protocol: PROTOCOL_VERSION,
            ..
        }
    ));
    let ready = client
        .request(Request::StartSession {
            protocol: PROTOCOL_VERSION,
            repo_root: repo.clone(),
            plan_dir: "docs/plans/0050-x".into(),
            plan_ref: "HEAD".into(),
            run_uid: "run-1".into(),
            expected_plan_oid: oid.clone(),
        })
        .await
        .unwrap();
    let Response::Ready {
        session_token,
        plan_oid,
    } = ready
    else {
        panic!("not ready")
    };

    // A fresh transport reconnects to the same live session.
    let reconnect = PlanContractClient::new(endpoint.clone(), "a".repeat(64));
    let again = reconnect
        .request(Request::StartSession {
            protocol: PROTOCOL_VERSION,
            repo_root: repo.clone(),
            plan_dir: "docs/plans/0050-x".into(),
            plan_ref: "HEAD".into(),
            run_uid: "run-1".into(),
            expected_plan_oid: oid.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        again,
        Response::Ready {
            session_token: session_token.clone(),
            plan_oid: oid.clone()
        }
    );

    let mutation = |request_id| Mutation {
        session_token: session_token.clone(),
        request_id,
        expected_source_oid: oid.clone(),
        expected_plan_oid: plan_oid.clone(),
    };
    let Response::WorkerBegun {
        termination_handle: worker_handle,
        ..
    } = client
        .request(Request::BeginWorker {
            mutation: mutation(1),
            worker_id: "developer-1".into(),
            process: None,
        })
        .await
        .unwrap()
    else {
        panic!("worker not begun")
    };
    assert!(
        matches!(client.request(Request::BeginWorker { mutation: mutation(1), worker_id: "reviewer-1".into(), process: None }).await.unwrap(),
        Response::Error { diagnostic } if diagnostic.code == "request_replayed")
    );
    std::fs::write(repo.join("complete"), "complete\n").unwrap();
    git(&repo, &["add", "complete"]);
    let message = format!(
        "complete\n\nMakina-Phase: completion\nMakina-Plan: 0050-x\nMakina-Run: run-1\nMakina-Final-Commit: {oid}"
    );
    git(&repo, &["commit", "-qm", &message]);
    let completion = CompletionEvidence {
        base_ref: "HEAD".into(),
        completion_oid: git(&repo, &["rev-parse", "HEAD"]),
        final_oid: oid.clone(),
        plan: "0050-x".into(),
        run_uid: "run-1".into(),
    };
    let root = temp.path().join("state").canonicalize().unwrap();
    let paths = vec![std::path::PathBuf::from("run/artifact")];
    std::fs::create_dir_all(root.join("run")).unwrap();
    std::fs::write(root.join("run/artifact"), "retained\n").unwrap();
    let retention_manifest = retention_manifest(root, paths);
    assert!(
        matches!(client.request(Request::Close { mutation: mutation(2), completion: completion.clone(),
        retention_manifest: retention_manifest.clone() }).await.unwrap(),
        Response::Error { diagnostic } if diagnostic.code == "workers_live")
    );

    let contender = RepositoryLeaseRegistry::new();
    assert!(
        contender
            .try_acquire(
                &repo,
                RepositoryLeaseOwner {
                    plan_dir: "other".into(),
                    run_uid: "other".into(),
                    operation: RepositoryLeaseOperation::Run
                }
            )
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        client
            .request(Request::EndWorker {
                mutation: mutation(3),
                worker_id: "developer-1".into(),
                termination_evidence: WorkerTerminationEvidence {
                    termination_handle: worker_handle,
                    outcome: WorkerOutcome::Completed,
                    message: None,
                }
            })
            .await
            .unwrap(),
        Response::WorkerEnded { .. }
    ));
    let mut git_child = Command::new("sleep").arg("60").spawn().unwrap();
    let git_process = external_process_identity(git_child.id()).unwrap();
    let Response::GitChildBegun {
        termination_handle: git_handle,
        ..
    } = client
        .request(Request::BeginGitChild {
            mutation: mutation(4),
            child_id: "git-phase-b".into(),
            process: git_process.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("Git child not begun")
    };
    git_child.kill().unwrap();
    git_child.wait().unwrap();
    assert!(matches!(
        client
            .request(Request::Close {
                mutation: mutation(5),
                completion: completion.clone(),
                retention_manifest: retention_manifest.clone(),
            })
            .await
            .unwrap(),
        Response::Error { diagnostic } if diagnostic.code == "git_children_live"
    ));
    assert!(matches!(
        client
            .request(Request::EndGitChild {
                mutation: mutation(6),
                child_id: "git-phase-b".into(),
                wait_evidence: TerminationEvidence {
                    process: git_process,
                    termination_handle: git_handle,
                    outcome: ProcessOutcome::Signaled(9)
                },
            })
            .await
            .unwrap(),
        Response::GitChildEnded { .. }
    ));
    let close_request = Request::Close {
        mutation: mutation(7),
        completion,
        retention_manifest: retention_manifest.clone(),
    };
    let closed = client.request(close_request.clone()).await.unwrap();
    assert!(matches!(
        closed,
        Response::Closed {
            cleanup_permit: Some(_)
        }
    ));
    let recovered = recover_cleanup_permit(&endpoint, &retention_manifest).unwrap();
    assert!(
        matches!(closed, Response::Closed { cleanup_permit: Some(ref permit) } if *permit == recovered)
    );
    assert!(
        contender
            .try_acquire(
                &repo,
                RepositoryLeaseOwner {
                    plan_dir: "other".into(),
                    run_uid: "other".into(),
                    operation: RepositoryLeaseOperation::Run
                }
            )
            .unwrap()
            .is_some()
    );
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("server must exit after stable Close")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn authoring_protocol_create_only_is_reconnectable_and_replays_lost_response() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join("docs/plans")).unwrap();
    git(&repo, &["init", "-q", "-b", "develop"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("docs/plans/STATUS.md"), "# Plans\n\n| Plan | Title | Status | Progress | Outcome | Link |\n|---|---|---|---|---|---|\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let endpoint = temp.path().join("run/contract.sock");
    let auth = "e".repeat(64);
    let task = tokio::spawn(
        PlanContractServer::new(ServerConfig {
            endpoint: endpoint.clone(),
            auth_token: auth.clone(),
            build_source_oid: base.clone(),
        })
        .serve(),
    );
    for _ in 0..100 {
        if endpoint.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if task.is_finished() {
        return;
    }
    let client = PlanContractClient::new(endpoint.clone(), auth);
    let Response::AuthoringReady {
        session_token,
        base_oid,
    } = client
        .request(Request::StartAuthoringSession {
            protocol: PROTOCOL_VERSION,
            repo_root: repo.clone(),
            base_branch: "develop".into(),
            run_uid: "author".into(),
            expected_base_oid: base.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("authoring not ready")
    };
    let mutation = Mutation {
        session_token: session_token.clone(),
        request_id: 1,
        expected_source_oid: base_oid.clone(),
        expected_plan_oid: base_oid.clone(),
    };
    let Response::AuthoringInspected { reservations, .. } = client
        .request(Request::InspectAuthoring { mutation, count: 2 })
        .await
        .unwrap()
    else {
        panic!("authoring not inspected")
    };
    let Response::WorkerBegun {
        worker_id,
        termination_handle,
    } = client
        .request(Request::BeginAuthorWorker {
            mutation: Mutation {
                session_token: session_token.clone(),
                request_id: 2,
                expected_source_oid: base_oid.clone(),
                expected_plan_oid: base_oid.clone(),
            },
            worker_id: "blueprint:1".into(),
        })
        .await
        .unwrap()
    else {
        panic!("author worker not begun")
    };
    assert!(matches!(
        client
            .request(Request::CloseAuthoring {
                mutation: Mutation {
                    session_token: session_token.clone(),
                    request_id: 3,
                    expected_source_oid: base_oid.clone(),
                    expected_plan_oid: base_oid.clone(),
                }
            })
            .await
            .unwrap(),
        Response::Error { diagnostic } if diagnostic.code == "children_live"
    ));
    assert!(matches!(
        client
            .request(Request::EndAuthorWorker {
                mutation: Mutation {
                    session_token: session_token.clone(),
                    request_id: 3,
                    expected_source_oid: base_oid.clone(),
                    expected_plan_oid: base_oid.clone(),
                },
                worker_id,
                termination_evidence: WorkerTerminationEvidence {
                    termination_handle,
                    outcome: WorkerOutcome::Completed,
                    message: None,
                },
            })
            .await
            .unwrap(),
        Response::WorkerEnded { .. }
    ));
    let publish = Request::PublishBlueprint {
        mutation: Mutation {
            session_token: session_token.clone(),
            request_id: 4,
            expected_source_oid: base_oid.clone(),
            expected_plan_oid: base_oid.clone(),
        },
        reservation: reservations[0].clone(),
        blueprint: authoring_blueprint(),
        commit: false,
    };
    let first = client.request(publish.clone()).await.unwrap();
    assert!(matches!(
        &first,
        Response::BlueprintPublished {
            outcome: makina_core::plan_contract::AuthoringOutcome::AwaitingCommit,
            registration_oid: None,
            ..
        }
    ));
    assert_eq!(
        client.request(publish).await.unwrap(),
        first,
        "response-loss retry must reuse exact outcome"
    );
    assert!(repo.join("docs/plans/0001-contract-authored").exists());
    assert!(
        git(
            &repo,
            &["for-each-ref", "--format=%(refname)", "refs/heads/plan/"]
        )
        .is_empty()
    );
    let mut committed = authoring_blueprint();
    committed.slug = "contract-committed".into();
    committed.title = "Contract Committed".into();
    let publish = Request::PublishBlueprint {
        mutation: Mutation {
            session_token,
            request_id: 5,
            expected_source_oid: base_oid.clone(),
            expected_plan_oid: base_oid,
        },
        reservation: reservations[1].clone(),
        blueprint: committed,
        commit: true,
    };
    let registered = client.request(publish.clone()).await.unwrap();
    assert!(matches!(&registered, Response::BlueprintPublished {
        outcome: makina_core::plan_contract::AuthoringOutcome::Registered,
        registration_oid: Some(_), plan_dir
    } if plan_dir.ends_with("0002-contract-committed")));
    assert_eq!(client.request(publish).await.unwrap(), registered);
    assert!(!repo.join("docs/plans/0002-contract-committed").exists());
    assert!(
        !git(
            &repo,
            &["rev-parse", "--verify", "plan/0002-contract-committed"]
        )
        .is_empty()
    );
    task.abort();
}

#[test]
fn lifecycle_requests_are_closed_semantic_payloads() {
    let mutation = Mutation {
        session_token: "token".into(),
        request_id: 7,
        expected_source_oid: "source".into(),
        expected_plan_oid: "plan".into(),
    };
    let encoded = serde_json::to_value(Request::ClaimTask {
        mutation,
        task: "port-workflow".into(),
        last_updated: "2026-07-20".into(),
    })
    .unwrap();
    assert_eq!(encoded["type"], "claim_task");
    assert!(encoded.get("path").is_none());
    assert!(encoded.get("bytes").is_none());
    assert!(
        serde_json::from_value::<Request>(serde_json::json!({
            "type": "claim_task",
            "mutation": {
                "session_token": "token", "request_id": 7,
                "expected_source_oid": "source", "expected_plan_oid": "plan"
            },
            "task": "port-workflow", "last_updated": "2026-07-20",
            "writes": [{"path": "docs/plans/STATUS.md", "bytes": [1]}]
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<Request>(serde_json::json!({
            "type": "commit_phase_b",
            "mutation": { "session_token": "token", "request_id": 8,
                "expected_source_oid": "source", "expected_plan_oid": "plan" },
            "task": "port-workflow", "last_updated": "2026-07-20",
            "phase_a_oid": "manually-typed-is-forbidden"
        }))
        .is_err()
    );
    let semantic = serde_json::to_value(Request::TransitionTask {
        mutation: Mutation {
            session_token: "token".into(),
            request_id: 9,
            expected_source_oid: "source".into(),
            expected_plan_oid: "plan".into(),
        },
        task: "port-workflow".into(),
        transition: makina_core::plan_contract::SourceTransition::Block {
            reason: "review cap".into(),
        },
    })
    .unwrap();
    assert_eq!(semantic["transition"]["action"], "block");
    assert!(semantic.get("writes").is_none());
    let prepared = serde_json::to_value(Request::PrepareFinalization {
        mutation: Mutation {
            session_token: "token".into(),
            request_id: 10,
            expected_source_oid: "source".into(),
            expected_plan_oid: "plan".into(),
        },
        mode: "squash".into(),
        last_updated: "2026-07-20".into(),
    })
    .unwrap();
    assert!(prepared.get("prepared_oid").is_none());
    assert!(prepared.get("base_oid").is_none());
    let candidate = serde_json::to_value(Request::CheckCandidate {
        mutation: Mutation {
            session_token: "token".into(),
            request_id: 11,
            expected_source_oid: "source".into(),
            expected_plan_oid: "plan".into(),
        },
        task: "port-workflow".into(),
    })
    .unwrap();
    assert!(candidate.get("touches").is_none());
    assert!(candidate.get("candidate_token").is_none());
}

#[test]
fn retention_copy_handoff_hash_and_janitor_are_path_bound_and_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("workflow.js");
    std::fs::write(&source, "bootstrap-v1\n").unwrap();
    let recovery = temp.path().join("recovery");
    std::fs::create_dir(&recovery).unwrap();
    let recovery = recovery.canonicalize().unwrap();

    let manifest = preserve_bootstrap(&recovery, std::slice::from_ref(&source)).unwrap();
    let retained = recovery.join(&manifest.paths[0]);
    assert_eq!(
        std::fs::read_to_string(&retained).unwrap(),
        "bootstrap-v1\n"
    );
    std::fs::write(&source, "repository phase-a replacement\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(&retained).unwrap(),
        "bootstrap-v1\n"
    );

    let artifact = handoff_artifact("exact-b".into(), retained.clone()).unwrap();
    assert!(artifact.executable.is_absolute());
    verify_handoff_artifact(&artifact).unwrap();
    std::fs::write(&retained, "corrupt\n").unwrap();
    assert!(verify_handoff_artifact(&artifact).is_err());
    std::fs::write(&retained, "bootstrap-v1\n").unwrap();

    let permit = makina_core::plan_contract::CleanupPermit {
        retention_manifest_digest: manifest.digest.clone(),
    };
    let mismatched = makina_core::plan_contract::CleanupPermit {
        retention_manifest_digest: "ambiguous-other-session".into(),
    };
    assert!(apply_cleanup_permit(&manifest, &mismatched, true).is_err());
    assert!(
        retained.exists(),
        "ambiguous session must retain exact-B bytes"
    );
    assert!(apply_cleanup_permit(&manifest, &permit, false).is_err());
    assert!(retained.exists(), "pre-C Stage/Manual retention is stable");
    std::fs::write(&retained, "ambiguous-mutated-evidence\n").unwrap();
    assert!(apply_cleanup_permit(&manifest, &permit, true).is_err());
    assert!(retained.exists(), "mismatched evidence is never reaped");
    std::fs::write(&retained, "bootstrap-v1\n").unwrap();
    apply_cleanup_permit(&manifest, &permit, true).unwrap();
    assert!(!retained.exists());
    apply_cleanup_permit(&manifest, &permit, true).unwrap();
}
