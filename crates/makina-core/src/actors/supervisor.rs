//! Domain `Supervisor` actor — the coordination hub of the star topology.
//!
//! # Role
//!
//! The domain `Supervisor` is the **hub** of the star topology.  All other role
//! actors (Planner, Developer, Reviewer) are spokes that communicate only with
//! this hub — never with each other.
//!
//! Note: this is *not* [`crate::supervision::RootSupervisor`] (the fault-tolerance
//! root).  The `Supervisor` here is a domain-level coordinator that the
//! `RootSupervisor` manages as one of its supervised children.
//!
//! # Skeleton
//!
//! This is a **skeleton** implementation.  Real orchestration logic (dispatching
//! tasks to Developers, routing Reviewer verdicts, updating task states, advancing
//! the dependency graph) will be added in **task 21 (develop-review-loop)**.
//!
//! # Messages
//!
//! - [`SetTaskGraph`] — stores the current task graph; reply `()`.
//! - [`TaskGraphSnapshot`] — returns the current graph for introspection/testing.

use kameo::{
    actor::{ActorRef, Spawn},
    message::Context,
};

use crate::task::TaskGraph;

// ── Actor ─────────────────────────────────────────────────────────────────────

/// The domain coordination hub for the multi-agent pipeline.
///
/// Holds the active [`TaskGraph`] and will route work to Planner, Developer(s),
/// and Reviewer in later tasks.  Spawnable as a supervised child of
/// [`crate::supervision::RootSupervisor`].
pub struct Supervisor {
    /// The active task graph.  `None` until [`SetTaskGraph`] is received.
    ///
    /// Expanded in task 21: the Supervisor will drive the develop→review loop
    /// by reading task states from this graph and dispatching work to spoke actors.
    graph: Option<TaskGraph>,
}

impl kameo::actor::Actor for Supervisor {
    /// No initialisation arguments needed; the Supervisor starts empty.
    type Args = ();
    type Error = std::convert::Infallible;

    async fn on_start(_args: (), _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Supervisor { graph: None })
    }
}

impl Supervisor {
    /// Convenience: spawn a `Supervisor` without supervision (useful in tests).
    #[allow(dead_code)]
    pub fn start() -> ActorRef<Self> {
        Supervisor::spawn(())
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Store (or replace) the active [`TaskGraph`].
///
/// The Supervisor holds the graph as the source of truth for all scheduling
/// decisions.  Later tasks (e.g. task 14 — Planner actor, task 21 — orchestration
/// loop) will populate this graph after the Planner interprets the task list.
pub struct SetTaskGraph(pub TaskGraph);

impl kameo::message::Message<SetTaskGraph> for Supervisor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetTaskGraph,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.graph = Some(msg.0);
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Return a snapshot of the current task graph for introspection and testing.
///
/// Returns `None` if no graph has been set yet.
///
/// Note: a full read-back query (e.g. per-task status) will be added in task 21;
/// this message exists only so tests can verify [`SetTaskGraph`] was accepted.
pub struct TaskGraphSnapshot;

impl kameo::message::Message<TaskGraphSnapshot> for Supervisor {
    type Reply = Option<TaskGraph>;

    async fn handle(
        &mut self,
        _msg: TaskGraphSnapshot,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.graph.clone()
    }
}
