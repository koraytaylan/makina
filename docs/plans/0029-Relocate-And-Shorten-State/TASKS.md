# Makina Plan 0029 — Relocate Runtime State to ~/.makina & Shorten Worktree Names

Move Makina's **transient** runtime state — task worktrees and run logs/metadata —
out of the target repo's `.makina/` and into a per-project namespace under
`~/.makina/projects/{ns}/`, while **keeping** the committed artifacts
(`config.toml`, `tasks/*.json`) where they are. Along the way, **shorten** the
worktree directory + branch names from `{plan_slug}--{task_id}` to a bounded,
still-unique `{plan#}-{task-trunc}-{hash4}`. This is **pre-release and
non-backwards-compatible**: there is **no migration**.

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

## 0080 — Relocate runtime state

### relocate-runtime-state — Worktrees + runs under `~/.makina/projects/{ns}/`

Add the per-project state root and re-root the five transient path helpers there,
leaving the two committed helpers in the repo's `.makina/`.

**Steps:**

1. In `crates/makina-core/src/paths.rs`, add a private `home_dir() ->
   Option<PathBuf>` that mirrors `config.rs::home_dir` exactly —
   `std::env::var_os("HOME").map(PathBuf::from)`. **Do not** add the `dirs` crate.
   Add a private `sanitize_ns_part(&str) -> String` (lowercases, keeps `[a-z0-9-]`,
   maps everything else to `-`, trims leading/trailing `-`) and a private 6-char
   hex hasher `hex6(&[u8]) -> String` over a fixed, release-stable algorithm
   (inline FNV-1a, **not** `DefaultHasher`).

2. Add `pub fn project_ns(repo_root: &Path) -> String`: canonicalize `repo_root`
   (`std::fs::canonicalize`, falling back to the given path on error), take its
   `file_name` as the basename (default `"repo"`), and return
   `format!("{}-{}", sanitize_ns_part(basename), hex6(canonical_path_bytes))`.

3. Add `pub fn state_root(repo_root: &Path) -> PathBuf`: when `HOME` is set,
   `home.join(".makina").join("projects").join(project_ns(repo_root))`; when
   `HOME` is unset, fall back to `repo_root.join(".makina")`. Document the
   env-var read in the doc-comment (the module is otherwise pure).

4. Re-root the transient helpers at `state_root(repo_root)` instead of
   `repo_root.join(".makina")`: change `run_dir` (`paths.rs:53`) and `worktree`
   (`paths.rs:120`); `audit_log` (`paths.rs:68`), `task_log` (`paths.rs:83`), and
   `run_logs_dir` (`paths.rs:96`) inherit the move because they build on `run_dir`.
   **Leave `config_file` (`paths.rs:20`) and `task_graph` (`paths.rs:35`) rooted at
   `repo_root/.makina`.** Update each moved helper's doc-comment example.

5. Update the `paths.rs` unit tests (`run_dir_path`, `audit_log_path`,
   `task_log_path`, `worktree_path`, `run_logs_dir_creates_and_is_idempotent`) to
   assert the moved helpers resolve under `state_root`/`{ns}` (build the expected
   prefix via `state_root`, set `HOME` to a `tempfile::tempdir()`), and keep
   `config_file_path` / `task_graph_path` asserting `repo_root/.makina`. No
   call-site signatures change, so `worktree.rs`, `crates/makina/src/log.rs`
   (`log.rs:251`), `orchestrator.rs` (`:412`, `:935`), `run_metadata.rs`
   (`:167`, `:184`), `audit.rs` (`:402`), `app.rs` (`:1473`), and `event.rs`
   (`:514`) keep compiling unchanged.

6. Add tests in `paths.rs` (set + restore `HOME` to a temp dir; `HOME` is
   process-global, so serialize these or use one test that saves/restores it):

   ```rust
   #[test]
   fn transient_helpers_live_under_state_root() { /* temp HOME; run_dir / worktree / task_log / audit_log / run_logs_dir all start with state_root(repo); config_file + task_graph start with repo_root/.makina */ }
   #[test]
   fn project_ns_is_stable_and_path_distinct() { /* project_ns(p)==project_ns(p) for one path; project_ns(a) != project_ns(b) for two distinct repo paths */ }
   ```

- **Depends on:** —
- **Done when:** the two new tests + the updated path tests pass; `run_dir`,
  `worktree`, `task_log`, `audit_log`, and `run_logs_dir` resolve under
  `~/.makina/projects/{project_ns}/`, while `config_file` and `task_graph` stay
  under `repo_root/.makina`; `paths.rs` reads `$HOME` via `var_os` with no `dirs`
  dependency; `cargo test`/`clippy`/`fmt` green.

---

## 0081 — Shorten worktree names

### shorten-worktree-names — `{plan#}-{task-trunc}-{hash4}` dir + branch

Replace the long `{plan_slug}--{task_id}` worktree dir + branch leaf with a
bounded, deterministic, still-unique short name used identically by `create` and
`remove`.

**Steps:**

1. In `crates/makina-core/src/paths.rs`, add `pub fn
   short_worktree_name(plan_slug: &str, task_id: &str) -> String` returning
   `format!("{plan_num}-{task}-{hash4}")` where: `plan_num` is the leading ASCII-
   digit run of `plan_slug` (fallback to a sanitized head when there are no leading
   digits, never empty); `task` is `sanitize_ns_part(task_id)` truncated to a
   `const TASK_TRUNC: usize = 20;` budget with trailing `-` trimmed; `hash4` is a
   4-char hex hash (`hex4`, same fixed algorithm as `hex6`) of the full
   `format!("{plan_slug}--{task_id}")`. Reuse `sanitize_ns_part` from 0080. The
   result must be `[a-z0-9-]`-valid.

2. In `paths::worktree` (`paths.rs:120`), build the dir leaf from
   `short_worktree_name(plan_slug, task_id)` instead of
   `format!("{plan_slug}--{task_id}")` (the `state_root` re-rooting from 0080
   stays). Update its doc-comment example to the new short form.

3. In `crates/makina-core/src/worktree.rs`, replace **both** branch builders —
   `create` (`worktree.rs:198`) and `remove` (`worktree.rs:271`) — from
   `format!("task/{plan_slug}--{task_id}")` to
   `format!("task/{}", paths::short_worktree_name(plan_slug, task_id))`. Because
   `short_worktree_name` is pure, `remove` regenerates the identical branch +
   (via `worktree_path` at `worktree.rs:325`) path that `create` produced, keeping
   reclaim-on-conflict and teardown correct.

4. Update the `worktree.rs` module doc (`//!`) to describe the new
   `~/.makina/projects/{ns}/worktrees/{short-name}` location + `task/{short-name}`
   branch, and update the invariant test
   `module_doc_describes_makina_plan_scoped_layout` (`worktree.rs:520`) and
   `worktree_path_is_under_repo_root` (`worktree.rs:503`) to match the new
   doc strings + expected short path (rename the latter test if "under repo root"
   no longer describes it).

5. Update `compact_paths` for the relocated worktree path. In
   `crates/makina/src/markup.rs`, `compact_paths` (`markup.rs:174`) currently
   hardcodes `let wt = format!("{root}/.makina/worktrees/");` (`markup.rs:176`)
   and is used at `crates/makina/src/ui.rs` (`ui.rs:1331`,
   `crate::markup::compact_paths(title, &app.repo_root)`) to strip the worktree
   prefix from displayed tool output. Since 0081 lands the final off-repo
   path+name shape (`~/.makina/projects/{ns}/worktrees/{short-name}/`), the
   in-repo `/.makina/worktrees/` pattern no longer matches the real worktree
   path. Update `compact_paths` to strip the **relocated** off-repo worktree
   prefix — derive it via `paths::state_root(repo_root).join("worktrees")` (the
   new project-namespace root from 0080) rather than `{root}/.makina/worktrees/`.
   Strip both the legacy in-repo `/.makina/worktrees/` prefix **and** the new
   `state_root(...)/worktrees/` prefix so output from either layout is compacted
   (the relocated path lives outside `repo_root`, so the trailing
   `out.replace("{root}/", "")` no longer covers it). Keep the existing
   `<root>/` repo-relative stripping for non-worktree paths.

6. Update the `ui.rs` test fixtures that hardcode the old worktree path. Grep
   `crates/makina/src/ui.rs` for `.makina/worktrees/` (around `ui.rs:4277`,
   `:4313`, `:4454`, `:4579`) — these tests construct/inspect
   `.makina/worktrees/{plan}--{id}/` (e.g.
   `.../.makina/worktrees/plan-0009--task1/...` and
   `.../.makina/worktrees/plan--pane-fidelity/...`) and assert the prefix is
   stripped after `compact_paths`. Update them to the new off-repo short-name
   format (build the worktree path via `paths::state_root`/`short_worktree_name`,
   or the equivalent `state_root(...)/worktrees/{short-name}/`), consistent with
   the relocated `compact_paths`, so they exercise the new prefix-stripping.

7. Add tests in `paths.rs`:

   ```rust
   #[test]
   fn short_worktree_name_is_bounded_and_valid() { /* len <= a small bound; only [a-z0-9-]; starts with the plan number, e.g. "0016-..." */ }
   #[test]
   fn short_worktree_name_is_deterministic_and_distinct() { /* same (plan,id) => same name; two distinct ids under one plan => distinct names even when truncated heads collide */ }
   ```

- **Depends on:** relocate-runtime-state
- **Done when:** the two tests pass; the worktree dir leaf and branch both use
  `short_worktree_name` and stay `[a-z0-9-]`-valid; `create` and `remove` derive
  the same path + branch (round-trip); the module-doc invariant test reflects the
  new location/naming; build + ui tests green (`compact_paths` handles the
  relocated off-repo worktree path and the `ui.rs` fixtures use the new
  short-name format); `cargo test`/`clippy`/`fmt` green.

---

## 0082 — Update gitignore + test

### update-gitignore-and-test — repo `.makina/` holds only committed artifacts now

With `worktrees/` and `runs/` gone from the repo's `.makina/`, drop their ignore
rules and rewrite the invariant test to assert the new committed-only reality.

**Steps:**

1. Update `.makina/.gitignore` (currently exactly `/runs/` and `/worktrees/`):
   remove both rules — those directories no longer exist in-repo (they live under
   `~/.makina/projects/{ns}/`). Either delete the file or reduce it to a single
   comment noting that runtime state moved to `~/.makina`. The repo's `.makina/`
   now contains only the committed `config.toml` + `tasks/`.

2. Rewrite `gitignore_worktrees_ignored_tasks_not_ignored` in
   `crates/makina-core/src/persist.rs` (`persist.rs:544`) to assert the new
   reality (rename it, e.g.
   `committed_artifacts_not_ignored_runtime_state_relocated`): drop the
   `.makina/.gitignore` must-list-`/runs/`-and-`/worktrees/` assertions; keep the
   root-`.gitignore`-does-not-ignore-`.makina` assertion (config + tasks stay
   committable); assert no in-repo `.gitignore` is needed to keep `/runs/` or
   `/worktrees/` out, since runtime state is now outside the repo. Keep it
   string-level (no `git check-ignore`) so it runs in any sandbox.

   The `.gitignore` rewrite is correct **because `state_root` puts runtime state
   off-repo when `$HOME` is set** (the normal case). The relocate tests (0080,
   and any test asserting a transient path) set a temp `$HOME` (e.g.
   `tempfile::tempdir()`) precisely so state resolves off-repo and never lands in
   `repo_root/.makina`. The `HOME`-unset fallback to `repo_root/.makina` is
   **best-effort** for `HOME`-less environments and is an explicitly-documented
   edge case (see ARCHITECTURE.md, 0080 `state_root`), not a silent behavior — so
   this test asserts the normal, `HOME`-set reality and does not depend on the
   fallback.

3. The rewritten test stub:

   ```rust
   #[test]
   fn committed_artifacts_not_ignored_runtime_state_relocated() { /* repo_root = two levels above CARGO_MANIFEST_DIR; assert root .gitignore does NOT ignore `.makina`/`.makina/` (config+tasks committable); assert in-repo gitignores do not rely on /runs/ or /worktrees/ rules (runtime state lives under ~/.makina/projects/{ns}) */ }
   ```

- **Depends on:** relocate-runtime-state
- **Done when:** `.makina/.gitignore` no longer lists `/runs/` or `/worktrees/`;
  the rewritten test passes and asserts that `config.toml` + `tasks/` stay
  un-ignored while runtime state lives outside the repo under `~/.makina`; no
  in-repo gitignore rule is required for the relocated dirs; `cargo
  test`/`clippy`/`fmt` green.

---

**End of plan 0029 TASKS.** When every "Done when" bullet is green, Makina writes
its worktrees, run logs, and run metadata to `~/.makina/projects/{ns}/` — never
into the target repo — under short, unique `{plan#}-{task-trunc}-{hash4}` names,
while `config.toml` and `tasks/*.json` remain committed inside the project's
`.makina/`.
