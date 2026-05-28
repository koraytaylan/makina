//! `Planner` actor — spoke that interprets the task list and populates the graph.
//!
//! # Role
//!
//! The `Planner` is a spoke in the star topology.  It holds a reference to the
//! domain [`Supervisor`] hub and communicates only with it.  Its job is to read a
//! task-list document (a Markdown file following the structured-text convention),
//! interpret it via an injected [`TaskListInterpreter`], and send the resulting
//! [`TaskGraph`] to the Supervisor via [`SetTaskGraph`].
//!
//! # Interpreter injection
//!
//! The interpreter is passed as `Arc<dyn TaskListInterpreter>` in [`PlannerArgs`].
//! This seam exists so that:
//! - Tests and offline environments use [`StructuredTextInterpreter`] (deterministic,
//!   no network, no model call).
//! - Task 18 (`planner-model-mechanism`) will add a model-backed interpreter behind
//!   the same trait and wire it in at the call site without touching this actor.
//! - Task 17 (`dependency-detection`) will augment the graph with inferred edges by
//!   decorating the injected interpreter.
//!
//! # Star topology
//!
//! The `Planner` holds an `ActorRef<Supervisor>` in its state — the **only**
//! actor ref it is allowed to hold.  If the Planner ever needed to talk to a
//! Developer or Reviewer it would do so by messaging the Supervisor first.
//!
//! **Note on stale refs after hub restart**: `ActorRef<Supervisor>` is `Clone +
//! Send + Sync`, so it is a valid `Args` field and survives spoke-level restarts.
//! However, if the domain `Supervisor` itself is restarted by the
//! `RootSupervisor`, all spokes will hold a stale ref pointing to the dead actor.
//! Resolving this (e.g. via a name-registry or dynamic re-wiring) is a
//! fault-tolerance concern deferred to a later task.
//!
//! # Messages
//!
//! - [`InterpretTaskList`] — read the file at `path`, call
//!   `interpreter.interpret(slug, text)`, and push the resulting graph to the
//!   Supervisor via [`SetTaskGraph`].
//!
//! [`TaskListInterpreter`]: crate::interpreter::TaskListInterpreter
//! [`StructuredTextInterpreter`]: crate::interpreter::StructuredTextInterpreter

use std::path::PathBuf;
use std::sync::Arc;

use kameo::actor::ActorRef;

use crate::interpreter::TaskListInterpreter;

use super::supervisor::{SetTaskGraph, Supervisor};

// ── Actor ─────────────────────────────────────────────────────────────────────

/// Spoke actor that interprets the task list and sends the resulting graph to the
/// domain Supervisor.
///
/// Spawnable as a supervised child of [`crate::supervision::RootSupervisor`].
/// The `Supervisor` ref and the [`TaskListInterpreter`] are passed via
/// [`PlannerArgs`].
pub struct Planner {
    /// Reference to the domain Supervisor hub.
    ///
    /// All outbound Planner messages go through this ref.  See the module doc for
    /// the stale-ref caveat that applies when the hub restarts.
    supervisor: ActorRef<Supervisor>,

    /// The injected interpreter used to convert task-list text into a
    /// [`TaskGraph`].
    ///
    /// Task 18 (`planner-model-mechanism`) will provide a model-backed
    /// implementation; task 17 (`dependency-detection`) will add a decorator that
    /// infers additional edges.  Both hook in here without changing the actor.
    interpreter: Arc<dyn TaskListInterpreter>,
}

/// Construction arguments for [`Planner`].
///
/// Both fields must be `Clone + Sync`:
/// - `ActorRef<Supervisor>` is `Clone + Send + Sync` by design.
/// - `Arc<dyn TaskListInterpreter>` is `Clone` (reference-counted pointer) and
///   `Sync` because the trait bound includes `Send + Sync`.
///
/// This satisfies the `C::Args: Clone + Sync` bound required by
/// [`crate::supervision::RootSupervisor::spawn_child`].
#[derive(Clone)]
pub struct PlannerArgs {
    /// The domain Supervisor hub this Planner will report to.
    pub supervisor: ActorRef<Supervisor>,

    /// The interpreter used to parse structured-text task lists into a
    /// [`TaskGraph`].
    ///
    /// Inject [`StructuredTextInterpreter`] for deterministic parsing (tests,
    /// offline), or a model-backed interpreter (task 18) for production use.
    ///
    /// [`StructuredTextInterpreter`]: crate::interpreter::StructuredTextInterpreter
    pub interpreter: Arc<dyn TaskListInterpreter>,
}

impl kameo::actor::Actor for Planner {
    type Args = PlannerArgs;
    type Error = std::convert::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Planner {
            supervisor: args.supervisor,
            interpreter: args.interpreter,
        })
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Instruct the Planner to interpret the task-list document at `path`.
///
/// # Behaviour
///
/// 1. Read the file at `path` using `tokio::fs::read_to_string`.
/// 2. Derive a `slug` from the file stem (e.g. `"TASKS"` from `TASKS.md`).
/// 3. Call `self.interpreter.interpret(slug, &text)`.
/// 4. On success: send the resulting [`TaskGraph`] to the Supervisor via
///    [`SetTaskGraph`], then return `Ok(())`.
/// 5. On any error (I/O, parse, validation): return `Err(message)` without
///    panicking.
///
/// # Error handling
///
/// All failures are returned as `Err(String)` in the reply; the actor does not
/// panic.  Callers can inspect the error and decide whether to retry.
pub struct InterpretTaskList {
    /// Path to the task-list document (e.g. a `.tasks/` Markdown file).
    pub path: PathBuf,
}

/// Reply returned by the [`InterpretTaskList`] handler.
///
/// `Ok(())` means the graph was successfully interpreted and handed to the
/// Supervisor.  `Err(String)` carries a human-readable error description
/// (I/O failure, parse error, or validation error).
pub type InterpretTaskListAck = Result<(), String>;

impl kameo::message::Message<InterpretTaskList> for Planner {
    type Reply = InterpretTaskListAck;

    async fn handle(
        &mut self,
        msg: InterpretTaskList,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // Step 1: derive slug from the file stem.
        let slug = msg
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("tasks")
            .to_string();

        // Step 2: read the file.
        // We use the synchronous `std::fs::read_to_string` here because `tokio`
        // is compiled without the `fs` feature in this workspace.  Task list files
        // are small (< 100 KB in practice) so a blocking read does not block the
        // executor for a meaningful duration.  If this becomes a concern, enabling
        // `tokio/fs` and switching to `tokio::fs::read_to_string` is a one-line
        // change.
        let text = std::fs::read_to_string(&msg.path)
            .map_err(|e| format!("failed to read `{}`: {e}", msg.path.display()))?;

        // Step 3: interpret.
        let graph = self
            .interpreter
            .interpret(&slug, &text)
            .await
            .map_err(|e| format!("interpretation failed: {e}"))?;

        // Step 4: hand the graph to the Supervisor.
        self.supervisor
            .ask(SetTaskGraph(graph))
            .send()
            .await
            .map_err(|e| format!("failed to deliver graph to Supervisor: {e}"))?;

        Ok(())
    }
}
