//! Makina — multi-agent software-factory orchestrator (library crate).
//!
//! # Crate layout
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | `tui` | Terminal lifecycle: raw mode, alternate screen, panic hook. |
//! | `event` | Async event loop: merges terminal input, periodic tick, and api events. |
//! | `app` | All TUI state + pure `update(AppEvent)` function. |
//! | `browser` | File-browser view state + pure navigation (no IO). |
//! | `ui` | Pure rendering: `App` → `Frame` (uses `ratatui::TestBackend` in tests). |
//! | `log` | Tracing-subscriber layers: per-run **file** layer (span-keyed routing) + a **TUI-channel** layer (`try_send` to a bounded mpsc). |
//! | `placeholder` | **test-only** `Api` double (`#[cfg(test)]`); the binary uses the real [`makina_core::orchestrator::CoreApi`]. |
//!
//! The crate exposes a thin library so integration tests (`tests/*.rs`) can
//! exercise individual modules — notably [`log::RunFileLayer`] — while the
//! `makina` binary (`src/main.rs`) wires the components together.
//!
//! # Architecture
//!
//! The TUI is **presentation only**.  It consumes `makina-core::api::Api` via
//! `Arc<dyn Api>` and holds **no orchestration logic**.  State changes arrive
//! through `api.subscribe()` (pushed) and initial snapshots are fetched from
//! `api.runs()` (pulled once at startup).

pub mod ansi;
pub mod app;
pub mod browser;
pub mod cli;
pub mod event;
pub mod exit;
pub mod folder_init;
pub mod log;
pub mod markup;
#[cfg(test)]
pub mod placeholder;
pub mod replay;
pub mod selection;
pub mod settings_validation;
pub mod syntax;
pub mod theme;
pub mod tui;
pub mod ui;
pub mod workspace;
