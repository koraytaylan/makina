//! `Planner` actor — spoke that interprets the task list and populates the graph.
//!
//! # Role
//!
//! The `Planner` is a spoke in the star topology.  It holds a reference to the
//! domain [`Supervisor`] hub and communicates only with it.  Its job is to read a
//! task-list document (e.g. a Markdown file), interpret it via a model, and send
//! the resulting [`TaskGraph`] to the Supervisor via [`SetTaskGraph`].
//!
//! # Star topology
//!
//! The `Planner` holds an `ActorRef<Supervisor>` in its state — this is the
//! **only** actor ref it is allowed to hold.  If the Planner ever needed to talk
//! to a Developer or Reviewer it would do so by messaging the Supervisor first.
//!
//! **Note on stale refs after hub restart**: `ActorRef<Supervisor>` is `Clone +
//! Send + Sync`, so it is a valid `Args` field and survives spoke-level restarts.
//! However, if the domain `Supervisor` itself is restarted by the
//! `RootSupervisor`, all spokes will hold a stale ref pointing to the dead actor.
//! Resolving this (e.g. via a name-registry or dynamic re-wiring) is a
//! fault-tolerance concern deferred to a later task.
//!
//! # Skeleton
//!
//! This is a **skeleton** implementation.  Real task-list interpretation (model
//! call, Markdown parsing, dependency detection, section assignment) is added in
//! **task 14 (planner-actor)**.  The handler here returns a placeholder ack
//! without calling any model or doing any I/O.
//!
//! # Messages
//!
//! - [`InterpretTaskList`] — skeleton handler; real work is task 14.

use std::path::PathBuf;

use kameo::actor::ActorRef;

use super::supervisor::Supervisor;

// ── Actor ─────────────────────────────────────────────────────────────────────

/// Spoke actor that interprets the task list and sends the resulting graph to the
/// domain Supervisor.
///
/// Spawnable as a supervised child of [`crate::supervision::RootSupervisor`].
/// The `Supervisor` ref is passed via [`PlannerArgs`] and stored for outbound
/// messages to the hub.
pub struct Planner {
    /// Reference to the domain Supervisor hub.
    ///
    /// All outbound Planner messages go through this ref.  See the module doc for
    /// the stale-ref caveat that applies when the hub restarts.
    ///
    /// Task 14 will use this ref to push the resulting `TaskGraph` via
    /// `SetTaskGraph`; currently unused in the skeleton handler.
    #[allow(dead_code)]
    supervisor: ActorRef<Supervisor>,
}

/// Construction arguments for [`Planner`].
///
/// `ActorRef<Supervisor>` is `Clone + Send + Sync`, satisfying the
/// `C::Args: Clone + Sync` bound required by
/// [`crate::supervision::RootSupervisor::spawn_child`].
#[derive(Clone)]
pub struct PlannerArgs {
    /// The domain Supervisor hub this Planner will report to.
    pub supervisor: ActorRef<Supervisor>,
}

impl kameo::actor::Actor for Planner {
    type Args = PlannerArgs;
    type Error = std::convert::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Planner {
            supervisor: args.supervisor,
        })
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Instruct the Planner to interpret the task-list document at `path`.
///
/// # Skeleton behaviour
///
/// This handler is a placeholder.  The real implementation (task 14) will:
/// 1. Read and parse the Markdown task-list file at `path`.
/// 2. Call the configured model to interpret task titles, descriptions, and
///    dependencies.
/// 3. Assign section/wave labels for parallel scheduling.
/// 4. Send the resulting [`TaskGraph`] to the Supervisor via `SetTaskGraph`.
///
/// For now the handler simply returns `Ok(())` without touching the file or the
/// model.
pub struct InterpretTaskList {
    /// Path to the task-list document (e.g. a `.tasks/` Markdown file).
    pub path: PathBuf,
}

/// Placeholder acknowledgement returned by the skeleton [`InterpretTaskList`]
/// handler.
///
/// Task 14 will expand this to a richer type (or use the `Supervisor` push
/// pattern instead of a reply) once the full Planner protocol is designed.
pub type InterpretTaskListAck = Result<(), String>;

impl kameo::message::Message<InterpretTaskList> for Planner {
    type Reply = InterpretTaskListAck;

    async fn handle(
        &mut self,
        _msg: InterpretTaskList,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // Skeleton: return placeholder ack.
        // Task 14 will read `_msg.path`, call the model, and push the resulting
        // TaskGraph to `self.supervisor` via `SetTaskGraph`.
        Ok(())
    }
}
