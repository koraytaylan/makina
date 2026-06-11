# Scope — Plan 0022

> What this plan delivers, what it leaves out, and the decisions behind it.
> Findings from the full-codebase review of 2026-06-11.

## Why this plan

The Exchange pane is the feature line plans 0003 → 0006 → 0009 → 0010 built:
a live, rich, replayable transcript of every agent turn. Functionally it is
done. Under load it silently loses data, does quadratic re-render work, grows
memory without bound inside a single turn, and ships input affordances that
cannot fire. None of these is a missing feature — each is a performance or
robustness defect in code that already exists:

1. **Silent data loss under load — and it's permanent.** `CoreApi::subscribe`
   maps broadcast `Lagged` errors away (`orchestrator.rs:1269–1273`, channel
   capacity 1024 at `orchestrator.rs:212`): a slow consumer silently skips
   events. The TUI is that slow consumer *by construction* — the event loop
   does one event → one full-frame redraw (`event.rs:160–169`), and every
   redraw rebuilds every transcript line (`render_exchange_pane`,
   `ui.rs:839–841`) and re-parses each entry's full accumulated Markdown
   (`render_markdown` per entry per frame, `ui.rs:1097`). A per-token
   streaming agent therefore costs O(n²) cumulative parse work, the loop
   falls behind, the channel laps it, and chunks vanish. Worse, the loss is
   unrepairable: `load_exchanges_for_selected_run` skips any `(run, task)`
   key already present (`app.rs:1196–1199`), and live events create exactly
   that key (`app.rs:1338`) — so the authoritative on-disk transcript
   (plan 0010) never backfills a lagged live log.

2. **Unbounded entry growth.** `EXCHANGE_LOG_CAP` (`app.rs:28`) bounds the
   *number* of entries, but `append_chunk` / `append_thought` grow a single
   entry's `String` without limit (`app.rs:193–212`, `app.rs:238–251`). One
   long turn — a model that streams megabytes — is unbounded memory. The
   doc claim that the cap means the buffer "cannot cause unbounded memory
   growth" (`app.rs:25–28`) is false on this axis.

3. **Blocking IO inside the "pure" update.** `SelectUp` / `SelectDown` call
   `load_exchanges_for_selected_run` from within `App::update` (`app.rs:922`,
   `app.rs:947`), which does a synchronous `std::fs::read_to_string` of every
   task transcript (`app.rs:1171–1216`, `replay.rs:27`) on the event-loop
   task. This contradicts both module contracts: "It holds no IO" (`app.rs:3`)
   and `resolve_io` being "the single place where the TUI touches the
   filesystem" (`event.rs:211–212`).

4. **Scroll math is wrong whenever a line wraps.** `scroll_max` counts
   *logical* lines (`ui.rs:847–849`) but the `Paragraph` wraps
   (`ui.rs:856–858`), so with any wrapped line "follow bottom" pins above the
   real bottom. `render_markdown`'s width parameter is ignored (`markup.rs:38`
   — `_width`), and the `as u16` cast at `ui.rs:849` truncates on huge logs.

5. **Dead input paths.** Mouse wheel events are translated
   (`event.rs:447–451`) but `EnableMouseCapture` is never executed
   (`Tui::init`, `tui.rs:53–61`), so crossterm never delivers a mouse event;
   there is no keyboard scroll binding either (arrows move *selection*,
   `event.rs:519–520`). The entire scroll subsystem — `scroll_up` /
   `scroll_down` / `last_scroll_max` (`app.rs:838–871`) — is unreachable in
   production.

6. **Noise and a hot spin.** No `EnvFilter` is installed (`main.rs:86–89`),
   so every dependency `debug!`/`trace!` event reaches both tracing layers,
   and `TuiLogLayer` forwards every level (`log.rs:382–396`). The event loop
   turns each record into an `ErrorMessage` (`event.rs:139–143`) and the pane
   title counts them all as errors (`ui.rs:732–734`) — the startup
   `tracing::info!` at `main.rs:175` alone makes a fresh launch read
   "Exchange (1 errors)". Separately: if the crossterm `EventStream` dies,
   the forwarder task exits (`event.rs:107–112`), `term_rx.recv()` returns
   `None` forever, and the `None` arm maps to `None` (`event.rs:127–129`) —
   the loop spins at 100 % CPU. Compare the api-stream arm, which correctly
   quits on `None` (`event.rs:134–135`).

7. **Tick-driven full redraws even when idle.** `AppEvent::Tick`
   unconditionally returns `true` (`app.rs:987–990`), so the whole frame is
   rebuilt every 250 ms (`event.rs:71`) with nothing on screen changing.

This plan is the performance/robustness pass over the exchange feature line:
make the consumer fast enough that lag does not happen, make the data bounded
and the scroll honest, and make loss visible and repairable when it does
happen.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0069–0071):

- **0069 — Frame-budget render loop.** Batch-drain every ready event per loop
  iteration (opportunistic `try_recv` after the first await), mark state
  dirty, and render at most once per ~30 ms frame budget. `Tick` redraws only
  when something is dirty — spinner state counts as dirty while any task is
  active, so the working indicator (plans 0009/0015) keeps animating. Fix the
  term-stream `None` arm to quit instead of spinning.
- **0070 — Render cache + true scroll.** Cache rendered lines per
  `ExchangeEntry`, keyed by (entry revision, pane width); invalidate only the
  entry that changed. Compute wrapped-row counts at the pane width and do all
  scroll math in wrapped-row space, so "bottom" is the real bottom. Byte-cap
  a streaming entry (keep head + tail with a `… truncated …` marker —
  `replay.rs:15` already *promises* a truncation marker that does not exist;
  this delivers it for live and replayed logs alike).
- **0071 — Replay backfill, input, and log hygiene.** Surface broadcast lag
  as data (`Event::EventsDropped`) instead of hiding it, mark the affected
  exchange logs suspect, and let the authoritative disk transcript backfill
  them on turn end or selection. Move transcript loading out of
  `App::update` into the IO layer. Enable mouse capture (with teardown on
  every restore path, including the panic hook) and add
  PageUp/PageDown/Home/End as capture-free keyboard scrolling. Install an
  `EnvFilter` (default `info`, `RUST_LOG` override), filter the TUI error
  channel to WARN+, and make the pane title honest ("N errors, M warnings").

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Broadcast `Lagged` mapped away; the TUI is the slow consumer; disk replay never repairs a lagged live log | `0069` (root cause), `0071` (repair) |
| O(n²) per-frame re-render: every entry's full Markdown re-parsed every frame | `0070` |
| `EXCHANGE_LOG_CAP` bounds entry count but a single entry's `String` grows without limit | `0070` |
| `SelectUp`/`SelectDown` perform synchronous filesystem reads inside the pure `update` | `0071` |
| `scroll_max` counts logical lines while the `Paragraph` wraps; `render_markdown` ignores its width; `as u16` truncation | `0070` |
| Mouse scroll translated but capture never enabled; no keyboard scroll; scroll subsystem unreachable | `0071` |
| No `EnvFilter`; every record becomes an "error"; first launch shows "Exchange (1 errors)" | `0071` |
| Dead `EventStream` → `term_rx` yields `None` → loop spins at 100 % CPU | `0069` |
| Tick redraws the full frame every 250 ms even when idle | `0069` |

## Locked decisions

- **Fix the consumer, don't slow the producer.** The broadcast channel, its
  capacity, and the engine's emission rate are untouched. The render loop
  stops doing per-event full-frame work (0069 + 0070) so the TUI keeps up;
  lag becomes a rare event instead of a steady state.
- **Lag becomes data, not silence.** `CoreApi::subscribe` stops filtering
  `Lagged` away and instead yields a new `Event::EventsDropped { count }`
  (the stream stays infallible — the trait contract is unchanged). The TUI
  uses it to mark suspect logs and trigger disk backfill; everything else
  may ignore it.
- **Disk stays authoritative (plan 0010's contract).** Backfill replaces a
  suspect in-memory log by re-reading the per-task transcript through the
  same replay reducer — no merge heuristics, no second rendering path.
- **Exact cache invalidation, no content hashing.** Each `ExchangeEntry`
  carries a `revision` counter bumped on every mutation; the render cache
  keys on (revision, width). A streaming turn invalidates exactly one entry
  per chunk.
- **Wrapped-row counts come from ratatui.** `Paragraph::line_count(width)`
  exists in ratatui 0.30 behind the `unstable-rendered-line-info` feature
  (verified: `ratatui-widgets-0.3.0/src/paragraph.rs:331`); enable the
  feature rather than hand-rolling span wrapping. It is a measurement-only
  API; if its semantics shift in a future ratatui, only `scroll_max`
  arithmetic is affected.
- **Cap keeps head + tail.** The per-entry byte cap keeps the head (the
  opening context) and the tail (the most recent output) around a
  `… N KB truncated …` marker, on UTF-8 boundaries — matching the precedent
  of gate-output tail truncation (`gate.rs:255`) and finally honouring the
  marker `replay.rs:15` promises.
- **Mouse capture returns deliberately, with an escape hatch.** Plan 0009
  removed capture because nothing consumed mouse events; the later
  `tui-mouse-scroll` task added the consumer but never re-enabled capture.
  0071 closes that loop *and* adds keyboard paging, so scrolling never
  depends on capture; native text selection remains available via the
  terminal's standard Shift-modifier.

## Out of scope

- Failure-reason UX and error-pane discoverability — **plan 0014** (the
  title-grammar fix here only corrects the level counting; 0014 owns the
  badge/hint design).
- Idle/hang detection and live-activity indicators — **plan 0015** (0069's
  dirty-tracking keeps its spinner contract working, nothing more).
- Snapshot-testing adoption for the render path. Worth doing — the cache and
  wrap changes would be safer under snapshot coverage — but it is a testing
  strategy decision for the whole TUI, not a workstream here (see the
  closing note in [ARCHITECTURE.md](ARCHITECTURE.md)).
- Any change to transcript file formats or the replay reducer's semantics
  (plan 0010); 0071 only adds the truncation marker that was already
  specified.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
