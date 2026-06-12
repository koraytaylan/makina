# Architecture — Plan 0014 (deltas)

> Edits in `crates/makina-core/src/api.rs`, `crates/makina-core/src/actors/
> supervisor.rs`, `crates/makina-core/src/persist.rs` (snapshot), and the TUI
> (`ui.rs`/`event.rs`/`app.rs`). Line numbers are hints; locate by symbol.

## 0047 — Thread the failure reason to `TaskView`

Today the supervisor records, per failed task, a `(TaskId, String)` reason in
`RunReport.failed_tasks` (`supervisor.rs:372`). The reason is a hard-error message
or a cap literal (`"wall-clock-cap-reached"`, gate/review-cap). Failures reach
`Failed` via `GateCapReached`, `ReviewCapReached`, `HardError`, and
`WallClockCapReached` (`supervisor.rs:23–114`). `TaskView` (`api.rs:168`) has no
field for any of this.

Edits:

- **New `FailureReason` type** in `api.rs` near `TaskState` (`api.rs:124`):

  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum FailureKind { GateCap, ReviewCap, MergeConflict, HardError, WallClockCap }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct FailureReason { pub kind: FailureKind, pub message: String }
  ```

- **`TaskView` gains `pub failure_reason: Option<FailureReason>`** (`api.rs:168`).
  `None` for any non-`Failed` task.

- **Populate at the failing transition.** Where the supervisor moves a task to
  `Failed` and pushes onto `failed_tasks`, also classify into a `FailureKind`:
  - gate-cap arm (`GateCapReached`) → `GateCap`;
  - reviewer-exhaustion arm → `ReviewCap`;
  - the *merge-conflict* path that currently reuses `ReviewCapReached`
    (`supervisor.rs:40,108`) → `MergeConflict`;
  - hard merge / dispatch / create errors (`HardError`, `supervisor.rs:23,36,41`)
    → `HardError`;
  - scheduler `WallClockCapReached` (`supervisor.rs:114`) → `WallClockCap`.

  (Run-control cancel is an unimplemented seam — no cancel reason reaches
  `failed_tasks` — so there is no `Cancelled` variant for now.)
  Keep the existing human `message` string as `FailureReason.message`.

- **Carry it on the event/snapshot that builds `TaskView`.** The `TaskView`
  snapshots are assembled from graph state; include the classified reason so a
  `Failed` task's view has it. (If `TaskView` is built purely from the in-memory
  graph today, store the reason on the task record at transition time and read it
  when building the view.)

- **Persisted snapshot (plan 0010).** Add `failure_reason` to the persisted
  task-state record in `persist.rs` so a reopened finished run shows the reason
  too. Serde with `#[serde(default)]` so older snapshots still load.

## 0048 — Render the failure reason

- **Detail block.** Where the selected task's detail renders (the same block that,
  per plan 0012, shows `gate ×n · review ×m`), add a red line when
  `failure_reason` is `Some`:

  ```rust
  if let Some(fr) = &task.failure_reason {
      let label = match fr.kind {
          FailureKind::GateCap => "gate cap",
          FailureKind::ReviewCap => "review cap",
          FailureKind::MergeConflict => "merge conflict",
          FailureKind::HardError => "hard error",
          FailureKind::WallClockCap => "wall-clock cap",
      };
      detail_lines.push(Line::from(Span::styled(
          format!("failed: {label} — {}", fr.message),
          Style::default().fg(Color::Red))));
  }
  ```

- **Task row suffix.** In `task_state_badge` usage (`ui.rs:1521`), for `Failed`
  append a compact reason tag after the badge when space allows, e.g.
  `[✗ failed] gate cap`. Keep within the column; truncate the message, not the
  label.

## 0049 — Error-pane discoverability + open-log

- **Status-bar hint + badge.** Add `[e] errors` to the status-bar string
  (`ui.rs:405`; coordinate with 0012/0013's string). When the error ring buffer
  (`app.rs`, ~50-msg cap) has messages the user hasn't viewed (pane closed since
  last append), render the hint with a count badge, e.g. `[e] errors(3)` in a
  warn colour. Reset the "unseen" marker when the pane is opened.

- **Open-log key.** Bind `L` (normal mode, main focus) in `event.rs` to open the
  focused task's log. Derive the path from the run id + task id:
  `.makina/runs/{run_id}/logs/{task}.log` (match `log.rs`'s construction — reuse
  its path helper rather than re-deriving the format). To open:
  1. leave the alternate screen / disable raw mode (reuse the teardown in
     `tui.rs`),
  2. spawn `$PAGER` (fallback `less`, then `more`) on the file and wait,
  3. restore the terminal and force a full redraw.
  If the file does not exist yet, emit a status message (`no log for this task
  yet`) instead of opening.

## Test strategy

- `task_view_carries_failure_reason`: drive a task to `Failed` via the gate cap in
  the `NoopBackend` harness; assert the resulting `TaskView.failure_reason` is
  `Some(GateCap)` with a non-empty message.
- `merge_conflict_classified_distinctly`: a failure that today routes through
  `ReviewCapReached`-for-conflict yields `FailureKind::MergeConflict`, not
  `ReviewCap`.
- `failed_detail_renders_reason`: render the detail for a `Failed` task with a
  reason; assert the red `failed: …` line appears.
- `status_bar_advertises_errors_key`: assert the status bar contains `[e]`; with
  unseen errors it shows a count.
- `snapshot_roundtrips_failure_reason`: persist + reload a finished-run snapshot;
  assert `failure_reason` survives, and that a snapshot without the field still
  deserialises (`serde(default)`).

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- Builds on 0012's relocated detail counts (adds a sibling line) and 0010's
  persisted snapshot (adds a field, back-compatible). Shares the status-bar string
  with 0012/0013. Independent of 0009/0011. Does **not** alter the FSM events — it
  only classifies and surfaces the reason that already exists.
