//! `Developer` actor — spoke that executes a single task in a worktree.
//!
//! # Role
//!
//! The `Developer` is a spoke in the star topology.  It holds a reference to the
//! domain [`Supervisor`] hub and communicates only with it.  Its job is to receive
//! a task assignment from the Supervisor, work on it inside a git worktree, run
//! quality gates, and report progress/completion back to the Supervisor.
//!
//! # Star topology
//!
//! The `Developer` holds an `ActorRef<Supervisor>` in its state — the **only**
//! actor ref it is allowed to hold.  Spoke-to-spoke communication is forbidden;
//! any cross-spoke coordination routes through the Supervisor.
//!
//! **Note on stale refs after hub restart**: `ActorRef<Supervisor>` is `Clone +
//! Send + Sync`, so it is a valid `Args` field and survives spoke-level restarts.
//! However, if the domain `Supervisor` itself is restarted by the
//! `RootSupervisor`, all spokes will hold a stale ref.  Resolving this is
//! deferred to a later fault-tolerance task.
//!
//! # Skeleton
//!
//! This is a **skeleton** implementation.  Real development work (model calls,
//! git worktree management, quality-gate integration, Supervisor progress
//! reporting) is added in **task 21 (develop-review-loop)** and related tasks.
//! The handler here simply returns a placeholder ack.
//!
//! # Messages
//!
//! - [`Develop`] — skeleton handler; real work is task 21.

use std::path::PathBuf;

use kameo::actor::ActorRef;

use crate::task::Task;

use super::supervisor::Supervisor;

// ── Actor ─────────────────────────────────────────────────────────────────────

/// Spoke actor that implements a single task inside a git worktree.
///
/// Spawnable as a supervised child of [`crate::supervision::RootSupervisor`].
/// The `Supervisor` ref is passed via [`DeveloperArgs`] and stored for outbound
/// messages to the hub.
///
/// The system may spawn multiple Developer instances in parallel (one per
/// concurrent task slot); each holds the same Supervisor ref.  Concurrency
/// management is handled by a later task (task 24 — concurrency).
pub struct Developer {
    /// Reference to the domain Supervisor hub.
    ///
    /// All progress and completion reports go through this ref.
    ///
    /// Task 21 will use this ref to push `GatesPassed` / `GateFailed` / `HardError`
    /// events; currently unused in the skeleton handler.
    #[allow(dead_code)]
    supervisor: ActorRef<Supervisor>,
}

/// Construction arguments for [`Developer`].
///
/// `ActorRef<Supervisor>` is `Clone + Send + Sync`, satisfying the
/// `C::Args: Clone + Sync` bound required by
/// [`crate::supervision::RootSupervisor::spawn_child`].
#[derive(Clone)]
pub struct DeveloperArgs {
    /// The domain Supervisor hub this Developer will report to.
    pub supervisor: ActorRef<Supervisor>,
}

impl kameo::actor::Actor for Developer {
    type Args = DeveloperArgs;
    type Error = std::convert::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Developer {
            supervisor: args.supervisor,
        })
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Instruct the Developer to work on `task` inside the given `worktree`.
///
/// # Skeleton behaviour
///
/// This handler is a placeholder.  The real implementation (task 21) will:
/// 1. Check out the task branch inside `worktree` (via worktree-manager, task 20).
/// 2. Invoke the model to generate code changes.
/// 3. Run quality gates (gate-runner, task 22).
/// 4. Report `GatesPassed` / `GateFailed` / `HardError` events to the Supervisor.
///
/// For now the handler simply returns `Ok(())`.
pub struct Develop {
    /// The task to implement.
    pub task: Task,
    /// Path to the git worktree where work should be done.
    pub worktree: PathBuf,
}

/// Placeholder acknowledgement returned by the skeleton [`Develop`] handler.
///
/// Task 21 will replace or augment this with a richer result type that carries
/// gate-runner output, iteration counts, and the final task state.
pub type DevelopAck = Result<(), String>;

impl kameo::message::Message<Develop> for Developer {
    type Reply = DevelopAck;

    async fn handle(
        &mut self,
        _msg: Develop,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // Skeleton: return placeholder ack.
        // Task 21 will perform real development work and push progress events to
        // `self.supervisor`.
        Ok(())
    }
}
