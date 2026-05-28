//! Actor skeletons for the Makina multi-agent pipeline.
//!
//! # Star topology
//!
//! ```text
//! RootSupervisor  (fault-tolerance root — crate::supervision)
//!   └─ Supervisor  (domain hub — this module)
//!        ├─ Planner    (spoke — interprets task list)
//!        ├─ Developer  (spoke — implements tasks)
//!        └─ Reviewer   (spoke — evaluates Developer output)
//! ```
//!
//! All spokes hold an `ActorRef<Supervisor>` and communicate **only** with the
//! hub — never with each other.  This star constraint is enforced by the types:
//! each spoke's `Args` struct contains only the hub ref, not any other spoke ref.
//!
//! # Supervision wiring
//!
//! All four actors are spawnable as supervised children of
//! [`crate::supervision::RootSupervisor`] via
//! [`crate::supervision::RootSupervisor::spawn_child`].  Spawn order matters:
//! spawn the `Supervisor` hub first, then pass its `ActorRef` into each spoke's
//! `Args`.
//!
//! # Planner interpreter injection
//!
//! Task 14 (`planner-actor`) replaced the skeleton `Planner` handler with a
//! real implementation.  The interpreter is injected via [`PlannerArgs`] as an
//! `Arc<dyn TaskListInterpreter>`.  Use
//! [`StructuredTextInterpreter`](crate::interpreter::StructuredTextInterpreter)
//! in tests; task 18 will plug in a model-backed interpreter.
//!
//! Task 21 (`develop-review-loop`) will add real orchestration to `Supervisor`,
//! `Developer`, and `Reviewer`.

pub mod developer;
pub mod planner;
pub mod reviewer;
pub mod supervisor;

// Convenience re-exports so callers can use `actors::Supervisor` etc.
pub use developer::{Develop, DevelopAck, Developer, DeveloperArgs};
pub use planner::{InterpretTaskList, InterpretTaskListAck, Planner, PlannerArgs};
pub use reviewer::{Review, ReviewVerdict, Reviewer, ReviewerArgs};
pub use supervisor::{SetTaskGraph, Supervisor, TaskGraphSnapshot};

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Integration tests proving that each actor spawns under `RootSupervisor` and
    //! accepts its message types.
    //!
    //! # Determinism guarantee
    //!
    //! All assertions use `ask(...).send().await` (request/reply semantics) which
    //! awaits full processing of the message before returning.  No `sleep` is needed
    //! — the test cannot observe a reply before the handler runs, so the ordering is
    //! deterministic.

    use std::{path::PathBuf, sync::Arc};

    use chrono::Utc;

    use crate::{
        actors::{
            Develop, Developer, DeveloperArgs, InterpretTaskList, Planner, PlannerArgs, Review,
            ReviewVerdict, Reviewer, ReviewerArgs, SetTaskGraph, Supervisor, TaskGraphSnapshot,
        },
        interpreter::StructuredTextInterpreter,
        supervision::{RestartConfig, RootSupervisor},
        task::{Task, TaskGraph, TaskId, TaskState},
    };

    // ── Helper ────────────────────────────────────────────────────────────────

    /// Build a minimal [`TaskGraph`] with one task for use in assertions.
    fn minimal_graph() -> TaskGraph {
        let now = Utc::now();
        TaskGraph {
            slug: "test-graph".to_string(),
            tasks: vec![Task {
                id: TaskId::new("test-task"),
                title: "Test task".to_string(),
                description: "A task used only in tests.".to_string(),
                done_when: "The test passes.".to_string(),
                depends_on: vec![],
                section: None,
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                created_at: now,
                updated_at: now,
                started_at: None,
                finished_at: None,
            }],
        }
    }

    // ── Main integration test ─────────────────────────────────────────────────

    /// Spawn all four actors under `RootSupervisor` and send each its full set of
    /// messages, asserting meaningful replies.
    ///
    /// # Wiring order
    ///
    /// 1. Spawn `RootSupervisor` (fault-tolerance root).
    /// 2. Spawn `Supervisor` (domain hub) as supervised child.
    /// 3. Spawn `Planner`, `Developer`, `Reviewer` (spokes) with the hub's ref.
    ///
    /// # Assertions
    ///
    /// - `Supervisor`: `SetTaskGraph` accepted; `TaskGraphSnapshot` returns the
    ///   stored graph with the correct task count.
    /// - `Planner`: `InterpretTaskList` returns `Ok(())`.
    /// - `Developer`: `Develop` returns `Ok(())`.
    /// - `Reviewer`: `Review` returns `Ok(ReviewVerdict::Approve)`.
    #[tokio::test]
    async fn all_actors_spawn_and_accept_messages_under_root_supervisor() {
        // ── Step 1: start fault-tolerance root ───────────────────────────────
        let root = RootSupervisor::start();

        // ── Step 2: spawn domain Supervisor hub ──────────────────────────────
        let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
            &root,
            (), // Supervisor::Args = ()
            RestartConfig::default(),
        )
        .await;

        // ── Step 3: spawn spokes with the hub ref ────────────────────────────

        // Planner — inject the deterministic reference interpreter (no model call).
        let planner_ref = RootSupervisor::spawn_child::<Planner>(
            &root,
            PlannerArgs {
                supervisor: supervisor_ref.clone(),
                interpreter: Arc::new(StructuredTextInterpreter::new()),
            },
            RestartConfig::default(),
        )
        .await;

        // Developer
        let developer_ref = RootSupervisor::spawn_child::<Developer>(
            &root,
            DeveloperArgs {
                supervisor: supervisor_ref.clone(),
            },
            RestartConfig::default(),
        )
        .await;

        // Reviewer
        let reviewer_ref = RootSupervisor::spawn_child::<Reviewer>(
            &root,
            ReviewerArgs {
                supervisor: supervisor_ref.clone(),
            },
            RestartConfig::default(),
        )
        .await;

        // ── Step 4: exercise Supervisor messages ──────────────────────────────

        let graph = minimal_graph();

        // SetTaskGraph: store the graph; reply is ()
        supervisor_ref
            .ask(SetTaskGraph(graph.clone()))
            .send()
            .await
            .expect("SetTaskGraph must be accepted");

        // TaskGraphSnapshot: read back the stored graph and verify task count.
        let snapshot = supervisor_ref
            .ask(TaskGraphSnapshot)
            .send()
            .await
            .expect("TaskGraphSnapshot must be accepted");

        let stored = snapshot.expect("graph should be Some after SetTaskGraph");
        assert_eq!(
            stored.tasks.len(),
            1,
            "stored graph should contain exactly 1 task"
        );
        assert_eq!(
            stored.slug, graph.slug,
            "stored graph slug should match the submitted slug"
        );

        // ── Step 5: exercise Planner message ─────────────────────────────────

        // `ask().send()` for a `Result<T, E>` reply unwraps the kameo SendError
        // and returns `Result<T, SendError<M, E>>` — so `.expect()` here gives
        // `()` (the inner Ok value) directly.
        planner_ref
            .ask(InterpretTaskList {
                path: PathBuf::from("/dev/null"), // placeholder path; handler ignores it
            })
            .send()
            .await
            .expect("InterpretTaskList skeleton must return Ok(())");

        // ── Step 6: exercise Developer message ───────────────────────────────

        let task = stored.tasks[0].clone();

        // Same pattern: ask().send() for `Result<(), String>` reply gives back
        // `()` on success (SendError is unwrapped by `.expect()`).
        developer_ref
            .ask(Develop {
                task: task.clone(),
                worktree: PathBuf::from("/tmp/test-worktree"),
            })
            .send()
            .await
            .expect("Develop skeleton must return Ok(())");

        // ── Step 7: exercise Reviewer message ────────────────────────────────

        // ask().send() for `Result<ReviewVerdict, String>` reply gives back
        // `ReviewVerdict` on success (the outer SendError is unwrapped by `.expect()`).
        let verdict = reviewer_ref
            .ask(Review {
                task: task.clone(),
                worktree: PathBuf::from("/tmp/test-worktree"),
            })
            .send()
            .await
            .expect("Review skeleton must return Ok(ReviewVerdict::Approve)");

        assert_eq!(
            verdict,
            ReviewVerdict::Approve,
            "Review skeleton should return Approve placeholder"
        );

        // ── Clean shutdown ────────────────────────────────────────────────────
        root.kill();
    }
}
