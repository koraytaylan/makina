# Architecture — Plan 0021 (deltas)

> Edits in `crates/makina-acp/src/transport.rs`, `crates/makina-acp/src/
> client.rs`, `crates/makina-acp/src/backend.rs`, `crates/makina-acp/src/
> protocol.rs`, `crates/makina-acp/src/error.rs`, plus `crates/makina-acp/
> Cargo.toml` (promote `tokio-util` to a real dependency). Line numbers are
> hints; locate by symbol.

## 0065 — Transport timeout policy

Today no operation in the crate has a deadline: `send_request` parks on
`rx.await` (`transport.rs:192`), `handshake` (`client.rs:372–419`) runs two
unbounded requests, and `AcpSession::terminate` (`backend.rs:500–514`) awaits
`reclaim_client()` → the worker's oneshot → the worker's
`stream.next().await` (`backend.rs:566`).

Edits:

- **New `Timeouts` on `AcpCommand`** (`client.rs:69`, next to the other
  builder fields):

  ```rust
  #[derive(Debug, Clone)]
  pub struct Timeouts {
      /// initialize + session/new combined (default 30s).
      pub connect: Duration,
      /// Any non-prompt request (default 60s).
      pub request: Duration,
      /// Max silence inside a turn before it is declared stalled (default 300s).
      pub turn_inactivity: Duration,
      /// How long terminate() waits for the worker before group-killing (default 5s).
      pub terminate: Duration,
  }
  ```

  `AcpCommand` gains `pub timeouts: Timeouts` (with `Default`), and a
  `#[must_use] fn timeouts(mut self, t: Timeouts) -> Self` builder. The struct
  is threaded into the transport via `AcpCommand::transport_inputs`
  (`client.rs:164–171`) / `Transport::new` so `send_request` can read it.

- **New `AcpError::Timeout { what: String, after: Duration }`** in `error.rs`,
  mapped to `BackendError::Transport` in `map_error` (`backend.rs:89–99`,
  the catch-all arm already covers new variants — add it explicitly to the
  handoff table in the doc comment).

- **Apply `connect`** by wrapping the body of `handshake` (`client.rs:372`) in
  `tokio::time::timeout(timeouts.connect, …)`.

- **Apply `request`** inside `TransportSender::send_request`
  (`transport.rs:157`): wrap the `rx.await` in
  `tokio::time::timeout(deadline, rx)` — *except* for
  `METHOD_SESSION_PROMPT`, which passes `None` (the turn-inactivity timer
  governs it). Plumb as an `Option<Duration>` parameter on an internal
  `send_request_with_deadline`, with the public `send_request` defaulting to
  `timeouts.request`. On timeout, remove the pending entry (so the oneshot
  cannot fire into nothing) and return `AcpError::Timeout`.

- **Apply `turn_inactivity` in `PromptStream`** (`client.rs:809–819`): add an
  `inactivity: Pin<Box<tokio::time::Sleep>>` field, armed when the stream is
  built and **reset on every received notification and on the prompt-response
  resolving** (`poll_next`, `client.rs:874–938`). When it fires while
  `Streaming`, transition to `Done` and yield
  `Err(AcpError::Timeout { what: "turn inactivity", .. })`. This is exactly the
  receive-side timer plan 0015 assumes; 0015's driver-level `caps.idle_secs`
  can later map onto it.

- **`terminate` races the reclaim** (`backend.rs:500–514`): store
  `pgid: Option<u32>` on `AcpSession` at construction (`from_client`,
  `backend.rs:402–413`; add a `pub(crate) fn pgid(&self) -> Option<u32>`
  accessor on `AcpClient`). Then:

  ```rust
  async fn terminate(&mut self) -> Result<(), BackendError> {
      if tokio::time::timeout(self.timeouts.terminate, self.reclaim_client())
          .await
          .is_err()
      {
          // Worker is wedged on the agent. Kill the group directly — the pgid
          // is reachable without the client — then forget the pending return.
          if let Some(pgid) = self.pgid.take() {
              crate::client::kill_group_now(pgid); // killpg(SIGKILL), factored
              crate::reaper::deregister(pgid as i32);
          }
          self.pending_return = None; // worker will error out and drop the client
          self.client = None;
          return Ok(());
      }
      // …existing shutdown path…
  }
  ```

  `kill_group_now` is the `killpg(SIGKILL)` branch factored out of
  `group_kill_force` (`client.rs:670–683`) so it is callable without a
  `&mut Child`. The worker's subsequent `stream.next()` observes EOF, errors,
  and drops the dead client (its `Drop` group-kill hits an already-killed,
  still-unreaped group — benign; the recycled-pgid hazard is the *reaped* case
  fixed in 0068).

## 0066 — Turn hygiene: cancel, drain, session-id checks

Today consumer-drop just stops forwarding (`backend.rs:606–608`); the agent
keeps generating into the unbounded notification channel
(`transport.rs:439–445`) and the next `PromptStream` replays those chunks
(`client.rs:887`). `METHOD_SESSION_CANCEL` (`protocol.rs:54`) is never sent;
`notif.session_id` is parsed but discarded (`client.rs:888–890`, `:912–915`).

Edits:

- **`AcpClient::cancel_turn`** (new, `client.rs`): send the
  `session/cancel` notification with the existing `CancelParams`
  (`protocol.rs:425–431`) via a cloned sender. Fire-and-forget
  (`send_notification`), errors logged not propagated.

- **`run_turn` cancels + drains on abandonment** (`backend.rs:548–617`): when
  `event_tx.send(event).await` fails (consumer dropped), instead of returning
  immediately:
  1. call `client.cancel_turn()` (via a sender clone captured before the
     `stream` borrow, or by restructuring so the cancel is sent after `stream`
     drops);
  2. keep driving `stream` to its end — `TurnComplete` (agents answer a
     cancelled turn with `stopReason: "cancelled"`, already modelled at
     `protocol.rs:471–472`) or error — discarding items. The 0065
     turn-inactivity timer bounds this drain, so a misbehaving agent cannot
     wedge the worker.
  The mock agent already accepts `session/cancel`
  (`tests/common/mod.rs:350`), so this is testable without a real CLI.

- **Drain stale notifications before each prompt** (`AcpClient::prompt`,
  `client.rs:581–611`): before sending `session/prompt`, loop
  `self.transport.notifications_mut().try_recv()` until `Empty`, counting and
  `tracing::warn!`-ing discarded items. With the cancel+drain above this is
  belt-and-braces; it also covers updates that arrive between turns.

- **Check `session_id`** in `PromptStream::poll_next`: give `PromptStream` a
  `session_id: String` field (cloned at `prompt()` time) and skip — with a
  `tracing::warn!` — any notification whose `notif.session_id` differs, at both
  delivery sites (`client.rs:887–894` and the post-response drain at
  `client.rs:912–915`).

## 0067 — Transport hardening: ids, buffers, write path

Edits, in dependency order:

- **`RequestId` enum** (`protocol.rs`):

  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
  #[serde(untagged)]
  pub enum RequestId { Num(u64), Str(String) }
  ```

  `IncomingMessage.id` (`protocol.rs:81`) becomes `Option<RequestId>`;
  `IncomingKind::Response { id: RequestId }` (`protocol.rs:114–117`). The
  pending map stays `HashMap<u64, …>` — we only *send* numeric ids — so
  `route_message` matches a waiter only for `RequestId::Num`; a `Str`-id
  response is a stray. `OutgoingResponse` / `OutgoingErrorResponse`
  (`protocol.rs:207–249`) carry `RequestId` and **echo the inbound id
  verbatim** — this is what makes a string-id
  `session/request_permission` answerable (`transport.rs:448`,
  `route_message`'s `req_id`). The noise-skip path (`transport.rs:388–393`)
  then no longer swallows string-id requests.

- **Capped line length**: promote `tokio-util` from dev-dependency
  (`makina-acp/Cargo.toml:32`) to `{ workspace = true, features = ["codec"] }`
  (it is already a workspace dependency, root `Cargo.toml:48`) and replace
  `BufReader::new(reader).lines()` in `read_loop` (`transport.rs:366`) with
  `FramedRead::new(reader, LinesCodec::new_with_max_length(MAX_LINE_BYTES))`,
  `MAX_LINE_BYTES = 8 MiB` (tool payloads are large; a no-newline flood is
  not). `MaxLineLengthExceeded` → `shared.shutdown(ReaderEnd::Io(...))` — a
  terminal transport error, not an OOM. Apply the same codec (skip-on-overflow
  rather than terminate) in `forward_stderr` (`client.rs:782–788`).

- **Bounded notification channel**: `mpsc::unbounded_channel()`
  (`transport.rs:285`) becomes `mpsc::channel(NOTIFICATION_CAPACITY)` (256,
  documented). `read_loop` sends with `.await` (`transport.rs:444`): a full
  channel **pauses the reader**, the agent's stdout pipe fills, and the agent
  throttles — backpressure end-to-end, no drops. Document why this cannot
  self-deadlock: the `PromptStream` drains notifications before polling the
  response (`client.rs:887`), abandoned turns drain via 0066, and permission
  replies no longer pass through the reader's await chain (next bullet).
  `Transport.notifications` / `notifications_mut` change receiver type
  (mechanical).

- **Dedicated writer task**: replace `SenderInner.writer:
  tokio::sync::Mutex<W>` (`transport.rs:109–122`) with
  `write_tx: mpsc::Sender<Vec<u8>>` feeding a spawned task that owns `W` and
  performs `write_all` + `flush` per message; on write error it calls
  `shared.shutdown(ReaderEnd::Io(...))` and exits. `write_message`
  (`transport.rs:239–248`) becomes an enqueue. `route_message`'s inline
  replies (`transport.rs:483`, `:519–559`) therefore never await the stdin
  pipe — the deadlock window closes. Since nothing holds `W` generically any
  more, the `W` type parameter on `Transport`/`TransportSender` and the
  `BoxedWriter` alias (`client.rs:54`) can be retired in the same change
  (mechanical, large-ish diff, keep it a dedicated commit).

- **`send_request` race fix** (`transport.rs:157–202`): after the
  pending-insert *and* successful write, re-check:

  ```rust
  if let Some(err) = self.inner.shared.ended_error() {
      // The reader may have drained pending between our check and insert.
      if self.remove_pending(id) {
          return Err(err);       // our entry was still there: nobody will wake us
      }
      // else: shutdown() already took it and sent the terminal error — rx resolves.
  }
  match rx.await { … }
  ```

  Combined with the 0065 `request` deadline this closes the hang both
  structurally and as a backstop.

## 0068 — Teardown correctness & exit diagnostics

Edits:

- **`shutdown` reaps once, then clears** (`client.rs:619–643`):

  ```rust
  group_kill(self.pgid, child);                                 // SIGTERM
  if tokio::time::timeout(KILL_GRACE, child.wait()).await.is_err() {
      group_kill_force(self.pgid, child);                        // SIGKILL
      let _ = child.wait().await;
  }
  if let Some(pgid) = self.pgid.take() {
      crate::reaper::deregister(pgid as i32);
  }
  self.child = None;
  ```

  The grace becomes a *race* instead of an unconditional sleep
  (`client.rs:632`), and clearing `child`/`pgid` makes `Drop`
  (`client.rs:688–703`) a no-op after a clean shutdown — no more raw
  `killpg(SIGKILL)` on a reaped, possibly recycled pgid. The `Drop` body keeps
  its current behaviour for the never-shutdown path, where the child is still
  ours.

- **Enrich `AgentExited`**: `spawn_transport` (`client.rs:711–773`) creates a
  `StderrTail` (`Arc<Mutex<VecDeque<String>>>`, capped at ~40 lines / 4 KiB),
  hands one clone to `forward_stderr` (push per line, pop-front on overflow)
  and one to `Transport::new` (new optional parameter; `None` for the
  duplex/test constructors). `ReaderEnd::to_error` (`transport.rs:56–64`)
  fills `AgentExited.stderr` from the tail instead of `String::new()`. The
  `status` field is filled where the child is owned: `run_turn`'s error arm
  (`backend.rs:599`) and `AcpClient` ask `child.try_wait()` via a small
  `AcpClient::exit_status_hint() -> Option<String>` and append it before
  mapping the error. `error.rs`'s `Display` already renders both fields when
  non-empty (`error.rs:53–55`).

## Test strategy

In-memory duplex tests (no subprocess) unless noted; new mock knobs go in
`tests/common/mod.rs`.

- `terminate_returns_within_deadline_on_wedged_agent`
  (`tests/backend_trait.rs`): mock completes the handshake, accepts
  `session/prompt`, then goes silent forever; `terminate()` with a short
  `Timeouts::terminate` returns `Ok` within the deadline (wrap in an outer
  `tokio::time::timeout` to fail fast instead of hanging the suite).
- `handshake_times_out_against_silent_agent` (`tests/lifecycle_errors.rs`,
  next to `agent_eof_during_handshake_is_typed_error`, `:37`): a peer that
  reads but never replies → `AcpError::Timeout` from `connect`, not a hang.
- `turn_inactivity_fires_when_agent_goes_quiet_mid_turn`: chunk, silence →
  the `PromptStream` yields `Err(Timeout)` and ends; no `TurnComplete`.
- `second_turn_sees_no_stale_chunks_after_early_drop`
  (`tests/backend_trait.rs`): the 100-chunk pattern from
  `early_drop_of_response_stream_reclaims_client` (`:329–361`), then a second
  prompt; assert the second turn's first chunk is `chunk-0 ` and the assembled
  text is exactly one fresh response — today this would replay turn 1's
  backlog.
- `abandoned_turn_sends_session_cancel`: recording mock asserts one
  `session/cancel` line with the right `sessionId` arrives after the early
  drop (the mock already routes the method, `tests/common/mod.rs:350`).
- `notification_with_wrong_session_id_is_ignored`: inject a chunk with
  `sessionId: "other"` mid-turn; it must not surface as a `Text` item.
- `string_id_permission_request_gets_reply` (`transport.rs` inline tests,
  next to `permission_request_mid_turn_is_answered_and_audited`, `:745`): the
  permission request uses `"id": "perm-1"`; the reply must echo the string id
  verbatim and the audit entry must still record.
- `oversized_line_fails_transport_not_memory` (`transport.rs` inline): write
  `MAX_LINE_BYTES + 1` bytes with no newline; pending request fails with a
  transport error promptly (bounded memory; no hang).
- `send_request_after_late_eof_errors_not_hangs` (`transport.rs` inline):
  regression for the insert race — with the re-check in place a request
  against a half-closed pipe resolves with `AgentExited`/`Timeout`, asserted
  under an outer timeout.
- `shutdown_clears_child_and_pgid` (`tests/lifecycle_errors.rs`): after
  `shutdown()`, the client's `Debug` output contains `pgid: None` (the Debug
  impl prints it, `client.rs:276–290`); extend
  `real_subprocess_is_killed_on_shutdown` (`:240`).
- `shutdown_skips_grace_when_child_already_exited`: spawn `sh -c 'exit 0'`,
  give it a moment, then time `shutdown()` — well under `KILL_GRACE` (generous
  bound; the old code always slept ≥ 200 ms).
- `agent_exited_carries_stderr_tail_and_status`
  (`tests/lifecycle_errors.rs`): spawn `sh -c 'echo oops >&2; exit 3'`; the
  prompt error's message contains `oops` and the exit status.
- Existing suites stay green untouched in spirit:
  `tests/process_group_kill.rs` (`agent_group_kill_reaps_descendants`, `:41`),
  `tests/lifecycle_errors.rs`, `tests/backend_trait.rs`, the `transport.rs`
  inline tests.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- **Plan 0015 (idle/hang detection):** complementary by design. 0015 detects a
  quiet stream at the *orchestrator* (opt-in `caps.idle_secs`, fails the task
  with a `FailureReason`); this plan provides the *transport* deadlines and the
  cancel/drain/teardown that make 0015's step-abort safe — without 0066, the
  abort path is precisely the consumer-drop that leaks chunks into the next
  turn. No shared code is modified by both; 0015 may later map
  `caps.idle_secs` onto `Timeouts::turn_inactivity`.
- **Plan 0002/0008 (governance/audit):** the permission-answer path keeps its
  semantics; only its *plumbing* changes (writer task, verbatim id echo).
  Policy semantics move in plan 0024, which builds on the same
  `route_message` sites — coordinate the `transport.rs:451–559` region if the
  plans land close together.
- **Plan 0014:** untouched; richer `AgentExited` messages flow into its
  failure-reason rendering for free via `BackendError::Transport`.
