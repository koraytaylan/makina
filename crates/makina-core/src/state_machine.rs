//! Lifecycle finite-state machine for [`TaskState`].
//!
//! This module answers a single question: given the current [`TaskState`] and a
//! domain [`TaskEvent`], what is the resulting state — or is the transition
//! illegal?
//!
//! # Responsibilities (and non-responsibilities)
//!
//! **This module IS responsible for:**
//! - Defining every legal `(state, event) → state` transition exactly as
//!   specified in the architecture state diagram.
//! - Rejecting every other combination with [`IllegalTransition`].
//! - Exposing [`is_terminal`] and [`legal_events`] as lightweight introspection
//!   helpers.
//!
//! **This module is NOT responsible for:**
//! - Counting gate or review iterations, or measuring wall-clock time (the caps
//!   are enforced *externally* by the Supervisor — see `termination-caps`
//!   (task 25)).  This module only defines the terminal transitions those caps
//!   trigger (`GateCapReached`, `ReviewCapReached`, `WallClockCapReached`).
//! - Deciding *when* to emit events (handled by `gate-runner`,
//!   `develop-review-loop`, etc.).
//! - Mutating [`crate::task::Task`] fields (`updated_at`, iteration counters,
//!   etc.).
//! - Any I/O or actor message passing.
//!
//! # Transition table
//!
//! ```text
//! ┌──────────────┬──────────────────────┬─────────────┐
//! │ From         │ Event                │ To          │
//! ├──────────────┼──────────────────────┼─────────────┤
//! │ New          │ DependenciesSatisfied│ Ready       │
//! │ Ready        │ Dispatched           │ InProgress  │
//! │ Ready        │ HardError            │ Failed      │ ← worktree-create fail (task 25)
//! │ Ready        │ WallClockCapReached  │ Failed      │ ← deadline (task 25)
//! │ InProgress   │ GateFailed           │ InProgress  │ ← self-loop
//! │ InProgress   │ GatesPassed          │ InReview    │
//! │ InProgress   │ GateCapReached       │ Failed      │
//! │ InProgress   │ HardError            │ Failed      │
//! │ InProgress   │ WallClockCapReached  │ Failed      │ ← deadline (task 25)
//! │ InReview     │ ReviewerRejected     │ InProgress  │ ← reject loop
//! │ InReview     │ ReviewerApproved     │ Done        │
//! │ InReview     │ ReviewCapReached     │ Failed      │
//! │ InReview     │ MergeConflict        │ Failed      │ ← squash-merge conflict (dedicated event)
//! │ InReview     │ HardError            │ Failed      │ ← review-time/merge hard error (task 25)
//! │ InReview     │ WallClockCapReached  │ Failed      │ ← deadline (task 25)
//! └──────────────┴──────────────────────┴─────────────┘
//! ```
//!
//! `Done` and `Failed` are terminal: no outgoing transitions exist for any
//! event.  `WallClockCapReached` is **not** legal from `New`: the per-task
//! wall-clock deadline starts at dispatch (when the driver begins), so a task
//! that has not been picked up yet cannot time out.

use thiserror::Error;

use crate::task::TaskState;

// ── TaskEvent ─────────────────────────────────────────────────────────────────

/// Domain events that drive the task lifecycle FSM.
///
/// Each variant corresponds to exactly one triggering condition in the
/// architecture state diagram. Actors that observe these conditions are
/// responsible for emitting the appropriate event to [`transition`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TaskEvent {
    /// All tasks listed in `depends_on` have reached [`TaskState::Done`].
    ///
    /// Emitted by the Supervisor after a dependency completes.  Moves the task
    /// from [`TaskState::New`] → [`TaskState::Ready`].
    DependenciesSatisfied,

    /// The Supervisor has assigned this task to a Developer agent.
    ///
    /// Moves the task from [`TaskState::Ready`] → [`TaskState::InProgress`].
    Dispatched,

    /// A quality gate ran and did **not** pass; the Developer must iterate.
    ///
    /// Self-loop: [`TaskState::InProgress`] → [`TaskState::InProgress`].
    /// The gate-runner increments `gate_iterations` before emitting this event
    /// (handled externally to this FSM).
    GateFailed,

    /// All quality gates passed; the output is ready for Reviewer inspection.
    ///
    /// Moves the task from [`TaskState::InProgress`] → [`TaskState::InReview`].
    GatesPassed,

    /// The gate iteration cap was reached without all gates passing.
    ///
    /// Moves the task from [`TaskState::InProgress`] → [`TaskState::Failed`]
    /// (terminal).  The cap is enforced externally to this FSM.
    GateCapReached,

    /// An unrecoverable error occurred at some point in the active lifecycle
    /// (e.g. a worktree could not be created, a tool crashed, the Developer or
    /// Reviewer session failed to dispatch/parse, or a hard — non-conflict —
    /// merge failure).
    ///
    /// Legal from every *active* state, all moving the task to
    /// [`TaskState::Failed`] (terminal):
    /// - [`TaskState::Ready`] → `Failed` — a worktree-create failure before the
    ///   Developer is ever dispatched.
    /// - [`TaskState::InProgress`] → `Failed` — a Developer-dispatch failure or
    ///   a gate that could not be launched.
    /// - [`TaskState::InReview`] → `Failed` — a Reviewer dispatch/parse failure
    ///   or a hard (non-conflict) squash-merge failure.
    HardError,

    /// The Reviewer requested changes; the Developer must iterate again.
    ///
    /// Reject loop: [`TaskState::InReview`] → [`TaskState::InProgress`].
    /// The review-loop actor increments `review_iterations` before emitting
    /// this event (handled externally to this FSM).
    ReviewerRejected,

    /// The Reviewer accepted the output; the task is complete.
    ///
    /// Moves the task from [`TaskState::InReview`] → [`TaskState::Done`]
    /// (terminal).
    ReviewerApproved,

    /// The review iteration cap was reached without approval.
    ///
    /// Moves the task from [`TaskState::InReview`] → [`TaskState::Failed`]
    /// (terminal).  The cap is enforced externally to this FSM.
    ReviewCapReached,

    /// A squash-merge on reviewer approval hit a conflict (distinct from a hard
    /// merge error). The merger already restored `develop` clean. Moves only
    /// [`TaskState::InReview`] → [`TaskState::Failed`] (terminal).
    MergeConflict,

    /// The per-task wall-clock deadline (`config.caps.wall_clock_secs`) elapsed
    /// while the task was still active.
    ///
    /// Legal from any *active* state the task can be in when the deadline fires —
    /// [`TaskState::Ready`], [`TaskState::InProgress`], or [`TaskState::InReview`]
    /// — all moving it to [`TaskState::Failed`] (terminal).  **Not** legal from
    /// [`TaskState::New`]: the clock starts when the driver picks the task up
    /// (dispatch), so an un-started task cannot time out.  Enforced externally
    /// (the scheduler wraps each driver in a `tokio::time::timeout`).
    WallClockCapReached,

    /// A prerequisite (transitive `depends_on`) of this task reached
    /// [`TaskState::Failed`], so the task can never become [`TaskState::Ready`].
    ///
    /// Legal from any *active* state — [`TaskState::New`], [`TaskState::Ready`],
    /// [`TaskState::InProgress`], or [`TaskState::InReview`] — all moving the task
    /// to [`TaskState::Skipped`] (terminal).  **Not** legal from a terminal state
    /// (`Done`, `Failed`, `Skipped`).  Enforced externally (the scheduler walks
    /// the reverse dependency edges when a task fails).
    DependencyFailed,
}

// ── IllegalTransition ─────────────────────────────────────────────────────────

/// Error returned when a [`TaskEvent`] is applied to a [`TaskState`] that has
/// no outgoing edge for that event in the lifecycle diagram.
///
/// Includes both terminal-state rejections (no event is legal from `Done` or
/// `Failed`) and any other `(state, event)` pair absent from the transition
/// table.
#[derive(Debug, Error, PartialEq)]
#[error("illegal transition: cannot apply {event:?} in state {from:?}")]
pub struct IllegalTransition {
    /// The state the task was in when the event arrived.
    pub from: TaskState,
    /// The event that was applied.
    pub event: TaskEvent,
}

// ── Core FSM function ─────────────────────────────────────────────────────────

/// Apply `event` to `from` and return the resulting [`TaskState`].
///
/// Returns [`Ok`] with the target state for every pair listed in the
/// [transition table](self), including self-loops (e.g.
/// `InProgress + GateFailed → InProgress`).
///
/// Returns [`Err(IllegalTransition)`](IllegalTransition) for every other
/// `(state, event)` combination, including any event applied to a terminal
/// state (`Done` or `Failed`).
///
/// # Example
///
/// ```rust
/// use makina_core::task::TaskState;
/// use makina_core::state_machine::{TaskEvent, transition};
///
/// assert_eq!(
///     transition(TaskState::New, TaskEvent::DependenciesSatisfied),
///     Ok(TaskState::Ready),
/// );
///
/// assert!(
///     transition(TaskState::Done, TaskEvent::Dispatched).is_err(),
///     "terminal state must reject all events",
/// );
/// ```
pub fn transition(from: TaskState, event: TaskEvent) -> Result<TaskState, IllegalTransition> {
    use TaskEvent::*;
    use TaskState::*;

    match (from, event) {
        // ── New ───────────────────────────────────────────────────────────────
        (New, DependenciesSatisfied) => Ok(Ready),

        // ── Ready ─────────────────────────────────────────────────────────────
        (Ready, Dispatched) => Ok(InProgress),
        (Ready, HardError) => Ok(Failed), // worktree-create failure (task 25)
        (Ready, WallClockCapReached) => Ok(Failed), // deadline before dispatch completes (task 25)

        // ── InProgress ────────────────────────────────────────────────────────
        (InProgress, GateFailed) => Ok(InProgress), // self-loop
        (InProgress, GatesPassed) => Ok(InReview),
        (InProgress, GateCapReached) => Ok(Failed),
        (InProgress, HardError) => Ok(Failed),
        (InProgress, WallClockCapReached) => Ok(Failed), // deadline (task 25)

        // ── InReview ──────────────────────────────────────────────────────────
        (InReview, ReviewerRejected) => Ok(InProgress), // reject loop
        (InReview, ReviewerApproved) => Ok(Done),
        (InReview, ReviewCapReached) => Ok(Failed),
        (InReview, MergeConflict) => Ok(Failed), // squash-merge conflict (dedicated)
        (InReview, HardError) => Ok(Failed),     // review-time / hard-merge error (task 25)
        (InReview, WallClockCapReached) => Ok(Failed), // deadline (task 25)

        // ── DependencyFailed (active → Skipped) ───────────────────────────────
        (New | Ready | InProgress | InReview, DependencyFailed) => Ok(Skipped),

        // ── Everything else (illegal) ─────────────────────────────────────────
        _ => Err(IllegalTransition { from, event }),
    }
}

// ── Introspection helpers ─────────────────────────────────────────────────────

/// Returns `true` if `state` is a terminal state from which no further
/// transitions are possible.
///
/// Currently `Done`, `Failed`, and `Skipped` are the terminal states.
pub fn is_terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Done | TaskState::Failed | TaskState::Skipped
    )
}

/// Returns the complete list of [`TaskEvent`]s that are legal in `state`.
///
/// For terminal states (`Done`, `Failed`) this is always empty.  Useful for
/// exhaustive test construction and for supervisor introspection.
pub fn legal_events(from: TaskState) -> Vec<TaskEvent> {
    use TaskEvent::*;
    use TaskState::*;

    match from {
        New => vec![DependenciesSatisfied, DependencyFailed],
        Ready => vec![Dispatched, HardError, WallClockCapReached, DependencyFailed],
        InProgress => vec![
            GateFailed,
            GatesPassed,
            GateCapReached,
            HardError,
            WallClockCapReached,
            DependencyFailed,
        ],
        InReview => vec![
            ReviewerRejected,
            ReviewerApproved,
            ReviewCapReached,
            MergeConflict,
            HardError,
            WallClockCapReached,
            DependencyFailed,
        ],
        Done | Failed | Skipped => vec![],
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Tests for the task lifecycle FSM.
    //!
    //! # Coverage strategy
    //!
    //! The primary test (`exhaustive_transition_table`) iterates the full
    //! Cartesian product of all 7 states × all 12 events (84 pairs total).
    //! For each pair it asserts the exact expected outcome: `Ok(target)` for
    //! the 19 legal transitions and `Err(IllegalTransition)` for the remaining
    //! 65 pairs.  This single test is sufficient proof that the implementation
    //! matches the architecture diagram exactly.
    //!
    //! The legal count grew from 9 → 14 in task 25 (`termination-caps`) ...
    //! Plan-0002 added `MergeConflict` (legal only from InReview) as the 11th event.
    //! This task adds `DependencyFailed` as the 12th event (legal from each active
    //! state → the new 7th state `Skipped`), so the table grows by one state and
    //! one event: 6 → 7 states, 11 → 12 events, 66 → 84 pairs, 15 → 19 legal,
    //! 51 → 65 illegal.
    //!
    //! Supporting tests cover `is_terminal` and `legal_events` independently.

    use super::*;
    use crate::task::TaskState;

    // ── Legal transition set ──────────────────────────────────────────────────

    /// The complete set of legal `(from, event, to)` triples from the
    /// architecture state diagram (now 19 entries: 15 + the 4 DependencyFailed
    /// edges into Skipped).  Used both by `exhaustive_transition_table` and as
    /// documentation of intent.
    fn legal_table() -> Vec<(TaskState, TaskEvent, TaskState)> {
        use TaskEvent::*;
        use TaskState::*;

        vec![
            (New, DependenciesSatisfied, Ready),
            (Ready, Dispatched, InProgress),
            (Ready, HardError, Failed), // worktree-create failure (task 25)
            (Ready, WallClockCapReached, Failed), // deadline (task 25)
            (InProgress, GateFailed, InProgress), // self-loop
            (InProgress, GatesPassed, InReview),
            (InProgress, GateCapReached, Failed),
            (InProgress, HardError, Failed),
            (InProgress, WallClockCapReached, Failed), // deadline (task 25)
            (InReview, ReviewerRejected, InProgress),  // reject loop
            (InReview, ReviewerApproved, Done),
            (InReview, ReviewCapReached, Failed),
            (InReview, MergeConflict, Failed), // squash-merge conflict (dedicated)
            (InReview, HardError, Failed),     // review-time / hard-merge error (task 25)
            (InReview, WallClockCapReached, Failed), // deadline (task 25)
            (New, DependencyFailed, Skipped),  // prerequisite failed (fsm-skipped-state)
            (Ready, DependencyFailed, Skipped),
            (InProgress, DependencyFailed, Skipped),
            (InReview, DependencyFailed, Skipped),
        ]
    }

    /// All [`TaskState`] variants — used to build the Cartesian product.
    fn all_states() -> Vec<TaskState> {
        use TaskState::*;
        vec![New, Ready, InProgress, InReview, Done, Failed, Skipped]
    }

    /// All [`TaskEvent`] variants — used to build the Cartesian product.
    fn all_events() -> Vec<TaskEvent> {
        use TaskEvent::*;
        vec![
            DependenciesSatisfied,
            Dispatched,
            GateFailed,
            GatesPassed,
            GateCapReached,
            HardError,
            ReviewerRejected,
            ReviewerApproved,
            ReviewCapReached,
            MergeConflict,
            WallClockCapReached,
            DependencyFailed,
        ]
    }

    // ── Exhaustive Cartesian-product test ─────────────────────────────────────

    /// For every `(state, event)` pair in the 7×12 Cartesian product:
    /// - If the pair is in the legal table → assert `Ok(expected_target)`.
    /// - Otherwise → assert `Err(IllegalTransition { from, event })`.
    ///
    /// This is the definitive proof that the FSM implementation matches the
    /// architecture diagram: 19 legal transitions and 65 illegal ones, totalling
    /// 84 assertions. (Task 25 grew the table; plan-0002 added MergeConflict;
    /// this task adds DependencyFailed as the 12th event and Skipped as the 7th
    /// state, with 4 new legal edges into Skipped.)
    #[test]
    fn exhaustive_transition_table() {
        use std::collections::HashMap;

        // Build a lookup map: (from, event) → to for legal transitions.
        let legal: HashMap<(TaskState, TaskEvent), TaskState> = legal_table()
            .into_iter()
            .map(|(from, event, to)| ((from, event), to))
            .collect();

        let states = all_states();
        let events = all_events();

        let total = states.len() * events.len();
        assert_eq!(total, 84, "expected 7 states × 12 events = 84 pairs");

        let mut legal_count = 0usize;
        let mut illegal_count = 0usize;

        for &state in &states {
            for &event in &events {
                let result = transition(state, event);

                if let Some(&expected_to) = legal.get(&(state, event)) {
                    assert_eq!(
                        result,
                        Ok(expected_to),
                        "legal transition ({state:?}, {event:?}) should yield {expected_to:?}"
                    );
                    legal_count += 1;
                } else {
                    assert_eq!(
                        result,
                        Err(IllegalTransition { from: state, event }),
                        "illegal transition ({state:?}, {event:?}) should be rejected"
                    );
                    illegal_count += 1;
                }
            }
        }

        assert_eq!(legal_count, 19, "expected exactly 19 legal transitions");
        assert_eq!(illegal_count, 65, "expected exactly 65 illegal transitions");
    }

    // ── is_terminal ───────────────────────────────────────────────────────────

    /// `Done` and `Failed` are the only terminal states.
    #[test]
    fn terminal_states_are_done_and_failed() {
        use TaskState::*;

        assert!(is_terminal(Done), "Done must be terminal");
        assert!(is_terminal(Failed), "Failed must be terminal");

        assert!(!is_terminal(New), "New must not be terminal");
        assert!(!is_terminal(Ready), "Ready must not be terminal");
        assert!(!is_terminal(InProgress), "InProgress must not be terminal");
        assert!(!is_terminal(InReview), "InReview must not be terminal");
    }

    /// No event is legal in a terminal state.
    #[test]
    fn terminal_states_have_no_legal_events() {
        use TaskState::*;

        for state in [Done, Failed] {
            assert!(
                legal_events(state).is_empty(),
                "terminal state {state:?} must have no legal events"
            );
            // Also verify via transition directly.
            for &event in &all_events() {
                assert!(
                    transition(state, event).is_err(),
                    "terminal state {state:?} must reject event {event:?}"
                );
            }
        }
    }

    // ── legal_events ─────────────────────────────────────────────────────────

    /// `legal_events` returns exactly the events present in the transition table
    /// for each non-terminal state.
    #[test]
    fn legal_events_matches_transition_table() {
        use std::collections::{HashMap, HashSet};

        // Build expected events per from-state from the canonical legal table.
        let mut expected: HashMap<TaskState, HashSet<TaskEvent>> = HashMap::new();
        for (from, event, _to) in legal_table() {
            expected.entry(from).or_default().insert(event);
        }

        for &state in &all_states() {
            let got: HashSet<TaskEvent> = legal_events(state).into_iter().collect();
            let exp: HashSet<TaskEvent> = expected.get(&state).cloned().unwrap_or_default();
            assert_eq!(
                got, exp,
                "legal_events({state:?}) does not match the legal table"
            );
        }
    }

    // ── Individual named transition tests (documentation value) ───────────────

    /// Each of the 9 legal transitions verified individually for clarity.
    #[test]
    fn new_plus_dependencies_satisfied_yields_ready() {
        assert_eq!(
            transition(TaskState::New, TaskEvent::DependenciesSatisfied),
            Ok(TaskState::Ready)
        );
    }

    #[test]
    fn ready_plus_dispatched_yields_in_progress() {
        assert_eq!(
            transition(TaskState::Ready, TaskEvent::Dispatched),
            Ok(TaskState::InProgress)
        );
    }

    #[test]
    fn in_progress_plus_gate_failed_self_loops() {
        assert_eq!(
            transition(TaskState::InProgress, TaskEvent::GateFailed),
            Ok(TaskState::InProgress)
        );
    }

    #[test]
    fn in_progress_plus_gates_passed_yields_in_review() {
        assert_eq!(
            transition(TaskState::InProgress, TaskEvent::GatesPassed),
            Ok(TaskState::InReview)
        );
    }

    #[test]
    fn in_progress_plus_gate_cap_reached_yields_failed() {
        assert_eq!(
            transition(TaskState::InProgress, TaskEvent::GateCapReached),
            Ok(TaskState::Failed)
        );
    }

    #[test]
    fn in_progress_plus_hard_error_yields_failed() {
        assert_eq!(
            transition(TaskState::InProgress, TaskEvent::HardError),
            Ok(TaskState::Failed)
        );
    }

    #[test]
    fn in_review_plus_reviewer_rejected_loops_to_in_progress() {
        assert_eq!(
            transition(TaskState::InReview, TaskEvent::ReviewerRejected),
            Ok(TaskState::InProgress)
        );
    }

    #[test]
    fn in_review_plus_reviewer_approved_yields_done() {
        assert_eq!(
            transition(TaskState::InReview, TaskEvent::ReviewerApproved),
            Ok(TaskState::Done)
        );
    }

    #[test]
    fn in_review_plus_review_cap_reached_yields_failed() {
        assert_eq!(
            transition(TaskState::InReview, TaskEvent::ReviewCapReached),
            Ok(TaskState::Failed)
        );
    }

    // ── Task 25 additions: HardError from Ready/InReview, WallClockCapReached ──

    /// A worktree-create failure fails a still-`Ready` task (task 25).
    #[test]
    fn ready_plus_hard_error_yields_failed() {
        assert_eq!(
            transition(TaskState::Ready, TaskEvent::HardError),
            Ok(TaskState::Failed)
        );
    }

    /// A review-time / hard-merge error fails an `InReview` task (task 25).
    #[test]
    fn in_review_plus_hard_error_yields_failed() {
        assert_eq!(
            transition(TaskState::InReview, TaskEvent::HardError),
            Ok(TaskState::Failed)
        );
    }

    /// The wall-clock deadline fails a task from each active state (task 25).
    #[test]
    fn wall_clock_cap_reached_fails_from_each_active_state() {
        for state in [TaskState::Ready, TaskState::InProgress, TaskState::InReview] {
            assert_eq!(
                transition(state, TaskEvent::WallClockCapReached),
                Ok(TaskState::Failed),
                "WallClockCapReached should fail an active {state:?} task"
            );
        }
    }

    /// `WallClockCapReached` is NOT legal from `New` — the deadline starts at
    /// dispatch, so an un-started task cannot time out (task 25).
    #[test]
    fn wall_clock_cap_reached_is_illegal_from_new() {
        assert_eq!(
            transition(TaskState::New, TaskEvent::WallClockCapReached),
            Err(IllegalTransition {
                from: TaskState::New,
                event: TaskEvent::WallClockCapReached,
            }),
            "the wall-clock deadline only applies once a task is dispatched"
        );
    }

    // ── IllegalTransition Display ─────────────────────────────────────────────

    /// The error message produced by `IllegalTransition` is human-readable.
    #[test]
    fn illegal_transition_display_is_informative() {
        let err = IllegalTransition {
            from: TaskState::Done,
            event: TaskEvent::Dispatched,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Done"),
            "display should mention the state: {msg}"
        );
        assert!(
            msg.contains("Dispatched"),
            "display should mention the event: {msg}"
        );
    }

    // ── Task fsm-merge-conflict-event assertions (RED: expect these to fail until GREEN) ──

    /// `(InReview, MergeConflict) → Failed` (the single legal transition for the new event).
    #[test]
    fn in_review_plus_merge_conflict_yields_failed() {
        assert_eq!(
            transition(TaskState::InReview, TaskEvent::MergeConflict),
            Ok(TaskState::Failed)
        );
    }

    /// `MergeConflict` is illegal from every non-InReview state (FSM totality).
    #[test]
    fn merge_conflict_is_illegal_from_non_in_review_states() {
        use TaskState::*;
        for &state in &[New, Ready, InProgress, Done, Failed] {
            assert_eq!(
                transition(state, TaskEvent::MergeConflict),
                Err(IllegalTransition {
                    from: state,
                    event: TaskEvent::MergeConflict,
                }),
                "MergeConflict must be rejected from {state:?}"
            );
        }
    }

    // ── Task fsm-skipped-state assertions ─────────────────────────────────────

    /// `DependencyFailed` moves a task from each active state to `Skipped`.
    #[test]
    fn each_active_state_plus_dependency_failed_yields_skipped() {
        for state in [
            TaskState::New,
            TaskState::Ready,
            TaskState::InProgress,
            TaskState::InReview,
        ] {
            assert_eq!(
                transition(state, TaskEvent::DependencyFailed),
                Ok(TaskState::Skipped),
                "DependencyFailed should skip an active {state:?} task"
            );
        }
    }

    /// `DependencyFailed` is illegal from every terminal state (FSM totality).
    #[test]
    fn dependency_failed_is_illegal_from_terminal_states() {
        use TaskState::*;
        for &state in &[Done, Failed, Skipped] {
            assert_eq!(
                transition(state, TaskEvent::DependencyFailed),
                Err(IllegalTransition {
                    from: state,
                    event: TaskEvent::DependencyFailed,
                }),
                "DependencyFailed must be rejected from terminal {state:?}"
            );
        }
    }

    /// `Skipped` is a terminal state.
    #[test]
    fn skipped_is_terminal() {
        assert!(
            is_terminal(TaskState::Skipped),
            "Skipped must be a terminal state"
        );
    }
}
