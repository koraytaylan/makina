# Makina Plan 0027 — Plan Auto-Discovery

Use the `docs/plans/NNNN-*/` structure as the recommended default: add a
`makina-core` helper that scans `docs/plans/*/` for the `SCOPE`/`ARCHITECTURE`/
`TASKS` convention, and surface the discovered plans on the plan-0016 sidebar
tree / file-browser open flow as the default. A dir **without** `TASKS.md` is
still listed (`has_tasks=false`) and routes to plan 0028's planner-generate.

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

## 0078 — Discover plans under `docs/plans`

### discover-plans-core — `PlanEntry` + `discover_plans(repo_root)` in makina-core

Add the scan helper next to `plan_slug` so plan-directory identity is
single-sourced.

**Steps:**

1. In `crates/makina-core/src/orchestrator.rs`, add a public
   `#[derive(Debug, Clone, PartialEq, Eq)] pub struct PlanEntry { pub dir:
   PathBuf, pub slug: String, pub has_tasks: bool }` with doc comments
   describing each field (see ARCHITECTURE).

2. Add `pub fn discover_plans(repo_root: &Path) -> Vec<PlanEntry>` that reads
   `repo_root/docs/plans/`, and for each immediate **sub-directory** that
   contains **both** `SCOPE.md` and `ARCHITECTURE.md`, pushes a `PlanEntry` with
   `dir` = the directory path, `has_tasks` = whether `dir/TASKS.md` is a file,
   and `slug` = `plan_slug(&dir.join("TASKS.md"))` (reuse the existing fn —
   `plan_slug` keys off the parent dir name, so the `TASKS.md` file need not
   exist). Skip non-directories and dirs missing the `SCOPE`+`ARCHITECTURE` pair.
   Return `vec![]` when `docs/plans` is absent or unreadable. Sort the result by
   `dir.file_name()` so `0001-…` precedes `0027-…`.

3. Add tests in `orchestrator.rs` (use `tempfile::TempDir`; create the dirs/files
   with `std::fs`):

   ```rust
   #[test]
   fn discover_plans_finds_convention_dirs() { /* tmp/docs/plans/0001-x/{SCOPE,ARCHITECTURE,TASKS}.md => one PlanEntry, has_tasks=true, slug=="0001-x", dir ends with "0001-x" */ }
   #[test]
   fn dir_without_tasks_flagged() { /* tmp/docs/plans/0002-y/{SCOPE,ARCHITECTURE}.md (no TASKS.md) => PlanEntry has_tasks==false, slug still =="0002-y" (== plan_slug of dir/TASKS.md) */ }
   #[test]
   fn non_plan_dirs_ignored() { /* tmp/docs/plans/assets/ with a stray foo.png (no SCOPE/ARCHITECTURE) => not present; and a missing docs/plans => discover_plans(...) == vec![] */ }
   ```

- **Depends on:** —
- **Done when:** the three tests pass; `discover_plans` lists only convention
  dirs (both `SCOPE.md` + `ARCHITECTURE.md` present), flags `has_tasks` from the
  presence of `TASKS.md`, derives `slug` via `plan_slug`, sorts by directory
  name, and returns `vec![]` with no `docs/plans`; `cargo test`/clippy/fmt green.

### plan-picker-state — Hold discovered plans on `App` and enter the picker

Carry the discovery result in app state and add a modal `PlanPicker` mode that
reuses the plan-0016 "Runs & Tasks" sidebar surface.

**Steps:**

1. In `crates/makina/src/app.rs`, add `PlanPicker` to `enum Mode` (`app.rs:374`,
   alongside `FileBrowser`/`ProviderConfig`/`Doctor`). Add fields to `App`
   (`app.rs:614`): `pub discovered_plans: Vec<makina_core::orchestrator::PlanEntry>`
   and `pub plan_cursor: usize`; initialise both in `App::new` (`app.rs:925`) as
   `vec![]` / `0` (and thus via `App::with_config`, `app.rs:987`, which calls
   `new`).

2. Add `pub fn selected_plan(&self) -> Option<&makina_core::orchestrator::PlanEntry>`
   returning `self.discovered_plans.get(self.plan_cursor)`.

3. Add `AppEvent::PlansDiscovered { plans: Vec<makina_core::orchestrator::PlanEntry> }`
   to `enum AppEvent` (`app.rs:459`). Handle it in `App::update` (`app.rs:1295`
   region): store `plans`, set `plan_cursor = 0`, set `mode = Mode::PlanPicker`,
   return `true`. Add `AppEvent::PlanPickerUp` / `PlanPickerDown` that clamp
   `plan_cursor` within `0..discovered_plans.len()` (mirror `BrowserUp`/
   `BrowserDown`, `app.rs:1306`). Ensure `CloseBrowser` (`app.rs:1326`) also
   exits `Mode::PlanPicker` back to `Mode::Normal` (clearing nothing else), or
   add a parallel `ClosePlanPicker`; pick one and keep Esc handling consistent.

4. Add tests in `app.rs`:

   ```rust
   #[test]
   fn plans_discovered_enters_picker() { /* App::update(PlansDiscovered{ two entries }) => mode==Mode::PlanPicker, discovered_plans.len()==2, plan_cursor==0, selected_plan()==first */ }
   #[test]
   fn plan_picker_cursor_clamps() { /* PlanPickerDown past end clamps to len-1; PlanPickerUp at 0 stays 0; selected_plan() tracks the cursor */ }
   ```

- **Depends on:** discover-plans-core
- **Done when:** the two tests pass; `Mode::PlanPicker` and the
  `discovered_plans`/`plan_cursor` state exist and are constructed in every `App`
  ctor; `PlansDiscovered` enters the picker and `selected_plan()` tracks a clamped
  cursor; `cargo test`/clippy/fmt green.

### plan-picker-io — Default open to discovery; activate routes by `has_tasks`

Wire the open flow to discovery and route activation to `OpenRun` (with tasks) or
the planner-generate seam (without).

**Steps:**

1. In `crates/makina/src/event.rs`, in `resolve_io`'s `AppEvent::OpenBrowser` arm
   (`event.rs:238`), first call
   `makina_core::orchestrator::discover_plans(&app.repo_root)`. If the result is
   **non-empty**, return `(AppEvent::PlansDiscovered { plans }, None)`; otherwise
   keep the existing `read_dir_event(&start)` CWD browse (`event.rs:240`) as the
   fallback so non-convention repos behave exactly as today.

2. Add `AppEvent::PlanActivate` to `enum AppEvent` and handle it in `resolve_io`:
   resolve `app.selected_plan()`. For a `has_tasks==true` entry,
   `app.api.execute(makina_core::api::Command::OpenRun { task_list_path:
   entry.dir.join("TASKS.md") })` then return `(AppEvent::CloseBrowser,
   Some("Interpreting {slug}..."))` (mirroring the file path at `event.rs:265`,
   mapping `Err` to `"Open failed: {e}"`). For a `has_tasks==false` entry, **do
   not** call `OpenRun`: return `(AppEvent::CloseBrowser, Some("{slug}: no
   TASKS.md — planner will generate the graph"))` and leave a single
   `// plan 0028: planner-generate(entry.dir)` seam comment at that call site.

3. In `crates/makina/src/event.rs`'s key resolution (the browser keymap block,
   `event.rs:701`), when `app.mode == Mode::PlanPicker`: map `Up`/`k` →
   `PlanPickerUp`, `Down`/`j` → `PlanPickerDown`, `Enter` → `PlanActivate`, `Esc`
   → `CloseBrowser` (close, do not quit). Leave `Ctrl+C` quit and the normal-mode
   keymap untouched.

4. Add tests in `event.rs` (reuse the `CoreApi`-recorder pattern from the
   existing `browser_activate_file_opens_run_via_core_api_and_appears_in_app`
   test, `event.rs:1346`):

   ```rust
   #[tokio::test]
   async fn open_browser_prefers_discovered_plans() { /* App with repo_root = a tmp repo containing docs/plans/0001-x with all three files; resolve_io(OpenBrowser) yields AppEvent::PlansDiscovered with one entry (not a BrowserOpened) */ }
   #[tokio::test]
   async fn plan_activate_with_tasks_opens_run() { /* App in Mode::PlanPicker, selected_plan has_tasks=true; resolve_io(PlanActivate) executes OpenRun{ dir/TASKS.md } on the recorder and yields CloseBrowser + Interpreting status */ }
   #[tokio::test]
   async fn plan_activate_without_tasks_does_not_open_run() { /* selected_plan has_tasks=false; resolve_io(PlanActivate) records NO OpenRun and yields CloseBrowser + a "planner will generate" status */ }
   ```

- **Depends on:** plan-picker-state
- **Done when:** the three tests pass; pressing `o` in a repo with `docs/plans/`
  shows the discovered plans (and falls back to the CWD browser when there are
  none); activating a plan with a `TASKS.md` opens it via `OpenRun`; activating
  one without a `TASKS.md` does not call `OpenRun` and surfaces the
  planner-generate status; `cargo test`/clippy/fmt green.

### render-plan-picker — Draw the discovered plans on the sidebar surface

Render the picker so the discovered plans are visible and navigable.

**Steps:**

1. In `crates/makina/src/ui.rs`, when `app.mode == Mode::PlanPicker`, render the
   `app.discovered_plans` as a `List` titled `"Plans"` over the sidebar surface
   (reuse `panel_block` and the cyan highlight style the plan-0016 sidebar /
   provider editor already use). Each row shows the entry `slug`; rows with
   `!has_tasks` append a dim `(no tasks — will plan)` suffix. Highlight the row
   at `app.plan_cursor` (seed the `ListState` from `plan_cursor`). Preserve an
   empty-state hint (`No plans under docs/plans …`) when `discovered_plans` is
   empty.

2. Keep the existing file-browser overlay rendering unchanged; `Mode::PlanPicker`
   is a sibling overlay branch, not a replacement.

3. Add a test in `ui.rs` (render to a test `Buffer`, like the existing sidebar
   render tests):

   ```rust
   #[test]
   fn plan_picker_renders_slugs_and_no_tasks_hint() { /* App in Mode::PlanPicker with two entries (one has_tasks, one not); render; assert the buffer contains "Plans", both slugs, and the "(no tasks — will plan)" hint on the second */ }
   ```

- **Depends on:** plan-picker-state
- **Done when:** the test passes; the picker lists discovered plan slugs with the
  `has_tasks=false` hint, highlights `plan_cursor`, and shows the empty-state hint
  when there are no plans; `cargo test`/clippy/fmt green.

### plan-discovery-docs — Document the convention and the open default

**Steps:**

1. Add a short section to the repo docs (e.g. the runtime/TUI doc under `docs/`
   that already describes the open flow, or `docs/plans/0027-Plan-Auto-Discovery/
   SCOPE.md`'s referenced behaviour) stating: Makina recommends the
   `docs/plans/NNNN-*/` layout (`SCOPE.md` + `ARCHITECTURE.md` + `TASKS.md`); on
   open (`o`), Makina auto-discovers those dirs and offers them as the default;
   a plan dir without `TASKS.md` is listed and routes to the planner-generate
   path; repos without `docs/plans/` fall back to the file browser.

2. Cross-link `discover_plans` / `PlanEntry` (the `makina-core` symbols) so a
   reader can find the implementation.

- **Depends on:** plan-picker-io, render-plan-picker
- **Done when:** the named doc exists and accurately describes the implemented
  discovery + open default (convention, `has_tasks` routing, browser fallback);
  no behavioural code changes; `cargo test`/clippy/fmt green.

---

**End of plan 0027 TASKS.** When every "Done when" bullet is green, pressing `o`
in a repo that uses the recommended `docs/plans/NNNN-*/` layout surfaces those
plans as the default open target — opening the ones that have a `TASKS.md` and
routing the ones that don't to the planner — instead of dropping the user into a
bare file browser at the CWD.
