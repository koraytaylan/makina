# Architecture — Plan 0017 (deltas)

> Edits in `crates/makina-core/src/config.rs`, `crates/makina/src/event.rs`,
> `crates/makina/src/app.rs`, and `crates/makina/src/main.rs`. Line numbers
> are hints; locate by symbol.

## 0055 — Commit providers/roles to the global layer, losslessly

Today `commit_provider_config` (`event.rs:294–341`) round-trips the **project**
file (`paths::config_file`, `event.rs:303`) through `GlobalConfig`
(`event.rs:306`), whose serialization (`event.rs:322`) cannot carry `gates` /
`base_branch` / project `caps` (Deserialize-only — `config.rs:363,386,406`).

Edits:

- **New `global_config_file()` in `config.rs`**, next to `home_dir()`
  (`config.rs:798–800`), extracted from the derivation inlined in
  `load_defaults` (`config.rs:760`):

  ```rust
  /// Path of the GLOBAL config layer (`~/.makina/config.toml`), or `None`
  /// when `HOME` is unavailable. Pure join on top of [`home_dir`]; the
  /// directory is not created and the file may not exist.
  pub fn global_config_file() -> Option<PathBuf> {
      home_dir().map(global_config_file_in)
  }

  /// Pure variant for tests: the global config path under a given home dir.
  pub fn global_config_file_in(home: impl Into<PathBuf>) -> PathBuf {
      home.into().join(".makina").join("config.toml")
  }
  ```

  `load_defaults` (`config.rs:759–764`) switches to calling it — one
  derivation, two callers.

- **`App` gains `global_config_path: Option<PathBuf>`** (`app.rs`, near
  `repo_root`), populated in `App::new` from
  `makina_core::config::global_config_file()`; tests overwrite it with a
  tempdir path (no `HOME` mutation).

- **Rewrite `commit_provider_config`** (`event.rs:294–341`) as a lossless
  read-modify-write against the global path:

  ```rust
  async fn commit_provider_config(app: &App) -> Option<String> {
      let editor = app.provider_editor.as_ref()?;
      // 0056 empty-state guard goes here.
      let Some(path) = app.global_config_path.as_deref() else {
          return Some("Config not saved: no global config path (HOME unset)".into());
      };
      let mut table: toml::Table = match tokio::fs::read_to_string(path).await {
          Ok(s) => match s.parse::<toml::Table>() {
              Ok(t) => t,
              // Refuse, never clobber (replaces today's unwrap_or_default()).
              Err(e) => return Some(format!("Config not saved: {} is not valid TOML: {e}", path.display())),
          },
          Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
          Err(e) => return Some(format!("Config read error: {e}")),
      };
      // Replace ONLY the two managed keys; every other key survives verbatim.
      table.insert("providers".into(), toml::Value::try_from(&editor.providers).ok()?);
      table.insert("roles".into(), toml::Value::try_from(&editor.roles).ok()?);
      // to_string_pretty(table) → create_dir_all(parent) → write, as today.
  }
  ```

  (`try_from` errors should surface as a `Config serialise error: …` status
  message rather than `ok()?` — sketch elided.) The `use
  makina_core::paths::config_file` import and the doc comment at
  `event.rs:289–290` ("back to `{repo_root}/.makina/config.toml`") go away;
  the project file is no longer referenced by this function.

- **Fix the existing test's wrong-layer assertion.**
  `provider_editor_commit_writes_config` (`event.rs:1335–1423`) currently
  asserts the *project* path is written (`event.rs:1391`); repoint it at the
  injected global path, and keep its provider/role round-trip assertions
  (`event.rs:1404–1422`) unchanged.

## 0056 — Seed the editor in the binary + empty-state guard

`App::with_config` (`app.rs:756`) has exactly one caller — the test at
`app.rs:2374`. Production (`main.rs:216`) uses `App::new`, so `app.providers`
is always empty (`app.rs:734`) and `g` opens a blank editor
(`AppEvent::OpenProviderEditor` copies `app.providers`/`app.roles` into the
editor — `app.rs:1071–1093`).

Edits:

- **Wire `with_config` into `main.rs`.** The resolved `Config` exposes
  `providers`/`roles` (`config.rs:457,460`) but is moved into
  `CoreApi::with_audit_registry` at `main.rs:204–212`; capture first:

  ```rust
  let (providers, roles) = (config.providers.clone(), config.roles.clone());
  let api: Arc<dyn Api> = Arc::new(CoreApi::with_audit_registry(/* …, config, … */));
  // …
  let mut app = app::App::with_config(Arc::clone(&api), initial_runs, repo_root, providers, roles);
  ```

  (replacing the `App::new` call at `main.rs:216`).

- **Empty-state guard** at the top of `commit_provider_config`: when
  `editor.providers.is_empty()`, return
  `Some("Config not saved: provider editor is empty (no providers loaded)")`
  without touching any file. With 0055 this closes the
  unseeded-binary-zeroes-the-config path end to end; resolution of roles
  against an empty provider set is meaningless anyway (`Config::resolve`
  synthesizes a default provider from `[backend]` when none are declared —
  `config.rs:284–291`).

## Test strategy

All in `crates/makina/src/event.rs` / `app.rs` test mods unless noted; each
injects `app.global_config_path = Some(tempdir-path)`.

- `editor_commit_preserves_unmanaged_global_fields`: seed the *global* file
  with `[backend]`, `[planner]`, `[caps]`, `concurrency`, **and** an unknown
  `[future]` table; commit; parse the result and assert every pre-existing
  key/value survives and only `providers`/`roles` changed.
- `editor_commit_never_touches_project_config`: seed
  `{repo_root}/.makina/config.toml` with this repo's actual shape —
  `base_branch`, `concurrency`, `[caps]`, three `[[gates]]`
  (`.makina/config.toml:26–63`); commit; assert the project file's **bytes**
  are identical and it still parses as `ProjectConfig` with three gates and
  `base_branch = "develop"`. This is the regression test for the destructive
  path.
- `editor_commit_refuses_invalid_global_toml`: a malformed global file yields
  an error status and an unchanged file (regression for
  `unwrap_or_default()`, `event.rs:306`).
- `editor_commit_refuses_without_home`: `global_config_path = None` →
  refusal message, nothing written.
- `commit_refuses_empty_editor`: `App::new` (mirrors today's production
  path), open the editor, commit → refusal status; neither the global nor the
  project file is created.
- `app_with_config_seeds_editor_from_resolved_config` (wiring level): build a
  `Config` via `GlobalConfig::from_toml_str` (with `[[providers]]`/`[roles]`)
  + `Config::resolve`, construct via `App::with_config` exactly as the new
  `main.rs` does, dispatch `OpenProviderEditor`, and assert the editor lists
  the configured providers/roles — complementing
  `provider_editor_opens_and_lists_providers` (`app.rs:2344`), which hands in
  literals.
- `global_config_path_is_home_dot_makina` (makina-core,
  `config.rs` tests): `global_config_file_in("/home/u")` ==
  `/home/u/.makina/config.toml`; `load_defaults` continues to compile against
  the shared helper.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- **0011 (Provider and Role Configuration)** introduced the editor, the
  `ProviderConfig`/`RolesConfig` types (already `Serialize` —
  `config.rs:110,157`), and the commit path this plan redirects; its
  discovered-capabilities rows (`ui.rs:1342–1396`) are untouched.
- **0013 (Preflight Doctor)** owns startup config diagnostics; this plan's
  refusals are status-bar messages inside the editor flow and add no doctor
  coupling.
- **0016 (workstream 0054)** documents the `g` key in README; the behaviour
  change here (commit goes to the global layer) keeps that documentation
  true — coordinate wording if both land close together.
- The editor's read-only keymap and `providers.len() + 3` selection mismatch
  (`event.rs:486–495`, `app.rs:1104`, `ui.rs:1287–1408`) are deliberately
  left for a future editor-interactivity plan; nothing here entrenches them.
