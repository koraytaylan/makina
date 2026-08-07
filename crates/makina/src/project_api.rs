//! Project-aware multiplexing for the TUI's single [`Api`] handle.
//!
//! Each [`CoreApi`](makina_core::orchestrator::CoreApi) is intentionally rooted
//! in one Git repository: its worktree manager, config, persistence, audit, and
//! transcript paths all belong to that repository. [`ProjectApiRouter`] keeps
//! that invariant while letting the workspace UI open several repositories.
//! Project-qualified plan identities select the Git root explicitly; the
//! router lazily creates one API per root and translates each project's
//! session-local [`RunId`] into a process-wide ID before exposing it to the TUI.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
            Event::PlanRegistered {
                plan_dir,
                registration_oid,
            } => Event::PlanRegistered {
                plan_dir,
                registration_oid,
            },
            // Forward the inner API's single event instead of synthesizing a
            // second copy in `execute`. This preserves the refresh events that
            // ReinterpretRun and ResetRun intentionally emit as RunOpened.
            Event::RunOpened { run, plan_dir } => Event::RunOpened {
                run: map(run),
                plan_dir,
            },
            Event::RunStatusChanged { run, status } => Event::RunStatusChanged {
                run: map(run),
                status,
            },
            Event::RepositoryLeaseWaiting { run, owner } => Event::RepositoryLeaseWaiting {
                run: map(run),
                owner,
            },
            Event::RunProgress { run, phase } => Event::RunProgress {
                run: map(run),
                phase,
            },
            Event::RunCommand {
                run,
                command,
                working_dir,
            } => Event::RunCommand {
                run: map(run),
                command,
                working_dir,
            },
            Event::TaskStateChanged { run, task, state } => Event::TaskStateChanged {
                run: map(run),
                task,
                state,
            },
            Event::TaskFailed { run, task, reason } => Event::TaskFailed {
                run: map(run),
                task,
                reason,
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
    /// Generate a plan through the API bound to an explicitly selected project.
    ///
    /// The project root is a routing concern and is intentionally not embedded
    /// in the repository-local generation command.
    pub async fn generate_plan_bundle(
        &self,
        project_root: &Path,
        blueprint: makina_core::api::GeneratedPlanBlueprint,
    ) -> Result<CommandOutcome, ApiError> {
        let project_root = self.require_allowed_project(project_root)?;
        self.project_api(&project_root)
            .await?
            .execute(Command::GeneratePlanBundle { blueprint })
            .await
    }

    /// Bring a registered plan's files into an explicitly selected project's
    /// checkout.
    ///
    /// Project-qualified for the same reason [`Self::open_plan`] is: a
    /// `PlanKey` is repository-relative, so the root has to be stated rather
    /// than guessed from matching directory names across the workspace.
    pub async fn check_out_plan(
        &self,
        project_root: &Path,
        plan_dir: makina_core::plan::PlanKey,
    ) -> Result<CommandOutcome, ApiError> {
        let project_root = self.require_allowed_project(project_root)?;
        self.project_api(&project_root)
            .await?
            .execute(Command::CheckOutPlan { plan_dir })
            .await
    }

    /// Open a plan in an explicitly selected workspace project.
    ///
    /// `PlanKey` is repository-relative, so the project root is deliberately
    /// supplied at this binary boundary instead of inferred from matching
    /// directory names across the workspace.
    ///
    /// # Why existence is not checked here
    ///
    /// A plan does **not** have to be present in the working tree to be real.
    /// Discovery resolves plans from three sources — the working tree, the
    /// configured base revision, and retained `refs/heads/plan/*` registration
    /// refs — so a plan that was authored and registered, but whose branch is
    /// not checked out into the main worktree, is legitimately listed and
    /// legitimately runnable (the run executes in its own worktree).
    ///
    /// This boundary used to `canonicalize` the plan directory and fail when it
    /// was absent, which made it strictly stricter than the orchestrator it
    /// delegates to: a registered plan visible in the sidebar could not be
    /// started, reporting `cannot resolve plan directory …: No such file or
    /// directory`. Existence is therefore left to the orchestrator, which knows
    /// all three sources and can say precisely which one it is missing from.
    ///
    /// What this boundary still owes is **containment**: the plan must not
    /// escape the project. `PlanKey::parse` already guarantees a relative path
    /// of `Normal` components (no `..`, no absolute prefix), so the join is
    /// lexically inside `project_root`; the one way out is a symlink, and a
    /// symlink can only escape if it exists. Checking containment exactly when
    /// the path resolves therefore keeps the guarantee without turning it into
    /// an existence requirement.
    pub async fn open_plan(
        &self,
        project_root: &Path,
        plan_dir: makina_core::plan::PlanKey,
    ) -> Result<CommandOutcome, ApiError> {
        let project_root = self.require_allowed_project(project_root)?;
        let candidate = project_root.join(&plan_dir.relative_dir);
        if let Ok(resolved) = std::fs::canonicalize(&candidate)
            && (!resolved.starts_with(&project_root) || !resolved.is_dir())
        {
            return Err(ApiError::InvalidCommand {
                reason: format!(
                    "plan directory {} is not contained in project {}",
                    candidate.display(),
                    project_root.display()
                ),
            });
        }
        let api = self.project_api(&project_root).await?;
        let outcome = api.execute(Command::OpenPlan { plan_dir }).await?;
        let CommandOutcome::RunOpened { run: local } = outcome else {
            return Err(ApiError::Internal {
                reason: "project API acknowledged OpenPlan without returning a run".to_string(),
            });
        };
        let run = self.events.global_for(&project_root, local);
        Ok(CommandOutcome::RunOpened { run })
    }

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
            Command::GeneratePlanBundle { .. } => Err(ApiError::InvalidCommand {
                reason:
                    "GeneratePlanBundle must be sent with an explicit project through the router"
                        .into(),
            }),
            Command::RegisterPlan { .. } => Err(ApiError::InvalidCommand {
                reason: "RegisterPlan must be sent to a repository-bound API".into(),
            }),
            Command::CheckOutPlan { .. } => Err(ApiError::InvalidCommand {
                reason: "CheckOutPlan must be sent with an explicit project through the router"
                    .into(),
            }),
            Command::SetTaskDisposition {
                run,
                task,
                expected_plan_oid,
                action,
            } => {
                self.execute_for_run(run, |run| Command::SetTaskDisposition {
                    run,
                    task,
                    expected_plan_oid,
                    action,
                })
                .await
            }
            Command::FinalizePlan {
                plan_dir,
                run_uid,
                expected_plan_oid,
                input,
            } => {
                let view = self
                    .runs()
                    .await
                    .into_iter()
                    .find(|view| view.run_uid == run_uid && view.plan_dir == plan_dir)
                    .ok_or_else(|| ApiError::InvalidCommand {
                        reason: "no routed run matches plan_dir and run_uid".into(),
                    })?;
                self.execute_for_run(view.id, |run| {
                    let _ = run;
                    Command::FinalizePlan {
                        plan_dir,
                        run_uid,
                        expected_plan_oid,
                        input,
                    }
                })
                .await
            }
            Command::ReprepareFinalization {
                plan_dir,
                run_uid,
                expected_plan_oid,
            } => {
                let view = self
                    .runs()
                    .await
                    .into_iter()
                    .find(|view| view.run_uid == run_uid && view.plan_dir == plan_dir)
                    .ok_or_else(|| ApiError::InvalidCommand {
                        reason: "no routed run matches plan_dir and run_uid".into(),
                    })?;
                self.execute_for_run(view.id, |run| {
                    let _ = run;
                    Command::ReprepareFinalization {
                        plan_dir,
                        run_uid,
                        expected_plan_oid,
                    }
                })
                .await
            }
            Command::OpenPlan { plan_dir } => Err(ApiError::InvalidCommand {
                reason: format!(
                    "OpenPlan for {} requires a project-qualified plan identity",
                    plan_dir.relative_dir.display()
                ),
            }),
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
                Command::OpenPlan { plan_dir } => {
                    let id = {
                        let mut runs = self.runs.lock().expect("runs mutex poisoned");
                        if let Some(run) = runs.iter().find(|run| run.plan_dir == plan_dir) {
                            run.id
                        } else {
                            let id = RunId(self.next_id.fetch_add(1, Ordering::Relaxed));
                            let project = "fake-project".to_string();
                            runs.push(RunView {
                                id,
                                run_uid: format!("fake-{}", id.0),
                                project,
                                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test")
                                    .unwrap(),
                                status: RunStatus::Pending,
                                tasks: Vec::new(),
                                report: IngestionReport::default(),
                            });
                            id
                        }
                    };
                    let _ = self.events.send(Event::RunOpened { run: id, plan_dir });
                    Ok(CommandOutcome::RunOpened { run: id })
                }
                Command::ReinterpretRun { run } | Command::ResetRun { run } => {
                    let plan_dir = self
                        .runs
                        .lock()
                        .expect("runs mutex poisoned")
                        .iter()
                        .find(|view| view.id == run)
                        .map(|view| view.plan_dir.clone())
                        .ok_or(ApiError::UnknownRun { run })?;
                    let _ = self.events.send(Event::RunOpened { run, plan_dir });
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
        std::fs::create_dir_all(dir.path().join("docs/plans/0001-Test"))
            .expect("create plan directory");
        std::fs::create_dir_all(dir.path().join("docs/plans/same-plan"))
            .expect("create routing marker directory");
        std::fs::write(dir.path().join("docs/plans/same-plan/marker"), "route\n")
            .expect("write routing marker");
        dir
    }

    fn generation_blueprint() -> makina_core::api::GeneratedPlanBlueprint {
        use makina_core::api::{
            GeneratedInitialStatusBlueprint, GeneratedPlanBlueprint, GeneratedTaskBlueprint,
            GeneratedWorkstreamBlueprint,
        };
        GeneratedPlanBlueprint {
            slug: "generated-plan".into(),
            title: "Generated plan".into(),
            scope: "Scope".into(),
            architecture: "Architecture".into(),
            initial_status: GeneratedInitialStatusBlueprint {
                goal: "Goal".into(),
                root_cause: "Cause".into(),
                approach: "Approach".into(),
                outcome: String::new(),
                last_updated: "2026-07-20".into(),
            },
            workstreams: vec![GeneratedWorkstreamBlueprint {
                id: "0001".into(),
                title: "Core".into(),
            }],
            tasks: vec![GeneratedTaskBlueprint {
                sequence: "01".into(),
                id: "generate".into(),
                title: "Generate".into(),
                workstream: "0001".into(),
                kind: "task".into(),
                depends_on: vec![],
                touches: vec!["crates/**".into()],
                gated: false,
                body: "Generate safely.".into(),
            }],
        }
    }

    /// A plan that is **not** in the working tree still opens.
    ///
    /// Discovery lists plans from the working tree, the base revision, and
    /// retained `refs/heads/plan/*` refs, so a plan that was authored and
    /// registered — but whose branch is not checked out into the main worktree —
    /// is shown in the sidebar and is runnable. This boundary used to
    /// `canonicalize` the directory and reject exactly that case with
    /// "cannot resolve plan directory …: No such file or directory", making it
    /// stricter than the orchestrator it delegates to: the plan was visible and
    /// could never be started.
    #[tokio::test]
    async fn a_plan_absent_from_the_working_tree_still_reaches_the_orchestrator() {
        let repo = project("router-ref-only");
        let fake = Arc::new(FakeApi::new());
        let routed = Arc::clone(&fake);
        let factory: ProjectApiFactory = Arc::new(move |_| {
            let api: Arc<dyn Api> = routed.clone();
            Ok(api)
        });
        let router = ProjectApiRouter::new([repo.path().to_path_buf()], factory);

        // Registered on a plan ref, never checked out here.
        let plan = makina_core::plan::PlanKey::parse("docs/plans/0002-Only-On-A-Ref").unwrap();
        assert!(
            !repo.path().join(&plan.relative_dir).exists(),
            "precondition: the plan is absent from the working tree"
        );

        let outcome = router
            .open_plan(repo.path(), plan.clone())
            .await
            .expect("a registered plan must reach the orchestrator, not be refused here");
        assert!(matches!(outcome, CommandOutcome::RunOpened { .. }));

        // Deciding whether the plan really exists is the orchestrator's job —
        // it is the only layer that knows all three sources.
        assert!(
            fake.commands.lock().unwrap().iter().any(
                |command| matches!(command, Command::OpenPlan { plan_dir } if *plan_dir == plan)
            ),
            "OpenPlan must have been delegated"
        );
    }

    #[tokio::test]
    async fn generation_requires_and_routes_an_explicit_project() {
        let repo = project("router-generation");
        let fake = Arc::new(FakeApi::new());
        let routed = Arc::clone(&fake);
        let factory: ProjectApiFactory = Arc::new(move |_| {
            let api: Arc<dyn Api> = routed.clone();
            Ok(api)
        });
        let router = ProjectApiRouter::new([repo.path().to_path_buf()], factory);

        router
            .generate_plan_bundle(repo.path(), generation_blueprint())
            .await
            .expect("qualified generation should reach project API");
        assert!(matches!(
            fake.commands.lock().unwrap().as_slice(),
            [Command::GeneratePlanBundle { .. }]
        ));

        let error = router
            .execute(Command::GeneratePlanBundle {
                blueprint: generation_blueprint(),
            })
            .await
            .expect_err("unqualified generation must fail closed");
        assert!(matches!(error, ApiError::InvalidCommand { .. }));
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

        let plan = makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap();
        let CommandOutcome::RunOpened { run: first_run } = router
            .open_plan(first.path(), plan.clone())
            .await
            .expect("open first")
        else {
            panic!("expected first RunOpened")
        };
        let CommandOutcome::RunOpened { run: second_run } = router
            .open_plan(second.path(), plan)
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

    #[cfg(unix)]
    #[tokio::test]
    async fn qualified_open_rejects_plan_directory_symlink_escape() {
        use std::os::unix::fs::symlink;

        let repo = project("router-symlink-escape");
        let outside = tempfile::tempdir().expect("create outside directory");
        let plan_path = repo.path().join("docs/plans/0001-Test");
        std::fs::remove_dir(&plan_path).expect("remove real plan directory");
        symlink(outside.path(), &plan_path).expect("create escaping symlink");

        let factory: ProjectApiFactory = Arc::new(|_| Ok(Arc::new(FakeApi::new())));
        let router = ProjectApiRouter::new([repo.path().to_path_buf()], factory);
        let error = router
            .open_plan(
                repo.path(),
                makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            )
            .await
            .expect_err("escaping plan symlink must be rejected");

        assert!(
            matches!(error, ApiError::InvalidCommand { .. }),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn run_opened_is_forwarded_once_and_refresh_events_are_preserved() {
        let repo = project("router-events");
        let factory: ProjectApiFactory = Arc::new(|_| Ok(Arc::new(FakeApi::new())));
        let router = ProjectApiRouter::new([repo.path().to_path_buf()], factory);
        let mut events = router.subscribe();

        let CommandOutcome::RunOpened { run } = router
            .open_plan(
                repo.path(),
                makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            )
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
            "OpenPlan must not be duplicated by the router"
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
                if let Command::OpenPlan { plan_dir } = command {
                    let run = RunId(1);
                    let _ = self.events.send(Event::RunOpened { run, plan_dir });
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
        let register_root = repo_root.clone();
        let register =
            tokio::spawn(async move { register_router.register_project(register_root).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !subscribe_entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("subscribe was never entered");

        let open_router = Arc::clone(&router);
        let open = tokio::spawn(async move {
            open_router
                .open_plan(
                    &repo_root,
                    makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
                )
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
        let plan_dir = makina_core::plan::PlanKey::parse("docs/plans/0001-Same-Plan").unwrap();
        fake.runs
            .lock()
            .expect("runs mutex poisoned")
            .push(RunView {
                id: RunId(40),
                run_uid: "01STABLE-ROUTER-UID".to_string(),
                project: "stable".to_string(),
                plan_dir,
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

        let error = router
            .open_plan(
                second.path(),
                makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            )
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
            .open_plan(
                second.path(),
                makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            )
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
            .open_plan(
                second.path(),
                makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            )
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
                .open_plan(
                    second.path(),
                    makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
                )
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
        let _wrong_path = std::fs::canonicalize(second.path().join("docs/plans/same-plan/marker"))
            .expect("canonical second routing marker");
        fake.runs
            .lock()
            .expect("runs mutex poisoned")
            .push(RunView {
                id: RunId(7),
                run_uid: "01WRONGPROJECTVIEW00000001".to_string(),
                project: "wrong".to_string(),
                plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
                status: RunStatus::Completed,
                tasks: Vec::new(),
                report: IngestionReport::default(),
            });

        assert_eq!(
            router.runs().await.len(),
            1,
            "a typed PlanKey is qualified by the owning project API, not a path embedded in the view"
        );

        let mut events = router.subscribe();
        let _ = fake.events.send(Event::RunOpened {
            run: RunId(7),
            plan_dir: makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.next())
                .await
                .is_ok(),
            "the owner-qualified event must be forwarded"
        );
    }

    #[tokio::test]
    async fn real_core_apis_persist_identical_plan_slugs_only_in_their_own_repos() {
        use makina_core::backend::AgentBackend;
        use makina_core::backend::noop::NoopBackend;
        use makina_core::config::{Config, GlobalConfig, ProjectConfig};
        use makina_core::dependency::EdgeInferrer;
        use makina_core::interpreter::SourceProjectionUnavailable;
        use makina_core::orchestrator::{CoreApi, run_slug};
        use makina_core::test_support::setup_temp_repo;
        use makina_core::worktree::WorktreeManager;

        fn copy_tree(source: &Path, destination: &Path) {
            std::fs::create_dir_all(destination).expect("create plan directory");
            for entry in std::fs::read_dir(source).expect("read fixture") {
                let entry = entry.expect("fixture entry");
                let target = destination.join(entry.file_name());
                if entry.file_type().expect("fixture type").is_dir() {
                    copy_tree(&entry.path(), &target);
                } else {
                    std::fs::copy(entry.path(), target).expect("copy fixture file");
                }
            }
        }

        fn write_plan(repo: &Path) {
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../makina-core/tests/fixtures/plan-bundles/valid/0049-Sample");
            copy_tree(&fixture, &repo.join("docs/plans/0049-Sample"));
            let git = |args: &[&str]| {
                let output = ProcessCommand::new("git")
                    .args(args)
                    .current_dir(repo)
                    .output()
                    .expect("run git");
                assert!(
                    output.status.success(),
                    "git {args:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            git(&["config", "user.email", "routing@example.invalid"]);
            git(&["config", "user.name", "Routing Test"]);
            git(&["branch", "-M", "develop"]);
            git(&["add", "."]);
            git(&["commit", "-qm", "typed plan fixture"]);
        }

        let first = setup_temp_repo();
        let second = setup_temp_repo();
        write_plan(first.path());
        write_plan(second.path());
        let factory: ProjectApiFactory = Arc::new(|root| {
            let interpreter = Arc::new(EdgeInferrer::new(Arc::new(
                SourceProjectionUnavailable::new(),
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

        let plan_key = makina_core::plan::PlanKey::parse("docs/plans/0049-Sample").unwrap();
        let CommandOutcome::RunOpened { run: first_run } = router
            .open_plan(first.path(), plan_key.clone())
            .await
            .expect("open first")
        else {
            panic!("expected first RunOpened")
        };
        assert_eq!(run_slug(&plan_key.relative_dir), "0049-sample");
        assert_eq!(
            router.run(first_run).await.unwrap().project,
            first.path().file_name().unwrap().to_string_lossy()
        );

        let CommandOutcome::RunOpened { run: second_run } = router
            .open_plan(second.path(), plan_key.clone())
            .await
            .expect("open second")
        else {
            panic!("expected second RunOpened")
        };
        assert_ne!(
            first_run, second_run,
            "local run:1 handles must be namespaced"
        );

        let first_view = router.run(first_run).await.expect("first route");
        let second_view = router.run(second_run).await.expect("second route");
        assert_eq!(first_view.plan_dir, plan_key);
        assert_eq!(second_view.plan_dir, first_view.plan_dir);
        assert_eq!(first_view.tasks[0].id, second_view.tasks[0].id);
        assert!(first_view.tasks[0].authored.is_some());
        assert!(second_view.tasks[0].authored.is_some());
    }

    #[tokio::test]
    async fn plan_routes_by_its_existing_parent_project() {
        let repo = project("router-plan-parent");
        let factory: ProjectApiFactory = Arc::new(|_| Ok(Arc::new(FakeApi::new())));
        let router = ProjectApiRouter::new([repo.path().to_path_buf()], factory);

        let result = router
            .open_plan(
                repo.path(),
                makina_core::plan::PlanKey::parse("docs/plans/0001-Test").unwrap(),
            )
            .await;
        assert!(matches!(result, Ok(CommandOutcome::RunOpened { .. })));
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
}
