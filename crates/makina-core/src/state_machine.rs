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
//! - Counting gate or review iterations (handled by a later `termination-caps`
//!   task).
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
//! │ InProgress   │ GateFailed           │ InProgress  │ ← self-loop
//! │ InProgress   │ GatesPassed          │ InReview    │
//! │ InProgress   │ GateCapReached       │ Failed      │
//! │ InProgress   │ HardError            │ Failed      │
//! │ InReview     │ ReviewerRejected     │ InProgress  │ ← reject loop
//! │ InReview     │ ReviewerApproved     │ Done        │
//! │ InReview     │ ReviewCapReached     │ Failed      │
//! └──────────────┴──────────────────────┴─────────────┘
//! ```
//!
//! `Done` and `Failed` are terminal: no outgoing transitions exist for any
//! event.

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

    /// An unrecoverable error occurred while the Developer was working on the
    /// task (e.g. tool crash, invalid workspace state).
    ///
    /// Moves the task from [`TaskState::InProgress`] → [`TaskState::Failed`]
    /// (terminal).
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

        // ── InProgress ────────────────────────────────────────────────────────
        (InProgress, GateFailed) => Ok(InProgress), // self-loop
        (InProgress, GatesPassed) => Ok(InReview),
        (InProgress, GateCapReached) => Ok(Failed),
        (InProgress, HardError) => Ok(Failed),

        // ── InReview ──────────────────────────────────────────────────────────
        (InReview, ReviewerRejected) => Ok(InProgress), // reject loop
        (InReview, ReviewerApproved) => Ok(Done),
        (InReview, ReviewCapReached) => Ok(Failed),

        // ── Everything else (illegal) ─────────────────────────────────────────
        _ => Err(IllegalTransition { from, event }),
    }
}

// ── Introspection helpers ─────────────────────────────────────────────────────

/// Returns `true` if `state` is a terminal state from which no further
/// transitions are possible.
///
/// Currently `Done` and `Failed` are the only terminal states.
pub fn is_terminal(state: TaskState) -> bool {
    matches!(state, TaskState::Done | TaskState::Failed)
}

/// Returns the complete list of [`TaskEvent`]s that are legal in `state`.
///
/// For terminal states (`Done`, `Failed`) this is always empty.  Useful for
/// exhaustive test construction and for supervisor introspection.
pub fn legal_events(from: TaskState) -> Vec<TaskEvent> {
    use TaskEvent::*;
    use TaskState::*;

    match from {
        New => vec![DependenciesSatisfied],
        Ready => vec![Dispatched],
        InProgress => vec![GateFailed, GatesPassed, GateCapReached, HardError],
        InReview => vec![ReviewerRejected, ReviewerApproved, ReviewCapReached],
        Done | Failed => vec![],
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
    //! Cartesian product of all 6 states × all 9 events (54 pairs total).
    //! For each pair it asserts the exact expected outcome: `Ok(target)` for
    //! the 9 legal transitions and `Err(IllegalTransition)` for the remaining
    //! 45 pairs.  This single test is sufficient proof that the implementation
    //! matches the architecture diagram exactly.
    //!
    //! Supporting tests cover `is_terminal` and `legal_events` independently.

    use super::*;
    use crate::task::TaskState;

    // ── Legal transition set ──────────────────────────────────────────────────

    /// The complete set of legal `(from, event, to)` triples from the
    /// architecture state diagram.  Used both by `exhaustive_transition_table`
    /// and as documentation of intent.
    fn legal_table() -> Vec<(TaskState, TaskEvent, TaskState)> {
        use TaskEvent::*;
        use TaskState::*;

        vec![
            (New, DependenciesSatisfied, Ready),
            (Ready, Dispatched, InProgress),
            (InProgress, GateFailed, InProgress), // self-loop
            (InProgress, GatesPassed, InReview),
            (InProgress, GateCapReached, Failed),
            (InProgress, HardError, Failed),
            (InReview, ReviewerRejected, InProgress), // reject loop
            (InReview, ReviewerApproved, Done),
            (InReview, ReviewCapReached, Failed),
        ]
    }

    /// All [`TaskState`] variants — used to build the Cartesian product.
    fn all_states() -> Vec<TaskState> {
        use TaskState::*;
        vec![New, Ready, InProgress, InReview, Done, Failed]
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
        ]
    }

    // ── Exhaustive Cartesian-product test ─────────────────────────────────────

    /// For every `(state, event)` pair in the 6×9 Cartesian product:
    /// - If the pair is in the legal table → assert `Ok(expected_target)`.
    /// - Otherwise → assert `Err(IllegalTransition { from, event })`.
    ///
    /// This is the definitive proof that the FSM implementation matches the
    /// architecture diagram: 9 legal transitions and 45 illegal ones, totalling
    /// 54 assertions.
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
        assert_eq!(total, 54, "expected 6 states × 9 events = 54 pairs");

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

        assert_eq!(legal_count, 9, "expected exactly 9 legal transitions");
        assert_eq!(illegal_count, 45, "expected exactly 45 illegal transitions");
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
}
