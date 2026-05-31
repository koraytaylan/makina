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

use std::sync::Arc;

use makina::{app, event, exit, log, tui};
use makina_acp::AcpBackend;
use makina_core::audit::JsonlAuditSink;
use makina_core::backend::AgentBackend;
use makina_core::config::Config;
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::StructuredTextInterpreter;
use makina_core::orchestrator::CoreApi;
use makina_core::worktree::WorktreeManager;

#[tokio::main]
async fn main() {
    // ── Config ──────────────────────────────────────────────────────────────────
    // Load the resolved two-layer config (global ~/.makina/config.toml + project
    // ./makina.toml).  Supplies the agent backend command, the gates, the caps,
    // the concurrency limit, and the base branch the orchestrator drives runs
    // with.  A load/validation failure is fatal (we cannot run without it).
    let config = match Config::load_defaults() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load configuration: {e}");
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

    let backend: Arc<dyn AgentBackend> =
        Arc::new(
            AcpBackend::new(config.backend.command.clone(), config.backend.args.clone())
                .with_audit_sink(
                    Arc::clone(&audit_sink) as Arc<dyn makina_core::governance::AuditSink>
                ),
        );
    let worktree_manager = WorktreeManager::new(repo_root, config.base_branch.clone());

    // ── Api ───────────────────────────────────────────────────────────────────
    // The real, core-backed orchestrator Api.  It opens Runs by reading a
    // task-list file and interpreting it via `build_ingestion_interpreter`
    // (the **model** interpreter is now the default per `config.planner.mechanism`
    // + ACP backend; deterministic `StructuredTextInterpreter` + `EdgeInferrer`
    // is the offline/`None`-backend fallback) — then drives them with the
    // injected backend + worktree manager + config (task 31).
    let interpreter = match makina_core::interpreter::build_ingestion_interpreter(
        &config.planner.mechanism,
        Some(Arc::clone(&backend)),
    ) {
        Ok(i) => i,
        Err(e) => {
            eprintln!(
                "planner mechanism unavailable; falling back to deterministic interpreter: {e}"
            );
            Arc::new(EdgeInferrer::new(
                Arc::new(StructuredTextInterpreter::new()),
            )) as Arc<dyn makina_core::interpreter::TaskListInterpreter>
        }
    };
    let api: Arc<dyn makina_core::api::Api> = Arc::new(CoreApi::with_audit_registry(
        interpreter,
        backend,
        worktree_manager,
        config,
        audit_sink as Arc<dyn makina_core::audit::AuditRegistry>,
    ));

    // ── Initial state ─────────────────────────────────────────────────────────
    let initial_runs = api.runs().await;
    let mut app = app::App::new(Arc::clone(&api), initial_runs);

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
