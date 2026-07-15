//! Makina — multi-agent software-factory orchestrator (binary entry point).
//!
//! This file is the thin `makina` binary: it wires the components together and
//! contains no logic. The TUI modules (`app`, `browser`, `event`, `tui`, `ui`)
//! and the per-run file log layer (`log`) live in the sibling `makina` **library**
//! crate (`src/lib.rs`) so integration tests can exercise them directly.
//!
//! # Architecture
//!
//! The TUI is **presentation only**.  It consumes `makina-core::api::Api` via
//! `Arc<dyn Api>` and holds **no orchestration logic**.  State changes arrive
//! through `api.subscribe()` (pushed) and initial snapshots are fetched from
//! `api.runs()` (pulled once at startup).
//!
//! # Interactive launch (manual only)
//!
//! `cargo run -p makina` starts the TUI.  Press `q`, `Esc`, or `Ctrl-C` to quit.
//! CI cannot drive an interactive terminal; the test suite uses `ratatui::TestBackend`
//! for rendering tests and unit-tests for update logic.

use std::collections::HashMap;
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use makina::project_api::{
    ProjectApiFactory, ProjectApiRouter, canonicalize_project_root, normalize_workspace_roots,
};
use makina::{app, event, exit, log, tui};
use makina_acp::AcpBackend;
use makina_core::api::{Api, ApiError};
use makina_core::audit::JsonlAuditSink;
use makina_core::backend::AgentBackend;
use makina_core::config::Config;
use makina_core::orchestrator::CoreApi;
use makina_core::preflight::probe_providers;
use makina_core::worktree::WorktreeManager;

/// Resolve one role's configured backend from a project's provider set.
fn resolve_role_backend(
    provider_name: Option<&str>,
    provider_backends: &HashMap<String, Arc<dyn AgentBackend>>,
    first_provider_name: Option<&str>,
    legacy_backend: Arc<dyn AgentBackend>,
) -> Arc<dyn AgentBackend> {
    let name = provider_name.or(first_provider_name).unwrap_or("default");
    provider_backends
        .get(name)
        .cloned()
        .unwrap_or(legacy_backend)
}

/// Build one repository-rooted orchestrator.
///
/// Config, worktrees, persisted graphs, transcripts, logs, and the permission
/// audit registry must all agree on this root. The project router calls this
/// once per opened Git repository instead of reusing the launch repository's
/// dependencies for every folder.
fn build_project_api(repo_root: &std::path::Path, config: Config) -> Arc<dyn Api> {
    let audit_sink = Arc::new(JsonlAuditSink::new(repo_root.to_path_buf()));

    let mut provider_backends: HashMap<String, Arc<dyn AgentBackend>> = HashMap::new();
    for provider in &config.providers {
        let mut backend =
            AcpBackend::new(provider.command.clone(), provider.args.clone()).with_audit_sink(
                Arc::clone(&audit_sink) as Arc<dyn makina_core::governance::AuditSink>,
            );
        for (key, value) in &provider.env {
            backend = backend.env(key.clone(), value.clone());
        }
        provider_backends.insert(provider.name.clone(), Arc::new(backend));
    }

    let legacy_backend: Arc<dyn AgentBackend> =
        Arc::new(
            AcpBackend::new(config.backend.command.clone(), config.backend.args.clone())
                .with_audit_sink(
                    Arc::clone(&audit_sink) as Arc<dyn makina_core::governance::AuditSink>
                ),
        );
    let first_provider_name = config
        .providers
        .first()
        .map(|provider| provider.name.as_str());
    let developer_backend = resolve_role_backend(
        config
            .roles
            .developer
            .as_ref()
            .map(|role| role.provider.as_str()),
        &provider_backends,
        first_provider_name,
        Arc::clone(&legacy_backend),
    );
    let reviewer_backend = resolve_role_backend(
        config
            .roles
            .reviewer
            .as_ref()
            .map(|role| role.provider.as_str()),
        &provider_backends,
        first_provider_name,
        Arc::clone(&legacy_backend),
    );
    let planner_backend = resolve_role_backend(
        config
            .roles
            .planner
            .as_ref()
            .map(|role| role.provider.as_str()),
        &provider_backends,
        first_provider_name,
        legacy_backend,
    );

    let ingestion_interpreter: Arc<dyn makina_core::interpreter::TaskListInterpreter> =
        Arc::new(makina_core::dependency::EdgeInferrer::new(Arc::new(
            makina_core::interpreter::StructuredTextInterpreter::new(),
        )));
    let planner_interpreter = match makina_core::interpreter::build_planner_interpreter(
        &config.planner.mechanism,
        Some(planner_backend),
    ) {
        Ok(interpreter) => interpreter,
        Err(error) => {
            tracing::warn!(
                project_root = %repo_root.display(),
                %error,
                "planner mechanism unavailable; using deterministic planner"
            );
            Arc::new(makina_core::dependency::EdgeInferrer::new(Arc::new(
                makina_core::interpreter::StructuredTextInterpreter::new(),
            ))) as Arc<dyn makina_core::interpreter::TaskListInterpreter>
        }
    };
    let worktree_manager =
        WorktreeManager::new(repo_root.to_path_buf(), config.base_branch.clone());

    Arc::new(CoreApi::with_audit_registry(
        ingestion_interpreter,
        planner_interpreter,
        developer_backend,
        reviewer_backend,
        worktree_manager,
        config,
        audit_sink as Arc<dyn makina_core::audit::AuditRegistry>,
    ))
}

/// Run the headless `--doctor` preflight check.
///
/// Reads `$PATH` once, calls `detect_backend_in_path` for the detected agent name,
/// attempts `Config::load_defaults()` for `config_loaded`, calls
/// `render_doctor_report` with both, prints the report, and returns its exit code
/// (0 if a backend is detected or config loads, 1 otherwise).
fn run_headless_doctor() -> i32 {
    use makina_core::preflight::detect_backend_in_path;

    // Read PATH once for detected backend lookup
    let path = std::env::var("PATH").unwrap_or_default();
    let detected = detect_backend_in_path(&path);

    // Extract agent name if detected
    let agent_name = detected.as_ref().map(|b| b.agent);

    // Attempt to load config
    let config_loaded = Config::load_defaults().is_ok();

    // Render the report and get the exit code
    let (report, exit_code) = makina::cli::render_doctor_report(agent_name, config_loaded);
    println!("{report}");
    exit_code
}

#[tokio::main]
async fn main() {
    // ── CLI Dispatch ────────────────────────────────────────────────────────────
    // Dispatch on command-line arguments before any TUI or config work.  This
    // ensures --help, --version, and --doctor print and exit without launching
    // the full TUI stack.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match makina::cli::parse_args(&args) {
        makina::cli::CliAction::ShowHelp => {
            println!("{}", makina::cli::help_text());
            return;
        }
        makina::cli::CliAction::ShowVersion => {
            println!("{}", makina::cli::version_text());
            return;
        }
        makina::cli::CliAction::RunDoctor => {
            std::process::exit(run_headless_doctor());
        }
        makina::cli::CliAction::Create { path, template } => {
            match makina::scaffold::scaffold_project(std::path::Path::new(&path), &template) {
                Ok(report) => {
                    println!("{report}");
                    return;
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        makina::cli::CliAction::CreateError(msg) => {
            eprintln!("error: {msg}\n\n{}", makina::cli::help_text());
            std::process::exit(2);
        }
        makina::cli::CliAction::Unknown(flag) => {
            eprintln!("unknown flag: {flag}\n\n{}", makina::cli::help_text());
            std::process::exit(2);
        }
        makina::cli::CliAction::LaunchTui => {
            // Fall through to the existing TUI startup
        }
    }

    // Resolve the launch checkout through Git before loading project config or
    // constructing any project-scoped service. Launching from a subdirectory,
    // linked worktree, or symlink alias must still select one authoritative
    // repository root.
    let launch_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let launch_project_root = canonicalize_project_root(&launch_dir).ok();
    let repo_root = launch_project_root
        .clone()
        .unwrap_or_else(|| std::fs::canonicalize(&launch_dir).unwrap_or(launch_dir));

    // ── Config ──────────────────────────────────────────────────────────────────
    // Load the resolved two-layer config (global ~/.makina/config.toml + project
    // ./makina.toml).  Supplies the agent backend command, the gates, the caps,
    // the concurrency limit, and the base branch the orchestrator drives runs
    // with.  A load/validation failure is fatal (we cannot run without it).
    //
    // On failure we emit a multi-line guidance block naming which files were
    // checked, whether each existed, the "project overrides global" precedence
    // note, and a pointer to the README "Configure" section.
    //
    // `load_for_repo_with_paths` returns `(Result<Config, ConfigError>, ConfigPaths)`
    // so the resolved paths are in hand even when loading fails — no need to
    // re-derive them in the error arm.
    let (load_result, load_paths) = Config::load_for_repo_with_paths(&repo_root);
    let config = match load_result {
        Ok(c) => c,
        Err(e) => {
            let global_status = match &load_paths.global {
                None => "  global  : (HOME unset — skipped)".to_string(),
                Some(p) => {
                    if p.exists() {
                        format!("  global  : {} (found)", p.display())
                    } else {
                        format!("  global  : {} (not found — using defaults)", p.display())
                    }
                }
            };
            let project_status = match &load_paths.project {
                None => "  project : (could not resolve — using defaults)".to_string(),
                Some(p) => {
                    if p.exists() {
                        format!("  project : {} (found)", p.display())
                    } else {
                        format!("  project : {} (not found — using defaults)", p.display())
                    }
                }
            };

            eprintln!(
                "error: failed to load configuration: {e}\n\
                 \n\
                 Files checked (project overrides global):\n\
                 {global_status}\n\
                 {project_status}\n\
                 \n\
                 See README § Configure for a minimal config example."
            );

            // If the error is the recoverable empty-backend case, print the explicit
            // pointer to the Doctor 'w' scaffold.
            if e.is_recoverable_empty_backend() {
                eprintln!(
                    "\n\
                     Open Makina and press 'w' in the Doctor overlay to auto-detect and write ~/.makina/config.toml."
                );
            }

            std::process::exit(1);
        }
    };

    // ── Execution dependencies (task 31: run-control) ─────────────────────────
    // The orchestrator drives Runs (StartRun → Supervisor scheduler) using:
    //  - the agent BACKEND: the ACP CLI from `config.backend` (the e2e/task-33
    //    seam swaps this for any other `AgentBackend`; tests inject NoopBackend);
    //  - a WORKTREE MANAGER rooted at the repo (CWD) on `config.base_branch`;
    //  - the resolved CONFIG (gates, caps, concurrency).
    // ── Workspace Persistence ───────────────────────────────────────────────
    // Load the workspace from $HOME/.makina/workspace.toml. Auto-discover the
    // launch CWD if it is a Makina-ready git repo (has .git and docs/plans),
    // and add it to opened_folders for multi-folder support.
    let mut workspace = makina::workspace::Workspace::load().unwrap_or_else(|e| {
        eprintln!("failed to load workspace: {e}");
        makina::workspace::Workspace::new()
    });

    // Auto-discover the authoritative launch root, then canonicalize every
    // persisted entry. This collapses subdirectory/symlink aliases before the
    // exact same list is given to both App and ProjectApiRouter.
    if let Some(root) = launch_project_root
        && root.join("docs/plans").exists()
    {
        workspace.add_folder(root);
    }
    let (opened_folders, rejected_folders) =
        normalize_workspace_roots(workspace.opened_folders.iter().cloned());
    for (folder, error) in rejected_folders {
        eprintln!(
            "ignoring invalid workspace folder {}: {error}",
            folder.display()
        );
    }
    workspace.opened_folders = opened_folders.iter().cloned().collect();

    // ── Tracing subscriber: file + TUI-channel layers (tasks log-subscriber-*) ─
    // Installed ONCE here, after the audit-sink setup and before the event loop.
    //
    // Two layers are composed onto one registry:
    //  - `RunFileLayer` (task log-subscriber-file): run ids are allocated lazily
    //    per OpenRun and many runs can be open at once, so the file destination
    //    cannot be a static path — it resolves per event from the current span's
    //    `run_uid` field and appends to `.makina/runs/{run_uid}/logs/run.log`.
    //    Events carry the key because `run_graph` opens an
    //    `info_span!(run_uid = …)`.
    //  - `TuiLogLayer` (task log-subscriber-tui-channel): converts each event to
    //    a `LogRecord` and `try_send`s it onto a bounded mpsc channel; on a full
    //    channel the record is dropped (never blocks). The `Receiver` is held
    //    here and threaded into the event loop so plan-0015 can drain it.
    //
    // There is no prior `tracing_subscriber` usage in the repo; this is the
    // first install.
    let log_rx = {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        let (tui_layer, log_rx) = log::tui_log_channel();
        tracing_subscriber::registry()
            .with(log::RunFileLayer::new(repo_root.clone()))
            .with(tui_layer)
            .init();
        log_rx
    };

    tracing::info!("Using project-scoped CoreApi instances with deterministic TUI ingestion");

    // Probe each provider's command for presence on the filesystem before
    // moving config into the API.
    let provider_probes = probe_providers(&config);

    // Check if the configured base branch exists in the repository (for doctor view).
    let base_branch_exists = {
        let output = std::process::Command::new("git")
            .args(["branch", "--list", &config.base_branch])
            .current_dir(&repo_root)
            .output()
            .unwrap_or_else(|_| std::process::Output {
                status: std::process::ExitStatus::from_raw(1),
                stdout: vec![],
                stderr: vec![],
            });
        let stdout = String::from_utf8_lossy(&output.stdout);
        !stdout.trim().is_empty()
    };

    // Clone providers, roles, caps, concurrency, merge mode, and theme_name before config is moved into the API.
    let providers_for_app = config.providers.clone();
    let roles_for_app = config.roles.clone();
    let caps_for_app = config.caps.clone();
    let concurrency_for_app = config.concurrency;
    let final_merge_for_app = config.merge.final_;
    let theme_name_for_app = config.theme_name.clone();

    let factory: ProjectApiFactory = Arc::new(|project_root| {
        let (result, _paths) = Config::load_for_repo_with_paths(project_root);
        let project_config = result.map_err(|error| ApiError::InvalidCommand {
            reason: format!(
                "failed to load configuration for {}: {error}",
                project_root.display()
            ),
        })?;
        Ok(build_project_api(project_root, project_config))
    });
    let project_api = Arc::new(ProjectApiRouter::new(opened_folders.clone(), factory));

    // Pre-register every persisted workspace folder so historical run
    // snapshots from all projects are visible at startup. A broken folder is
    // non-fatal: discovery can still render it and OpenRun will surface the
    // project-specific configuration error if the user tries to execute it.
    for folder in &opened_folders {
        if let Err(error) = project_api.register_project(folder).await {
            tracing::warn!(
                project_root = %folder.display(),
                %error,
                "workspace project runtime is unavailable"
            );
        }
    }
    let api: Arc<dyn Api> = project_api;

    // ── Initial state ─────────────────────────────────────────────────────────
    let initial_runs = api.runs().await;

    let mut app = app::App::with_config(
        Arc::clone(&api),
        initial_runs,
        repo_root,
        providers_for_app,
        roles_for_app,
        provider_probes,
        load_paths,
        base_branch_exists,
        caps_for_app,
        concurrency_for_app,
        final_merge_for_app,
        opened_folders,
        workspace,
    );

    // Restore theme from GlobalConfig; unknown/absent names fall back to Ayu Dark with no panic.
    let active_theme = makina::theme::Theme::builtin_themes()
        .into_iter()
        .find(|t| t.name == theme_name_for_app)
        .unwrap_or_else(makina::theme::ayu_dark);
    app.active_theme = active_theme;

    app.load_initial_exchanges();

    // ── Terminal lifecycle ────────────────────────────────────────────────────
    let mut tui = match tui::Tui::init() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to initialise terminal: {e}");
            std::process::exit(1);
        }
    };

    // ── No-orphan: out-of-band signal reaper (task tui-exit-reaps-agents) ──────
    // The in-TUI Ctrl-C is already a clean quit (it reaches the teardown below),
    // so this background task only covers SIGINT/SIGTERM arriving from OUTSIDE
    // the TUI — e.g. `kill <makina-pid>`. On receipt it reaps every live agent
    // process group, restores the terminal, and exits, so an external signal can
    // never orphan a `grok`/agent subprocess. No-op on non-unix.
    exit::install_signal_reaper(tui::restore_terminal);

    // ── Event loop ────────────────────────────────────────────────────────────
    // `log_rx` (the tracing→TUI channel receiver) is threaded in so plan-0015's
    // `tui-error-pane-channel-wire` can drain it in the loop's `tokio::select!`.
    if let Err(e) = event::run(&mut tui, &mut app, log_rx).await {
        // No-orphan teardown (error arm): cancel any still-open runs, then reap
        // any agent process group still live, BEFORE restoring the terminal and
        // exiting — so an event-loop failure leaves nothing behind.
        exit::reap_open_runs(api.as_ref()).await;
        // Restore the terminal before printing the error, so the message is
        // visible even if raw mode was active.
        tui.restore();
        eprintln!("TUI error: {e}");
        std::process::exit(1);
    }

    // No-orphan teardown (clean arm): cancel any still-open runs, then reap any
    // agent process group still live, BEFORE the final terminal restore — so the
    // normal `q`/`Esc`/`Ctrl-C` quit never orphans an agent.
    exit::reap_open_runs(api.as_ref()).await;

    // tui.restore() is called by the Drop impl, but calling it explicitly here
    // ensures we exit the alternate screen before any post-main cleanup runs.
    tui.restore();
}
