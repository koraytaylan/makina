//! Makina core library.
//!
//! This crate provides all orchestration logic for the Makina multi-agent
//! software-factory: actor topology, task lifecycle/state machine, worktree
//! manager, config loading, the agent-backend trait, and the
//! `makina_core::api` command/query/event surface consumed by the TUI.

pub mod actors;
pub mod api;
pub mod audit;
pub mod backend;
pub mod config;
pub mod constants;
pub mod dependency;
pub mod discovery;
pub mod gate;
pub mod governance;
pub mod ingestion;
pub mod interpreter;
pub(crate) mod json;
pub mod log_record;
pub mod merge;
pub mod normalizer;
pub mod orchestrator;
pub mod paths;
pub mod persist;
pub mod preflight;
pub mod roles;
pub mod run_metadata;
pub mod state_machine;
pub mod supervision;
pub mod task;
pub mod worktree;

/// Process-global serialisation lock for tests that mutate the `HOME` env var.
///
/// `HOME` is process-global state; tests that call `paths::state_root` (or any
/// helper built on it) must hold this lock **for the entire duration of the test**
/// so that concurrent tests do not observe an inconsistent `HOME`.
///
/// This is a `tokio::sync::Mutex` so that async tests can hold the guard across
/// `.await` points without triggering the `clippy::await_holding_lock` lint.
/// Sync tests should call `.blocking_lock()`.
///
/// Available in all in-process test binaries (integration tests and unit tests)
/// via `makina_core::HOME_ENV_LOCK`.  Not gated behind `#[cfg(test)]` so that
/// downstream crates (e.g. `makina`) can import it in their own test modules
/// without a `test-helpers` feature.
pub static HOME_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
