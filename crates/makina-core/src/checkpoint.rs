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

/// Resolve the state root for checkpoint storage. With the in-repo state
/// layout this is simply `repo_root/.makina/` — always available, no `$HOME`
/// dependency, no containment check needed.
pub fn external_state_root(repo_root: &Path) -> Result<PathBuf, CheckpointError> {
    crate::paths::state_root(repo_root).map_err(|source| CheckpointError::Io {
        path: repo_root.to_path_buf(),
        source,
    })
}

pub fn checkpoint_path(repo_root: &Path, key: &PlanKey) -> Result<PathBuf, CheckpointError> {
    let mut hash = Sha256::new();
    hash.update(key.relative_dir.to_string_lossy().as_bytes());
    let encoded = crate::json::hex_encode(&hash.finalize());
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
    // Build a lookup of saved states by task id so we can inspect dependency
    // states when deciding whether a Skipped task should be un-skipped.
    let saved_states: std::collections::HashMap<&str, TaskState> = checkpoint
        .tasks
        .iter()
        .map(|saved| (saved.id.as_str(), saved.state))
        .collect();

    tracing::info!(
        tasks = ?checkpoint.tasks.iter().map(|t| (&t.id, t.state)).collect::<Vec<_>>(),
        "overlay_compatible: applying checkpoint"
    );

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
            // A Skipped task is only meaningful within a single run: it means a
            // dependency failed. On resume, if no dependency is still Failed or
            // Skipped (in the checkpoint), the task should be un-skipped back
            // to New so the scheduler can run it once its dependencies complete.
            // Otherwise the task stays Skipped forever even after the blocking
            // task is reset to Ready — which is the "first task ready, other two
            // skipped" regression.
            TaskState::Skipped => {
                let any_dep_failed = task.depends_on.iter().any(|dep| {
                    matches!(
                        saved_states.get(dep.0.as_str()),
                        Some(TaskState::Failed | TaskState::Skipped)
                    )
                });
                if any_dep_failed {
                    TaskState::Skipped
                } else {
                    task.state
                }
            }
            state => state,
        };
    }

    // Second pass: un-skip transitive chains. A Skipped task whose dependency
    // was ALSO Skipped in the checkpoint (and is now being un-skipped by the
    // first pass) must itself be un-skipped. The first pass only checks the
    // checkpoint state of dependencies, but a dependency that was Skipped in
    // the checkpoint may have been un-skipped to New by the first pass (because
    // ITS dependencies were not failed). So we re-scan: any Skipped task whose
    // dependencies are all non-Failed/non-Skipped in the POST-overlay graph
    // state is un-skipped to New. We iterate to a fixed point to handle
    // arbitrarily deep chains.
    loop {
        // Collect the ids to un-skip this pass (avoid borrowing graph.tasks
        // mutably while also reading dependency states from the same graph).
        let to_unskip: Vec<usize> = graph
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.state == TaskState::Skipped)
            .filter(|(_, task)| {
                task.depends_on.iter().all(|dep| {
                    graph
                        .get(dep)
                        .is_none_or(|d| !matches!(d.state, TaskState::Failed | TaskState::Skipped))
                })
            })
            .map(|(idx, _)| idx)
            .collect();
        if to_unskip.is_empty() {
            break;
        }
        tracing::info!(
            count = to_unskip.len(),
            ids = ?to_unskip.iter().map(|&i| &graph.tasks[i].id).collect::<Vec<_>>(),
            "overlay_compatible: un-skipping stale dependents"
        );
        for idx in to_unskip {
            graph.tasks[idx].state = TaskState::New;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_root_is_in_repo_makina_dir() {
        let repo = tempfile::tempdir().unwrap();
        let result = external_state_root(repo.path()).unwrap();
        assert_eq!(
            result,
            repo.path().join(".makina"),
            "state root must be repo/.makina"
        );
    }

    #[test]
    fn checkpoint_path_is_in_repo_and_plan_qualified() {
        let repo = tempfile::tempdir().unwrap();
        let key = PlanKey::parse("docs/plans/0048-Example").unwrap();
        let path = checkpoint_path(repo.path(), &key).unwrap();
        // Path must be under repo/.makina/checkpoints/
        assert!(
            path.starts_with(repo.path().join(".makina").join("checkpoints")),
            "checkpoint path must be under repo/.makina/checkpoints, got {}",
            path.display()
        );
        assert_eq!(path.file_name().unwrap(), "checkpoint.json");
        assert_eq!(path.parent().unwrap().file_name().unwrap().len(), 64);
    }
}
