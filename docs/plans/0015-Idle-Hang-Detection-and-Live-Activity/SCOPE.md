# Scope — Plan 0015

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

An agent turn that stalls — a model API timeout, a dropped pipe, a subprocess
wedged mid-stream — is currently invisible until the **wall-clock cap** fires.
That cap defaults to **1200 s (20 minutes)** per task (`wall_clock_secs`), and it
is the *only* backstop: the supervisor's own module docs say so explicitly —

> **Idle / heartbeat detection** (FUTURE): only the three caps above exist;
> there is no per-step idle timeout. (`supervisor.rs:126–127`)

So a hung task burns up to twenty minutes of wall-clock before the user gets any
signal, and even then the TUI gives no *live* indication that a task has gone
quiet versus is legitimately thinking. The exchange pane streams chunks while the
agent talks, but once the stream goes silent the spinner keeps spinning
identically whether the agent is composing a long answer or has died.

Two paired gaps:

1. **No idle detection below the wall-clock cap.** There is no per-step "no output
   for N seconds" timeout that would catch a stall ~40× faster than the 1200 s
   cap and emit a *useful* event instead of a silent expiry.
2. **No live activity feedback.** The TUI shows a spinner but no "last activity
   Ns ago" and no countdown toward the wall-clock deadline, so the user cannot
   distinguish "working" from "stuck."

> **Note — the permission hang is already solved.** The historical 20-minute hang
> in `trial-findings.md` was the *unanswered `session/request_permission`* case.
> That is fixed: `WorktreePolicy` auto-allows and the transport answers permission
> requests (`makina-acp/src/permission.rs`, `transport.rs`). This plan is about
> the *remaining* stall causes (API timeouts, dropped pipes, mid-stream crashes),
> which the permission fix does not cover.

This plan adds an **idle watchdog** that fires far below the wall-clock cap and a
**live activity** indicator so stalls are caught fast and shown clearly.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0050–0051):

- **0050 — Idle watchdog.** Add an optional `caps.idle_secs` and a per-step
  idle timer in the task driver: reset on every received stream chunk (response,
  thought, tool update); if it elapses with no chunk, treat the step as stalled —
  abort it and drive the task to `Failed` with a distinct reason, emitting an
  event well before the wall-clock cap. Off by default (opt-in) so existing
  behaviour is unchanged unless configured.
- **0051 — Live activity feedback.** Surface, per in-progress task, the time
  since the last stream chunk ("idle 12s") and a countdown toward the wall-clock
  deadline in the exchange/detail header, so a quiet task is visibly quiet.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| No per-step idle timeout; 20-min wall-clock cap is the only stall backstop | `0050` |
| Spinner can't distinguish "thinking" from "stalled"; no activity/countdown | `0051` |

## Locked decisions

- **Idle detection is opt-in and additive.** `caps.idle_secs` is `Option<u64>`;
  when unset, behaviour is exactly as today (wall-clock cap only). When set, the
  idle timer is the *earlier* of the two triggers. This keeps the default
  conservative — a slow-but-alive agent on a hard task is not killed prematurely.
- **Reset on any stream activity, not just responses.** Thoughts and tool updates
  count as liveness; only true silence (no chunk of any kind) advances the idle
  clock. This avoids false positives during long tool runs that emit progress.
- **Reuse `FailureReason` from plan 0014.** An idle-killed task fails with a
  dedicated `FailureKind::IdleTimeout` (add the variant), so it renders with a
  clear "idle timeout" reason via 0014's machinery rather than a generic failure.
- **Don't add a heartbeat *protocol*.** This is a *receive-side* idle timer over
  the existing response stream; it does not introduce ACP ping/pong or any new
  wire message.

## Out of scope

- A cooperative ACP heartbeat / keepalive protocol (receive-side timer only).
- Killing or restarting individual agent *processes* from the TUI beyond the
  existing cancel path.
- Cost/budget caps (separate future work).
- The failure-reason rendering machinery itself (plan 0014; this plan adds one
  variant and relies on 0014 to display it).

## Dependency note

0051's "idle timeout" rendering rides on plan 0014's `FailureReason`. The idle
*detection* (0050) is independent and can land first; the new
`FailureKind::IdleTimeout` variant should be added wherever the `FailureKind` enum
lives (0014 if merged first, else introduce the enum here and 0014 extends it).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
