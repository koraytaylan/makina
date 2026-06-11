# Architecture — Plan 0022 (deltas)

> Edits in `crates/makina/src/event.rs`, `app.rs`, `ui.rs`, `markup.rs`,
> `replay.rs`, `tui.rs`, `main.rs`, plus one additive event in
> `crates/makina-core/src/api.rs` / `orchestrator.rs` and a ratatui feature
> flag in `Cargo.toml`. Line numbers are hints; locate by symbol.

## 0069 — Frame-budget render loop

Today the loop is one event → one `App::update` → one full `tui.draw`
(`event.rs:160–169`). Under a streaming turn that is one full-frame rebuild
*per token*, which is what makes the TUI the lagging broadcast consumer.

Edits (all in `event.rs::run`):

- **Batch-drain after the first await.** Keep the existing `tokio::select!`
  for the *first* event of an iteration, then opportunistically drain
  everything already queued before rendering:

  ```rust
  let mut dirty = app_event_into_update(app, first_event).await; // resolve_io + update
  // Drain without awaiting: whatever is already buffered joins this frame.
  while let Ok(ev) = term_rx.try_recv() {
      dirty |= app_event_into_update(app, translate_terminal_event(ev, …)).await;
  }
  while let Some(Some(ev)) = api_stream.next().now_or_never() {
      dirty |= app_event_into_update(app, resolve_api_event(&app.api, ev).await).await;
  }
  while let Ok(rec) = log_rx.try_recv() {
      dirty |= app.update(AppEvent::ErrorMessageArrived { msg: error_message_from_log_record(rec) });
  }
  ```

  (`app_event_into_update` is the existing `resolve_io` + `update` + status
  pair from `event.rs:158–163`, factored into a helper so the drain arms
  share it.)

- **Render at most once per frame budget.** Add
  `const FRAME_BUDGET: Duration = Duration::from_millis(30);` next to
  `TICK_INTERVAL` (`event.rs:71`). Track `last_frame: Instant` and a `dirty`
  flag across iterations; draw only when `dirty` and the budget has elapsed.
  So a pending frame is never starved while events go quiet, give the
  `select!` a deadline arm:

  ```rust
  _ = tokio::time::sleep_until(frame_deadline), if dirty => { /* falls through to draw */ }
  ```

  Net effect: a 10 000-chunk burst becomes ~30 `update` batches and ~30
  draws, not 10 000 draws.

- **Quit when the terminal stream dies.** The forwarder task exits when
  `EventStream` ends (`event.rs:107–112`); today the `None` arm maps to
  `None` (`event.rs:127–129`) and the loop spins. Change it to
  `Some(AppEvent::Quit)`, mirroring the api-stream arm (`event.rs:134–135`).
  A TUI without terminal input is dead; exiting cleanly (teardown in
  `main.rs:251–258` still reaps runs/agents) beats a silent 100 % CPU spin.

- **Tick redraws only when something can have changed.** In `App::update`'s
  `Tick` arm (`app.rs:987–990`): advance `self.tick` and return `true` only
  when an animation is live — i.e. any visible task is `InProgress` /
  `InReview` or the focused log has a trailing incomplete response (the
  spinner conditions, `ui.rs:735–736`). Otherwise return `false`. Add
  `App::has_active_animation()` so the condition is testable and shared.

## 0070 — Exchange render cache & true scroll

Today `render_exchange_pane` rebuilds every line of every entry per frame
(`ui.rs:839–841`), `render_markdown` re-parses each entry's full accumulated
text (`ui.rs:1097`) and ignores its width argument (`markup.rs:38`), and
`scroll_max` counts logical lines (`ui.rs:847–849`) while the `Paragraph`
wraps (`ui.rs:856–858`).

Edits:

- **Entry identity + revision** (`app.rs`). `ExchangeEntry` gains
  `seq: u64` (stable id assigned by `ExchangeLog::push`, monotonic per log)
  and `revision: u32` (bumped by every mutator: `append_chunk`,
  `append_thought`, `start_tool`/`update_tool`, `complete_turn`,
  `finalize_trailing_response`). Mutators call a tiny `entry.touch()`.

- **Render cache** (`ui.rs`). A cache on `App` behind interior mutability —
  the same pattern as `last_scroll_max` (`app.rs:675`):

  ```rust
  pub struct EntryRender { pub revision: u32, pub width: u16,
                           pub lines: Vec<Line<'static>>, pub rows: u16 }
  pub exchange_render_cache: RefCell<HashMap<u64 /* seq */, EntryRender>>,
  ```

  `render_exchange_pane` looks each entry up by `seq`; on a
  (revision, width) hit it reuses `lines`/`rows`, on a miss it calls
  `exchange_entry_lines` for that one entry and stores the result. The cache
  is cleared when the focused `(RunId, TaskId)` key changes and pruned of
  `seq`s evicted by the log cap, so it stays bounded by
  `EXCHANGE_LOG_CAP`.

- **Wrapped-row scroll math** (`ui.rs` + `Cargo.toml`). Enable ratatui's
  `unstable-rendered-line-info` feature (workspace `Cargo.toml:21`);
  `Paragraph::line_count(width)` is verified present in ratatui 0.30
  (`ratatui-widgets-0.3.0/src/paragraph.rs:331`). Each cache fill computes
  `rows = Paragraph::new(lines.clone()).wrap(Wrap { trim: false })
  .line_count(inner.width) as u16`; the pane then computes

  ```rust
  let total_rows: usize = entries.map(|e| cached_rows(e)).sum();
  let scroll_max = total_rows.saturating_sub(pane_height).min(u16::MAX as usize) as u16;
  ```

  replacing the logical-line count at `ui.rs:847–849` (and neutralising the
  `as u16` truncation at `ui.rs:849` with the explicit `min`). The
  `Paragraph` keeps its `Wrap` (`ui.rs:856–858`); only the offset arithmetic
  changes. `App::scroll_down` / `effective_offset` (`app.rs:838–871`) are
  untouched — they already trust the caller's `scroll_max`.

- **`render_markdown` honours width or sheds the lie.** With wrapping
  delegated to `Paragraph` + `line_count`, the `_width` parameter
  (`markup.rs:38`) is genuinely unused — remove it and fix the one caller
  (`ui.rs:1097` passes a hard-coded `80` today) rather than keep a parameter
  that promises wrapping it does not do.

- **Per-entry byte cap** (`app.rs` + `replay.rs`). Add

  ```rust
  pub const EXCHANGE_ENTRY_BYTE_CAP: usize = 256 * 1024;
  fn cap_streaming_text(text: &mut String) { /* keep head ≈¼, tail ≈¾,
      splice "… {n} KB truncated …" on char boundaries */ }
  ```

  called from `append_chunk` (`app.rs:193–212`) and `append_thought`
  (`app.rs:238–251`) after each push. Because replay folds through the same
  reducer (`replay.rs:30–32`), replayed logs get the identical marker —
  finally implementing what the `load_task_exchange` doc promises
  (`replay.rs:15`). Correct the now-false comment above `EXCHANGE_LOG_CAP`
  (`app.rs:25–28`) to state both bounds (entry count × entry bytes).

## 0071 — Replay backfill, input, and log hygiene

### Lag surfaced and backfilled

- **`Event::EventsDropped { count: u64 }`** (additive variant,
  `api.rs` near `Event`). `CoreApi::subscribe` (`orchestrator.rs:1269–1273`)
  changes `filter_map(|r| r.ok())` to:

  ```rust
  let stream = BroadcastStream::new(rx).map(|result| match result {
      Ok(ev) => ev,
      Err(BroadcastStreamRecvError::Lagged(count)) => Event::EventsDropped { count },
  });
  ```

  The stream stays infallible; the trait contract and every other consumer
  are unaffected (the TUI's `apply_api_event` match gains one arm).

- **Suspect keys** (`app.rs`). `App` gains
  `suspect_exchanges: HashSet<(RunId, TaskId)>`. On `EventsDropped`, insert
  every key whose log has a trailing incomplete response plus every task of
  a `Running` run (we cannot know *which* events dropped, so suspect the
  in-flight set). On `ExchangeEvent::TurnComplete` for a suspect key — and
  on run selection — the key is queued for reload.

- **Reload through the IO layer, not `update`.** This also fixes finding 3.
  `App::update` stops calling `load_exchanges_for_selected_run`
  (`app.rs:922`, `app.rs:947`); instead selection changes and suspect
  turn-ends record requests in `App::pending_exchange_loads:
  Vec<(RunId, TaskId, PathBuf)>`. The event loop drains it after each
  update batch, reads the transcripts via `tokio::task::spawn_blocking`
  (wrapping the existing `replay::load_task_exchange`), and feeds the result
  back as a new `AppEvent::ExchangesLoaded { key, log }` which `update`
  merges purely — replacing the log and clearing the suspect mark. The
  skip-if-cached check (`app.rs:1196–1199`) becomes "skip if present *and
  not suspect*", so backfill repairs exactly the lagged keys and re-selection
  stays free. `app.rs:3` ("holds no IO") and `event.rs:211–212` become true
  statements again; `load_initial_exchanges` (`main.rs:217`, `app.rs:708`)
  switches to the same request/merge path.

### Input

- **Mouse capture on, everywhere off.** `Tui::init` (`tui.rs:53–61`) adds
  `EnableMouseCapture` to its `execute!`; `Tui::restore` (`tui.rs:71–79`)
  and the standalone `restore_terminal` (`tui.rs:153–156`, used by the panic
  hook `tui.rs:112–123` and the signal reaper) add `DisableMouseCapture`.
  The already-written wheel translation (`event.rs:447–451`) and scroll
  state (`app.rs:838–871`) become reachable.

- **Capture-free keyboard scrolling.** Normal keymap (`event.rs:498–522`)
  gains `PageUp`/`PageDown` → `AppEvent::ScrollPageUp/ScrollPageDown`
  (step = last rendered pane height, recorded by the render pass in a new
  `last_pane_height: Cell<u16>` beside `last_scroll_max`), `Home` → offset 0
  with auto-follow off, `End` → re-engage auto-follow. Arrows/`j`/`k` keep
  moving selection.

### Log hygiene

- **`EnvFilter`** (`main.rs:86–89`). Compose
  `EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))`
  onto the registry (tracing-subscriber `env-filter` feature). Default
  `info`; `RUST_LOG` overrides for debugging.

- **TUI channel carries WARN+ only.** Wrap the layer:
  `tui_layer.with_filter(LevelFilter::WARN)` at `main.rs:88`. `TuiLogLayer`
  itself (`log.rs:378–397`) is unchanged — filtering is the subscriber's
  job. The startup `tracing::info!` (`main.rs:175`) still reaches the run
  log file but no longer pollutes the error pane.

- **Honest title.** `render_exchange_pane`'s badge (`ui.rs:732–734`) counts
  by `ErrorLevel`: `Exchange (2 errors)`, `Exchange (1 error, 3 warnings)`,
  with singular/plural grammar; only `ErrorLevel::Error` is called an error.

## Test strategy

- `tick_without_activity_does_not_redraw`: `update(Tick)` returns `false`
  with no runs / all-terminal tasks; returns `true` while a task is
  `InProgress` or a response is streaming (spinner contract preserved).
- `entry_revision_bumps_on_every_mutator`: each of
  `append_chunk`/`append_thought`/`update_tool`/`complete_turn` bumps the
  touched entry's `revision` and nothing else's.
- `render_cache_reuses_unchanged_entries`: render twice via `TestBackend`;
  assert the cache holds the same `EntryRender`s for untouched `seq`s and a
  fresh one for the appended entry.
- `scroll_max_counts_wrapped_rows`: narrow `TestBackend`, one long line;
  auto-follow renders the final wrapped row on the last pane line (fails
  against today's logical-line math).
- `entry_byte_cap_keeps_head_and_tail_with_marker`: stream >
  `EXCHANGE_ENTRY_BYTE_CAP` into one entry; text stays ≤ cap + marker,
  starts with the original head, ends with the newest tail, marker names
  the truncated KB. A replayed oversized transcript shows the same marker.
- `subscribe_surfaces_lag_as_events_dropped` (makina-core): overflow a
  small broadcast channel; the mapped stream yields
  `Event::EventsDropped { count ≥ 1 }` instead of skipping silently.
- `suspect_exchange_reloads_from_disk`: mark a key suspect, deliver
  `ExchangesLoaded` with the disk log; the in-memory log is replaced and the
  suspect mark cleared; a non-suspect cached key is not reloaded.
- `select_up_does_no_io_in_update`: after `SelectUp`, the load request sits
  in `pending_exchange_loads` (update performed no read); draining it is the
  loop's job.
- `page_and_home_end_keys_translate_to_scroll`: `PageUp`/`PageDown`/`Home`/
  `End` map to the new scroll events in the normal keymap.
- `exchange_title_counts_levels_honestly`: one Warn + no Error renders
  `(1 warning)`, never `(1 errors)`.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay
green.

## Interaction with prior plans

- **0006/0009/0010 are the substrate.** 0069/0070 change *when and how much*
  of their pipeline runs per frame, never the entry semantics: segmentation
  (0009) and the replay-equals-live reducer (0010) are untouched. 0009
  explicitly deferred "streaming Markdown re-layout optimisation … revisit
  only if profiling shows it matters" — this plan is that revisit. 0010's
  locked decision promised "the UI indicates the log was truncated";
  0070 delivers the marker `replay.rs:15` already documents.
- **0009 removed mouse capture; the later `tui-mouse-scroll` task added the
  consumer without re-enabling it.** 0071 completes that feature the way
  0009's out-of-scope note anticipated (capture returns as a deliberate
  feature, selection preserved via the terminal's Shift-modifier).
- **0014 (error pane) and 0015 (idle/live activity)** share surfaces: the
  pane title string (0014's badge) and the tick-driven spinner (0015's
  indicators must count as "active animation" in
  `App::has_active_animation()` when they land). Coordinate strings; no
  structural conflict.
- **Closing note:** the render cache and wrap math would benefit from
  snapshot tests over `TestBackend` frames. Adopting snapshot testing is
  deliberately *not* a workstream here — if the team wants it, it should be
  introduced TUI-wide in its own plan, not smuggled in via one pane.
