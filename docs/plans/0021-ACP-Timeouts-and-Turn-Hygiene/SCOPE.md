# Scope — Plan 0021

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

The `makina-acp` crate has **no deadline anywhere**. `send_request` awaits its
response oneshot forever (`transport.rs:192`); the `initialize` + `session/new`
handshake (`client.rs:372–419`) has no timeout. Worst of all,
`AcpSession::terminate` (`backend.rs:500–514`) first awaits `reclaim_client()`,
which awaits the per-turn worker's oneshot — while that worker sits in
`stream.next().await` (`backend.rs:566`). Against a wedged-but-alive agent you
cannot even **kill the subprocess** through the trait: the kill path is gated
behind an unbounded wait on the thing that is hung.

Four more verified defects compound it:

1. **Abandoned turns leak into the next one.** Nothing ever sends
   `session/cancel`: `METHOD_SESSION_CANCEL` (`protocol.rs:54`) has zero
   send-side call sites, and `lib.rs:32` misleadingly advertises it as
   "(available)". When the consumer drops the `ResponseStream` mid-turn,
   `run_turn` returns on send failure (`backend.rs:606–608`) and the client is
   reclaimed while the agent is **still generating**; its remaining
   `agent_message_chunk`s land in the transport's notification channel
   (`transport.rs:439–445`), and the next turn's `PromptStream`
   (`client.rs:887`) delivers the previous turn's chunks as the new turn's
   text. The existing test `prompt_after_early_drop_still_works`
   (`tests/backend_trait.rs:363–388`) passes **by accident**: its 3 chunks fit
   in `CHANNEL_CAPACITY = 64` (`backend.rs:77`), so that turn quietly completes
   before the second prompt.
2. **A hang race in `send_request`.** The `ended_error()` fail-fast check
   happens *before* the pending-map insert (`transport.rs:162–176`). If the
   reader EOFs and `Shared::shutdown` drains the pending map
   (`transport.rs:84–96`) between the check and the insert, and the write still
   succeeds (half-closed duplex), `rx.await` (`transport.rs:192`) blocks
   forever — no one will ever complete that oneshot.
3. **A pipe-deadlock window.** The reader task answers inbound requests
   *inline*: `route_message` sends permission/error responses via
   `sender.send_response(...).await` (`transport.rs:483`, `:519–559`), which
   takes the writer mutex (`transport.rs:244`) and awaits `write_all` on the
   child's stdin. If a large `session/prompt` is mid-write holding that mutex
   on a full stdin pipe, and the agent emits `session/request_permission` and
   stops draining stdin until it gets a reply, the reader blocks on the mutex —
   a four-way deadlock. Unbounded buffers sit next to it:
   `BufReader::lines()` has no line-length cap (`transport.rs:366`; same in
   `forward_stderr`, `client.rs:784`), and the notification channel is
   `unbounded_channel()` (`transport.rs:285`) — the bounded
   `CHANNEL_CAPACITY` in `backend.rs` does **not** propagate backpressure (a
   worker blocked on `event_tx.send` stops polling, while the reader keeps
   pumping the unbounded channel). On top, JSON-RPC ids are typed
   `Option<u64>` (`protocol.rs:81`): a **string** id — legal JSON-RPC, and the
   agent's own namespace for server→client requests like
   `session/request_permission` — fails deserialization and the line is dropped
   as noise (`transport.rs:388–393`), so the agent waits forever for its
   permission reply.
4. **Teardown can signal a stranger.** `shutdown()` (`client.rs:619–643`)
   group-kills, `child.wait().await`s (reaping the child), deregisters from the
   reaper — but never clears `self.child` / `self.pgid`. `Drop`
   (`client.rs:688–703`) then unconditionally calls `group_kill_force` → a raw
   `killpg(SIGKILL)` on a reaped, possibly OS-recycled pgid — which can kill an
   unrelated process group. `shutdown` also sleeps the full
   `KILL_GRACE = 200ms` (`client.rs:632`, `:647`) even when the child already
   exited. And when the agent *does* die, the error is mute:
   `AgentExited { status, stderr }` has the fields (`error.rs:56–61`) but its
   only construction uses empty strings (`transport.rs:58–61`), even though
   stderr is already captured line-by-line into `tracing`
   (`forward_stderr`, `client.rs:782–788`).

This plan gives the transport a timeout policy, makes abandoned turns cancel
and drain cleanly, hardens the wire layer (ids, buffers, write path), and fixes
the teardown/diagnostics path.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0065–0068):

- **0065 — Transport timeout policy.** A `Timeouts` config on `AcpCommand`
  (connect, per-request, turn-inactivity) applied to the handshake and
  `send_request`; `terminate()` races reclaim against a deadline and then falls
  through to a group-kill via the pgid — which the session can reach regardless
  of who currently holds the client.
- **0066 — Turn hygiene.** Send `session/cancel` when a turn is abandoned (the
  worker detects the consumer drop), drain the in-flight prompt response, and
  drain/discard stale notifications before each new `session/prompt`; also
  check `session_id` on notifications (parsed today, never compared —
  `client.rs:887–890`).
- **0067 — Transport hardening.** JSON-RPC id as an untagged `Num`/`Str` enum
  echoed verbatim; a capped line length; a bounded notification channel with a
  documented overflow policy; the `send_request` insert-race fix; and a
  dedicated writer task so replies never block the read loop.
- **0068 — Teardown correctness.** Clear `child`/`pgid` after the reap; skip
  the grace sleep when the child already exited; enrich `AgentExited` with the
  exit status and a stderr tail.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| No timeouts: `rx.await` forever, handshake unbounded, `terminate` gated behind a hung worker | `0065` |
| Abandoned turns never cancelled; stale chunks delivered as the next turn's text | `0066` |
| `send_request` check-before-insert race can hang forever on a half-closed pipe | `0067` |
| Reader answers permission requests inline through the shared writer mutex (deadlock window) | `0067` |
| Unbounded line buffer + unbounded notification channel (OOM; no backpressure) | `0067` |
| String JSON-RPC ids dropped as noise → agent waits forever for its permission reply | `0067` |
| `Drop` re-kills a reaped, possibly recycled pgid; unconditional 200 ms grace sleep | `0068` |
| `AgentExited` always empty despite stderr/status being available | `0068` |

## Locked decisions

- **Timeouts live on `AcpCommand`, with finite defaults.** A
  `Timeouts { connect, request, turn_inactivity }` struct, defaulted
  generously (connect 30 s, request 60 s, turn-inactivity 300 s) so healthy
  agents never notice. `session/prompt` is **exempt** from the per-request
  deadline — a turn legitimately runs for minutes; it is governed by the
  *turn-inactivity* timer instead, which resets on every `session/update`.
- **This is the transport-level complement of plan 0015, not a duplicate.**
  Plan 0015's `caps.idle_secs` watchdog is *orchestrator-level* and opt-in: it
  detects a quiet stream in the task driver and fails the task with a typed
  reason. This plan supplies the layer underneath: deadlines that make the
  `PromptStream` itself end with a typed error, and the cancel/drain/teardown
  machinery that makes 0015's "abort the step" actually release the client and
  the subprocess — today that abort *is* the consumer-drop that leaks chunks
  into the next turn (finding 2 above). 0015 may map `caps.idle_secs` onto
  `AcpCommand`'s `turn_inactivity`; this plan does not touch `caps` config or
  any UX.
- **`session/cancel` is best-effort and bounded.** On abandonment the worker
  sends the notification and drains the turn to completion under the
  turn-inactivity deadline; cancellation never blocks teardown. No new wire
  *messages* are introduced — `CancelParams` already exists, unused
  (`protocol.rs:425–431`).
- **Bounded notification channel with reader-side `await` (backpressure, not
  drops).** A full channel pauses the reader, which fills the agent's stdout
  pipe and throttles the agent end-to-end. This is safe because the
  `PromptStream` always drains notifications before polling the response
  (`client.rs:887`) and abandoned turns are now drained by 0066; audit/permission
  replies are decoupled from the reader by the 0067 writer task.
- **Ids are echoed verbatim, never coerced.** Inbound ids deserialize into an
  untagged `Num(u64) | Str(String)` enum and outbound *responses* echo exactly
  what arrived. Our own outgoing requests keep numeric ids, so the pending map
  stays keyed by `u64`.
- **`Drop` only kills when `shutdown` never ran.** `shutdown` clears
  `child`/`pgid` after the reap, making `Drop`'s group-kill a no-op on the
  normal path. The raw-`killpg` backstop remains for the abnormal path (drop
  without shutdown), where the child is still ours and unreaped.

## Out of scope

- Orchestrator-level idle detection, the `caps.idle_secs` knob, and the
  "idle 12s" live-activity UX (plan 0015 — this plan is its transport-level
  substrate).
- Permission *policy* semantics — what gets allowed/denied and how denials are
  expressed (plan 0024).
- Session reuse via `session/load` across prompts/runs (future work).
- Retry/backoff on agent spawn failures (future work — a natural follow-up
  once `terminate` is deadline-bounded, noted here so it isn't forgotten).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
