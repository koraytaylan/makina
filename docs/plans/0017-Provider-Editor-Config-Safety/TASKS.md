# Makina Plan 0017 — Provider Editor Config Safety

Stop the provider editor's `Enter` from rewriting the committed project config
as `GlobalConfig` (deleting `base_branch` and every `[[gates]]` entry):
commit providers/roles to the **global** `~/.makina/config.toml` via a
lossless `toml::Table` read-modify-write, seed the editor from the loaded
config in `main.rs` (today it is always empty in production), and refuse
empty/unseeded commits.

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

## 0055 — Write the right layer, losslessly

### global-config-path-helper — Extract the global-layer path derivation

The global config path is derived inline in `Config::load_defaults`
(`config.rs:760`); the editor needs the same derivation, so extract it once.

**Steps:**

1. In `crates/makina-core/src/config.rs`, next to `home_dir()`
   (`config.rs:798–800`), add:

   ```rust
   pub fn global_config_file() -> Option<PathBuf> { home_dir().map(global_config_file_in) }
   pub fn global_config_file_in(home: impl Into<PathBuf>) -> PathBuf { /* {home}/.makina/config.toml */ }
   ```

2. Switch `load_defaults` (`config.rs:759–764`) to call it — one derivation,
   two callers.

3. Add a test:

   ```rust
   #[test]
   fn global_config_path_is_home_dot_makina() { /* global_config_file_in("/home/u") == /home/u/.makina/config.toml */ }
   ```

- **Depends on:** —
- **Done when:** the test passes; `load_defaults` and the helper share one
  derivation (no duplicated `.join(".makina").join("config.toml")` for the
  global layer); cargo test/clippy/fmt green.

### lossless-global-commit — Rewrite `commit_provider_config` for the global layer

`commit_provider_config` (`event.rs:294–341`) writes the **project** file
(`event.rs:303`) through `GlobalConfig` (`event.rs:306,322`), destroying
`base_branch` and all `[[gates]]` (Deserialize-only — `config.rs:363,386,406`)
despite the comment at `event.rs:300–302` claiming otherwise.

**Steps:**

1. In `crates/makina/src/app.rs`, add
   `pub global_config_path: Option<PathBuf>` to `App`, defaulted in
   `App::new` from `makina_core::config::global_config_file()`; tests inject
   a tempdir path (never mutate `HOME` — process-global env races parallel
   tests).

2. Rewrite `commit_provider_config` per
   [ARCHITECTURE.md](ARCHITECTURE.md): target `app.global_config_path`
   (refuse with a status message when `None`); read the file into a
   `toml::Table` (missing file → empty table; **unparseable file → refuse,
   don't clobber** — replaces the `unwrap_or_default()` at `event.rs:306`);
   `insert` only the `providers` and `roles` keys (both `Serialize` —
   `config.rs:110,157`); serialize and write back. Remove the
   `paths::config_file` import and fix the stale doc comment
   (`event.rs:289–290`).

3. Repoint the existing test `provider_editor_commit_writes_config`
   (`event.rs:1335–1423`) at the injected global path (it currently asserts
   the project path is written — `event.rs:1391`), keeping its provider/role
   round-trip assertions.

4. Add the regression tests:

   ```rust
   #[tokio::test]
   async fn editor_commit_preserves_unmanaged_global_fields() { /* seed [backend]/[planner]/[caps]/concurrency + unknown [future] table; commit; all keys survive, only providers/roles changed */ }
   #[tokio::test]
   async fn editor_commit_never_touches_project_config() { /* seed {repo_root}/.makina/config.toml shaped like THIS repo (base_branch + concurrency + [caps] + three [[gates]], cf. .makina/config.toml:26–63); commit; project file bytes identical, still parses as ProjectConfig with 3 gates */ }
   #[tokio::test]
   async fn editor_commit_refuses_invalid_global_toml() { /* malformed global file => error status, file unchanged */ }
   #[tokio::test]
   async fn editor_commit_refuses_without_home() { /* global_config_path == None => refusal message, nothing written */ }
   ```

- **Depends on:** global-config-path-helper
- **Done when:** all four tests pass; an editor commit can no longer modify
  `{repo_root}/.makina/config.toml` under any input; unmanaged global fields
  (including unknown keys) round-trip losslessly; parse failures refuse
  instead of clobbering; cargo test/clippy/fmt green.

---

## 0056 — Seed the editor in the binary + empty-state guard

### seed-editor-in-main — Wire `App::with_config` into production

`App::with_config` (`app.rs:756`, doc: "Use this variant when a config is
available (main.rs)" — `app.rs:752–755`) is called only from a test
(`app.rs:2374`); `main.rs:216` uses `App::new`, so the production editor is
always empty.

**Steps:**

1. In `crates/makina/src/main.rs`, capture
   `let (providers, roles) = (config.providers.clone(), config.roles.clone());`
   **before** `config` is moved into `CoreApi::with_audit_registry`
   (`main.rs:204–212`; resolved `Config` exposes both — `config.rs:457,460`),
   then replace the `App::new` call (`main.rs:216`) with
   `App::with_config(Arc::clone(&api), initial_runs, repo_root, providers, roles)`.

2. Add a wiring-level test:

   ```rust
   #[test]
   fn app_with_config_seeds_editor_from_resolved_config() { /* GlobalConfig::from_toml_str with [[providers]]/[roles] + Config::resolve; App::with_config exactly as main.rs; dispatch OpenProviderEditor; editor lists the configured providers/roles */ }
   ```

   (Complements `provider_editor_opens_and_lists_providers` —
   `app.rs:2344–2380` — which hands in literals rather than a parsed config.)

- **Depends on:** —
- **Done when:** the test passes; `App::with_config` has a production caller
  in `main.rs`; pressing `g` in a configured binary lists the configured
  providers/roles instead of an empty pane; cargo test/clippy/fmt green.

### guard-empty-commit — Refuse to commit an unseeded editor

**Steps:**

1. At the top of `commit_provider_config`, when `editor.providers.is_empty()`
   return `Some("Config not saved: provider editor is empty (no providers
   loaded)")` without touching any file. (Resolve synthesizes a default
   provider from `[backend]` when none are declared — `config.rs:284–291` —
   so a legitimately-empty providers list never needs persisting.)

2. Add a test:

   ```rust
   #[tokio::test]
   async fn commit_refuses_empty_editor() { /* App::new (mirrors the pre-0056 production path), open editor, commit => refusal status; neither global nor project file is created */ }
   ```

- **Depends on:** lossless-global-commit
- **Done when:** the test passes; an empty editor's `Enter` produces a
  status message and zero filesystem writes; combined with 0055 the
  unseeded-binary-zeroes-the-config path is closed end to end; cargo
  test/clippy/fmt green.

---

**End of plan 0017 TASKS.** When every "Done when" bullet is green, `g` shows
the configuration that is actually loaded, `Enter` writes providers/roles to
the layer they belong to without losing a single unmanaged field, and the
committed project config — gates included — can no longer be silently
destroyed from the TUI.
