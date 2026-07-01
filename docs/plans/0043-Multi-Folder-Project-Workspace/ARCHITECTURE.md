# Architecture — Plan 0043 (deltas)

> The concrete deltas. This plan adds two new modules —
> `crates/makina/src/workspace.rs` and `crates/makina/src/folder_init.rs` — plus a
> committed authoring-guide template `crates/makina/src/templates/plans_readme.md`,
> and touches `crates/makina/src/main.rs`, `crates/makina/src/lib.rs`,
> `crates/makina/src/app.rs`, `crates/makina/src/event.rs`,
> `crates/makina/src/browser.rs`, `crates/makina/src/ui.rs`,
> `crates/makina/Cargo.toml`, and `crates/makina-core/src/orchestrator.rs`.
> It also adds an integration test under `crates/makina/tests/`.
> Line numbers are hints; locate every site by the named symbol (grep).

## 0001 — Workspace-Persistence-And-Multi-Folder-State

Today Makina has no workspace-level state. `crates/makina/src/main.rs:95`
computes `repo_root = std::env::current_dir()` once at startup, and the `App`
struct holds a single immutable `repo_root: PathBuf`
(`crates/makina/src/app.rs:1427`, set from the `App::new()` /
`App::with_config()` constructors at `app.rs:1956` / `app.rs:2060`). There is no
mechanism to persist a *set* of opened folders across restarts, so a user's
choice of which projects to work on is lost on every relaunch.

**Edits:**

**Add a `workspace` module (`crates/makina/src/workspace.rs`, `lib.rs`,
`Cargo.toml`).** Introduce a `Workspace` struct holding
`opened_folders: HashSet<PathBuf>` with `load()` / `save()` / `add_folder()` /
`remove_folder()` and a `workspace_path()` helper resolving
`dirs::home_dir()/.makina/workspace.toml`. `load()` returns an empty `Workspace`
when the file is absent (new user); `save()` writes atomically (temp file +
rename) via the `toml` crate. Declare `pub mod workspace;` in `lib.rs`; add
`toml` and `dirs` to `Cargo.toml` if not already present.

**Load the workspace at startup (`main.rs`).** After config loading (~line 95),
load the workspace, auto-discover the launch CWD if it is a Makina-ready git repo
(`.git` and `docs/plans` both present), collect `opened_folders`, and pass the
vector plus the `Workspace` handle into the constructor.

**Hold workspace state on `App` (`app.rs`).** Add `pub opened_folders:
Vec<PathBuf>` and `pub workspace: workspace::Workspace` to the `App` struct
(`app.rs:1246`), initialize them in the constructor, and extend
`App::with_config()` to accept the `opened_folders` vector.

## 0002 — Sidebar-Tree-Refactor-To-3-Level-Hierarchy

Today the sidebar is two levels. The `TreeNode` enum
(`crates/makina/src/app.rs:724`) has only `Run`, `Task`, `Plan`, and `PlanTask`
variants, and `visible_tree_nodes()` (`app.rs:1609`) flattens a single global
`discovered_plans` list — plans then runs — with collapse state keyed by a flat
`collapsed_plans: HashSet<usize>` (`app.rs:1314`). There is no Folder level, so
plans from different folders cannot be nested or distinguished.

**Edits:**

**Extend `TreeNode` with folder-scoped variants (`app.rs`).** Add `Folder {
folder_idx }`, `PlanInFolder { folder_idx, plan_idx }`, and `PlanTaskInFolder {
folder_idx, plan_idx, task_idx }` alongside the retained legacy `Plan` /
`PlanTask` variants (kept for backward-compat with existing pattern matches and
tests). Add `pub collapsed_folders: HashSet<usize>` and `pub plans_by_folder:
HashMap<usize, Vec<makina_core::orchestrator::PlanEntry>>` to the `App` struct
and initialize both empty in the constructor (~`app.rs:1994`, near the existing
`collapsed_plans: HashSet::new()`).

**Rewrite `visible_tree_nodes()` (`app.rs:1609`).** Iterate `opened_folders`
first — push a `Folder` node, and when the folder is not in `collapsed_folders`,
push its `PlanInFolder` children from `plans_by_folder` (reusing the existing
run-slug dedup against `self.runs`), then their `PlanTaskInFolder` children when
the plan is expanded. Append open runs and their tasks afterward, unchanged. All
call sites (sidebar render, tree cursor, selection handlers) must still compile.

**Rewire the tree interaction handlers (`app.rs`).** Emitting the new variants is
not enough — the keyboard/mouse handlers must act on them. Add match arms for
`Folder`, `PlanInFolder`, and `PlanTaskInFolder` to the existing expand/collapse
handler (mutating `collapsed_folders` / `collapsed_plans` near `app.rs:1851`), the
activate-on-Enter handler (opening a folder-scoped plan/task from
`plans_by_folder`), and the focus-left/right handlers — keeping the legacy arms
intact so existing behavior and tests are unaffected. Without this the 3-level tree
renders but cannot be navigated.

## 0003 — Command-Palette-Extensions

Today `CommandPalette::default_actions()` (`crates/makina/src/app.rs:471`)
returns a fixed list of palette actions, and the `AppEvent` enum
(`app.rs:743`) has no variants for folder operations. There is no way to open,
close, or initialize a folder from the palette.

**Edits:**

**Add `AppEvent` variants (`app.rs:743`).** Add `OpenFolder`,
`OpenFolderSelected { path }`, `CloseFolderRequested`, `CloseFolderConfirmed {
path }`, `InitializeFolderRequested`, and `InitializeFolderSelected { path }`.
These are declaration-only here; the exhaustiveness gaps are closed by the
handlers in later workstreams.

**Add three palette actions (`app.rs:471`).** Append `PaletteAction::Regular`
entries labelled "Open Folder", "Close Folder", and "Initialize Folder", each
dispatching the matching `AppEvent`, grouped after the existing folder-adjacent
items so palette filtering by `open`/`close`/`init` surfaces them.

## 0004 — Folder-Browser-And-File-Picker-UI

Today `FileBrowser` (`crates/makina/src/browser.rs:45`, constructed via
`FileBrowser::new()` at `browser.rs:59`) is used only to pick a task-list *file*,
and the `Mode` enum (`crates/makina/src/app.rs:409`) has no state for a
directory picker. There is no way to navigate the filesystem and select a
*folder* to add to the workspace.

**Edits:**

**Add a `FolderBrowser` mode (`app.rs`).** Extend the `Mode` enum
(`app.rs:409`) with `FolderBrowser { purpose: FolderBrowserPurpose }` and define
`FolderBrowserPurpose { OpenFolder, InitializeFolder }` so the same picker serves
both flows.

**Reuse `FileBrowser` for directories (`browser.rs`).** Extend `FileBrowser`
(or add a thin wrapper) to start from a given root (e.g. `$HOME`), show only
directories, and, on Enter, yield the selected directory path rather than a file.

**Wire the open flow (`event.rs`).** In the event loop, handle
`AppEvent::OpenFolder` by switching to `Mode::FolderBrowser { purpose:
OpenFolder }` rooted at `dirs::home_dir()`, and dispatch
`AppEvent::OpenFolderSelected { path }` when the user confirms a directory; Esc
returns to `Mode::Normal`.

**Handle open/close (`event.rs`).** In `resolve_io()`
(`crates/makina/src/event.rs:278`) / the update path, handle
`OpenFolderSelected` (validate it is a directory, push to `opened_folders`, save
the workspace, trigger rediscovery via the pre-existing `spawn_discover_plans`
helper at `event.rs:497`, return to Normal) and `CloseFolderConfirmed` (remove
from `opened_folders`, save, rediscover).

## 0005 — Folder-Initialization-Flow

Today there is no way to make an empty directory Makina-ready from inside the
app. Discovery requires a `docs/plans/` tree with convention files, and the
orchestrator's `discover_plans()` (`crates/makina-core/src/orchestrator.rs:403`)
silently skips folders that lack it — so an uninitialized folder is simply
invisible, with no command to bootstrap `.git`, branches, or `docs/plans/`.

**Edits:**

**Add a `folder_init` module (`crates/makina/src/folder_init.rs`, `lib.rs`).**
Export `initialize_folder(folder: &Path) -> Result<(), String>` that, via a small
`run_git()` helper wrapping `std::process::Command`, ensures `.git` exists
(`git init`); then, when HEAD is unborn (a fresh repo has no commit, so no branch
can be created until one exists), creates an **empty initial commit** — falling
back to a repo-local git identity if none is configured — and names that branch
`main` (`git branch -M main`); ensures a `develop` branch exists (create only when
absent); and creates `docs/plans/` with a `README.md` that is a COMPLETE
plan-authoring guide, written verbatim from a committed template
(`crates/makina/src/templates/plans_readme.md`, `include_str!`'d) so it documents
the four-file layout and the exact `TASKS.md` heading / `Depends on` / `Done when`
contract with no guesswork. Every step is guarded by an existence check
(`rev-parse --verify --quiet`) so the function is idempotent. Declare
`pub mod folder_init;` in `lib.rs`.

**Wire the init flow (`event.rs`).** Handle
`AppEvent::InitializeFolderRequested` by opening `Mode::FolderBrowser { purpose:
InitializeFolder }`, and `AppEvent::InitializeFolderSelected { path }` by calling
`folder_init::initialize_folder(&path)`; on success add the folder to
`opened_folders`, save the workspace, trigger rediscovery, and set a status
message; on error push an `ErrorMessage` and return to Normal.

## 0006 — Plan-Auto-Discovery-Per-Folder

Today `orchestrator::discover_plans(repo_root)`
(`crates/makina-core/src/orchestrator.rs:403`) searches exactly one
`repo_root/docs/plans/` and returns a flat `Vec<PlanEntry>` (`PlanEntry` defined
at `orchestrator.rs:219`). In `event.rs`, `spawn_discover_plans()`
(`crates/makina/src/event.rs:497`) calls it and emits
`AppEvent::PlansDiscovered { plans }` (`event.rs:514`). Nothing scopes plans to a
folder, so multiple opened folders cannot each carry their own plan set.

**Edits:**

**Add `discover_plans_per_folder()` (`orchestrator.rs`).** Add a public function
taking `&[PathBuf]` and returning `HashMap<usize, Vec<PlanEntry>>` — for each
`(folder_idx, folder)`, run the existing per-folder discovery logic against
`folder/docs/plans/` (gated on `SCOPE.md` and `ARCHITECTURE.md` both existing)
and insert the result under `folder_idx`, always inserting an entry (empty vec
when the folder has no plans). Keep `discover_plans()` for existing call sites.

**Emit a per-folder discovery event (`event.rs`, `app.rs`).** Update
`spawn_discover_plans()` to accept `opened_folders: Vec<PathBuf>`, call
`discover_plans_per_folder()`, and emit a new `AppEvent::PlansDiscoveredPerFolder
{ plans_map }`. Handle that event in `App::update()` by assigning
`plans_by_folder` and seeding `collapsed_folders` with every returned
`folder_idx` (folders start collapsed). Update all call sites to pass
`opened_folders`.

**Render the 3-level tree (`ui.rs`).** Update the sidebar renderer (the
`TreeNode::Plan` / `TreeNode::PlanTask` arms at `crates/makina/src/ui.rs:582` /
`ui.rs:628`) to also draw `Folder` at depth 0 with a `[+]`/`[-]` collapse
affordance — or, when the folder has no plans, a DISTINCT dimmed
`(empty — Initialize Folder)` style — `PlanInFolder` at depth 1, and
`PlanTaskInFolder` at depth 2. (The Space/Enter/focus interaction wiring for these
variants lives in `rewire-tree-navigation-for-folders`, workstream 0002; this is
rendering only.)

## 0007 — Multi-Folder-Workflow-Integration-Verification

Today none of the multi-folder behavior is covered end-to-end. The unit tests
exercise pieces (`discover_plans` in `orchestrator.rs`, sidebar rendering in
`ui.rs`), but nothing asserts the *composed* workflow — open, close, and
initialize folders; per-folder discovery; sidebar rendering; and workspace
persistence across a simulated restart — so a regression that only appears when
the pieces are wired together would slip through.

**Edits:**

**Add `crates/makina/tests/multi_folder_integration_test.rs`.** Create two temp
directories initialized as Makina-ready folders, then simulate: Open Folder A and
B (assert the sidebar shows two folders with their plans), Initialize Folder
(assert `.git`, branches, and `docs/plans/` appear), Close Folder (assert removal
from the tree), and a restart (assert `workspace.toml` was written and reloads
the same folder set). Use `MockApi` / a test fixture so no live agent is needed.

## Properties that make this safe

- **Additive, backward-compatible enums.** The new `TreeNode` folder variants
  (`Folder`, `PlanInFolder`, `PlanTaskInFolder`), `AppEvent` variants, and `Mode`
  variants are added alongside the existing ones; the legacy `Plan`/`PlanTask`
  variants and `discover_plans(repo_root)` are retained, so existing pattern
  matches, call sites, and tests keep compiling and behaving as before.
- **Empty-state and existence guards everywhere.** `Workspace::load()` returns an
  empty set for a missing file, `discover_plans_per_folder()` always inserts an
  entry per folder (empty vec when none), and `folder_init::initialize_folder()`
  guards each `.git` / branch / `docs/plans` step on prior existence — so a fresh
  user, a folder with no plans, and a re-initialized folder are all no-error paths
  and initialization is idempotent.
- **Home-level persistence is independent of any repo.** Opened folders live in
  `$HOME/.makina/workspace.toml`, written atomically (temp + rename), so a
  crashed save never corrupts the workspace and the folder set is decoupled from
  any single project's committed state.
- **Discovery is folder-scoped and collision-free.** `plans_by_folder` keys plans
  by `folder_idx`, so identical plan slugs in different folders never collide and
  the sidebar distinguishes them structurally by nesting.
- **The workstream ids are 1:1 across SCOPE, ARCHITECTURE, and TASKS** (0001–0007,
  each id used exactly once), and every "Done when" in TASKS.md closes on the same
  gate commands (`cargo test`, `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --check`), so the plan parses under the implement-plan contract and
  its acceptance criteria are uniformly falsifiable.
