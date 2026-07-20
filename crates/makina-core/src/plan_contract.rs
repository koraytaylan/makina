//! Versioned, authenticated local JSON-lines contract for plan workflows.

use crate::repository_lease::{
    RepositoryLeaseGuard, RepositoryLeaseOperation, RepositoryLeaseOwner, RepositoryLeaseRegistry,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Derive an unguessable per-run Windows pipe name without exposing the
/// endpoint credential itself. Remote pipe clients are separately rejected.
pub fn windows_pipe_name(endpoint: &Path, auth_token: &str) -> String {
    let label = endpoint
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("contract")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let digest = format!("{:x}", Sha256::digest(auth_token.as_bytes()));
    format!(r"\\.\pipe\makina-{label}-{}", &digest[..24])
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub endpoint: PathBuf,
    pub auth_token: String,
    pub build_source_oid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub session_token: String,
    pub request_id: u64,
    pub expected_source_oid: String,
    pub expected_plan_oid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Hello {
        protocol: u32,
    },
    StartSession {
        protocol: u32,
        repo_root: PathBuf,
        plan_dir: PathBuf,
        plan_ref: String,
        run_uid: String,
        expected_plan_oid: String,
    },
    StartAuthoringSession {
        protocol: u32,
        repo_root: PathBuf,
        base_branch: String,
        run_uid: String,
        expected_base_oid: String,
    },
    InspectAuthoring {
        mutation: Mutation,
        count: u8,
    },
    PublishBlueprint {
        mutation: Mutation,
        reservation: String,
        blueprint: crate::api::GeneratedPlanBlueprint,
        commit: bool,
    },
    BeginAuthorWorker {
        mutation: Mutation,
        worker_id: String,
    },
    EndAuthorWorker {
        mutation: Mutation,
        worker_id: String,
        termination_evidence: WorkerTerminationEvidence,
    },
    CloseAuthoring {
        mutation: Mutation,
    },
    CancelSession {
        mutation: Mutation,
    },
    BeginWorker {
        mutation: Mutation,
        worker_id: String,
        #[serde(default)]
        process: Option<ProcessIdentity>,
    },
    EndWorker {
        mutation: Mutation,
        worker_id: String,
        termination_evidence: WorkerTerminationEvidence,
    },
    BeginGitChild {
        mutation: Mutation,
        child_id: String,
        process: ProcessIdentity,
    },
    EndGitChild {
        mutation: Mutation,
        child_id: String,
        wait_evidence: TerminationEvidence,
    },
    ReconcileTask {
        mutation: Mutation,
        task: String,
    },
    InspectPlan {
        mutation: Mutation,
    },
    ReadyTasks {
        mutation: Mutation,
    },
    CheckCandidate {
        mutation: Mutation,
        task: String,
    },
    LandTask {
        mutation: Mutation,
        task: String,
        candidate_token: String,
        last_updated: String,
    },
    ClaimTask {
        mutation: Mutation,
        task: String,
        last_updated: String,
    },
    LandPhaseA {
        mutation: Mutation,
        task: String,
    },
    CommitPhaseB {
        mutation: Mutation,
        task: String,
        last_updated: String,
    },
    SetDisposition {
        mutation: Mutation,
        task: String,
        disposition: Disposition,
    },
    TransitionTask {
        mutation: Mutation,
        task: String,
        transition: SourceTransition,
    },
    PrepareFinalization {
        mutation: Mutation,
        mode: String,
        last_updated: String,
    },
    IntegrateFinalization {
        mutation: Mutation,
        manual_oid: Option<String>,
    },
    CompleteFinalization {
        mutation: Mutation,
        last_updated: String,
    },
    Close {
        mutation: Mutation,
        completion: CompletionEvidence,
        retention_manifest: RetentionManifest,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Hello {
        protocol: u32,
        build_source_oid: String,
    },
    Ready {
        session_token: String,
        plan_oid: String,
    },
    AuthoringReady {
        session_token: String,
        base_oid: String,
    },
    AuthoringInspected {
        base_oid: String,
        reservations: Vec<String>,
    },
    BlueprintPublished {
        outcome: AuthoringOutcome,
        plan_dir: PathBuf,
        registration_oid: Option<String>,
    },
    AuthoringClosed,
    Cancelled,
    WorkerBegun {
        worker_id: String,
        termination_handle: String,
    },
    WorkerEnded {
        worker_id: String,
    },
    GitChildBegun {
        child_id: String,
        termination_handle: String,
    },
    GitChildEnded {
        child_id: String,
    },
    TaskReconciled {
        evidence: TaskEvidence,
    },
    PlanInspected {
        plan: PlanSnapshot,
    },
    ReadyTasks {
        tasks: Vec<PlanTaskSnapshot>,
        complete: bool,
        blocked: bool,
    },
    CandidateChecked {
        task: String,
        candidate_token: String,
    },
    TaskLanded {
        task: String,
        implementation_oid: String,
        plan_oid: String,
    },
    TaskClaimed {
        task: String,
        plan_oid: String,
        worktree: PathBuf,
    },
    PhaseALanded {
        task: String,
        evidence_token: String,
    },
    PhaseBCommitted {
        task: String,
        phase_a_oid: String,
        plan_oid: String,
    },
    DispositionCommitted {
        task: String,
        plan_oid: String,
    },
    TransitionCommitted {
        task: String,
        plan_oid: String,
    },
    FinalizationPrepared {
        plan_oid: String,
    },
    FinalizationIntegrated {
        final_oid: String,
    },
    FinalizationCompleted {
        completion_oid: String,
        final_oid: String,
    },
    Closed {
        cleanup_permit: Option<CleanupPermit>,
    },
    Error {
        diagnostic: Diagnostic,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_identity: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TerminationEvidence {
    pub process: ProcessIdentity,
    pub termination_handle: String,
    pub outcome: ProcessOutcome,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerTerminationEvidence {
    pub termination_handle: String,
    pub outcome: WorkerOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOutcome {
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOutcome {
    Exited(i32),
    Signaled(i32),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskEvidence {
    RegistrationOnly,
    Claimed {
        claim_oid: String,
    },
    LandingPending {
        implementation_oid: String,
    },
    Complete {
        implementation_oid: String,
        status_oid: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Disposition {
    Ungate,
    Drop { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceTransition {
    Block { reason: String },
    Retry,
    Requeue,
    Cancel,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthoringOutcome {
    AwaitingCommit,
    Registered,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanSnapshot {
    pub plan_dir: PathBuf,
    pub title: String,
    pub source_digest: String,
    pub executable_digest: String,
    pub tasks: Vec<PlanTaskSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanTaskSnapshot {
    pub id: String,
    pub title: String,
    pub status: String,
    pub gated: bool,
    pub dependencies: Vec<String>,
    pub source_path: PathBuf,
    pub scope_path: PathBuf,
    pub architecture_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanupPermit {
    pub retention_manifest_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionEvidence {
    pub base_ref: String,
    pub completion_oid: String,
    pub final_oid: String,
    pub plan: String,
    pub run_uid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionManifest {
    pub root: PathBuf,
    pub paths: Vec<PathBuf>,
    pub file_digests: BTreeMap<PathBuf, String>,
    pub digest: String,
}

/// Durable evidence used to select a handoff binary without consulting PATH.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HandoffArtifact {
    pub build_source_oid: String,
    pub executable: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct HandoffRequest {
    pub artifact: HandoffArtifact,
    pub endpoint: PathBuf,
    pub auth_token: String,
    pub repo_root: PathBuf,
    pub plan_dir: PathBuf,
    pub plan_ref: String,
    pub run_uid: String,
    pub expected_plan_oid: String,
    pub tasks: Vec<String>,
}

/// Live exact-B server returned only after Hello, lease acquisition, complete
/// task reconciliation, and a reconnecting Ready have all succeeded.
pub struct HandoffReady {
    pub server: std::process::Child,
    pub client: PlanContractClient,
    pub session_token: String,
    pub plan_oid: String,
    pub evidence: BTreeMap<String, TaskEvidence>,
}

/// Halt and reap the old coordinator before removing only its Unix socket.
/// Recovery copies and retained artifacts are deliberately untouched.
#[cfg(unix)]
pub fn reap_old_coordinator(
    coordinator: &mut std::process::Child,
    endpoint: &Path,
) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;
    if coordinator
        .try_wait()
        .map_err(|error| error.to_string())?
        .is_none()
    {
        coordinator.kill().map_err(|error| error.to_string())?;
        coordinator.wait().map_err(|error| error.to_string())?;
    }
    match std::fs::symlink_metadata(endpoint) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(endpoint).map_err(|error| error.to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err("refusing to remove a non-socket coordinator endpoint".into()),
        Err(error) => Err(error.to_string()),
    }
}

/// Launch and validate an exact-build contract server by absolute path. PATH is
/// never consulted. Lease contention is represented by the StartSession call
/// remaining pending without allowing any reconciliation mutation.
#[cfg(unix)]
pub async fn launch_exact_handoff(request: HandoffRequest) -> Result<HandoffReady, String> {
    verify_handoff_artifact(&request.artifact)?;
    if request.endpoint.exists() {
        return Err("old coordinator endpoint still exists; reap it before handoff".into());
    }
    let mut command = std::process::Command::new(&request.artifact.executable);
    command
        .args(["plan-contract", "serve", "--endpoint"])
        .arg(&request.endpoint)
        .args(["--auth-token", &request.auth_token, "--build-source-oid"])
        .arg(&request.artifact.build_source_oid)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut server = command.spawn().map_err(|error| error.to_string())?;
    let client = PlanContractClient::new(request.endpoint.clone(), request.auth_token.clone());
    let result = async {
        for _ in 0..200 {
            if request.endpoint.exists() {
                break;
            }
            if server
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err("exact-B contract server exited before Hello".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let hello = client
            .request(Request::Hello {
                protocol: PROTOCOL_VERSION,
            })
            .await
            .map_err(|error| error.to_string())?;
        if hello
            != (Response::Hello {
                protocol: PROTOCOL_VERSION,
                build_source_oid: request.artifact.build_source_oid.clone(),
            })
        {
            return Err("handoff Hello does not identify the exact-B build".into());
        }
        let ready = client
            .request(Request::StartSession {
                protocol: PROTOCOL_VERSION,
                repo_root: request.repo_root.clone(),
                plan_dir: request.plan_dir.clone(),
                plan_ref: request.plan_ref.clone(),
                run_uid: request.run_uid.clone(),
                expected_plan_oid: request.expected_plan_oid.clone(),
            })
            .await
            .map_err(|error| error.to_string())?;
        let Response::Ready {
            session_token,
            plan_oid,
        } = ready
        else {
            return Err(format!("handoff session was not Ready: {ready:?}"));
        };
        let mut evidence = BTreeMap::new();
        for (offset, task) in request.tasks.iter().enumerate() {
            let response = client
                .request(Request::ReconcileTask {
                    mutation: Mutation {
                        session_token: session_token.clone(),
                        request_id: offset as u64 + 1,
                        expected_source_oid: request.expected_plan_oid.clone(),
                        expected_plan_oid: plan_oid.clone(),
                    },
                    task: task.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
            let Response::TaskReconciled {
                evidence: task_evidence,
            } = response
            else {
                return Err(format!("task {task} did not reconcile: {response:?}"));
            };
            evidence.insert(task.clone(), task_evidence);
        }
        let final_ready = client
            .request(Request::StartSession {
                protocol: PROTOCOL_VERSION,
                repo_root: request.repo_root,
                plan_dir: request.plan_dir,
                plan_ref: request.plan_ref,
                run_uid: request.run_uid,
                expected_plan_oid: plan_oid.clone(),
            })
            .await
            .map_err(|error| error.to_string())?;
        if !matches!(&final_ready, Response::Ready { session_token: token, plan_oid: oid }
            if token == &session_token && oid == &plan_oid)
        {
            return Err(format!("handoff did not finish Ready: {final_ready:?}"));
        }
        Ok((session_token, plan_oid, evidence))
    }
    .await;
    match result {
        Ok((session_token, plan_oid, evidence)) => Ok(HandoffReady {
            server,
            client,
            session_token,
            plan_oid,
            evidence,
        }),
        Err(cause) => {
            let _ = server.kill();
            let _ = server.wait();
            Err(cause)
        }
    }
}

/// Hash and copy bootstrap inputs below a run-owned recovery root.
///
/// Sources must be regular files and destinations are always relative, so a
/// repository update cannot overwrite the recovery copy used by the old
/// coordinator after Phase B.
pub fn preserve_bootstrap(
    recovery_root: &Path,
    sources: &[PathBuf],
) -> Result<RetentionManifest, String> {
    if !recovery_root.is_absolute() || sources.is_empty() {
        return Err("recovery root must be absolute and sources nonempty".into());
    }
    std::fs::create_dir_all(recovery_root).map_err(|error| error.to_string())?;
    let root = std::fs::canonicalize(recovery_root).map_err(|error| error.to_string())?;
    let mut paths = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        let metadata = std::fs::symlink_metadata(source).map_err(|error| error.to_string())?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "bootstrap source is not a regular file: {}",
                source.display()
            ));
        }
        let name = source
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "bootstrap source has no UTF-8 file name".to_owned())?;
        let relative = PathBuf::from(format!("bootstrap/{index:04}-{name}"));
        let destination = root.join(&relative);
        std::fs::create_dir_all(destination.parent().expect("destination has parent"))
            .map_err(|error| error.to_string())?;
        std::fs::copy(source, &destination).map_err(|error| error.to_string())?;
        paths.push(relative);
    }
    Ok(retention_manifest(root, paths))
}

/// Construct the canonical digest for an owned-path retention manifest.
pub fn retention_manifest(root: PathBuf, paths: Vec<PathBuf>) -> RetentionManifest {
    let file_digests = paths
        .iter()
        .filter_map(|path| {
            artifact_digest(&root.join(path))
                .ok()
                .map(|v| (path.clone(), v))
        })
        .collect();
    retention_manifest_with_digests(root, paths, file_digests)
}

fn artifact_digest(path: &Path) -> Result<String, String> {
    fn visit(path: &Path, relative: &Path, hash: &mut Sha256) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err("retained artifacts cannot contain symlinks".into());
        }
        hash.update(relative.as_os_str().as_encoded_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            hash.update(metadata.permissions().mode().to_le_bytes());
        }
        if metadata.is_file() {
            hash.update(b"file");
            hash.update(std::fs::read(path).map_err(|error| error.to_string())?);
        } else if metadata.is_dir() {
            hash.update(b"directory");
            let mut entries = std::fs::read_dir(path)
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                visit(&entry.path(), &relative.join(entry.file_name()), hash)?;
            }
        } else {
            return Err("retained artifact must be a regular file or directory".into());
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    visit(path, Path::new("."), &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn retention_manifest_with_digests(
    root: PathBuf,
    paths: Vec<PathBuf>,
    file_digests: BTreeMap<PathBuf, String>,
) -> RetentionManifest {
    let mut hash = Sha256::new();
    hash.update(root.as_os_str().as_encoded_bytes());
    for path in &paths {
        hash.update([0]);
        hash.update(path.as_os_str().as_encoded_bytes());
        hash.update([0]);
        if let Some(digest) = file_digests.get(path) {
            hash.update(digest.as_bytes());
        }
    }
    RetentionManifest {
        root,
        paths,
        file_digests,
        digest: format!("{:x}", hash.finalize()),
    }
}

/// Record and later verify the exact absolute binary selected for handoff.
pub fn handoff_artifact(
    build_source_oid: String,
    executable: PathBuf,
) -> Result<HandoffArtifact, String> {
    if !executable.is_absolute() {
        return Err("handoff executable must use an absolute path".into());
    }
    let bytes = std::fs::read(&executable).map_err(|error| error.to_string())?;
    Ok(HandoffArtifact {
        build_source_oid,
        executable,
        sha256: format!("{:x}", Sha256::digest(bytes)),
    })
}

pub fn verify_handoff_artifact(artifact: &HandoffArtifact) -> Result<(), String> {
    let actual = handoff_artifact(
        artifact.build_source_oid.clone(),
        artifact.executable.clone(),
    )?;
    if actual.sha256 != artifact.sha256 {
        return Err("handoff executable hash mismatch".into());
    }
    Ok(())
}

/// Build a handoff executable from an exact Git tree into an external,
/// run-owned target directory. The source OID is embedded at compile time and
/// the returned absolute path/hash are the only supported selection evidence.
pub fn build_exact_handoff(
    repo: &Path,
    plan_ref: &str,
    external_root: &Path,
    package: &str,
    binary: &str,
) -> Result<HandoffArtifact, String> {
    if !external_root.is_absolute() {
        return Err("handoff build root must be absolute".into());
    }
    let source_oid = resolve_ref(repo, plan_ref)?;
    std::fs::create_dir_all(external_root).map_err(|error| error.to_string())?;
    let external_root = std::fs::canonicalize(external_root).map_err(|error| error.to_string())?;
    let checkout = external_root.join(format!(
        "source-{}",
        &source_oid[..12.min(source_oid.len())]
    ));
    let target = external_root.join("target");
    if checkout.exists() {
        return Err("exact-tree checkout already exists; reconcile it before building".into());
    }
    let added = std::process::Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&checkout)
        .arg(&source_oid)
        .current_dir(repo)
        .output()
        .map_err(|error| error.to_string())?;
    if !added.status.success() {
        return Err(String::from_utf8_lossy(&added.stderr).trim().into());
    }
    let built = std::process::Command::new("cargo")
        .args(["build", "--offline", "--package", package, "--bin", binary])
        .arg("--target-dir")
        .arg(&target)
        .env("MAKINA_BUILD_SOURCE_OID", &source_oid)
        .current_dir(&checkout)
        .output()
        .map_err(|error| error.to_string());
    let removed = std::process::Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&checkout)
        .current_dir(repo)
        .output();
    let built = built?;
    if !built.status.success() {
        return Err(String::from_utf8_lossy(&built.stderr).trim().into());
    }
    if !matches!(removed, Ok(output) if output.status.success()) {
        return Err("built binary but failed to reap its exact-tree checkout".into());
    }
    let executable = target
        .join("debug")
        .join(format!("{binary}{}", std::env::consts::EXE_SUFFIX));
    handoff_artifact(source_oid, executable)
}

/// Build OID embedded by [`build_exact_handoff`]. Normal developer builds do
/// not claim an exact handoff identity.
pub const fn embedded_build_source_oid() -> Option<&'static str> {
    option_env!("MAKINA_BUILD_SOURCE_OID")
}

/// Apply a manifest-bound cleanup capability after the client has reaped the
/// server. Repeating cleanup is safe, which covers client death after Close.
pub fn apply_cleanup_permit(
    manifest: &RetentionManifest,
    permit: &CleanupPermit,
    server_reaped: bool,
) -> Result<(), String> {
    verify_manifest(manifest)?;
    if !server_reaped {
        return Err("server must be reaped before retained artifacts are removed".into());
    }
    if permit.retention_manifest_digest != manifest.digest {
        return Err("cleanup permit does not authorize this manifest".into());
    }
    for relative in manifest.paths.iter().rev() {
        let target = manifest.root.join(relative);
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("refusing to clean a retained symlink".into());
            }
            Ok(metadata) => {
                let actual = artifact_digest(&target)?;
                if manifest.file_digests.get(relative) != Some(&actual) {
                    return Err(format!(
                        "retained artifact changed before cleanup: {}",
                        relative.display()
                    ));
                }
                if metadata.is_dir() {
                    std::fs::remove_dir_all(&target).map_err(|error| error.to_string())?;
                } else {
                    std::fs::remove_file(&target).map_err(|error| error.to_string())?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn cleanup_receipt_path(endpoint: &Path) -> Result<PathBuf, String> {
    Ok(endpoint
        .parent()
        .ok_or_else(|| "contract endpoint has no parent".to_owned())?
        .join("cleanup-permit.json"))
}

fn persist_cleanup_permit(endpoint: &Path, permit: &CleanupPermit) -> Result<(), String> {
    let path = cleanup_receipt_path(endpoint)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec(permit).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    std::fs::rename(temporary, path).map_err(|error| error.to_string())
}

/// Recover the manifest-bound cleanup capability after the Close response or
/// its client was lost. The receipt is written before the server replies.
pub fn recover_cleanup_permit(
    endpoint: &Path,
    manifest: &RetentionManifest,
) -> Result<CleanupPermit, String> {
    verify_manifest(manifest)?;
    let permit: CleanupPermit = serde_json::from_slice(
        &std::fs::read(cleanup_receipt_path(endpoint)?).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if permit.retention_manifest_digest != manifest.digest {
        return Err("persisted cleanup permit belongs to another manifest".into());
    }
    Ok(permit)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum ContractError {
    #[error("plan contract I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("plan contract JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("plan contract is unsupported on this platform")]
    Unsupported,
}

struct Session {
    token: String,
    source_oid: String,
    plan_oid: String,
    last_request_id: u64,
    workers: BTreeMap<String, LeaseSentinel>,
    git_children: BTreeMap<String, LeaseSentinel>,
    coordinator: crate::orchestrator::PlanContractCoordinator,
    phase_a: BTreeMap<String, (String, String)>,
    candidates: BTreeMap<String, String>,
    prepared: Option<crate::orchestrator::PreparedFinalization>,
    final_oid: Option<String>,
    identity: SessionIdentity,
    _lease: RepositoryLeaseGuard,
}

struct LeaseSentinel {
    child: std::process::Child,
    metadata: PathBuf,
    external: Option<ProcessIdentity>,
    termination_handle: String,
}

#[derive(Serialize, Deserialize)]
struct SentinelMetadata {
    pid: u32,
    start_identity: String,
    external: Option<ProcessIdentity>,
    termination_handle: String,
}

fn sentinel_start_identity(pid: u32) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|error| error.to_string())?;
        stat.split_whitespace()
            .nth(21)
            .map(str::to_owned)
            .ok_or_else(|| "sentinel process identity is unavailable".into())
    }
    #[cfg(not(target_os = "linux"))]
    Ok(pid.to_string())
}

pub fn external_process_identity(pid: u32) -> Result<ProcessIdentity, String> {
    if pid == 0 {
        return Err("process PID cannot be zero".into());
    }
    Ok(ProcessIdentity {
        pid,
        start_identity: sentinel_start_identity(pid)?,
    })
}

fn verify_live_process(identity: &ProcessIdentity) -> Result<(), String> {
    if identity.start_identity.trim().is_empty() {
        return Err("process start identity cannot be empty".into());
    }
    if external_process_identity(identity.pid)? != *identity {
        return Err("process PID/start identity mismatch".into());
    }
    Ok(())
}

#[cfg(unix)]
fn spawn_lease_sentinel(
    lease: &RepositoryLeaseGuard,
    config: &ServerConfig,
    kind: &str,
    id: &str,
    external: Option<ProcessIdentity>,
) -> Result<LeaseSentinel, String> {
    if let Some(external) = &external {
        verify_live_process(external)?;
    }
    let token = lease.child_token().map_err(|error| error.to_string())?;
    let mut command = std::process::Command::new("sh");
    command.args(["-c", "trap '' TERM; while :; do sleep 3600; done"]);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    token.inherit_into_std(&mut command);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let metadata_root = config
        .endpoint
        .parent()
        .ok_or_else(|| "contract endpoint has no parent".to_owned())?
        .join("sentinels");
    std::fs::create_dir_all(&metadata_root).map_err(|error| error.to_string())?;
    let key = format!("{:x}", Sha256::digest(format!("{kind}\0{id}").as_bytes()));
    let metadata = metadata_root.join(format!("{kind}-{key}.json"));
    let record = SentinelMetadata {
        pid: child.id(),
        start_identity: sentinel_start_identity(child.id())?,
        external: external.clone(),
        termination_handle: mint_token(
            &config.auth_token,
            child.id().into(),
            external
                .as_ref()
                .map_or(id, |value| value.start_identity.as_str()),
        ),
    };
    let bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
    if let Err(cause) = std::fs::write(&metadata, bytes) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(cause.to_string());
    }
    Ok(LeaseSentinel {
        child,
        metadata,
        external,
        termination_handle: record.termination_handle,
    })
}

#[cfg(not(unix))]
fn spawn_lease_sentinel(
    _lease: &RepositoryLeaseGuard,
    _config: &ServerConfig,
    _kind: &str,
    _id: &str,
    _external: Option<ProcessIdentity>,
) -> Result<LeaseSentinel, String> {
    Err("lease sentinels are unsupported on this platform".into())
}

fn terminate_sentinel(sentinel: &mut LeaseSentinel) -> Result<(), String> {
    sentinel.child.kill().map_err(|error| error.to_string())?;
    sentinel.child.wait().map_err(|error| error.to_string())?;
    match std::fs::remove_file(&sentinel.metadata) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn verify_worker_termination(
    sentinel: &LeaseSentinel,
    evidence: &WorkerTerminationEvidence,
) -> Result<(), String> {
    if evidence.termination_handle.is_empty()
        || !constant_time_eq(&evidence.termination_handle, &sentinel.termination_handle)
    {
        return Err("termination evidence handle is empty or invalid".into());
    }
    Ok(())
}

fn verify_termination_identity(
    external: &ProcessIdentity,
    termination_handle: &str,
    evidence: &TerminationEvidence,
) -> Result<(), String> {
    if &evidence.process != external {
        return Err("termination evidence belongs to another process identity".into());
    }
    if evidence.termination_handle.is_empty()
        || !constant_time_eq(&evidence.termination_handle, termination_handle)
    {
        return Err("termination evidence handle is empty or invalid".into());
    }
    match external_process_identity(evidence.process.pid) {
        Ok(actual) if actual == evidence.process => {
            Err("process is still live; wait evidence is premature".into())
        }
        Ok(_) => Ok(()), // PID was reused only after the registered process exited.
        Err(_) => Ok(()),
    }
}

/// Validate closed, identity-bound host wait evidence without releasing a
/// lease sentinel. Useful to reject spoofed or premature evidence at adapters.
pub fn verify_process_termination(
    registered: &ProcessIdentity,
    termination_handle: &str,
    evidence: &TerminationEvidence,
) -> Result<(), String> {
    verify_termination_identity(registered, termination_handle, evidence)
}

/// Reap a sentinel left by a hard-dead server after external recovery has
/// verified that its associated worker/Git child terminated.
#[cfg(unix)]
pub fn reap_orphan_sentinel(
    metadata: &Path,
    termination_evidence: &TerminationEvidence,
) -> Result<(), String> {
    let record: SentinelMetadata =
        serde_json::from_slice(&std::fs::read(metadata).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    if sentinel_start_identity(record.pid)? != record.start_identity {
        return Err("sentinel PID identity changed; refusing to signal it".into());
    }
    let external = record.external.as_ref().ok_or_else(|| "worker sentinel has no recoverable process identity; original host EndWorker is required".to_owned())?;
    verify_termination_identity(external, &record.termination_handle, termination_evidence)?;
    // SAFETY: PID reuse is guarded by the recorded process start identity.
    if unsafe { libc::kill(record.pid as i32, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    for _ in 0..100 {
        let mut status = 0;
        // SAFETY: nonblocking reap of the exact PID when it is our orphaned
        // child; ECHILD is expected when another init/subreaper owns it.
        let _ = unsafe { libc::waitpid(record.pid as i32, &mut status, libc::WNOHANG) };
        // SAFETY: signal zero only probes existence.
        if unsafe { libc::kill(record.pid as i32, 0) } != 0 {
            std::fs::remove_file(metadata).map_err(|error| error.to_string())?;
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err("orphan sentinel did not terminate".into())
}

#[derive(PartialEq, Eq)]
struct SessionIdentity {
    repo_root: PathBuf,
    plan_dir: PathBuf,
    plan_ref: String,
    run_uid: String,
}

struct State {
    session: Option<Session>,
    authoring: Option<AuthoringContractSession>,
    closed: Option<ClosedReceipt>,
    nonce: u64,
}

struct AuthoringContractSession {
    token: String,
    base_oid: String,
    last_request_id: u64,
    last_request_digest: Option<String>,
    last_response: Option<Response>,
    reservations: BTreeMap<String, String>,
    workers: BTreeMap<String, LeaseSentinel>,
    coordinator: crate::orchestrator::AuthoringCoordinator,
    _lease: RepositoryLeaseGuard,
}

struct ClosedReceipt {
    token: String,
    source_oid: String,
    plan_oid: String,
    request_id: u64,
    permit: CleanupPermit,
}

/// Long-lived server. Connection loss does not release its session lease.
pub struct PlanContractServer {
    config: ServerConfig,
    leases: Arc<RepositoryLeaseRegistry>,
    state: Arc<Mutex<State>>,
    shutdown: CancellationToken,
}

impl PlanContractServer {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            leases: Arc::new(RepositoryLeaseRegistry::new()),
            state: Arc::new(Mutex::new(State {
                session: None,
                authoring: None,
                closed: None,
                nonce: 0,
            })),
            shutdown: CancellationToken::new(),
        }
    }

    #[cfg(unix)]
    pub async fn serve(self) -> Result<(), ContractError> {
        use std::os::unix::fs::PermissionsExt;
        let parent = self.config.endpoint.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "endpoint has no parent")
        })?;
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        if self.config.endpoint.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "refusing to replace an existing endpoint",
            )
            .into());
        }
        // Some Unix filesystems reject chmod(2) on socket nodes. Bind under a
        // restrictive umask so the endpoint is never briefly over-permissive.
        // SAFETY: umask is restored immediately around the synchronous bind.
        let previous_umask = unsafe { libc::umask(0o177) };
        let bound = UnixListener::bind(&self.config.endpoint);
        // SAFETY: restore the process mask captured immediately above.
        unsafe { libc::umask(previous_umask) };
        let listener = bound?;
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = self.shutdown.cancelled() => break,
            };
            let (stream, _) = accepted?;
            let config = self.config.clone();
            let leases = Arc::clone(&self.leases);
            let state = Arc::clone(&self.state);
            let shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                let _ = serve_connection(stream, config, leases, state, shutdown).await;
            });
        }
        Ok(())
    }

    #[cfg(windows)]
    pub async fn serve(self) -> Result<(), ContractError> {
        use tokio::net::windows::named_pipe::ServerOptions;
        let name = windows_pipe_name(&self.config.endpoint, &self.config.auth_token);
        let mut first = true;
        loop {
            let mut options = ServerOptions::new();
            options.reject_remote_clients(true);
            if first {
                options.first_pipe_instance(true);
                first = false;
            }
            let server = options.create(&name)?;
            tokio::select! {
                connected = server.connect() => connected?,
                () = self.shutdown.cancelled() => break,
            }
            let config = self.config.clone();
            let leases = Arc::clone(&self.leases);
            let state = Arc::clone(&self.state);
            let shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                let _ = serve_connection(server, config, leases, state, shutdown).await;
            });
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    pub async fn serve(self) -> Result<(), ContractError> {
        Err(ContractError::Unsupported)
    }
}

async fn serve_connection<S>(
    stream: S,
    config: ServerConfig,
    leases: Arc<RepositoryLeaseRegistry>,
    state: Arc<Mutex<State>>,
    shutdown: CancellationToken,
) -> Result<(), ContractError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let response = if line.len() > MAX_LINE_BYTES {
            error(
                "input_too_large",
                "request exceeds the JSON-lines input limit",
            )
        } else {
            match serde_json::from_str::<AuthenticatedRequest>(&line) {
                Ok(request) if constant_time_eq(&request.auth_token, &config.auth_token) => {
                    dispatch(request.request, &config, &leases, &state, &shutdown).await
                }
                Ok(_) => error("authentication_failed", "invalid endpoint credential"),
                Err(parse) => error("invalid_request", &parse.to_string()),
            }
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write.write_all(&encoded).await?;
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthenticatedRequest {
    auth_token: String,
    request: Request,
}

async fn dispatch(
    request: Request,
    config: &ServerConfig,
    leases: &Arc<RepositoryLeaseRegistry>,
    state: &Mutex<State>,
    shutdown: &CancellationToken,
) -> Response {
    match request {
        Request::Hello { protocol } => {
            if protocol != PROTOCOL_VERSION {
                return error("protocol_mismatch", "unsupported protocol version");
            }
            Response::Hello {
                protocol: PROTOCOL_VERSION,
                build_source_oid: embedded_build_source_oid()
                    .unwrap_or(&config.build_source_oid)
                    .to_owned(),
            }
        }
        Request::StartAuthoringSession {
            protocol,
            repo_root,
            base_branch,
            run_uid,
            expected_base_oid,
        } => {
            if protocol != PROTOCOL_VERSION {
                return error("protocol_mismatch", "unsupported protocol version");
            }
            if run_uid.is_empty() || base_branch.is_empty() {
                return error(
                    "invalid_authoring_identity",
                    "base branch and run UID are required",
                );
            }
            let repo_root = match std::fs::canonicalize(repo_root) {
                Ok(value) => value,
                Err(cause) => return error("repository_unavailable", &cause.to_string()),
            };
            let coordinator = crate::orchestrator::AuthoringCoordinator::new(
                repo_root.clone(),
                base_branch,
                Arc::clone(leases),
            )
            .with_held_session();
            let actual = match coordinator.start_authoring_session().await {
                Ok(value) => value.expected_base_oid,
                Err(cause) => return error("authoring_unavailable", &cause.to_string()),
            };
            if actual != expected_base_oid {
                return error("stale_base_oid", "base ref no longer matches expected OID");
            }
            let mut locked = state.lock().await;
            if locked.session.is_some() {
                return error(
                    "session_identity_conflict",
                    "an execution session already owns this endpoint",
                );
            }
            if let Some(authoring) = &locked.authoring {
                if authoring.base_oid != actual {
                    return error(
                        "session_identity_conflict",
                        "active authoring session has another base",
                    );
                }
                return Response::AuthoringReady {
                    session_token: authoring.token.clone(),
                    base_oid: actual,
                };
            }
            let lease = match leases
                .acquire(
                    &repo_root,
                    RepositoryLeaseOwner {
                        plan_dir: PathBuf::from("docs/plans"),
                        run_uid,
                        operation: RepositoryLeaseOperation::RegisterPlan,
                    },
                    &CancellationToken::new(),
                )
                .await
            {
                Ok(value) => value,
                Err(cause) => return error("lease_failed", &cause.to_string()),
            };
            locked.nonce += 1;
            let token = mint_token(&config.auth_token, locked.nonce, &actual);
            locked.authoring = Some(AuthoringContractSession {
                token: token.clone(),
                base_oid: actual.clone(),
                last_request_id: 0,
                last_request_digest: None,
                last_response: None,
                reservations: BTreeMap::new(),
                workers: BTreeMap::new(),
                coordinator,
                _lease: lease,
            });
            Response::AuthoringReady {
                session_token: token,
                base_oid: actual,
            }
        }
        Request::InspectAuthoring { mutation, count } => {
            let mut locked = state.lock().await;
            let Some(authoring) = locked.authoring.as_mut() else {
                return error("session_missing", "no active authoring session");
            };
            let digest = authoring_digest(&("inspect", count));
            if let Some(response) = validate_authoring_mutation(authoring, &mutation, &digest) {
                return response;
            }
            if !(1..=5).contains(&count) {
                return error(
                    "invalid_count",
                    "authoring count must be between one and five",
                );
            }
            authoring.last_request_id = mutation.request_id;
            let numbers = match authoring.coordinator.reserve_numbers(count.into()).await {
                Ok(value) => value,
                Err(cause) => return error("reservation_failed", &cause.to_string()),
            };
            authoring.reservations.clear();
            let reservations = numbers
                .into_iter()
                .enumerate()
                .map(|(index, number)| {
                    let token = mint_token(
                        &authoring.token,
                        mutation.request_id + index as u64,
                        &number,
                    );
                    authoring.reservations.insert(token.clone(), number);
                    token
                })
                .collect();
            let response = Response::AuthoringInspected {
                base_oid: authoring.base_oid.clone(),
                reservations,
            };
            authoring.last_request_digest = Some(digest);
            authoring.last_response = Some(response.clone());
            response
        }
        Request::PublishBlueprint {
            mutation,
            reservation,
            blueprint,
            commit,
        } => {
            let mut locked = state.lock().await;
            let Some(authoring) = locked.authoring.as_mut() else {
                return error("session_missing", "no active authoring session");
            };
            let digest = authoring_digest(&("publish", &reservation, &blueprint, commit));
            if let Some(response) = validate_authoring_mutation(authoring, &mutation, &digest) {
                return response;
            }
            authoring.last_request_id = mutation.request_id;
            let Some(number) = authoring.reservations.get(&reservation).cloned() else {
                return error(
                    "reservation_invalid",
                    "unknown or already-consumed authoring reservation",
                );
            };
            let response = match authoring
                .coordinator
                .render_blueprint_candidate_reserved(blueprint, commit, Some(number.clone()))
                .await
            {
                Ok((key, crate::api::CommandOutcome::AwaitingCommit)) => {
                    Response::BlueprintPublished {
                        outcome: AuthoringOutcome::AwaitingCommit,
                        plan_dir: key.relative_dir,
                        registration_oid: None,
                    }
                }
                Ok((key, crate::api::CommandOutcome::PlanRegistered { registration_oid }))
                | Ok((
                    key,
                    crate::api::CommandOutcome::PlanGenerated {
                        registration_oid, ..
                    },
                )) => Response::BlueprintPublished {
                    outcome: AuthoringOutcome::Registered,
                    plan_dir: key.relative_dir,
                    registration_oid: Some(registration_oid),
                },
                Ok(_) => error(
                    "authoring_outcome_invalid",
                    "authoring coordinator returned an unexpected outcome",
                ),
                Err(cause) => error("blueprint_invalid", &cause.to_string()),
            };
            if !matches!(response, Response::Error { .. }) {
                authoring.reservations.remove(&reservation);
            }
            authoring.last_request_digest = Some(digest);
            authoring.last_response = Some(response.clone());
            response
        }
        Request::BeginAuthorWorker {
            mutation,
            worker_id,
        } => {
            let mut locked = state.lock().await;
            let Some(authoring) = locked.authoring.as_mut() else {
                return error("session_missing", "no active authoring session");
            };
            let digest = authoring_digest(&("begin_author_worker", &worker_id));
            if let Some(response) = validate_authoring_mutation(authoring, &mutation, &digest) {
                return response;
            }
            authoring.last_request_id = mutation.request_id;
            let response = if worker_id.is_empty() || authoring.workers.contains_key(&worker_id) {
                error("worker_conflict", "worker ID is empty or already live")
            } else {
                match spawn_lease_sentinel(
                    &authoring._lease,
                    config,
                    "author-worker",
                    &worker_id,
                    None,
                ) {
                    Ok(sentinel) => {
                        let termination_handle = sentinel.termination_handle.clone();
                        authoring.workers.insert(worker_id.clone(), sentinel);
                        Response::WorkerBegun {
                            worker_id,
                            termination_handle,
                        }
                    }
                    Err(cause) => error("sentinel_failed", &cause),
                }
            };
            authoring.last_request_digest = Some(digest);
            authoring.last_response = Some(response.clone());
            response
        }
        Request::EndAuthorWorker {
            mutation,
            worker_id,
            termination_evidence,
        } => {
            let mut locked = state.lock().await;
            let Some(authoring) = locked.authoring.as_mut() else {
                return error("session_missing", "no active authoring session");
            };
            let digest =
                authoring_digest(&("end_author_worker", &worker_id, &termination_evidence));
            if let Some(response) = validate_authoring_mutation(authoring, &mutation, &digest) {
                return response;
            }
            authoring.last_request_id = mutation.request_id;
            let response = if let Some(sentinel) = authoring.workers.get(&worker_id) {
                if let Err(cause) = verify_worker_termination(sentinel, &termination_evidence) {
                    error("worker_evidence_invalid", &cause)
                } else {
                    let mut sentinel = authoring.workers.remove(&worker_id).expect("checked above");
                    if let Err(cause) = terminate_sentinel(&mut sentinel) {
                        authoring.workers.insert(worker_id.clone(), sentinel);
                        error("sentinel_reap_failed", &cause)
                    } else {
                        Response::WorkerEnded { worker_id }
                    }
                }
            } else {
                error("worker_evidence_required", "worker is not live")
            };
            authoring.last_request_digest = Some(digest);
            authoring.last_response = Some(response.clone());
            response
        }
        Request::CloseAuthoring { mutation } => {
            let mut locked = state.lock().await;
            let Some(authoring) = locked.authoring.as_ref() else {
                return error("session_missing", "no active authoring session");
            };
            if let Some(response) =
                validate_authoring_mutation(authoring, &mutation, &authoring_digest(&"close"))
            {
                return response;
            }
            if !authoring.workers.is_empty() {
                return error(
                    "children_live",
                    "cannot close authoring while workers remain live",
                );
            }
            locked.authoring = None;
            Response::AuthoringClosed
        }
        Request::CancelSession { mutation } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_ref() else {
                return error("session_missing", "no active execution session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            if !session.workers.is_empty() || !session.git_children.is_empty() {
                return error(
                    "children_live",
                    "cannot cancel while workers or Git children remain live",
                );
            }
            locked.session = None;
            Response::Cancelled
        }
        Request::StartSession {
            protocol,
            repo_root,
            plan_dir,
            plan_ref,
            run_uid,
            expected_plan_oid,
        } => {
            if protocol != PROTOCOL_VERSION {
                return error("protocol_mismatch", "unsupported protocol version");
            }
            let repo_root = match std::fs::canonicalize(&repo_root) {
                Ok(path) => path,
                Err(cause) => return error("repository_unavailable", &cause.to_string()),
            };
            let identity = SessionIdentity {
                repo_root: repo_root.clone(),
                plan_dir: plan_dir.clone(),
                plan_ref: plan_ref.clone(),
                run_uid: run_uid.clone(),
            };
            let plan = match crate::plan::PlanKey::parse(plan_dir.clone()) {
                Ok(plan) => plan,
                Err(cause) => return error("invalid_plan", &cause.to_string()),
            };
            let coordinator = match crate::orchestrator::PlanContractCoordinator::new(
                &repo_root, plan, &plan_ref, &run_uid,
            ) {
                Ok(coordinator) => coordinator,
                Err(cause) => return error("invalid_session", &cause),
            };
            let actual = match resolve_ref(&repo_root, &plan_ref) {
                Ok(oid) => oid,
                Err(message) => return error("plan_ref_unavailable", &message),
            };
            if actual != expected_plan_oid {
                return error("stale_plan_oid", "plan ref no longer matches expected OID");
            }
            let mut locked = state.lock().await;
            if locked.authoring.is_some() {
                return error(
                    "session_identity_conflict",
                    "an authoring session already owns this endpoint",
                );
            }
            if let Some(session) = &locked.session {
                if session.identity != identity || session.plan_oid != actual {
                    return error(
                        "session_identity_conflict",
                        "active session belongs to a different repository, plan, ref, or run",
                    );
                }
                return Response::Ready {
                    session_token: session.token.clone(),
                    plan_oid: session.plan_oid.clone(),
                };
            }
            // A new, independently verified session supersedes a prior close
            // receipt. The receipt is retained only to make response loss at
            // the close boundary recoverable.
            locked.closed = None;
            let lease = match leases
                .acquire(
                    &repo_root,
                    RepositoryLeaseOwner {
                        plan_dir,
                        run_uid,
                        operation: RepositoryLeaseOperation::Run,
                    },
                    &CancellationToken::new(),
                )
                .await
            {
                Ok(lease) => lease,
                Err(cause) => return error("lease_failed", &cause.to_string()),
            };
            locked.nonce += 1;
            let token = mint_token(&config.auth_token, locked.nonce, &actual);
            locked.session = Some(Session {
                token: token.clone(),
                source_oid: actual.clone(),
                plan_oid: actual.clone(),
                last_request_id: 0,
                workers: BTreeMap::new(),
                git_children: BTreeMap::new(),
                coordinator,
                phase_a: BTreeMap::new(),
                candidates: BTreeMap::new(),
                prepared: None,
                final_oid: None,
                identity,
                _lease: lease,
            });
            Response::Ready {
                session_token: token,
                plan_oid: actual,
            }
        }
        Request::BeginWorker {
            mutation,
            worker_id,
            process,
        } => {
            mutate(state, mutation, |session| {
                if worker_id.is_empty() || session.workers.contains_key(&worker_id) {
                    return error("worker_conflict", "worker ID is empty or already live");
                }
                let sentinel = match spawn_lease_sentinel(
                    &session._lease,
                    config,
                    "worker",
                    &worker_id,
                    process,
                ) {
                    Ok(value) => value,
                    Err(cause) => return error("sentinel_failed", &cause),
                };
                let termination_handle = sentinel.termination_handle.clone();
                session.workers.insert(worker_id.clone(), sentinel);
                Response::WorkerBegun {
                    worker_id,
                    termination_handle,
                }
            })
            .await
        }
        Request::EndWorker {
            mutation,
            worker_id,
            termination_evidence,
        } => {
            mutate(state, mutation, |session| {
                let Some(sentinel) = session.workers.get(&worker_id) else {
                    return error("worker_evidence_required", "worker is not live");
                };
                if let Err(cause) = verify_worker_termination(sentinel, &termination_evidence) {
                    return error("worker_evidence_invalid", &cause);
                }
                let mut sentinel = session.workers.remove(&worker_id).expect("checked above");
                if let Err(cause) = terminate_sentinel(&mut sentinel) {
                    session.workers.insert(worker_id.clone(), sentinel);
                    return error("sentinel_reap_failed", &cause);
                }
                Response::WorkerEnded { worker_id }
            })
            .await
        }
        Request::BeginGitChild {
            mutation,
            child_id,
            process,
        } => {
            mutate(state, mutation, |session| {
                if child_id.is_empty() || session.git_children.contains_key(&child_id) {
                    return error(
                        "git_child_conflict",
                        "Git child ID is empty or already live",
                    );
                }
                let sentinel = match spawn_lease_sentinel(
                    &session._lease,
                    config,
                    "git",
                    &child_id,
                    Some(process),
                ) {
                    Ok(value) => value,
                    Err(cause) => return error("sentinel_failed", &cause),
                };
                let termination_handle = sentinel.termination_handle.clone();
                session.git_children.insert(child_id.clone(), sentinel);
                Response::GitChildBegun {
                    child_id,
                    termination_handle,
                }
            })
            .await
        }
        Request::EndGitChild {
            mutation,
            child_id,
            wait_evidence,
        } => {
            mutate(state, mutation, |session| {
                let Some(sentinel) = session.git_children.get(&child_id) else {
                    return error("git_child_evidence_required", "Git child is not live");
                };
                if let Err(cause) = verify_termination_identity(
                    sentinel
                        .external
                        .as_ref()
                        .expect("Git sentinel has process identity"),
                    &sentinel.termination_handle,
                    &wait_evidence,
                ) {
                    return error("git_child_evidence_invalid", &cause);
                }
                let mut sentinel = session
                    .git_children
                    .remove(&child_id)
                    .expect("checked above");
                if let Err(cause) = terminate_sentinel(&mut sentinel) {
                    session.git_children.insert(child_id.clone(), sentinel);
                    return error("sentinel_reap_failed", &cause);
                }
                Response::GitChildEnded { child_id }
            })
            .await
        }
        Request::ReconcileTask { mutation, task } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            let task = match crate::plan::TaskId::parse(task) {
                Ok(task) => task,
                Err(cause) => return error("invalid_task", &cause.to_string()),
            };
            match session.coordinator.inspect_task(&task).await {
                Ok(value) => Response::TaskReconciled {
                    evidence: match value {
                        crate::landing::TaskEvidenceState::RegistrationOnly => {
                            TaskEvidence::RegistrationOnly
                        }
                        crate::landing::TaskEvidenceState::Claimed { claim_oid } => {
                            TaskEvidence::Claimed { claim_oid }
                        }
                        crate::landing::TaskEvidenceState::LandingPending {
                            implementation_oid,
                        } => TaskEvidence::LandingPending { implementation_oid },
                        crate::landing::TaskEvidenceState::Complete {
                            implementation_oid,
                            status_oid,
                        } => TaskEvidence::Complete {
                            implementation_oid,
                            status_oid,
                        },
                    },
                },
                Err(cause) => error("reconciliation_failed", &cause),
            }
        }
        Request::InspectPlan { mutation } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            match session.coordinator.load() {
                Ok(plan) => {
                    let plan_dir = plan.key.relative_dir.clone();
                    Response::PlanInspected {
                        plan: PlanSnapshot {
                            plan_dir: plan_dir.clone(),
                            title: plan.title,
                            source_digest: plan.source_digest.to_string(),
                            executable_digest: plan.executable_digest.to_string(),
                            tasks: plan
                                .tasks
                                .into_iter()
                                .map(|task| PlanTaskSnapshot {
                                    id: task.frontmatter.id.to_string(),
                                    title: task.frontmatter.title,
                                    status: task.frontmatter.status.to_string(),
                                    gated: task.frontmatter.gated,
                                    dependencies: task
                                        .frontmatter
                                        .depends_on
                                        .into_iter()
                                        .map(|id| id.to_string())
                                        .collect(),
                                    source_path: task.source_path,
                                    scope_path: plan_dir.join("SCOPE.md"),
                                    architecture_path: plan_dir.join("ARCHITECTURE.md"),
                                })
                                .collect(),
                        },
                    }
                }
                Err(cause) => error("plan_invalid", &cause),
            }
        }
        Request::ReadyTasks { mutation } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            let ready = match session.coordinator.ready_tasks() {
                Ok(value) => value,
                Err(cause) => return error("readiness_failed", &cause),
            };
            let plan = match session.coordinator.load() {
                Ok(value) => value,
                Err(cause) => return error("plan_invalid", &cause),
            };
            let wanted = ready
                .tasks
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>();
            let plan_dir = plan.key.relative_dir.clone();
            let tasks = plan
                .tasks
                .into_iter()
                .filter(|task| wanted.contains(&task.frontmatter.id))
                .map(|task| PlanTaskSnapshot {
                    id: task.frontmatter.id.to_string(),
                    title: task.frontmatter.title,
                    status: task.frontmatter.status.to_string(),
                    gated: task.frontmatter.gated,
                    dependencies: task
                        .frontmatter
                        .depends_on
                        .into_iter()
                        .map(|id| id.to_string())
                        .collect(),
                    source_path: task.source_path,
                    scope_path: plan_dir.join("SCOPE.md"),
                    architecture_path: plan_dir.join("ARCHITECTURE.md"),
                })
                .collect();
            Response::ReadyTasks {
                tasks,
                complete: ready.complete,
                blocked: ready.blocked,
            }
        }
        Request::CheckCandidate { mutation, task } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            let task_id = match crate::plan::TaskId::parse(task.clone()) {
                Ok(value) => value,
                Err(cause) => return error("invalid_task", &cause.to_string()),
            };
            match session.coordinator.validate_candidate(&task_id).await {
                Ok(()) => {
                    let token = mint_token(&session.token, mutation.request_id, &task);
                    session.candidates.insert(task.clone(), token.clone());
                    Response::CandidateChecked {
                        task,
                        candidate_token: token,
                    }
                }
                Err(cause) => error("candidate_invalid", &cause),
            }
        }
        Request::LandTask {
            mutation,
            task,
            candidate_token,
            last_updated,
        } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            if session
                .candidates
                .get(&task)
                .is_none_or(|token| !constant_time_eq(token, &candidate_token))
            {
                return error(
                    "candidate_token_invalid",
                    "landing requires the latest server-issued candidate token",
                );
            }
            let task_id = match crate::plan::TaskId::parse(task.clone()) {
                Ok(value) => value,
                Err(cause) => return error("invalid_task", &cause.to_string()),
            };
            let implementation_oid = match session.coordinator.land_phase_a(&task_id).await {
                Ok(value) => value,
                Err(cause) => return error("phase_a_failed", &cause),
            };
            let typed = match session.coordinator.parse_oid(implementation_oid.clone()) {
                Ok(value) => value,
                Err(cause) => return error("phase_a_invalid", &cause),
            };
            match session
                .coordinator
                .complete_task(&task_id, &implementation_oid, typed, &last_updated)
                .await
            {
                Ok(plan_oid) => {
                    session.plan_oid = plan_oid.clone();
                    session.candidates.remove(&task);
                    Response::TaskLanded {
                        task,
                        implementation_oid,
                        plan_oid,
                    }
                }
                Err(cause) => error("phase_b_failed", &cause),
            }
        }
        Request::ClaimTask {
            mutation,
            task,
            last_updated,
        } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            let task_id = match crate::plan::TaskId::parse(task.clone()) {
                Ok(task) => task,
                Err(cause) => return error("invalid_task", &cause.to_string()),
            };
            match session
                .coordinator
                .claim_task(&task_id, &session.plan_oid, &last_updated)
                .await
            {
                Ok(plan_oid) => {
                    session.plan_oid = plan_oid.clone();
                    match session.coordinator.ensure_task_worktree(&task_id).await {
                        Ok(worktree) => Response::TaskClaimed {
                            task,
                            plan_oid,
                            worktree,
                        },
                        Err(cause) => error("worktree_failed", &cause),
                    }
                }
                Err(cause) => error("claim_failed", &cause),
            }
        }
        Request::LandPhaseA { mutation, task } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            let task_id = match crate::plan::TaskId::parse(task.clone()) {
                Ok(value) => value,
                Err(cause) => return error("invalid_task", &cause.to_string()),
            };
            match session.coordinator.land_phase_a(&task_id).await {
                Ok(oid) => {
                    let token = mint_token(&session.token, mutation.request_id, &oid);
                    session.plan_oid = oid.clone();
                    session.phase_a.insert(task.clone(), (token.clone(), oid));
                    Response::PhaseALanded {
                        task,
                        evidence_token: token,
                    }
                }
                Err(cause) => error("phase_a_failed", &cause),
            }
        }
        Request::CommitPhaseB {
            mutation,
            task,
            last_updated,
        } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            let task_id = match crate::plan::TaskId::parse(task.clone()) {
                Ok(value) => value,
                Err(cause) => return error("invalid_task", &cause.to_string()),
            };
            let Some((_, retained_oid)) = session.phase_a.get(&task).cloned() else {
                return error(
                    "phase_a_missing",
                    "Phase B requires server-retained Phase A evidence",
                );
            };
            let oid = match session.coordinator.parse_oid(retained_oid.clone()) {
                Ok(value) => value,
                Err(cause) => return error("invalid_phase_a_oid", &cause),
            };
            match session
                .coordinator
                .complete_task(&task_id, &session.plan_oid, oid, &last_updated)
                .await
            {
                Ok(plan_oid) => {
                    session.plan_oid = plan_oid.clone();
                    Response::PhaseBCommitted {
                        task,
                        phase_a_oid: retained_oid,
                        plan_oid,
                    }
                }
                Err(cause) => error("phase_b_failed", &cause),
            }
        }
        Request::SetDisposition {
            mutation,
            task,
            disposition,
        } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            let task_id = match crate::plan::TaskId::parse(task.clone()) {
                Ok(value) => value,
                Err(cause) => return error("invalid_task", &cause.to_string()),
            };
            let action = match disposition {
                Disposition::Ungate => crate::api::TaskDispositionAction::Ungate,
                Disposition::Drop { reason } => crate::api::TaskDispositionAction::Drop { reason },
            };
            match session
                .coordinator
                .set_disposition(&task_id, &session.plan_oid, action)
                .await
            {
                Ok(plan_oid) => {
                    session.plan_oid = plan_oid.clone();
                    Response::DispositionCommitted { task, plan_oid }
                }
                Err(cause) => error("disposition_failed", &cause),
            }
        }
        Request::TransitionTask {
            mutation,
            task,
            transition,
        } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            let task_id = match crate::plan::TaskId::parse(task.clone()) {
                Ok(value) => value,
                Err(cause) => return error("invalid_task", &cause.to_string()),
            };
            let action = match transition {
                SourceTransition::Block { reason } => {
                    crate::orchestrator::PlanSourceAction::Block { reason }
                }
                SourceTransition::Retry => crate::orchestrator::PlanSourceAction::Retry,
                SourceTransition::Requeue => crate::orchestrator::PlanSourceAction::Requeue,
                SourceTransition::Cancel => crate::orchestrator::PlanSourceAction::Cancel,
            };
            match session
                .coordinator
                .transition_task(&task_id, &session.plan_oid, action)
                .await
            {
                Ok(plan_oid) => {
                    session.plan_oid = plan_oid.clone();
                    session.phase_a.remove(&task);
                    Response::TransitionCommitted { task, plan_oid }
                }
                Err(cause) => error("transition_failed", &cause),
            }
        }
        Request::PrepareFinalization {
            mutation,
            mode,
            last_updated,
        } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            match session
                .coordinator
                .prepare_finalization(&session.plan_oid, &mode, &last_updated)
                .await
            {
                Ok(prepared) => {
                    session.plan_oid = prepared.prepared_oid.clone();
                    let oid = prepared.prepared_oid.clone();
                    session.prepared = Some(prepared);
                    Response::FinalizationPrepared { plan_oid: oid }
                }
                Err(cause) => error("prepare_finalization_failed", &cause),
            }
        }
        Request::IntegrateFinalization {
            mutation,
            manual_oid,
        } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            let Some(prepared) = session.prepared.clone() else {
                return error(
                    "finalization_not_prepared",
                    "Phase F requires server-retained Phase P",
                );
            };
            match session
                .coordinator
                .integrate_finalization(&prepared, manual_oid.as_deref())
                .await
            {
                Ok(final_oid) => {
                    session.final_oid = Some(final_oid.clone());
                    Response::FinalizationIntegrated { final_oid }
                }
                Err(cause) => error("integrate_finalization_failed", &cause),
            }
        }
        Request::CompleteFinalization {
            mutation,
            last_updated,
        } => {
            let mut locked = state.lock().await;
            let Some(session) = locked.session.as_mut() else {
                return error("session_missing", "no active session");
            };
            if let Some(response) = validate_mutation(session, &mutation) {
                return response;
            }
            session.last_request_id = mutation.request_id;
            let Some(prepared) = session.prepared.clone() else {
                return error(
                    "finalization_not_prepared",
                    "Phase C requires server-retained Phase P",
                );
            };
            let Some(final_oid) = session.final_oid.clone() else {
                return error(
                    "finalization_not_integrated",
                    "Phase C requires server-retained Phase F",
                );
            };
            match session
                .coordinator
                .complete_finalization(&prepared, &final_oid, &last_updated)
                .await
            {
                Ok(completion_oid) => Response::FinalizationCompleted {
                    completion_oid,
                    final_oid,
                },
                Err(cause) => error("complete_finalization_failed", &cause),
            }
        }
        Request::Close {
            mutation,
            completion,
            retention_manifest,
        } => {
            {
                let locked = state.lock().await;
                if let Some(closed) = &locked.closed {
                    if constant_time_eq(&mutation.session_token, &closed.token)
                        && mutation.request_id == closed.request_id
                        && mutation.expected_source_oid == closed.source_oid
                        && mutation.expected_plan_oid == closed.plan_oid
                        && retention_manifest.digest == closed.permit.retention_manifest_digest
                    {
                        return Response::Closed {
                            cleanup_permit: Some(closed.permit.clone()),
                        };
                    }
                    return error(
                        "session_closed",
                        "the session is closed and only its exact Close may be replayed",
                    );
                }
            }
            let repo = {
                let locked = state.lock().await;
                match &locked.session {
                    Some(session)
                        if completion.run_uid == session.identity.run_uid
                            && session
                                .identity
                                .plan_dir
                                .file_name()
                                .and_then(|name| name.to_str())
                                == Some(completion.plan.as_str()) =>
                    {
                        session.identity.repo_root.clone()
                    }
                    Some(_) => {
                        return error(
                            "final_identity_mismatch",
                            "completion evidence belongs to another plan or run",
                        );
                    }
                    None => return error("session_missing", "no active session"),
                }
            };
            if let Err(message) = verify_completion(&repo, &completion) {
                return error("final_not_verified", &message);
            }
            if let Err(message) = verify_manifest(&retention_manifest) {
                return error("manifest_invalid", &message);
            }
            let retention_manifest_digest = retention_manifest.digest;
            let close_token = mutation.session_token.clone();
            let close_source_oid = mutation.expected_source_oid.clone();
            let close_plan_oid = mutation.expected_plan_oid.clone();
            let close_request_id = mutation.request_id;
            let response = mutate(state, mutation, |session| {
                if !session.workers.is_empty() {
                    return error("workers_live", "cannot close while workers remain live");
                }
                if !session.git_children.is_empty() {
                    return error(
                        "git_children_live",
                        "cannot close while Git children remain live",
                    );
                }
                Response::Closed {
                    cleanup_permit: Some(CleanupPermit {
                        retention_manifest_digest,
                    }),
                }
            })
            .await;
            if matches!(response, Response::Closed { .. }) {
                if let Response::Closed {
                    cleanup_permit: Some(permit),
                } = &response
                    && let Err(cause) = persist_cleanup_permit(&config.endpoint, permit)
                {
                    return error("cleanup_receipt_failed", &cause);
                }
                let mut locked = state.lock().await;
                if let Response::Closed {
                    cleanup_permit: Some(permit),
                } = &response
                {
                    locked.closed = Some(ClosedReceipt {
                        token: close_token,
                        source_oid: close_source_oid,
                        plan_oid: close_plan_oid,
                        request_id: close_request_id,
                        permit: permit.clone(),
                    });
                }
                locked.session = None;
                shutdown.cancel();
            }
            response
        }
    }
}

async fn mutate(
    state: &Mutex<State>,
    mutation: Mutation,
    action: impl FnOnce(&mut Session) -> Response,
) -> Response {
    let mut locked = state.lock().await;
    let Some(session) = locked.session.as_mut() else {
        return error("session_missing", "no active session");
    };
    if !constant_time_eq(&mutation.session_token, &session.token) {
        return error("session_token_invalid", "unknown or cross-session token");
    }
    if mutation.request_id <= session.last_request_id {
        return error("request_replayed", "request ID must increase monotonically");
    }
    if mutation.expected_source_oid != session.source_oid
        || mutation.expected_plan_oid != session.plan_oid
    {
        return error(
            "stale_expected_oid",
            "expected source/ref OIDs do not match the session",
        );
    }
    session.last_request_id = mutation.request_id;
    action(session)
}

fn validate_mutation(session: &Session, mutation: &Mutation) -> Option<Response> {
    if !constant_time_eq(&mutation.session_token, &session.token) {
        return Some(error(
            "session_token_invalid",
            "unknown or cross-session token",
        ));
    }
    if mutation.request_id <= session.last_request_id {
        return Some(error(
            "request_replayed",
            "request ID must increase monotonically",
        ));
    }
    if mutation.expected_source_oid != session.source_oid
        || mutation.expected_plan_oid != session.plan_oid
    {
        return Some(error(
            "stale_expected_oid",
            "expected source/ref OIDs do not match the session",
        ));
    }
    None
}

fn validate_authoring_mutation(
    session: &AuthoringContractSession,
    mutation: &Mutation,
    digest: &str,
) -> Option<Response> {
    if !constant_time_eq(&mutation.session_token, &session.token) {
        return Some(error(
            "session_token_invalid",
            "unknown or cross-session token",
        ));
    }
    if mutation.request_id == session.last_request_id {
        if session.last_request_digest.as_deref() == Some(digest) {
            return session.last_response.clone();
        }
        return Some(error(
            "request_payload_mismatch",
            "request ID was already used for a different authoring payload",
        ));
    }
    if mutation.request_id < session.last_request_id {
        return Some(error(
            "request_replayed",
            "request ID must increase monotonically",
        ));
    }
    if mutation.expected_source_oid != session.base_oid
        || mutation.expected_plan_oid != session.base_oid
    {
        return Some(error(
            "stale_expected_oid",
            "authoring mutations must name the exact session base OID",
        ));
    }
    None
}

fn authoring_digest(value: &impl Serialize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("closed authoring payload serializes"))
    )
}

fn error(code: &str, message: &str) -> Response {
    Response::Error {
        diagnostic: Diagnostic {
            code: code.into(),
            message: message.into(),
        },
    }
}

fn resolve_ref(repo: &Path, reference: &str) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--verify", reference])
        .current_dir(repo)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

fn verify_completion(repo: &Path, evidence: &CompletionEvidence) -> Result<(), String> {
    let tip = resolve_ref(repo, &evidence.base_ref)?;
    if tip != evidence.completion_oid {
        return Err("base ref does not point at the claimed completion commit".into());
    }
    let message = std::process::Command::new("git")
        .args(["show", "-s", "--format=%B", &evidence.completion_oid])
        .current_dir(repo)
        .output()
        .map_err(|cause| cause.to_string())?;
    if !message.status.success() {
        return Err("completion commit is unavailable".into());
    }
    let message = String::from_utf8_lossy(&message.stdout);
    for trailer in [
        "Makina-Phase: completion".to_owned(),
        format!("Makina-Plan: {}", evidence.plan),
        format!("Makina-Run: {}", evidence.run_uid),
        format!("Makina-Final-Commit: {}", evidence.final_oid),
    ] {
        if message.lines().filter(|line| *line == trailer).count() != 1 {
            return Err(format!(
                "missing or duplicate completion trailer: {trailer}"
            ));
        }
    }
    Ok(())
}

fn verify_manifest(manifest: &RetentionManifest) -> Result<(), String> {
    if !manifest.root.is_absolute() || manifest.paths.is_empty() {
        return Err("manifest root must be absolute and paths nonempty".into());
    }
    let canonical_root = std::fs::canonicalize(&manifest.root)
        .map_err(|error| format!("manifest root is unavailable: {error}"))?;
    if canonical_root != manifest.root {
        return Err("manifest root must be canonical and cannot be a symlink".into());
    }
    for path in &manifest.paths {
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err("manifest path escapes its owned root".into());
        }
        let mut cursor = manifest.root.clone();
        for component in path.components() {
            cursor.push(component.as_os_str());
            match std::fs::symlink_metadata(&cursor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err("manifest paths cannot traverse symlinks".into());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.to_string()),
            }
        }
        if !manifest.file_digests.contains_key(path) {
            return Err("every retained artifact requires a content digest".into());
        }
    }
    let expected = retention_manifest_with_digests(
        manifest.root.clone(),
        manifest.paths.clone(),
        manifest.file_digests.clone(),
    );
    if expected.digest != manifest.digest {
        return Err("manifest digest mismatch".into());
    }
    Ok(())
}

fn mint_token(auth: &str, nonce: u64, oid: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hash = Sha256::new();
    hash.update(auth.as_bytes());
    hash.update(nonce.to_le_bytes());
    hash.update(now.to_le_bytes());
    hash.update(std::process::id().to_le_bytes());
    hash.update(oid.as_bytes());
    format!("{:x}", hash.finalize())
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

#[derive(Clone, Debug)]
pub struct PlanContractClient {
    endpoint: PathBuf,
    auth_token: String,
}

impl PlanContractClient {
    pub fn new(endpoint: PathBuf, auth_token: String) -> Self {
        Self {
            endpoint,
            auth_token,
        }
    }

    pub async fn start_authoring_session(
        &self,
        repo_root: PathBuf,
        base_branch: String,
        run_uid: String,
        expected_base_oid: String,
    ) -> Result<Response, ContractError> {
        self.request(Request::StartAuthoringSession {
            protocol: PROTOCOL_VERSION,
            repo_root,
            base_branch,
            run_uid,
            expected_base_oid,
        })
        .await
    }

    pub async fn inspect_authoring(
        &self,
        mutation: Mutation,
        count: u8,
    ) -> Result<Response, ContractError> {
        self.request(Request::InspectAuthoring { mutation, count })
            .await
    }

    pub async fn publish_blueprint(
        &self,
        mutation: Mutation,
        reservation: String,
        blueprint: crate::api::GeneratedPlanBlueprint,
        commit: bool,
    ) -> Result<Response, ContractError> {
        self.request(Request::PublishBlueprint {
            mutation,
            reservation,
            blueprint,
            commit,
        })
        .await
    }

    pub async fn begin_author_worker(
        &self,
        mutation: Mutation,
        worker_id: String,
    ) -> Result<Response, ContractError> {
        self.request(Request::BeginAuthorWorker {
            mutation,
            worker_id,
        })
        .await
    }

    pub async fn end_author_worker(
        &self,
        mutation: Mutation,
        worker_id: String,
        termination_evidence: WorkerTerminationEvidence,
    ) -> Result<Response, ContractError> {
        self.request(Request::EndAuthorWorker {
            mutation,
            worker_id,
            termination_evidence,
        })
        .await
    }

    pub async fn close_authoring(&self, mutation: Mutation) -> Result<Response, ContractError> {
        self.request(Request::CloseAuthoring { mutation }).await
    }

    pub async fn cancel_session(&self, mutation: Mutation) -> Result<Response, ContractError> {
        self.request(Request::CancelSession { mutation }).await
    }

    pub async fn reconcile_task(
        &self,
        mutation: Mutation,
        task: impl Into<String>,
    ) -> Result<Response, ContractError> {
        self.request(Request::ReconcileTask {
            mutation,
            task: task.into(),
        })
        .await
    }

    pub async fn inspect_plan(&self, mutation: Mutation) -> Result<Response, ContractError> {
        self.request(Request::InspectPlan { mutation }).await
    }

    pub async fn ready_tasks(&self, mutation: Mutation) -> Result<Response, ContractError> {
        self.request(Request::ReadyTasks { mutation }).await
    }

    pub async fn check_candidate(
        &self,
        mutation: Mutation,
        task: impl Into<String>,
    ) -> Result<Response, ContractError> {
        self.request(Request::CheckCandidate {
            mutation,
            task: task.into(),
        })
        .await
    }

    pub async fn land_task(
        &self,
        mutation: Mutation,
        task: impl Into<String>,
        candidate_token: impl Into<String>,
        last_updated: impl Into<String>,
    ) -> Result<Response, ContractError> {
        self.request(Request::LandTask {
            mutation,
            task: task.into(),
            candidate_token: candidate_token.into(),
            last_updated: last_updated.into(),
        })
        .await
    }

    pub async fn claim_task(
        &self,
        mutation: Mutation,
        task: impl Into<String>,
        last_updated: impl Into<String>,
    ) -> Result<Response, ContractError> {
        self.request(Request::ClaimTask {
            mutation,
            task: task.into(),
            last_updated: last_updated.into(),
        })
        .await
    }

    pub async fn commit_phase_b(
        &self,
        mutation: Mutation,
        task: impl Into<String>,
        last_updated: impl Into<String>,
    ) -> Result<Response, ContractError> {
        self.request(Request::CommitPhaseB {
            mutation,
            task: task.into(),
            last_updated: last_updated.into(),
        })
        .await
    }

    pub async fn land_phase_a(
        &self,
        mutation: Mutation,
        task: impl Into<String>,
    ) -> Result<Response, ContractError> {
        self.request(Request::LandPhaseA {
            mutation,
            task: task.into(),
        })
        .await
    }

    pub async fn set_disposition(
        &self,
        mutation: Mutation,
        task: impl Into<String>,
        disposition: Disposition,
    ) -> Result<Response, ContractError> {
        self.request(Request::SetDisposition {
            mutation,
            task: task.into(),
            disposition,
        })
        .await
    }

    pub async fn transition_task(
        &self,
        mutation: Mutation,
        task: impl Into<String>,
        transition: SourceTransition,
    ) -> Result<Response, ContractError> {
        self.request(Request::TransitionTask {
            mutation,
            task: task.into(),
            transition,
        })
        .await
    }

    pub async fn prepare_finalization(
        &self,
        mutation: Mutation,
        mode: impl Into<String>,
        last_updated: impl Into<String>,
    ) -> Result<Response, ContractError> {
        self.request(Request::PrepareFinalization {
            mutation,
            mode: mode.into(),
            last_updated: last_updated.into(),
        })
        .await
    }

    pub async fn integrate_finalization(
        &self,
        mutation: Mutation,
        manual_oid: Option<String>,
    ) -> Result<Response, ContractError> {
        self.request(Request::IntegrateFinalization {
            mutation,
            manual_oid,
        })
        .await
    }

    pub async fn complete_finalization(
        &self,
        mutation: Mutation,
        last_updated: impl Into<String>,
    ) -> Result<Response, ContractError> {
        self.request(Request::CompleteFinalization {
            mutation,
            last_updated: last_updated.into(),
        })
        .await
    }

    #[cfg(unix)]
    pub async fn request(&self, request: Request) -> Result<Response, ContractError> {
        let stream = UnixStream::connect(&self.endpoint).await?;
        exchange_request(stream, &self.auth_token, request).await
    }

    #[cfg(windows)]
    pub async fn request(&self, request: Request) -> Result<Response, ContractError> {
        use tokio::net::windows::named_pipe::ClientOptions;
        let name = windows_pipe_name(&self.endpoint, &self.auth_token);
        let stream = loop {
            match ClientOptions::new().open(&name) {
                Ok(stream) => break stream,
                Err(error) if error.raw_os_error() == Some(231) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        exchange_request(stream, &self.auth_token, request).await
    }

    #[cfg(not(any(unix, windows)))]
    pub async fn request(&self, _request: Request) -> Result<Response, ContractError> {
        Err(ContractError::Unsupported)
    }
}

async fn exchange_request<S>(
    mut stream: S,
    auth_token: &str,
    request: Request,
) -> Result<Response, ContractError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut encoded = serde_json::to_vec(&AuthenticatedRequest {
        auth_token: auth_token.to_owned(),
        request,
    })?;
    encoded.push(b'\n');
    stream.write_all(&encoded).await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    Ok(serde_json::from_str(&line)?)
}
