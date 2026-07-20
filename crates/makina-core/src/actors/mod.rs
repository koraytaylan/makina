//! Async orchestration components for the Makina multi-agent pipeline.
//!
//! The original implementation exposed these pieces as Kameo actors. The engine
//! now drives the same planner, developer, reviewer, and scheduler behavior
//! directly on Tokio futures.

pub mod agent_turn;
pub mod developer;
pub mod reviewer;
pub mod supervisor;

pub use developer::{Develop, DevelopAck, DevelopOutcome, develop};
pub use reviewer::{Review, ReviewReply, ReviewVerdict, review};
pub use supervisor::{EventSink, RunControl, RunReport, run_graph};
