//! `Reviewer` actor — spoke that evaluates a Developer's output.
//!
//! # Role
//!
//! The `Reviewer` is a spoke in the star topology.  It holds a reference to the
//! domain [`Supervisor`] hub and communicates only with it.  Its job is to
//! receive a review assignment from the Supervisor, evaluate the Developer's
//! changes, and return a [`ReviewVerdict`] indicating approval or rejection with
//! feedback.
//!
//! # Star topology
//!
//! The `Reviewer` holds an `ActorRef<Supervisor>` in its state — the **only**
//! actor ref it is allowed to hold.  Reviewer-to-Developer communication is
//! forbidden; all coordination routes through the Supervisor.
//!
//! **Note on stale refs after hub restart**: `ActorRef<Supervisor>` is `Clone +
//! Send + Sync`, so it is a valid `Args` field and survives spoke-level restarts.
//! However, if the domain `Supervisor` itself is restarted by the
//! `RootSupervisor`, all spokes will hold a stale ref.  Resolving this is
//! deferred to a later fault-tolerance task.
//!
//! # Skeleton
//!
//! This is a **skeleton** implementation.  Real review logic (model call,
//! diff analysis, acceptance-criteria checking, feedback generation) is added in
//! **task 21 (develop-review-loop)**.  The handler here returns a placeholder
//! `ReviewVerdict::Approve` without calling any model.
//!
//! # Messages
//!
//! - [`Review`] — skeleton handler; real work is task 21.

use std::path::PathBuf;

use kameo::actor::ActorRef;

use crate::task::Task;

use super::supervisor::Supervisor;

// ── ReviewVerdict ─────────────────────────────────────────────────────────────

/// The Reviewer's verdict on a Developer's output.
///
/// Returned by the [`Review`] message.  The Supervisor uses this verdict to
/// drive the FSM (task 21 will call [`crate::state_machine::transition`] with
/// `ReviewerApproved` or `ReviewerRejected`).
#[derive(Debug, Clone, PartialEq)]
pub enum ReviewVerdict {
    /// The output meets the acceptance criteria; the task should advance to Done.
    Approve,

    /// The output does not meet the acceptance criteria; the Developer should
    /// iterate.  The `feedback` string will be passed to the Developer in the
    /// next iteration (task 21).
    Reject {
        /// Human-readable explanation of what must change.
        feedback: String,
    },
}

// ── Actor ─────────────────────────────────────────────────────────────────────

/// Spoke actor that evaluates a Developer's output and returns a
/// [`ReviewVerdict`].
///
/// Spawnable as a supervised child of [`crate::supervision::RootSupervisor`].
/// The `Supervisor` ref is passed via [`ReviewerArgs`] and stored for outbound
/// messages to the hub.
pub struct Reviewer {
    /// Reference to the domain Supervisor hub.
    ///
    /// All verdict reports and review-cap events go through this ref.
    ///
    /// Task 21 will use this ref to push `ReviewerApproved` / `ReviewerRejected` /
    /// `ReviewCapReached` events; currently unused in the skeleton handler.
    #[allow(dead_code)]
    supervisor: ActorRef<Supervisor>,
}

/// Construction arguments for [`Reviewer`].
///
/// `ActorRef<Supervisor>` is `Clone + Send + Sync`, satisfying the
/// `C::Args: Clone + Sync` bound required by
/// [`crate::supervision::RootSupervisor::spawn_child`].
#[derive(Clone)]
pub struct ReviewerArgs {
    /// The domain Supervisor hub this Reviewer will report to.
    pub supervisor: ActorRef<Supervisor>,
}

impl kameo::actor::Actor for Reviewer {
    type Args = ReviewerArgs;
    type Error = std::convert::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Reviewer {
            supervisor: args.supervisor,
        })
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Instruct the Reviewer to evaluate the Developer's output for `task` inside
/// the given `worktree`.
///
/// # Skeleton behaviour
///
/// This handler is a placeholder.  The real implementation (task 21) will:
/// 1. Read the diff / changed files from `worktree`.
/// 2. Call the model with the task's `done_when` acceptance criterion and the
///    diff.
/// 3. Return `ReviewVerdict::Approve` or `ReviewVerdict::Reject { feedback }`.
/// 4. Optionally push review-cap events to the Supervisor.
///
/// For now the handler returns `ReviewVerdict::Approve` unconditionally.
pub struct Review {
    /// The task being reviewed.
    pub task: Task,
    /// Path to the git worktree containing the Developer's output.
    pub worktree: PathBuf,
}

impl kameo::message::Message<Review> for Reviewer {
    /// Wrapped in `Result` to satisfy kameo's `Reply` bound.
    ///
    /// `ReviewVerdict` itself does not implement `Reply` (kameo's trait is not
    /// automatically derived for custom types).  Wrapping in `Result<ReviewVerdict, String>`
    /// leverages the blanket `impl Reply for Result<T, E>` already provided by
    /// kameo.  The `Err` arm is reserved for handler-level errors (e.g. model
    /// unavailable); in this skeleton it is never used.
    type Reply = Result<ReviewVerdict, String>;

    async fn handle(
        &mut self,
        _msg: Review,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // Skeleton: return placeholder approval.
        // Task 21 will perform real review via model call and return the actual
        // verdict.  The Supervisor will then use `transition()` to advance task
        // state accordingly.
        Ok(ReviewVerdict::Approve)
    }
}
