//! Makina core library.
//!
//! This crate provides all orchestration logic for the Makina multi-agent
//! software-factory: actor topology, task lifecycle/state machine, worktree
//! manager, config loading, the agent-backend trait, and the
//! `makina_core::api` command/query/event surface consumed by the TUI.

pub mod actors;
pub mod api;
pub mod backend;
pub mod config;
pub mod interpreter;
pub mod state_machine;
pub mod supervision;
pub mod task;
