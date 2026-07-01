# Scope — Plan 0043

> Enable Makina to manage multiple project folders with persistent workspace state at the user home level, and initialize new folders with a git + `docs/plans/` structure whose README is a self-explanatory plan-authoring guide.

## Why this plan

**1. Current architecture assumes a single project folder at launch.** `crates/makina/src/main.rs:95` hardcodes `repo_root = std::env::current_dir()`, and `crates/makina/src/app.rs:1427` holds a single `repo_root: PathBuf`. The TreeNode enum at `crates/makina/src/app.rs:724–734` has only Plan, PlanTask, Run, and Task variants — no Folder level. Plan discovery at `crates/makina-core/src/orchestrator.rs` takes a single repo_root and searches `docs/plans/` relative to it. The state_root at `crates/makina-core/src/paths.rs:190–197` derives the project namespace from one repo_root; there is no workspace-level persistence for multiple folders.

**2. Users cannot open additional folders or persist their workspace.** The user brief requires (a) a 3-level sidebar tree showing FOLDER → PLAN → TASK, (b) command-palette actions to Open and Close folders with a file explorer, (c) persistent workspace state at the user home level surviving restarts, and (d) auto-discovery of the launch CWD if it's a Makina-ready git repo. Currently none of these exist — there is no workspace persistence layer, no multi-folder state tracking, and no folder-picker UI.

**3. Uninitialized folders are not discoverable or actionable.** The user brief specifies (e) an "uninitialized folder" state in the sidebar tree distinct from folders with plans, and (f) an "Initialize Folder" command to bootstrap git + docs/plans structure. There is no such state or command — folders are either not opened, or they are opened and have plans. There is no way to initialize an empty folder.

**4. New folders need an unambiguous plan-authoring guide.** Plan authoring stays external: users already run their agent CLI (e.g. `claude`) for ACP, and in-app authoring would require a follow-up-question / iterative-refinement chat loop, which is a non-goal. But an initialized folder must tell that external model exactly how to author a plan Makina can parse. The user brief requires (g) that Initialize Folder write a `docs/plans/README.md` authoring guide complete enough to leave no guesswork — not an in-app Create Plan command.

## In scope

Work items in [TASKS.md](TASKS.md) (workstreams 0001–0007):

- **0001 — Workspace Persistence and Multi-Folder State.** Add a persistent workspace config at `$HOME/.makina/workspace.toml` tracking opened folders. Implement an OpenedFolders app struct, load workspace state on startup, and save it whenever folders are opened, closed, or initialized. Ensure the workspace survives restarts and the app can recover its folder set on relaunch.
- **0002 — Sidebar Tree Refactor to 3-Level Hierarchy.** Extend the TreeNode enum to add a Folder variant at the top level of the hierarchy. Refactor visible_tree_nodes() to flatten the Folder → Plan → Task structure. Rewire the tree keyboard/mouse handlers (Space/Enter/focus) for the new folder-scoped variants so the tree is navigable, not just rendered. Update sidebar rendering to show folder names with expand/collapse affordances and proper indentation.
- **0003 — Command Palette Extensions.** Add three new command-palette actions: Open Folder (triggers a file-picker modal), Close Folder (lists opened folders and removes selected one), and Initialize Folder (checks/creates git and docs/plans structure).
- **0004 — Folder Browser and File-Picker UI.** Implement a file-picker modal for folder selection, similar to the existing FileBrowser used for opening task lists. Wire it to the Open Folder action so users can navigate and select a folder to add to the workspace.
- **0005 — Folder Initialization Flow.** Implement the Initialize Folder command: check whether the folder has .git (create if absent), create an empty initial commit so a branch can be born, ensure the main branch exists, create a develop branch if missing, create docs/plans/ containing a COMPLETE authoring-guide README (written from a committed template describing the four-file layout and the exact TASKS.md contract), and persist the folder to the workspace.
- **0006 — Plan Auto-Discovery Per Folder.** Extend plan discovery to iterate over all opened folders instead of just repo_root. Return a map of folder index to plans discovered in that folder. Update sidebar rendering to scope plans by their parent folder and to draw a folder that has no plans in a distinct empty/uninitialized style.
- **0007 — Multi-Folder Workflow Integration Verification.** Add an end-to-end integration test that exercises the multi-folder happy path (open, close, initialize folders; per-folder plan discovery; sidebar rendering; workspace persistence across restarts) once workstreams 0001–0006 have landed.

## Origin -> workstream mapping

| Finding | Addressed by |
|---|---|
| Single repo_root assumption blocks multi-folder support | `0001` |
| No workspace persistence layer for opened folders | `0001` |
| TreeNode enum lacks Folder level for 3-level tree | `0002` |
| visible_tree_nodes() flattens only Plan/PlanTask/Run/Task, not Folder level | `0002` |
| Command palette has no Open/Close/Initialize Folder actions | `0003` |
| No file-picker UI for folder selection; Open Folder action missing | `0004` |
| No Folder initialization command or git bootstrap logic | `0005` |
| Initialized folders need an unambiguous plan-authoring guide (README) | `0005` |
| discover_plans() takes single repo_root; does not scope per folder | `0006` |
| Multi-folder workflow has no end-to-end regression coverage | `0007` |

## Locked decisions

- **Multi-folder state persists at `$HOME/.makina/workspace.toml`, not in repo.** The user brief specifies that workspace state (the set of opened folders) must survive restarts. Persisting at the user home level (`$HOME/.makina/workspace.toml`) keeps the workspace independent of any single project and allows users to have different folder sets for different machines. The workspace file is a TOML dict of opened folder paths; it is loaded on startup and saved on every Open/Close/Initialize operation. If the file does not exist, the app creates it on first save.
- **TreeNode enum includes both legacy variants (Plan, PlanTask) and new folder-scoped variants (Folder, PlanInFolder, PlanTaskInFolder) for backward-compat.** The existing visible_tree_nodes() logic and sidebar rendering handle the legacy Plan/PlanTask variants. To avoid rewriting the entire sidebar and tree-navigation logic at once, the new variants coexist with the old ones. Over time, the old variants can be phased out. The legacy variants are no longer generated by the new visible_tree_nodes() logic, but they remain in the enum to avoid breaking any pattern matches in tests or other code.
- **Folder initialization is idempotent and does not fail if .git, branches, or docs/plans already exist.** The Initialize Folder command must be safe to run multiple times. The folder_init::initialize_folder() function checks for the existence of .git, main, and develop before creating them. If all are present, it succeeds with no changes (a no-op). This allows users to initialize the same folder twice without error and makes recovery from partial initialization straightforward.
- **Plan discovery is scoped per folder; plans in different folders are completely independent.** The discover_plans_per_folder() function returns a HashMap<folder_idx, Vec<PlanEntry>> so that plans are keyed by their parent folder. A plan slug (e.g., '0001-Initial') can appear in multiple opened folders without collision — the sidebar tree distinguishes them by nesting under their respective folders. The app stores plans in plans_by_folder, keyed by folder_idx, so no plan collisions are possible.
- **The folder browser reuses existing UI patterns (Mode enum, modal dispatch).** The FolderBrowser mode extends the existing Mode enum and reuses the FileBrowser navigation (up/down, Enter, Esc), minimizing code duplication and keeping the modal pattern consistent.

## Out of scope

- **In-app plan creation (and any chat / Q&A UI).** Makina does not author plans itself. Authoring is inherently interactive — follow-up questions, decisions, iterative refinement — which would require a chat system (a non-goal), and users already have an agent CLI for ACP. Initialize Folder instead writes a `docs/plans/README.md` authoring guide, so plans are authored externally with that CLI and simply discovered by Makina.
- **Moving or renaming opened folders.** The workspace stores absolute paths; if a folder is moved, it will not be found on restart. Users must Close and re-Open the folder. Path canonicalization and symlink resolution are deferred.
- **Validating opened-folder paths (e.g., checking ownership, permissions, or disk availability).** The app assumes all opened folders are readable and writable. Permission errors will surface when discovery or initialization runs. Proactive validation (with a doctor or preflight check) is deferred.
- **UI customization of folder/plan names or tree display options.** The sidebar renders folder paths and plan slugs as-is. Custom naming, abbreviation, or reordering of folders in the sidebar are deferred.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the concrete edits.
See [TASKS.md](TASKS.md) for the executable task list with "Done when" criteria.
