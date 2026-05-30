# Makina Plan 0003 — Runtime & TUI Hardening

Structured-text task list for the second post-MVP plan, derived from the
plan-0002 dogfood findings. Unifies runtime state under `.makina/`, adds a
logging subsystem, makes the scheduler continue past a failed task, and
polishes the TUI. See [SCOPE.md](SCOPE.md) for what is in and out, and
[ARCHITECTURE.md](ARCHITECTURE.md) for the design and file-level seams.

**Conventions**
- Each task has a stable kebab-case **id** (also used for its branch
  `task/{id}` and worktree `.makina/worktrees/{id}/`).
- **Depends on** lists *direct* structural prerequisites only; the Planner
  adds further dependency edges automatically for tasks that touch the same
  files or areas.
- **Done when** is the verifiable acceptance check used by gates and the
  Reviewer. Every task must also keep `cargo test`, `cargo clippy --all-targets
  -- -D warnings`, and `cargo fmt --check` green.
- Tasks are intentionally small and self-contained: each names the exact
  file(s) and function/type to touch and the approach, so it can be picked up
  without further context.

---

## 0012 — Unified `.makina/` Workspace & Run Identity

### mk-paths-module — Add the `.makina/` paths module
Create `crates/makina-core/src/paths.rs` with pure path-building helpers (no
I/O, no logic, no validation — each parameter is an opaque string). Pin the
signatures exactly: `config_file(repo_root: &Path) -> PathBuf` → `.makina/config.toml`;
`task_graph(repo_root: &Path, slug: &str) -> PathBuf` → `.makina/tasks/{slug}.json`;
`run_dir(repo_root: &Path, run_id: &str) -> PathBuf` → `.makina/runs/{run_id}`;
`audit_log(repo_root: &Path, run_id: &str) -> PathBuf` → `.makina/runs/{run_id}/audit.jsonl`;
`task_log(repo_root: &Path, run_id: &str, task_slug: &str) -> PathBuf` →
`.makina/runs/{run_id}/logs/{task_slug}.log`;
`worktree(repo_root: &Path, task_id: &str) -> PathBuf` → `.makina/worktrees/{task_id}`.
Mirror the existing helper `crates/makina-core/src/persist.rs:88-101` (`tasks_path`):
pure, no-I/O, `repo_root: &Path` first arg, returns `PathBuf`, and a rustdoc
`# Example` block whose hidden lines are `# use std::path::Path;` and
`# use makina_core::paths::<fn>;` (the second `# use` is required because
doctests run as external-crate code) followed by an `assert_eq!`. Register the
module by inserting `pub mod paths;` in `crates/makina-core/src/lib.rs` between
line 19 (`pub mod orchestrator;`) and line 20 (`pub mod persist;`) to keep the
alphabetical ordering.
- **Depends on:** —
- **Done when:** a `#[cfg(test)] mod tests` in `paths.rs` has one `assert_eq!`
  per helper using `Path::new("/repo")`: `config_file` → `/repo/.makina/config.toml`;
  `task_graph(.., "my-feature")` → `/repo/.makina/tasks/my-feature.json`;
  `run_dir(.., "01ABC")` → `/repo/.makina/runs/01ABC`;
  `audit_log(.., "01ABC")` → `/repo/.makina/runs/01ABC/audit.jsonl`;
  `task_log(.., "01ABC", "task-a")` → `/repo/.makina/runs/01ABC/logs/task-a.log`;
  `worktree(.., "task-a")` → `/repo/.makina/worktrees/task-a`. Both
  `cargo test -p makina-core paths` and `cargo test -p makina-core --doc paths`
  (the rustdoc examples) pass.

### mk-run-id — Persistent ULID run identity
Allocate a persistent, sortable run id (26-char ULID string) when a Run is
opened, and expose it read-only on the view. Use the **`ulid`** crate (not
`uuid`; the architecture's "uuid/ulid" slash is resolved to `ulid` here): add
`ulid = "1"` to `crates/makina-core/Cargo.toml` `[dependencies]` (it is absent
from `Cargo.lock` today), and generate the string with
`ulid::Ulid::new().to_string()`. Keep `RunId(u64)` (`api.rs:53`) as the
in-memory session handle. Work through these sites mechanically:
1. Add a `run_uid: String` field to `RunEntry` (**`orchestrator.rs:135-148`** —
   note `RunEntry` lives in `orchestrator.rs`, not `api.rs`).
2. In `open_run` (where `alloc_id` is called, `orchestrator.rs:454`) mint
   `let run_uid = ulid::Ulid::new().to_string();` and store it on the new
   `RunEntry` field at the insert site (`orchestrator.rs:461-469`).
3. Add `pub run_uid: String` to `RunView` (**`api.rs:223-238`**) and supply it
   in `build_view` (`orchestrator.rs:156-176`) — add it to `build_view`'s
   parameter list and read it from the `RunEntry` at the `run`/`runs` call sites.
4. Extend the `start_run` destructure tuple (the `(graph, cancel, pause,
   run_slug)` binding at `orchestrator.rs:547`, returned at `:579`) to also
   carry `run_uid`.
5. Add a `run_uid: String` param to `run_graph` (`supervisor.rs:750-758`, after
   `run_slug`) and pass it at the `run_graph` call in `orchestrator.rs:605-613`.
6. Add a `run_uid: String` field to `DriverContext` (next to `run_slug` at
   `supervisor.rs:448`) and populate it in BOTH constructors — the one inside
   `run_graph` (`supervisor.rs:793-806`) and `Supervisor::driver_context`
   (`supervisor.rs:696-709`; add the param to the method signature at
   `:672-678`; ask-path callers pass `String::new()` exactly as they do for
   `run_slug`).

Note: `supervisor.rs:~436` is the `DriverContext.audit_registry`/`run_slug`
**field** region; the `register()` **call** that consumes these is at
`supervisor.rs:1356` and is owned by `mk-audit-register-runuid` — this task only
threads `run_uid` into the `DriverContext` struct/constructors and does **not**
change `register()`. Follow `run_slug` end-to-end as the structural template and
add `run_uid` in lockstep beside it.
- **Depends on:** mk-paths-module
- **Done when:** a test `open_runs_carry_distinct_sortable_run_uids` in
  `orchestrator.rs` `mod tests` (modeled on `multiple_open_runs_get_distinct_ids`,
  `orchestrator.rs:1079-1108`) opens two runs via `Command::OpenRun`, snapshots
  `api.runs()`, and asserts each `RunView.run_uid` is 26 chars, the two differ,
  and the second sorts after the first; make the chronological ordering
  deterministic by using ulid's monotonic generator or inserting a small
  `std::time::Duration` gap between the two `open_run` calls so the timestamp
  prefix (not just the random tail) guarantees order. `RunId(u64)` still works as
  the handle. `cargo test -p makina-core open_runs_carry_distinct_sortable_run_uids`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` pass.

### mk-run-slug — Derive a collision-free, plan-scoped run slug
The slug is currently derived from `task_list_path.file_stem()` at **two**
independent sites: `open_run` (`orchestrator.rs:404-408`) and `start_run`
(`orchestrator.rs:571-576`, inside the runs-registry lock); `RunEntry`
(`orchestrator.rs:135-148`) stores no slug, so each site re-derives. If only
`open_run` is changed, the persisted artifact uses the new plan-scoped slug
while `start_run` still computes the old `TASKS` slug and threads it as
`run_slug` (returned at `:579`, passed into the scheduler at `:612`) — feeding
the audit ledger and per-task logs — so the two slugs diverge silently. Fix both
by extracting one pure free function `fn run_slug(task_list_path: &Path) ->
String` near `SLUG_FALLBACK` (`orchestrator.rs:97`, value `"task-list"`) and
calling it at both sites. The function computes
`format!("{parent_dir_name}-{file_stem}")`, lowercases it, then sanitizes to a
valid kebab id per `docs/spec/runtime-artifact-schema.md` §4.1 (lines 68-76):
lowercase; map every maximal run of non-`[a-z0-9]` chars to a single `-`; trim
leading/trailing `-`; and if the result is empty OR shorter than 2 chars (the
§4.1 minimum), return `SLUG_FALLBACK`. Fall back to the lowercased stem alone
when there is no usable parent directory (and to `SLUG_FALLBACK` if even that is
<2 chars), keeping `SLUG_FALLBACK` as the single terminal fallback. There is no
existing sanitizer to copy (none in `orchestrator.rs`/`persist.rs`/`interpret.rs`),
so write the char-class logic from scratch. This makes
`.makina/tasks/{slug}.json` (and the read-path's `load_graph(slug)`) unique per
plan, so opening different `TASKS.md` files no longer collide on the slug `TASKS`
and shadow each other's persisted graphs.
- **Depends on:** mk-task-graph-path
- **Done when:** a `#[test] fn run_slug_is_plan_scoped_and_valid()` in
  `orchestrator.rs` `mod tests` (anchor `orchestrator.rs:782`) asserts (1)
  `…/0003-Runtime-and-TUI-Hardening/TASKS.md` → `"0003-runtime-and-tui-hardening-tasks"`;
  (2) two different plan dirs each named `TASKS.md` yield **distinct** slugs;
  (3) a path with no usable parent falls back to the lowercased stem (or
  `SLUG_FALLBACK` if <2 chars); (4) the result satisfies the §4.1 kebab
  predicate (start/end alnum, no `--`, len ≥ 2). A
  `#[tokio::test] async fn open_run_uses_plan_scoped_slug()` (modeled on
  `open_run_seeds_artifact_before_start_run`, `orchestrator.rs:1179`, and
  `multiple_open_runs_get_distinct_ids`, `orchestrator.rs:1078`, using the
  `execution_core_api()` helper + a tempdir) writes a `TASKS.md` under a
  plan-style dir, `OpenRun`s it, and asserts the persisted artifact resolves via
  `persist::tasks_path` to the plan-scoped slug (so the assertion is
  location-agnostic and tracks the `.makina/tasks/` relocation) and that no
  unrelated `TASKS.json` is loaded. `cargo test -p makina-core run_slug`,
  `cargo test -p makina-core open_run`, clippy, and fmt pass.

### mk-config-path — Load project config from `.makina/config.toml` (+ legacy fallback)
In `config.rs` `load_defaults` (`config.rs:563`, currently arg-less; the
CWD-relative `let project_path = Some(std::path::PathBuf::from("makina.toml"));`
is at `config.rs:565`), keep the function arg-less and derive `repo_root` inside
it via `let repo_root = std::env::current_dir().unwrap_or_else(|_|
std::path::PathBuf::from("."));` — mirroring `crates/makina/src/main.rs:68` — so
the sole caller at `main.rs:54` needs no change. Replace the `project_path`
computation with explicit path-selection done by the caller (because
`Config::load`, `config.rs:511`, silently treats a missing project path as
`Default` and cannot itself express "try A else B"): compute
`let primary = paths::config_file(&repo_root);` and
`let legacy = repo_root.join("makina.toml");`, then
`let project_path = if primary.exists() { Some(primary) } else if legacy.exists()
{ tracing::warn!(path = %legacy.display(), "loading deprecated ./makina.toml;
move it to .makina/config.toml"); Some(legacy) } else { Some(primary) };`, and
pass `project_path.as_deref()` to `Config::load`. Add `use tracing::warn;` (or
call `tracing::warn!` fully-qualified) — `config.rs` has no `tracing` import
today, but `tracing` is a workspace dep of `makina-core`. Update the
`load_defaults` doc-comment (`config.rs:550-558`) so it states the new
precedence: `.makina/config.toml` (preferred); legacy `./makina.toml`
(deprecated, warns). To make the test deterministic without the flaky
`set_current_dir` global state, extract a small private helper
`fn resolve_project_config_path(repo_root: &Path) -> (Option<PathBuf>, bool)`
(the `bool` is "legacy was chosen") and have `load_defaults` use it.
- **Depends on:** mk-paths-module
- **Done when:** a unit test `resolve_project_config_path_prefers_makina_dir` in
  `config.rs`'s `#[cfg(test)]` module asserts a `tempfile::tempdir()` repo
  containing `.makina/config.toml` resolves to that path with `legacy == false`;
  a unit test `resolve_project_config_path_falls_back_to_legacy` asserts a temp
  repo containing only `./makina.toml` resolves to the legacy path with
  `legacy == true` (exercising the warn branch). Use the temp-repo pattern from
  `crates/makina-core/tests/worktree.rs:40` (`setup_temp_repo`).
  `cargo test -p makina-core config` and clippy pass.

### mk-task-graph-path — Route persistence paths through the paths module
In `persist.rs`, relocate the three `.tasks` join sites to `.makina/tasks/`,
delegating to `paths::task_graph` where possible. Behavior is otherwise unchanged
(atomic temp-then-rename). Exact edits: (1) `tasks_path` body
(`persist.rs:99-101`) → `paths::task_graph(repo_root, slug)`; (2) the tasks-dir
construction in `persist_graph` (`persist.rs:151`, `let tasks_dir =
repo_root.join(".tasks")`) → derive from paths, e.g.
`let tasks_dir = repo_root.join(".makina").join("tasks");` (mk-paths-module
provides only `task_graph(repo_root, slug)` and **no** temp/dir helper, so build
the directory explicitly here or via `paths::task_graph(repo_root,
&graph.slug).parent()`); (3) `temp_path` body (`persist.rs:112-119`) → place the
temp file under the same `.makina/tasks/` directory
(`repo_root.join(".makina").join("tasks").join(format!(".{slug}.json.tmp.{pid}.{seq}"))`)
so the same-filesystem-rename invariant still holds. `tasks_path` is the single
public entry point — integration tests (`tests/orchestrator_read_path.rs`,
`tests/supervisor_write_path.rs`) and `orchestrator.rs:1210` all resolve the
artifact path via it, so repointing the helper propagates automatically with no
caller edits. Update the runnable doctest on `tasks_path` (`persist.rs:93-97`)
from `/repo/.tasks/my-feature.json` to `/repo/.makina/tasks/my-feature.json`
(`cargo test` runs doctests, so this is required). Do **not** touch the
gitignore-invariant test `gitignore_worktrees_ignored_tasks_not_ignored`
(`persist.rs:530-563`) — its update is owned by `mk-gitignore`. Defer the
module/function prose doc-comments that still say `.tasks/` (e.g.
`persist.rs:4,16,27,39,123-129,194`) to `mk-doc-refs`.
- **Depends on:** mk-paths-module
- **Done when:** the in-file unit test `tasks_path_constructs_correct_path`
  (`persist.rs:510-518`) asserts `{repo}/.makina/tasks/{slug}.json` (update its
  literal to `PathBuf::from("/some/repo/.makina/tasks/plan-0002.json")`) and the
  atomicity test's glob (`persist.rs:485-486`) reads
  `root.join(".makina").join("tasks")` and finds the artifact with no `.tmp`
  residue; `cargo test -p makina-core persist` (including doctests) and
  `cargo clippy -p makina-core` pass.

### mk-worktree-path — Route worktree paths through the paths module
In `worktree.rs`, change `WorktreeManager::worktree_path` (`worktree.rs:313`,
currently `self.repo_root.join(".worktrees").join(task_id)`) to
`paths::worktree(&self.repo_root, task_id)` so worktrees live at
`.makina/worktrees/{task-id}/`. The signature is unchanged
(`fn worktree_path(&self, task_id: &str) -> PathBuf`); `create`
(`worktree.rs:182`) and `remove` (`worktree.rs:258`) are the only callers and
need no change. Add `use crate::paths;` at the top of `worktree.rs` (the module
has no `paths` import today). `mk-paths-module` exposes
`paths::worktree(repo_root: &Path, task_id: &str) -> PathBuf`. Because several
tests independently recompute `.worktrees`, they must be updated **in this PR**
so the broad `cargo test` stays green: (a) the in-module unit test
`worktree_path_is_under_repo_root` (`worktree.rs:490-495`) →
`PathBuf::from("/repo/.makina/worktrees/my-task")`; (b) the integration test
`tests/worktree.rs:134-137` → `repo_root.join(".makina").join("worktrees")
.join("sample-task")` (and fix the doc-comment at `tests/worktree.rs:111/115`);
(c) `tests/supervisor_audit_registry.rs:228` (`expected_working_dir`); (d)
`tests/termination_caps.rs:205` (the `assert_worktree_gone` helper, also
covering `:307/:438`); (e) `tests/develop_review_loop.rs:210/381/458-459`; (f)
`tests/squash_merge.rs:484` — all from `.join(".worktrees")` to
`.join(".makina").join("worktrees")` (preferably by calling
`paths::worktree(repo_root, id)` so they cannot drift again). Leave the
`worktree.rs` module doc-comments (lines 5, 26) to `mk-doc-refs`. Keep this as
one PR: splitting the production change from its test updates would leave `main`
red.
- **Depends on:** mk-paths-module
- **Done when:** `cargo test -p makina-core worktree` passes with the worktree
  created/torn down under `.makina/worktrees/{task-id}/` (unit test asserts
  `/repo/.makina/worktrees/my-task`; the integration test asserts
  `handle.path == repo_root.join(".makina").join("worktrees").join("sample-task")`
  and that the checkout exists on disk under that path before `remove` and is
  gone after); `cargo test -p makina-core` is green workspace-wide (the
  audit-registry `working_dir`, termination_caps, develop_review_loop, and
  squash_merge worktree-path assertions are updated); clippy and fmt are clean.

### mk-audit-register-runuid — Thread `run_uid` through the AuditRegistry seam
Pure signature/threading change with **no** path relocation (that is
`mk-audit-relocate-path`). In `audit.rs`: add `run_uid: String` to
`AuditContext` (`audit.rs:46`); add `run_uid: String` as the **2nd** param of
the `AuditRegistry::register` trait method (`audit.rs:70`, new signature
`register(&self, working_dir: PathBuf, run_uid: String, run_id: String, slug:
String, task_id: String)`); update both production impls —
`NoopAuditRegistry::register` (`audit.rs:83`) and `JsonlAuditSink::register`
(`audit.rs:142`) to accept and store it; `record()` keeps writing to the
**old** `.tasks/{slug}/audit.jsonl` path for now (the entry still enriches
`run_id`/`slug`/`task_id`). In `supervisor.rs`: add a `run_uid: String` field to
`DriverContext` (next to `run_slug`, `supervisor.rs:448`), its constructor
(`supervisor.rs:676-708`), and the `run_graph` signature (`supervisor.rs:750`,
after `run_slug`); pass `ctx.run_uid.clone()` as the new 2nd arg at the
`register` call site (`supervisor.rs:1356`). In `orchestrator.rs`: pull the
`run_uid` from `RunEntry` (added by `mk-run-id`) and thread it through
`start_run` (`orchestrator.rs:547/579`) into the `run_graph` call
(`orchestrator.rs:605-613`). Update the test spy in
`crates/makina-core/tests/supervisor_audit_registry.rs`: add `run_uid` to
`RegisterCall` (line 41), to the `SpyAuditRegistry::register` impl (line 71),
and to the assertion block (lines 218-248); add the new arg to the `run_graph(…)`
call (lines 199-209). Also fix the in-crate `run_graph` test harness in
`supervisor.rs` to pass a known `run_uid`.
- **Depends on:** mk-paths-module, mk-run-id
- **Done when:** `cargo test -p makina-core` compiles all `run_graph` callers +
  the updated spy and passes; `supervisor_audit_registry.rs` asserts the spy
  captured the new `run_uid` arg alongside `run_id`/`slug`/`task_id`;
  `cargo clippy -p makina-core` is clean.

### mk-audit-relocate-path — Relocate the audit ledger to `.makina/runs/{run_uid}/audit.jsonl`
Pure I/O relocation in `JsonlAuditSink::record` (`audit.rs`) only. The
`register` trait/`AuditContext`/threading work is already done by
`mk-audit-register-runuid`; here, change the dir computation (`audit.rs:224`,
`self.repo_root.join(".tasks").join(&ctx.slug)`) and the file join
(`audit.rs:239`, `dir.join("audit.jsonl")`) to
`paths::audit_log(&self.repo_root, &ctx.run_uid)` (which already returns
`.makina/runs/{run_uid}/audit.jsonl`), and create the parent dir via
`create_dir_all` of that path's parent. Update the existing unit test
`jsonl_audit_sink_enriches_and_appends` (`audit.rs:304`): pass a known `run_uid`
to `register`, then assert the file lands at
`repo_root.join(".makina").join("runs").join(<run_uid>).join("audit.jsonl")`
(replacing the old `.tasks/my-slug/audit.jsonl` assertion at `audit.rs:326`),
keeping the second-record append-to-2-lines assertion (`audit.rs:358-366`); fix
`unregistered_working_dir_is_silently_discarded` (`audit.rs:377`) to check that
`.makina/runs/` is empty/absent. Defer the module/function prose doc-comments
(e.g. `audit.rs:5-6,20,32,88-89,108,118,174`) that say `.tasks/{slug}/audit.jsonl`
to `mk-doc-refs`. The end-to-end check is
`interleaved_permission_request_completes_turn_and_records_audit_entry`
(`crates/makina/tests/e2e.rs`) but that requires a real ACP backend (heavy,
gated) — the unit + spy tests are the gating verification, not the e2e.
- **Depends on:** mk-audit-register-runuid
- **Done when:** `cargo test -p makina-core audit` passes;
  `jsonl_audit_sink_enriches_and_appends` asserts the enriched line lands at
  `.makina/runs/{run_uid}/audit.jsonl` and that a second record appends a 2nd
  line; `cargo test -p makina-core` (which includes
  `tests/supervisor_audit_registry.rs`) compiles and passes; `cargo clippy -p
  makina-core` is clean.

### mk-gitignore — Commit/ignore split for `.makina/` + migrate the repo's own config
Add the commit/ignore split and migrate this repo's own config. In the **root**
`.gitignore` (`/Users/koraytaylan/Workspace/makina/.gitignore`, currently 8
lines), delete lines 5-8 as a block — the 3-line worktree/`.tasks` comment
(lines 5-7) plus the active `/.worktrees/` rule (line 8) — and do **not** add
any `.makina/` rule (it stays tracked, since config + tasks are committed).
Create `/Users/koraytaylan/Workspace/makina/.makina/.gitignore` containing
exactly two lines: `/runs/` and `/worktrees/`. Migrate the repo's own
`makina.toml` → `.makina/config.toml` (e.g. `git mv makina.toml
.makina/config.toml`); the config still loads post-move because `mk-config-path`
added the legacy fallback. Also `git mv .tasks/TASKS.json
.makina/tasks/TASKS.json` and remove the now-stale on-disk `.tasks/` and
`.worktrees/` dirs from the working tree so no stale committed artifact remains.
Update the gitignore-invariant test `gitignore_worktrees_ignored_tasks_not_ignored`
(`crates/makina-core/src/persist.rs:531`, Test 6 block ~`persist.rs:520`): keep
it **string-level** (do NOT spawn `git check-ignore`, per the test's own
rationale at `persist.rs:525-529` — it must work in CI sandboxes without a full
git context) — read `.makina/.gitignore` and assert it contains the rules
`/runs/` and `/worktrees/`, assert the root `.gitignore` no longer contains
`/.worktrees/` and does not ignore `.makina/`. Update the `persist.rs`
module doc-comment at lines 39-41 to describe the new `.makina/` split (config +
tasks committed; runs + worktrees gitignored via `.makina/.gitignore`), OR defer
that prose to `mk-doc-refs` — but do not leave it contradicting the new layout.
- **Depends on:** mk-config-path, mk-worktree-path, mk-audit-relocate-path
- **Done when:** `cargo test -p makina-core gitignore` passes (the string-level
  invariant test asserts `.makina/.gitignore` ignores `runs/`/`worktrees/` while
  the root `.gitignore` no longer ignores `.worktrees/` or `.makina/`); the
  repo's config now lives at `.makina/config.toml` and the stale
  `.tasks/TASKS.json` is removed. As a **manual** acceptance note (not an
  automated gate, to preserve the CI-sandbox guarantee): `git check-ignore
  .makina/runs/x .makina/worktrees/x` exits 0 while `git check-ignore
  .makina/config.toml .makina/tasks/x.json` exits 1.

### mk-doc-refs — Update docs/comment references to the new layout
Documentation-only. Update the old path literals (`makina.toml`, `.tasks/`,
`.worktrees/`) to the new `.makina/` layout in three named prose files, and
nothing more. Exact lines: `docs/spec/runtime-artifact-schema.md:1` (the title
`# Runtime Artifact Schema — \`.tasks/{slug}.json\``) and `:20` (the §2 Path
table cell) → `.makina/tasks/{slug}.json` (leave §4.1 "Task ID Rules" at line 68
untouched — it is the kebab spec, unaffected by relocation); `README.md:67`
(`makina.toml` → `.makina/config.toml`), `:128` (`.worktrees/{id}/` →
`.makina/worktrees/{id}/`), `:150` (`.tasks/{slug}.json` →
`.makina/tasks/{slug}.json`); `docs/trial/e2e-run.md:23/56/108/117/219/230` (NB:
`:165` is `~/.makina/config.toml`, the **global** config — already correct,
leave it). Do **NOT** edit historical archives — leave
`docs/plans/0001-Initial/*`, `docs/plans/0002-Governance-and-Persistence/*`, and
`docs/trial/trial-findings.md` unchanged (they record past designs). Do **NOT**
edit opaque test-fixture path strings like `PathBuf::from(".tasks/foo.json")` in
`crates/makina/src/{ui.rs,event.rs,app.rs,placeholder.rs}` — those are arbitrary
fixture paths, not layout references. In-function layout doc-comments are owned
by the relocate tasks that rewrite those functions: `config.rs` by
`mk-config-path`, `persist.rs` by `mk-task-graph-path` + `mk-gitignore`,
`worktree.rs` by `mk-worktree-path`, `audit.rs` by `mk-audit-relocate-path`; this
task covers only the three prose files plus any stray doc-comment **not** inside
those four modules. (The architecture's "~120 references" recon at
`ARCHITECTURE.md:38-40` is the upstream relocate tasks' churn, not this task's
scope.)
- **Depends on:** mk-config-path, mk-task-graph-path, mk-worktree-path, mk-audit-relocate-path, mk-gitignore
- **Done when:** `grep -nE "makina\.toml|\.tasks/|\.worktrees/" README.md
  docs/trial/e2e-run.md docs/spec/runtime-artifact-schema.md` returns only fenced
  historical/"was …" notes (zero live old-layout references); and
  `cargo doc -p makina-core -p makina` builds clean.

---

## 0013 — Logging & Diagnostics

### log-run-dir — Create the per-run log directory on StartRun
Add `pub fn run_logs_dir(repo_root: &Path, run_id: &str) -> std::io::Result<PathBuf>` to
`crates/makina-core/src/paths.rs` (the file created by `mk-paths-module`). Implement it as
`let dir = paths::run_dir(repo_root, run_id).join("logs"); std::fs::create_dir_all(&dir)?; Ok(dir)`.
Note this is the one **I/O** helper in an otherwise-pure module: `mk-paths-module` defines
`run_dir`/`audit_log`/`task_log` as pure no-I/O path builders, so keep `run_logs_dir` clearly
separate (it creates the directory) even though it lives in the same file. Wire it into
`start_run` (`orchestrator.rs:544`, which is **synchronous**): inside the existing run-state lock
block (`:547-580`) also pull `entry.run_uid.clone()` (the ULID added by `mk-run-id`) into the
returned tuple; then after the guard is dropped (`:580`) and **before** the background scheduler
`tokio::spawn` (`:604`), call `paths::run_logs_dir(&self.state.worktree_manager.repo_root,
&run_uid)` (reach `repo_root` via `WorktreeManager.repo_root`, `worktree.rs:133`). Because
`start_run` is sync, use `std::fs::create_dir_all`, **not** `tokio::fs`. Best-effort: on `Err`,
`tracing::warn!(run_uid=%run_uid, error=%e, "failed to create per-run logs dir; continuing")` and
do **not** return early — never abort the run. Mirror the best-effort dir-create+warn pattern at
`audit.rs:224-233`.
- **Depends on:** mk-paths-module, mk-run-id
- **Done when:** a unit test `run_logs_dir_creates_and_is_idempotent` in `paths.rs` (using
  `tempfile::tempdir()` per `persist.rs:359`) asserts the returned path ends with
  `.makina/runs/{run_id}/logs`, that `dir.is_dir()` is true after the call, and that a second call
  also returns `Ok` (idempotent); `cargo test -p makina-core paths` passes. A wiring assertion is
  covered by `log-subscriber-file`'s Done-when (which drives a run and writes under the per-run
  logs dir), so the helper cannot ship unwired.

### log-subscriber-file — Install the per-run file layer of the tracing subscriber
Add the tracing-subscriber dependencies to `crates/makina/Cargo.toml`: `tracing = { workspace =
true }`, `tracing-subscriber = { version = "0.3", features = ["registry", "env-filter"] }`, and
(recommended) `tracing-appender = "0.2"` for non-blocking file writes; add the new versions to the
root `[workspace.dependencies]` next to `tracing = "0.1"` (Cargo.toml:52) and reference via
`workspace = true`, matching the existing convention. In `crates/makina/src/main.rs`, build a
`tracing_subscriber::registry()` and `.init()` it **after** the audit-sink setup (main.rs:78) and
**before** the event loop (`event::run`, main.rs:122). The central design constraint: the
subscriber is installed once at startup, but run ids are allocated **lazily** per `OpenRun`
(`next_id: AtomicU64`, orchestrator.rs:208; fetched at :225) and multiple runs can be open at once,
so the file layer **cannot** use a static path. Implement it as a custom `impl
tracing_subscriber::Layer<S>` (or a span-keyed `MakeWriter`) that, on each event, reads the current
span's `run_uid` field and appends to the per-run logs dir resolved via `paths::run_dir` /
`run_logs_dir` (`log-run-dir`). Add a `tracing::info_span!(run_uid = %run_uid)` at the run-graph
entry (the scheduler `tokio::spawn` at orchestrator.rs:604 / `run_graph`) so events carry the key.
Mirror the resolve-path-then-append pattern in `JsonlAuditSink::record` (audit.rs:177-236) and the
per-task register precedent (`ctx.audit_registry.register(...)`, supervisor.rs:1356-1361) — there
is no existing `tracing_subscriber`/span usage in the repo, so this is the first one. Document the
MVP scope: support concurrent runs via span-keyed routing, or restrict to one active run and note
the limitation.
- **Depends on:** mk-run-id, mk-paths-module, log-run-dir
- **Done when:** `crates/makina/tests/log_subscriber_file.rs` has `writes_warn_to_per_run_log`:
  install the file layer with `tracing::subscriber::with_default` over a `tempfile::tempdir()`,
  enter a span carrying a sample `run_uid`, emit `tracing::warn!("probe")`, flush, and assert the
  file under the resolved `.makina/runs/{run_uid}/logs/` contains `"probe"`. `cargo test -p makina
  log_subscriber_file` + `cargo clippy -p makina --all-targets -- -D warnings` + `cargo fmt
  --check` pass.

### log-subscriber-tui-channel — Add the tracing→mpsc TUI channel layer and compose the subscriber
Define a self-contained `LogRecord { timestamp, level: tracing::Level, message: String, target:
String }` (in `makina-core`, or reuse `tui-error-pane-state`'s `ErrorMessage` if that task lands
first and `plan-0015` maps it). Implement a second `impl tracing_subscriber::Layer<S>` that
converts each event into that record and `try_send`s it onto a **bounded** `tokio::sync::mpsc`
channel (capacity `256`); on `TrySendError::Full`, drop the record (guard against re-entrancy — do
not re-`tracing::warn!` from inside the layer). In `crates/makina/src/main.rs` compose
`tracing_subscriber::registry().with(file_layer).with(tui_layer).init()` (the two layers via
`SubscriberExt`/`SubscriberInitExt`) and hold the `mpsc::Receiver`, threading it into
`App::new`/`event::run` (event.rs already uses `tokio::sync::mpsc` at :38/:71) so `plan-0015`'s
`tui-error-pane-channel-wire` can drain it in the `tokio::select!`. `tokio`'s `sync` feature is already
available via the workspace, so no extra dep is needed beyond what `log-subscriber-file` added.
Existing `tracing::warn!`/`error!` calls now reach both the file and the channel.
- **Depends on:** log-subscriber-file
- **Done when:** `crates/makina/tests/log_subscriber_channel.rs` has
  `composes_two_layers_without_panic` (build the registry with both layers + `.try_init()` → `Ok` /
  no panic) and `forwards_warn_to_channel` (install the tui layer, emit `tracing::warn!("probe")`,
  assert `rx.try_recv()` yields a record whose message contains `"probe"`). `cargo test -p makina
  log_subscriber_channel` + clippy + fmt pass.

### log-tracing-transition-events — Emit tracing events at task state-transition and gate-output sites
The per-task log content (state transitions, gate output) is **not** emitted as tracing events
today: `supervisor.rs` has exactly one `tracing::warn!` (at `:501`, the persist-failure path), and
state changes flow through `apply_event_locked` + the `EventSink` (`DriverContext::emit_task_state`,
supervisor.rs:458), not tracing. Add structured `tracing::info!` emissions at the sites that already
mutate state / produce gate output, so a per-task subscriber has something to capture. Concretely:
alongside the `emit_task_state` calls in `task_driver` (the InProgress / InReview / terminal-Failed
transition sites) emit `tracing::info!(task = %task_id.0, from = ?prev, to = ?new, "task state
transition")`; in `develop_until_gates_pass` (supervisor.rs:1621) emit `tracing::info!(task =
%task_id.0, gate = %gate, exit_code, "gate failed")` on the `GateOutcome::Failed` arm (and a Passed
counterpart). Use a consistent field convention (`task` = the task id). These are **additive** — do
not change `EventSink` behavior.
- **Depends on:** —
- **Done when:** an integration test installs a capturing `tracing` collector (e.g. `tracing-test`
  or a small custom collector) over a single `run_graph` task with `NoopBackend` in a `tempdir`
  (mirror `crates/makina-core/tests/supervisor_audit_registry.rs`) and asserts the captured events
  include the task's state transitions and at least one gate-output record; `cargo test -p
  makina-core` + clippy + fmt pass.

### log-per-task-routing-layer — Add a span-field-aware file Layer that fans out per-task log writers
Span-field-to-distinct-file routing is **not** a built-in `tracing_subscriber` feature — it needs a
custom Layer. Add an `impl tracing_subscriber::Layer<S>` (in the file-layer module created by
`log-subscriber-file`, or a new module) that, on each event, walks the current span scope, reads the
`task_slug` field set by the per-task span (via `on_new_span` + a `Visit`or, stored in
`span.extensions()`), and appends the formatted record to `paths::task_log(repo_root, run_uid,
task_slug)` (the helper from `mk-paths-module`). Open/cache one writer per `task_slug`. Records with
no task span fall back to the per-run log (the `log-subscriber-file` behavior). Mirror the
resolve-then-append pattern in `JsonlAuditSink::record` (audit.rs:177-236), driven by a span field
instead of `working_dir`.
- **Depends on:** log-subscriber-file
- **Done when:** a unit test `per_task_routing` feeds two events under two different `task_slug`
  spans through the layer and asserts each lands in its own `{task_slug}.log`, while a span-less
  event goes to the run-level log; `cargo test -p makina per_task_routing` + clippy pass.

### log-per-task-files — Tag each task driver with a task_slug span so its records route to its log file
Attach a per-task span to the spawned driver future so its records route to
`.makina/runs/{run_id}/logs/{task_slug}.log` via `log-per-task-routing-layer`. In `supervisor.rs`,
at the `JoinSet` spawn (supervisor.rs:1008, where `driver_id` is in scope), wrap the future:
`task_driver(&driver_ctx, &driver_id).instrument(tracing::info_span!("task", task_slug =
%driver_id.0))` (add `use tracing::Instrument;`). Use `.instrument(future)` — **not** `.in_scope()`
— because `task_driver` (supervisor.rs:1283) `.await`s across the `JoinSet`, so the span must
persist across await points. The span's `task_slug` field is what `log-per-task-routing-layer` keys
on; the transition/gate records come from `log-tracing-transition-events`. There is no existing
`.instrument`/span usage anywhere in the repo (verified), so introduce it here.
- **Depends on:** log-per-task-routing-layer, log-tracing-transition-events
- **Done when:** a new integration test `crates/makina-core/tests/per_task_logs.rs` (mirroring
  `tests/supervisor_audit_registry.rs`: drive one task via `run_graph` + `NoopBackend` in a
  `tempdir`, with the two-layer subscriber installed) asserts `paths::task_log(repo_root, run_uid,
  task_slug)` exists and contains the task's state transitions (and a gate-output line where gates
  run); `cargo test -p makina-core per_task_logs` + clippy + fmt pass.

### log-run-metadata-type — Add the RunMetadata type + best-effort writer
Add a new file `crates/makina-core/src/run_metadata.rs` and register `pub mod run_metadata;` in
`lib.rs` (after `pub mod persist;`). Define
`#[derive(Debug, Clone, Serialize, Deserialize)] pub struct RunMetadata { run_uid: String,
run_slug: String, status: RunStatus, started_at: DateTime<Utc>, ended_at: DateTime<Utc> }` (no
task→worktree map in this sub-task: `Task` has no worktree field, the orchestrator stores no
task→worktree map, and `WorktreeManager::worktree_path` is private and still returns the
pre-relocation path — there is no usable in-graph source). `RunStatus` lives in `api.rs:197` and
already derives `Serialize`. Add `pub async fn write_run_metadata(meta: &RunMetadata, repo_root:
&Path) -> std::io::Result<()>` that serializes via `serde_json::to_string_pretty` and writes to
`paths::run_dir(repo_root, &meta.run_uid).join("run.json")`, creating the dir with
`tokio::fs::create_dir_all` — mirror `persist::persist_graph` (persist.rs:150-186). `chrono` (with
`serde`) and `serde_json` are already deps of `makina-core` (Cargo.toml:25,12-13).
- **Depends on:** mk-paths-module, mk-run-id
- **Done when:** a unit test in `run_metadata.rs` round-trips a `RunMetadata` through
  `write_run_metadata` + `serde_json::from_str` and asserts `run_uid`/`run_slug`/`status`/timestamps
  survive and that the file lands at `.makina/runs/{run_uid}/run.json`; `cargo test -p makina-core
  run_metadata` passes.

### log-run-metadata-wire — Persist run identity on RunEntry and write run.json at finalization
Make `run_uid`, `run_slug`, and a start timestamp reachable at run finalization, then call the
writer. In `orchestrator.rs`: add `run_slug: String` and `started_at: Option<DateTime<Utc>>` to
`RunEntry` (orchestrator.rs:135) — `run_uid: String` is added by `mk-run-id`. Set `run_slug` +
`run_uid` in `open_run`'s `RunEntry` insert; set `started_at = Some(chrono::Utc::now())` in
`start_run` where the status is set to `Running` (orchestrator.rs:567). In `finalize_run_status`
(orchestrator.rs:251), after deriving the terminal status (~:283), build a `RunMetadata` from the
entry's `run_uid`/`run_slug`/`started_at` plus `ended_at = chrono::Utc::now()` and call
`write_run_metadata` **best-effort** — mirror the seed-persist warn-only pattern at
orchestrator.rs:522-528: `if let Err(e) = write_run_metadata(...).await { tracing::warn!(run_uid=%uid,
error=%e, "run.json write failed"); }`. Never propagate or abort the run.
- **Depends on:** log-run-metadata-type, mk-run-slug, log-run-dir
- **Done when:** an integration test `run_metadata_terminal` in
  `crates/makina-core/tests/run_metadata.rs` (mirror `build_api` in
  `tests/orchestrator_read_path.rs:83`) issues `OpenRun` + `StartRun`, waits for
  `RunStatus::Completed` via subscribe, then asserts `.makina/runs/{run_uid}/run.json` parses to a
  `RunMetadata` with the `run_uid`, `run_slug`, a terminal status, and `started_at <= ended_at`;
  `cargo test -p makina-core run_metadata_terminal` passes.

### audit-async-write — Offload the audit sink's file I/O to a background writer
Refactor `JsonlAuditSink` so the sync `record` (audit.rs:177) no longer does blocking `std::fs` on
the caller's (ACP reader) thread. In `JsonlAuditSink::new` (audit.rs:121, **sync**) create a
`tokio::sync::mpsc::channel::<(PathBuf, String)>(AUDIT_QUEUE_CAP)` (name a constant, e.g. `const
AUDIT_QUEUE_CAP: usize = 1024;`), store the `Sender`, and spawn the writer via
`tokio::runtime::Handle::try_current()` — mirror the precedent at supervisor.rs:1220 (`if let Ok(h)
= Handle::try_current() { h.spawn(writer_loop(rx)); }`). When no runtime is current (unit tests),
fall back to the existing synchronous write path so behavior is unchanged. In `record`, resolve the
destination per message — `let path = paths::audit_log(&self.repo_root, &ctx.run_uid);` (the
`paths::audit_log` helper from `mk-paths-module`; the `run_uid` field on `AuditContext` from
`mk-audit-register-runuid`) — and `try_send((path, line))`; on `TrySendError::Full`, `tracing::warn!(...
"audit writer queue full; dropping entry")` and drop. Use `try_send` (non-blocking) only — never
`send().await` or `blocking_send()` — to keep the sync, must-return-quickly trait contract
(governance.rs:100-104). The writer loop does `while let Some((path, line)) = rx.recv().await { ...
create_dir_all(path.parent()), open create+append, writeln!(line) }`, moving the blocking block
currently at audit.rs:224-262 off the caller thread; re-open per message because paths differ per
run. Keep the `AuditSink` trait sync + unchanged. `tokio`'s `sync` feature is already enabled
workspace-wide (Cargo.toml:13) — no Cargo edit is needed. Convert the three existing audit.rs unit
tests (audit.rs:303/:376/:398) to `#[tokio::test]` and add a drain step before file assertions
(drop the sink to close the sender, then join/await the writer, or expose a test-only `flush().await`
seam); preserve their existing assertions (enrichment, append-not-truncate).
- **Depends on:** mk-audit-relocate-path
- **Done when:** `#[tokio::test] async fn record_enqueues_without_blocking_and_writer_flushes_in_order`
  in `audit.rs`: spam N=1000 `record` calls and assert each returns promptly (does not block on
  I/O), then flush the writer and assert the audit file under `.makina/runs/{run_uid}/audit.jsonl`
  contains exactly N lines in enqueue order; `cargo test -p makina-core audit` + clippy + fmt pass.

### audit-registry-evict — Evict registry entries when a run completes
Add `fn evict_run(&self, run_id: &str)` to the `AuditRegistry` trait (audit.rs:64) with a **default
no-op body** `{}` on the trait, so `NoopAuditRegistry` (audit.rs:82) and the test `SpyAuditRegistry`
(`tests/supervisor_audit_registry.rs:70`) keep compiling. Implement it on `JsonlAuditSink`: lock
`self.registry` and `map.retain(|_, ctx| ctx.run_id != run_id)`. Note the map is keyed by
`working_dir` (PathBuf), **not** by run id — `run_id` is a value field on `AuditContext`
(audit.rs:46), so you must match on the `AuditContext.run_id` **value**, not the key. On a poisoned
mutex, `tracing::warn!` and return (mirror `register`'s poison handling). Update the stale
"Registry entries are never evicted" doc block (audit.rs:100-105) to describe the new
eviction-on-terminal behavior. Call it from `CoreState::finalize_run_status` (orchestrator.rs:251),
after the terminal status is derived (~:283) and **after** the `run.json` writer from
`log-run-metadata-wire` runs: `self.audit_registry.evict_run(&run.to_string());` — `run` is a
`RunId`, and the stored ids are the `"run:{n}"` form (`RunId` Display, api.rs:57), so pass
`&run.to_string()`, **not** `&run.0.to_string()`. This needs no new deps.
- **Depends on:** mk-audit-relocate-path, log-run-metadata-wire
- **Done when:** a unit test `evict_run_removes_only_that_runs_entries` in audit.rs's
  `#[cfg(test)] mod tests` (modeled on `jsonl_audit_sink_enriches_and_appends`, audit.rs:304):
  register two working_dirs under run id `"run:1"` and one under `"run:2"`, call
  `sink.evict_run("run:1")`, and assert (via the in-module-visible private `self.registry`) that the
  map retains only the `"run:2"` entry; `cargo test -p makina-core audit` passes. (Optional
  integration leg: extend `SpyAuditRegistry` with an `evicted: Arc<Mutex<Vec<String>>>` + an
  `evict_run` impl, drive a run to terminal through the CoreApi, and assert `evict_run("run:{n}")`
  fired once after completion — cite `tests/supervisor_audit_registry.rs` as the precedent.)

---

## 0014 — Scheduler Robustness

### fsm-skipped-state — Add the `Skipped` terminal state + `DependencyFailed` event
Add a `Skipped` terminal state and a `DependencyFailed` event to the task FSM, then fix the **cross-crate compile-breaking ripple** the new variant produces. In `crates/makina-core/src/task.rs`, add `TaskState::Skipped` to the enum at `task.rs:82-105` (after `Failed`; note the enum carries `#[serde(rename_all = "kebab-case")]` at `:81`, which serializes the single word to `"skipped"`). In `crates/makina-core/src/state_machine.rs`, add `TaskEvent::DependencyFailed` to `TaskEvent` (alongside `MergeConflict` ~`:135`); add the four arms `(New | Ready | InProgress | InReview, DependencyFailed) => Ok(Skipped)` to `transition()` **before** the `_ => Err(IllegalTransition { .. })` wildcard at `state_machine.rs:194/:223`; add `Skipped` to `is_terminal()` at `:233` (`matches!(state, Done | Failed | Skipped)`); and add `DependencyFailed` to the four non-terminal arms of `legal_events()` at `:241`. Keep the FSM total — `DependencyFailed` must be illegal from `Done/Failed/Skipped`.

Because both `TaskState` enums are matched **exhaustively (no wildcard) in several places**, adding the variant breaks compilation until each is updated — do these by hand, there is no shortcut: (1) `persist.rs:262-267` `recover_for_resume` matches every variant — add `Skipped` to the no-op `New | Ready | Done | Failed => {}` arm (resume should leave a skipped task untouched). (2) The **view** enum `api::TaskState` at `api.rs:120-133` (note: `#[serde(rename_all = "snake_case")]` at `:119` — a *different* serde convention from the domain enum, fine for `Skipped` since both render `"skipped"`) is its own six-variant enum — add a `Skipped` variant; then add `Domain::Skipped => TaskState::Skipped` to the exhaustive `From<crate::task::TaskState>` impl at `api.rs:141-152`. (3) In the **separate `makina` crate**, `ui.rs:674-683` `task_state_badge(&makina_core::api::TaskState)` matches all six view variants exhaustively — add `TaskState::Skipped => ("[⊘ skipped]", Color::DarkGray)`. (`app.rs` uses only `==`/`matches!`, so it is unaffected.)

Mirror the plan-0002 `MergeConflict` **event** addition (event ~`state_machine.rs:135`, transition arm `:218`, legal_events `:259`, dedicated tests `:600-623`) for the `DependencyFailed` event half. Note there is **no existing precedent for adding a new `TaskState` VARIANT** — the cross-crate ripple above is the new work and must be done explicitly. In `state_machine.rs`'s tests module add, mirroring the existing cap tests: `each_active_state_plus_dependency_failed_yields_skipped` (loop New/Ready/InProgress/InReview → `Skipped`, like `wall_clock_cap_reached_fails_from_each_active_state` ~`:554`), `dependency_failed_is_illegal_from_terminal_states` (loop Done/Failed/Skipped, like `merge_conflict_is_illegal_from_non_in_review_states` `:611`), and `skipped_is_terminal`. (The exhaustive transition-table count update is owned by the companion task `fsm-skipped-tests`.)
- **Depends on:** —
- **Done when:** the new unit tests assert each of the four `→ Skipped` transitions, that `DependencyFailed` is rejected from `Done/Failed/Skipped`, and that `Skipped` is terminal; **and the whole workspace compiles** (so the cross-crate ripple is caught). Run `cargo build --workspace && cargo test --workspace` (at minimum `cargo test -p makina-core state_machine && cargo build -p makina`) — both pass.

### fsm-skipped-tests — Re-prove FSM totality with the new state/event
Extend the exhaustive Cartesian-product totality test `exhaustive_transition_table` at `crates/makina-core/src/state_machine.rs:353` (inside `mod tests`) and its **three driving helpers** — do **not** confuse this with the named MergeConflict tests at `:602`/`:611`. Edit: (a) `all_states()` (`:319`) — append `Skipped` → 7 states; (b) `all_events()` (`:325`) — append `DependencyFailed` → 12 events; (c) `legal_table()` (`:295`) — append the four rows `(New, DependencyFailed, Skipped)`, `(Ready, DependencyFailed, Skipped)`, `(InProgress, DependencyFailed, Skipped)`, `(InReview, DependencyFailed, Skipped)`, mirroring the `(InReview, MergeConflict, Failed)` row already at `:312`. This is the same shape as how MergeConflict was added in plan-0002 (one `legal_table` row `:312` + one `all_events` entry `:337`), except `DependencyFailed` also adds a new **state** so `all_states` grows too.

Then update the three hard-coded count asserts: `total == 84` (`:366`, 7×12), `legal_count == 19` (`:393`, 15+4), `illegal_count == 65` (`:394`, 84−19), and fix their message strings (e.g. `"7 states × 12 events = 84 pairs"`, `"exactly 19 legal transitions"`, `"exactly 65 illegal transitions"`). Also update the stale rationale doc-comments that embed the old numbers so a reviewer accepts the PR: the test-module doc at `:276` (6→7 states, 11→12 events, 66→84), `:278-:283` (15→19 legal, 51→65 illegal; note `DependencyFailed` as the 12th event and `Skipped` as the 7th state), and the test rustdoc at `:344-:351` (6×11 → 7×12; 15/51/66 → 19/65/84).
- **Depends on:** fsm-skipped-state
- **Done when:** `cargo test -p makina-core state_machine` passes with `exhaustive_transition_table` now iterating 84 pairs and asserting exactly 19 legal / 65 illegal, proving the FSM is still total with `Skipped` + `DependencyFailed`.

### sched-skip-dependents — Mark a failed task's transitive dependents `Skipped`
Add `fn mark_dependents_skipped(graph: &mut TaskGraph, failed_task_id: &TaskId) -> Vec<TaskId>` near the other locked helpers in `crates/makina-core/src/actors/supervisor.rs` (~`:1852`, beside `mark_finished_locked`). It is a **reverse-edge** BFS: `depends_on` (`task.rs:133`) lists a task's *prerequisites*, so a failed task's dependents are tasks whose `depends_on` transitively *contains* the failed id — there is **no** reverse-adjacency / `dependents()` helper anywhere, so build the set inline. Starting from `failed_task_id`, repeatedly scan `graph.tasks` for any task whose `depends_on` contains an already-collected id; for each newly found task that is **not already terminal** (guard with `is_terminal(state)`), apply `apply_event_locked(graph, id, TaskEvent::DependencyFailed)` (`supervisor.rs:1760`) and `mark_finished_locked(graph, id)` (`:1852`) under the held lock, and collect the id. (`apply_event_locked` already rejects the event for `Done/Failed/Skipped`, so the `is_terminal` guard keeps it clean.)

Call it from the two **driver-Failed terminal arms** of `scheduler()` (`supervisor.rs:914`): the hard-error arm `Some(Ok((id, Some(Err(e)))))` at `:1060-1074` and the wall-clock-cap arm `Some(Ok((id, None)))` at `:1075-1105`. The graph-advance-failure path at `:986-987` is **out of scope** (it is not a driver outcome). Respect the graph-lock invariant: never hold the guard across an `.await`. So per arm: collect the skipped ids under the lock, **drop the guard**, then call `ctx.persist().await` (`:492`) once, then `ctx.emit_task_state(id, TaskState::Skipped)` (`:458`) for each skipped id so the TUI reflects it, and push `(id, TaskState::Skipped)` onto `outcomes` — mirroring the wall-clock arm's outcome push at `:1104`. This is required because `next_ready_task_id` (`:1136`) needs every dep `== Done`, so a failed task's dependents would otherwise dangle non-terminal forever. (`TaskState::Skipped` / `TaskEvent::DependencyFailed` come from `fsm-skipped-state`.)
- **Depends on:** fsm-skipped-state
- **Done when:** add `#[tokio::test] async fn dependents_of_failed_task_are_skipped()` to `crates/makina-core/tests/termination_caps.rs` — reuse its `config()` / `setup_temp_repo` and the always-failing `false` gate with `gate_iterations: 2` (`termination_caps.rs:229/:241`) to force task A to `Failed`, with a concurrency-2 graph A=[], B=[A], C=[A], D=[B] built via the `task(id, &[deps])` helper style from `concurrency.rs:277`. Assert via the `TaskGraphSnapshot` ask that A is `Failed` and B, C, D each reach `TaskState::Skipped` (with `finished_at` stamped) in the final graph **and** appear as `Skipped` in `report.outcomes`. Run `cargo test -p makina-core --test termination_caps dependents_of_failed_task_are_skipped` plus `cargo test --workspace`; clippy + fmt pass.

### sched-continue-on-failure — Keep launching independents after a task fails
Change `scheduler()` (`crates/makina-core/src/actors/supervisor.rs:914`) so a **task-level** failure no longer halts the run. There are four `stop_launching = true` sites: `:953` (cancel path — **do NOT touch**), `:987` (advance-to-ready `Err`), `:1073` (driver hard-`Err` arm), `:1118` (panic arm). At the driver hard-`Err` arm `:1060-1074`, remove **both** `fatal_error.get_or_insert(e)` and `stop_launching = true`: the task is already moved to `Failed` and emitted at `:1066-1071` (and its dependents are now `Skipped` by `sched-skip-dependents`), so it must not feed `fatal_error`. This is load-bearing — the final match at `:1125-1128` returns `Err(e)` whenever `fatal_error` is `Some`, so leaving `fatal_error.get_or_insert(e)` would still report the whole run as a hard error and contradict `sched-run-status-failed`. At the advance-to-ready error path `:984-988`, keep recording the error but do not stop launching for a task-level failure. The **only** thing that may remain fatal (set `fatal_error` + `stop_launching`) is a genuine driver **panic** at the join-error arm `:1116-1119` (`!join_err.is_cancelled()`).

Test failure-injection: the existing `false`-gate path (`termination_caps.rs:241`) is **run-wide** (gates live in `Config`), so to fail exactly one of three independents add a test-only backend keyed by task id (mirror `CountingBackend` at `concurrency.rs:104-152`) whose `prompt()` returns `Err` / drives `Failed` for the designated task and `Ok`/approve for the other two. For the panic half, add a second test-only backend whose `AgentSession::prompt` does `panic!("injected")` for its task.
- **Depends on:** sched-skip-dependents
- **Done when:** add `crates/makina-core/tests/continue_on_failure.rs` mirroring the `termination_caps.rs`/`concurrency.rs` harness (`setup_temp_repo`, `build_actor_tree`, `run_with_timeout`, `config(...)`). With three independent ready tasks where one fails, assert `report.outcomes` contains the two others as `Done` and the failed one as `Failed`, and that `run_with_timeout` did **not** return `Err` (run not halted). With the panic backend, assert the `RunReadyTasks` reply is `Err` containing `"panicked"` (the `fatal_error` path at `:1126`). Run `cargo test -p makina-core --test continue_on_failure` and `cargo test -p makina-core`; clippy + fmt pass.

### sched-run-status-failed — Report a `Failed` run status without halting
Enrich the test-facing `RunReport` so a completed-but-failed run records *which* tasks failed and *why*. `RunReport` is defined at `crates/makina-core/src/actors/supervisor.rs:357-362` (**not** `api.rs`) and built at exactly one site, `:1127` (`None => Ok(RunReport { outcomes })`). Add `pub failed_tasks: Vec<(TaskId, String)>` to the struct (the `String` is the failure reason; the struct derives `Debug/Clone/PartialEq/Eq`, and `Vec<(TaskId, String)>` is `Eq`-comparable). In `scheduler()`, add `let mut failed_tasks: Vec<(TaskId, String)> = Vec::new();` next to `outcomes` (~`:929`), push `(id.clone(), reason)` in each arm that yields a `Failed` terminal, and change the build site `:1127` to `Ok(RunReport { outcomes, failed_tasks })`.

Reason sources (a junior cannot infer these — they are specified here): the hard-error arm at `:1060` already binds `e` → use `e`. Cap failures carry no reason string today, so synthesize a literal from the arm that produced the `Failed` terminal — `"wall-clock-cap-reached"` at the wall-clock arm `:1075-1105`; for gate/review caps that surface through the success arm (`Some(Ok((id, Some(Ok(state)))))` at `:1054` when `state == Failed`) push a reason derived from the terminal cause (e.g. `"gate-cap-reached"` / `"review-cap-reached"`). **Scope note:** the orchestrator already derives terminal `RunStatus::Failed` from *live* task states (`orchestrator.rs:272-283` — `all_done` → `Completed` else `Failed`), and `Skipped`/`Failed` tasks are not `Done`, so **no orchestrator/run-status change is needed**; this task only enriches `RunReport`. If feeding `failed_tasks` into `run.json` is later wanted, that is a separate task. Audit existing `RunReport` consumers — they read only `.outcomes` (`supervisor_write_path.rs:187/314/432`, `termination_caps.rs:272/379`, `concurrency.rs`, `supervisor_audit_registry.rs:213`), so they survive the field addition; confirm none build a full `RunReport` literal for comparison.
- **Depends on:** sched-continue-on-failure
- **Done when:** extend the `continue_on_failure.rs` test from `sched-continue-on-failure` to assert `report.failed_tasks` contains the failed task id with a non-empty reason, while `report.outcomes` shows the two independents as `Done` and any dependents as `Skipped`. Run `cargo test -p makina-core --test continue_on_failure` and `cargo test -p makina-core`; clippy + fmt pass.

### sched-parallelism-instrument — Make per-driver start/end intervals readable in tests
Make each driver's start/end interval observable for an overlap assertion. **The timestamps already exist:** `Task.started_at` (`task.rs:163`, stamped by `mark_started_locked` at `supervisor.rs:1843`, called at the Ready→Dispatched step in `task_driver`) and `Task.finished_at` (`task.rs:168`, stamped by `mark_finished_locked` at `supervisor.rs:1852` on every terminal), both readable via a `TaskGraphSnapshot` ask → `graph.get(id)` (`task.rs:197`). So this is a **test-only task — no production change is required**; do *not* add a redundant second mechanism. (Only if a *live* event is genuinely needed — it is not for this task — would you add a concrete `api::Event` variant at `api.rs:430` like `DriverStarted { run, task, at }` and emit it from `task_driver` (`supervisor.rs:1283`) next to `mark_started_locked`, updating every match site.)

Reuse the existing deterministic overlap harness in `crates/makina-core/tests/concurrency.rs`: `CountingBackend` (`:104`) with an N-party `tokio::sync::Barrier` and `max_observed()` (`:126`), as used by `parallel_up_to_the_limit` (`:357`), and read per-task state via the `TaskGraphSnapshot` ask → `snapshot.get(&TaskId::new(...))` (`concurrency.rs:396`).
- **Depends on:** —
- **Done when:** add `tests/concurrency.rs::driver_intervals_observable` (or extend `parallel_up_to_the_limit`) running two independent tasks at `concurrency = 2` with a 2-party barrier; after the run, read `snapshot.get(a).started_at/finished_at` and the same for `b` and assert the intervals **overlap**: `a.started_at < b.finished_at && b.started_at < a.finished_at` (keep the strict inequality — do not weaken to "non-None"). Run `cargo test -p makina-core --test concurrency driver_intervals_observable`; it passes.

### sched-parallelism-verify-test — Prove ≥2 drivers overlap at concurrency=2 (deterministic)
In `crates/makina-core/tests/concurrency.rs`, add a test that proves real parallelism the way the suite already does — via the deterministic barrier, **not** flaky timestamp-only timing (the module doc at `concurrency.rs:32-37` explicitly rejects timing-based assertions and arbitrary sleeps). Mirror `parallel_up_to_the_limit` (`:357`): two **independent** ready tasks, `concurrency = 2`, `CountingBackend::new(Some(2), <approve-verdict>)` so the 2-party `Barrier` (`:91`, `:113`) forces simultaneity, and assert `probe.max_observed() == 2` (the existing `AtomicUsize` peak, `:126`). Reuse `setup_temp_repo` (`:213`), `task(id, &[deps])` (`:277`), `config_with_concurrency` (`:297`), `build_actor_tree` (`:305`), `run_with_timeout` (`:340`). If `sched-parallelism-instrument` exposes the start/end intervals, additionally read them via `snapshot.get(id)` and assert the two drivers' intervals intersect, **cross-checked against** `max_observed() == 2` so the assertion stays deterministic.
- **Depends on:** sched-parallelism-instrument
- **Done when:** `cargo test -p makina-core --test concurrency drivers_overlap_under_concurrency_2` passes, asserting `max_observed() == 2` (and, when the instrument hook exists, intersecting start/end intervals for the two drivers).

### sched-parallelism-root-cause — Root-cause the dogfood "sequential appearance" and record the finding
Investigation only — no production code change here. Determine why **real (non-test)** runs looked sequential. Crucially, the in-process integration harness uses `CountingBackend` with **no `cargo` gates** (`config_with_concurrency`, `concurrency.rs:297`; `develop_until_gates_pass` runs `run_gates` over an empty gate list), so the prime suspect — concurrent `cargo` builds across worktrees that share a target dir (no `CARGO_TARGET_DIR` isolation exists anywhere in `makina-core/src` or `worktree.rs` today) — is **not reproducible** by the deterministic test above and must be probed against a *real* gate config. Provide a concrete method: configure a real slow cargo gate (`GateConfig { command: "cargo build", .. }`, per `config.rs`) across two worktrees and observe wall-clock overlap of the gate subprocesses (`GateRunner::run_gates` shells each `GateConfig.command` via `sh -c` in the worktree `working_dir`); capture whether the worktrees share a target dir. Write the conclusion — measured root cause + either a concrete fix recommendation (naming the file to change) or an explicitly-accepted limitation — into a new `### Parallelism root-cause` subsection in `docs/plans/0003-Runtime-and-TUI-Hardening/ARCHITECTURE.md`, after the "Decisions & open questions" section.
- **Depends on:** sched-parallelism-verify-test
- **Done when:** `grep -n 'Parallelism root-cause' docs/plans/0003-Runtime-and-TUI-Hardening/ARCHITECTURE.md` returns the new subsection, which names the measured cause and either a concrete fix recommendation (with the file to change) or an explicitly-accepted limitation.

### sched-isolated-target-dirs — Isolate the cargo target dir per worktree (only if root-cause confirms it)
**Conditional** on `sched-parallelism-root-cause` finding a real, reproducible build-contention defect. Give each worktree its own cargo target so concurrent gate `cargo` builds do not serialize on a shared target lock. Likely touch points: worktree creation in `crates/makina-core/src/worktree.rs` and/or gate execution in `gate.rs` (`GateRunner::run_gates` builds a `tokio::process::Command` — add `.env("CARGO_TARGET_DIR", <per-worktree path>)`). Behavior must stay unchanged for non-cargo gates.
- **Depends on:** sched-parallelism-root-cause
- **Done when:** an integration test with two concurrent tasks each running a real cargo gate shows their gate subprocesses overlap in wall-clock (no target-lock serialization), and `cargo test -p makina-core` passes; the ARCHITECTURE `Parallelism root-cause` subsection is updated to mark the limitation resolved.

---

## 0015 — TUI Presentation & Views

### tui-error-pane-state — Error-pane state on `App`
In `crates/makina/src/app.rs`, mirror the existing `ExchangeLog` bounded-ring
pattern. Add a self-contained `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub
enum ErrorLevel { Error, Warn, Info }` and `#[derive(Debug, Clone)] pub struct
ErrorMessage { pub timestamp: std::time::SystemTime, pub level: ErrorLevel, pub
text: String }` near `ExchangeEntry` (~`app.rs:27`) — this avoids pulling
`chrono`/`tracing` into `crates/makina/Cargo.toml` (neither is currently a
dependency of the `makina` crate). Add `pub const ERROR_MESSAGES_CAP: usize =
50;` near `EXCHANGE_LOG_CAP` (`app.rs:23`). Add `pub error_pane_open: bool` and
`pub error_messages: Vec<ErrorMessage>` to the `App` struct (`app.rs:251`) and
initialize both in `App::new` (`app.rs:308-331`, e.g. `error_pane_open: false,
error_messages: Vec::new()`). Add an `impl App` method `pub fn push_error(&mut
self, msg: ErrorMessage)` that pushes and, when `error_messages.len() >
ERROR_MESSAGES_CAP`, calls `self.error_messages.remove(0)` — copy
`ExchangeLog::push` at `app.rs:53-60`. Pure state — no rendering or events yet.
- **Depends on:** —
- **Done when:** a new unit test `error_messages_bounded_at_cap` in the
  `#[cfg(test)] mod tests` block (`app.rs:638`) pushes `ERROR_MESSAGES_CAP + 5`
  messages with distinguishable `text` (e.g. `format!("msg {i}")`) via
  `push_error`, then asserts `app.error_messages.len() == ERROR_MESSAGES_CAP`
  **and** that the first element is the 6th-pushed message and the last is the
  most recent (i.e. the OLDEST was evicted, not merely len-capped — stronger
  than the existing `exchange_log_bounded_at_cap` at `app.rs:1770/:1805` which
  only checks `<= cap`); `cargo test -p makina error_messages_bounded_at_cap`
  and the full `cargo test -p makina` pass.

### tui-error-pane-toggle — Toggle key + event
Add `AppEvent::ToggleErrorPane` to the `AppEvent` enum (`app.rs:148-234`, place
it near the other simple variants). Map `KeyCode::Char('e') |
KeyCode::Char('E') => AppEvent::ToggleErrorPane` in the normal-mode keymap match
inside `translate_key` (`event.rs:354-373`; `e`/`E` are currently unmapped —
verified). Add an `App::update` arm in the match at `app.rs:368` that sets
`self.error_pane_open = !self.error_pane_open` and returns `true`. Mirror the
`FocusNext` flag-flip arm (`app.rs:379-385`): returning `true` IS the redraw
request (the bool is consumed at `event.rs:120-127`); there is no
`request_redraw()` API. `error_pane_open: bool` is added by
`tui-error-pane-state` — do not add the field here.
- **Depends on:** tui-error-pane-state
- **Done when:** an `app.rs` unit test `error_pane_toggle_flips_flag` (mirroring
  `focus_cycles_between_panels` at `app.rs:671`) asserts `error_pane_open`
  starts `false`, `app.update(AppEvent::ToggleErrorPane)` flips it to `true` and
  returns `true`, and a second call flips it back; an `event.rs` test
  `e_key_translates_to_toggle_error_pane` (mirroring `tab_translates_to_focus_next`
  at `event.rs:420`, using the `key_press` helper at `event.rs:383`) asserts
  `matches!(translate_terminal_event(key_press(KeyCode::Char('e'),
  KeyModifiers::NONE), false), AppEvent::ToggleErrorPane)`; `cargo test -p
  makina` passes.

### tui-error-pane-render — Render the collapsible pane + error badge
In `crates/makina/src/ui.rs`, add `render_error_pane(app: &App, frame: &mut
Frame, area: Rect)` mirroring `render_exchange_pane` (`ui.rs:385`), and call it
from `render` against a new layout chunk (mirror the call site at `ui.rs:334`).
Extend the per-task inner vertical split at `ui.rs:236-243` by adding a 4th
constraint AFTER the exchange pane: `Constraint::Length(error_pane_height)` (e.g.
5 rows) when `app.error_pane_open`, else `Constraint::Length(0)`; pass `split[3]`
to `render_error_pane` (a 0-height area is a no-op). When open, show the recent
`error_messages`, coloring each by level via a match (`ErrorLevel::Error =>
Color::Red`, `Warn => Color::Yellow`, `Info => Color::DarkGray`) — use the
`ErrorMessage.level: ErrorLevel` type from `tui-error-pane-state`. When closed
but `!error_messages.is_empty()`, append the count to the Exchange title at
`ui.rs:386-387`, e.g. `.title(format!(" Exchange ({n} errors) "))`. This task
reads `App.error_pane_open: bool` and `App.error_messages` (bounded buffer of
`ErrorMessage { timestamp, level, text }`) added by
`tui-error-pane-state`/`-toggle`.
- **Depends on:** tui-error-pane-toggle
- **Done when:** two `ratatui::TestBackend` render tests in the `ui.rs` `mod
  tests` (using `make_terminal` at `ui.rs:722` and `screen_of` at `ui.rs:1110`):
  `render_error_pane_shows_messages_when_open` sets `error_pane_open = true` with
  messages present and asserts `screen_of(&terminal)` contains a message's text
  AND that a `cell.fg` matches the level color (clone the buffer and scan
  `buf.content()`, mirroring `render_failed_badge_uses_red_fg` at
  `ui.rs:1036/:1050-1054`); `render_error_badge_when_collapsed_with_errors` sets
  `error_pane_open = false` with messages present and asserts `screen_of`
  contains the count badge in the Exchange title and that message text is NOT
  shown; `cargo test -p makina ui::tests::render_error_pane` passes.

### tui-error-pane-channel-wire — Thread the log channel into the event loop
Wire plan-0013's `log-subscriber-tui-channel` TUI channel into the event loop and append its
records to `error_messages`. Change `event::run` (`event.rs:63`, currently `pub
async fn run(tui: &mut Tui, app: &mut App) -> std::io::Result<()>`) to accept the
receiver, e.g. `pub async fn run(tui: &mut Tui, app: &mut App, mut log_rx:
tokio::sync::mpsc::Receiver<ErrorMessage>) -> std::io::Result<()>` — the item
type is the one `log-subscriber-tui-channel` sends; if `log-subscriber-tui-channel` instead forwards a
raw `tracing` record type, name it there and do the record→`ErrorMessage`
(timestamp/level/text extraction) conversion in THIS task. Update the call site
at `main.rs:122` to pass the receiver returned by `log-subscriber-tui-channel`'s init. Add a
fourth arm to the `tokio::select!` at `event.rs:89-108`, mirroring the existing
`term_tx`/`term_rx` mpsc channel + arm (`event.rs:71`, `:93-95`): `maybe_log =
log_rx.recv() => maybe_log.map(|msg| AppEvent::ErrorMessageArrived { msg })`. Add
`ErrorMessageArrived { msg: ErrorMessage }` to `enum AppEvent` after
`StatusMessage(String)` (`app.rs:233`), and an `update` arm after the
`StatusMessage` arm (`app.rs:519`) that pushes `msg` via `app.push_error(msg)`
(respecting `ERROR_MESSAGES_CAP` from `tui-error-pane-state`) and returns `true`
for redraw — mirror the `StatusMessage` arm exactly. Note: `event::run` and the
`select!` loop are at `event.rs:63`/`:89-108` — these are NOT the `~320` mouse
site (that is `translate_terminal_event` at `event.rs:324-325`, handled by
`tui-mouse-scroll`).
- **Depends on:** log-subscriber-tui-channel, tui-error-pane-state
- **Done when:** a new `crates/makina/tests/error_pane_wire.rs` `#[tokio::test]
  fn log_record_appears_in_error_messages` builds an `mpsc::channel::<ErrorMessage>(8)`,
  sends one record, drives the drain path, and asserts `app.error_messages`
  contains it; plus a `ratatui::TestBackend` render test asserting the message
  text renders when `error_pane_open == true`; `cargo test -p makina --test
  error_pane_wire` + clippy + fmt pass.

### tui-error-pane-no-frame-bypass — Stop in-frame system errors bypassing the frame
Audit the live-frame region of the event loop so in-frame `tracing::error!` /
`tracing::warn!` records reach the pane (via `tui-error-pane-channel-wire`)
rather than stdout/stderr. Explicitly DO NOT touch `main.rs:57/116/126`: those
three `eprintln!` sites are pre-`Tui::init()` (config-load failure at `main.rs:57`,
runs before `Tui::init()` at `:113`), the terminal-init failure itself
(`main.rs:116`), and a post-`tui.restore()` print (`main.rs:126`) — none has a
live ratatui frame to render into, so they correctly stay as `eprintln!`. Update
ARCHITECTURE.md (the claim at lines 144-145 that the pane "Replaces the
`eprintln!` frame-bypass (`main.rs:57/116/126`)") to note these three fatal sites
are intentionally exempt. If any `eprintln!`/`println!` exists between
`Tui::init()` (`main.rs:113`) and `tui.restore()` (`main.rs:125`), redirect it to
a `tracing::error!` so it flows into the pane.
- **Depends on:** tui-error-pane-channel-wire
- **Done when:** a test (or CI `grep`) asserts no `eprintln!`/`println!` appears
  inside the live-frame region of the event loop; ARCHITECTURE.md records that
  the three fatal `main.rs` sites are exempt; `cargo test -p makina` + clippy +
  fmt pass.

### tui-ansi-parser — ANSI SGR → ratatui style parser
Add `crates/makina/src/ansi.rs` with `parse_ansi(input: &str) -> Vec<AnsiSpan>`
where `pub struct AnsiSpan { pub text: String, pub style: ratatui::style::Style }`,
converting SGR sequences (colors, bold, reset) to `ratatui::style::Style` and
stripping non-SGR control codes. Register the module by adding `mod ansi;` to
`crates/makina/src/main.rs` (alongside the existing `mod app; … mod ui;` at
`main.rs:28-34`) — without this the new file is never compiled (the crate has no
`lib.rs`). Hand-roll the parser — the SGR subset is small and avoids a new
dependency: map `32 → Color::Green`, `31 → Color::Red`, `1 → Modifier::BOLD`,
`0 → reset to Style::default()`, mirroring the `Color`/`Modifier` usage already in
`ui.rs:40,461,468`.
- **Depends on:** —
- **Done when:** an inline `#[cfg(test)] mod tests` in `ansi.rs` (mirroring
  `ui.rs:711` / `app.rs:638`) asserts: a `\x1b[32m` run yields a span with
  `style.fg == Some(Color::Green)`; `\x1b[31m` → `Color::Red`; `\x1b[1m` →
  `Modifier::BOLD` set; `\x1b[0m` resets to `Style::default()`; and a non-SGR
  sequence like `\x1b[2J` or `\x1b[H` is stripped so no literal `\x1b` byte
  remains in any `AnsiSpan.text`. `cargo test -p makina ansi` passes and `cargo
  clippy -p makina --all-targets -- -D warnings` is clean.

### tui-diff-coloring — Unified-diff line coloring
Add `pub fn diff_line_style(line: &str) -> Option<ratatui::style::Style>` to
`crates/makina/src/ansi.rs` (the module created and registered by
`tui-ansi-parser`). It inspects the line prefix only and does NOT modify the
text, so indentation/alignment are preserved by construction. Return
`Some(Style::default().fg(Color::Green))` when the line starts with `+` (but not
`+++`), `Some(...fg(Color::Red))` for `-` (but not `---`),
`Some(...fg(Color::Cyan))` for a line starting with `@@`; `None` otherwise (and
treat `+++`/`---` file headers as `None`). `AnsiSpan` and `parse_ansi` are
provided by `tui-ansi-parser`; this task only adds the prefix→`Style` helper.
Composition contract for the consumer `tui-exchange-render`: `diff_line_style`
produces a per-line base `fg`; the consumer overlays `parse_ansi` SGR spans on
top, so ANSI styling WINS where present and the diff `fg` applies otherwise.
Mirror the `Color`/`Style` usage in `exchange_entry_lines` (`ui.rs:453`,
`Span::styled(text, Style::default().fg(Color::Green))`).
- **Depends on:** tui-ansi-parser
- **Done when:** an inline test `diff_line_style_colors_prefixes` in
  `crates/makina/src/ansi.rs` (precedent `ui.rs:711`) asserts on `Style.fg` (not
  rendered text): `diff_line_style("+added").unwrap().fg == Some(Color::Green)`,
  `diff_line_style("-removed").unwrap().fg == Some(Color::Red)`,
  `diff_line_style("@@ -1 +1 @@").unwrap().fg == Some(Color::Cyan)`,
  `diff_line_style("  context") == None`, and `diff_line_style("+++ b/file") ==
  None` (file header, not an added line); `cargo test -p makina diff_line_style`
  passes and `cargo clippy -p makina --all-targets -- -D warnings` is clean.

### tui-exchange-render — Apply ANSI + diff styling in the exchange pane
Refactor `exchange_entry_lines` (`crates/makina/src/ui.rs:453`, signature `fn
exchange_entry_lines(entry: &ExchangeEntry) -> Vec<Line<'static>>` — the `'static`
return means spans must OWN their text, so clone out of any `AnsiSpan`). Import
via `use crate::ansi::{parse_ansi, AnsiSpan, diff_line_style};` (`mod ansi;` is
registered by `tui-ansi-parser`). Apply parsing only to the response-text spans
(`ui.rs:500-505`); leave the prompt branch (`ui.rs:458-482`) and all role labels
unchanged. Mechanical composition order: (1) compute `text_to_show` exactly as
today, including the streaming-cursor append at `ui.rs:494-499` (`format!("{}\u{2588}",
entry.text)`) — the cursor must be appended BEFORE parsing so it is not eaten as
an escape (it contains no ESC, so it survives); (2) for each `text_line` in
`text_to_show.lines()`, run `parse_ansi(text_line)` to get `Vec<AnsiSpan>`; (3)
if `diff_line_style(text_line)` is `Some(s)`, apply `s.fg` to every span on that
line (diff prefix sets the base fg, ANSI SGR wins where present), else use each
`AnsiSpan.style`; (4) prefix the line with the existing two-space indent `"  "`
as its own default-styled `Span`; (5) push `Line::from(spans)`. No raw `\x1b[..m`
renders literally anymore.
- **Depends on:** tui-ansi-parser, tui-diff-coloring
- **Done when:** `#[test] fn exchange_render_styles_ansi_and_diff_no_literal_escape()`
  in the `ui.rs` `mod tests`, mirroring `render_exchange_pane_shows_prompt_and_concatenated_answer`
  (`ui.rs:1582`) to build the `App` and `render_focused_task_row_is_highlighted`
  (`ui.rs:1649`) to inspect cells, builds an `ExchangeEntry` whose `text` contains
  `\x1b[32m+added\x1b[0m` and a `@@` hunk header, then asserts (a) no cell symbol
  equals the ESC char `\u{1b}` and the flattened buffer has no `[32m` substring
  (scan `terminal.backend().buffer().content()`, do NOT rely on `screen_of`
  alone — it keeps only each cell's first char), and (b) at least one cell on the
  `+added` line has `fg == Color::Green` and the `@@` line has `fg ==
  Color::Cyan`; prompt/label rendering is unchanged. `cargo test -p makina
  exchange_render_styles_ansi_and_diff_no_literal_escape` passes plus `cargo
  clippy --all-targets -- -D warnings` and `cargo fmt --check`.

### tui-scroll-state — Exchange-pane scroll offset on `App`
In `crates/makina/src/app.rs`, add `pub exchange_scroll: u16` (manual offset) and
`pub exchange_auto_follow: bool` (default `true`) to the `App` struct
(`app.rs:251-302`, after `exchange_logs`), and initialize both in `App::new`
(`app.rs:308-331`). `App` does not know the rendered line count or pane height,
so the clamp upper bound must be passed IN: define `pub fn scroll_up(&mut self)`
and `pub fn scroll_down(&mut self, scroll_max: u16)`; the caller (the
render/event layer in `tui-mouse-scroll`) computes `scroll_max` exactly like the
current `render_exchange_pane` logic at `ui.rs:431-442` (`total_lines.saturating_sub(pane_height)`).
Auto-follow rules, mirroring the clamped-index helpers already in `App::update`
(`SelectDown` `(current + 1).min(last)` at `app.rs:424-429`, `SelectUp`
`saturating_sub(1)` at `app.rs:402`): `scroll_up` sets `exchange_auto_follow =
false` and `exchange_scroll = exchange_scroll.saturating_sub(1)`; `scroll_down`
sets `exchange_scroll = (exchange_scroll + 1).min(scroll_max)` and, if
`exchange_scroll == scroll_max`, sets `exchange_auto_follow = true` (re-engage at
bottom). Also add `pub fn effective_offset(&self, scroll_max: u16) -> u16`
returning `scroll_max` when `exchange_auto_follow` else
`exchange_scroll.min(scroll_max)`. The `ui.rs:431-442` auto-scroll block is what
`tui-mouse-scroll` will later replace — do not change it here.
- **Depends on:** —
- **Done when:** a new unit test `exchange_scroll_clamps_and_auto_follow_reengages`
  in the `#[cfg(test)] mod` of `app.rs` (alongside `task_selection_up_down_main_panel`
  at `app.rs:1673`) asserts (1) `scroll_down(max)` never exceeds `max` and
  `scroll_up` never goes below `0`, (2) `scroll_up` clears `exchange_auto_follow`,
  (3) `scroll_down` reaching `max` re-sets `exchange_auto_follow = true`; `cargo
  test -p makina` passes.

### tui-mouse-scroll — Mouse wheel scrolls the focused pane
Enable mouse capture and route wheel events to the scroll helpers. First, in
`crates/makina/src/tui.rs`: add `EnableMouseCapture` to the `Tui::init` `execute!`
(`tui.rs:57`) and `DisableMouseCapture` to the `restore` `execute!` (`tui.rs:74`),
importing both from `ratatui::crossterm::event` (extend the import near
`tui.rs:27-29`) — WITHOUT this, crossterm delivers no `Mouse` events and the
feature is dead. In `translate_terminal_event` (`event.rs:320`), add an arm
BEFORE the `_ => AppEvent::Tick` catch-all (`event.rs:325`): `CrosstermEvent::Mouse(m)
=> match m.kind { MouseEventKind::ScrollUp => AppEvent::ScrollUp,
MouseEventKind::ScrollDown => AppEvent::ScrollDown, _ => AppEvent::Tick }`, and
extend the import at `event.rs:36` to add `MouseEvent, MouseEventKind`. Scroll
regardless of the `browsing` flag (the exchange pane is not the browser). In
`enum AppEvent` (`app.rs:148`), add `ScrollUp` and `ScrollDown` after `SelectDown`
(`app.rs:159`), with doc comments mirroring `SelectUp`/`SelectDown`. In
`App::update` (`app.rs:368`), add arms mirroring `AppEvent::BrowserUp`/`BrowserDown`
(`app.rs:486-497`): `AppEvent::ScrollUp => { self.scroll_up(); true }` and
`AppEvent::ScrollDown => { self.scroll_down(scroll_max); true }` using the helpers
from `tui-scroll-state` (`scroll_up()`, `scroll_down(scroll_max)`,
`effective_offset`, fields `exchange_scroll` / `exchange_auto_follow`). In
`render_exchange_pane` (`ui.rs:385`), replace the auto-scroll block at
`ui.rs:431-442`: compute `let scroll_max = total_lines.saturating_sub(pane_height)
as u16;` and `let scroll_offset = app.effective_offset(scroll_max);`, then keep
`.scroll((scroll_offset, 0))`. Task switching stays on keys/sidebar.
- **Depends on:** tui-scroll-state
- **Done when:** in the `event.rs` tests module, a helper `fn wheel(kind:
  MouseEventKind) -> CrosstermEvent { CrosstermEvent::Mouse(MouseEvent { kind,
  column: 0, row: 0, modifiers: KeyModifiers::NONE }) }` and a test
  `wheel_translates_to_scroll` assert `matches!(translate_terminal_event(wheel(MouseEventKind::ScrollUp),
  false), AppEvent::ScrollUp)` (and `ScrollDown`); an `app.rs` `update` test
  `scroll_event_changes_offset_not_selection` records `selected_task`, drives
  `app.update(AppEvent::ScrollUp)`/`ScrollDown`, and asserts the scroll offset
  changed while `selected_task` is unchanged; `Tui::init` enables mouse capture
  and `Tui::restore` disables it (covered by a `tui.rs` init+restore round-trip
  smoke test / code review). `cargo test -p makina` passes.

### runview-project-field — Add `project` (repo basename) to `RunView`
In `crates/makina-core/src/api.rs` (struct `RunView` at `api.rs:224-238`, which
today carries only `id`, `task_list_path`, `status`, `tasks`), add `pub project:
String` and document it as the repo directory basename. Populate it in
`build_view` in `crates/makina-core/src/orchestrator.rs` (`:156-172`) from the
worktree manager's `repo_root` basename (`repo_root.file_name().and_then(|s|
s.to_str()).unwrap_or("")`). Update every `RunView { .. }` literal (test
constructors in `api.rs`, `orchestrator.rs`, and the TUI crate's `app.rs`/`ui.rs`
tests) to set the new field. Mirror how the other `RunView` fields are threaded
through `build_view`.
- **Depends on:** —
- **Done when:** a unit test on `build_view` asserts `view.project` equals the
  `repo_root` basename for a known `repo_root`; `cargo test -p makina-core`
  passes (workspace builds clean with the new field set everywhere).

### tui-sidebar-label — Show `{project}/{plan}` as the run label
In `crates/makina/src/ui.rs:130-134`, replace the `file_stem()`-only `name`
derivation (`run.task_list_path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown")`,
rendered via `Span::raw(name)` at `ui.rs:139`) with a `{project}/{plan}` label.
`{plan}` = the parent-directory name of `task_list_path` when the path is
plan-style; `{project}` = `run.project` (from `runview-project-field`). Use the
safe accessor idiom from `orchestrator.rs:404-408`
(`.file_stem()/.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str())`).
Plan-style predicate (so the fallback is mechanical): the path fits the shape when
its `file_name` is `TASKS.md` (case-insensitive) AND its parent dir name is
non-empty; otherwise fall back to `file_stem()` alone. Render the exact string
`format!("{project}/{plan}")` (no surrounding spaces). Example: `docs/plans/0002-Governance-and-Persistence/TASKS.md`
→ `makina/0002-Governance-and-Persistence`.
- **Depends on:** runview-project-field
- **Done when:** `#[test] fn render_sidebar_shows_plan_label()` in the
  `crates/makina/src/ui.rs` tests module (mirroring
  `render_sidebar_shows_run_name_and_status_badge` at `ui.rs:860`) builds a
  `RunView { task_list_path: PathBuf::from("docs/plans/0002-Governance-and-Persistence/TASKS.md"),
  project: "makina".into(), .. }`, renders on a `TestBackend`, flattens the
  buffer, and asserts `screen.contains("0002-Governance-and-Persistence")` and
  `!screen.contains("TASKS")`; a second case with a `.tasks/feature.json` path
  still renders `feature` (the stem fallback). `cargo test -p makina
  ui::tests::render_sidebar_shows_plan_label` passes.

### tui-gr-legend — Clarify the `G`/`R` columns with a legend
In `crates/makina/src/ui.rs`, append a legend to the existing status-bar text
rather than stealing a body row (the top-level layout at `ui.rs:59-66` is
`Length(1)/Min(0)/Length(1)` with no spare row). Near the status-bar block
(`ui.rs:344-362`), compute `let show_legend = app.selected_run().is_some_and(|r|
r.tasks.iter().any(|t| t.gate_iterations > 0 || t.review_iterations > 0));` and,
when `show_legend`, append `"  │  G = gate iterations  R = review iterations"` to
`status_text` (`ui.rs:358-359`). Use the ASCII `│` already used as the `trailer`
separator at `ui.rs:349/353` (avoid the non-ASCII `·`, since `screen_of`
flattens each cell to its first char). Render-only — no new types or fields:
`App::selected_run() -> Option<&RunView>` is at `app.rs:347`, `RunView.tasks` is
`Vec<TaskView>`, and `TaskView.gate_iterations` / `review_iterations` (`u32`) are
at `crates/makina-core/src/api.rs:173/177`.
- **Depends on:** —
- **Done when:** `#[test] fn render_status_bar_shows_gr_legend_when_counts_nonzero()`
  in the `ui.rs` tests module builds the app via `task_status_app()` (`ui.rs:1234`,
  whose tasks already carry non-zero gate/review counts, as exercised by
  `render_task_status_shows_iteration_counts` at `ui.rs:1315`), draws into
  `make_terminal(120, 30)`, and asserts `screen_of(&terminal).contains("gate
  iterations")` and `.contains("review iterations")`; a sibling `#[test] fn
  render_status_bar_hides_gr_legend_when_counts_zero()` with an all-zero-count run
  asserts `!screen.contains("gate iterations")`. `cargo test -p makina
  render_status_bar_shows_gr_legend render_status_bar_hides_gr_legend` passes.

### tui-dep-list — Dependency view mode + list rendering
Define `pub enum DependencyViewMode { Off, List, Tree, Timeline }` next to the
`Mode` enum at `crates/makina/src/app.rs:117` (mirror it: derive `Debug, Clone,
Copy, PartialEq, Eq`). Add `pub dependency_view: DependencyViewMode` to the `App`
struct (`app.rs:251`, after the `mode` field at `:264`) and initialize it to
`DependencyViewMode::Off` in `App::new` at `app.rs:322` (next to `mode:
Mode::Normal`). Add a single shared render function `fn render_dependency_view(app:
&App, frame: &mut Frame, area: Rect)` in `crates/makina/src/ui.rs` so `tui-dep-tree`
and `tui-dep-timeline` only add their match arms. In `render` (`ui.rs:54`), when
`app.dependency_view == DependencyViewMode::List`, split the existing
`exchange_area` (the `Constraint::Min(3)` region at `ui.rs:241/:247`) into a top
"Dependencies" sub-pane and the exchange pane below it, and call
`render_dependency_view` against the top chunk (this is the placement
`tui-dep-tree`/`tui-dep-timeline` also use). Render the selected task's
`depends_on` as a compact `[state] task-id` list: read `app.selected_task_id()`
(`app.rs:355`), find its `TaskView` in `app.selected_run().tasks`, then for each
id in `depends_on` look up the matching `TaskView` in the same `tasks` vec and
render `format!("{} {}", task_state_badge(&dep.state).0, dep.id.0)` per line.
`TaskView.depends_on: Vec<TaskId>` is at `crates/makina-core/src/api.rs:181`
(NOT `crates/makina/src/api.rs` — that file does not exist); `task_state_badge(&TaskState)
-> (&'static str, Color)` is at `ui.rs:674`.
- **Depends on:** —
- **Done when:** `render_dependency_list_shows_prereqs_with_badges` in the
  `crates/makina/src/ui.rs` tests module (after `ui.rs:1311`) reuses
  `task_status_app()` (`ui.rs:1234`) with `app.selected_task = Some(2)` (gamma,
  which `depends_on` beta) and `app.dependency_view = DependencyViewMode::List`,
  draws with `make_terminal(120, 30)`, captures via `screen_of`, and asserts the
  frame shows the beta dependency id and its state badge AND that the Exchange
  title/border still renders (proving no overlap); `cargo test -p makina
  render_dependency_list_shows_prereqs_with_badges` passes.

### tui-dep-toggle — Cycle the dependency view with a key
Add the `CycleDependencyView` variant to the `AppEvent` enum in
`crates/makina/src/app.rs` (enum at `app.rs:148`; place it near `FocusNext` at
`app.rs:155`, mirroring that variant's doc-comment) — the enum lives in `app.rs`,
NOT `event.rs`. Map the key in `translate_key`'s normal-mode match block in
`crates/makina/src/event.rs:354-372`: add `KeyCode::Char('v') | KeyCode::Char('V')
=> AppEvent::CycleDependencyView,` mirroring `KeyCode::Tab => AppEvent::FocusNext`
(`event.rs:359`; `v`/`V` are currently unmapped — verified). Add the `update` arm
in `App::update` (`app.rs:368`), modeled on the `FocusNext` arm (`app.rs:379-385`):
match `self.dependency_view` (the `DependencyViewMode` field added by
`tui-dep-list`) and set it to the next variant `Off → List → Tree → Timeline →
Off`, returning `true`. Confirm the four variant names against `tui-dep-list`'s
enum before writing the match. Do NOT add the `DependencyViewMode` enum, the
`App.dependency_view` field, or any rendering here — those are owned by
`tui-dep-list`.
- **Depends on:** tui-dep-list
- **Done when:** in the `event.rs` tests module, `v_translates_to_cycle_dependency_view`
  (mirroring `tab_translates_to_focus_next` at `event.rs:419-426`) asserts
  `matches!(translate_terminal_event(key_press(KeyCode::Char('v'),
  KeyModifiers::NONE), false), AppEvent::CycleDependencyView)`; in the `app.rs`
  tests module, `dependency_view_cycles` (mirroring `focus_cycles_between_panels`
  at `app.rs:670-678`) calls `app.update(AppEvent::CycleDependencyView)` four
  times and `assert_eq!`s `dependency_view` cycling `Off → List → Tree → Timeline
  → Off`; `cargo test -p makina v_translates_to_cycle_dependency_view
  dependency_view_cycles` passes.

### tui-dep-tree — Dependency tree view
Render `DependencyViewMode::Tree` by adding a `Tree` arm to the shared
`render_dependency_view` function introduced by `tui-dep-list` (in
`crates/makina/src/ui.rs`), so List and Tree share the same render surface (the
Dependencies sub-pane carved from `exchange_area` at `ui.rs:236-247`). Draw an
indented ASCII tree of the selected task's prerequisites. Algorithm: root = the
selected `TaskView` (`app.selected_run()?.tasks[app.selected_task?]`); for each
id in `root.depends_on`, look it up in `run.tasks` by `id` and emit one line
`<indent><connector>[state] <id>` where `[state]` comes from
`task_state_badge(&tv.state)` (`ui.rs:674`) and `<connector>` is `├── ` for
non-last children / `└── ` for the last child / `│   ` for carried indent;
recurse on that child's `depends_on` up to depth 2 (cap explicit). Handle a
missing/unknown id by rendering the bare id with a `[?]` badge.
`TaskView.depends_on: Vec<TaskId>` is at `crates/makina-core/src/api.rs:181`
(write the full crate-qualified path — `crates/makina/src/api.rs` does not
exist). Scope: prerequisites (forward `depends_on`) only.
- **Depends on:** tui-dep-list
- **Done when:** `#[test] fn render_dependency_tree_shows_connectors_and_badges()`
  in the `crates/makina/src/ui.rs` tests module builds an `App` whose selected
  task has `depends_on = [a (Done), b (Failed)]` and `a` `depends_on [c]`, sets
  `dependency_view = DependencyViewMode::Tree`, draws via `render()` on
  `make_terminal(...)` (`ui.rs:722`) and captures with `screen_of` (`ui.rs:1110`),
  then asserts `screen.contains("├── ")`, `screen.contains("└── ")`,
  `screen.contains("[✓ done]")` / a failed badge, the child id strings, and that
  the indent for grandchild `c` is deeper than for `a` (mirror
  `render_task_status_shows_state_badges` at `ui.rs:1296`); `cargo test -p makina
  render_dependency_tree` passes.

### tui-dep-timeline — Dependency timeline (lane) view
Render `DependencyViewMode::Timeline` by adding a `Timeline` arm to the shared
`render_dependency_view` function (`tui-dep-list`) in `crates/makina/src/ui.rs`:
a lane view over scheduling order where tasks that can run in parallel appear
side-by-side and dependents appear after their prerequisites (respecting the
DAG). There is no existing topological-sort/level helper, so compute one. Add a
free fn `fn dependency_levels(tasks: &[TaskView]) -> Vec<Vec<&TaskView>>` in
`ui.rs` that assigns each task a static longest-path level: `level(t) = 0` when
`t.depends_on` is empty, else `1 + max(level(d) for d in t.depends_on)` over the
selected run's tasks (`app.selected_run().tasks`); a `depends_on` id missing from
`tasks` is treated as level 0 (cycle/unknown guard). Render one ROW per level
(lane); tasks at the same level sit side-by-side in that row; a task always
renders in a row strictly below all of its prerequisites. Reuse the `[state] id`
badge format (`task_state_badge` at `ui.rs:674`) and draw in the same
Dependencies sub-pane as `tui-dep-list`/`tui-dep-tree`, respecting pane height.
`TaskView.depends_on: Vec<TaskId>` is at `crates/makina-core/src/api.rs:181`
(the TUI reads tasks via `App::selected_run().tasks` at `app.rs:347`, not from
`api.rs` directly).
- **Depends on:** tui-dep-list
- **Done when:** a unit test `dependency_levels_assigns_parallel_siblings_same_level`
  asserts `level(A) == level(B) == 0` and `level(C) == 1` for a fixture with A
  (no deps), B (no deps), C (`depends_on A`); and a render test
  `timeline_groups_independent_tasks_and_orders_dependents` in the
  `crates/makina/src/ui.rs` tests module sets `DependencyViewMode::Timeline`,
  draws via `render()` on `make_terminal(120, 40)`, captures with `screen_of`,
  and asserts A and B appear on the SAME terminal row (level 0) while C appears
  on a strictly LATER row (locate each id's row by scanning the `screen_of`
  lines); `cargo test -p makina ui::tests::timeline_groups_independent_tasks_and_orders_dependents
  dependency_levels_assigns_parallel_siblings_same_level` passes plus `cargo
  clippy -p makina --all-targets -- -D warnings`.

---

## 0016 — Run Lifecycle: No-Orphan Shutdown & Plan-Scoped Worktrees

Two robustness gaps surfaced by the plan-0003 dogfood. **(1) Orphaned agent
processes:** the ACP client only kills the *direct* child (grok), so grok's
descendants (its MCP servers / model workers) survive, and `Drop` never runs on
an out-of-band signal — a quit can leave live agent processes behind. **(2)
Stale-worktree poisoning:** an interrupted run leaves a `task/{id}` worktree +
branch that `WorktreeManager::create` then refuses to clobber, failing the next
run in setup and (on current code) halting the whole run. This section makes
shutdown leave **no** live agent process behind, plan-scopes the worktree/branch
namespace, and makes `create` reclaim its own stale slots.

### acp-process-group-kill — Spawn each agent in its own process group and group-kill it
Make the agent subprocess reapable as a whole tree, not just the direct child,
in `crates/makina-acp/src/client.rs`. (1) In `spawn_transport` (`client.rs:478`),
after the existing `kill_on_drop(true)` builder call (`:487`) and behind
`#[cfg(unix)]`, add `cmd.process_group(0)` (tokio's `Command` re-exposes the std
Unix builder) so the agent becomes the **leader of a fresh process group** (pgid
== its pid) that also contains every descendant it forks. (2) Add `nix = {
version = "0.29", features = ["signal"] }` to `crates/makina-acp/Cargo.toml`
`[dependencies]` (the crate depends only on `tokio`/`tokio-stream` today,
`:11-17`) for `killpg`. (3) Capture `let pgid = child.id();` right after spawn
(`:493`) and store it on `AcpClient` next to the `child: Option<Child>` field
(`:200`); replace the direct-child kill in **both** `AcpClient::shutdown`
(`client.rs:451`) and `Drop` (`client.rs:468`) with a group kill —
`nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid as i32),
Signal::SIGTERM)`, then `SIGKILL` after a short grace — falling back to
`child.start_kill()` when `pgid` is `None` or on non-Unix. Keep
`kill_on_drop(true)` as the last-resort direct-child backstop, and keep
`shutdown` idempotent (the `self.closed` guard at `:444`).
- **Depends on:** —
- **Done when:** an integration test `agent_group_kill_reaps_descendants` in
  `crates/makina-acp/tests/` spawns a helper that forks a long-lived grandchild
  (e.g. `sh -c 'sleep 300 & echo $!; wait'`), reads the grandchild pid from its
  stdout, calls `shutdown()`, and asserts the grandchild is gone
  (`nix::sys::signal::kill(grandchild_pid, None)` → `Err(Errno::ESRCH)`);
  `cargo test -p makina-acp` + `cargo clippy -p makina-acp --all-targets -- -D
  warnings` + `cargo fmt --check` pass.

### acp-agent-registry — Track live agent process groups and expose a reaper
Add a process-wide registry so any exit path can guarantee no agent survives. In
`crates/makina-acp/src/client.rs` (or a new `reaper.rs` registered in `lib.rs`),
define a private `static AGENT_PGIDS: OnceLock<Mutex<HashSet<i32>>>`. In
`spawn_transport`, insert the `pgid` (from `acp-process-group-kill`) into the set
right after spawn; in `AcpClient::shutdown`/`Drop`, remove it after the group
kill. Add `pub fn kill_all_agents()` that drains the set and `killpg(SIGKILL)`s
every remaining pgid, best-effort (ignore `ESRCH`). Export `kill_all_agents` from
`lib.rs` so the binary can call it from a **sync** context (the TUI panic hook)
and an **async** one (the signal handler). To keep the kill seam testable, route
the actual `killpg` through a small private fn so a test can substitute a
recording stub.
- **Depends on:** acp-process-group-kill
- **Done when:** a unit test exercises the registry bookkeeping against the kill
  seam: register two pgids, assert `kill_all_agents()` records both targets and
  empties the set, and that a normally-shut-down client deregisters its pgid so
  it is **not** re-killed; `cargo test -p makina-acp` + clippy + fmt pass.

### tui-exit-reaps-agents — Reap agents (and cancel runs) on every exit path
Guarantee `makina_acp::kill_all_agents()` (from `acp-agent-registry`) runs no
matter how the TUI exits. In `crates/makina/src/main.rs`: after `event::run`
returns, both on the clean path (before the final `tui.restore()`,
`main.rs:130-132`) and the error arm (`:124-128`), cancel any still-open runs
(iterate `api.runs().await` and issue `Command::CancelRun`) then call
`makina_acp::kill_all_agents()`. Spawn a `tokio::signal` task that awaits
`SIGINT`/`SIGTERM` (`tokio::signal::unix::{signal, SignalKind}`) and, on receipt,
calls `kill_all_agents()`, restores the terminal, and `std::process::exit` — so
an external `kill`/SIGTERM cannot orphan agents (the **in-TUI Ctrl-C is already a
clean quit** via `event.rs:337`, so the handler only covers out-of-band signals).
Enable tokio's `signal` feature (add it to the workspace `tokio` features /
`crates/makina/Cargo.toml:16`). In `crates/makina/src/tui.rs`, extend
`install_panic_hook` (`tui.rs:~100`) so the hook calls `kill_all_agents()` before
the terminal restore + default panic handler, so a panic mid-run leaves nothing
behind.
- **Depends on:** acp-agent-registry
- **Done when:** a unit/integration test drives the clean-exit cleanup path
  (registering a fake agent pgid, running the main-loop teardown, asserting the
  registry is drained via the `acp-agent-registry` kill seam) and a panic-hook
  test asserts the hook invokes the reaper; a **manual** acceptance note (not an
  automated gate): `Ctrl-C` and `kill <makina-pid>` during a live run leave **no**
  `grok` processes (`pgrep -f 'grok agent'` is empty). `cargo test -p makina` +
  clippy + fmt pass.

### plan-slug-derive — Derive a plan slug and thread it to the worktree layer
Add `pub fn plan_slug(task_list_path: &Path) -> String` in
`crates/makina-core/src/orchestrator.rs`, beside the `run_slug` free function
added by `mk-run-slug` (`orchestrator.rs:~97`). It returns the lowercased-kebab
of the task list's **parent directory name only** (no file stem) — e.g.
`…/0003-Runtime-and-TUI-Hardening/TASKS.md` → `0003-runtime-and-tui-hardening` —
reusing `mk-run-slug`'s kebab sanitizer; fall back to `SLUG_FALLBACK`
(`orchestrator.rs:97`) when there is no usable parent. Store it on `RunEntry`
(`orchestrator.rs:135`), set it in `open_run`'s insert, and thread it into
`DriverContext` (next to `run_slug`/`run_uid` at `supervisor.rs:448`) through the
**exact same sites** `mk-run-id` threads `run_uid` (the `start_run` tuple
`:547/:579`, `run_graph` `:750`, and both `DriverContext` constructors
`:696-709`/`:793-806`), so per-task worktree calls can read it.
- **Depends on:** mk-run-slug, mk-run-id
- **Done when:** a unit test `plan_slug_is_kebab_parent_dir` in `orchestrator.rs`
  `mod tests` asserts `plan_slug(Path::new("…/0003-Runtime-and-TUI-Hardening/TASKS.md"))
  == "0003-runtime-and-tui-hardening"` and the no-parent fallback to
  `SLUG_FALLBACK`; the workspace compiles with the new threaded field; `cargo
  test -p makina-core plan_slug` + clippy + fmt pass.

### worktree-plan-scoped-naming — Make the worktree dir + branch `{plan_slug}--{task_id}`
Plan-scope the worktree directory and branch so different plans never collide.
(1) Change `paths::worktree` (added by `mk-worktree-path`) to
`paths::worktree(repo_root: &Path, plan_slug: &str, task_id: &str) -> PathBuf` →
`.makina/worktrees/{plan_slug}--{task_id}`. (2) In
`crates/makina-core/src/worktree.rs`, change `WorktreeManager::worktree_path`
(`:313`), `create` (`:179`), and `remove` (`:255`) to take `plan_slug: &str` and
build the path via `paths::worktree` and the branch as
`format!("task/{plan_slug}--{task_id}")` (`:183`/`:259`). Keep `validate_task_id`
(`:378`) on the **clean** `task_id` only — `--` is the delimiter and never
appears inside a kebab part, so the composite is unambiguous. (3) Pass
`plan_slug` (from `plan-slug-derive`'s `DriverContext` field) at every supervisor
`create`/`remove` call site (in `task_driver` and the `DriverGuard` teardown).
Update the worktree tests to pass a sample `plan_slug` and assert the new shapes:
the in-module unit `worktree_path_is_under_repo_root` (`worktree.rs:490`), the
integration `tests/worktree.rs`, and the same suites `mk-worktree-path`
enumerates (`tests/supervisor_audit_registry.rs`, `termination_caps.rs`,
`develop_review_loop.rs`, `squash_merge.rs`).
- **Depends on:** plan-slug-derive, mk-worktree-path
- **Done when:** the unit test asserts
  `.makina/worktrees/0003-runtime-and-tui-hardening--sample-task` and the
  integration test asserts both `handle.path` and `handle.branch ==
  "task/0003-runtime-and-tui-hardening--sample-task"`, with the checkout present
  on disk before `remove` and gone after; `cargo test -p makina-core worktree` +
  the workspace suites + clippy + fmt pass.

### worktree-reclaim-on-conflict — Make `create()` reclaim a stale slot instead of erroring
Reverse the anti-clobber guards in `WorktreeManager::create`
(`worktree.rs:189-204`) into **reclaim-on-conflict** — safe now that the
`{plan_slug}--{task_id}` namespace is unambiguously Makina-owned transient state.
After the existing `git_worktree_prune` (`:187`), if the worktree path exists
**or** `branch_exists` is true (`:190`/`:199`), do **not** return
`GitCommandFailed`; instead `tracing::warn!(plan_slug, task_id, "reclaiming stale
worktree/branch from a prior interrupted run")`, call `self.remove(plan_slug,
task_id).await?` (already idempotent — worktree `remove --force`, dir cleanup,
`branch -D`, prune; `worktree.rs:255`), then fall through to the normal `git
worktree add -b {branch} {base_branch}` (`:208`) so the slot is recreated **fresh
off the current `base_branch`** (Option A — reset, not resume: the prior attempt
was never merged, so its work is throwaway). Update the `create` rustdoc
(`:159-167`) from "rather than silently clobbering" to the reclaim semantics.
This removes the stale-worktree poisoning that halts a re-run.
- **Depends on:** worktree-plan-scoped-naming
- **Done when:** a worktree test `create_reclaims_a_stale_slot` pre-creates a
  worktree+branch for `(plan_slug, task_id)`, writes a sentinel file into it, then
  calls `create()` again for the same pair and asserts it returns `Ok` with a
  fresh checkout at the same path on a branch newly cut from `base_branch` (the
  sentinel is gone), **no** `GitCommandFailed` — replacing the old
  "rejects-pre-existing" assertion; `cargo test -p makina-core worktree` + clippy
  + fmt pass.
