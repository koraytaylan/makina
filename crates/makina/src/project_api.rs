//! Project-aware multiplexing for the TUI's single [`Api`] handle.
//!
//! Each [`CoreApi`](makina_core::orchestrator::CoreApi) is intentionally rooted
//! in one Git repository: its worktree manager, config, persistence, audit, and
//! transcript paths all belong to that repository. [`ProjectApiRouter`] keeps
//! that invariant while letting the workspace UI open several repositories.
//! It resolves an `OpenRun` path to its Git root, lazily creates one API per
//! root, and translates each project's session-local [`RunId`] into a process-
//! wide ID before exposing it to the TUI.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt as _;
use makina_core::api::{
    Api, ApiError, Command, CommandOutcome, Event, EventStream, RunId, RunView,
};
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tokio_stream::wrappers::BroadcastStream;

const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Builds the repository-rooted API used for one workspace project.
pub type ProjectApiFactory = Arc<dyn Fn(&Path) -> Result<Arc<dyn Api>, ApiError> + Send + Sync>;

#[derive(Clone)]
struct RunRoute {
    project_root: PathBuf,
    local: RunId,
}

#[derive(Default)]
struct RouteTable {
    global_to_local: HashMap<RunId, RunRoute>,
    local_to_global: HashMap<(PathBuf, RunId), RunId>,
    uid_to_global: HashMap<(PathBuf, String), RunId>,
}

struct RouterEvents {
    routes: Mutex<RouteTable>,
    next_id: AtomicU64,
    event_tx: broadcast::Sender<Event>,
}

impl RouterEvents {
    fn allocate_locked(&self, routes: &mut RouteTable, project_root: &Path, local: RunId) -> RunId {
        let global = RunId(self.next_id.fetch_add(1, Ordering::Relaxed));
        routes
            .local_to_global
            .insert((project_root.to_path_buf(), local), global);
        routes.global_to_local.insert(
            global,
            RunRoute {
                project_root: project_root.to_path_buf(),
                local,
            },
        );
        global
    }

    fn global_for(&self, project_root: &Path, local: RunId) -> RunId {
        let mut routes = self.routes.lock().expect("run route mutex poisoned");
        let key = (project_root.to_path_buf(), local);
        if let Some(global) = routes.local_to_global.get(&key) {
            return *global;
        }
        self.allocate_locked(&mut routes, project_root, local)
    }

    /// Resolve a view to a process-global id, preferring its persistent UID.
    ///
    /// Core assigns session-only ids to historical disk snapshots. Those ids
    /// may change each time `runs()` is queried, while `run_uid` is stable.
    /// Binding both keys here keeps the TUI handle stable and refreshes the
    /// reverse route to the newest local id.
    fn global_for_view(&self, project_root: &Path, local: RunId, run_uid: &str) -> RunId {
        if run_uid.is_empty() {
            return self.global_for(project_root, local);
        }

        let mut routes = self.routes.lock().expect("run route mutex poisoned");
        let local_key = (project_root.to_path_buf(), local);
        let uid_key = (project_root.to_path_buf(), run_uid.to_string());

        let global = if let Some(global) = routes.uid_to_global.get(&uid_key) {
            *global
        } else if let Some(global) = routes.local_to_global.get(&local_key) {
            *global
        } else {
            self.allocate_locked(&mut routes, project_root, local)
        };

        routes.uid_to_global.insert(uid_key, global);
        routes.local_to_global.insert(local_key, global);
        routes.global_to_local.insert(
            global,
            RunRoute {
                project_root: project_root.to_path_buf(),
                local,
            },
        );
        global
    }

    fn route_for(&self, global: RunId) -> Option<RunRoute> {
        self.routes
            .lock()
            .expect("run route mutex poisoned")
            .global_to_local
            .get(&global)
            .cloned()
    }

    fn remap_event(&self, project_root: &Path, event: Event) -> Option<Event> {
        let map = |run| self.global_for(project_root, run);
        Some(match event {
            // Forward the inner API's single event instead of synthesizing a
            // second copy in `execute`. This preserves the refresh events that
            // ReinterpretRun and ResetRun intentionally emit as RunOpened.
            Event::RunOpened {
                run,
                task_list_path,
            } => {
                let task_list_path = match project_task_path(project_root, &task_list_path) {
                    Ok(path) => path,
                    Err(error) => {
                        tracing::warn!(
                            expected_project_root = %project_root.display(),
                            task_list_path = %task_list_path.display(),
                            %error,
                            "dropping run event whose task list belongs to another project"
                        );
                        return None;
                    }
                };
                Event::RunOpened {
                    run: map(run),
                    task_list_path,
                }
            }
            Event::RunStatusChanged { run, status } => Event::RunStatusChanged {
                run: map(run),
                status,
            },
            Event::TaskStateChanged { run, task, state } => Event::TaskStateChanged {
                run: map(run),
                task,
                state,
            },
            Event::TaskIterationsUpdated {
                run,
                task,
                gate_iterations,
                review_iterations,
            } => Event::TaskIterationsUpdated {
                run: map(run),
                task,
                gate_iterations,
                review_iterations,
            },
            Event::SessionCapabilities {
                run,
                task,
                role,
                capabilities,
            } => Event::SessionCapabilities {
                run: map(run),
                task,
                role,
                capabilities,
            },
            Event::CurrentModeUpdate {
                run,
                task,
                role,
                current_mode_id,
            } => Event::CurrentModeUpdate {
                run: map(run),
                task,
                role,
                current_mode_id,
            },
            Event::AgentExchange {
                run,
                task,
                role,
                event,
            } => Event::AgentExchange {
                run: map(run),
                task,
                role,
                event,
            },
            Event::TaskIdle {
                run,
                task,
                idle_secs,
            } => Event::TaskIdle {
                run: map(run),
                task,
                idle_secs,
            },
            Event::TaskRetried { run, task } => Event::TaskRetried {
                run: map(run),
                task,
            },
            Event::RoleTurnMetrics {
                run,
                task,
                role,
                model,
                duration_ms,
                usage,
            } => Event::RoleTurnMetrics {
                run: map(run),
                task,
                role,
                model,
                duration_ms,
                usage,
            },
            Event::RunIntegrationBranchLeft { run, branch } => Event::RunIntegrationBranchLeft {
                run: map(run),
                branch,
            },
            Event::PlanOperation {
                run,
                plan_slug,
                label,
                operation,
                phase,
                message,
            } => Event::PlanOperation {
                run: map(run),
                plan_slug,
                label,
                operation,
                phase,
                message,
            },
            Event::ProjectDiscovered {
                project_root: reported_root,
                gate_count,
                scanned_files,
            } => {
                let Ok(reported_root) = canonicalize_project_root(&reported_root) else {
                    tracing::warn!(
                        expected_project_root = %project_root.display(),
                        reported_project_root = %reported_root.display(),
                        "dropping project event with an invalid root"
                    );
                    return None;
                };
                if reported_root != project_root {
                    tracing::warn!(
                        expected_project_root = %project_root.display(),
                        reported_project_root = %reported_root.display(),
                        "dropping project event emitted for another repository"
                    );
                    return None;
                }
                Event::ProjectDiscovered {
                    project_root: reported_root,
                    gate_count,
                    scanned_files,
                }
            }
        })
    }
}

/// One TUI-facing API backed by one repository-rooted API per project.
pub struct ProjectApiRouter {
    allowed_projects: Mutex<HashSet<PathBuf>>,
    factory: ProjectApiFactory,
    projects: AsyncMutex<HashMap<PathBuf, Arc<dyn Api>>>,
    events: Arc<RouterEvents>,
}

impl ProjectApiRouter {
    /// Create a router with an explicit initial workspace allowlist. Project
    /// APIs are built lazily or via [`register_project`](Self::register_project).
    pub fn new(
        initial_projects: impl IntoIterator<Item = PathBuf>,
        factory: ProjectApiFactory,
    ) -> Self {
        let allowed_projects = initial_projects
            .into_iter()
            .filter_map(|root| canonicalize_project_root(&root).ok())
            .collect();
        let (event_tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            allowed_projects: Mutex::new(allowed_projects),
            factory,
            projects: AsyncMutex::new(HashMap::new()),
            events: Arc::new(RouterEvents {
                routes: Mutex::new(RouteTable::default()),
                next_id: AtomicU64::new(1),
                event_tx,
            }),
        }
    }

    /// Ensure a workspace folder has a repository-rooted API and contributes
    /// its disk snapshots to [`Api::runs`].
    pub async fn register_project(&self, root: impl AsRef<Path>) -> Result<(), ApiError> {
        let root = canonicalize_project_root(root.as_ref()).map_err(invalid_project)?;
        // Do not authorize a dynamic folder until its repository-scoped API is
        // fully initialized. A config/factory failure must leave it denied.
        self.project_api(&root).await?;
        self.allowed_projects
            .lock()
            .expect("allowed projects mutex poisoned")
            .insert(root);
        Ok(())
    }

    /// Remove a folder from the new-command allowlist without invalidating
    /// routes for runs that were already opened there.
    pub fn unregister_project(&self, root: impl AsRef<Path>) {
        let requested = root.as_ref();
        let root = canonicalize_project_root(requested)
            .or_else(|_| {
                std::fs::canonicalize(requested).map_err(|error| {
                    format!(
                        "cannot resolve project path {}: {error}",
                        requested.display()
                    )
                })
            })
            .unwrap_or_else(|_| {
                if requested.is_absolute() {
                    requested.to_path_buf()
                } else {
                    std::env::current_dir().unwrap_or_default().join(requested)
                }
            });
        self.allowed_projects
            .lock()
            .expect("allowed projects mutex poisoned")
            .remove(&root);
    }

    fn require_allowed_project(&self, root: &Path) -> Result<PathBuf, ApiError> {
        let root = canonicalize_project_root(root).map_err(invalid_project)?;
        if self
            .allowed_projects
            .lock()
            .expect("allowed projects mutex poisoned")
            .contains(&root)
        {
            Ok(root)
        } else {
            Err(ApiError::InvalidCommand {
                reason: format!(
                    "project {} is not registered in this workspace",
                    root.display()
                ),
            })
        }
    }

    async fn project_api(&self, root: &Path) -> Result<Arc<dyn Api>, ApiError> {
        // Keep initialization private until its event receiver exists. The
        // async mutex also makes concurrent first-use calls share exactly one
        // factory result.
        let mut projects = self.projects.lock().await;
        if let Some(api) = projects.get(root) {
            return Ok(Arc::clone(api));
        }
        let api = (self.factory)(root)?;
        let mut stream = api.subscribe();
        let events = Arc::clone(&self.events);
        let project_root = root.to_path_buf();
        tokio::spawn(async move {
            while let Some(event) = stream.next().await {
                if let Some(event) = events.remap_event(&project_root, event) {
                    let _ = events.event_tx.send(event);
                }
            }
        });
        projects.insert(root.to_path_buf(), Arc::clone(&api));
        Ok(api)
    }

    async fn execute_for_run(
        &self,
        global: RunId,
        command: impl FnOnce(RunId) -> Command,
    ) -> Result<CommandOutcome, ApiError> {
        let Some(route) = self.events.route_for(global) else {
            return Err(ApiError::UnknownRun { run: global });
        };
        let api = self.project_api(&route.project_root).await?;
        match api.execute(command(route.local)).await {
            Err(ApiError::UnknownRun { .. }) => Err(ApiError::UnknownRun { run: global }),
            other => other,
        }
    }

    fn qualify_view(&self, project_root: &Path, mut view: RunView) -> Option<RunView> {
        view.task_list_path = match project_task_path(project_root, &view.task_list_path) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(
                    expected_project_root = %project_root.display(),
                    task_list_path = %view.task_list_path.display(),
                    run_uid = %view.run_uid,
                    %error,
                    "dropping run view whose task list belongs to another project"
                );
                return None;
            }
        };
        view.id = self
            .events
            .global_for_view(project_root, view.id, &view.run_uid);
        Some(view)
    }
}

#[async_trait]
impl Api for ProjectApiRouter {
    async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
        match command {
            Command::OpenRun { task_list_path } => {
                let (project_root, task_list_path) = project_for_task_path(&task_list_path)
                    .map_err(|reason| ApiError::InvalidCommand { reason })?;
                self.require_allowed_project(&project_root)?;
                let api = self.project_api(&project_root).await?;
                let outcome = api
                    .execute(Command::OpenRun {
                        task_list_path: task_list_path.clone(),
                    })
                    .await?;
                let CommandOutcome::RunOpened { run: local } = outcome else {
                    return Err(ApiError::Internal {
                        reason: "project API acknowledged OpenRun without returning a run"
                            .to_string(),
                    });
                };
                let run = self.events.global_for(&project_root, local);
                Ok(CommandOutcome::RunOpened { run })
            }
            Command::StartRun { run } => {
                self.execute_for_run(run, |run| Command::StartRun { run })
                    .await
            }
            Command::PauseRun { run } => {
                self.execute_for_run(run, |run| Command::PauseRun { run })
                    .await
            }
            Command::CancelRun { run } => {
                self.execute_for_run(run, |run| Command::CancelRun { run })
                    .await
            }
            Command::ReinterpretRun { run } => {
                self.execute_for_run(run, |run| Command::ReinterpretRun { run })
                    .await
            }
            Command::RetryTask { run, task } => {
                self.execute_for_run(run, |run| Command::RetryTask { run, task })
                    .await
            }
            Command::RetryFailedTasks { run } => {
                self.execute_for_run(run, |run| Command::RetryFailedTasks { run })
                    .await
            }
            Command::ResetRun { run } => {
                self.execute_for_run(run, |run| Command::ResetRun { run })
                    .await
            }
            Command::RegisterProject { project_root } => {
                self.register_project(project_root).await?;
                Ok(CommandOutcome::Acknowledged)
            }
            Command::UnregisterProject { project_root } => {
                self.unregister_project(project_root);
                Ok(CommandOutcome::Acknowledged)
            }
            Command::UpdateRuntimeSettings {
                project_root,
                caps,
                concurrency,
                final_merge,
            } => {
                let root = self.require_allowed_project(&project_root)?;
                let settings = Command::UpdateRuntimeSettings {
                    project_root: root.clone(),
                    caps,
                    concurrency,
                    final_merge,
                };
                self.project_api(&root).await?.execute(settings).await
            }
            Command::DiscoverProject { project_root } => {
                let root = self.require_allowed_project(&project_root)?;
                self.project_api(&root)
                    .await?
                    .execute(Command::DiscoverProject { project_root: root })
                    .await
            }
            Command::PurgeWorktrees { project_root } => {
                let root = self.require_allowed_project(&project_root)?;
                self.project_api(&root)
                    .await?
                    .execute(Command::PurgeWorktrees { project_root: root })
                    .await
            }
        }
    }

    async fn runs(&self) -> Vec<RunView> {
        // Closed projects keep their APIs and run routes so in-flight lifecycle
        // commands can finish safely, but they must not reappear in a fresh
        // workspace snapshot after UnregisterProject.
        let allowed_projects = self
            .allowed_projects
            .lock()
            .expect("allowed projects mutex poisoned")
            .clone();
        let projects: Vec<_> = self
            .projects
            .lock()
            .await
            .iter()
            .filter(|(root, _)| allowed_projects.contains(*root))
            .map(|(root, api)| (root.clone(), Arc::clone(api)))
            .collect();
        let mut views = Vec::new();
        for (root, api) in projects {
            views.extend(
                api.runs()
                    .await
                    .into_iter()
                    .filter_map(|view| self.qualify_view(&root, view)),
            );
        }
        views.sort_by(|a, b| a.run_uid.cmp(&b.run_uid));
        views
    }

    async fn run(&self, id: RunId) -> Option<RunView> {
        let route = self.events.route_for(id)?;
        let api = self.project_api(&route.project_root).await.ok()?;
        api.run(route.local)
            .await
            .and_then(|view| self.qualify_view(&route.project_root, view))
    }

    fn subscribe(&self) -> EventStream {
        let stream = BroadcastStream::new(self.events.event_tx.subscribe())
            .filter_map(|item| async move { item.ok() });
        Box::pin(stream)
    }
}

fn invalid_project(error: String) -> ApiError {
    ApiError::InvalidCommand { reason: error }
}

/// Resolve a task-list identity reported by a project API without allowing it
/// to escape that API's canonical repository root.
///
/// Disk snapshots can legitimately point at a file that no longer exists, so
/// this walks components instead of requiring the complete path to canonicalize.
/// Any existing symlink component is still resolved and checked before the
/// remaining (possibly missing) suffix is appended.
fn project_task_path(project_root: &Path, task_path: &Path) -> Result<PathBuf, String> {
    let relative = if task_path.is_absolute() {
        task_path.strip_prefix(project_root).map_err(|_| {
            format!(
                "task list {} is outside project {}",
                task_path.display(),
                project_root.display()
            )
        })?
    } else {
        task_path
    };

    let mut resolved = project_root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                resolved.push(name);
                match std::fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        resolved = std::fs::canonicalize(&resolved).map_err(|error| {
                            format!(
                                "cannot resolve task-list path {}: {error}",
                                resolved.display()
                            )
                        })?;
                        if !resolved.starts_with(project_root) {
                            return Err(format!(
                                "task list {} escapes project {} through a symlink",
                                task_path.display(),
                                project_root.display()
                            ));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "cannot inspect task-list path {}: {error}",
                            resolved.display()
                        ));
                    }
                }
            }
            Component::ParentDir => {
                if resolved == project_root || !resolved.pop() {
                    return Err(format!(
                        "task list {} escapes project {}",
                        task_path.display(),
                        project_root.display()
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "task list {} has an invalid project-relative path",
                    task_path.display()
                ));
            }
        }
    }
    if !resolved.starts_with(project_root) {
        return Err(format!(
            "task list {} escapes project {}",
            task_path.display(),
            project_root.display()
        ));
    }
    Ok(resolved)
}

/// Resolve any existing path inside a worktree to Git's authoritative top-level.
///
/// This deliberately asks Git instead of walking ancestors for a `.git`
/// marker. It therefore handles linked worktrees (`.git` is a file), rejects
/// fake markers, and normalizes subdirectories and symlink aliases to one key.
pub fn canonicalize_project_root(path: &Path) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve project path {}: {error}", path.display()))?;
    let probe_dir = if canonical.is_dir() {
        canonical.as_path()
    } else {
        canonical
            .parent()
            .ok_or_else(|| format!("project path has no parent: {}", canonical.display()))?
    };

    let output = ProcessCommand::new("git")
        .args(["-C"])
        .arg(probe_dir)
        .args(["rev-parse", "--show-toplevel"])
        // Ambient repository overrides must not redirect a workspace path to
        // an unrelated checkout.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(|error| format!("failed to run git for {}: {error}", path.display()))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{} is not inside a Git worktree{}",
            path.display(),
            if detail.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", detail.trim())
            }
        ));
    }
    let output_root = std::str::from_utf8(&output.stdout)
        .map_err(|_| format!("Git root for {} is not valid UTF-8", path.display()))?
        .trim_end_matches(['\r', '\n']);
    if output_root.is_empty() {
        return Err(format!(
            "git returned an empty worktree root for {}",
            path.display()
        ));
    }
    std::fs::canonicalize(output_root).map_err(|error| {
        format!(
            "cannot canonicalize Git root {} for {}: {error}",
            output_root,
            path.display()
        )
    })
}

/// Canonicalize, sort, and de-duplicate persisted workspace roots.
///
/// The rejected entries are returned with diagnostics so startup can warn
/// without giving invalid paths to either the router or the application model.
pub fn normalize_workspace_roots(
    roots: impl IntoIterator<Item = PathBuf>,
) -> (Vec<PathBuf>, Vec<(PathBuf, String)>) {
    let mut normalized = HashSet::new();
    let mut rejected = Vec::new();
    for root in roots {
        match canonicalize_project_root(&root) {
            Ok(root) => {
                normalized.insert(root);
            }
            Err(error) => rejected.push((root, error)),
        }
    }
    let mut normalized: Vec<_> = normalized.into_iter().collect();
    normalized.sort();
    (normalized, rejected)
}

/// Resolve an input task path to `(git root, canonical task path)`.
///
/// A missing final `TASKS.md` is accepted when its parent exists so Core can
/// execute the documented TASKS-less generation flow. Existing symlinks are
/// canonicalized before the Git root is selected, preventing a path that looks
/// project-local from routing execution into a different repository.
pub fn project_for_task_path(task_path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let absolute = if task_path.is_absolute() {
        task_path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("cannot resolve current directory: {error}"))?
            .join(task_path)
    };

    let canonical_task = match std::fs::symlink_metadata(&absolute) {
        Ok(_) => std::fs::canonicalize(&absolute).map_err(|error| {
            // In particular, reject a dangling final symlink instead of
            // treating it as the documented missing-TASKS.md case.
            format!("cannot resolve task list {}: {error}", absolute.display())
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = absolute
                .parent()
                .ok_or_else(|| format!("task-list path has no parent: {}", absolute.display()))?;
            let parent = std::fs::canonicalize(parent).map_err(|error| {
                format!(
                    "cannot resolve task-list parent {}: {error}",
                    parent.display()
                )
            })?;
            let name = absolute
                .file_name()
                .ok_or_else(|| format!("task-list path has no filename: {}", absolute.display()))?;
            parent.join(name)
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect task list {}: {error}",
                absolute.display()
            ));
        }
    };

    let search_from = if canonical_task.is_dir() {
        canonical_task.as_path()
    } else {
        canonical_task
            .parent()
            .ok_or_else(|| format!("task-list path has no parent: {}", canonical_task.display()))?
    };
    let project_root = canonicalize_project_root(search_from).map_err(|error| {
        format!(
            "task list {} is not inside a Git repository: {error}",
            canonical_task.display()
        )
    })?;
    if !canonical_task.starts_with(&project_root) {
        return Err(format!(
            "task list {} escapes project {}",
            canonical_task.display(),
            project_root.display()
        ));
    }
    Ok((project_root, canonical_task))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;

    use makina_core::api::{IngestionReport, PlanOperationKind, PlanOperationPhase, RunStatus};

    struct FakeApi {
        next_id: AtomicU64,
        runs: Mutex<Vec<RunView>>,
        commands: Mutex<Vec<Command>>,
        events: broadcast::Sender<Event>,
    }

    impl FakeApi {
        fn new() -> Self {
            let (events, _rx) = broadcast::channel(32);
            Self {
                next_id: AtomicU64::new(1),
                runs: Mutex::new(Vec::new()),
                commands: Mutex::new(Vec::new()),
                events,
            }
        }
    }

    #[async_trait]
    impl Api for FakeApi {
        async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
            self.commands
                .lock()
                .expect("command mutex poisoned")
                .push(command.clone());
            match command {
                Command::OpenRun { task_list_path } => {
                    let id = {
                        let mut runs = self.runs.lock().expect("runs mutex poisoned");
                        if let Some(run) =
                            runs.iter().find(|run| run.task_list_path == task_list_path)
                        {
                            run.id
                        } else {
                            let id = RunId(self.next_id.fetch_add(1, Ordering::Relaxed));
                            let project = canonicalize_project_root(&task_list_path)
                                .ok()
                                .and_then(|root| root.file_name().map(|name| name.to_owned()))
                                .and_then(|name| name.to_str().map(str::to_owned))
                                .unwrap_or_default();
                            runs.push(RunView {
                                id,
                                run_uid: format!("fake-{}", id.0),
                                project,
                                task_list_path: task_list_path.clone(),
                                status: RunStatus::Pending,
                                tasks: Vec::new(),
                                report: IngestionReport::default(),
                            });
                            id
                        }
                    };
                    let _ = self.events.send(Event::RunOpened {
                        run: id,
                        task_list_path,
                    });
                    Ok(CommandOutcome::RunOpened { run: id })
                }
                Command::ReinterpretRun { run } | Command::ResetRun { run } => {
                    let task_list_path = self
                        .runs
                        .lock()
                        .expect("runs mutex poisoned")
                        .iter()
                        .find(|view| view.id == run)
                        .map(|view| view.task_list_path.clone())
                        .ok_or(ApiError::UnknownRun { run })?;
                    let _ = self.events.send(Event::RunOpened {
                        run,
                        task_list_path,
                    });
                    Ok(CommandOutcome::Acknowledged)
                }
                _ => Ok(CommandOutcome::Acknowledged),
            }
        }

        async fn runs(&self) -> Vec<RunView> {
            self.runs.lock().expect("runs mutex poisoned").clone()
        }

        async fn run(&self, id: RunId) -> Option<RunView> {
            self.runs
                .lock()
                .expect("runs mutex poisoned")
                .iter()
                .find(|run| run.id == id)
                .cloned()
        }

        fn subscribe(&self) -> EventStream {
            let stream = BroadcastStream::new(self.events.subscribe())
                .filter_map(|item| async move { item.ok() });
            Box::pin(stream)
        }
    }

    fn project(name: &str) -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .expect("create project");
        let output = ProcessCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .output()
            .expect("run git init");
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::create_dir_all(dir.path().join("docs/plans/same-plan"))
            .expect("create plan directory");
        std::fs::write(
            dir.path().join("docs/plans/same-plan/TASKS.md"),
            "# tasks\n",
        )
        .expect("write tasks");
        dir
    }

    #[tokio::test]
    async fn same_local_run_ids_are_namespaced_and_commands_route_to_the_owner() {
        let first = project("router-first");
        let second = project("router-second");
        let built: Arc<Mutex<HashMap<PathBuf, Arc<FakeApi>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let factory_state = Arc::clone(&built);
        let factory: ProjectApiFactory = Arc::new(move |root| {
            let fake = Arc::new(FakeApi::new());
            factory_state
                .lock()
                .expect("factory mutex poisoned")
                .insert(root.to_path_buf(), Arc::clone(&fake));
            Ok(fake)
        });
        let router = ProjectApiRouter::new([first.path().to_path_buf()], factory);
        router
            .register_project(second.path())
            .await
            .expect("register second project");

        let open = |path: PathBuf| Command::OpenRun {
            task_list_path: path,
        };
        let CommandOutcome::RunOpened { run: first_run } = router
            .execute(open(first.path().join("docs/plans/same-plan/TASKS.md")))
            .await
            .expect("open first")
        else {
            panic!("expected first RunOpened")
        };
        let CommandOutcome::RunOpened { run: second_run } = router
            .execute(open(second.path().join("docs/plans/same-plan/TASKS.md")))
            .await
            .expect("open second")
        else {
            panic!("expected second RunOpened")
        };

        assert_ne!(first_run, second_run, "local run:1 values must not alias");
        router
            .execute(Command::StartRun { run: second_run })
            .await
            .expect("start second");

        let first_root = std::fs::canonicalize(first.path()).expect("canonical first");
        let second_root = std::fs::canonicalize(second.path()).expect("canonical second");
        let (first_fake, second_fake) = {
            let fakes = built.lock().expect("factory mutex poisoned");
            (
                Arc::clone(&fakes[&first_root]),
                Arc::clone(&fakes[&second_root]),
            )
        };
        {
            let second_commands = second_fake.commands.lock().expect("command mutex poisoned");
            assert!(matches!(
                second_commands.last(),
                Some(Command::StartRun { run: RunId(1) })
            ));
        }
        {
            let first_commands = first_fake.commands.lock().expect("command mutex poisoned");
            assert_eq!(
                first_commands.len(),
                1,
                "first project must not receive Start"
            );
        }

        let mut events = router.subscribe();
        let _ = second_fake.events.send(Event::PlanOperation {
            run: RunId(1),
            plan_slug: "same-plan".to_string(),
            label: "Same plan".to_string(),
            operation: PlanOperationKind::Reset,
            phase: PlanOperationPhase::Started,
            message: "Resetting".to_string(),
        });
        let operation = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.next().await.expect("event stream ended");
                if matches!(event, Event::PlanOperation { .. }) {
                    break event;
                }
            }
        })
        .await
        .expect("plan operation timed out");
        assert!(
            matches!(
                &operation,
                Event::PlanOperation { run, .. } if *run == second_run
            ),
            "unexpected remapped operation: {operation:?}; expected run {second_run}"
        );

        // A project-level event is forwarded only when its claimed root agrees
        // with the API that emitted it.
        let _ = second_fake.events.send(Event::ProjectDiscovered {
            project_root: first_root,
            gate_count: 1,
            scanned_files: 2,
        });
        let _ = second_fake.events.send(Event::ProjectDiscovered {
            project_root: second_root.clone(),
            gate_count: 1,
            scanned_files: 2,
        });
        let discovered = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.next().await.expect("event stream ended");
                if matches!(event, Event::ProjectDiscovered { .. }) {
                    break event;
                }
            }
        })
        .await
        .expect("project event timed out");
        assert!(matches!(
            discovered,
            Event::ProjectDiscovered { project_root, .. } if project_root == second_root
        ));
    }

    #[tokio::test]
    async fn run_opened_is_forwarded_once_and_refresh_events_are_preserved() {
        let repo = project("router-events");
        let factory: ProjectApiFactory = Arc::new(|_| Ok(Arc::new(FakeApi::new())));
        let router = ProjectApiRouter::new([repo.path().to_path_buf()], factory);
        let mut events = router.subscribe();

        let CommandOutcome::RunOpened { run } = router
            .execute(Command::OpenRun {
                task_list_path: repo.path().join("docs/plans/same-plan/TASKS.md"),
            })
            .await
            .expect("open run")
        else {
            panic!("expected RunOpened outcome")
        };
        let opened = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .expect("open event timed out")
            .expect("open event stream ended");
        assert!(matches!(opened, Event::RunOpened { run: event_run, .. } if event_run == run));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), events.next())
                .await
                .is_err(),
            "OpenRun must not be duplicated by the router"
        );

        router
            .execute(Command::ReinterpretRun { run })
            .await
            .expect("reinterpret");
        let refresh = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .expect("reinterpret refresh timed out")
            .expect("event stream ended");
        assert!(matches!(refresh, Event::RunOpened { run: event_run, .. } if event_run == run));

        router
            .execute(Command::ResetRun { run })
            .await
            .expect("reset");
        let refresh = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .expect("reset refresh timed out")
            .expect("event stream ended");
        assert!(matches!(refresh, Event::RunOpened { run: event_run, .. } if event_run == run));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn project_is_not_published_until_its_event_receiver_exists() {
        struct BlockingSubscribeApi {
            subscribe_entered: Arc<AtomicBool>,
            release_subscribe: Arc<AtomicBool>,
            events: broadcast::Sender<Event>,
        }

        #[async_trait]
        impl Api for BlockingSubscribeApi {
            async fn execute(&self, command: Command) -> Result<CommandOutcome, ApiError> {
                if let Command::OpenRun { task_list_path } = command {
                    let run = RunId(1);
                    let _ = self.events.send(Event::RunOpened {
                        run,
                        task_list_path,
                    });
                    Ok(CommandOutcome::RunOpened { run })
                } else {
                    Ok(CommandOutcome::Acknowledged)
                }
            }

            async fn runs(&self) -> Vec<RunView> {
                Vec::new()
            }

            async fn run(&self, _id: RunId) -> Option<RunView> {
                None
            }

            fn subscribe(&self) -> EventStream {
                self.subscribe_entered.store(true, Ordering::Release);
                while !self.release_subscribe.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                let stream = BroadcastStream::new(self.events.subscribe())
                    .filter_map(|item| async move { item.ok() });
                Box::pin(stream)
            }
        }

        let repo = project("router-init-publication");
        let subscribe_entered = Arc::new(AtomicBool::new(false));
        let release_subscribe = Arc::new(AtomicBool::new(false));
        let entered_for_factory = Arc::clone(&subscribe_entered);
        let release_for_factory = Arc::clone(&release_subscribe);
        let factory: ProjectApiFactory = Arc::new(move |_| {
            let (events, _rx) = broadcast::channel(8);
            Ok(Arc::new(BlockingSubscribeApi {
                subscribe_entered: Arc::clone(&entered_for_factory),
                release_subscribe: Arc::clone(&release_for_factory),
                events,
            }))
        });
        let router = Arc::new(ProjectApiRouter::new([repo.path().to_path_buf()], factory));
        let mut events = router.subscribe();

        let register_router = Arc::clone(&router);
        let repo_root = repo.path().to_path_buf();
        let register =
            tokio::spawn(async move { register_router.register_project(repo_root).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !subscribe_entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("subscribe was never entered");

        let open_router = Arc::clone(&router);
        let task_path = repo.path().join("docs/plans/same-plan/TASKS.md");
        let open = tokio::spawn(async move {
            open_router
                .execute(Command::OpenRun {
                    task_list_path: task_path,
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let opened_before_subscription = open.is_finished();
        release_subscribe.store(true, Ordering::Release);

        register
            .await
            .expect("register task panicked")
            .expect("register failed");
        let outcome = open
            .await
            .expect("open task panicked")
            .expect("open failed");
        assert!(matches!(outcome, CommandOutcome::RunOpened { .. }));
        assert!(
            !opened_before_subscription,
            "another caller observed the project before subscribe completed"
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.next())
                .await
                .expect("forwarded event timed out"),
            Some(Event::RunOpened { .. })
        ));
    }

    #[tokio::test]
    async fn persistent_run_uid_keeps_global_id_stable_when_local_id_changes() {
        let repo = project("router-stable-uid");
        let built: Arc<Mutex<Option<Arc<FakeApi>>>> = Arc::new(Mutex::new(None));
        let built_for_factory = Arc::clone(&built);
        let factory: ProjectApiFactory = Arc::new(move |_| {
            let fake = Arc::new(FakeApi::new());
            *built_for_factory.lock().expect("factory mutex poisoned") = Some(Arc::clone(&fake));
            Ok(fake)
        });
        let router = ProjectApiRouter::new([repo.path().to_path_buf()], factory);
        router
            .register_project(repo.path())
            .await
            .expect("register");
        let fake = built
            .lock()
            .expect("factory mutex poisoned")
            .clone()
            .expect("fake built");
        let task_list_path =
            std::fs::canonicalize(repo.path().join("docs/plans/same-plan/TASKS.md"))
                .expect("canonical task list");
        fake.runs
            .lock()
            .expect("runs mutex poisoned")
            .push(RunView {
                id: RunId(40),
                run_uid: "01STABLE-ROUTER-UID".to_string(),
                project: "stable".to_string(),
                task_list_path,
                status: RunStatus::Completed,
                tasks: Vec::new(),
                report: IngestionReport::default(),
            });

        let first = router.runs().await;
        let first_id = first[0].id;
        fake.runs.lock().expect("runs mutex poisoned")[0].id = RunId(99);
        let second = router.runs().await;
        assert_eq!(second[0].id, first_id);
        assert_eq!(
            router.run(first_id).await.expect("stable route").id,
            first_id,
            "reverse route must refresh to the latest local handle"
        );
    }

    #[tokio::test]
    async fn unregistered_project_is_rejected_then_register_command_enables_it() {
        let first = project("router-allowed-first");
        let second = project("router-allowed-second");
        let factory: ProjectApiFactory = Arc::new(|_| Ok(Arc::new(FakeApi::new())));
        let router = ProjectApiRouter::new([first.path().to_path_buf()], factory);
        let second_tasks = second.path().join("docs/plans/same-plan/TASKS.md");

        let error = router
            .execute(Command::OpenRun {
                task_list_path: second_tasks.clone(),
            })
            .await
            .expect_err("unregistered project must be rejected");
        assert!(
            matches!(error, ApiError::InvalidCommand { reason } if reason.contains("not registered"))
        );

        let outcome = router
            .execute(Command::RegisterProject {
                project_root: second.path().to_path_buf(),
            })
            .await
            .expect("register project command");
        assert!(matches!(outcome, CommandOutcome::Acknowledged));
        let CommandOutcome::RunOpened { run } = router
            .execute(Command::OpenRun {
                task_list_path: second_tasks.clone(),
            })
            .await
            .expect("open registered project")
        else {
            panic!("expected RunOpened")
        };

        router
            .execute(Command::UnregisterProject {
                project_root: second.path().to_path_buf(),
            })
            .await
            .expect("unregister project");
        assert!(
            router.runs().await.is_empty(),
            "closed projects must not reappear in fresh run snapshots"
        );
        let error = router
            .execute(Command::OpenRun {
                task_list_path: second_tasks.clone(),
            })
            .await
            .expect_err("closed project must reject new opens");
        assert!(
            matches!(error, ApiError::InvalidCommand { reason } if reason.contains("not registered"))
        );
        router
            .execute(Command::StartRun { run })
            .await
            .expect("existing run route must survive close");
        assert!(
            router.run(run).await.is_some(),
            "an existing route must remain queryable while its run winds down"
        );

        router
            .execute(Command::RegisterProject {
                project_root: second.path().to_path_buf(),
            })
            .await
            .expect("re-register project");
        assert!(matches!(
            router
                .execute(Command::OpenRun {
                    task_list_path: second_tasks,
                })
                .await,
            Ok(CommandOutcome::RunOpened { .. })
        ));
        assert_eq!(router.runs().await.len(), 1);
    }

    #[tokio::test]
    async fn project_api_cannot_report_a_run_view_or_event_for_another_repo() {
        let first = project("router-view-root-first");
        let second = project("router-view-root-second");
        let built: Arc<Mutex<Option<Arc<FakeApi>>>> = Arc::new(Mutex::new(None));
        let built_for_factory = Arc::clone(&built);
        let factory: ProjectApiFactory = Arc::new(move |_| {
            let fake = Arc::new(FakeApi::new());
            *built_for_factory.lock().expect("factory mutex poisoned") = Some(Arc::clone(&fake));
            Ok(fake)
        });
        let router = ProjectApiRouter::new([first.path().to_path_buf()], factory);
        router
            .register_project(first.path())
            .await
            .expect("register first project");
        let fake = built
            .lock()
            .expect("factory mutex poisoned")
            .clone()
            .expect("fake built");
        let wrong_path = std::fs::canonicalize(second.path().join("docs/plans/same-plan/TASKS.md"))
            .expect("canonical second task list");
        fake.runs
            .lock()
            .expect("runs mutex poisoned")
            .push(RunView {
                id: RunId(7),
                run_uid: "01WRONGPROJECTVIEW00000001".to_string(),
                project: "wrong".to_string(),
                task_list_path: wrong_path.clone(),
                status: RunStatus::Completed,
                tasks: Vec::new(),
                report: IngestionReport::default(),
            });

        assert!(
            router.runs().await.is_empty(),
            "a disk view cannot escape the API's registered project identity"
        );

        let mut events = router.subscribe();
        let _ = fake.events.send(Event::RunOpened {
            run: RunId(7),
            task_list_path: wrong_path,
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.next())
                .await
                .is_err(),
            "a RunOpened event cannot claim another repository's task list"
        );
    }

    #[tokio::test]
    async fn real_core_apis_persist_identical_plan_slugs_only_in_their_own_repos() {
        use makina_core::backend::AgentBackend;
        use makina_core::backend::noop::NoopBackend;
        use makina_core::config::{Config, GlobalConfig, ProjectConfig};
        use makina_core::dependency::EdgeInferrer;
        use makina_core::interpreter::StructuredTextInterpreter;
        use makina_core::orchestrator::{CoreApi, run_slug};
        use makina_core::test_support::setup_temp_repo;
        use makina_core::worktree::WorktreeManager;

        const TASKS: &str = r#"# Same Plan — Task List

A project-local routing regression fixture.

---

## 0001 — Foundation

### local-task — Implement the local task
Make a project-local change.
- **Depends on:** —
- **Done when:** The local task is complete and tested.
"#;

        fn write_plan(repo: &Path) -> PathBuf {
            let plan = repo.join("docs/plans/same-plan");
            std::fs::create_dir_all(&plan).expect("create plan directory");
            let tasks = plan.join("TASKS.md");
            std::fs::write(&tasks, TASKS).expect("write task list");
            tasks
        }

        let first = setup_temp_repo();
        let second = setup_temp_repo();
        let first_tasks = write_plan(first.path());
        let second_tasks = write_plan(second.path());
        let factory: ProjectApiFactory = Arc::new(|root| {
            let interpreter = Arc::new(EdgeInferrer::new(Arc::new(
                StructuredTextInterpreter::new(),
            )));
            let backend: Arc<dyn AgentBackend> = Arc::new(NoopBackend::new());
            let worktrees = WorktreeManager::new(root.to_path_buf(), "develop".to_string());
            let config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
            Ok(Arc::new(CoreApi::new(
                interpreter,
                backend,
                worktrees,
                config,
            )))
        });
        let router = ProjectApiRouter::new([first.path().to_path_buf()], factory);
        router
            .register_project(first.path())
            .await
            .expect("register first");
        router
            .register_project(second.path())
            .await
            .expect("register second");

        let CommandOutcome::RunOpened { run: first_run } = router
            .execute(Command::OpenRun {
                task_list_path: first_tasks.clone(),
            })
            .await
            .expect("open first")
        else {
            panic!("expected first RunOpened")
        };
        let artifact_name = format!("{}.json", run_slug(&first_tasks));
        let first_artifact = first.path().join(".makina/tasks").join(&artifact_name);
        let second_artifact = second.path().join(".makina/tasks").join(&artifact_name);
        assert!(
            first_artifact.exists(),
            "first CoreApi must persist in repo one"
        );
        assert!(
            !second_artifact.exists(),
            "opening repo one must not write repo two"
        );

        let CommandOutcome::RunOpened { run: second_run } = router
            .execute(Command::OpenRun {
                task_list_path: second_tasks.clone(),
            })
            .await
            .expect("open second")
        else {
            panic!("expected second RunOpened")
        };
        assert!(
            second_artifact.exists(),
            "second CoreApi must persist in repo two"
        );
        assert_ne!(
            first_run, second_run,
            "local run:1 handles must be namespaced"
        );

        let first_view = router.run(first_run).await.expect("first route");
        let second_view = router.run(second_run).await.expect("second route");
        assert!(first_view.task_list_path.starts_with(first.path()));
        assert!(second_view.task_list_path.starts_with(second.path()));
        assert!(!first_view.task_list_path.starts_with(second.path()));
        assert!(!second_view.task_list_path.starts_with(first.path()));
    }

    #[tokio::test]
    async fn taskless_plan_routes_by_its_existing_parent_project() {
        let repo = project("router-taskless");
        std::fs::remove_file(repo.path().join("docs/plans/same-plan/TASKS.md"))
            .expect("remove tasks");
        let factory: ProjectApiFactory = Arc::new(|_| Ok(Arc::new(FakeApi::new())));
        let router = ProjectApiRouter::new([repo.path().to_path_buf()], factory);

        let result = router
            .execute(Command::OpenRun {
                task_list_path: repo.path().join("docs/plans/same-plan/TASKS.md"),
            })
            .await;
        assert!(matches!(result, Ok(CommandOutcome::RunOpened { .. })));
    }

    #[test]
    fn path_outside_a_git_project_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("TASKS.md");
        std::fs::write(&path, "# tasks\n").expect("write tasks");
        let error = project_for_task_path(&path).expect_err("must reject non-project path");
        assert!(error.contains("not inside a Git repository"));
    }

    #[test]
    fn git_resolves_nested_directories_to_the_authoritative_top_level() {
        let repo = project("router-git-root");
        let nested = repo.path().join("docs/plans/same-plan");
        assert_eq!(
            canonicalize_project_root(&nested).expect("resolve nested path"),
            std::fs::canonicalize(repo.path()).expect("canonical repo")
        );
    }

    #[test]
    fn fake_dot_git_marker_is_not_accepted_as_a_repository() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(dir.path().join(".git")).expect("fake marker");
        let error = canonicalize_project_root(dir.path()).expect_err("fake marker must fail");
        assert!(error.contains("not inside a Git worktree"));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_roots_collapse_symlink_and_subdirectory_aliases() {
        use std::os::unix::fs::symlink;

        let repo = project("router-workspace-alias");
        let aliases = tempfile::tempdir().expect("alias parent");
        let alias = aliases.path().join("repo-link");
        symlink(repo.path(), &alias).expect("create repo alias");
        let (roots, rejected) =
            normalize_workspace_roots([repo.path().to_path_buf(), alias, repo.path().join("docs")]);
        assert!(rejected.is_empty());
        assert_eq!(roots.len(), 1);
        assert_eq!(
            roots[0],
            std::fs::canonicalize(repo.path()).expect("canonical repo")
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_final_task_symlink_is_rejected_not_treated_as_missing() {
        use std::os::unix::fs::symlink;

        let repo = project("router-dangling-task");
        let task_path = repo.path().join("docs/plans/same-plan/TASKS.md");
        std::fs::remove_file(&task_path).expect("remove task file");
        symlink("missing-target.md", &task_path).expect("create dangling link");
        let error = project_for_task_path(&task_path).expect_err("dangling link must fail");
        assert!(error.contains("cannot resolve task list"));
    }
}
