# Architecture — Plan 0013 (deltas)

> Edits in `crates/makina-core/src/config.rs`, a new
> `crates/makina-core/src/preflight.rs`, `crates/makina/src/main.rs`, and the TUI
> (`app.rs`/`event.rs`/`ui.rs`). Line numbers are hints; locate by symbol.

## 0044 — Actionable config errors

Today the load path is `Config::load_defaults()` (`config.rs`, called at
`main.rs:39`), which resolves the global path (`~/.makina/config.toml` via
`home_dir()`) and the project path (`resolve_project_config_path`), parses each
with `from_toml_str(toml, source_label)` (`config.rs:336`/`:435`), merges, and
calls `validate()` (`config.rs:590`). Errors are `ConfigError::Parse {
source_label, .. }` and `ConfigError::Validation { reason }` (`config.rs:47`).
`main.rs:42` prints `failed to load configuration: {e}` and exits.

Edits — make the *presentation* actionable without changing rules:

- **Carry the file into validation errors.** `validate()` knows the merged
  config but not which file a field came from. Add a light wrapper —
  `Config::load_defaults` already holds both resolved `Option<PathBuf>`s — that,
  on a `Validation` error, appends a context line listing the two paths it read
  (and which existed), e.g.:

  ```
  failed to load configuration: backend.command must not be empty

    checked:  ~/.makina/config.toml         (not found)
              .makina/config.toml            (found)
    project settings override global on conflict.
    no provider is defined and the legacy [backend].command is empty — add a
    [[providers]] entry or set [backend] command. See README "Configure".
  ```

  Implement as a `ConfigError::Validation` → display enrichment in `main.rs`
  (preferred: keep the engine error pure; format the guidance in the binary where
  the resolved paths are in hand) **or** add an optional `context: Option<String>`
  to the error variants. Pick the binary-side formatter to avoid widening the
  public error type.

- **List the valid providers on the unknown-provider error.** At `config.rs:622`
  the message is `role '{}' references unknown provider {:?}`. Extend the `reason`
  to append `— defined providers: [a, b, default]` computed from
  `self.providers.iter().map(|p| &p.name)`. This stays inside the engine because
  the data is local to `validate()`.

- **Label parse errors as global vs project.** `from_toml_str` already takes a
  `source_label`; ensure the two call sites pass human labels (`"global
  (~/.makina/config.toml)"`, `"project (.makina/config.toml)"`) so a TOML syntax
  error names the file, not just `display()`.

## 0045 — Provider binary preflight

New module `crates/makina-core/src/preflight.rs`:

```rust
pub struct ProviderProbe {
    pub provider: String,    // provider name
    pub command: String,     // first token of `command`
    pub resolved: Option<PathBuf>,   // Some(path) if found on PATH or absolute
    pub note: Option<String>,        // e.g. "$PATH empty", "is a directory"
}

/// Resolve each provider's command binary against $PATH (first token; honour an
/// absolute/`./` path directly). Pure filesystem + env; never spawns the agent.
pub fn probe_providers(cfg: &Config) -> Vec<ProviderProbe> { … }
```

Resolution mirrors shell lookup of the command's first whitespace token: if it
contains `/`, stat it directly; otherwise split `$PATH` and look for an
executable entry. No process is spawned (spawning an unauthenticated agent can
itself hang — see plan 0015), so the probe checks **presence**, not auth.

Wiring:

- `main.rs` calls `probe_providers(&config)` after load and passes the results
  into the TUI app state.
- TUI renders a dismissible **warning banner / panel** when any probe has
  `resolved == None`: `⚠ provider "default" command 'gemini' not found on PATH —
  install it or fix [[providers]].command`. Non-fatal; the app continues to the
  normal browse state.

## 0046 — In-app Doctor view + first-run scaffold

- **Mode + key.** Add `Mode::Doctor` alongside `Normal`/`FileBrowser`/
  `ProviderConfig` (`app.rs`), opened with `?` (and listed in the status bar).
  `Esc` returns to `Normal`.
- **Checks.** The doctor renders a checklist computed from data already on hand:
  - config files: which of the two paths exist (from 0044's resolved paths);
  - providers: reuse `probe_providers` results from 0045 (✓/✗ per provider);
  - base branch: `config.base_branch` exists in the repo (cheap `git rev-parse
    --verify` via the existing git seam, or `worktree`/`merge` helpers in
    `makina-core`);
  - workspace: `.makina/` present and writable.
  Each row is `✓`/`✗`/`⚠` + a one-line remedy.
- **First-run scaffold.** When **neither** config file exists, the doctor (and a
  hint on the empty Runs sidebar) offers `[w] write starter config`. On confirm,
  write a commented template to `~/.makina/config.toml` and `.makina/config.toml`
  (reuse the snippets from README "Configure"; never overwrite an existing file —
  re-probe and refuse if present). Emit a status message naming the files
  written.

## Test strategy

- `config_error_lists_defined_providers`: a config with `roles.developer.provider
  = "nope"` and providers `["a","default"]` → the `Validation` reason contains
  `nope` **and** `a`, `default`.
- `parse_error_names_source_file`: malformed project TOML → error string contains
  the project label.
- `probe_reports_missing_binary`: `probe_providers` on a config whose command is
  `definitely-not-a-real-binary-xyz` returns `resolved: None`; an absolute path to
  an existing executable resolves to `Some`.
- `probe_resolves_command_on_synthetic_path`: set `$PATH` to a tempdir containing
  an executable named `foo`; a provider with `command = "foo --acp"` resolves to
  that path.
- `doctor_overlay_lists_checks` / `doctor_scaffold_writes_when_absent`: render the
  doctor with a no-config fixture; assert the checklist rows render and that the
  scaffold action targets the two paths and refuses when a file exists.

`cargo test`, `clippy --all-targets -D warnings`, and `fmt --check` stay green.

## Interaction with prior plans

- Shares the status-bar string with 0012; 0013 adds `[?] doctor` without removing
  existing hints. Shares the modal/`Mode` machinery with 0011's provider editor —
  the doctor is a sibling mode, not a change to it. Independent of 0009/0010.
