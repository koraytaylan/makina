# Scope — Plan 0023

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Makina's TUI has accreted a handful of modal actions, each reached by its own
single-key chord in Normal mode: `o` opens the file browser
(`AppEvent::OpenBrowser`), `g` opens the provider/role editor
(`AppEvent::OpenProviderEditor`, plan 0011), `?` opens the doctor overlay
(`AppEvent::OpenDoctor`, plan 0013). The status bar
(`ui.rs`, the `status_bar` `Paragraph`) is already a dense single line —
`[o] open  [s/p/c] start/pause/cancel  [Tab] panel  [v] view  [L] log  [?] doctor` —
and every new capability has to either steal another scarce letter or hide with
no hint at all. There is **no single discoverable entry point** to "what can I
do here?", and there is **no way at all to edit the run caps** (gate / reviewer /
wall-clock / idle / concurrency) that govern every task: they live only in
`config.toml` and can be changed today only by hand-editing the file and
restarting.

Two concrete gaps:

1. **No command surface.** Actions are scattered across one-key chords with no
   menu; a user cannot list or fuzzy-search the available commands, and we are
   running out of letters as the action set grows.
2. **Caps are not editable in-app.** The termination caps
   (`CapsConfig::gate_iterations`, `reviewer_iterations`, `wall_clock_secs`,
   `idle_secs`) and `concurrency` are read from `config.toml` at startup and never
   surfaced for editing; tuning them means leaving the app.

This plan adds a **`Ctrl+P` command palette** — one modal that lists every action
with a type-to-filter box — and a **settings screen** reachable from it that reads
the loaded caps/concurrency and writes edits back to `config.toml`
merge-preservingly via the same writer plan 0011 uses for the provider editor.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0069–0070):

- **0069 — Command palette.** Add `Mode::CommandPalette` and a
  `CommandPaletteState` (a `filter` string plus a static list of `PaletteAction`
  items). Bind `Ctrl+P` (`KeyModifiers::CONTROL` + `Char('p')`) in `event.rs`
  `translate_key` to open it; type-to-filter narrows the list; `Up`/`Down` move
  the selection; `Enter` executes the focused action (dispatching the existing
  intent `AppEvent` for it — `OpenBrowser`, `OpenDoctor`, `OpenProviderEditor`,
  the new `OpenSettings`, `Quit`, and forward-referenced stubs for retry / project
  discovery); `Esc` closes. Render a centered modal mirroring
  `render_provider_editor`; advertise `[^P]` in the status bar.
- **0070 — Settings screen.** Add `Mode::Settings` and a `SettingsState`, opened
  by the palette's **Settings** action. A modal lists the editable config values
  read from the App's loaded caps — `gate_iterations`, `reviewer_iterations`,
  `wall_clock_secs`, `idle_secs`, `concurrency` — lets the user navigate fields
  and edit values, validates each value with the same rules as `Config::validate`,
  and commits by writing `config.toml` merge-preservingly via the plan-0011 writer
  pattern (`commit_provider_config`'s read-merge-write recipe). `Esc` cancels
  without writing. (`commit_provider_config` round-trips the project `config.toml`
  through `GlobalConfig`, so only the `GlobalConfig` fields it edits —
  caps/concurrency — are written and the other `GlobalConfig` fields, providers
  and roles, are preserved; it does **not** carry `ProjectConfig` `[[gates]]` /
  `base_branch`, which the gates-aware project-config writer in plan 0025 owns.)

## Origin → workstream mapping

| Finding | Addressed by |
|---|---|
| Actions scattered across scarce one-key chords; no menu / fuzzy search | `0069` |
| Running out of status-bar letters as capabilities grow | `0069` |
| Run caps & concurrency editable only by hand-editing `config.toml` | `0070` |
| No in-app surface to (re-)trigger LLM-driven project discovery | `0069` (the `Discover project` palette action) |

## Locked decisions

- **One palette, every action.** `Ctrl+P` is the discoverable front door; new
  capabilities register a `PaletteAction` rather than fighting for a letter. The
  existing one-key chords (`o`, `g`, `?`, `s/p/c`) **stay** — the palette is
  additive, not a replacement.
- **Palette actions are thin dispatchers.** Each `PaletteAction` carries the
  `AppEvent` it emits; `Enter` closes the palette and re-feeds that `AppEvent`
  through the normal `App::update`/`resolve_io` path. The palette adds **no new
  execution path** — it just routes to the intents that already exist (`Quit`,
  `OpenBrowser`, `OpenDoctor`, `OpenProviderEditor`) plus the new `OpenSettings`.
- **Forward-referenced actions are explicit stubs.** `Retry failed task`
  (plan 0017's `FailureKind` retry surface) and `Discover project` (plan 0025)
  appear in the list but dispatch a `StatusMessage("… not yet available")` until
  their owning plans land; they are not silently omitted, so the palette is the
  stable home those plans wire into.
- **Settings reads the *resolved* caps, writes via the `GlobalConfig` writer.** The
  screen shows the effective `CapsConfig`/`concurrency` the App was built with, and
  on commit writes them into `{repo_root}/.makina/config.toml` using the same
  read-existing → merge → write-back recipe as `commit_provider_config`. That writer
  round-trips the file through `GlobalConfig`, so the other `GlobalConfig` fields
  (providers, roles) are preserved untouched — but it does **not** model the
  `ProjectConfig` `[[gates]]` / `base_branch` sections, which the gates-aware
  project-config writer in plan 0025 owns. This plan edits only `GlobalConfig`
  fields (caps + concurrency) and does not touch gates.
- **Validation mirrors `Config::validate`.** A field edit that would fail
  validation (`gate_iterations`/`reviewer_iterations`/`wall_clock_secs`/
  `concurrency` `< 1`, or `idle_secs == Some(0)`) is **rejected at the field** with
  the same precise reason string `Config::validate` produces; commit never writes
  an invalid config.
- **No new config schema.** The settings screen edits only fields that already
  exist in `CapsConfig`/`GlobalConfig`; it introduces no new on-disk key. The
  `[discovery]` table and the discovery-stamp record are owned entirely by plan
  0025 (a `ProjectConfig` stamp, not a `GlobalConfig` flag), and this plan neither
  defines nor reads it.
- **TUI-only.** This plan touches only the `makina` (TUI) crate; no `makina-core`
  config-schema, orchestration, scheduler, or backend change.

## Out of scope

- The LLM-driven discovery agent itself, gate merging, the `[discovery]` stamp,
  and the planner's TASKS.md auto-generation (sibling discovery plan 0024/0025;
  this plan only adds the `Discover project` palette action stub that 0025 wires
  to its discovery trigger).
- Retrying / re-dispatching failed tasks (plan 0017 owns the retry event; the
  palette only reserves the action slot).
- Editing providers/roles from the settings screen — that stays in the dedicated
  provider editor (plan 0011); settings owns caps/concurrency only.
- Per-role `system_prompt` editing and the `#1` duration/model metrics surfacing
  (separate sibling plans); not part of the palette or settings screen here.
- Configurable / rebindable keys, command history, or recently-used ordering in
  the palette.
- Live hot-reload of caps into a running supervisor — a committed caps change
  applies to runs opened after the write, exactly as today.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
