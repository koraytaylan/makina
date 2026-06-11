# Scope — Plan 0017

> What this plan delivers, what it leaves out, and the decisions behind it.

## Why this plan

Findings from the full-codebase review (2026-06-11). The TUI's provider/role
editor (key `g`, introduced by plan 0011) has a **destructive write path** and
is **never seeded** in the shipping binary.

1. **Pressing `g` then `Enter` deletes the project's gates.**
   `commit_provider_config` (`event.rs:294–341`) writes to
   `paths::config_file(&app.repo_root)` = `{repo}/.makina/config.toml`
   (`event.rs:303`; `paths.rs:20–22`) — the committed **project** layer — but
   parses it as `GlobalConfig` (`event.rs:306`) and serializes back
   `toml::to_string_pretty(GlobalConfig)` (`event.rs:322`). `GlobalConfig`
   (`config.rs:281–312`) has no `gates`/`base_branch`/`caps`-override fields,
   and the project-side types are **Deserialize-only** (`GateConfig`
   `config.rs:363`, `CapsOverride` `config.rs:386`, `ProjectConfig`
   `config.rs:406`) — they *cannot* be written back. So the rewrite drops
   `base_branch` and every `[[gates]]` entry — **silently disabling all
   quality gates** for subsequent runs (this very repo's `.makina/config.toml`
   carries `base_branch`, `concurrency`, `[caps]`, and three `[[gates]]`).
   The comment at `event.rs:300–302` — "so we don't lose fields we don't
   manage (e.g. gates, caps, base_branch)" — promises exactly what the type
   system guarantees to lose. Worse, a *parse failure* hits
   `.unwrap_or_default()` (`event.rs:306`), replacing a malformed file
   wholesale with defaults. And per the module docs (`config.rs:6–11`),
   providers/roles are **global-layer** (`~/.makina/config.toml`) settings —
   the editor writes them to the wrong layer entirely.

2. **The editor is never seeded in the real binary.** `main.rs:216` calls
   `App::new(...)`, which leaves `app.providers` empty (`app.rs:734`).
   `App::with_config` (`app.rs:756`) — whose doc says "Use this variant when a
   config is available (main.rs)" (`app.rs:752–755`) — is called from exactly
   one place: a test (`app.rs:2374`, in `provider_editor_opens_and_lists_providers`,
   `app.rs:2344–2380`) whose comment admits it "simulates what main.rs does"
   (`app.rs:2348–2349`). In production, `g` opens an **empty** editor and
   `Enter` commits that emptiness over the project file.

3. **Both bugs are masked.** The only commit-path test
   (`provider_editor_commit_writes_config`, `event.rs:1335–1423`) starts from
   a tempdir with **no** pre-existing config file (`event.rs:1342–1343`), so
   the round-trip destruction of an existing file is never exercised — and it
   *asserts* the project path is written (`event.rs:1391`), enshrining the
   wrong layer. Adjacent: the editor keymap is Up/Down/Enter/Esc only
   (`event.rs:486–495`) — a read-only list whose sole action key is the
   destructive commit — and `selection_index` ranges over
   `providers.len() + 3` (`app.rs:1104`), which does not correspond to the
   rendered rows (unconditional roles header `ui.rs:1310`, *conditional* role
   rows `ui.rs:1328–1340`, conditional discovered rows `ui.rs:1342–1396`,
   clamp at `ui.rs:1408`).

This plan redirects the commit to the correct (global) layer with a lossless
read-modify-write, seeds the editor from the loaded config in `main.rs`, and
refuses empty/unseeded commits.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0055–0056):

- **0055 — Write the right layer, losslessly.** Commit providers/roles to the
  **global** `~/.makina/config.toml` (extracting and reusing the path
  derivation `Config::load_defaults` already uses — `config.rs:760,798–800`),
  via a `toml::Table` read-modify-write that preserves every unmanaged field;
  refuse (don't clobber) on unparseable input; never touch the project file.
- **0056 — Seed the editor in the binary + empty-state guard.** Wire
  `App::with_config` into `main.rs` so `g` shows the loaded providers/roles,
  and make the commit path refuse when the editor is empty/unseeded.

## Origin → workstream mapping

| Finding (full-codebase review, 2026-06-11) | Addressed by |
|---|---|
| Editor commit rewrites the project config as `GlobalConfig`, deleting `base_branch` and all `[[gates]]` | `0055` |
| Providers/roles written to the project layer though they belong in the global layer (`config.rs:6–11`) | `0055` |
| Parse failure → `unwrap_or_default()` → file replaced with defaults | `0055` |
| Commit-path test starts from a non-existent file; destruction never exercised | `0055` |
| `App::with_config` called only from a test; production editor is always empty | `0056` |
| Empty editor + Enter commits emptiness over a real config file | `0056` |

## Locked decisions

- **The editor commits to the global layer only.** Extract
  `global_config_file()` in `config.rs` next to `home_dir()`
  (`config.rs:798–800`), reusing the exact derivation inlined in
  `load_defaults` (`config.rs:760`); `commit_provider_config` targets it. The
  project file (`paths::config_file`) is **never** written by the editor.
- **Lossless via `toml::Table`, no new dependency.** The repo already ships
  `toml` 1.x (`Cargo.toml:27`). Read the global file into a `toml::Table`,
  replace only the `providers` and `roles` keys (both types are `Serialize` —
  `config.rs:110,157`), and write the table back. This preserves all unknown
  and unmanaged keys/values; TOML **comments are not preserved** — accepted,
  because the global file is machine-local and uncommitted (the comment-rich
  *project* file is no longer touched at all). `toml_edit` (comment-preserving)
  was considered and rejected as a new dependency for a non-requirement.
- **Refuse, never clobber.** A global file that exists but fails to parse
  aborts the commit with a status-bar error (replacing today's
  `unwrap_or_default()` overwrite). A missing file is created fresh.
- **Injectable target path.** `App` gains
  `global_config_path: Option<PathBuf>`, defaulted from
  `global_config_file()`; tests inject a tempdir path (no `HOME` env mutation
  — env is process-global and races parallel tests). `None` (no `HOME`)
  refuses with a message; it never falls back to the project file.
- **Empty commits are refused.** If the editor holds no providers, `Enter`
  produces a status message, not a write — the unseeded-production-binary
  case can no longer zero out a config file.
- **The keymap/selection mismatch is documented, not fixed.** The
  Up/Down/Enter/Esc-only keymap (`event.rs:486–495`) and the
  `providers.len() + 3` row mismatch (`app.rs:1104` vs `ui.rs:1287–1408`)
  belong to a future make-the-editor-actually-edit plan; this plan only makes
  its one action key safe.

## Out of scope

- Making editor fields actually editable (text input for provider command,
  args, role model/effort) — future work.
- Provider discovery UX beyond the existing "Discovered (live agent)" rows.
- Plan-0013 doctor integration (startup config diagnostics stay the doctor's
  domain; this plan's refusals are status-bar messages only).
- The permission policy and CI/README work (plan 0016 and the future 0024).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
