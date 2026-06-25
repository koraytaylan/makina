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

use makina::{app, event, exit, log, tui};
use makina_acp::AcpBackend;
use makina_core::audit::JsonlAuditSink;
use makina_core::backend::AgentBackend;
use makina_core::config::Config;
use makina_core::orchestrator::CoreApi;
use makina_core::preflight::probe_providers;
use makina_core::worktree::WorktreeManager;

#[tokio::main]
async fn main() {
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
    // `load_defaults_with_paths` returns `(Result<Config, ConfigError>, ConfigPaths)`
    // so the resolved paths are in hand even when loading fails — no need to
    // re-derive them in the error arm.
    let (load_result, load_paths) = Config::load_defaults_with_paths();
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
            std::process::exit(1);
        }
    };

    // ── Execution dependencies (task 31: run-control) ─────────────────────────
    // The orchestrator drives Runs (StartRun → Supervisor scheduler) using:
    //  - the agent BACKEND: the ACP CLI from `config.backend` (the e2e/task-33
    //    seam swaps this for any other `AgentBackend`; tests inject NoopBackend);
    //  - a WORKTREE MANAGER rooted at the repo (CWD) on `config.base_branch`;
    //  - the resolved CONFIG (gates, caps, concurrency).
    let repo_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // ── Audit sink (task supervisor-audit-writer) ─────────────────────────────
    // The `JsonlAuditSink` is the Supervisor-owned ledger writer.  A single
    // `Arc` is shared as both:
    //  - the `AuditSink` injected into the ACP backend (transport fires it on
    //    every permission decision), and
    //  - the `AuditRegistry` passed into the orchestrator so the Supervisor can
    //    register each task's worktree context before dispatching a driver.
    // This guarantees the sink is the ONLY writer under `.tasks/{slug}/audit.jsonl`.
    let audit_sink = Arc::new(JsonlAuditSink::new(repo_root.clone()));

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

    // ── Build one backend per declared provider ───────────────────────────────
    // Build a name → Arc<dyn AgentBackend> map from config.providers.
    // The shared audit sink is the same Arc injected into every backend so all
    // permission decisions are routed through the same JSONL ledger.
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

    /// Resolve the backend for a role from the provider map.
    ///
    /// Looks up the role's assignment provider name, falling back to "default"
    /// or the first configured provider, and finally to the legacy `[backend]`
    /// field for configs that predate named providers.
    fn resolve_role_backend(
        provider_name_opt: Option<&str>,
        provider_backends: &HashMap<String, Arc<dyn AgentBackend>>,
        first_provider_name: Option<&str>,
        legacy_backend: Arc<dyn AgentBackend>,
    ) -> Arc<dyn AgentBackend> {
        let name = provider_name_opt
            .or(first_provider_name)
            .unwrap_or("default");
        provider_backends
            .get(name)
            .cloned()
            .unwrap_or(legacy_backend)
    }

    // Build the legacy fallback backend (used only when no providers are configured).
    let legacy_backend: Arc<dyn AgentBackend> =
        Arc::new(
            AcpBackend::new(config.backend.command.clone(), config.backend.args.clone())
                .with_audit_sink(
                    Arc::clone(&audit_sink) as Arc<dyn makina_core::governance::AuditSink>
                ),
        );

    let first_provider_name = config.providers.first().map(|p| p.name.as_str());

    let developer_backend: Arc<dyn AgentBackend> = resolve_role_backend(
        config.roles.developer.as_ref().map(|a| a.provider.as_str()),
        &provider_backends,
        first_provider_name,
        Arc::clone(&legacy_backend),
    );

    let reviewer_backend: Arc<dyn AgentBackend> = resolve_role_backend(
        config.roles.reviewer.as_ref().map(|a| a.provider.as_str()),
        &provider_backends,
        first_provider_name,
        Arc::clone(&legacy_backend),
    );

    // The developer backend is also used for the planner (one-shot-agent path)
    // when no planner assignment is configured.
    let backend: Arc<dyn AgentBackend> = resolve_role_backend(
        config.roles.planner.as_ref().map(|a| a.provider.as_str()),
        &provider_backends,
        first_provider_name,
        Arc::clone(&legacy_backend),
    );
    let worktree_manager = WorktreeManager::new(repo_root.clone(), config.base_branch.clone());

    // ── Api ───────────────────────────────────────────────────────────────────
    // The real, core-backed orchestrator Api.  It opens Runs by reading a
    // task-list file and interpreting it via the deterministic
    // `StructuredTextInterpreter` + `EdgeInferrer` path (always, for TUI
    // OpenRun/ReinterpretRun responsiveness; we always use the deterministic
    // ingestion interpreter in the shipping binary). The model path selected by
    // `config.planner.mechanism` + ACP backend is reserved exclusively for the
    // Planner actor/spoke; ingestion in the TUI binary is never the model path.
    // (always use the deterministic path for TUI ingestion)
    // The run is then driven with the injected backend + worktree manager +
    // config (task 31).
    tracing::info!(
        "Using deterministic structured-text + edge inference for TUI OpenRun/ReinterpretRun (planner mechanism only affects the Planner actor)"
    );
    let ingestion_interpreter: Arc<dyn makina_core::interpreter::TaskListInterpreter> =
        Arc::new(makina_core::dependency::EdgeInferrer::new(Arc::new(
            makina_core::interpreter::StructuredTextInterpreter::new(),
        )));
    // INVARIANT: the ingestion interpreter used for OpenRun/ReinterpretRun in the
    // shipping TUI is *never* the model-backed interpreter.  All model use for task-list
    // interpretation goes through the Planner actor (build_planner_interpreter).
    // If you change this, update plan 0005 and the test that asserts the invariant.

    // Build a *separate* planner interpreter that *does* respect the configured
    // mechanism (may be model-backed).  This is passed through CoreApi state
    // into run_graph so the Planner actor (when spawned) uses the user's choice.
    // Ingestion stays det for TUI responsiveness.
    let planner_interpreter = match makina_core::interpreter::build_planner_interpreter(
        &config.planner.mechanism,
        Some(Arc::clone(&backend)),
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("planner mechanism unavailable; falling back to deterministic planner: {e}");
            Arc::new(makina_core::dependency::EdgeInferrer::new(Arc::new(
                makina_core::interpreter::StructuredTextInterpreter::new(),
            ))) as Arc<dyn makina_core::interpreter::TaskListInterpreter>
        }
    };

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

    // Clone providers, roles, caps, concurrency, and theme_name before config is moved into the API.
    let providers_for_app = config.providers.clone();
    let roles_for_app = config.roles.clone();
    let caps_for_app = config.caps.clone();
    let concurrency_for_app = config.concurrency;
    let theme_name_for_app = config.theme_name.clone();

    let api: Arc<dyn makina_core::api::Api> = Arc::new(CoreApi::with_audit_registry(
        ingestion_interpreter,
        planner_interpreter,
        developer_backend,
        reviewer_backend,
        worktree_manager,
        config,
        audit_sink as Arc<dyn makina_core::audit::AuditRegistry>,
    ));

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
