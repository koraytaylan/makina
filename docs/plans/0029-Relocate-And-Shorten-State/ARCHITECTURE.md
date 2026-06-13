# Architecture — Plan 0029

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan touches `crates/makina-core` (paths, worktree,
> persist) plus a handful of `makina` (TUI) call sites.

## Current shape (what exists)

- **Path helpers** (`crates/makina-core/src/paths.rs`): all pure string joins
  rooted at `repo_root/.makina`. `config_file` (`paths.rs:20`,
  `.makina/config.toml`), `task_graph` (`paths.rs:35`, `.makina/tasks/{slug}.json`),
  `run_dir` (`paths.rs:53`, `.makina/runs/{run_id}`), `audit_log` (`paths.rs:68`),
  `task_log` (`paths.rs:83`), `run_logs_dir` (`paths.rs:96`, the **one I/O**
  helper — `create_dir_all`), `worktree` (`paths.rs:120`,
  `.makina/worktrees/{plan_slug}--{task_id}`). Each function takes `repo_root:
  &Path`.
- **Worktree manager** (`crates/makina-core/src/worktree.rs`): `WorktreeManager {
  repo_root, base_branch }` (`worktree.rs:133`). `worktree_path` (`worktree.rs:325`)
  delegates to `paths::worktree`. The branch is built **twice** as
  `format!("task/{plan_slug}--{task_id}")` — in `create` (`worktree.rs:198`) and in
  `remove` (`worktree.rs:271`). `validate_task_id` (`worktree.rs:390`) enforces
  non-empty, no `..`/`/`, charset `[a-z0-9-]`. A module-doc invariant test
  (`worktree.rs:520`, `module_doc_describes_makina_plan_scoped_layout`) asserts the
  `//!` block references `.makina/worktrees/{plan_slug}--{task_id}/`.
- **`$HOME` reading** (`crates/makina-core/src/config.rs`): the **only** place
  `$HOME` is read is the private `home_dir()` (`config.rs:915`):
  `std::env::var_os("HOME").map(PathBuf::from)`, used at `config.rs:865` for the
  global config. **No `dirs` crate** is a dependency.
- **Callers of the transient helpers:**
  - `crates/makina/src/log.rs` — `RunFileLayer::append` (`log.rs:251`) uses
    `paths::task_log`, `paths::run_dir(...).join("logs")`, and
    `paths::run_logs_dir`. It holds a `repo_root: PathBuf` (`log.rs:240`).
  - `crates/makina-core/src/orchestrator.rs` — `paths::run_logs_dir` at
    `orchestrator.rs:412` and `orchestrator.rs:935`.
  - `crates/makina-core/src/run_metadata.rs` — `write_run_metadata`
    (`run_metadata.rs:167`) and `read_run_metadata` (`run_metadata.rs:184`) use
    `paths::run_dir(...).join("run.json")`.
  - `crates/makina-core/src/audit.rs` — `paths::audit_log` (`audit.rs:402`).
  - `crates/makina/src/app.rs` — `paths::run_dir(...).join("logs")`
    (`app.rs:1473`) for replay transcript loading.
  - `crates/makina/src/event.rs` — `paths::task_log` (`event.rs:514`).
  - `WorktreeManager::new` constructions: `crates/makina/src/main.rs:204`,
    `crates/makina-core/src/actors/mod.rs:186` and `:386`.
- **gitignore + its test**: `.makina/.gitignore` currently contains exactly two
  lines, `/runs/` and `/worktrees/`. The root `.gitignore` has **no** makina/
  worktree rules. `persist.rs::gitignore_worktrees_ignored_tasks_not_ignored`
  (`persist.rs:544`) asserts `.makina/.gitignore` lists both, and that the root
  `.gitignore` does not ignore `.makina` or `/.worktrees/`.

The crucial split this plan formalizes: **`repo_root/.makina`** holds only the two
*committed* helpers (`config_file`, `task_graph`); **`state_root(repo_root)`**
(`~/.makina/projects/{ns}`) holds the five *transient* helpers (`worktree`,
`run_dir`, `run_logs_dir`, `task_log`, `audit_log`).

## 0080 — Relocate runtime state

Edits primarily in `crates/makina-core/src/paths.rs`, plus a `repo_root` →
`state_root` swap in `worktree.rs` and at each transient-helper call site.

- **Add `$HOME` access to `paths.rs`.** Mirror `config.rs::home_dir` exactly — do
  **not** add the `dirs` crate. Because `paths.rs` is documented as "pure string/
  path join: no I/O, no validation", note in the doc-comment that `state_root`
  reads the `HOME` *environment variable* (not the filesystem) and that the result
  is therefore a pure function of `HOME` + `repo_root`:

  ```rust
  /// Resolve `$HOME` via the `HOME` env var (mirrors `config.rs::home_dir`;
  /// deliberately no `dirs` crate). `None` when `HOME` is unset.
  fn home_dir() -> Option<PathBuf> {
      std::env::var_os("HOME").map(PathBuf::from)
  }
  ```

- **`project_ns(repo_root) -> String`.** Canonicalize the repo path (fall back to
  the given path if canonicalization fails — e.g. a not-yet-created dir in a
  test), take its file-name as the human basename, and append a 6-char hex hash of
  the canonical path string. Sanitize the basename to `[a-z0-9-]` so the namespace
  stays filesystem- and branch-safe:

  ```rust
  /// Per-project namespace under `~/.makina/projects/`:
  /// `"{repo_basename}-{hash6}"`, where `hash6` is a 6-char hex hash of the
  /// canonicalized absolute repo path. Stable for a given path; distinct for
  /// two different repo paths even when they share a basename.
  pub fn project_ns(repo_root: &Path) -> String {
      let canonical = std::fs::canonicalize(repo_root)
          .unwrap_or_else(|_| repo_root.to_path_buf());
      let basename = sanitize_ns_part(
          canonical.file_name().and_then(|s| s.to_str()).unwrap_or("repo"),
      );
      let hash6 = hex6(canonical.to_string_lossy().as_bytes());
      format!("{basename}-{hash6}")
  }
  ```

  Use a small stable hasher (e.g. `std::hash::Hasher` via
  `std::collections::hash_map::DefaultHasher` is **not** stable across releases —
  prefer a fixed algorithm such as FNV-1a implemented inline, or an already-present
  hashing dep; pick one and keep it deterministic). Hex-truncate to 6 chars.

- **`state_root(repo_root) -> PathBuf`.** `~/.makina/projects/{ns}`. When `HOME`
  is unset, fall back to `repo_root/.makina` so a `HOME`-less environment still has
  *a* writable root rather than panicking (document this):

  ```rust
  /// Runtime-state root for a project: `$HOME/.makina/projects/{project_ns}`.
  /// Holds the transient `worktrees/` and `runs/` trees. Falls back to
  /// `repo_root/.makina` when `HOME` is unset.
  pub fn state_root(repo_root: &Path) -> PathBuf {
      match home_dir() {
          Some(home) => home.join(".makina").join("projects").join(project_ns(repo_root)),
          None => repo_root.join(".makina"),
      }
  }
  ```

  The `HOME`-unset branch is an **explicitly-documented edge case**, not a silent
  behavior: when `HOME` is set (the normal, expected case), `state_root` resolves
  **off-repo** to `~/.makina/projects/{ns}` and the transient `worktrees/` +
  `runs/` trees never touch `repo_root/.makina`. Only when `HOME` is unset does
  `state_root` fall back to `repo_root/.makina` — a **best-effort** root so a
  `HOME`-less environment still has *a* writable location rather than panicking.
  Document this fallback in the `state_root` doc-comment. This edge case is why
  0082 can safely drop the `/runs/` and `/worktrees/` ignore rules: with `HOME`
  set those dirs are off-repo, and the relocate tests (0080) pin `HOME` to a temp
  dir so state is deterministically off-repo. In the rare `HOME`-unset fallback,
  the dropped rules mean a transient `runs/`/`worktrees/` *could* reappear in-repo
  uncommitted — an accepted, documented trade-off for this pre-release plan, not a
  silent regression.

- **Re-root the five transient helpers.** Replace `repo_root.join(".makina")` with
  `state_root(repo_root)` in `run_dir`, `worktree`, and — transitively, since they
  build on `run_dir` — `audit_log`, `task_log`, `run_logs_dir`. Leave
  `config_file` and `task_graph` rooted at `repo_root/.makina`:

  ```rust
  pub fn run_dir(repo_root: &Path, run_id: &str) -> PathBuf {
      state_root(repo_root).join("runs").join(run_id)
  }
  pub fn worktree(repo_root: &Path, plan_slug: &str, task_id: &str) -> PathBuf {
      state_root(repo_root)
          .join("worktrees")
          .join(short_worktree_name(plan_slug, task_id)) // name from 0081
  }
  ```

  (`audit_log`/`task_log`/`run_logs_dir` already delegate to `run_dir`, so they
  move for free.) Update the doc-comment examples + the `paths.rs` unit tests
  (`run_dir_path`, `audit_log_path`, `task_log_path`, `worktree_path`,
  `run_logs_dir_creates_and_is_idempotent`) to assert under
  `state_root`/`{ns}` rather than `/repo/.makina`, using a temp `HOME`.

- **No call-site signature changes.** Every helper still takes `repo_root: &Path`,
  so `worktree.rs`, `log.rs`, `orchestrator.rs`, `run_metadata.rs`, `audit.rs`,
  `app.rs`, and `event.rs` call them unchanged — the re-rooting is internal. The
  `WorktreeManager` keeps `repo_root` (its `worktree_path` at `worktree.rs:325`
  still calls `paths::worktree(&self.repo_root, …)`, now resolving under
  `state_root`). The TUI `RunFileLayer` keeps its `repo_root` field; its
  `paths::*` calls resolve under `state_root` automatically.

- **Tests use a temp `$HOME`.** Set `HOME` to a `tempfile::tempdir()` for any test
  that asserts a transient path (serialize via a guard or a single-threaded test;
  `HOME` is process-global). Assert: `run_dir`/`worktree`/`task_log`/`audit_log`/
  `run_logs_dir` resolve under `{tmp_home}/.makina/projects/{ns}/…`; `config_file`
  and `task_graph` still resolve under `repo_root/.makina`; `project_ns` is stable
  for one path and distinct for two distinct paths.

## 0081 — Shorten worktree names

Edits in `crates/makina-core/src/paths.rs` (new `short_worktree_name`, used by
`worktree`) and `crates/makina-core/src/worktree.rs` (branch construction in
`create` + `remove`).

- **`short_worktree_name(plan_slug, task_id) -> String`.** Produce
  `{plan#}-{task-trunc}-{hash4}`:
  - `plan#` — the leading run of ASCII digits in `plan_slug` (e.g. `"0016"` from
    `"0016-sidebar-tree"`); if `plan_slug` has no leading digits, use the
    sanitized `plan_slug` head as a fallback so the name is never empty.
  - `task-trunc` — `task_id` sanitized to `[a-z0-9-]` and truncated to a fixed
    budget (define `const TASK_TRUNC: usize = 20;`), trimming any trailing `-`.
  - `hash4` — 4-char hex hash of the **full** `format!("{plan_slug}--{task_id}")`,
    so two tasks that truncate to the same prefix still differ.

  ```rust
  const TASK_TRUNC: usize = 20;

  /// Bounded, unique, `[a-z0-9-]`-valid worktree dir/branch leaf:
  /// `"{plan#}-{task-trunc}-{hash4}"`, e.g. `"0016-sidebar-tree-nav-a1b2"`.
  /// Deterministic in `(plan_slug, task_id)`, so `create` and `remove` agree.
  pub fn short_worktree_name(plan_slug: &str, task_id: &str) -> String {
      let plan_num = leading_digits(plan_slug); // "0016", fallback to sanitized head
      let task = truncate_trim(&sanitize_ns_part(task_id), TASK_TRUNC);
      let hash4 = hex4(format!("{plan_slug}--{task_id}").as_bytes());
      format!("{plan_num}-{task}-{hash4}")
  }
  ```

  Share the `sanitize_ns_part` + hex helpers with `project_ns` (0080).

- **Use it in `paths::worktree`.** Already shown in 0080 — the dir leaf becomes
  `short_worktree_name(plan_slug, task_id)` instead of `{plan_slug}--{task_id}`.

- **Use it for the branch in `worktree.rs`.** Replace **both**
  `format!("task/{plan_slug}--{task_id}")` occurrences (`create` at
  `worktree.rs:198`, `remove` at `worktree.rs:271`) with
  `format!("task/{}", paths::short_worktree_name(plan_slug, task_id))`. Because the
  function is pure, `remove` regenerates the **same** branch `create` made, so the
  reclaim-on-conflict + teardown paths stay correct. The `worktree_path`
  (`worktree.rs:325`) already routes through `paths::worktree`, so the dir leaf
  matches the branch leaf automatically.

- **Charset stays valid.** `short_worktree_name`'s output is `[a-z0-9-]` (digits,
  sanitized kebab, hex), so it satisfies `validate_task_id`'s charset for the
  composite and is safe as both a path segment and a git branch name. `--` no
  longer appears in the leaf; the delimiter is a single `-`, and the trailing
  `hash4` guarantees uniqueness.

- **Update the module-doc + invariant test.** The `worktree.rs` module doc
  (`//!`) and `module_doc_describes_makina_plan_scoped_layout` (`worktree.rs:520`)
  reference `.makina/worktrees/{plan_slug}--{task_id}/`. Since runtime state now
  lives under `~/.makina/projects/{ns}/worktrees/{short-name}`, update the module
  doc to describe the new location + naming and update that invariant test's
  asserted strings accordingly (it `include_str!`s the source and matches on the
  doc block). Also update `worktree_path_is_under_repo_root` (`worktree.rs:503`)
  to assert the new short leaf under `state_root` (rename if "under repo root" no
  longer holds).

- **Re-root the TUI's `compact_paths`.** `crates/makina/src/markup.rs::compact_paths`
  (`markup.rs:174`) hardcodes the in-repo worktree prefix
  `let wt = format!("{root}/.makina/worktrees/");` (`markup.rs:176`) and is used
  at `crates/makina/src/ui.rs:1331` to strip the worktree prefix from displayed
  tool-output titles. After 0080+0081 the worktree lives **off-repo** at
  `~/.makina/projects/{ns}/worktrees/{short-name}/`, so `{root}/.makina/worktrees/`
  no longer matches the real path — and because the relocated path is **outside**
  `repo_root`, the trailing `out.replace("{root}/", "")` does not cover it either.
  Update `compact_paths` to derive the worktree prefix from
  `paths::state_root(repo_root).join("worktrees")` (the new namespace root) and
  strip **both** the relocated `state_root(...)/worktrees/{short-name}/` prefix
  **and** the legacy in-repo `/.makina/worktrees/` prefix, then keep the existing
  repo-relative `<root>/` stripping for non-worktree paths. The `ui.rs` test
  fixtures that hardcode `.makina/worktrees/{plan}--{id}/` (grep `ui.rs` for
  `.makina/worktrees/` — around `ui.rs:4277`, `:4313`, `:4454`, `:4579`; e.g.
  `plan-0009--task1` and `plan--pane-fidelity`) must move to the new off-repo
  short-name layout (build the path via `paths::state_root`/`short_worktree_name`)
  so they exercise the relocated prefix-stripping. Done-when for 0081 includes
  build + ui tests green.

## 0082 — Update gitignore + test

Edits in `.makina/.gitignore` and
`crates/makina-core/src/persist.rs::gitignore_worktrees_ignored_tasks_not_ignored`
(`persist.rs:544`).

- **`.makina/.gitignore`.** It currently holds exactly `/runs/` and `/worktrees/`.
  Both directories no longer exist in-repo (they live under `~/.makina/projects/
  {ns}/`), so those rules are dead. The repo's `.makina/` now holds only the
  committed `config.toml` + `tasks/`. Either delete `.makina/.gitignore` or reduce
  it to a comment explaining that runtime state moved to `~/.makina`. The plan's
  test must match whichever choice is made.

- **Rewrite the invariant test.** The current test asserts `.makina/.gitignore`
  lists `/runs/` and `/worktrees/`; that is now false. Rewrite it to assert the
  *new* reality:
  - `.makina/` contains only committed artifacts; no `/runs/` or `/worktrees/`
    rule is **required** (and the dirs themselves are not present in-repo).
  - The root `.gitignore` still does **not** ignore `.makina` (so `config.toml` +
    `tasks/` remain committable) — keep that half of the original assertion.
  - `~/.makina` is outside the repo entirely, so nothing about it belongs in any
    in-repo `.gitignore`.

  ```rust
  #[test]
  fn committed_artifacts_not_ignored_runtime_state_relocated() {
      /* repo_root = two levels above CARGO_MANIFEST_DIR.
         Assert the root .gitignore does NOT ignore `.makina` / `.makina/`
         (config + tasks stay committable). Assert no in-repo `.gitignore`
         relies on `/runs/` or `/worktrees/` rules to keep runtime state out,
         because that state now lives under `~/.makina/projects/{ns}/`. */
  }
  ```

  Keep it string-level (no `git check-ignore`) so it runs in any sandbox, exactly
  as the original did.

- **Why dropping the rules is correct.** The `/runs/` and `/worktrees/` ignore
  rules are safe to drop because, with `$HOME` set (the normal case),
  `state_root` puts those trees **off-repo** under `~/.makina/projects/{ns}/` —
  they never appear in `repo_root/.makina`. The relocate tests pin `$HOME` to a
  temp dir so this off-repo placement is deterministic. The `HOME`-unset fallback
  to `repo_root/.makina` (see 0080 `state_root`) is an explicitly-documented,
  best-effort edge case; in that rare environment a transient `runs/`/`worktrees/`
  could reappear in-repo uncommitted — an accepted trade-off for this pre-release
  plan, not a silent regression.

## Testing notes

- **Temp `$HOME`.** Path tests that touch `state_root` must set `HOME` to a temp
  dir. `HOME` is process-global, so either run those asserts in one test that
  saves/restores `HOME`, or gate them behind a serializing guard. Prefer building
  the *expected* path via `paths::state_root(repo_root)` rather than hard-coding
  `~/.makina/...`, so the test is robust to the chosen hash.
- **Determinism / distinctness.** `project_ns` and `short_worktree_name` are pure:
  assert same-input ⇒ same-output, and two distinct inputs ⇒ distinct outputs
  (different repo paths ⇒ different `ns`; two task ids under one plan ⇒ different
  short names even if the truncated head collides, thanks to `hash4`).
- **Round-trip symmetry.** A `create`→`remove` test (the existing worktree tests
  in `worktree.rs`/`orchestrator.rs`) must still pass: `remove` recomputes the
  same dir + branch `create` produced.
- `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --check` stay green.
