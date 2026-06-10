# Makina Plan 0013 — Preflight Doctor & First-Run Guidance

Make the onboarding path legible: turn cryptic config errors into actionable,
file-named guidance; add an up-front provider-binary preflight; and add an in-app
**Doctor** view that can scaffold a starter config when none exists.

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

## 0044 — Actionable config errors

### enrich-config-errors — Name the file, show precedence, list valid providers

Config load failures (`main.rs:42` printing `failed to load configuration: {e}`)
must tell the user *which file* and *how to fix it*.

**Steps:**

1. In `crates/makina-core/src/config.rs`, in `validate()` (the unknown-provider
   arm near `config.rs:622`, `role '{}' references unknown provider {:?}`),
   append the defined provider names to the `reason`:
   `… — defined providers: [{}]` from `self.providers.iter().map(|p| &p.name)`.
   If `self.providers` is empty, say `— no [[providers]] are defined`.

2. Ensure the two `from_toml_str(toml, source_label)` call sites in
   `load_defaults` pass human labels (`"global (~/.makina/config.toml)"` and
   `"project (.makina/config.toml)"`) so `ConfigError::Parse` names the file.

3. In `crates/makina/src/main.rs`, where `Config::load_defaults()` errors
   (`main.rs:39–43`), format a multi-line guidance block using the two resolved
   paths (which file existed, the "project overrides global" note, and a pointer
   to README "Configure"). Keep the engine `ConfigError` unchanged; do the
   path-aware formatting in the binary where both `Option<PathBuf>`s are in hand
   (expose them from the load path if not already returned).

4. Add tests:

   ```rust
   #[test]
   fn config_error_lists_defined_providers() { /* roles.developer.provider="nope", providers=[a,default]; assert reason contains "nope","a","default" */ }
   #[test]
   fn parse_error_names_source_file() { /* malformed project TOML; assert error string contains the project label */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; the unknown-provider `reason` includes the
  defined provider names; parse errors name global vs project; cargo
  test/clippy/fmt green.

---

## 0045 — Provider binary preflight

### provider-preflight — Resolve each provider command on PATH before a run

Add a non-fatal check that each provider's binary exists, surfaced as a warning
the user can act on instead of a late mid-task failure.

**Steps:**

1. Create `crates/makina-core/src/preflight.rs` with `ProviderProbe { provider,
   command, resolved: Option<PathBuf>, note: Option<String> }` and
   `pub fn probe_providers(cfg: &Config) -> Vec<ProviderProbe>`. Resolve the
   command's **first whitespace token**: if it contains `/`, stat it; otherwise
   walk `$PATH` for an executable of that name. Never spawn the agent. Export the
   module from `crates/makina-core/src/lib.rs`.

2. In `crates/makina/src/main.rs`, after a successful load, call
   `probe_providers(&config)` and pass the `Vec<ProviderProbe>` into the TUI app
   state (new field on the app struct, seeded at construction).

3. In the TUI, render a dismissible warning line/panel when any probe has
   `resolved.is_none()`: `⚠ provider "{provider}" command '{command}' not found
   on PATH`. Non-fatal — the app proceeds to the normal browse state.

4. Add tests:

   ```rust
   #[test]
   fn probe_reports_missing_binary() { /* command="definitely-not-real-xyz" => resolved None */ }
   #[test]
   fn probe_resolves_command_on_synthetic_path() { /* tempdir with exec "foo"; PATH=tempdir; command="foo --acp" => resolved Some(that path) */ }
   ```

- **Depends on:** —
- **Done when:** both tests pass; `probe_providers` resolves real binaries and
  reports missing ones without spawning; the TUI shows a non-fatal warning for a
  missing provider binary; cargo test/clippy/fmt green.

---

## 0046 — In-app Doctor view + first-run scaffold

### doctor-view — Health checklist overlay with starter-config scaffold

Add a `[?]` Doctor overlay that lists health checks and, when no config exists,
offers to write a commented starter config.

**Steps:**

1. In `crates/makina/src/app.rs`, add `Mode::Doctor` to the mode enum and an
   `AppEvent`/key to open and close it. In `crates/makina/src/event.rs`, bind `?`
   (normal mode) → open doctor; `Esc` → return to `Normal`.

2. In `crates/makina/src/ui.rs`, render the doctor overlay (mirror the
   `ProviderConfig` modal style at `ui.rs:1259`). Rows, each `✓`/`✗`/`⚠` + remedy:
   - config files found (from 0044's resolved paths);
   - providers resolvable (reuse 0045 `ProviderProbe` results);
   - `config.base_branch` exists in the repo (cheap git check via the existing
     `makina-core` git seam);
   - `.makina/` present and writable.

3. Add `[?] doctor` to the status-bar hint string (coordinate with 0012's string;
   do not consume a letter beyond `?`).

4. **First-run scaffold.** When *neither* config file exists, show `[w] write
   starter config` in the doctor and a hint on the empty Runs sidebar. On confirm,
   write commented templates (from README "Configure") to `~/.makina/config.toml`
   and `.makina/config.toml`; **never overwrite** an existing file (re-probe and
   refuse, emitting a status message). Emit a status message naming files written.

5. Add tests:

   ```rust
   #[test]
   fn doctor_overlay_lists_checks() { /* render doctor with a no-config + missing-binary fixture; assert rows render with ✓/✗ */ }
   #[test]
   fn doctor_scaffold_refuses_when_present() { /* with an existing config file, the scaffold action is a no-op and reports refusal */ }
   ```

- **Depends on:** provider-preflight, enrich-config-errors
- **Done when:** both tests pass; `?` opens a Doctor overlay listing the four
  checks; the scaffold writes both templates only when absent and refuses to
  overwrite; status bar advertises `[?]`; cargo test/clippy/fmt green.

---

**End of plan 0013 TASKS.** When every "Done when" bullet is green, a first-time
user gets a named, fixable error instead of a cryptic one, a warning before a
missing-binary run, and an in-app Doctor that can bootstrap a starter config.
