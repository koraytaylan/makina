//! Domain types for tasks and the task graph.
//!
//! This module defines the authoritative Rust representation of the data
//! persisted in `.tasks/{slug}.json` files.  These types are the **source of
//! truth**; view-level DTOs in [`crate::api`] (e.g. `api::TaskView`,
//! `api::TaskId`) are intentionally separate — mapping from domain types to
//! view types will be added in a later task once both layers are stable.
//!
//! # File format
//!
//! A task graph is serialized to JSON via `serde_json`.  Timestamps are
//! RFC3339 strings (e.g. `"2026-05-28T10:00:00Z"`), keeping diffs readable
//! when the artifact is committed to a repository.
//!
//! # Lifecycle overview
//!
//! ```text
//!  new ──► ready ──► in-progress ──► in-review ──► done
//!                                        │
//!                              (changes requested)
//!                                        │
//!                                        ▼
//!                              back to in-progress
//!                                  (or failed)
//! ```
//!
//! State-transition logic lives in a later task (`task-state-machine`); this
//! module only defines the enum and the surrounding data structures.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── TaskId ────────────────────────────────────────────────────────────────────

/// A stable, kebab-case identifier for a task (e.g. `"core-api-surface"`).
///
/// `TaskId` is the primary key used both in `depends_on` references and in the
/// `.tasks/{slug}.json` file name itself.  Uniqueness within a [`TaskGraph`] is
/// enforced by [`TaskGraph::validate`].
///
/// Serializes transparently as a plain JSON string so artifact diffs stay
/// minimal.
///
/// # Note on duplication
///
/// [`crate::api::TaskId`] is a separate, view-level newtype intentionally not
/// coupled to this domain type.  A `From<task::TaskId> for api::TaskId`
/// conversion will be added in a future task.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl TaskId {
    /// Create a `TaskId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── TaskState ─────────────────────────────────────────────────────────────────

/// The lifecycle state of a task within a [`TaskGraph`].
///
/// Variants are serialized in kebab-case JSON (e.g. `"in-progress"`) to match
/// the project's state-naming convention.
///
/// Transition logic is deferred to the `task-state-machine` task; this enum
/// represents the states only.
///
/// # Default
///
/// A freshly created task begins in [`TaskState::New`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    /// Registered in the graph; one or more `depends_on` prerequisites have not
    /// yet reached [`TaskState::Done`].
    #[default]
    New,

    /// All prerequisites are [`TaskState::Done`]; eligible to be picked up by a
    /// Developer agent.
    Ready,

    /// A Developer agent is actively working on this task.
    InProgress,

    /// The Developer agent has finished; a Reviewer agent is evaluating the
    /// output.
    InReview,

    /// The task was accepted by the Reviewer (or auto-approved).  Terminal.
    Done,

    /// The task permanently failed after exhausting retry / gate limits, or
    /// encountered an unrecoverable error.  Terminal.
    Failed,

    /// A prerequisite of this task failed, so the task was never run.  Reached
    /// via [`crate::state_machine::TaskEvent::DependencyFailed`] from any active
    /// state.  Terminal.
    Skipped,
}

// ── Task ──────────────────────────────────────────────────────────────────────

/// A single task in the graph, as persisted in `.tasks/{slug}.json`.
///
/// All fields are serialized to JSON.  Optional timestamp fields are omitted
/// from the artifact when `None` (`skip_serializing_if = "Option::is_none"`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// Stable kebab-case identifier, unique within the owning [`TaskGraph`].
    pub id: TaskId,

    /// Short human-readable title, taken verbatim from the task-list document.
    pub title: String,

    /// Longer description of what the task entails.
    pub description: String,

    /// Acceptance criterion: the condition that must hold for the task to be
    /// considered [`TaskState::Done`].
    pub done_when: String,

    /// IDs of tasks that must reach [`TaskState::Done`] before this task
    /// transitions to [`TaskState::Ready`].
    ///
    /// May be augmented by the Planner with additional edges derived from
    /// file/area overlap analysis.
    pub depends_on: Vec<TaskId>,

    /// Scheduling-section hint assigned by the Planner (e.g. `"0003"` for the
    /// third wave of tasks that can run in parallel).  `None` if the Planner
    /// has not yet processed this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,

    /// Current lifecycle state.  Defaults to [`TaskState::New`] for a freshly
    /// created task.
    #[serde(default)]
    pub state: TaskState,

    /// How many times this task has cycled through the Developer → gate →
    /// failed-gate loop.
    pub gate_iterations: u32,

    /// How many times this task has cycled through the Developer → Reviewer →
    /// changes-requested loop.
    pub review_iterations: u32,

    /// When the task was first added to the graph.
    pub created_at: DateTime<Utc>,

    /// When the task record was last modified (state change, counter increment,
    /// etc.).
    pub updated_at: DateTime<Utc>,

    /// When a Developer agent first picked up this task.  `None` until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,

    /// When the task reached a terminal state ([`TaskState::Done`] or
    /// [`TaskState::Failed`]).  `None` until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,

    /// Why the task reached [`TaskState::Failed`], if applicable.
    ///
    /// Set by the supervisor at the failing transition.  `None` for tasks that
    /// are not `Failed` (or failed before plan 0014 populated this field — the
    /// `#[serde(default)]` ensures older snapshots still deserialise cleanly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<crate::api::FailureReason>,
}

// ── TaskGraph ─────────────────────────────────────────────────────────────────

/// The task graph persisted as `.tasks/{slug}.json`.
///
/// Tasks are stored in authored order (natural reading/display order).
/// The `slug` field matches the file stem (e.g. `"my-feature"` for
/// `.tasks/my-feature.json`).
///
/// # Validation
///
/// Call [`TaskGraph::validate`] after loading to verify that all `depends_on`
/// references resolve and that task IDs are unique.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskGraph {
    /// The file-stem identifier used in `.tasks/{slug}.json`.
    pub slug: String,

    /// Ordered list of tasks.  Preserves authored order for display purposes.
    pub tasks: Vec<Task>,
}

impl TaskGraph {
    /// Look up a task by its [`TaskId`].
    ///
    /// Returns `None` if no task with the given ID exists in this graph.
    /// Performs a linear scan (graphs are small — typically < 100 tasks).
    pub fn get(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| &t.id == id)
    }

    /// Validate structural integrity of the graph.
    ///
    /// Checks:
    /// 1. All task IDs are unique within this graph.
    /// 2. Every ID referenced in any `depends_on` list exists in the graph.
    ///
    /// Does **not** check for cycles (that is a scheduling concern handled by a
    /// later task).
    ///
    /// # Errors
    ///
    /// Returns the first [`TaskGraphError`] encountered.
    pub fn validate(&self) -> Result<(), TaskGraphError> {
        // Check for duplicate IDs.
        let mut seen = std::collections::HashSet::new();
        for task in &self.tasks {
            if !seen.insert(&task.id) {
                return Err(TaskGraphError::DuplicateId {
                    id: task.id.clone(),
                });
            }
        }

        // Check that all dependency references resolve.
        for task in &self.tasks {
            for dep in &task.depends_on {
                if self.get(dep).is_none() {
                    return Err(TaskGraphError::UnresolvedDependency {
                        task: task.id.clone(),
                        missing: dep.clone(),
                    });
                }
            }
        }

        Ok(())
    }
}

// ── TaskGraphError ────────────────────────────────────────────────────────────

/// Errors returned by [`TaskGraph::validate`].
#[derive(Debug, Error, PartialEq)]
pub enum TaskGraphError {
    /// Two or more tasks share the same [`TaskId`].
    #[error("duplicate task id: {id}")]
    DuplicateId {
        /// The duplicated identifier.
        id: TaskId,
    },

    /// A task's `depends_on` list references an ID that does not exist in the
    /// graph.
    #[error("task `{task}` depends on unknown task `{missing}`")]
    UnresolvedDependency {
        /// The task that has the dangling reference.
        task: TaskId,
        /// The missing dependency ID.
        missing: TaskId,
    },
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Tests for the task domain model.
    //!
    //! The required round-trip test constructs a sample [`TaskGraph`] with
    //! three tasks in varied states, serializes it to JSON with `serde_json`,
    //! deserializes it back, and asserts equality.
    //!
    //! Additional tests cover [`TaskGraph::validate`]: a graph with a dangling
    //! dependency fails validation, and a well-formed graph passes.

    use super::*;
    use chrono::TimeZone;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn fixed_ts(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 10, 0, 0)
            .single()
            .expect("valid date")
    }

    fn sample_graph() -> TaskGraph {
        let t0 = fixed_ts(2026, 5, 1);
        let t1 = fixed_ts(2026, 5, 2);
        let t2 = fixed_ts(2026, 5, 3);

        TaskGraph {
            slug: "my-feature".to_string(),
            tasks: vec![
                Task {
                    id: TaskId::new("workspace-scaffold"),
                    title: "Scaffold the workspace".to_string(),
                    description: "Set up Cargo workspace with three crates.".to_string(),
                    done_when: "cargo build passes from the workspace root".to_string(),
                    depends_on: vec![],
                    section: Some("0001".to_string()),
                    state: TaskState::Done,
                    gate_iterations: 1,
                    review_iterations: 0,
                    created_at: t0,
                    updated_at: t1,
                    started_at: Some(t0),
                    finished_at: Some(t1),
                    failure_reason: None,
                },
                Task {
                    id: TaskId::new("task-model"),
                    title: "Task and task-graph types".to_string(),
                    description: "Data structures for tasks and the task graph with serde."
                        .to_string(),
                    done_when: "A sample task graph round-trips through serialize/deserialize."
                        .to_string(),
                    depends_on: vec![TaskId::new("workspace-scaffold")],
                    section: Some("0002".to_string()),
                    state: TaskState::InProgress,
                    gate_iterations: 0,
                    review_iterations: 0,
                    created_at: t0,
                    updated_at: t2,
                    started_at: Some(t2),
                    finished_at: None,
                    failure_reason: None,
                },
                Task {
                    id: TaskId::new("config-loading"),
                    title: "Config loading".to_string(),
                    description: "Load and validate makina.toml configuration.".to_string(),
                    done_when: "A sample config round-trips through load/validate.".to_string(),
                    depends_on: vec![TaskId::new("workspace-scaffold")],
                    section: None,
                    state: TaskState::New,
                    gate_iterations: 0,
                    review_iterations: 0,
                    created_at: t0,
                    updated_at: t0,
                    started_at: None,
                    finished_at: None,
                    failure_reason: None,
                },
            ],
        }
    }

    // ── Round-trip test ───────────────────────────────────────────────────────

    /// Serialize a sample [`TaskGraph`] to JSON and deserialize it back.
    /// The deserialized value must equal the original.
    #[test]
    fn task_graph_round_trips_through_json() {
        let original = sample_graph();

        let json = serde_json::to_string_pretty(&original)
            .expect("TaskGraph must serialize to JSON without error");

        // Sanity-check the JSON shape.
        assert!(
            json.contains("\"slug\": \"my-feature\""),
            "slug should appear in JSON"
        );
        assert!(
            json.contains("\"in-progress\""),
            "kebab-case state should appear in JSON"
        );
        assert!(
            json.contains("\"done\""),
            "done state should appear in JSON"
        );
        // Optional null timestamps should be absent (skip_serializing_if = "Option::is_none").
        assert!(
            !json.contains("\"finished_at\": null"),
            "None finished_at must be omitted, not serialized as null"
        );

        let deserialized: TaskGraph =
            serde_json::from_str(&json).expect("JSON must deserialize back to TaskGraph");

        assert_eq!(
            original, deserialized,
            "Deserialized TaskGraph must equal the original"
        );
    }

    // ── Validation tests ──────────────────────────────────────────────────────

    /// A well-formed graph (unique IDs, all deps resolve) passes validation.
    #[test]
    fn validate_passes_for_well_formed_graph() {
        let graph = sample_graph();
        assert!(
            graph.validate().is_ok(),
            "sample graph should pass validation"
        );
    }

    /// A graph where a task references an unknown dependency fails validation
    /// with [`TaskGraphError::UnresolvedDependency`].
    #[test]
    fn validate_fails_for_dangling_dependency() {
        let t0 = fixed_ts(2026, 5, 1);
        let graph = TaskGraph {
            slug: "bad-graph".to_string(),
            tasks: vec![Task {
                id: TaskId::new("orphan"),
                title: "Orphan task".to_string(),
                description: "Depends on a non-existent task.".to_string(),
                done_when: "n/a".to_string(),
                depends_on: vec![TaskId::new("ghost-task")],
                section: None,
                state: TaskState::New,
                gate_iterations: 0,
                review_iterations: 0,
                created_at: t0,
                updated_at: t0,
                started_at: None,
                finished_at: None,
                failure_reason: None,
            }],
        };

        let err = graph.validate().expect_err("should fail with dangling dep");
        assert_eq!(
            err,
            TaskGraphError::UnresolvedDependency {
                task: TaskId::new("orphan"),
                missing: TaskId::new("ghost-task"),
            }
        );
    }

    /// A graph with two tasks sharing the same ID fails validation with
    /// [`TaskGraphError::DuplicateId`].
    #[test]
    fn validate_fails_for_duplicate_ids() {
        let t0 = fixed_ts(2026, 5, 1);
        let task = Task {
            id: TaskId::new("dupe"),
            title: "Duplicate".to_string(),
            description: "Same id appears twice.".to_string(),
            done_when: "n/a".to_string(),
            depends_on: vec![],
            section: None,
            state: TaskState::New,
            gate_iterations: 0,
            review_iterations: 0,
            created_at: t0,
            updated_at: t0,
            started_at: None,
            finished_at: None,
            failure_reason: None,
        };
        let graph = TaskGraph {
            slug: "dup-graph".to_string(),
            tasks: vec![task.clone(), task],
        };

        let err = graph.validate().expect_err("should fail with duplicate id");
        assert_eq!(
            err,
            TaskGraphError::DuplicateId {
                id: TaskId::new("dupe"),
            }
        );
    }

    // ── TaskState default ─────────────────────────────────────────────────────

    /// `TaskState::default()` returns `New`.
    #[test]
    fn task_state_default_is_new() {
        assert_eq!(TaskState::default(), TaskState::New);
    }

    // ── TaskId display and newtype ────────────────────────────────────────────

    #[test]
    fn task_id_display_matches_inner_string() {
        let id = TaskId::new("some-task");
        assert_eq!(id.to_string(), "some-task");
    }

    /// `TaskId` serializes as a bare JSON string (transparent).
    #[test]
    fn task_id_serializes_transparently() {
        let id = TaskId::new("foo-bar");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"foo-bar\"");
        let back: TaskId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    /// `TaskState` variants serialize in kebab-case.
    #[test]
    fn task_state_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&TaskState::InProgress).unwrap(),
            "\"in-progress\""
        );
        assert_eq!(
            serde_json::to_string(&TaskState::InReview).unwrap(),
            "\"in-review\""
        );
        assert_eq!(serde_json::to_string(&TaskState::New).unwrap(), "\"new\"");
        assert_eq!(serde_json::to_string(&TaskState::Done).unwrap(), "\"done\"");
        assert_eq!(
            serde_json::to_string(&TaskState::Failed).unwrap(),
            "\"failed\""
        );
        assert_eq!(
            serde_json::to_string(&TaskState::Ready).unwrap(),
            "\"ready\""
        );
    }
}
