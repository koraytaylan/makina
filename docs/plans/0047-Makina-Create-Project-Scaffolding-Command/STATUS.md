# Plan 0047 — Project Scaffolding Command — makina create <path> [--template <name>] — status

Task-level status lives here; the roll-up row in [../STATUS.md](../STATUS.md) must stay in sync.

**Status:** ✅ Complete — 6/6 tasks landed on `implement-plan/0047`, squash-merged into develop as `85a5568`.

_Last updated: 2026-07-07, against develop._

- **Goal:** A headless `makina create <path> [--template todo]` subcommand scaffolds a brand-new, immediately-runnable experiment project outside the makina repo, giving outside developers a zero-risk first run.
- **Root cause:** Makina can bootstrap a folder in-TUI (`folder_init::initialize_folder`) but has no headless command to create a complete, runnable project OUTSIDE the repo, and no committed sample project to stamp — so newcomers experiment inside or beside the makina working tree (the `934d1b3` clobber class), and the README's only 'experiment safely' guidance is a throwaway clone.
- **Approach:** Five workstreams: commit an embeddable `todo` starter template under neutral file names; add `scaffold.rs::scaffold_project` that refuses non-empty targets, reuses `initialize_folder`, and commits the template on `develop`; extend `cli.rs` with a `Create`/`CreateError` action and dispatch it headlessly in `main.rs`; add a hermetic end-to-end scaffold test using `test_support` git helpers (plus an `#[ignore]`d full-compile check); and refresh the README with a 'Try it safely' Quickstart recommending `makina create` as the zero-risk first-run path — all gates green.

| WS | Workstream | Tasks | State |
|---|---|---|---|
| 0001 | Embedded `todo` Template | `add-todo-template-files` ✅ | ✅ Done |
| 0002 | Folder Bootstrap & Conflict Rules | `add-scaffold-module` ✅ | ✅ Done |
| 0003 | Create Subcommand CLI Surface | `add-create-cli-action` ✅, `wire-create-dispatch-in-main` ✅ | ✅ Done |
| 0004 | Parser & End-to-End Scaffold Tests | `add-scaffold-integration-test` ✅ | ✅ Done |
| 0005 | README Quickstart 'Try It Safely' | `readme-quickstart-try-it-safely` ✅ | ✅ Done |
