# Makina Plan 0022 — Exchange Pane Performance & Fidelity

Make the Exchange pane survive load: batch events into frame-budgeted
renders, cache per-entry rendering with exact invalidation, bound per-entry
memory, do scroll math in wrapped-row space, surface broadcast lag as data
and repair it from the authoritative disk transcript, revive the dead scroll
inputs, and stop counting every log record as an "error".

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

## 0069 — Frame-budget render loop

### frame-budget-event-loop — Batch-drain events, render once per budget

The loop does one event → one full redraw (`event.rs:160–169`), which makes
the TUI the broadcast channel's lagging consumer (`orchestrator.rs:1269–1273`).

**Steps:**

1. In `crates/makina/src/event.rs`, factor the body at `event.rs:158–169`
   into a helper (`resolve_io` + `update` + optional status update) that
   returns the `needs_redraw` bool.

2. After the first `tokio::select!` event, drain opportunistically:
   `term_rx.try_recv()` in a loop, `api_stream.next().now_or_never()` in a
   loop (each api event still goes through `resolve_api_event`), and
   `log_rx.try_recv()` in a loop, OR-ing every `needs_redraw` into a `dirty`
   flag carried across iterations.

3. Add `const FRAME_BUDGET: Duration = Duration::from_millis(30);` beside
   `TICK_INTERVAL` (`event.rs:71`). Track `last_frame: Instant`; draw only
   when `dirty && last_frame.elapsed() >= FRAME_BUDGET`, then clear `dirty`.
   Add a `sleep_until(frame_deadline), if dirty` select arm so a pending
   frame renders when input goes quiet instead of waiting for the next tick.

4. Change the terminal-stream `None` arm (`event.rs:127–129`) to
   `Some(AppEvent::Quit)`, mirroring the api-stream arm (`event.rs:134–135`),
   so a dead crossterm `EventStream` (forwarder exit, `event.rs:107–112`)
   exits cleanly instead of spinning at 100 % CPU.

5. Add tests:

   ```rust
   #[test]
   fn drain_helper_coalesces_redraws() { /* feed N queued events through the factored helper path; assert one dirty flag, not N draws (drive via the helper, not a real terminal) */ }
   #[test]
   fn term_stream_end_maps_to_quit() { /* the None-arm translation yields AppEvent::Quit */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; a burst of queued events produces at most
  one draw per 30 ms budget; a closed terminal stream quits the loop; cargo
  test/clippy/fmt green.

### tick-dirty-gating — Tick redraws only while something animates

**Steps:**

1. In `crates/makina/src/app.rs`, add `App::has_active_animation()`: true
   when any visible task is `InProgress`/`InReview` or the focused log ends
   in an incomplete response (the spinner conditions, `ui.rs:735–736`).

2. Change the `Tick` arm (`app.rs:987–990`) to advance `self.tick` and
   return `has_active_animation()` instead of unconditional `true`, so an
   idle frame is not rebuilt every 250 ms but the 0009/0015 spinner keeps
   animating while a turn is in flight.

3. Add a test:

   ```rust
   #[test]
   fn tick_without_activity_does_not_redraw() { /* idle app: update(Tick) == false; with an InProgress task or streaming response: == true */ }
   ```

- **Depends on:** frame-budget-event-loop
- **Done when:** the test passes; an idle TUI draws no tick frames while the
  spinner still animates during active turns; cargo test/clippy/fmt green.

---

## 0070 — Exchange render cache & true scroll

### exchange-render-cache — Cache rendered lines per entry by (revision, width)

Every frame rebuilds every entry (`ui.rs:839–841`) and re-parses each
entry's full Markdown (`ui.rs:1097`) — O(n²) over a streaming turn.

**Steps:**

1. In `crates/makina/src/app.rs`, give `ExchangeEntry` a stable `seq: u64`
   (assigned in `ExchangeLog::push`, `app.rs:161–167`) and a
   `revision: u32` bumped by every mutator (`append_chunk`,
   `append_thought`, `start_tool`, `update_tool`, `complete_turn`,
   `finalize_trailing_response`) via a shared `touch()`.

2. Add `exchange_render_cache: RefCell<HashMap<u64, EntryRender>>` on `App`
   (interior mutability, same pattern as `last_scroll_max`, `app.rs:675`),
   where `EntryRender { revision, width, lines, rows }`.

3. In `crates/makina/src/ui.rs::render_exchange_pane` (`ui.rs:839–841`),
   look each entry up by `seq`; reuse on (revision, width) match, otherwise
   recompute via `exchange_entry_lines` and store. Clear the cache on
   focused-key change; prune `seq`s evicted by `EXCHANGE_LOG_CAP`.

4. Add tests:

   ```rust
   #[test]
   fn entry_revision_bumps_on_every_mutator() { /* each mutator bumps only its entry's revision */ }
   #[test]
   fn render_cache_reuses_unchanged_entries() { /* two renders: untouched seqs keep their EntryRender; the appended entry gets a fresh one */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; re-rendering a 100-entry log with one
  streaming entry re-parses exactly one entry; cargo test/clippy/fmt green.

### wrapped-row-scroll — Compute scroll_max in wrapped-row space

`scroll_max` counts logical lines (`ui.rs:847–849`) while the `Paragraph`
wraps (`ui.rs:856–858`); `render_markdown` ignores its width (`markup.rs:38`).

**Steps:**

1. Enable ratatui's `unstable-rendered-line-info` feature in the workspace
   `Cargo.toml` (`ratatui = "0.30"`, `Cargo.toml:21`);
   `Paragraph::line_count(width)` is verified at
   `ratatui-widgets-0.3.0/src/paragraph.rs:331`.

2. On each cache fill (exchange-render-cache), compute the entry's wrapped
   `rows` via `Paragraph::new(...).wrap(Wrap { trim: false })
   .line_count(inner.width)`; in `render_exchange_pane` replace the
   logical-line `scroll_max` (`ui.rs:847–849`) with
   `sum(rows).saturating_sub(pane_height)` clamped through
   `.min(u16::MAX as usize)` (removing the raw `as u16` at `ui.rs:849`).

3. Remove `render_markdown`'s dead `_width` parameter (`markup.rs:38`) and
   fix its caller (`ui.rs:1097`, which passes a hard-coded `80`).

4. Add a test:

   ```rust
   #[test]
   fn scroll_max_counts_wrapped_rows() { /* narrow TestBackend + one long line: auto-follow shows the final wrapped row on the last pane line */ }
   ```

- **Depends on:** exchange-render-cache
- **Done when:** the test passes (it fails against logical-line math);
  follow-bottom reaches the true bottom with wrapped content; cargo
  test/clippy/fmt green.

### entry-byte-cap — Bound a single entry, with the promised marker

`EXCHANGE_LOG_CAP` bounds entry *count* (`app.rs:28`) but one entry's
`String` grows forever (`app.rs:193–212`, `app.rs:238–251`); `replay.rs:15`
promises a truncation marker that does not exist.

**Steps:**

1. In `crates/makina/src/app.rs`, add
   `pub const EXCHANGE_ENTRY_BYTE_CAP: usize = 256 * 1024;` and a
   `cap_streaming_text(&mut String)` helper that keeps head (≈¼) + tail
   (≈¾) around a `… {n} KB truncated …` marker, splitting only on char
   boundaries (tail-keep precedent: `gate.rs:255`).

2. Call it from `append_chunk` and `append_thought` after each push.
   Replay (`replay.rs:30–32`) folds through the same reducer, so replayed
   oversized transcripts get the identical marker — honouring the
   `load_task_exchange` doc (`replay.rs:15`).

3. Correct the `EXCHANGE_LOG_CAP` doc comment (`app.rs:25–28`) to state both
   bounds (entry count × per-entry bytes).

4. Add a test:

   ```rust
   #[test]
   fn entry_byte_cap_keeps_head_and_tail_with_marker() { /* stream > cap into one entry: len ≤ cap+marker, original head kept, newest tail kept, marker names the KB; replayed oversized transcript shows the same marker */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; a megabyte turn cannot grow an entry past
  the cap live or on replay; cargo test/clippy/fmt green.

---

## 0071 — Replay backfill, input, and log hygiene

### surface-lag-and-backfill — Lag becomes data; disk repairs the log

`CoreApi::subscribe` hides `Lagged` (`orchestrator.rs:1269–1273`) and the
replay cache-skip (`app.rs:1196–1199`) makes any live loss permanent
(`app.rs:1338` creates the key).

**Steps:**

1. In `crates/makina-core/src/api.rs`, add the additive variant
   `Event::EventsDropped { count: u64 }`. In
   `crates/makina-core/src/orchestrator.rs::subscribe`, replace
   `filter_map(|r| r.ok())` with a `map` that converts
   `BroadcastStreamRecvError::Lagged(n)` into `Event::EventsDropped
   { count: n }` (the stream stays infallible).

2. In `crates/makina/src/app.rs`, add
   `suspect_exchanges: HashSet<(RunId, TaskId)>`; on `EventsDropped`, mark
   every key with a trailing incomplete response plus all tasks of `Running`
   runs. On `TurnComplete` for a suspect key, and on selection, queue the
   key for reload.

3. Extend the cache-skip (`app.rs:1196–1199`) to "skip if present *and not
   suspect*"; a reload replaces the log wholesale through
   `replay::load_task_exchange` and clears the mark (disk is authoritative —
   plan 0010).

4. Add tests:

   ```rust
   #[test]
   fn subscribe_surfaces_lag_as_events_dropped() { /* makina-core: overflow a small broadcast; mapped stream yields EventsDropped{count ≥ 1} */ }
   #[test]
   fn suspect_exchange_reloads_from_disk() { /* suspect key + ExchangesLoaded → log replaced, mark cleared; non-suspect cached key untouched */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; after a forced lag, ending the turn (or
  re-selecting the run) restores the full transcript from disk; cargo
  test/clippy/fmt green.

### selection-load-off-update — Move transcript IO out of the pure update

`SelectUp`/`SelectDown` synchronously read every transcript inside
`App::update` (`app.rs:922`, `app.rs:947`, `replay.rs:27`), contradicting
`app.rs:3` and `event.rs:211–212`.

**Steps:**

1. In `crates/makina/src/app.rs`, replace the direct
   `load_exchanges_for_selected_run` calls with requests recorded in
   `pending_exchange_loads: Vec<(RunId, TaskId, PathBuf)>` (path derived as
   in `app.rs:1190–1202`); `load_initial_exchanges` (`app.rs:708`) uses the
   same mechanism.

2. In `crates/makina/src/event.rs`, after each update batch drain the
   requests, run `replay::load_task_exchange` inside
   `tokio::task::spawn_blocking`, and feed results back as a new
   `AppEvent::ExchangesLoaded { key, log }` that `update` merges purely.

3. Add a test:

   ```rust
   #[test]
   fn select_up_does_no_io_in_update() { /* SelectUp queues a pending load; exchange_logs unchanged until ExchangesLoaded is applied */ }
   ```

- **Depends on:** surface-lag-and-backfill
- **Done when:** the test passes; `app.rs` no longer calls
  `replay::load_task_exchange` (grep: its only callers live in
  `event.rs`/`replay.rs`), so `App::update` performs no filesystem access;
  cargo test/clippy/fmt green.

### mouse-capture-and-page-keys — Make the scroll subsystem reachable

Wheel events are translated (`event.rs:447–451`) but capture is never
enabled (`Tui::init`, `tui.rs:53–61`); no keyboard scroll exists
(`event.rs:498–522`), so `app.rs:838–871` is dead code in production.

**Steps:**

1. In `crates/makina/src/tui.rs`, add `EnableMouseCapture` to `Tui::init`'s
   `execute!` (`tui.rs:57`) and `DisableMouseCapture` to both `Tui::restore`
   (`tui.rs:74–78`) and `restore_terminal` (`tui.rs:153–156` — the panic
   hook at `tui.rs:112–123` and the signal reaper both call it). Note
   Shift-modifier selection in the module docs.

2. In `crates/makina/src/event.rs`, bind `PageUp`/`PageDown` to new
   `AppEvent::ScrollPageUp/ScrollPageDown` (step = `last_pane_height`, a new
   `Cell<u16>` recorded by the render pass beside `last_scroll_max`,
   `ui.rs:853`), `Home` to top (auto-follow off), `End` to bottom
   (re-engage auto-follow via `scroll_down(last_scroll_max)`).

3. Add a test:

   ```rust
   #[test]
   fn page_and_home_end_keys_translate_to_scroll() { /* PageUp/PageDown/Home/End map to the new scroll events in the normal keymap */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; wheel scrolling works in a real terminal,
  paging works with capture disabled, and every restore path disables
  capture; cargo test/clippy/fmt green.

### env-filter-and-honest-title — Filter noise; count levels honestly

No `EnvFilter` exists (`main.rs:86–89`), `TuiLogLayer` forwards every level
(`log.rs:382–396`), every record becomes an `ErrorMessage`
(`event.rs:139–143`), and the title calls them all errors (`ui.rs:732–734`)
— the startup `info!` (`main.rs:175`) alone yields "Exchange (1 errors)".

**Steps:**

1. In `crates/makina/src/main.rs`, compose
   `EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))`
   onto the registry (`main.rs:86–89`; tracing-subscriber `env-filter`
   feature).

2. Filter the TUI channel layer to WARN+:
   `tui_layer.with_filter(LevelFilter::WARN)` — `TuiLogLayer` itself stays
   level-agnostic.

3. In `crates/makina/src/ui.rs`, rewrite the badge (`ui.rs:732–734`) to
   count `error_messages` by `ErrorLevel` with correct grammar:
   `(1 error)`, `(2 errors, 1 warning)`; only `ErrorLevel::Error` counts as
   an error. Coordinate the string with plan 0014's badge work.

4. Add tests:

   ```rust
   #[test]
   fn exchange_title_counts_levels_honestly() { /* one Warn, zero Error → "(1 warning)"; never "(1 errors)" */ }
   #[test]
   fn tui_channel_drops_info_records() { /* with the WARN filter, an info! event never reaches log_rx */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; a fresh launch shows a clean "Exchange"
  title; `RUST_LOG=debug` re-enables verbose file logging without flooding
  the pane; cargo test/clippy/fmt green.

---

**End of plan 0022 TASKS.** When every "Done when" bullet is green, the pane
keeps up with a streaming agent at a fixed frame budget, memory is bounded
per entry and per log, "bottom" means the bottom, lost events are visible
and healed from disk, scrolling actually works, and the title only reports
real errors.
