//! Makina — multi-agent software-factory orchestrator.
//!
//! # Crate layout
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | `main` | Wires the components; no logic. |
//! | `tui` | Terminal lifecycle: raw mode, alternate screen, panic hook. |
//! | `event` | Async event loop: merges terminal input, periodic tick, and api events. |
//! | `app` | All TUI state + pure `update(AppEvent)` function. |
//! | `browser` | File-browser view state + pure navigation (no IO). |
//! | `ui` | Pure rendering: `App` → `Frame` (uses `ratatui::TestBackend` in tests). |
//! | `placeholder` | **test-only** `Api` double (`#[cfg(test)]`); the binary uses the real [`makina_core::orchestrator::CoreApi`]. |
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

mod app;
mod browser;
mod event;
#[cfg(test)]
mod placeholder;
mod tui;
mod ui;

use std::sync::Arc;

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
    // task-list file and interpreting it with the DETERMINISTIC interpreter
    // (`StructuredTextInterpreter`) wrapped in the cross-cutting `EdgeInferrer`
    // decorator — so the TUI can open runs with no model/auth — then drives them
    // with the injected backend + worktree manager + config (task 31).
    //
    // Seam for the e2e (task 33): swap `StructuredTextInterpreter` for a
    // `ModelInterpreter` over the ACP backend to get model-backed planning.
    let interpreter = Arc::new(EdgeInferrer::new(
        Arc::new(StructuredTextInterpreter::new()),
    ));
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

    // ── Event loop ────────────────────────────────────────────────────────────
    if let Err(e) = event::run(&mut tui, &mut app).await {
        // Restore the terminal before printing the error, so the message is
        // visible even if raw mode was active.
        tui.restore();
        eprintln!("TUI error: {e}");
        std::process::exit(1);
    }

    // tui.restore() is called by the Drop impl, but calling it explicitly here
    // ensures we exit the alternate screen before any post-main cleanup runs.
    tui.restore();
}
