//! Source-aware, external runtime checkpoints.
//!
//! Plan documents remain authoritative. A checkpoint is accepted only when
//! its complete plan identity, executable digest, task IDs, and task source
//! paths match the freshly projected source graph.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::plan::{PlanDocument, PlanKey};
use crate::task::{TaskGraph, TaskState};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("external user state directory is unavailable")]
    StateUnavailable,
    #[error("repository root cannot be resolved: {0}")]
    Repository(std::io::Error),
    #[error("runtime state root resolves inside the repository: {0}")]
    StateInsideRepository(PathBuf),
    #[error("checkpoint I/O failed for `{path}`: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("checkpoint JSON is invalid for `{path}`: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointIdentity {
    pub plan_dir: PathBuf,
    pub executable_digest: String,
    pub task_ids: Vec<String>,
    pub task_source_paths: Vec<PathBuf>,
}

impl CheckpointIdentity {
    pub fn from_plan(plan: &PlanDocument) -> Self {
        Self {
            plan_dir: plan.key.relative_dir.clone(),
            executable_digest: plan.executable_digest.as_str().to_owned(),
            task_ids: plan
                .tasks
                .iter()
                .map(|t| t.frontmatter.id.as_str().to_owned())
                .collect(),
            task_source_paths: plan.tasks.iter().map(|t| t.source_path.clone()).collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeTaskCheckpoint {
    pub id: String,
    pub state: TaskState,
    pub gate_iterations: u32,
    pub review_iterations: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanCheckpoint {
    pub schema_version: u32,
    pub identity: CheckpointIdentity,
    pub tasks: Vec<RuntimeTaskCheckpoint>,
    #[serde(default)]
    pub active_refs: Vec<String>,
    #[serde(default)]
    pub active_worktrees: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointDisposition {
    Missing,
    Compatible,
    ReplaceClean { reason: String },
    RetainForRecovery { reason: String },
    Unavailable { reason: String },
}

pub fn inspect_checkpoint(
    plan: &PlanDocument,
    checkpoint: Option<&PlanCheckpoint>,
) -> CheckpointDisposition {
    let Some(checkpoint) = checkpoint else {
        return CheckpointDisposition::Missing;
    };
    let expected = CheckpointIdentity::from_plan(plan);
    if checkpoint.schema_version == 1 && checkpoint.identity == expected {
        return CheckpointDisposition::Compatible;
    }
    let reason = "checkpoint identity does not match current validated plan source".to_owned();
    if checkpoint.active_refs.is_empty() && checkpoint.active_worktrees.is_empty() {
        CheckpointDisposition::ReplaceClean { reason }
    } else {
        CheckpointDisposition::RetainForRecovery { reason }
    }
}

/// Resolve without creating anything. Existing ancestors are canonicalized so
/// a HOME symlink into the repository cannot bypass containment.
pub fn external_state_root(repo_root: &Path) -> Result<PathBuf, CheckpointError> {
    crate::paths::state_root(repo_root).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound if source.to_string().contains("HOME") => {
            CheckpointError::StateUnavailable
        }
        std::io::ErrorKind::InvalidInput
            if source
                .to_string()
                .contains("runtime state root resolves inside repository") =>
        {
            CheckpointError::StateInsideRepository(repo_root.to_path_buf())
        }
        _ => CheckpointError::Io {
            path: repo_root.to_path_buf(),
            source,
        },
    })
}

pub fn checkpoint_path(repo_root: &Path, key: &PlanKey) -> Result<PathBuf, CheckpointError> {
    let mut hash = Sha256::new();
    hash.update(key.relative_dir.to_string_lossy().as_bytes());
    let encoded = format!("{:x}", hash.finalize());
    Ok(external_state_root(repo_root)?
        .join("checkpoints")
        .join(encoded)
        .join("checkpoint.json"))
}

pub async fn load_checkpoint(
    repo_root: &Path,
    key: &PlanKey,
) -> Result<Option<PlanCheckpoint>, CheckpointError> {
    let path = checkpoint_path(repo_root, key)?;
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(CheckpointError::Io { path, source }),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| CheckpointError::Json { path, source })
}

/// Read recovery evidence from Git itself. Checkpoint fields are only a cache;
/// callers merge this evidence before deciding whether stale state is safe to
/// archive.
pub async fn inspect_repository_evidence(
    repo_root: &Path,
    key: &PlanKey,
) -> Result<(Vec<String>, Vec<PathBuf>), CheckpointError> {
    let plan_ref = format!("refs/heads/plan/{}-{}", key.number, key.slug);
    let task_ref_prefix = format!("refs/heads/task/{}-", key.number);
    let task_ref_pattern = format!("{task_ref_prefix}*");
    let refs = tokio::process::Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            &plan_ref,
            &task_ref_pattern,
        ])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|source| CheckpointError::Io {
            path: repo_root.into(),
            source,
        })?;
    if !refs.status.success() {
        return Err(CheckpointError::Io {
            path: repo_root.into(),
            source: std::io::Error::other(String::from_utf8_lossy(&refs.stderr).into_owned()),
        });
    }
    let active_refs = String::from_utf8_lossy(&refs.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();

    let worktrees = tokio::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|source| CheckpointError::Io {
            path: repo_root.into(),
            source,
        })?;
    if !worktrees.status.success() {
        return Err(CheckpointError::Io {
            path: repo_root.into(),
            source: std::io::Error::other(String::from_utf8_lossy(&worktrees.stderr).into_owned()),
        });
    }
    let worktree_prefix = format!("{}-", key.number);
    let mut active_worktrees = Vec::new();
    for record in String::from_utf8_lossy(&worktrees.stdout).split("\n\n") {
        let path = record
            .lines()
            .find_map(|line| line.strip_prefix("worktree "))
            .map(PathBuf::from);
        let branch = record.lines().find_map(|line| line.strip_prefix("branch "));
        let path_matches = path.as_ref().is_some_and(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&worktree_prefix))
        });
        let branch_matches = branch.is_some_and(|branch| branch.starts_with(&task_ref_prefix));
        match (path_matches, branch_matches, path) {
            (true, true, Some(path)) => active_worktrees.push(path),
            (false, false, _) => {}
            _ => {
                return Err(CheckpointError::Io {
                    path: repo_root.into(),
                    source: std::io::Error::other(
                        "ambiguous Makina recovery evidence: task worktree path and branch do not agree",
                    ),
                });
            }
        }
    }
    Ok((active_refs, active_worktrees))
}

/// Atomically persist a scheduler snapshot to the external, plan-qualified
/// checkpoint. Callers invoke this only from the lease-bound apply/run path.
pub async fn persist_checkpoint(
    repo_root: &Path,
    identity: CheckpointIdentity,
    graph: &TaskGraph,
) -> Result<(), CheckpointError> {
    persist_checkpoint_with_evidence(repo_root, identity, graph, vec![], vec![]).await
}

pub async fn persist_checkpoint_with_evidence(
    repo_root: &Path,
    identity: CheckpointIdentity,
    graph: &TaskGraph,
    active_refs: Vec<String>,
    active_worktrees: Vec<PathBuf>,
) -> Result<(), CheckpointError> {
    let key = PlanKey::parse(identity.plan_dir.clone()).map_err(|error| CheckpointError::Io {
        path: identity.plan_dir.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string()),
    })?;
    let path = checkpoint_path(repo_root, &key)?;
    let parent = path.parent().expect("checkpoint path has parent");
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|source| CheckpointError::Io {
            path: parent.into(),
            source,
        })?;
    let checkpoint = PlanCheckpoint {
        schema_version: 1,
        identity,
        tasks: graph
            .tasks
            .iter()
            .map(|task| RuntimeTaskCheckpoint {
                id: task.id.0.clone(),
                state: task.state,
                gate_iterations: task.gate_iterations,
                review_iterations: task.review_iterations,
            })
            .collect(),
        active_refs,
        active_worktrees,
    };
    let bytes = serde_json::to_vec_pretty(&checkpoint).map_err(|source| CheckpointError::Json {
        path: path.clone(),
        source,
    })?;
    let temporary = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut bytes_with_newline = bytes;
    bytes_with_newline.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await
        .map_err(|source| CheckpointError::Io {
            path: temporary.clone(),
            source,
        })?;
    use tokio::io::AsyncWriteExt;
    file.write_all(&bytes_with_newline)
        .await
        .map_err(|source| CheckpointError::Io {
            path: temporary.clone(),
            source,
        })?;
    file.sync_all()
        .await
        .map_err(|source| CheckpointError::Io {
            path: temporary.clone(),
            source,
        })?;
    drop(file);
    tokio::fs::rename(&temporary, &path)
        .await
        .map_err(|source| CheckpointError::Io {
            path: path.clone(),
            source,
        })?;
    let directory = tokio::fs::File::open(parent)
        .await
        .map_err(|source| CheckpointError::Io {
            path: parent.into(),
            source,
        })?;
    directory
        .sync_all()
        .await
        .map_err(|source| CheckpointError::Io {
            path: parent.into(),
            source,
        })
}

pub async fn archive_clean_checkpoint(
    repo_root: &Path,
    key: &PlanKey,
) -> Result<Option<PathBuf>, CheckpointError> {
    let path = checkpoint_path(repo_root, key)?;
    if !tokio::fs::try_exists(&path)
        .await
        .map_err(|source| CheckpointError::Io {
            path: path.clone(),
            source,
        })?
    {
        return Ok(None);
    }
    static ARCHIVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let archive = path.with_extension(format!(
        "archive.{}.{}",
        std::process::id(),
        ARCHIVE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    tokio::fs::rename(&path, &archive)
        .await
        .map_err(|source| CheckpointError::Io {
            path: path.clone(),
            source,
        })?;
    let parent = path.parent().expect("checkpoint path parent");
    let directory = tokio::fs::File::open(parent)
        .await
        .map_err(|source| CheckpointError::Io {
            path: parent.into(),
            source,
        })?;
    directory
        .sync_all()
        .await
        .map_err(|source| CheckpointError::Io {
            path: parent.into(),
            source,
        })?;
    Ok(Some(archive))
}

/// Start-time capability probe. Creates the plan-qualified directory, writes
/// and fsyncs a unique file, removes it, and syncs the directory before any
/// run status or agent mutation is allowed.
pub async fn probe_writable_checkpoint_root(
    repo_root: &Path,
    key: &PlanKey,
) -> Result<(), CheckpointError> {
    let path = checkpoint_path(repo_root, key)?;
    let parent = path.parent().expect("checkpoint path parent");
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|source| CheckpointError::Io {
            path: parent.into(),
            source,
        })?;
    let probe = parent.join(format!(
        ".probe.{}.{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
        .await
        .map_err(|source| CheckpointError::Io {
            path: probe.clone(),
            source,
        })?;
    file.sync_all()
        .await
        .map_err(|source| CheckpointError::Io {
            path: probe.clone(),
            source,
        })?;
    drop(file);
    tokio::fs::remove_file(&probe)
        .await
        .map_err(|source| CheckpointError::Io {
            path: probe,
            source,
        })?;
    let directory = tokio::fs::File::open(parent)
        .await
        .map_err(|source| CheckpointError::Io {
            path: parent.into(),
            source,
        })?;
    directory
        .sync_all()
        .await
        .map_err(|source| CheckpointError::Io {
            path: parent.into(),
            source,
        })
}

/// Overlay volatile scheduler fields only. `Done` is deliberately never
/// restored from JSON; Git/status reconciliation owns that proof.
pub fn overlay_compatible(graph: &mut TaskGraph, checkpoint: &PlanCheckpoint) {
    for task in &mut graph.tasks {
        let Some(saved) = checkpoint.tasks.iter().find(|saved| saved.id == task.id.0) else {
            continue;
        };
        task.gate_iterations = saved.gate_iterations;
        task.review_iterations = saved.review_iterations;
        let source_protected = graph.authored.get(&task.id).is_some_and(|authored| {
            authored.gated
                || authored.status != crate::plan::AuthoredTaskStatus::Planned
                || !matches!(
                    authored.seed,
                    crate::task::AuthoredSeedOutcome::Seeded(TaskState::New | TaskState::Ready)
                )
        });
        if source_protected {
            continue;
        }
        task.state = match saved.state {
            TaskState::Done => task.state,
            TaskState::InProgress | TaskState::InReview => TaskState::Ready,
            state => state,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_home_fails_without_repository_fallback() {
        let _guard = crate::HOME_ENV_LOCK.blocking_lock();
        let repo = tempfile::tempdir().unwrap();
        let old = std::env::var_os("HOME");
        unsafe { std::env::remove_var("HOME") };
        let result = external_state_root(repo.path());
        if let Some(value) = old {
            unsafe { std::env::set_var("HOME", value) }
        }
        assert!(matches!(result, Err(CheckpointError::StateUnavailable)));
    }

    #[test]
    fn home_symlink_into_repository_fails_containment() {
        let _guard = crate::HOME_ENV_LOCK.blocking_lock();
        let repo = tempfile::tempdir().unwrap();
        let link_parent = tempfile::tempdir().unwrap();
        let home_link = link_parent.path().join("home");
        #[cfg(unix)]
        std::os::unix::fs::symlink(repo.path(), &home_link).unwrap();
        let old = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home_link) };
        let result = external_state_root(repo.path());
        if let Some(value) = old {
            unsafe { std::env::set_var("HOME", value) }
        } else {
            unsafe { std::env::remove_var("HOME") }
        }
        assert!(matches!(
            result,
            Err(CheckpointError::StateInsideRepository(_))
        ));
    }

    #[test]
    fn checkpoint_path_is_external_and_plan_qualified() {
        let _guard = crate::HOME_ENV_LOCK.blocking_lock();
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let old = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        let key = PlanKey::parse("docs/plans/0048-Example").unwrap();
        let path = checkpoint_path(repo.path(), &key).unwrap();
        if let Some(value) = old {
            unsafe { std::env::set_var("HOME", value) }
        } else {
            unsafe { std::env::remove_var("HOME") }
        }
        assert!(path.starts_with(home.path()));
        assert!(!path.starts_with(repo.path()));
        assert_eq!(path.file_name().unwrap(), "checkpoint.json");
        assert_eq!(path.parent().unwrap().file_name().unwrap().len(), 64);
    }
}
