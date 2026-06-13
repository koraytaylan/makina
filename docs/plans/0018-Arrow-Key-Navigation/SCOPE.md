# Scope — Plan 0018

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Plan 0016 turns the sidebar into a **"Runs & Tasks" tree**: runs are expandable
parents, tasks nest beneath them, and a single tree cursor (`App::tree_cursor`)
drives the derived `selected_run`/`selected_task`. But 0016 only wires `Up`/`Down`
(walk the visible nodes) and `Space` (toggle expand/collapse). The arrow keys that
a tree *wants* — `Left`/`Right` for collapse/expand and for stepping between the
sidebar and the content it controls — are still **unhandled**: in
`crates/makina/src/event.rs` `translate_key`, the normal keymap binds `Tab`,
`Up`/`k`, and `Down`/`j`, and everything else (including `KeyCode::Left` and
`KeyCode::Right`) falls through to `AppEvent::Tick`.

The result is a tree you can walk but not *traverse* the way the keyboard implies:

1. **No directional focus.** The only way to move focus between the sidebar and
   the main (content) pane is `Tab`. There is no spatial `Left`/`Right` motion that
   matches the left-sidebar / right-content layout, and no way to expand a run with
   `Right` the way every tree widget does.
2. **`Up`/`Down` only ever navigate.** When the main pane is focused, `Up`/`Down`
   move `selected_task` (0016: walk nodes) — they never **scroll the content** the
   user is reading, even though the exchange pane already has
   `scroll_up()`/`scroll_down()` plumbing it inherits from the mouse-wheel path.

This plan adds **arrow-key directional navigation** layered on 0016's tree:
`Right` expands a collapsed run, then (on a run with nothing left to expand)
crosses into the content pane; `Left` collapses or steps back to the sidebar;
`Up`/`Down` **navigate** the tree when the sidebar is focused but **scroll** the
content when the main pane is focused. `Tab` stays exactly as it is.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0059–0060):

- **0059 — Directional focus traversal.** Bind `KeyCode::Left`/`KeyCode::Right`
  in `event.rs` to two new `AppEvent` variants and handle them on `App` using
  0016's tree: `Right` expands a collapsed run, else crosses focus into
  `Panel::Main`; `Left` returns focus to `Panel::Sidebar` from the main pane, else
  collapses an expanded run. `Tab` (`AppEvent::FocusNext`) is unchanged.
- **0060 — Up/Down: navigate vs scroll.** Make the existing
  `AppEvent::SelectUp`/`SelectDown` handlers route to 0016's `tree_move(±1)` when
  the sidebar is focused, and to `scroll_up()`/`scroll_down(last_scroll_max)` so
  the **content pane scrolls** when the main pane is focused. `k`/`j` stay as
  aliases.

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| `KeyCode::Left`/`Right` unhandled (fall through to `Tick`); no directional focus or tree expand/collapse via arrows | `0059` |
| `Up`/`Down` always navigate; the content pane can't be scrolled from the keyboard when focused | `0060` |

## Locked decisions

- **`Tab` stays the panel-focus toggle.** `AppEvent::FocusNext` and its
  `Sidebar`↔`Main` toggle are untouched. Arrow keys are *additive* directional
  navigation, not a replacement for `Tab`.
- **`Right` is a two-step gesture on a collapsed run.** On a collapsed
  `TreeNode::Run` (its `RunId` in `collapsed_runs`), the first `Right` *expands*
  it (`tree_toggle_expand`); a second `Right` — now that the run is expanded, or on
  any already-expanded run or a `TreeNode::Task` — moves focus into `Panel::Main`.
  This matches file-tree conventions: `Right` drills in, then crosses to content.
- **`Left` mirrors `Right`.** From `Panel::Main`, `Left` returns focus to
  `Panel::Sidebar` (it does not collapse). From `Panel::Sidebar`, `Left` collapses
  an expanded run (`tree_toggle_expand`); on an already-collapsed run or a leaf
  task it is a no-op.
- **`Up`/`Down` are context-sensitive.** Sidebar focus ⇒ `tree_move` (0016).
  Main focus ⇒ `scroll_up()`/`scroll_down(self.last_scroll_max.get())` (the same
  helpers the mouse wheel uses), so the user scrolls the exchange they came to read
  instead of moving `selected_task`.
- **No new tree state.** All expand/collapse and selection state already exists on
  `App` from plan 0016 (`collapsed_runs`, `tree_cursor`, `focused_node`,
  `tree_move`, `tree_toggle_expand`); this plan only adds key bindings, two
  `AppEvent` variants, and the routing logic. No `makina-core` change.

## Out of scope

- Any change to plan 0016's tree model, render, or `Space`/`Tab` behaviour (this
  plan depends on 0016 being merged).
- Retry / re-dispatch keys (plan 0017).
- Horizontal page scrolling of the exchange pane (`Left`/`Right` never scroll
  content; they only move focus / expand-collapse).
- Mouse-driven focus, click-to-select, or resizing the 30/70 split.
- Configurable / remappable key bindings.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
