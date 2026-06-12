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
//! `Args`.  Because a Developer/Reviewer needs the hub's `ActorRef` to be
//! constructed (a construction cycle), and because the concurrent scheduler
//! (task 24) spawns a **per-task** Developer/Reviewer pair on demand, the hub is
//! given the *means to spawn* those spokes via a post-spawn
//! [`SetSpokes`](supervisor::SetSpokes) message (the `RootSupervisor` ref, the
//! hub's own ref, and the shared backend).  (The previous unsupervised
//! `Supervisor::start()` helper has been removed — it was a footgun, and
//! `spawn_child` is the canonical path.)
//!
//! # Planner interpreter injection
//!
//! Task 14 (`planner-actor`) replaced the skeleton `Planner` handler with a
//! real implementation.  The interpreter is injected via [`PlannerArgs`] as an
//! `Arc<dyn TaskListInterpreter>`.  Use
//! [`StructuredTextInterpreter`](crate::interpreter::StructuredTextInterpreter)
//! in tests; task 18 will plug in a model-backed interpreter.
//!
//! Task 21 (`develop-review-loop`) added the real orchestration: the
//! `Supervisor` drives [`RunReadyTasks`](supervisor::RunReadyTasks) as an
//! `ask`-based develop→review loop, and the `Developer`/`Reviewer` drive an
//! injected `Arc<dyn AgentBackend>`.  The gate loop (task 22), squash-merge
//! (task 23), concurrency (task 24), and termination caps (task 25 — gate +
//! reviewer + wall-clock) are all implemented in `supervisor.rs`.

pub mod developer;
pub mod planner;
pub mod reviewer;
pub mod supervisor;

// Convenience re-exports so callers can use `actors::Supervisor` etc.
pub use developer::{Develop, DevelopAck, DevelopOutcome, Developer, DeveloperArgs};
pub use planner::{InterpretTaskList, InterpretTaskListAck, Planner, PlannerArgs};
pub use reviewer::{Review, ReviewReply, ReviewVerdict, Reviewer, ReviewerArgs};
pub use supervisor::{
    EventSink, RunControl, RunReadyTasks, RunReport, SetSpokes, SetTaskGraph, Supervisor,
    SupervisorArgs, TaskGraphSnapshot, run_graph,
};

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

    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use chrono::Utc;

    use crate::{
        actors::{
            Develop, DevelopOutcome, Developer, DeveloperArgs, InterpretTaskList, Planner,
            PlannerArgs, Review, ReviewVerdict, Reviewer, ReviewerArgs, SetTaskGraph, Supervisor,
            SupervisorArgs, TaskGraphSnapshot,
        },
        api,
        backend::{AgentBackend, ResponseEvent, noop::NoopBackend},
        config::{Config, GlobalConfig, ProjectConfig},
        interpreter::StructuredTextInterpreter,
        supervision::{RestartConfig, RootSupervisor},
        task::{Task, TaskGraph, TaskId, TaskState},
        worktree::WorktreeManager,
    };

    // ── Helper ────────────────────────────────────────────────────────────────

    /// Build a resolved [`Config`] with no gates (gate loop is a no-op).
    fn test_config_no_gates() -> Config {
        Config::resolve(GlobalConfig::default(), ProjectConfig::default())
    }

    /// Initialise a minimal git repo in `path` so the Developer's commit step
    /// (task 23: `git add -A` + `git commit`) has a real working tree to commit
    /// into.  The smoke test only exercises message acceptance, so a bare init +
    /// identity + one commit is enough (no `develop`/worktree dance needed).
    fn init_git_repo(path: &std::path::Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .status()
                .expect("git must be available");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);
        run(&["commit", "--allow-empty", "-m", "init"]);
    }

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
                failure_reason: None,
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
    /// - `Developer`: `Develop` returns `Ok(DevelopOutcome)` with non-empty output.
    /// - `Reviewer`: `Review` returns `Ok(ReviewVerdict::Approve)`.
    ///
    /// This is a message-acceptance smoke test; the full develop→review loop is
    /// exercised by the `develop_review_loop.rs` integration test.
    #[tokio::test]
    async fn all_actors_spawn_and_accept_messages_under_root_supervisor() {
        // ── Step 1: start fault-tolerance root ───────────────────────────────
        let root = RootSupervisor::start();

        // A NoopBackend configured so the Reviewer parses an approve verdict.
        let backend: Arc<dyn AgentBackend> = Arc::new(NoopBackend::with_responses(vec![
            "developer output".into(),
            r#"{"verdict":"approve"}"#.into(),
        ]));

        // ── Step 2: spawn domain Supervisor hub ──────────────────────────────
        let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
            &root,
            SupervisorArgs {
                // A no-op manager pointed at a dummy path; this smoke test never
                // calls create/remove (it only exercises message acceptance).
                worktree_manager: WorktreeManager::new(
                    PathBuf::from("/tmp/makina-actor-smoke"),
                    "develop".into(),
                ),
                // No gates: this smoke test never runs the gate loop (it only
                // exercises message acceptance, not RunReadyTasks).
                config: test_config_no_gates(),
            },
            RestartConfig::default(),
        )
        .await;

        // ── Step 3: spawn spokes with the hub ref and backend ────────────────

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
                backend: Arc::clone(&backend),
                assignment: None,
            },
            RestartConfig::default(),
        )
        .await;

        // Reviewer
        let reviewer_ref = RootSupervisor::spawn_child::<Reviewer>(
            &root,
            ReviewerArgs {
                supervisor: supervisor_ref.clone(),
                backend: Arc::clone(&backend),
                assignment: None,
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
                path: PathBuf::from("/dev/null"), // /dev/null reads as empty string -> empty but valid TaskGraph
            })
            .send()
            .await
            .expect("InterpretTaskList skeleton must return Ok(())");

        // ── Step 6: exercise Developer message ───────────────────────────────

        let task = stored.tasks[0].clone();

        // ask().send() for `Result<DevelopOutcome, String>` reply gives back the
        // `DevelopOutcome` on success (SendError is unwrapped by `.expect()`).
        // The NoopBackend returns "developer output" for the first prompt.
        //
        // The Developer commits the worktree (task 23: `git add -A` + `git
        // commit`), so it needs a real git working tree — give it a temp repo.
        let dev_worktree = tempfile::tempdir().expect("temp worktree dir");
        init_git_repo(dev_worktree.path());
        let outcome = developer_ref
            .ask(Develop {
                task: task.clone(),
                worktree: dev_worktree.path().to_path_buf(),
                feedback: None,
                // Smoke test: no run context; a no-op sink (events are additive).
                run: crate::api::RunId(0),
                sink: std::sync::Arc::new(|_| {}),
                idle_secs: None,
            })
            .send()
            .await
            .expect("Develop must return Ok(DevelopOutcome)");

        assert_eq!(
            outcome,
            DevelopOutcome {
                output: "developer output".to_string()
            },
            "Developer should return the backend's canned output"
        );

        // ── Step 7: exercise Reviewer message ────────────────────────────────

        // ask().send() for `Result<ReviewVerdict, String>` reply gives back the
        // `ReviewVerdict` on success.  The NoopBackend returns the approve JSON
        // for the second prompt, which the Reviewer parses into `Approve`.
        let verdict = reviewer_ref
            .ask(Review {
                task: task.clone(),
                worktree: PathBuf::from("/tmp/test-worktree"),
                run: crate::api::RunId(0),
                sink: std::sync::Arc::new(|_| {}),
                idle_secs: None,
            })
            .send()
            .await
            .expect("Review must return Ok(ReviewVerdict)");

        assert_eq!(
            verdict,
            ReviewVerdict::Approve,
            "Reviewer should parse the approve verdict from the backend"
        );

        // ── Clean shutdown ────────────────────────────────────────────────────
        root.kill();
    }

    // ── Rich exchange-event forwarding (plan-0006 task 5) ──────────────────────

    /// Drive a Developer turn whose backend emits a mix of `TextChunk`,
    /// `ThoughtChunk`, `ToolCall`, and `ToolCallUpdate` events, and assert:
    ///
    /// 1. The thought and both tool events are forwarded to the sink as
    ///    `api::Event::AgentExchange` events with the matching `ExchangeEvent`
    ///    kinds (observability side channel).
    /// 2. NONE of the thought/tool text leaks into the Developer's collected
    ///    `output` — the final answer is built solely from the `TextChunk`s, so it
    ///    is exactly `"answer done"` (the two `TextChunk`s concatenated).
    ///
    /// This reuses the same `RootSupervisor` + `Develop`-`ask` harness as the
    /// smoke test above; the only additions are the `scripted` backend and a
    /// capturing sink. The end-to-end orchestrator-driven assertion lives in the
    /// task-9 acceptance test; this one isolates the actor's forwarding contract.
    #[tokio::test]
    async fn developer_forwards_thoughts_and_tools_without_polluting_output() {
        let root = RootSupervisor::start();

        // A backend whose single turn emits text + thought + tool events
        // (TurnComplete is appended automatically by `scripted`).
        let backend: Arc<dyn AgentBackend> = Arc::new(NoopBackend::scripted(vec![
            ResponseEvent::TextChunk {
                text: "answer ".into(),
            },
            ResponseEvent::ThoughtChunk {
                text: "thinking".into(),
            },
            ResponseEvent::ToolCall {
                id: "t1".into(),
                title: "run".into(),
                kind: Some("execute".into()),
                status: "pending".into(),
            },
            ResponseEvent::ToolCallUpdate {
                id: "t1".into(),
                status: Some("completed".into()),
                title: None,
            },
            ResponseEvent::TextChunk {
                text: "done".into(),
            },
        ]));

        let supervisor_ref = RootSupervisor::spawn_child::<Supervisor>(
            &root,
            SupervisorArgs {
                worktree_manager: WorktreeManager::new(
                    PathBuf::from("/tmp/makina-actor-rich"),
                    "develop".into(),
                ),
                config: test_config_no_gates(),
            },
            RestartConfig::default(),
        )
        .await;

        let developer_ref = RootSupervisor::spawn_child::<Developer>(
            &root,
            DeveloperArgs {
                supervisor: supervisor_ref.clone(),
                backend: Arc::clone(&backend),
                assignment: None,
            },
            RestartConfig::default(),
        )
        .await;

        // Capturing sink: collect every AgentExchange event the handler emits.
        let captured: Arc<Mutex<Vec<api::Event>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = Arc::clone(&captured);
            std::sync::Arc::new(move |event: api::Event| {
                captured.lock().unwrap().push(event);
            })
        };

        // The Developer commits the worktree, so it needs a real git repo.
        let dev_worktree = tempfile::tempdir().expect("temp worktree dir");
        init_git_repo(dev_worktree.path());

        let task = minimal_graph().tasks[0].clone();
        let outcome = developer_ref
            .ask(Develop {
                task,
                worktree: dev_worktree.path().to_path_buf(),
                feedback: None,
                run: api::RunId(7),
                sink,
                idle_secs: None,
            })
            .send()
            .await
            .expect("Develop must return Ok(DevelopOutcome)");

        // ── Output purity: ONLY the TextChunks contributed. ───────────────────
        assert_eq!(
            outcome,
            DevelopOutcome {
                output: "answer done".to_string()
            },
            "output must be the TextChunks concatenated — no thought/tool text",
        );

        // ── Forwarding: the sink saw the thought + both tool exchange events. ──
        let events = captured.lock().unwrap();

        let saw_thought = events.iter().any(|e| {
            matches!(
                e,
                api::Event::AgentExchange {
                    role: api::AgentRole::Developer,
                    event: api::ExchangeEvent::ThoughtChunk { text },
                    ..
                } if text == "thinking"
            )
        });
        let saw_tool_call = events.iter().any(|e| {
            matches!(
                e,
                api::Event::AgentExchange {
                    role: api::AgentRole::Developer,
                    event: api::ExchangeEvent::ToolCall { id, kind, status, .. },
                    ..
                } if id == "t1" && kind.as_deref() == Some("execute") && status == "pending"
            )
        });
        let saw_tool_update = events.iter().any(|e| {
            matches!(
                e,
                api::Event::AgentExchange {
                    role: api::AgentRole::Developer,
                    event: api::ExchangeEvent::ToolCallUpdate { id, status, .. },
                    ..
                } if id == "t1" && status.as_deref() == Some("completed")
            )
        });

        assert!(
            saw_thought,
            "sink must observe a ThoughtChunk exchange event"
        );
        assert!(saw_tool_call, "sink must observe a ToolCall exchange event");
        assert!(
            saw_tool_update,
            "sink must observe a ToolCallUpdate exchange event",
        );

        root.kill();
    }
}
