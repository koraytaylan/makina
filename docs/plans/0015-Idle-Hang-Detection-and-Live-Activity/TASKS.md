# Makina Plan 0015 — Idle/Hang Detection & Live Activity

Catch stalled agent turns far below the 20-minute wall-clock cap with an opt-in
idle watchdog, and show live "thinking vs stalled" feedback in the TUI.

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md)
for the deltas. (The historical permission hang is already fixed via
`WorktreePolicy`; this plan targets the *remaining* stall causes.)

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}` and
  worktree `.makina/worktrees/{plan_slug}--{id}/`).
- **Depends on** lists *direct* structural prerequisites only.
- **Done when** is the verifiable acceptance check. Every task must keep
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` green.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0050 — Idle watchdog

### idle-cap-config — Add an optional `caps.idle_secs` and validate it

**Steps:**

1. In `crates/makina-core/src/config.rs`, add `idle_secs: Option<u64>` to the caps
   struct (next to `gate_iterations`, `reviewer_iterations`, `wall_clock_secs`),
   with serde default `None`.

2. In `validate()` (`config.rs:590`), if `idle_secs` is `Some(n)` require
   `n >= 1` with reason `caps.idle_secs must be at least 1`. Optionally emit a
   `tracing::warn!` (not a hard error) when `idle_secs >= wall_clock_secs`.

3. Add a test:

   ```rust
   #[test]
   fn idle_cap_validates() { /* Some(0) => Err with the precise reason; Some(30) Ok; None Ok */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; `idle_secs` parses from TOML and validates;
  `None` and a positive value both load; cargo test/clippy/fmt green.

### idle-watchdog — Abort a step that produces no output for `idle_secs`

**Steps:**

1. In `crates/makina-core/src/actors/supervisor.rs`, find the loop that consumes
   the agent `ResponseStream` for a step (near the `tokio::time::timeout(...
   wall_clock_secs ...)` sites, `supervisor.rs:1064,1195`). When `config.caps
   .idle_secs` is `Some(idle)`, wrap each next-chunk await:
   `tokio::time::timeout(Duration::from_secs(idle), stream.next())`. Any received
   item (response chunk, thought, tool update) restarts the timer (inherent to
   per-`next()` timeout).

2. On elapse: abort/cancel the in-flight step via the existing wall-clock
   cancellation path, drive the task to `Failed`, and classify the failure as
   `FailureKind::IdleTimeout` (add the variant to the `FailureKind` enum from plan
   0014; if 0014 is not yet merged, introduce the enum here) with message
   `no agent output for {idle}s`.

3. Emit `ApiEvent::TaskIdle { task, idle_secs }` (new variant in `api.rs`) when the
   watchdog fires so the TUI sees it immediately.

4. When `idle_secs` is `None`, leave the await as the bare `stream.next()` — no
   behaviour change.

5. Add tests (use `tokio::time` pause + `advance`, no real sleeping):

   ```rust
   #[test]
   fn idle_watchdog_fires_on_silence() { /* stream: one chunk then stalls; small idle_secs, large wall_clock_secs => task Failed w/ IdleTimeout before wall-clock would fire */ }
   #[test]
   fn idle_watchdog_resets_on_activity() { /* stream emits a chunk every idle_secs/2 => completes normally, no IdleTimeout */ }
   #[test]
   fn idle_disabled_matches_legacy() { /* idle_secs=None + stalling stream => no IdleTimeout (wall-clock path only) */ }
   ```

- **Depends on:** idle-cap-config
- **Done when:** all three tests pass; a stalled step fails with `IdleTimeout`
  before the wall-clock cap when `idle_secs` is set; activity resets the timer; the
  `None` path is byte-for-byte the legacy behaviour; cargo test/clippy/fmt green.

---

## 0051 — Live activity feedback

### live-activity-header — Show idle time and wall-clock countdown

**Steps:**

1. In `crates/makina/src/app.rs`, track per in-progress task: (a) a "last activity"
   tick updated on every exchange-append event (response/thought/tool update), and
   (b) the tick at which the task entered `InProgress`/`InReview`. Derive seconds
   from the existing `Tick` cadence (no new clock).

2. In `crates/makina/src/ui.rs`, extend the exchange-pane header / focused-task
   detail (`ui.rs:706–806`) to render, for an in-progress focused task:
   - `idle {n}s` — dim when small, amber past ~half of `caps.idle_secs` (when
     configured), red as it approaches it; resets to `0s` when a chunk arrives;
   - `· wall-clock {m}m {s}s left` — `wall_clock_secs − elapsed`.

3. When a task fails via the idle watchdog, rely on plan 0014's renderer to show
   `[✗ failed] idle timeout`; no extra rendering needed here.

4. Add a test:

   ```rust
   #[test]
   fn header_shows_idle_and_countdown() { /* app state with known last-activity + step-start ticks; render header; assert an "idle" indicator and a wall-clock-left countdown appear */ }
   ```

- **Depends on:** idle-watchdog
- **Done when:** the test passes; the exchange header shows live idle time
  (resetting on activity) and a wall-clock countdown for the focused in-progress
  task; an idle-failed task reads `idle timeout`; cargo test/clippy/fmt green.

---

**End of plan 0015 TASKS.** When every "Done when" bullet is green, a stalled
agent is caught in seconds (when configured), and the TUI visibly distinguishes a
thinking task from a stuck one instead of spinning identically for both.
