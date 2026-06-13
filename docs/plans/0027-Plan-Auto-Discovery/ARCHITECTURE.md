# Architecture — Plan 0027

> The concrete deltas. Line numbers are hints; locate every site by the named
> symbol (grep). This plan adds one helper in `makina-core` and wires it into
> the `makina` (TUI) open flow.

## Current shape (what exists)

- **Slug derivation** (`crates/makina-core/src/orchestrator.rs`): `run_slug(&Path)`
  (`orchestrator.rs:120`) and `plan_slug(&Path)` (`orchestrator.rs:163`) derive a
  kebab id from a `TASKS.md` path's **parent directory name**
  (e.g. `…/0027-Plan-Auto-Discovery/TASKS.md` → `0027-plan-auto-discovery`). Both
  are `pub` and share a `sanitize_kebab` helper. There is **no** scan helper that
  enumerates plan directories.
- **Open command** (`crates/makina-core/src/api.rs`): `Command::OpenRun {
  task_list_path: PathBuf }` (`api.rs:344`) reads + interprets that file and
  registers a Run; `RunView` (`api.rs:284`) carries `task_list_path` and a derived
  `project` label.
- **File browser** (`crates/makina/src/browser.rs`): `FileBrowser { cwd, entries:
  Vec<DirEntry>, selected }` is pure view state; `DirEntry { name, path, is_dir }`.
  All `read_dir` IO lives in the event loop, not here.
- **Open flow** (`crates/makina/src/event.rs`): `resolve_io` handles
  `AppEvent::OpenBrowser` by reading `std::env::current_dir()` via
  `read_dir_event` (`event.rs:238,240,599`) and emitting `AppEvent::BrowserOpened`;
  `AppEvent::BrowserActivate` on a **file** calls
  `api.execute(Command::OpenRun { task_list_path: entry.path })` then
  `CloseBrowser` (`event.rs:248,265`).
- **App state** (`crates/makina/src/app.rs`): `App { mode: Mode, browser:
  Option<FileBrowser>, runs: Vec<RunView>, repo_root: PathBuf, … }`
  (`app.rs:614,627,636,656,764`); `enum Mode { Normal, FileBrowser, ProviderConfig,
  Doctor }` (`app.rs:374`). The plan-0016 sidebar tree is already present:
  `enum TreeNode { Run { run }, Task { run, task } }` and
  `App::visible_tree_nodes() -> Vec<TreeNode>` (`app.rs:809`), with
  `collapsed_runs: HashSet<RunId>` and `tree_cursor` (`app.rs:671,673`).
- **Events** (`crates/makina/src/app.rs`): `enum AppEvent` (`app.rs:459`) with
  `OpenBrowser`, `BrowserOpened { dir, entries }`, `BrowserActivate`,
  `CloseBrowser` (`app.rs:509,512,528,536`), each routed through `App::update`
  (`app.rs:1295`).

## 0078 — Discover plans under `docs/plans`

### `discover_plans` in `makina-core`

New `pub` items in `crates/makina-core/src/orchestrator.rs` (beside `plan_slug`,
reusing it for the slug so identity stays single-sourced).

- **`PlanEntry`.** A discovered plan directory:

  ```rust
  /// One plan directory discovered under `docs/plans/`.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct PlanEntry {
      /// Absolute path to the plan directory (e.g. `…/docs/plans/0027-Plan-Auto-Discovery`).
      pub dir: PathBuf,
      /// The plan slug `plan_slug(dir/TASKS.md)` derives (e.g. `0027-plan-auto-discovery`).
      pub slug: String,
      /// `true` when the dir contains a `TASKS.md` (openable via `OpenRun`);
      /// `false` routes to the planner-generate path (plan 0028).
      pub has_tasks: bool,
  }
  ```

- **`discover_plans`.** Scan `repo_root/docs/plans/*/` for the convention:

  ```rust
  /// Scan `repo_root/docs/plans/*/` for plan directories following the
  /// `SCOPE.md` / `ARCHITECTURE.md` / `TASKS.md` convention.
  ///
  /// A directory is a plan iff it contains **both** `SCOPE.md` and
  /// `ARCHITECTURE.md`. `TASKS.md` is optional and recorded as
  /// [`PlanEntry::has_tasks`]. Returns entries sorted by directory name
  /// (so `0001-…` precedes `0027-…`). A missing `docs/plans` yields `vec![]`.
  pub fn discover_plans(repo_root: &Path) -> Vec<PlanEntry> {
      let plans_root = repo_root.join("docs").join("plans");
      let mut entries = Vec::new();
      let Ok(rd) = std::fs::read_dir(&plans_root) else {
          return entries; // no docs/plans → nothing discovered
      };
      for ent in rd.flatten() {
          let dir = ent.path();
          if !dir.is_dir() {
              continue;
          }
          // Convention gate: SCOPE.md AND ARCHITECTURE.md must both exist.
          if !dir.join("SCOPE.md").is_file() || !dir.join("ARCHITECTURE.md").is_file() {
              continue; // non-plan dirs (assets/, etc.) are ignored
          }
          let tasks = dir.join("TASKS.md");
          let has_tasks = tasks.is_file();
          // Slug is exactly what plan_slug derives from this dir's TASKS.md path,
          // whether or not the file exists (plan_slug keys off the parent dir name).
          let slug = plan_slug(&tasks);
          entries.push(PlanEntry { dir, slug, has_tasks });
      }
      entries.sort_by(|a, b| a.dir.file_name().cmp(&b.dir.file_name()));
      entries
  }
  ```

  Notes: `plan_slug` keys off the **parent directory name** of the path it is
  given (`orchestrator.rs:163`), so passing `dir/TASKS.md` derives the right slug
  even for `has_tasks=false` dirs (the file need not exist). The convention gate
  is `SCOPE.md` + `ARCHITECTURE.md` so a folder of assets is never mistaken for a
  plan. Re-export from `lib.rs` is automatic (the module is already `pub mod
  orchestrator`, `lib.rs:21`).

### Surface discovered plans in the TUI open flow

Edits in `crates/makina/src/event.rs` and `crates/makina/src/app.rs`.

- **Default the browser to discovered plans.** In `resolve_io`'s
  `AppEvent::OpenBrowser` arm (`event.rs:238`), instead of unconditionally reading
  the process CWD, first call `makina_core::orchestrator::discover_plans(&app
  .repo_root)`. When it returns a non-empty list, emit a new
  `AppEvent::PlansDiscovered { plans }` (carry the `Vec<PlanEntry>`); when it is
  empty, fall back to the existing `read_dir_event(&start)` browse exactly as
  today. This keeps the raw browser as the escape hatch for repos that do not use
  the convention.

  ```rust
  AppEvent::OpenBrowser => {
      let plans = makina_core::orchestrator::discover_plans(&app.repo_root);
      if plans.is_empty() {
          let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
          (read_dir_event(&start).await, None)
      } else {
          (AppEvent::PlansDiscovered { plans }, None)
      }
  }
  ```

- **Hold discovered plans on `App`.** Add a `Mode::PlanPicker` variant
  (`app.rs:374`) and a `pub discovered_plans: Vec<PlanEntry>` field on `App`
  (`app.rs:614`, initialised `vec![]` in `App::new`, `app.rs:925`), plus a
  cursor `pub plan_cursor: usize`. Handle `AppEvent::PlansDiscovered { plans }`
  in `App::update` (`app.rs:1295` region) by storing the list, resetting the
  cursor, and switching to `Mode::PlanPicker`. The picker reuses the sidebar
  tree's "Plans" affordance: render the `discovered_plans` list with each entry's
  `slug` and a `(no tasks — will plan)` hint when `!has_tasks`.

- **Activate a plan.** Add `AppEvent::PlanActivate` (bound to Enter while
  `Mode::PlanPicker`, mirroring `BrowserActivate`'s key in `event.rs`). In
  `resolve_io`, resolve the cursor's `PlanEntry`:
  - **`has_tasks == true`:** `api.execute(Command::OpenRun { task_list_path:
    entry.dir.join("TASKS.md") })` then `CloseBrowser`, exactly the existing file
    path in `event.rs:265` (status `Interpreting {slug}…`).
  - **`has_tasks == false`:** route to plan 0028's planner-generate entry instead
    of `OpenRun`. Until 0028 lands, this emits a status message
    `No TASKS.md — planner will generate the graph` and leaves a single seam
    (a `// plan 0028: planner-generate(entry.dir)` call site) so 0028 wires the
    generate command without touching discovery.

  ```rust
  AppEvent::PlanActivate => match app.selected_plan() {
      Some(entry) if entry.has_tasks => {
          let path = entry.dir.join("TASKS.md");
          let status = format!("Interpreting {}...", entry.slug);
          let result = app.api.execute(Command::OpenRun { task_list_path: path }).await;
          let msg = result.err().map(|e| format!("Open failed: {e}")).unwrap_or(status);
          (AppEvent::CloseBrowser, Some(msg))
      }
      Some(entry) => {
          // plan 0028: planner-generate(entry.dir) — route here instead of OpenRun.
          (AppEvent::CloseBrowser, Some(format!(
              "{}: no TASKS.md — planner will generate the graph", entry.slug)))
      }
      None => (AppEvent::Tick, None),
  }
  ```

- **`App::selected_plan`.** Add `pub fn selected_plan(&self) ->
  Option<&PlanEntry>` (`discovered_plans.get(plan_cursor)`) and reuse the existing
  browser Up/Down keymap (`event.rs` browser keymap block) to move `plan_cursor`
  while `Mode::PlanPicker`, clamping to `discovered_plans.len()`.

## Testing notes

- `discover_plans` is filesystem logic: build a `tempfile::TempDir` with a
  `docs/plans/` containing (a) a convention dir with all three files, (b) a
  convention dir with `SCOPE.md`+`ARCHITECTURE.md` but **no** `TASKS.md`, and
  (c) a non-plan dir (e.g. `assets/` with a stray `.png`). Assert the first is
  `has_tasks=true`, the second `has_tasks=false`, the third is **absent**, the
  slugs equal `plan_slug(dir/TASKS.md)`, and a missing `docs/plans` yields `vec![]`.
- TUI wiring: drive `AppEvent::PlansDiscovered`/`PlanActivate` through
  `App::update`/`resolve_io` against a fixture `App`; assert `Mode::PlanPicker` is
  entered, that activating a `has_tasks=true` entry issues `OpenRun{ dir/TASKS.md }`
  (via the existing `CoreApi`-style test recorder used by the browser tests in
  `event.rs`), and that a `has_tasks=false` entry does **not** call `OpenRun`.
