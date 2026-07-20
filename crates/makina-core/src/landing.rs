//! Recoverable coordinator bookkeeping commits.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use thiserror::Error;
use tokio::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedWrite {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusLandingIdentity {
    pub plan: String,
    pub task: String,
    pub run: String,
    pub landing: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionFailpoint {
    Render,
    Replace,
    Validate,
    Commit,
    Cas,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispositionIdentity {
    pub plan: String,
    pub task: String,
    pub run: String,
    pub action: String,
    pub previous_source_digest: String,
    pub source_digest: String,
    pub previous_plan_digest: String,
    pub plan_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceTransitionIdentity {
    pub plan: String,
    pub task: String,
    pub run: String,
    /// Closed coordinator action such as `retry`, `requeue`, `cancel`, or
    /// `blocker`. It is recorded verbatim in `Makina-Transition`.
    pub action: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizationIdentity {
    pub plan: String,
    pub run: String,
    pub mode: String,
    pub expected_base: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizationEvidence {
    pub prepared_oid: String,
    pub final_oid: Option<String>,
    pub completion_oid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskEvidenceState {
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

#[derive(Debug, Error)]
pub enum LandingError {
    #[error("invalid landing identity: {0}")]
    InvalidIdentity(&'static str),
    #[error("path is not coordinator-owned: {0}")]
    UnownedPath(PathBuf),
    #[error("injected transaction failure at {0:?}")]
    Injected(TransactionFailpoint),
    #[error("ref moved: expected {expected}, found {actual}")]
    RefMoved { expected: String, actual: String },
    #[error("git {command} failed: {stderr}")]
    Git { command: String, stderr: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

fn validate(identity: &StatusLandingIdentity) -> Result<(), LandingError> {
    for (name, value) in [
        ("plan", &identity.plan),
        ("task", &identity.task),
        ("run", &identity.run),
        ("landing", &identity.landing),
    ] {
        if value.is_empty() || value.contains(['\n', '\r', '\0']) {
            return Err(LandingError::InvalidIdentity(name));
        }
    }
    Ok(())
}

fn validate_writes(writes: &[OwnedWrite]) -> Result<(), LandingError> {
    let mut seen = std::collections::BTreeSet::new();
    for write in writes {
        let text = write.path.to_string_lossy();
        let owned = text == "docs/plans/STATUS.md"
            || (text.starts_with("docs/plans/")
                && (text.ends_with("/STATUS.md")
                    || (text.contains("/tasks/") && text.ends_with(".md"))));
        if write.path.is_absolute()
            || write
                .path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            || !owned
            || !seen.insert(write.path.clone())
        {
            return Err(LandingError::UnownedPath(write.path.clone()));
        }
    }
    Ok(())
}

async fn git(repo: &Path, args: &[&str]) -> Result<String, LandingError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    if !out.status.success() {
        return Err(LandingError::Git {
            command: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn exact(message: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    let values = message
        .lines()
        .filter_map(|l| l.strip_prefix(&prefix))
        .collect::<Vec<_>>();
    (values.len() == 1).then(|| values[0].to_owned())
}

async fn ref_checked_out(repo: &Path, reference: &str) -> Result<bool, LandingError> {
    let listing = git(repo, &["worktree", "list", "--porcelain"]).await?;
    Ok(listing
        .lines()
        .any(|line| line.strip_prefix("branch ") == Some(reference)))
}

pub async fn inspect_finalization_evidence(
    repo: &Path,
    plan_ref: &str,
    identity: &FinalizationIdentity,
) -> Result<Option<FinalizationEvidence>, LandingError> {
    let log = git(
        repo,
        &[
            "log",
            "--first-parent",
            "--format=%H%x00%B%x00%x1e",
            plan_ref,
        ],
    )
    .await?;
    let mut prepared = Vec::new();
    for record in log.split('\x1e') {
        let Some((oid, message)) = record.trim_matches(['\n', '\0']).split_once('\0') else {
            continue;
        };
        if exact(message, "Makina-Phase").as_deref() == Some("finalization-prepared")
            && exact(message, "Makina-Plan").as_deref() == Some(&identity.plan)
            && exact(message, "Makina-Run").as_deref() == Some(&identity.run)
            && exact(message, "Makina-Final-Mode").as_deref() == Some(&identity.mode)
        {
            prepared.push(oid.to_owned());
        }
    }
    match prepared.first() {
        None => Ok(None),
        Some(p) => Ok(Some(FinalizationEvidence {
            prepared_oid: p.clone(),
            final_oid: None,
            completion_oid: None,
        })),
    }
}

/// Build Phase P as a child of the exact retained tip and CAS-publish it while
/// verifying the target base has not moved.
#[allow(clippy::too_many_arguments)]
pub async fn commit_finalization_prepared(
    repo: &Path,
    plan_ref: &str,
    base_ref: &str,
    expected_plan: &str,
    expected_base: &str,
    writes: &[OwnedWrite],
    identity: &FinalizationIdentity,
    reuse_existing: bool,
) -> Result<String, LandingError> {
    validate_writes(writes)?;
    if reuse_existing
        && let Some(evidence) = inspect_finalization_evidence(repo, plan_ref, identity).await?
    {
        return Ok(evidence.prepared_oid);
    }
    if git(repo, &["rev-parse", plan_ref]).await? != expected_plan {
        return Err(LandingError::RefMoved {
            expected: expected_plan.into(),
            actual: git(repo, &["rev-parse", plan_ref]).await?,
        });
    }
    if git(repo, &["rev-parse", base_ref]).await? != expected_base {
        return Err(LandingError::RefMoved {
            expected: expected_base.into(),
            actual: git(repo, &["rev-parse", base_ref]).await?,
        });
    }
    git(repo, &["checkout", "--detach", expected_plan]).await?;
    let merge = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge", "--squash", expected_base])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    if !merge.status.success() {
        return Err(LandingError::Git {
            command: "prepare finalization tree".into(),
            stderr: String::from_utf8_lossy(&merge.stderr).trim().into(),
        });
    }
    for write in writes {
        let path = repo.join(&write.path);
        tokio::fs::write(path, &write.bytes).await?;
    }
    let paths = writes
        .iter()
        .map(|w| w.path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut add = vec!["add", "--"];
    add.extend(paths.iter().map(String::as_str));
    git(repo, &add).await?;
    let message = format!(
        "chore(plan): prepare {} finalization\n\nMakina-Phase: finalization-prepared\nMakina-Final-Mode: {}\nMakina-Plan: {}\nMakina-Run: {}\nMakina-Expected-Base: {}",
        identity.plan, identity.mode, identity.plan, identity.run, expected_base
    );
    git(repo, &["commit", "-m", &message]).await?;
    let candidate = git(repo, &["rev-parse", "HEAD"]).await?;
    let transaction = format!(
        "start\nverify {base_ref} {expected_base}\nupdate {plan_ref} {candidate} {expected_plan}\nprepare\ncommit\n"
    );
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    use tokio::io::AsyncWriteExt;
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(transaction.as_bytes())
        .await?;
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        return Err(LandingError::Git {
            command: "publish Phase P".into(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().into(),
        });
    }
    Ok(candidate)
}

/// Integrate the exact Phase-P tree onto the expected base as Phase F.
pub async fn commit_final_integration(
    repo: &Path,
    base_ref: &str,
    expected_base: &str,
    prepared_oid: &str,
    identity: &FinalizationIdentity,
    merge_commit: bool,
    tasks: &[(String, String)],
) -> Result<String, LandingError> {
    let log = git(
        repo,
        &[
            "log",
            "--first-parent",
            "--format=%H%x00%B%x00%x1e",
            base_ref,
        ],
    )
    .await?;
    let hits = log
        .split('\x1e')
        .filter_map(|record| {
            let (oid, message) = record.trim_matches(['\n', '\0']).split_once('\0')?;
            (exact(message, "Makina-Phase").as_deref() == Some("final-integration")
                && exact(message, "Makina-Plan").as_deref() == Some(&identity.plan)
                && exact(message, "Makina-Run").as_deref() == Some(&identity.run)
                && exact(message, "Makina-Plan-Tip").as_deref() == Some(prepared_oid))
            .then(|| oid.to_owned())
        })
        .collect::<Vec<_>>();
    match hits.as_slice() {
        [oid] => return Ok(oid.clone()),
        [_, _, ..] => {
            return Err(LandingError::Git {
                command: "inspect final integration".into(),
                stderr: "ambiguous Phase F evidence".into(),
            });
        }
        [] => {}
    }
    let actual = git(repo, &["rev-parse", base_ref]).await?;
    if actual != expected_base {
        return Err(LandingError::RefMoved {
            expected: expected_base.into(),
            actual,
        });
    }
    if ref_checked_out(repo, base_ref).await? {
        return Err(LandingError::Git {
            command: "publish final integration".into(),
            stderr: "target base is checked out; Phase P remains pending".into(),
        });
    }
    if merge_commit {
        for (_, implementation_oid) in tasks {
            git(
                repo,
                &[
                    "merge-base",
                    "--is-ancestor",
                    implementation_oid,
                    prepared_oid,
                ],
            )
            .await
            .map_err(|_| LandingError::Git {
                command: "verify merge-commit task ancestry".into(),
                stderr: format!(
                    "task implementation {implementation_oid} is not retained by Phase P"
                ),
            })?;
        }
    }
    let tree = git(repo, &["rev-parse", &format!("{prepared_oid}^{{tree}}")]).await?;
    let mut message = format!(
        "feat(plan): integrate {}\n\nMakina-Phase: final-integration\nMakina-Final-Mode: {}\nMakina-Plan: {}\nMakina-Run: {}\nMakina-Plan-Tip: {}",
        identity.plan, identity.mode, identity.plan, identity.run, prepared_oid
    );
    for (task, oid) in tasks {
        message.push_str(&format!("\nMakina-Task: {task} {oid}"));
    }
    let mut args = vec!["commit-tree", tree.as_str(), "-p", expected_base];
    if merge_commit {
        args.extend(["-p", prepared_oid]);
    }
    args.extend(["-m", message.as_str()]);
    let candidate = git(repo, &args).await?;
    git(repo, &["update-ref", base_ref, &candidate, expected_base])
        .await
        .map_err(|_| LandingError::RefMoved {
            expected: expected_base.into(),
            actual: candidate.clone(),
        })?;
    Ok(candidate)
}

/// Validate a human-produced Manual F at the current base tip.
pub async fn verify_manual_final_integration(
    repo: &Path,
    base_ref: &str,
    expected_base: &str,
    prepared_oid: &str,
    supplied_oid: &str,
    identity: &FinalizationIdentity,
) -> Result<String, LandingError> {
    if git(repo, &["rev-parse", base_ref]).await? != supplied_oid {
        return Err(LandingError::Git {
            command: "verify manual finalization".into(),
            stderr: "supplied commit is not the current base tip".into(),
        });
    }
    let parents = git(repo, &["show", "-s", "--format=%P", supplied_oid]).await?;
    let parents = parents.split_whitespace().collect::<Vec<_>>();
    if parents.first().copied() != Some(expected_base)
        || parents.len() > 2
        || parents.get(1).is_some_and(|parent| *parent != prepared_oid)
    {
        return Err(LandingError::Git {
            command: "verify manual finalization".into(),
            stderr: "manual commit parents do not match the prepared recipe".into(),
        });
    }
    if git(repo, &["rev-parse", &format!("{supplied_oid}^{{tree}}")]).await?
        != git(repo, &["rev-parse", &format!("{prepared_oid}^{{tree}}")]).await?
    {
        return Err(LandingError::Git {
            command: "verify manual finalization".into(),
            stderr: "manual commit tree differs from Phase P".into(),
        });
    }
    let message = git(repo, &["show", "-s", "--format=%B", supplied_oid]).await?;
    if exact(&message, "Makina-Phase").as_deref() != Some("final-integration")
        || exact(&message, "Makina-Plan").as_deref() != Some(&identity.plan)
        || exact(&message, "Makina-Run").as_deref() != Some(&identity.run)
        || exact(&message, "Makina-Plan-Tip").as_deref() != Some(prepared_oid)
    {
        return Err(LandingError::Git {
            command: "verify manual finalization".into(),
            stderr: "manual commit lacks the exact finalization trailers".into(),
        });
    }
    Ok(supplied_oid.into())
}

/// Commit Phase C on top of F; STATUS writes record F, never C.
pub async fn commit_finalization_completion(
    repo: &Path,
    base_ref: &str,
    final_oid: &str,
    writes: &[OwnedWrite],
    identity: &FinalizationIdentity,
) -> Result<String, LandingError> {
    validate_writes(writes)?;
    let message = git(repo, &["show", "-s", "--format=%B", base_ref]).await?;
    if exact(&message, "Makina-Phase").as_deref() == Some("completion")
        && exact(&message, "Makina-Plan").as_deref() == Some(&identity.plan)
        && exact(&message, "Makina-Run").as_deref() == Some(&identity.run)
        && exact(&message, "Makina-Final-Commit").as_deref() == Some(final_oid)
    {
        return git(repo, &["rev-parse", base_ref]).await;
    }
    let actual = git(repo, &["rev-parse", base_ref]).await?;
    if actual != final_oid {
        return Err(LandingError::RefMoved {
            expected: final_oid.into(),
            actual,
        });
    }
    if ref_checked_out(repo, base_ref).await? {
        return Err(LandingError::Git {
            command: "publish completion".into(),
            stderr: "target base is checked out; Phase F remains pending".into(),
        });
    }
    git(repo, &["checkout", "--detach", final_oid]).await?;
    for write in writes {
        tokio::fs::write(repo.join(&write.path), &write.bytes).await?;
    }
    let paths = writes
        .iter()
        .map(|write| write.path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut add = vec!["add", "--"];
    add.extend(paths.iter().map(String::as_str));
    git(repo, &add).await?;
    let commit_message = format!(
        "chore(plan): complete {}\n\nMakina-Phase: completion\nMakina-Plan: {}\nMakina-Run: {}\nMakina-Final-Commit: {}",
        identity.plan, identity.plan, identity.run, final_oid
    );
    git(repo, &["commit", "-m", &commit_message]).await?;
    let candidate = git(repo, &["rev-parse", "HEAD"]).await?;
    git(repo, &["update-ref", base_ref, &candidate, final_oid])
        .await
        .map_err(|_| LandingError::RefMoved {
            expected: final_oid.into(),
            actual: candidate.clone(),
        })?;
    Ok(candidate)
}

/// Inspect one task exclusively from the retained plan lineage. This is the
/// source-of-truth used before checkpoint overlay on start/resume/reset.
pub async fn inspect_task_evidence(
    repo: &Path,
    plan_ref: &str,
    plan: &str,
    task: &str,
) -> Result<TaskEvidenceState, LandingError> {
    for (name, value) in [("plan", plan), ("task", task)] {
        if value.is_empty() || value.contains(['\n', '\r', '\0']) {
            return Err(LandingError::InvalidIdentity(name));
        }
    }
    let log = git(
        repo,
        &[
            "log",
            "--first-parent",
            "--format=%H%x00%B%x00%x1e",
            plan_ref,
        ],
    )
    .await?;
    let mut claims = Vec::new();
    let mut landings = Vec::new();
    let mut statuses = Vec::new();
    for record in log.split('\x1e') {
        let Some((oid, message)) = record.trim_matches(['\n', '\0']).split_once('\0') else {
            continue;
        };
        if exact(message, "Makina-Plan").as_deref() != Some(plan) {
            continue;
        }
        if exact(message, "Makina-Phase").as_deref() == Some("task-transition")
            && matches!(exact(message, "Makina-Task").as_deref(), Some(value) if value == task || value == "all")
        {
            // A source-first retry/cancel supersedes older volatile claim/A
            // evidence for dispatch purposes without erasing its history.
            return Ok(TaskEvidenceState::RegistrationOnly);
        }
        if exact(message, "Makina-Task").as_deref() != Some(task) {
            continue;
        }
        match exact(message, "Makina-Phase").as_deref() {
            Some("task-claim") => claims.push(oid.to_owned()),
            Some("task-status") => {
                let landing =
                    exact(message, "Makina-Landing").ok_or_else(|| LandingError::Git {
                        command: "inspect task-status evidence".into(),
                        stderr: "task-status lacks Makina-Landing".into(),
                    })?;
                statuses.push((oid.to_owned(), landing));
            }
            None if exact(message, "Makina-Run").is_some() => landings.push(oid.to_owned()),
            _ => {}
        }
    }
    if claims.len() > 1 || landings.len() > 1 || statuses.len() > 1 {
        return Err(LandingError::Git {
            command: "inspect task evidence".into(),
            stderr: "ambiguous duplicate phase evidence".into(),
        });
    }
    match (claims.first(), landings.first(), statuses.first()) {
        (_, Some(landing), Some((status, linked))) if linked == landing => {
            Ok(TaskEvidenceState::Complete {
                implementation_oid: landing.clone(),
                status_oid: status.clone(),
            })
        }
        (_, Some(landing), None) => Ok(TaskEvidenceState::LandingPending {
            implementation_oid: landing.clone(),
        }),
        (Some(claim), None, None) => Ok(TaskEvidenceState::Claimed {
            claim_oid: claim.clone(),
        }),
        (None, None, None) => Ok(TaskEvidenceState::RegistrationOnly),
        _ => Err(LandingError::Git {
            command: "inspect task evidence".into(),
            stderr: "phase ordering/linkage is incoherent".into(),
        }),
    }
}

/// Find the unique exact Phase-B evidence on the plan's first-parent lineage.
pub async fn find_status_landing(
    repo: &Path,
    plan_ref: &str,
    identity: &StatusLandingIdentity,
) -> Result<Option<String>, LandingError> {
    validate(identity)?;
    let log = git(
        repo,
        &[
            "log",
            "--first-parent",
            "--format=%H%x00%B%x00%x1e",
            plan_ref,
        ],
    )
    .await?;
    let hits = log
        .split('\x1e')
        .filter_map(|record| {
            let (oid, message) = record.trim_matches(['\n', '\0']).split_once('\0')?;
            (exact(message, "Makina-Phase").as_deref() == Some("task-status")
                && exact(message, "Makina-Plan").as_deref() == Some(&identity.plan)
                && exact(message, "Makina-Task").as_deref() == Some(&identity.task)
                && exact(message, "Makina-Run").as_deref() == Some(&identity.run)
                && exact(message, "Makina-Landing").as_deref() == Some(&identity.landing))
            .then(|| oid.to_owned())
        })
        .collect::<Vec<_>>();
    match hits.as_slice() {
        [] => Ok(None),
        [oid] => Ok(Some(oid.clone())),
        _ => Err(LandingError::Git {
            command: "verify task-status evidence".into(),
            stderr: "ambiguous exact Phase-B evidence".into(),
        }),
    }
}

async fn find_claim(
    repo: &Path,
    plan_ref: &str,
    identity: &StatusLandingIdentity,
) -> Result<Option<String>, LandingError> {
    let log = git(
        repo,
        &[
            "log",
            "--first-parent",
            "--format=%H%x00%B%x00%x1e",
            plan_ref,
        ],
    )
    .await?;
    let hits = log
        .split('\x1e')
        .filter_map(|record| {
            let (oid, message) = record.trim_matches(['\n', '\0']).split_once('\0')?;
            (exact(message, "Makina-Phase").as_deref() == Some("task-claim")
                && exact(message, "Makina-Plan").as_deref() == Some(&identity.plan)
                && exact(message, "Makina-Task").as_deref() == Some(&identity.task)
                && exact(message, "Makina-Run").as_deref() == Some(&identity.run))
            .then(|| oid.to_owned())
        })
        .collect::<Vec<_>>();
    match hits.as_slice() {
        [] => Ok(None),
        [oid] => Ok(Some(oid.clone())),
        _ => Err(LandingError::Git {
            command: "verify task-claim evidence".into(),
            stderr: "ambiguous exact claim evidence".into(),
        }),
    }
}

pub async fn commit_task_claim(
    repo: &Path,
    plan_ref: &str,
    expected_old: &str,
    writes: &[OwnedWrite],
    identity: &StatusLandingIdentity,
) -> Result<String, LandingError> {
    validate(identity)?;
    validate_writes(writes)?;
    if let Some(oid) = find_claim(repo, plan_ref, identity).await? {
        return Ok(oid);
    }
    commit_status(
        repo,
        plan_ref,
        expected_old,
        writes,
        identity,
        "task-claim",
        None,
        None,
    )
    .await
}

pub async fn commit_task_claim_with_failpoint(
    repo: &Path,
    plan_ref: &str,
    expected_old: &str,
    writes: &[OwnedWrite],
    identity: &StatusLandingIdentity,
    failpoint: TransactionFailpoint,
) -> Result<String, LandingError> {
    validate(identity)?;
    validate_writes(writes)?;
    commit_status(
        repo,
        plan_ref,
        expected_old,
        writes,
        identity,
        "task-claim",
        None,
        Some(failpoint),
    )
    .await
}

/// Commit already-validated coordinator-owned files detached, then CAS the plan ref.
/// A failed CAS retains the detached commit and workspace for recovery.
pub async fn commit_task_status(
    repo: &Path,
    plan_ref: &str,
    expected_old: &str,
    writes: &[OwnedWrite],
    identity: &StatusLandingIdentity,
) -> Result<String, LandingError> {
    validate(identity)?;
    validate_writes(writes)?;
    if let Some(oid) = find_status_landing(repo, plan_ref, identity).await? {
        return Ok(oid);
    }
    commit_status(
        repo,
        plan_ref,
        expected_old,
        writes,
        identity,
        "task-status",
        Some(&identity.landing),
        None,
    )
    .await
}

/// Test/recovery harness exposing each handled Phase-B interruption boundary.
pub async fn commit_task_status_with_failpoint(
    repo: &Path,
    plan_ref: &str,
    expected_old: &str,
    writes: &[OwnedWrite],
    identity: &StatusLandingIdentity,
    failpoint: TransactionFailpoint,
) -> Result<String, LandingError> {
    validate(identity)?;
    validate_writes(writes)?;
    commit_status(
        repo,
        plan_ref,
        expected_old,
        writes,
        identity,
        "task-status",
        Some(&identity.landing),
        Some(failpoint),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn commit_status(
    repo: &Path,
    plan_ref: &str,
    expected_old: &str,
    writes: &[OwnedWrite],
    identity: &StatusLandingIdentity,
    phase: &str,
    landing: Option<&str>,
    failpoint: Option<TransactionFailpoint>,
) -> Result<String, LandingError> {
    if failpoint == Some(TransactionFailpoint::Render) {
        return Err(LandingError::Injected(TransactionFailpoint::Render));
    }
    let actual = git(repo, &["rev-parse", plan_ref]).await?;
    if actual != expected_old {
        return Err(LandingError::RefMoved {
            expected: expected_old.into(),
            actual,
        });
    }
    git(repo, &["checkout", "--detach", expected_old]).await?;
    let prepared = async {
        for (index, write) in writes.iter().enumerate() {
            let path = repo.join(&write.path);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let temp = path.with_extension("makina-tmp");
            tokio::fs::write(&temp, &write.bytes).await?;
            tokio::fs::rename(&temp, &path).await?;
            if index == 0 && failpoint == Some(TransactionFailpoint::Replace) { return Err(LandingError::Injected(TransactionFailpoint::Replace)); }
        }
        if failpoint == Some(TransactionFailpoint::Validate) { return Err(LandingError::Injected(TransactionFailpoint::Validate)); }
        let paths = writes.iter().map(|w| w.path.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let mut add = vec!["add", "--"]; add.extend(paths.iter().map(String::as_str));
        git(repo, &add).await?;
        let mut message = format!("chore(plan): record {} {phase}\n\nMakina-Phase: {phase}\nMakina-Plan: {}\nMakina-Task: {}\nMakina-Run: {}", identity.task, identity.plan, identity.task, identity.run);
        if let Some(landing) = landing { message.push_str(&format!("\nMakina-Landing: {landing}")); }
        if failpoint == Some(TransactionFailpoint::Commit) { return Err(LandingError::Injected(TransactionFailpoint::Commit)); }
        git(repo, &["commit", "--allow-empty", "-m", &message]).await?;
        git(repo, &["rev-parse", "HEAD"]).await
    }.await;
    let candidate = match prepared {
        Ok(candidate) => candidate,
        Err(error) => {
            // Handled preparation failures restore the exact Phase-A tree and
            // index. Process death never executes this path and therefore
            // retains the workspace for forensic recovery.
            let _ = git(repo, &["reset", "--hard", expected_old]).await;
            return Err(error);
        }
    };
    if failpoint == Some(TransactionFailpoint::Cas) {
        return Err(LandingError::Injected(TransactionFailpoint::Cas));
    }
    git(repo, &["update-ref", plan_ref, &candidate, expected_old])
        .await
        .map_err(|_| LandingError::RefMoved {
            expected: expected_old.into(),
            actual: candidate.clone(),
        })?;
    Ok(candidate)
}

/// Commit a source-first lifecycle transition and CAS-publish it before callers
/// mutate their volatile graph. Exact evidence is reusable after response loss.
pub async fn commit_source_transition(
    repo: &Path,
    plan_ref: &str,
    expected_old: &str,
    writes: &[OwnedWrite],
    identity: &SourceTransitionIdentity,
) -> Result<String, LandingError> {
    validate_writes(writes)?;
    for value in [
        &identity.plan,
        &identity.task,
        &identity.run,
        &identity.action,
    ] {
        if value.is_empty() || value.contains(['\n', '\r', '\0']) {
            return Err(LandingError::InvalidIdentity("source transition"));
        }
    }
    let log = git(
        repo,
        &[
            "log",
            "--first-parent",
            "--format=%H%x00%B%x00%x1e",
            plan_ref,
        ],
    )
    .await?;
    let hits = log
        .split('\x1e')
        .filter_map(|record| {
            let (oid, message) = record.trim_matches(['\n', '\0']).split_once('\0')?;
            (exact(message, "Makina-Phase").as_deref() == Some("task-transition")
                && exact(message, "Makina-Plan").as_deref() == Some(&identity.plan)
                && exact(message, "Makina-Task").as_deref() == Some(&identity.task)
                && exact(message, "Makina-Run").as_deref() == Some(&identity.run)
                && exact(message, "Makina-Transition").as_deref() == Some(&identity.action)
                && exact(message, "Makina-Previous-Plan").as_deref() == Some(expected_old))
            .then(|| oid.to_owned())
        })
        .collect::<Vec<_>>();
    match hits.as_slice() {
        [oid] => return Ok(oid.clone()),
        [_, _, ..] => {
            return Err(LandingError::Git {
                command: "verify task-transition evidence".into(),
                stderr: "ambiguous exact transition evidence".into(),
            });
        }
        [] => {}
    }
    let actual = git(repo, &["rev-parse", plan_ref]).await?;
    if actual != expected_old {
        return Err(LandingError::RefMoved {
            expected: expected_old.into(),
            actual,
        });
    }
    git(repo, &["checkout", "--detach", expected_old]).await?;
    for write in writes {
        let path = repo.join(&write.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let temp = path.with_extension("makina-tmp");
        tokio::fs::write(&temp, &write.bytes).await?;
        tokio::fs::rename(&temp, &path).await?;
    }
    let paths = writes
        .iter()
        .map(|write| write.path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut add = vec!["add", "--"];
    add.extend(paths.iter().map(String::as_str));
    git(repo, &add).await?;
    let message = format!(
        "chore(plan): {} {}\n\nMakina-Phase: task-transition\nMakina-Plan: {}\nMakina-Task: {}\nMakina-Run: {}\nMakina-Transition: {}\nMakina-Previous-Plan: {}",
        identity.action,
        identity.task,
        identity.plan,
        identity.task,
        identity.run,
        identity.action,
        expected_old
    );
    git(repo, &["commit", "-m", &message]).await?;
    let candidate = git(repo, &["rev-parse", "HEAD"]).await?;
    git(repo, &["update-ref", plan_ref, &candidate, expected_old])
        .await
        .map_err(|_| LandingError::RefMoved {
            expected: expected_old.into(),
            actual: candidate.clone(),
        })?;
    let branch = plan_ref.strip_prefix("refs/heads/").unwrap_or(plan_ref);
    git(repo, &["checkout", branch]).await?;
    Ok(candidate)
}

/// Commit a validated disposition and publish it with expected-old ref CAS.
pub async fn commit_task_disposition(
    repo: &Path,
    plan_ref: &str,
    expected_old: &str,
    writes: &[OwnedWrite],
    identity: &DispositionIdentity,
) -> Result<String, LandingError> {
    validate_writes(writes)?;
    for value in [
        &identity.plan,
        &identity.task,
        &identity.run,
        &identity.action,
        &identity.previous_source_digest,
        &identity.source_digest,
        &identity.previous_plan_digest,
        &identity.plan_digest,
    ] {
        if value.is_empty() || value.contains(['\n', '\r', '\0']) {
            return Err(LandingError::InvalidIdentity("disposition"));
        }
    }
    let actual = git(repo, &["rev-parse", plan_ref]).await?;
    if actual != expected_old {
        return Err(LandingError::RefMoved {
            expected: expected_old.into(),
            actual,
        });
    }
    git(repo, &["checkout", "--detach", expected_old]).await?;
    for write in writes {
        let path = repo.join(&write.path);
        let temp = path.with_extension("makina-tmp");
        tokio::fs::write(&temp, &write.bytes).await?;
        tokio::fs::rename(&temp, &path).await?;
    }
    let paths = writes
        .iter()
        .map(|w| w.path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut add = vec!["add", "--"];
    add.extend(paths.iter().map(String::as_str));
    git(repo, &add).await?;
    let message = format!(
        "chore(plan): {} {}\n\nMakina-Phase: task-disposition\nMakina-Plan: {}\nMakina-Task: {}\nMakina-Run: {}\nMakina-Disposition: {}\nMakina-Previous-Source-Digest: {}\nMakina-Source-Digest: {}\nMakina-Previous-Plan-Digest: {}\nMakina-Plan-Digest: {}",
        identity.action,
        identity.task,
        identity.plan,
        identity.task,
        identity.run,
        identity.action,
        identity.previous_source_digest,
        identity.source_digest,
        identity.previous_plan_digest,
        identity.plan_digest
    );
    git(repo, &["commit", "-m", &message]).await?;
    let candidate = git(repo, &["rev-parse", "HEAD"]).await?;
    git(repo, &["update-ref", plan_ref, &candidate, expected_old])
        .await
        .map_err(|_| LandingError::RefMoved {
            expected: expected_old.into(),
            actual: candidate.clone(),
        })?;
    let branch = plan_ref.strip_prefix("refs/heads/").unwrap_or(plan_ref);
    git(repo, &["checkout", branch]).await?;
    Ok(candidate)
}
