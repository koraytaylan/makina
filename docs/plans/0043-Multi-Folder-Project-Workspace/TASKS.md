# XAgent Plan 0043 — Multi-Folder Project Workspace

Refactor Makina's architecture from a single repo_root to support multiple concurrently opened folders: persist the set of opened folders in `$HOME/.makina/workspace.toml`, add a 3-level sidebar tree (Folder → Plan → Task), extend the command palette with Open/Close/Initialize Folder actions, implement a file-picker modal for folder selection, auto-discover and scope plans per folder instead of globally, and initialize uninitialized folders with a git + `docs/plans/` structure whose `README.md` is a complete plan-authoring guide. (Plans are authored with the user's existing agent CLI — which they already have for ACP — not inside Makina; in-app authoring would need a chat/Q&A loop, a non-goal.)

See [SCOPE.md](SCOPE.md) for boundaries and [ARCHITECTURE.md](ARCHITECTURE.md) for the deltas.

**Conventions**
- Each task has a stable kebab-case **id** (also its branch `task/{id}`).
- **Depends on** lists *direct* prerequisites only; "—" means none.
- **Done when** is the verifiable criterion, and every task keeps the gate
  commands green — the full forms are `cargo test`,
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`;
  abbreviated as "cargo test/clippy/fmt green" thereafter.
- GPU tests self-skip without an adapter.
- Line numbers are hints; locate every site by the named symbol (grep).

---

## 0001 — Workspace Persistence and Multi-Folder State

### add-workspace-persistence — Implement workspace persistence layer at $HOME/.makina/workspace.toml

The app currently has no workspace-level state. It accepts a single `repo_root` at startup (`main.rs:95`) and holds it immutably (`app.rs:1427`). There is no mechanism to persist a set of opened folders across restarts. The user brief requires persistent multi-folder state at the home level.

To enable plan discovery, sidebar rendering, and state management across multiple folders, we must first add a workspace persistence layer that survives restarts.

**Steps:**

1. Create `crates/makina/src/workspace.rs` with a `Workspace` struct holding `opened_folders: HashSet<PathBuf>`. Implement `load()` to read from `$HOME/.makina/workspace.toml` (using `toml` crate for serialization; add `toml` to Cargo.toml if not present), `save()` to write it back atomically, `add_folder()`, and `remove_folder()` methods. The load() method should gracefully return an empty Workspace if the file does not exist (new user).
2. Declare the module in `crates/makina/src/lib.rs` as `pub mod workspace;`.
3. Add `dirs` crate to dependencies (for home_dir) if not present. The workspace_path() helper should return `home_dir()/.makina/workspace.toml`.
4. In `crates/makina/src/main.rs` after config loading (line ~95), add:
   ```rust
   let mut workspace = workspace::Workspace::load()
     .unwrap_or_else(|e| {
       eprintln!("failed to load workspace: {e}");
       workspace::Workspace::new()
     });

   // Auto-discover: if CWD is a git repo with docs/plans, add it to opened_folders.
   if current_dir.join(".git").exists() && current_dir.join("docs/plans").exists() {
     workspace.add_folder(current_dir.clone());
   }
   let opened_folders: Vec<PathBuf> = workspace.opened_folders.iter().cloned().collect();
   ```
5. Update `App::with_config()` signature to accept `opened_folders: Vec<PathBuf>`. Add `app.opened_folders = opened_folders;` in the constructor. In `main.rs`, pass `opened_folders` to `with_config()`.
6. Add to `App` struct (`app.rs:1246`) a new field:
   ```rust
   /// Opened folders persisted in workspace.
   pub opened_folders: Vec<PathBuf>,

   /// Workspace state for save/load.
   pub workspace: workspace::Workspace,
   ```

- **Depends on:** —
- **Done when:** The workspace module loads/saves correctly; `cargo test` confirms `Workspace::load()` returns an empty set for a nonexistent file and `Workspace::save()` creates the file atomically; the app loads opened_folders on startup and stores the workspace for later saves; cargo test, clippy, and fmt all pass green.

---

## 0002 — Sidebar Tree Refactor to 3-Level Hierarchy

### extend-treenode-enum — Extend TreeNode enum to add Folder variant for 3-level hierarchy

The TreeNode enum at `crates/makina/src/app.rs:724–734` currently has only four variants: Run, Task, Plan, PlanTask. The sidebar renders two levels (plans and runs with their tasks). The user brief requires a 3-level tree: Folder → Plan → Task. To support this, TreeNode must gain a Folder variant and a way to scope plans to their parent folder.

**Steps:**

1. Locate TreeNode enum at `crates/makina/src/app.rs:724`. Extend it with new variants:
   ```rust
   pub enum TreeNode {
     /// A folder at the top level (index into App::opened_folders).
     Folder { folder_idx: usize },
     /// A plan discovered under opened_folders[folder_idx].
     PlanInFolder { folder_idx: usize, plan_idx: usize },
     /// A task preview under an expanded plan in a folder.
     PlanTaskInFolder { folder_idx: usize, plan_idx: usize, task_idx: usize },
     /// The run at `runs[run]`.
     Run { run: usize },
     /// Task at `runs[run].tasks[task]`.
     Task { run: usize, task: usize },
     /// Legacy: a discovered plan (for backward compat).
     Plan { plan_idx: usize },
     /// Legacy: a task preview under an expanded plan.
     PlanTask { plan_idx: usize, task_idx: usize },
   }
   ```
2. Add to the App struct (`app.rs:1246`) two new fields:
   ```rust
   /// Folder indices currently collapsed in the sidebar tree.
   pub collapsed_folders: HashSet<usize>,

   /// Plans scoped to each folder: folder_idx → list of PlanEntry.
   pub plans_by_folder: HashMap<usize, Vec<makina_core::orchestrator::PlanEntry>>,
   ```
3. Initialize `collapsed_folders` as empty and `plans_by_folder` as empty in `App::new()` (line ~1956).
4. Ensure the impl blocks for TreeNode handle pattern matching on all variants (compile errors will guide this; no logic change needed yet, just exhaustiveness).

- **Depends on:** —
- **Done when:** TreeNode compiles with all seven variants; App struct holds collapsed_folders and plans_by_folder; no compilation errors; cargo test, clippy, and fmt all pass green.

---

### refactor-visible-tree-nodes — Refactor visible_tree_nodes() to flatten Folder → Plan → Task hierarchy

The `visible_tree_nodes()` method at `crates/makina/src/app.rs:1609` currently flattens Plan and PlanTask from a single discovered_plans list, then adds open runs and their tasks. With the new Folder variant and per-folder plan discovery (plans_by_folder), the method must iterate folders first, then their plans, then tasks.

**Steps:**

1. Locate `visible_tree_nodes()` at `crates/makina/src/app.rs:1609`. Rewrite the logic:
   ```rust
   pub fn visible_tree_nodes(&self) -> Vec<TreeNode> {
     let mut nodes = Vec::new();

     // Step 1: Add discovered folders and their plans (unchanged run dedup logic).
     let run_slugs: HashSet<String> = self.runs.iter()
       .map(|r| makina_core::orchestrator::plan_slug(&r.task_list_path))
       .collect();

     for folder_idx in 0..self.opened_folders.len() {
       nodes.push(TreeNode::Folder { folder_idx });

       // Only expand this folder if not collapsed.
       if !self.collapsed_folders.contains(&folder_idx) {
         if let Some(plans) = self.plans_by_folder.get(&folder_idx) {
           for (local_plan_idx, plan) in plans.iter().enumerate() {
             // Skip if this plan has an open Run (same dedup as before).
             if run_slugs.contains(&plan.slug) {
               continue;
             }
             nodes.push(TreeNode::PlanInFolder { folder_idx, plan_idx: local_plan_idx });

             // Expand plan tasks if not collapsed.
             if !self.collapsed_plans.contains(&(folder_idx * 1000 + local_plan_idx)) {
               for task_idx in 0..plan.tasks.len() {
                 nodes.push(TreeNode::PlanTaskInFolder {
                   folder_idx,
                   plan_idx: local_plan_idx,
                   task_idx,
                 });
               }
             }
           }
         }
       }
     }

     // Step 2: Add open runs and their expanded tasks (unchanged).
     for (run_idx, run) in self.runs.iter().enumerate() {
       // ... existing run dedup and rendering logic ...
     }

     nodes
   }
   ```
   (Note: The collapsed_plans keying must be updated to avoid collision — using folder_idx * 1000 + plan_idx is a temporary workaround; a better approach is a custom key type, but this is good enough for the plan.)
2. Verify that all call sites of `visible_tree_nodes()` (sidebar rendering, tree cursor logic, selection handlers) still compile after the refactor. No logic change is needed; the rendering will be updated in a downstream task.

- **Depends on:** extend-treenode-enum
- **Done when:** visible_tree_nodes() returns a Vec<TreeNode> with folders at depth 0, plans in their folders at depth 1, and tasks at depth 2; the method compiles and has no logic errors (verified with existing tests); cargo test, clippy, and fmt all pass green.

---

### rewire-tree-navigation-for-folders — Rewire tree keyboard/mouse handlers for the folder-scoped TreeNode variants

`refactor-visible-tree-nodes` makes `visible_tree_nodes()` emit the new `Folder`, `PlanInFolder`, and `PlanTaskInFolder` variants, but the tree INTERACTION handlers in `crates/makina/src/app.rs` still only match the legacy `Plan` / `PlanTask` / `Run` / `Task` variants. Until they handle the new variants the 3-level tree is inert: Space does not expand/collapse folders or folder-scoped plans, and Enter does not open a folder-scoped plan or task — so the sidebar renders but cannot be driven.

**Steps:**

1. Grep `app.rs` for the existing `TreeNode::Plan` / `TreeNode::PlanTask` match arms — the expand/collapse handler that mutates `collapsed_plans` (around `app.rs:1851`), the activate/open-on-Enter handler, and the focus-left/right handlers. Add arms for the new variants (keep the legacy arms intact):
   - **Expand/collapse (Space):** `Folder { folder_idx }` toggles membership in `collapsed_folders`; `PlanInFolder { folder_idx, plan_idx }` toggles the SAME folder-scoped collapse key that `visible_tree_nodes()` uses (e.g. `folder_idx * 1000 + plan_idx`) in `collapsed_plans`.
   - **Activate/open (Enter):** `PlanInFolder` opens the plan-detail view sourced from `plans_by_folder[folder_idx][plan_idx]` (NOT the legacy `discovered_plans`); `PlanTaskInFolder` opens that task's preview; `Folder` expands/collapses (same as Space).
   - **Focus-right / focus-left:** a collapsed `Folder` or `PlanInFolder` expands on focus-right; an expanded one collapses on focus-left; a `PlanTaskInFolder` leaf collapses its parent plan and parks the cursor on the plan header.
2. After a collapse/expand toggle, re-park the cursor on the header node (mirror the existing `move_cursor_to_plan_header` behavior for the folder-scoped variants) so the selection does not jump.

- **Depends on:** refactor-visible-tree-nodes
- **Done when:** Space on a `Folder` node toggles its expansion (its `PlanInFolder` children appear/disappear in `visible_tree_nodes()`); Space on a `PlanInFolder` toggles its `PlanTaskInFolder` previews; Enter on a `PlanInFolder` opens the plan detail sourced from `plans_by_folder`; focus-left/right expand and collapse the folder-scoped nodes; unit tests assert `focused_node()` and the post-toggle `visible_tree_nodes()` for the folder-scoped variants (not only the legacy ones); cargo test, clippy, and fmt all pass green.

---

## 0003 — Command Palette Extensions

### extend-appevent-for-folders — Add AppEvent variants for folder operations

The command palette will dispatch new actions (Open Folder, Close Folder, Initialize Folder) that do not yet have corresponding AppEvent variants. These variants are needed to wire palette actions through the event loop to their handlers in resolve_io.

**Steps:**

1. Locate the AppEvent enum in `crates/makina/src/app.rs` (around line ~743). Add new variants:
   ```rust
   /// User requested to open a folder via palette action.
   OpenFolder,

   /// User selected a folder path to open (from file picker).
   OpenFolderSelected { path: std::path::PathBuf },

   /// User requested to close a folder via palette action.
   CloseFolderRequested,

   /// User selected a folder path to close (from list).
   CloseFolderConfirmed { path: std::path::PathBuf },

   /// User requested to initialize a folder via palette action.
   InitializeFolderRequested,

   /// User selected a folder path to initialize (from file picker).
   InitializeFolderSelected { path: std::path::PathBuf },
   ```

- **Depends on:** —
- **Done when:** All six new AppEvent variants (OpenFolder, OpenFolderSelected, CloseFolderRequested, CloseFolderConfirmed, InitializeFolderRequested, InitializeFolderSelected) are defined in the AppEvent enum; the code compiles (no exhaustiveness errors in pattern matches yet, as they will be handled in resolve_io); cargo test, clippy, and fmt all pass green.

---

### add-palette-actions — Add three new folder actions to CommandPalette::default_actions()

The CommandPalette::default_actions() method at `crates/makina/src/app.rs:471–525` returns a static list of 13 actions. Three new actions (Open Folder, Close Folder, Initialize Folder) must be added and wired to their corresponding AppEvents.

**Steps:**

1. Locate CommandPalette::default_actions() at `crates/makina/src/app.rs:471`. Add three new PaletteAction::Regular entries to the returned vec (order them after the existing folder-related palette items for logical grouping):
   ```rust
   PaletteAction::Regular {
     label: "Open Folder",
     event: AppEvent::OpenFolder,
   },
   PaletteAction::Regular {
     label: "Close Folder",
     event: AppEvent::CloseFolderRequested,
   },
   PaletteAction::Regular {
     label: "Initialize Folder",
     event: AppEvent::InitializeFolderRequested,
   },
   ```

- **Depends on:** extend-appevent-for-folders, refactor-visible-tree-nodes
- **Done when:** CommandPalette::default_actions() includes the three new actions; filtering by 'open', 'close', or 'init' in the palette shows the respective action; cargo test, clippy, and fmt all pass green.

---

## 0004 — Folder Browser and File-Picker UI

### implement-folder-browser-mode — Add Mode::FolderBrowser variant and file-picker modal for folders

The FileBrowser struct at `crates/makina/src/browser.rs` is used for opening task-list files. For folder selection, we need a similar modal that (a) shows a directory tree starting from $HOME, (b) filters to directories only, and (c) emits AppEvent::OpenFolderSelected on selection. Rather than duplicate FileBrowser, we can extend it or create a thin wrapper.

Alternatively, we can reuse FileBrowser with a filter to show only directories and a different event dispatch on Enter.

**Steps:**

1. Add a `FolderBrowser` variant to the EXISTING `Mode` enum (`pub enum Mode` at
   `crates/makina/src/app.rs:409`). Add ONLY the new variant — do not remove the
   enum's other variants (`Normal`, `FileBrowser`, `ProviderConfig`, `Doctor`,
   `CommandPalette`, `Settings`, `ResetConfirm`, `OperationNotice`) — and add the
   new `FolderBrowserPurpose` enum alongside it:
   ```rust
   // add this one variant inside `enum Mode` (keep all the others):
   /// The modal directory browser for picking a folder to open or initialize.
   FolderBrowser { purpose: FolderBrowserPurpose },

   // new enum, declared next to `Mode`:
   pub enum FolderBrowserPurpose {
       OpenFolder,
       InitializeFolder,
   }
   ```
2. In `crates/makina/src/browser.rs`, extend the FileBrowser struct or create a new struct that can be used for folder selection. The key behavior: (a) start from a given root directory (e.g., $HOME), (b) show only directories in the tree (skip files), (c) on Enter, emit a path to the selected directory.
3. In `crates/makina/src/event.rs`, add handlers in the event loop for AppEvent::OpenFolder and AppEvent::InitializeFolderRequested:
   ```rust
   AppEvent::OpenFolder => {
     let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
     app.mode = Mode::FolderBrowser { purpose: FolderBrowserPurpose::OpenFolder };
     app.browser = Some(FileBrowser::new(home, true)); // true = folders only
   }
   ```
4. When the user navigates in FolderBrowser and presses Enter on a selected folder, dispatch AppEvent::OpenFolderSelected { path: <selected> }.

- **Depends on:** extend-appevent-for-folders
- **Done when:** AppEvent::OpenFolder opens a FolderBrowser mode at $HOME; the browser shows only directories and responds to arrow keys (up/down) and Enter; pressing Enter on a folder emits AppEvent::OpenFolderSelected with the folder path; pressing Esc returns to Normal mode; cargo test, clippy, and fmt all pass green.

---

### handle-folder-open-close-events — Implement handlers for OpenFolderSelected and CloseFolderConfirmed in event.rs

The AppEvent::OpenFolderSelected and AppEvent::CloseFolderConfirmed events are dispatched from the file-picker modals, but there are no handlers in `crates/makina/src/event.rs` to process them. These handlers must update app.opened_folders, save the workspace, trigger plan discovery, and return to Normal mode.

**Steps:**

1. In `crates/makina/src/event.rs` resolve_io() or the main event-update logic, add:
   ```rust
   AppEvent::OpenFolderSelected { path } => {
     // Verify it's a valid directory.
     if !path.is_dir() {
       app.push_error(ErrorMessage { ... });
       return; // User tried to select a non-directory; stay in FolderBrowser.
     }

     // Add to opened_folders if not already present.
     if !app.opened_folders.contains(&path) {
       app.opened_folders.push(path.clone());
     }

     // Save workspace.
     if let Err(e) = app.workspace.save() {
       app.push_error(ErrorMessage { text: format!("failed to save workspace: {e}"), ... });
     }

     // Trigger plan discovery. `spawn_discover_plans` already exists in event.rs
     // today with the SINGLE-PATH signature `(repo_root: PathBuf, tx, bool)`, so
     // call THAT form here — passing a `Vec<PathBuf>` would not type-check until
     // update-discovery-event-and-handler migrates the signature and rewrites every
     // call site (including this one) to pass `app.opened_folders`. That task
     // depends on this one, so it runs afterward and completes the migration.
     spawn_discover_plans(app.repo_root.clone(), background_tx.clone(), false);

     // Return to Normal mode.
     app.mode = Mode::Normal;
   }
   ```
2. Similarly, for CloseFolderRequested, show a list of opened_folders (as a multi-select or single-select modal) and emit CloseFolderConfirmed when the user selects one.
3. In the CloseFolderConfirmed handler, remove the folder from app.opened_folders, save workspace, and trigger rediscovery.

- **Depends on:** implement-folder-browser-mode, add-workspace-persistence
- **Done when:** AppEvent::OpenFolderSelected adds the selected folder to app.opened_folders, saves workspace, and triggers plan discovery; AppEvent::CloseFolderRequested shows a selectable list of opened folders; CloseFolderConfirmed removes the folder, saves workspace, and triggers rediscovery; the app returns to Normal mode after each operation; status message is shown (e.g., 'Folder opened' or 'Folder closed'); cargo test, clippy, and fmt all pass green.

---

## 0005 — Folder Initialization Flow

### create-folder-init-module — Create folder_init.rs module with git bootstrap and docs/plans setup

The Initialize Folder command must check for `.git` (create if absent), create an **empty initial commit** so a branch can be born (a fresh `git init` leaves HEAD unborn, and `git branch` cannot create a branch when there is no commit to point at), ensure the `main` and `develop` branches exist, and create `docs/plans/` containing a complete authoring-guide `README.md`. That README is the plan-authoring surface Makina ships: there is no in-app plan creation (authoring needs follow-up questions and iterative refinement — i.e. a chat system, which is a non-goal, and users already run their agent CLI for ACP anyway). Instead the guide tells any model exactly how to author a plan Makina can consume, with no guesswork. This logic should be in a separate module for clarity and testability.

**Steps:**

1. Create `crates/makina/src/folder_init.rs` with the following functions:
   ```rust
   use std::path::Path;
   use std::process::Command;

   pub fn initialize_folder(folder: &Path) -> Result<(), String> {
     // 1. Ensure a git repository exists.
     if !folder.join(".git").exists() {
       run_git(folder, &["init"])?;
     }

     // 2. Ensure there is at least one commit, on `main`. A fresh `git init`
     //    leaves HEAD unborn — no commit exists, so NO branch can be created yet
     //    (`git branch`/`checkout -b` need a commit to point at). Bootstrap only
     //    when HEAD is unborn, so re-running on an initialized repo is a no-op.
     if run_git(folder, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
       // A commit needs an author identity; fall back to a repo-local one when the
       // machine has no global git identity configured (otherwise the commit errors).
       if run_git(folder, &["config", "user.email"]).is_err() {
         run_git(folder, &["config", "user.email", "makina@localhost"])?;
         run_git(folder, &["config", "user.name", "Makina"])?;
       }
       run_git(folder, &["commit", "--allow-empty", "-m", "chore: initialize repository"])?;
       // Name the initial branch `main` regardless of the user's init.defaultBranch.
       run_git(folder, &["branch", "-M", "main"])?;
     }

     // 3. Ensure a `develop` branch exists (created off main, only when absent).
     if run_git(folder, &["rev-parse", "--verify", "--quiet", "develop"]).is_err() {
       run_git(folder, &["branch", "develop"])?;
     }

     // 4. Create docs/plans/ and write the authoring-guide README verbatim from a
     //    committed template (see step 2). include_str! keeps the guide in a real
     //    Markdown file humans can edit and writes it with correct newlines.
     let plans_dir = folder.join("docs").join("plans");
     std::fs::create_dir_all(&plans_dir)
       .map_err(|e| format!("failed to create docs/plans: {e}"))?;
     const PLANS_README: &str = include_str!("templates/plans_readme.md");
     std::fs::write(plans_dir.join("README.md"), PLANS_README)
       .map_err(|e| format!("failed to write docs/plans/README.md: {e}"))?;

     Ok(())
   }

   fn run_git(folder: &Path, args: &[&str]) -> Result<(), String> {
     let output = Command::new("git")
       .args(args)
       .current_dir(folder)
       .output()
       .map_err(|e| format!("failed to run git: {e}"))?;
     if !output.status.success() {
       let stderr = String::from_utf8_lossy(&output.stderr);
       return Err(format!("git failed: {}", stderr));
     }
     Ok(())
   }
   ```
2. Create the committed template `crates/makina/src/templates/plans_readme.md` — the authoring guide `include_str!`'d above. It MUST describe the plan format precisely enough that a model authoring a plan from it needs no guesswork. Author it with this content (adjust wording, but keep every rule):
   ````markdown
   # Plans

   This directory holds the project's implementation plans. A plan is authored by a
   human — typically with an agent CLI such as the `create-plan` skill — and then
   executed by Makina. Makina does not author plans itself; it reads plans that
   follow the format below.

   ## Layout

   One directory per plan, named `NNNN-Title-Case-Kebab/`, where `NNNN` is a
   zero-padded 4-digit number one greater than the highest existing plan (the first
   plan is `0001`). Each plan directory contains exactly four files:

   - `SCOPE.md` — why the plan exists, what is in scope, what is explicitly out of
     scope, and any locked decisions.
   - `ARCHITECTURE.md` — the concrete code deltas, grouped by workstream, ideally
     with real `path/to/file.rs:line` anchors.
   - `TASKS.md` — the executable task list (contract below).
   - `STATUS.md` — a status marker (`📋 Planned`, `🚧 In progress`, or `✅ Done`) and a
     table mapping each workstream to its task ids.

   ## TASKS.md contract (follow exactly — Makina parses this mechanically)

   - A single `# ` title line at the top.
   - Workstreams are `## NNNN — Workstream Title` headings.
   - Each task is a `### {kebab-id} — {Title}` heading. The separator between id and
     title is: space, EM DASH (`—`, U+2014), space — NOT a hyphen.
   - `{kebab-id}` is a unique, stable, lowercase-kebab identifier; it also names the
     task's git branch.
   - After the task's prose (and an optional `**Steps:**` list), every task ends with
     exactly these two bullets:
     - `- **Depends on:** id-a, id-b` — the comma-separated ids of this task's DIRECT
       prerequisites, or a single `—` if it has none.
     - `- **Done when:** <criterion>` — one falsifiable completion criterion that
       includes the project's quality gates passing.
   - The `Depends on` edges must form a directed acyclic graph, and tasks must appear
     in a valid topological order: every id referenced in a `Depends on` line must
     belong to a task defined EARLIER in the file.

   ## Gates

   Every task's `Done when` must keep the project's quality gates green. State the
   exact gate commands in `ARCHITECTURE.md` (for a Rust project, typically
   `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`).
   ````
3. Declare the module in `crates/makina/src/lib.rs` as `pub mod folder_init;`.

- **Depends on:** —
- **Done when:** The folder_init module exports initialize_folder(folder) which returns Result<(), String>; on success the folder has `.git`, at least one commit on `main` (an empty initial commit when the repo was freshly created), a `develop` branch, and a `docs/plans/README.md` that is the FULL authoring guide written with real newlines (it documents the four-file layout AND the exact `### {id} — {title}` / `Depends on` / `Done when` TASKS.md contract — not a stub); the function is idempotent (running it twice succeeds both times and makes no further commits/branches); `cargo test` includes a test on a temp directory asserting `git rev-parse HEAD` resolves to a commit, `git branch` lists both `main` and `develop`, and the written README contains the headings `## Layout` and `## TASKS.md contract` and does not contain the literal substring `\n`; cargo test, clippy, and fmt all pass green.

---

### handle-initialize-folder-event — Implement InitializeFolderSelected handler in event.rs

AppEvent::InitializeFolderRequested and AppEvent::InitializeFolderSelected are dispatched from the file-picker modal, but there is no handler to invoke the folder_init logic and update app state.

**Steps:**

1. In `crates/makina/src/event.rs`, add handlers:
   ```rust
   AppEvent::InitializeFolderRequested => {
     // Show FolderBrowser with purpose InitializeFolder.
     let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
     app.mode = Mode::FolderBrowser { purpose: FolderBrowserPurpose::InitializeFolder };
     app.browser = Some(FileBrowser::new(home, true));
   }

   AppEvent::InitializeFolderSelected { path } => {
     // Call folder_init::initialize_folder().
     match folder_init::initialize_folder(&path) {
       Ok(_) => {
         // Add to opened_folders if not present.
         if !app.opened_folders.contains(&path) {
           app.opened_folders.push(path.clone());
         }

         // Save workspace.
         if let Err(e) = app.workspace.save() {
           app.push_error(...);
         }

         // Trigger plan discovery via the current single-path helper; the
         // per-folder (Vec) migration is done by update-discovery-event-and-handler.
         spawn_discover_plans(app.repo_root.clone(), background_tx.clone(), false);

         app.mode = Mode::Normal;
         app.status_message = Some(format!("Folder initialized: {}", path.display()));
       }
       Err(e) => {
         app.push_error(ErrorMessage { text: format!("Failed to initialize folder: {e}"), ... });
         // Return to Normal mode; user can try again.
         app.mode = Mode::Normal;
       }
     }
   }
   ```

- **Depends on:** create-folder-init-module, implement-folder-browser-mode, add-workspace-persistence
- **Done when:** AppEvent::InitializeFolderRequested opens a FolderBrowser; AppEvent::InitializeFolderSelected calls folder_init::initialize_folder(), adds the folder to opened_folders, saves workspace, and triggers plan discovery; on success, status message shows 'Folder initialized: <path>'; on error, an error message is shown in the error pane; cargo test, clippy, and fmt all pass green.

---

## 0006 — Plan Auto-Discovery Per Folder

### add-discover-plans-per-folder — Add discover_plans_per_folder() to orchestrator.rs

The `orchestrator::discover_plans(repo_root)` function at `crates/makina-core/src/orchestrator.rs` searches a single repo_root's docs/plans/ directory. To support multi-folder discovery (workstream 0006), we need a new function that takes a slice of opened_folders and returns a HashMap mapping folder_idx to discovered plans.

**Steps:**

1. In `crates/makina-core/src/orchestrator.rs`, add a new public function:
   ```rust
   pub fn discover_plans_per_folder(
     opened_folders: &[std::path::PathBuf]
   ) -> std::collections::HashMap<usize, Vec<PlanEntry>> {
     let mut result = std::collections::HashMap::new();
     for (folder_idx, folder) in opened_folders.iter().enumerate() {
       let plans_root = folder.join("docs").join("plans");
       let mut entries = Vec::new();
       let Ok(rd) = std::fs::read_dir(&plans_root) else {
         result.insert(folder_idx, entries);
         continue;
       };
       for ent in rd.flatten() {
         let dir = ent.path();
         if !dir.is_dir() {
           continue;
         }
         // Convention gate: SCOPE.md AND ARCHITECTURE.md must both exist.
         if !dir.join("SCOPE.md").is_file() || !dir.join("ARCHITECTURE.md").is_file() {
           continue;
         }
         // ... rest of the existing discover_plans logic adapted for this folder ...
         // (parse task preview, build PlanEntry, push to entries)
       }
       result.insert(folder_idx, entries);
     }
     result
   }
   ```
   (Copy the existing discover_plans() logic verbatim but iterate folder-scoped and insert into the HashMap.)
2. Keep the existing `discover_plans(repo_root)` for backward compat with existing call sites.

- **Depends on:** —
- **Done when:** discover_plans_per_folder() takes &[PathBuf], returns HashMap<usize, Vec<PlanEntry>>, and correctly discovers plans in each folder; the returned HashMap has an entry for every folder_idx (even if plans is empty); `cargo test` confirms the function works on multiple folders; cargo test, clippy, and fmt all pass green.

---

### update-discovery-event-and-handler — Update spawn_discover_plans() to use discover_plans_per_folder()

The `spawn_discover_plans()` function in `event.rs` currently calls `discover_plans(repo_root)` and emits AppEvent::PlansDiscovered. It must be updated to call discover_plans_per_folder() with app.opened_folders and emit a new event variant containing the HashMap.

**Steps:**

1. In `crates/makina/src/event.rs`, locate spawn_discover_plans() (around line ~497). Update it:
   ```rust
   fn spawn_discover_plans(
     opened_folders: Vec<std::path::PathBuf>,
     background_tx: mpsc::Sender<AppEvent>,
     fallback_to_browser: bool,
   ) {
     tokio::spawn(async move {
       let plans_map = tokio::task::spawn_blocking(move || {
         makina_core::orchestrator::discover_plans_per_folder(&opened_folders)
       })
       .await
       .unwrap_or_default();

       let event = if plans_map.is_empty() && fallback_to_browser {
         // ... existing fallback-to-browser logic ...
       } else {
         AppEvent::PlansDiscoveredPerFolder { plans_map }
       };
       let _ = background_tx.send(event).await;
     });
   }
   ```
2. Update AppEvent enum to add (or rename) the variant:
   ```rust
   PlansDiscoveredPerFolder {
     plans_map: std::collections::HashMap<usize, Vec<makina_core::orchestrator::PlanEntry>>,
   }
   ```
3. Update the handler in app.rs for this event:
   ```rust
   AppEvent::PlansDiscoveredPerFolder { plans_map } => {
     app.plans_by_folder = plans_map;
     // Start every folder collapsed EXCEPT the first (folder_idx 0). The
     // auto-discovered launch folder / sole opened folder stays expanded so the
     // single-folder launch case keeps its plans immediately visible — matching
     // today's behavior and preserving the "run inside a repo" experience.
     for folder_idx in app.plans_by_folder.keys().copied() {
       if folder_idx != 0 {
         app.collapsed_folders.insert(folder_idx);
       }
     }
   }
   ```
4. Update all call sites of spawn_discover_plans() to pass app.opened_folders instead of repo_root.

- **Depends on:** add-discover-plans-per-folder, extend-treenode-enum, refactor-visible-tree-nodes, add-palette-actions, implement-folder-browser-mode, handle-initialize-folder-event, handle-folder-open-close-events
- **Done when:** spawn_discover_plans() takes `opened_folders: Vec<PathBuf>`, calls discover_plans_per_folder(), and emits PlansDiscoveredPerFolder with a HashMap; the handler populates app.plans_by_folder and collapses every folder except folder_idx 0; **every** existing call site is migrated to the per-folder signature — the two startup/fallback sites (`event.rs:128`, `event.rs:288`) and the new open/close and initialize handlers all now pass `app.opened_folders`; a unit test confirms that after startup auto-discovery the launch folder's PlanInFolder nodes are present in visible_tree_nodes(); cargo test, clippy, and fmt all pass green.

---

### update-sidebar-rendering-for-folders — Update sidebar UI rendering to show folder tree with Folder, Plan, Task levels

The sidebar rendering code in `crates/makina/src/ui.rs` currently renders plans (with an optional collapse icon) and runs. With the new Folder variant in TreeNode and the 3-level hierarchy, the sidebar must display folders at level 0, plans at level 1 (indented), and tasks at level 2 (further indented), with collapse/expand icons for folders and plans.

**Steps:**

1. Locate the sidebar rendering code in `crates/makina/src/ui.rs` (typically a function like `render_sidebar()` or similar; grep for 'Plan {' to find where TreeNode::Plan is rendered).
2. Update the rendering logic to handle the new TreeNode variants:
   - TreeNode::Folder: render at level 0, with a [+] or [-] icon (collapsed/expanded), and the folder name. When the folder has NO plans (its `plans_by_folder` entry is empty or absent), render it in a DISTINCT style instead — e.g. dimmed, no collapse icon, and a trailing hint such as `(empty — Initialize Folder)` — so an uninitialized folder is visually distinguished from one that holds plans. (The user brief explicitly requires empty folders to look different, and this signposts the Initialize Folder command.)
   - TreeNode::PlanInFolder: render at level 1 (indented by 2 spaces or a fixed pixel width), with a [+]/[-] icon, and the plan slug.
   - TreeNode::PlanTaskInFolder: render at level 2 (further indented), with the task id and status.
   - The existing TreeNode::Run and TreeNode::Task are rendered unchanged (but may need adjustment to maintain visual separation from folders).
3. This task owns only RENDERING (indentation, icons, the distinct empty-folder style, and readable labels for every visible node). The keyboard/mouse INTERACTION for the new variants — Space to expand/collapse, Enter to open, focus-left/right — is implemented in `rewire-tree-navigation-for-folders` (workstream 0002); do not duplicate that logic here.

- **Depends on:** refactor-visible-tree-nodes
- **Done when:** The sidebar renders a 3-level tree: folders at level 0 with [+]/[-], plans in folders at level 1 with [+]/[-], and tasks at level 2; a folder with no plans renders in the distinct empty/uninitialized style (dimmed + `(empty — Initialize Folder)` hint) rather than looking identical to a populated folder; every visible node is drawn with correct indentation and a readable label; cargo test, clippy, and fmt all pass green.

---

## 0007 — Multi-Folder Workflow Integration Verification

### verify-multi-folder-integration — Integration test: verify multi-folder workspace workflow end-to-end

After implementing all workstreams, we need to verify that the entire multi-folder workflow works: opening folders, closing them, auto-discovering plans per folder, and initializing folders. This integration test exercises the happy path and a few error cases.

**Steps:**

1. Write a test (in `crates/makina/tests/multi_folder_integration_test.rs`) that:
   1. Creates two temporary directories, each initialized as a Makina-ready folder (with .git, develop branch, docs/plans/).
   2. Simulates user actions: Open Folder A, Open Folder B, verify sidebar shows two folders with their plans.
   3. Simulates Initialize Folder: verify it creates .git, branches, and docs/plans/.
   4. Simulates Close Folder: verify the folder is removed from the sidebar.
   5. Verifies that workspace.toml is created and persists across app restarts.
2. The test should use the existing MockApi or a test fixture to avoid needing a live agent.

- **Depends on:** handle-folder-open-close-events, update-sidebar-rendering-for-folders, handle-initialize-folder-event, update-discovery-event-and-handler, rewire-tree-navigation-for-folders
- **Done when:** The integration test passes; it exercises opening/closing/initializing folders and verifies sidebar rendering and workspace persistence; `cargo test` green; cargo test, clippy, and fmt all pass green.

---

**End of plan 0043 TASKS.** When every "Done when" bullet is green, Makina evolves
from managing a single repo_root to supporting multiple concurrently opened
folders: opened folders persist in `$HOME/.makina/workspace.toml` and survive
restarts, the sidebar renders a 3-level Folder → Plan → Task tree (with empty
folders shown distinctly), the command palette offers Open/Close/Initialize Folder
actions backed by a directory file-picker modal, uninitialized folders bootstrap
git + an empty initial commit + a develop branch + a `docs/plans/README.md`
authoring guide, and plan discovery is scoped per folder.
