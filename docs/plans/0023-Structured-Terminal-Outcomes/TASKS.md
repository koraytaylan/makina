# Makina Plan 0023 — Structured Terminal Outcomes

Stop the scheduler guessing why tasks failed: drivers return a typed
`TerminalOutcome { state, cause }`, merge conflicts are recorded as merge
conflicts (with git's conflict details preserved instead of discarded), and
`RunReport` carries the typed cause so plan 0014's view mapping becomes a
direct conversion instead of string classification.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0072 — `TerminalOutcome` type

### define-terminal-cause — Add `TerminalCause`/`TerminalOutcome` + legacy `Display`

**Steps:**

1. In `crates/makina-core/src/actors/supervisor.rs`, near `RunReport`
   (`supervisor.rs:368`), add:

   ```rust
   pub enum TerminalCause { Completed, GateCap, ReviewCap, MergeConflict { details: String }, WallClock, Cancelled, HardError { message: String } }
   pub struct TerminalOutcome { pub state: TaskState, pub cause: TerminalCause }
   ```

   with derives matching `RunReport` (`Debug, Clone, PartialEq, Eq`).

2. Implement `Display for TerminalCause` reproducing today's literal reason
   strings: `GateCap → "gate-cap-reached"`, `ReviewCap →
   "review-cap-reached"` (`supervisor.rs:1251–1255`), `WallClock →
   "wall-clock-cap-reached"` (`supervisor.rs:1347`), `HardError → message`,
   `MergeConflict → "merge-conflict"`, `Cancelled → "cancelled"`,
   `Completed → "completed"`. Document the 0014 `FailureReason` mapping
   contract on the enum (see ARCHITECTURE.md, 0073).

3. Add a unit test:

   ```rust
   #[test]
   fn display_matches_legacy_literals() { /* GateCap/ReviewCap/WallClock Display equals the exact pre-plan strings; HardError displays its message */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; the types exist with the legacy `Display`;
  cargo test/clippy/fmt green.

### rewire-driver-and-scheduler — Drivers state their cause; the scheduler stops counter-guessing

The scheduler infers reasons from `review_iterations`
(`supervisor.rs:1248–1258`), mislabeling merge conflicts as
`"gate-cap-reached"` and post-rejection gate caps as `"review-cap-reached"`,
and the conflict arm discards git's details (`let _ = details;`,
`supervisor.rs:1799`).

**Steps:**

1. Change `task_driver` (`supervisor.rs:1539`) to return
   `Result<TerminalOutcome, String>`. Set the cause at each `Ok` exit:
   `{ Done, Completed }` after a clean merge (`supervisor.rs:1786`);
   `{ Failed, GateCap }` at the gate cap (`supervisor.rs:1677`);
   `{ Failed, ReviewCap }` at the review cap (`supervisor.rs:1874`); and
   `{ Failed, MergeConflict { details } }` in the conflict arm — deleting
   `let _ = details;` (`supervisor.rs:1799`). Hard-error paths keep returning
   `Err(String)`.

2. In `scheduler` (`supervisor.rs:1055`): retype the `JoinSet`
   (`supervisor.rs:1061`) and `failed_tasks` (`supervisor.rs:1075`) for
   `TerminalOutcome`/`TerminalCause`; in the `Ok(Ok(_))` arm delete the
   `review_iterations` inference (`supervisor.rs:1248–1258`) and push
   `outcome.cause` for `Failed` outcomes; wrap the `Ok(Err(e))` arm's string
   as `HardError { message: e }` (`supervisor.rs:1299`); push `WallClock` in
   the timeout arm (`supervisor.rs:1347`); wrap the fill-phase
   `advance_to_ready` failure (`supervisor.rs:1150`) as `HardError`. The
   aborted-join arm (`supervisor.rs:1355–1364`) still records nothing —
   `Cancelled` is its reserved typed slot.

3. Change `RunReport.failed_tasks` to `Vec<(TaskId, TerminalCause)>`
   (`supervisor.rs:377`) and update its doc comment; fix every construction
   site (including the inline scheduler-recovery test around
   `supervisor.rs:2331`) and adapt the string assertions in
   `crates/makina-core/tests/continue_on_failure.rs`
   (`continue_on_failure.rs:416–440,511–534`) to
   `matches!(cause, TerminalCause::HardError { .. })` plus a non-empty
   `to_string()` check.

4. Add tests (reuse the conflicting-worktree setup from
   `tests/squash_merge.rs` and the per-task-keyed backends from
   `tests/continue_on_failure.rs`):

   ```rust
   #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
   async fn merge_conflict_records_merge_conflict_cause() { /* two tasks edit the same file; the conflicted task's cause is MergeConflict{..}, NOT GateCap, despite review_iterations == 0 */ }
   #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
   async fn gate_cap_after_review_rejection_records_gate_cap() { /* reviewer rejects once, gates then never pass; cause is GateCap, NOT ReviewCap, despite review_iterations == 1 */ }
   ```

- **Depends on:** define-terminal-cause
- **Done when:** both tests pass; no code path derives a cause from
  iteration counters; `failed_tasks` is typed; cargo test/clippy/fmt green.

---

## 0073 — Thread the cause to consumers

### thread-cause-to-consumers — Causes in transition logs; conflict details surfaced end-to-end

**Steps:**

1. Add a `cause = %…` field to the terminal `tracing::info!(… "task state
   transition")` records in the driver: gate cap (`supervisor.rs:1671–1676`),
   merge conflict (`supervisor.rs:1813–1818`), review cap
   (`supervisor.rs:1868–1873`) — so per-task logs name the real cause. Keep
   `tests/supervisor_tracing_transitions.rs` green (extend its shape
   assertions for the new field).

2. Prove the details survive the whole stack: an integration test drives a
   real squash-merge conflict through `run_graph` and asserts the report
   entry is `MergeConflict { details }` with non-empty `details` containing
   git's conflict output (the merger guarantees it, `merge.rs:144–148`).

3. Wire the 0014 seam, handling both orderings (0014 is currently
   unimplemented — no `FailureReason` in the crates): if 0014's
   `FailureReason` exists by execution time, replace its string classifier
   with `From<TerminalCause> for FailureReason` (mapping per
   ARCHITECTURE.md); otherwise verify the mapping contract documented on
   `TerminalCause` (define-terminal-cause, step 2) names every variant so
   0014 can implement the `From` directly.

4. Add a test:

   ```rust
   #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
   async fn conflict_details_reach_run_report() { /* end-to-end conflict; failed_tasks carries MergeConflict{details} with non-empty git conflict output */ }
   ```

- **Depends on:** rewire-driver-and-scheduler
- **Done when:** the test passes; terminal transition log records carry the
  cause; conflict details are observable in `RunReport` (nothing discards
  them); the 0014 mapping is either implemented (`From<TerminalCause>`) or
  fully specified on the type; cargo test/clippy/fmt green.

---

**End of plan 0023 TASKS.** When every "Done when" bullet is green, the
engine reports why each task ended instead of guessing from counters, merge
conflicts are named (with their details intact for the reconciliation seam),
and the view layer can map causes losslessly.
