# Makina Plan 0014 — Failure Reasons & Log Access

Surface *why* a task failed — using the reason the supervisor already records —
in the task view, make the error pane discoverable, and add a key to open a
task's log file.

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

## 0047 — Thread the failure reason to `TaskView`

### thread-failure-reason — Classify the failure and carry it to the view

The supervisor records `(TaskId, String)` reasons in `RunReport.failed_tasks`
(`supervisor.rs:372`) but `TaskView` (`api.rs:168`) drops them. Add a typed
reason to the view and populate it at the failing transition.

**Steps:**

1. In `crates/makina-core/src/api.rs`, add near `TaskState` (`api.rs:124`):

   ```rust
   pub enum FailureKind { GateCap, ReviewCap, MergeConflict, HardError, WallClockCap, Cancelled }
   pub struct FailureReason { pub kind: FailureKind, pub message: String }
   ```

   and add `pub failure_reason: Option<FailureReason>` to `TaskView`
   (`api.rs:168`). `None` unless the task is `Failed`.

2. In `crates/makina-core/src/actors/supervisor.rs`, at each site that moves a
   task to `Failed` and records a reason, classify into a `FailureKind`:
   gate-cap → `GateCap`; reviewer-exhaustion → `ReviewCap`; the merge-conflict
   path that reuses `ReviewCapReached` (`supervisor.rs:40,108`) → `MergeConflict`;
   `HardError` arms (`:23,:36,:41`) → `HardError`; `WallClockCapReached` (`:114`)
   → `WallClockCap`; explicit cancel → `Cancelled`. Store the classified reason on
   the task record so the `TaskView` builder can read it; keep the human string as
   `message`.

3. Ensure the `TaskView` snapshot builder copies the stored reason into
   `failure_reason` for `Failed` tasks.

4. Add tests:

   ```rust
   #[test]
   fn task_view_carries_failure_reason() { /* NoopBackend: drive a task to Failed via gate cap; assert TaskView.failure_reason == Some(GateCap) with non-empty message */ }
   #[test]
   fn merge_conflict_classified_distinctly() { /* conflict path => FailureKind::MergeConflict, not ReviewCap */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; `TaskView` exposes `failure_reason`; gate-cap,
  review-cap, merge-conflict, hard-error, and wall-clock failures classify to
  distinct `FailureKind`s; cargo test/clippy/fmt green.

### persist-failure-reason — Carry the reason through the run snapshot

Plan 0010 persists finished-run state; the reason must survive a reopen.

**Steps:**

1. In `crates/makina-core/src/persist.rs`, add `failure_reason` to the persisted
   per-task state record, serialized with `#[serde(default)]` so older snapshots
   still load.

2. Populate it when writing the snapshot and read it back into the reconstructed
   `TaskView` when a finished run is reopened.

3. Add a test:

   ```rust
   #[test]
   fn snapshot_roundtrips_failure_reason() { /* persist+reload; failure_reason survives; a snapshot lacking the field still deserialises */ }
   ```

- **Depends on:** thread-failure-reason
- **Done when:** the test passes; a reopened finished run shows the same failure
  reason it had live; pre-0014 snapshots still load; cargo test/clippy/fmt green.

---

## 0048 — Render the failure reason

### render-failure-reason — Show why a task failed, inline

**Steps:**

1. In `crates/makina/src/ui.rs`, in the task **detail** block (the one showing
   `gate ×n · review ×m` from plan 0012), when `task.failure_reason` is `Some`,
   push a red line `failed: {label} — {message}` mapping `FailureKind` to a short
   label (`gate cap`, `review cap`, `merge conflict`, `hard error`,
   `wall-clock cap`, `cancelled`).

2. In the task-row rendering near `task_state_badge` (`ui.rs:1521`), for a
   `Failed` task append a compact reason label after the badge when the column has
   room (e.g. `[✗ failed] gate cap`); truncate the message, never the label.

3. Add a test:

   ```rust
   #[test]
   fn failed_detail_renders_reason() { /* render detail for a Failed task w/ MergeConflict; assert a red "failed: merge conflict" line appears */ }
   ```

- **Depends on:** thread-failure-reason
- **Done when:** the test passes; a failed task's detail shows a red reason line
  and the row shows a compact reason tag; cargo test/clippy/fmt green.

---

## 0049 — Error-pane discoverability + open-log

### surface-errors-and-logs — Advertise `[e]`, badge unseen, open `[L]` log

**Steps:**

1. In `crates/makina/src/ui.rs`, add `[e] errors` to the status-bar hint string
   (coordinate with 0012/0013). When the error ring buffer has messages appended
   since the pane was last opened, render a count badge (`[e] errors(3)`) in a
   warn colour. Track an "unseen since open" flag in `app.rs`; clear it when the
   pane opens.

2. In `crates/makina/src/event.rs`, bind `L` (normal mode, main focus) to open
   the focused task's log. Derive the path via `log.rs`'s path helper
   (`.makina/runs/{run_id}/logs/{task}.log`). To open: tear down the terminal
   (reuse `tui.rs` teardown), spawn `$PAGER` (fallback `less`, then `more`) on the
   file and wait, then restore the terminal and force a redraw. If the file is
   absent, emit a status message (`no log for this task yet`).

3. Add tests:

   ```rust
   #[test]
   fn status_bar_advertises_errors_key() { /* assert status bar contains "[e]"; with unseen errors a count shows */ }
   #[test]
   fn open_log_resolves_expected_path() { /* assert the derived path equals .makina/runs/{run}/logs/{task}.log for a known run/task (path logic unit-tested without spawning a pager) */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; `[e]` is advertised with an unseen-count badge;
  `L` opens the focused task's log in `$PAGER` (or reports its absence) and the
  derived path matches `log.rs`; cargo test/clippy/fmt green.

---

**End of plan 0014 TASKS.** When every "Done when" bullet is green, a failed task
explains itself inline, the error pane is discoverable, and a task's full log is
one keystroke away.
