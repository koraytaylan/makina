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
//! | `ui` | Pure rendering: `App` → `Frame` (uses `ratatui::TestBackend` in tests). |
//! | `placeholder` | **PLACEHOLDER** `Api` impl used until the real core-backed Api is wired (task 33 / e2e). |
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
mod event;
mod placeholder;
mod tui;
mod ui;

use std::sync::Arc;

use placeholder::PlaceholderApi;

#[tokio::main]
async fn main() {
    // ── Api ───────────────────────────────────────────────────────────────────
    // TODO (task 33 / e2e): replace PlaceholderApi with the real core-backed Api.
    // The rest of main.rs is untouched; the Api trait is the only seam.
    let api: Arc<dyn makina_core::api::Api> = Arc::new(PlaceholderApi::new());

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
