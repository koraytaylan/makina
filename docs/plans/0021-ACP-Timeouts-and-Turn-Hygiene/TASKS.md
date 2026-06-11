# Makina Plan 0021 — ACP Timeouts & Turn Hygiene

Give the `makina-acp` transport a timeout policy, cancel and drain abandoned
turns so no stale chunk ever leaks into the next one, harden the wire layer
(id fidelity, bounded buffers, a decoupled write path, the `send_request`
insert race), and fix teardown so a reaped pgid is never re-killed and an
agent's death is diagnosable.

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

## 0065 — Transport timeout policy

### acp-timeouts-config — `Timeouts` on `AcpCommand`, applied to handshake and requests

No operation has a deadline today: `send_request` parks on `rx.await`
(`transport.rs:192`) and `handshake` (`client.rs:372–419`) is unbounded.

**Steps:**

1. In `crates/makina-acp/src/client.rs`, add a `Timeouts` struct (`connect`
   30 s, `request` 60 s, `turn_inactivity` 300 s, `terminate` 5 s; `Default`
   impl) and a `pub timeouts: Timeouts` field + builder on `AcpCommand`
   (`client.rs:69`). Thread it through `AcpCommand::transport_inputs`
   (`client.rs:164–171`) / `Transport::new` so the sender can read it.

2. In `crates/makina-acp/src/error.rs`, add
   `AcpError::Timeout { what: String, after: Duration }`; extend the
   `map_error` handoff table (`backend.rs:89–99`) to map it to
   `BackendError::Transport`.

3. Wrap the body of `handshake` (`client.rs:372`) in
   `tokio::time::timeout(timeouts.connect, …)`.

4. In `TransportSender::send_request` (`transport.rs:157`), wrap `rx.await`
   in a deadline (internal `send_request_with_deadline(Option<Duration>)`;
   public default = `timeouts.request`). `METHOD_SESSION_PROMPT` passes
   `None` — the turn-inactivity timer governs turns. On timeout, remove the
   pending entry before returning `AcpError::Timeout`.

5. Add tests:

   ```rust
   #[tokio::test]
   async fn handshake_times_out_against_silent_agent() { /* peer reads, never replies → AcpError::Timeout, no hang */ }
   #[tokio::test]
   async fn request_times_out_when_agent_never_responds() { /* post-handshake set_mode against a mute peer → Timeout */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass under an outer `tokio::time::timeout`;
  default behaviour for healthy agents is unchanged (full existing suite
  green); `session/prompt` is exempt from the request deadline; cargo
  test/clippy/fmt green.

### turn-inactivity-watchdog — Transport-level stall detection inside a turn

**Steps:**

1. In `crates/makina-acp/src/client.rs`, add an
   `inactivity: Pin<Box<tokio::time::Sleep>>` field to `PromptStream`
   (`client.rs:809–819`), armed from `timeouts.turn_inactivity` at
   `prompt()` time and **reset on every received notification and on the
   prompt response resolving** in `poll_next` (`client.rs:874–938`).

2. When it fires while `Streaming`: set state to `Done`, yield
   `Err(AcpError::Timeout { what: "turn inactivity", .. })`. Never emit
   `TurnComplete` after it.

3. Add a test:

   ```rust
   #[tokio::test]
   async fn turn_inactivity_fires_when_agent_goes_quiet_mid_turn() { /* one chunk then silence → Err(Timeout), stream ends, no TurnComplete */ }
   ```

4. Doc-comment the division of labour with plan 0015: this timer is the
   transport-level enforcement; 0015's opt-in `caps.idle_secs` driver watchdog
   sits above it and may map onto `Timeouts::turn_inactivity`.

- **Depends on:** acp-timeouts-config
- **Done when:** the test passes; a steadily-chunking turn longer than the
  timeout is NOT killed (resets proven by a test or by the existing streaming
  tests with a short timer); cargo test/clippy/fmt green.

### terminate-deadline-group-kill — `terminate()` can always kill the agent

`AcpSession::terminate` (`backend.rs:500–514`) awaits `reclaim_client()`,
which awaits a worker stuck in `stream.next().await` (`backend.rs:566`) — a
wedged agent makes the kill path unreachable.

**Steps:**

1. In `crates/makina-acp/src/client.rs`, factor the `killpg(SIGKILL)` branch
   of `group_kill_force` (`client.rs:670–683`) into
   `pub(crate) fn kill_group_now(pgid: u32)`; add
   `pub(crate) fn pgid(&self) -> Option<u32>` on `AcpClient`.

2. In `crates/makina-acp/src/backend.rs`, store `pgid: Option<u32>` and the
   command's `Timeouts` on `AcpSession` at `from_client`
   (`backend.rs:402–413`). In `terminate` (`backend.rs:500`), race
   `reclaim_client()` against `timeouts.terminate`; on expiry: `kill_group_now`
   the stored pgid, `reaper::deregister` it, drop `pending_return`, set
   `client = None`, return `Ok(())`. Document why the worker's later client
   drop is benign (group killed but not yet reaped).

3. Add a test:

   ```rust
   #[tokio::test]
   async fn terminate_returns_within_deadline_on_wedged_agent() { /* mock accepts session/prompt then goes mute; terminate() with a short deadline returns Ok promptly */ }
   ```

- **Depends on:** acp-timeouts-config
- **Done when:** the test passes under an outer timeout;
  `early_drop_of_response_stream_reclaims_client` and
  `prompt_after_terminate_returns_terminated` (`tests/backend_trait.rs`) still
  pass; cargo test/clippy/fmt green.

---

## 0066 — Turn hygiene: cancel, drain, session-id checks

### cancel-abandoned-turns — Send `session/cancel` and drain when the consumer drops

`METHOD_SESSION_CANCEL` (`protocol.rs:54`) has zero send-side call sites and
`lib.rs:32` misleadingly advertises it; consumer-drop just stops forwarding
(`backend.rs:606–608`) while the agent keeps generating.

**Steps:**

1. In `crates/makina-acp/src/client.rs`, add `AcpClient::cancel_turn()`:
   send the `session/cancel` notification using the existing `CancelParams`
   (`protocol.rs:425–431`) via a sender clone; best-effort (log, don't
   propagate).

2. In `run_turn` (`backend.rs:548–617`), on `event_tx.send` failure: send the
   cancel, then keep driving the `PromptStream` to `TurnComplete`
   (`stopReason: "cancelled"`, `protocol.rs:471–472`) or error, discarding
   items — bounded by the 0065 turn-inactivity timer so a misbehaving agent
   cannot wedge the worker.

3. Update the `lib.rs:32` protocol table ("(available)" → actually sent on
   abandonment) and the module docs at `backend.rs:29–47`.

4. Add tests (mock already routes the method, `tests/common/mod.rs:350`):

   ```rust
   #[tokio::test]
   async fn abandoned_turn_sends_session_cancel() { /* early drop → recording mock saw one session/cancel with the right sessionId */ }
   #[tokio::test]
   async fn second_turn_sees_no_stale_chunks_after_early_drop() { /* 100-chunk pattern from early_drop_of_response_stream_reclaims_client (tests/backend_trait.rs:329); second prompt's first chunk is "chunk-0 " and text is exactly one fresh response */ }
   ```

- **Depends on:** turn-inactivity-watchdog
- **Done when:** both tests pass; `prompt_after_early_drop_still_works`
  (`tests/backend_trait.rs:363–388`) passes for the *right* reason (drained,
  not lucky channel capacity); cargo test/clippy/fmt green.

### drain-and-check-session-id — Pre-prompt drain + session-id filtering

**Steps:**

1. In `AcpClient::prompt` (`client.rs:581–611`), before sending
   `session/prompt`, drain `notifications_mut().try_recv()` until `Empty`;
   `tracing::warn!` the discarded count when non-zero.

2. Give `PromptStream` a `session_id: String` field; at both delivery sites
   in `poll_next` (`client.rs:887–894`, `:912–915`) skip — with a warning —
   notifications whose `session_id` differs (today it is parsed and silently
   discarded, `client.rs:888–890`).

3. Add a test:

   ```rust
   #[tokio::test]
   async fn notification_with_wrong_session_id_is_ignored() { /* inject a chunk with sessionId "other" mid-turn; it must not surface */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; existing streaming tests are unaffected;
  cargo test/clippy/fmt green.

---

## 0067 — Transport hardening: ids, buffers, write path

### jsonrpc-id-fidelity — Untagged `Num`/`Str` ids, echoed verbatim

`protocol.rs:81` types ids as `Option<u64>`; a string-id
`session/request_permission` fails to parse and is dropped as noise
(`transport.rs:388–393`) — the agent waits forever for its reply.

**Steps:**

1. In `crates/makina-acp/src/protocol.rs`, add
   `#[serde(untagged)] enum RequestId { Num(u64), Str(String) }`
   (`Serialize + Deserialize + Hash + Eq`); change `IncomingMessage.id`
   (`protocol.rs:81`), `IncomingKind::Response` (`protocol.rs:114–117`),
   `OutgoingResponse.id` and `OutgoingErrorResponse.id`
   (`protocol.rs:207–249`) to use it.

2. In `transport.rs`, keep the pending map keyed by `u64` (we only send
   numeric ids); `route_message` matches a waiter for `RequestId::Num` only,
   and echoes the inbound `RequestId` verbatim in every response
   (`transport.rs:448`, `:483`, `:519–559`).

3. Add tests:

   ```rust
   #[tokio::test]
   async fn string_id_permission_request_gets_reply() { /* perm request with "id": "perm-1" → reply echoes the string id; audit entry recorded */ }
   #[test]
   fn request_id_roundtrips_num_and_str() { /* serde unit test for the untagged enum */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; `incoming_message_classification`
  (`protocol.rs:915`) is extended for string ids; cargo test/clippy/fmt green.

### cap-line-length — Bounded line decoding on stdout and stderr

**Steps:**

1. In `crates/makina-acp/Cargo.toml`, promote `tokio-util` from
   dev-dependency (`Cargo.toml:32`) to a real dependency
   `{ workspace = true, features = ["codec"] }` (already a workspace dep,
   root `Cargo.toml:48`).

2. In `read_loop` (`transport.rs:357–401`), replace
   `BufReader::new(reader).lines()` (`transport.rs:366`) with
   `FramedRead::new(reader, LinesCodec::new_with_max_length(MAX_LINE_BYTES))`,
   `MAX_LINE_BYTES = 8 * 1024 * 1024`. Overflow →
   `shared.shutdown(ReaderEnd::Io(…))` (terminal transport error, bounded
   memory).

3. Apply the same codec in `forward_stderr` (`client.rs:782–788`), but
   *skip* oversized lines rather than terminating (stderr is diagnostics).

4. Add a test:

   ```rust
   #[tokio::test]
   async fn oversized_line_fails_transport_not_memory() { /* MAX+1 bytes, no newline → pending request errors promptly */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; `non_jsonrpc_lines_are_skipped_not_fatal`
  (`transport.rs:707`) still passes; cargo test/clippy/fmt green.

### bound-notification-channel — Backpressure instead of an unbounded queue

**Steps:**

1. In `Transport::new` (`transport.rs:285`), replace `unbounded_channel()`
   with `mpsc::channel(NOTIFICATION_CAPACITY)` (256, with a doc comment
   stating the overflow policy: the reader `await`s the send
   (`transport.rs:444`), pausing reads → the agent's stdout pipe fills → the
   agent throttles; nothing is dropped).

2. Document the deadlock-freedom argument at the constant: `PromptStream`
   drains notifications before polling the response (`client.rs:887`);
   abandoned turns are drained by `cancel-abandoned-turns`; replies bypass the
   reader via the writer task.

3. Mechanically update `Transport.notifications` / `notifications_mut`
   receiver types and the `read_loop` send site.

- **Depends on:** cancel-abandoned-turns
- **Done when:** full suite green (streaming, early-drop, permission tests
  unchanged in behaviour); a doc comment records capacity + policy; cargo
  test/clippy/fmt green.

### writer-task-and-race-fix — Decouple writes from the read loop; close the insert race

**Steps:**

1. In `transport.rs`, replace `SenderInner.writer: tokio::sync::Mutex<W>`
   (`transport.rs:109–122`) with a `write_tx: mpsc::Sender<Vec<u8>>` feeding
   a spawned writer task that owns `W` (`write_all` + `flush` per message;
   on error → `shared.shutdown(ReaderEnd::Io(…))` and exit). `write_message`
   (`transport.rs:239–248`) becomes an enqueue. The reader's inline replies
   (`transport.rs:483`, `:519–559`) no longer await the stdin pipe.

2. Retire the now-unused `W` type parameter on `Transport` /
   `TransportSender` and the `BoxedWriter` alias (`client.rs:54`) —
   mechanical follow-through.

3. In `send_request` (`transport.rs:157–202`), after the pending-insert and
   successful write, re-check `ended_error()`; if ended and our pending entry
   is still present, remove it and return the terminal error (if absent, the
   shutdown drain already completed the oneshot — proceed to `rx.await`).

4. Add a test:

   ```rust
   #[tokio::test]
   async fn send_request_after_late_eof_errors_not_hangs() { /* race regression: half-closed pipe; request resolves with a terminal error under an outer timeout */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; `eof_wakes_pending_request_with_agent_exited`
  (`transport.rs:689`) and `permission_request_mid_turn_is_answered_and_audited`
  (`transport.rs:745`) still pass; cargo test/clippy/fmt green.

---

## 0068 — Teardown correctness & exit diagnostics

### reap-once-clear-pgid — Never re-kill a reaped pgid; race the grace period

`shutdown` (`client.rs:619–643`) reaps but never clears `child`/`pgid`, so
`Drop` (`client.rs:688–703`) `killpg(SIGKILL)`s a possibly recycled pgid; it
also sleeps the full `KILL_GRACE` (`client.rs:632`) even when the child is
already dead.

**Steps:**

1. In `AcpClient::shutdown`, replace the unconditional sleep with
   `tokio::time::timeout(KILL_GRACE, child.wait())`; only on expiry send
   `group_kill_force` and `wait()` again.

2. After the reap and `reaper::deregister` (`client.rs:639–641`), set
   `self.child = None` and `self.pgid = None`, making `Drop` a no-op on the
   clean path (its backstop remains for drop-without-shutdown).

3. Add tests in `tests/lifecycle_errors.rs` (real subprocess, alongside
   `real_subprocess_is_killed_on_shutdown`, `:240`):

   ```rust
   #[tokio::test]
   async fn shutdown_clears_child_and_pgid() { /* after shutdown(), format!("{client:?}") contains "pgid: None" (Debug prints it, client.rs:276) */ }
   #[tokio::test]
   async fn shutdown_skips_grace_when_child_already_exited() { /* sh -c 'exit 0'; shutdown() completes well under KILL_GRACE */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; `real_subprocess_no_zombie_after_drop`
  (`tests/lifecycle_errors.rs:260`) and `tests/process_group_kill.rs` still
  pass; cargo test/clippy/fmt green.

### enrich-agent-exited — Status + stderr tail on `AgentExited`

The fields exist (`error.rs:56–61`) but the only construction uses empty
strings (`transport.rs:58–61`), while stderr already flows through
`forward_stderr` (`client.rs:782–788`).

**Steps:**

1. In `spawn_transport` (`client.rs:711–773`), create a `StderrTail`
   (`Arc<Mutex<VecDeque<String>>>`, ~40 lines / 4 KiB cap); clone into
   `forward_stderr` (push, pop-front on overflow) and into `Transport::new`
   (new optional parameter; `None` in the duplex/test constructors).

2. In `ReaderEnd::to_error` (`transport.rs:56–64`), fill `AgentExited.stderr`
   from the tail.

3. Add `AcpClient::exit_status_hint()` (a `child.try_wait()` formatter) and
   use it where the child is owned — `run_turn`'s error arm (`backend.rs:599`)
   and `shutdown` — to fill/append `status` before mapping the error.

4. Add a test:

   ```rust
   #[tokio::test]
   async fn agent_exited_carries_stderr_tail_and_status() { /* sh -c 'echo oops >&2; exit 3' → prompt error message contains "oops" and the exit status */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; `error_mapping_follows_the_handoff_table`
  (`backend.rs:631`) still passes; the `AgentExited` `Display` renders both
  fields (`error.rs:53–55`); cargo test/clippy/fmt green.

---

**End of plan 0021 TASKS.** When every "Done when" bullet is green, a wedged
agent can always be killed within a deadline, an abandoned turn is cancelled
and fully drained so the next turn starts clean, string-id requests get their
replies, every buffer is bounded, and a dead agent's exit status and last
stderr lines arrive in the error instead of an empty string.
