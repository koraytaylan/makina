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
pub mod dependency;
pub mod gate;
pub mod governance;
pub mod interpreter;
pub(crate) mod json;
pub mod log_record;
pub mod merge;
pub mod orchestrator;
pub mod paths;
pub mod persist;
pub mod roles;
pub mod run_metadata;
pub mod state_machine;
pub mod supervision;
pub mod task;
pub mod worktree;
