# Architecture — Plan 0015 (deltas)

> Edits in `crates/makina-core/src/config.rs` (caps), `crates/makina-core/src/
> actors/supervisor.rs` (task driver / scheduler), `crates/makina-core/src/api.rs`
> (event + `FailureKind`), and the TUI (`app.rs`/`ui.rs`). Line numbers are hints;
> locate by symbol.

## 0050 — Idle watchdog

Today each task step runs under `tokio::time::timeout(Duration::from_secs(
config.caps.wall_clock_secs), …)` in the driver (`supervisor.rs:1020,1064,1195`).
There is no shorter per-step timer; `supervisor.rs:126–127` marks idle detection
FUTURE.

Config:

- In `crates/makina-core/src/config.rs`, add `idle_secs: Option<u64>` to the caps
  struct (alongside `gate_iterations`, `reviewer_iterations`, `wall_clock_secs`).
  In `validate()` (`config.rs:590`), if `Some(n)`, require `n >= 1` with a precise
  reason (`caps.idle_secs must be at least 1`), and ideally warn if
  `idle_secs >= wall_clock_secs` (the idle timer would never win).

Driver:

- In the loop that consumes the agent's `ResponseStream` for a step, wrap each
  *next-chunk* await in `tokio::time::timeout(idle, stream.next())` when
  `idle_secs` is `Some`. Any received item — response chunk, thought, or tool
  update — resets the clock (i.e. the timeout applies per `next()`, so each
  received chunk naturally restarts it). On elapse:
  - cancel the in-flight prompt / abort the step (reuse the wall-clock
    cancellation path the scheduler already uses on `WallClockCapReached`),
  - drive the task to `Failed` with reason kind `IdleTimeout` and a message like
    `no agent output for {idle_secs}s`,
  - emit a distinct event (see below) so the TUI can show it immediately.
- When `idle_secs` is `None`, the await is the bare `stream.next()` as today — zero
  behavioural change.

Events / reasons:

- Add `FailureKind::IdleTimeout` to the `FailureKind` enum introduced by plan 0014
  (merged to the base branch before this plan runs). The supervisor's failure
  classifier (plan 0014, `supervisor.rs` failing transition) maps the idle abort to it.
- Add an `Event::TaskIdle { task, idle_secs }` (or reuse the activity event
  from 0051) emitted when the watchdog fires, so the reason is visible without
  waiting for the next snapshot.

## 0051 — Live activity feedback

The TUI must show *when* a task last produced output and how close it is to the
wall-clock deadline.

- **Track last-activity per task.** In `crates/makina/src/app.rs`, on every
  exchange event that appends a chunk for a task (response/thought/tool update —
  the same events that already drive the exchange log), record a "last activity
  tick/timestamp" for that task. The app already receives `Tick` events (drives
  the spinner) — derive "Ns since last activity" from the tick delta, no new clock
  needed.
- **Track step start per task.** Record when a task entered `InProgress`/`InReview`
  so a countdown toward `wall_clock_secs` can be shown (`remaining = wall_clock −
  elapsed`).
- **Render in the exchange header.** Extend the exchange-pane title / detail header
  (`ui.rs:706–806`) for the focused in-progress task:
  - `idle 12s` (dim while small; amber once it exceeds, say, half of `idle_secs`
    when configured; red as it approaches it),
  - `· wall-clock 18m left` countdown.
  When a chunk arrives the idle counter resets to `0s` — giving the user the exact
  "thinking vs stalled" signal the bare spinner cannot.
- **Idle-timeout surfacing.** When 0050's watchdog fires, the task shows
  `[✗ failed] idle timeout` via plan 0014's renderer; the header's amber/red idle
  indicator already told the story leading up to it.

## Test strategy

- `idle_cap_validates`: `idle_secs = Some(0)` → `validate()` errors with the
  precise reason; `Some(30)` validates; `None` validates.
- `idle_watchdog_fires_on_silence`: with a test backend whose stream yields one
  chunk then stalls indefinitely and `idle_secs` small, the driver aborts the step
  and the task reaches `Failed` with `FailureKind::IdleTimeout` — **before** the
  (much larger) `wall_clock_secs` would elapse. Use a controllable/mock stream or
  `tokio::time` pause/advance so the test is deterministic and fast.
- `idle_watchdog_resets_on_activity`: a stream that emits a chunk every
  `idle_secs/2` runs to normal completion (no false idle-kill).
- `idle_disabled_matches_legacy`: with `idle_secs = None`, a stalling stream is
  bounded only by the wall-clock path (no `IdleTimeout`), proving additive
  behaviour.
- `header_shows_idle_and_countdown`: drive app state so a task has a known
  last-activity tick and step-start; render the exchange header; assert it
  contains an `idle` indicator and a wall-clock-left countdown.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.
Use `tokio::time` virtual time (pause + `advance`) for the watchdog tests — no
real sleeping.

## Interaction with prior plans

- Depends on plan 0014's `FailureReason`/`FailureKind` for rendering the idle
  outcome (adds the `IdleTimeout` variant). Reuses 0009/0010's exchange events for
  the activity signal and 0010's persisted snapshot (an idle-failed task persists
  its reason via 0014). Reuses the scheduler's existing wall-clock cancellation
  path rather than adding a second abort mechanism.
