# Scope — Plan 0009

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

The Exchange pane is the primary place a user watches an agent work. Plan 0003
built the prompt-answer-stream; plan 0006 enriched it with thoughts and tool
calls. The plumbing is now rich, but five **fidelity** defects make the pane
misleading or hard to read. None is a missing feature — each is a correctness or
presentation gap in code that already exists:

1. **Chronology is wrong.** Thoughts and tool calls that happen *while* a
   response streams are rendered *after* the whole response. The cause is real
   and local: `ExchangeLog::append_chunk` (`crates/makina/src/app.rs`) scans
   *backward past* `Thought`/`Tool` entries to coalesce every response chunk
   into one `Response` entry anchored at the **first** chunk. A
   `preamble → think → call tool → more response` turn therefore renders the
   entire response first and the interleaved reasoning/tools afterward — the
   exact opposite of what happened.

2. **Markdown and ANSI render poorly.** Agent responses routinely contain
   Markdown (headings, lists, fences, tables, emphasis) and sometimes raw ANSI.
   The pane renders Markdown as literal text, and `crates/makina/src/ansi.rs`
   understands only four SGR codes (reset, bold, red, green), dropping 256-/RGB
   colour, underline, italic, and everything else.

3. **Tool paths are unreadably long.** Tool-call titles are passed through
   verbatim from the agent, so an absolute path inside a worktree
   (`…/.makina/worktrees/{slug}--{id}/src/main.rs`) is shown in full.

4. **Text selection is broken.** `crates/makina/src/tui.rs` enables
   `EnableMouseCapture`, which makes the terminal hand mouse events to the app
   instead of performing native selection — yet nothing in the TUI consumes
   mouse events, so the only effect is that users cannot select/copy text.

5. **No "working" indicator.** There is no spinner while a turn is in flight,
   even though the event loop already ticks every 250 ms (`event.rs`) and the
   task model already exposes `InProgress`/`InReview` states.

This plan makes the pane a faithful, readable, chronological transcript.

## In scope

Exactly the work items in [TASKS.md](TASKS.md) (workstreams 0029–0033):

- **0029 — Chronological response segmentation.** Rewrite `append_chunk` so a
  response chunk coalesces *only* with an immediately-preceding open `Response`
  of the same role; once a `Thought`/`Tool` intervenes, the next chunk starts a
  new response **segment**. The pane then renders prompt → response-seg →
  thought → tool → response-seg in true arrival order.
- **0030 — Full Markdown + full ANSI rendering.** Render response and thought
  bodies as CommonMark + GFM (headings, emphasis, lists, blockquotes, fenced
  code, **tables**) mapped to ratatui `Line`/`Span`, with full ANSI (16/256/RGB,
  bold/italic/underline) applied inside text and code. The existing diff overlay
  for tool/edit content is preserved.
- **0031 — Repo-root-relative tool paths.** Thread the repo root into the TUI
  and rewrite any absolute path under the repo/worktree root to a compact
  repo-root-relative form in tool titles and content.
- **0032 — Restore text selection.** Remove mouse capture so the terminal's
  native selection/copy works again.
- **0033 — Working spinner.** Drive a frame counter off the existing tick and
  render a braille spinner next to `InProgress`/`InReview` tasks and in the
  Exchange header while the selected task's response is still streaming.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Thoughts/tools render after the response instead of interleaved | `0029` |
| Markdown / coloured (ANSI) output not displayed properly | `0030` |
| Absolute worktree paths in tool uses are too long | `0031` |
| Cannot select text in the TUI | `0032` |
| No spinner while the model is working | `0033` |

## Locked decisions

- **Segments, not reordering.** We fix chronology by *not merging across*
  thought/tool boundaries, producing multiple short `Response` segments. We do
  **not** add timestamps and sort, and we do **not** reorder entries after
  arrival. Arrival order is already correct; the merge was destroying it.
- **The actor's answer string is unaffected.** The Developer/Reviewer `output`
  string used for commit messages is accumulated in the actor loop
  (`developer.rs`/`reviewer.rs`), not from `ExchangeLog`. Segmenting the TUI log
  changes display only; the answer contract from plan 0006 is untouched.
- **Crate-backed rendering.** Markdown via a maintained CommonMark+GFM parser
  (`pulldown-cmark`, with `comrak` if GFM tables need more) and ANSI via
  `ansi-to-tui`, rather than extending the hand-rolled four-code parser. The
  pane keeps the diff-aware styling for tool content.
- **Path compaction is prefix-relative and bounded.** We rewrite only absolute
  paths that start with the known repo root or the `.makina/worktrees/…` prefix;
  anything else is left verbatim. No fragile guessing.
- **Remove mouse capture outright.** Nothing consumes mouse events today, so the
  capture is pure cost. If scroll-wheel support is wanted later it returns as its
  own deliberate feature (with selection preserved via a modifier).
- **Spinner reuses the existing tick.** No new timer; a frame index advances on
  `AppEvent::Tick` and is read at render time. Purely additive UI state.

## Out of scope — deferred to later plans

- Persisting/replaying the exchange for finished runs — **plan 0010**.
- Provider/model/effort configuration — **plan 0011**.
- Task-list column cleanup and dependency-view discoverability — **plan 0012**.
- Collapsible/foldable thoughts, filtering the pane, or a "thoughts only" view.
- Mouse-driven scrolling or click targets.
- Syntax highlighting of code blocks (beyond Markdown fence styling + ANSI).
- Streaming Markdown re-layout optimisation (we re-render the entry's text per
  frame; revisit only if profiling shows it matters).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete deltas and data flow.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
