//! Integration test for multi-folder workspace functionality.
//!
//! Verifies the complete workflow end-to-end: opening/closing/initializing
//! folders *through the real event wiring* (`event::resolve_io_for_test` +
//! `App::update`, exactly what the production event loop in
//! [`makina::event::run`] drives), workspace persistence, sidebar rendering
//! with the 3-level Folder → Plan → Task hierarchy, and plan discovery per
//! folder.
//!
//! `event::resolve_io_for_test` is a `test-util`-feature-gated public wrapper
//! around the crate's private `resolve_io` (see `crates/makina/Cargo.toml`
//! and `crates/makina/src/event.rs`), so this external test crate can dispatch
//! the exact same `AppEvent`s (`OpenFolderSelected`, `CloseFolderConfirmed`,
//! `InitializeFolderSelected`) the TUI dispatches on real user actions,
//! instead of mutating `App` fields directly.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use makina::app::{App, AppEvent, TreeNode};
use makina::event::resolve_io_for_test;
use makina::workspace::Workspace;
use makina_core::api::{Api, ApiError, Command, CommandOutcome, EventStream, RunId, RunView};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Minimal `Api` double for testing.
struct PlaceholderApi;

#[async_trait]
impl Api for PlaceholderApi {
    async fn execute(&self, _command: Command) -> Result<CommandOutcome, ApiError> {
        Err(ApiError::Internal {
            reason: "stub".into(),
        })
    }

    async fn runs(&self) -> Vec<RunView> {
        vec![]
    }

    async fn run(&self, _id: RunId) -> Option<RunView> {
        None
    }

    fn subscribe(&self) -> EventStream {
        Box::pin(futures::stream::empty())
    }
}

/// Build a bare `App` wired to a `PlaceholderApi`, with no folders opened.
fn new_app() -> App {
    let api = Arc::new(PlaceholderApi);
    App::new(api, vec![], PathBuf::from("/"))
}

/// Add a minimal Makina plan directory (`SCOPE.md` + `ARCHITECTURE.md`, the
/// convention gate `discover_plans_per_folder` checks) under
/// `folder/docs/plans/<slug>/` so the folder's plan discovery finds it.
fn add_plan(folder: &Path, slug: &str) {
    let plan_dir = folder.join("docs").join("plans").join(slug);
    fs::create_dir_all(&plan_dir).expect("failed to create plan dir");
    fs::write(plan_dir.join("SCOPE.md"), "# Scope\n").expect("failed to write SCOPE.md");
    fs::write(plan_dir.join("ARCHITECTURE.md"), "# Architecture\n")
        .expect("failed to write ARCHITECTURE.md");
}

/// Render `app` to an off-screen `TestBackend` and return its content as a
/// flat string, so sidebar assertions can check for folder/plan text.
fn render_to_string(app: &App) -> String {
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| makina::ui::render(app, f))
        .expect("render");
    let buffer = terminal.backend().buffer().clone();
    buffer.content().iter().map(|cell| cell.symbol()).collect()
}

/// Discover plans across `app.opened_folders` and feed the result through
/// `App::update` exactly as the production event loop's
/// `spawn_discover_plans` background task does (task
/// `update-discovery-event-and-handler`) — this is what populates
/// `plans_by_folder` and makes `PlanInFolder`/`PlanTaskInFolder` sidebar nodes
/// appear.
fn discover_and_apply(app: &mut App) {
    let plans_map = makina_core::orchestrator::discover_plans_per_folder(&app.opened_folders);
    app.update(AppEvent::PlansDiscoveredPerFolder { plans_map });
}

/// Open `folder` by dispatching the real `OpenFolderSelected` event through
/// `resolve_io_for_test` (IO resolution) followed by `App::update` (state
/// mutation) — the same two-step pipeline the production event loop runs —
/// then applies discovery so the sidebar reflects the folder's plans.
async fn open_folder_via_event(app: &mut App, folder: &Path) {
    let (resolved, status) = resolve_io_for_test(
        app,
        AppEvent::OpenFolderSelected {
            path: folder.to_path_buf(),
        },
    )
    .await;
    app.update(resolved);
    if let Some(msg) = status {
        app.update(AppEvent::StatusMessage(msg));
    }
    discover_and_apply(app);
}

/// Close `folder` by dispatching the real `CloseFolderConfirmed` event.
async fn close_folder_via_event(app: &mut App, folder: &Path) {
    let (resolved, status) = resolve_io_for_test(
        app,
        AppEvent::CloseFolderConfirmed {
            path: folder.to_path_buf(),
        },
    )
    .await;
    app.update(resolved);
    if let Some(msg) = status {
        app.update(AppEvent::StatusMessage(msg));
    }
    discover_and_apply(app);
}

/// Initialize `folder` by dispatching the real `InitializeFolderSelected`
/// event — this is what actually calls `folder_init::initialize_folder`
/// (bootstrapping .git, main/develop branches, docs/plans/README.md).
async fn initialize_folder_via_event(app: &mut App, folder: &Path) {
    let (resolved, status) = resolve_io_for_test(
        app,
        AppEvent::InitializeFolderSelected {
            path: folder.to_path_buf(),
        },
    )
    .await;
    app.update(resolved);
    if let Some(msg) = status {
        app.update(AppEvent::StatusMessage(msg));
    }
    discover_and_apply(app);
}

/// Test opening a single folder via the real event wiring, and verify it
/// appears both in `opened_folders` and as a rendered sidebar Folder node.
#[tokio::test]
async fn test_open_folder_single() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let folder_a = temp_dir.path().join("folder-a");
    fs::create_dir_all(&folder_a).expect("failed to create folder-a");
    makina::folder_init::initialize_folder(&folder_a).expect("failed to init folder A");

    let workspace_path = temp_dir.path().join("workspace.toml");
    let mut app = new_app();
    app.workspace_path_override = Some(workspace_path.clone());

    open_folder_via_event(&mut app, &folder_a).await;

    assert_eq!(app.opened_folders.len(), 1);
    assert_eq!(app.opened_folders[0], folder_a);
    assert!(
        app.error_messages.is_empty(),
        "opening a valid directory must not push errors"
    );

    let rendered = render_to_string(&app);
    assert!(
        rendered.contains("folder-a"),
        "sidebar must render the opened folder's name; rendered: {rendered}"
    );

    // Persisted too — the event handler must call save_workspace().
    let saved = Workspace::load_from(&workspace_path).expect("failed to load workspace");
    assert!(saved.opened_folders.contains(&folder_a));
}

/// Test opening two folders via real events; verify the sidebar shows both
/// folders with their respective discovered plans (task 2 of the plan spec:
/// "verify sidebar shows two folders with their plans").
#[tokio::test]
async fn test_open_folder_multiple_shows_plans_in_sidebar() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let folder_a = temp_dir.path().join("folder-a");
    let folder_b = temp_dir.path().join("folder-b");
    fs::create_dir_all(&folder_a).expect("failed to create folder-a");
    fs::create_dir_all(&folder_b).expect("failed to create folder-b");

    makina::folder_init::initialize_folder(&folder_a).expect("failed to init folder A");
    makina::folder_init::initialize_folder(&folder_b).expect("failed to init folder B");
    add_plan(&folder_a, "0001-plan-a");
    add_plan(&folder_b, "0001-plan-b");

    let workspace_path = temp_dir.path().join("workspace.toml");
    let mut app = new_app();
    app.workspace_path_override = Some(workspace_path);

    open_folder_via_event(&mut app, &folder_a).await;
    open_folder_via_event(&mut app, &folder_b).await;

    assert_eq!(app.opened_folders.len(), 2);
    assert_eq!(app.opened_folders[0], folder_a);
    assert_eq!(app.opened_folders[1], folder_b);

    // Both folders discovered a plan.
    assert_eq!(app.plans_by_folder.len(), 2);
    assert_eq!(app.plans_by_folder[&0][0].slug, "0001-plan-a");
    assert_eq!(app.plans_by_folder[&1][0].slug, "0001-plan-b");

    // Expand folder B (folder 0 auto-expands per PlansDiscoveredPerFolder;
    // folder 1 starts collapsed) so both plans are visible in the tree.
    app.collapsed_folders.remove(&1);

    let nodes = app.visible_tree_nodes();
    assert!(
        nodes
            .iter()
            .any(|n| matches!(n, TreeNode::Folder { folder_idx: 0 })),
        "folder A must appear as a Folder node"
    );
    assert!(
        nodes
            .iter()
            .any(|n| matches!(n, TreeNode::Folder { folder_idx: 1 })),
        "folder B must appear as a Folder node"
    );
    assert!(
        nodes.iter().any(|n| matches!(
            n,
            TreeNode::PlanInFolder {
                folder_idx: 0,
                plan_idx: 0
            }
        )),
        "folder A's plan must appear as a PlanInFolder node"
    );
    assert!(
        nodes.iter().any(|n| matches!(
            n,
            TreeNode::PlanInFolder {
                folder_idx: 1,
                plan_idx: 0
            }
        )),
        "folder B's plan must appear as a PlanInFolder node"
    );

    // The rendered sidebar must show both folder names and both plan slugs.
    let rendered = render_to_string(&app);
    assert!(rendered.contains("folder-a"), "rendered: {rendered}");
    assert!(rendered.contains("folder-b"), "rendered: {rendered}");
    assert!(rendered.contains("0001-plan-a"), "rendered: {rendered}");
    assert!(rendered.contains("0001-plan-b"), "rendered: {rendered}");
}

/// Test initializing a folder from scratch via the real `InitializeFolderSelected`
/// event: verify it creates .git, main/develop branches, and docs/plans/.
#[tokio::test]
async fn test_initialize_folder_creates_structure() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let new_folder = temp_dir.path().join("new-folder");
    fs::create_dir_all(&new_folder).expect("failed to create new-folder");

    let workspace_path = temp_dir.path().join("workspace.toml");
    let mut app = new_app();
    app.workspace_path_override = Some(workspace_path);

    initialize_folder_via_event(&mut app, &new_folder).await;

    assert!(
        app.opened_folders.contains(&new_folder),
        "InitializeFolderSelected must add the folder to opened_folders"
    );
    assert!(
        app.error_messages.is_empty(),
        "successful initialization must not push errors"
    );

    // Verify .git exists.
    assert!(new_folder.join(".git").exists(), ".git should exist");

    // Verify main branch exists.
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "main"])
        .current_dir(&new_folder)
        .output()
        .expect("git failed");
    assert!(output.status.success(), "main branch should exist");

    // Verify develop branch exists.
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "develop"])
        .current_dir(&new_folder)
        .output()
        .expect("git failed");
    assert!(output.status.success(), "develop branch should exist");

    // Verify docs/plans/README.md exists.
    let readme = new_folder.join("docs").join("plans").join("README.md");
    assert!(readme.exists(), "docs/plans/README.md should exist");

    let content = fs::read_to_string(&readme).expect("failed to read README");
    assert!(
        content.contains("## Layout"),
        "README should contain Layout section"
    );
    assert!(
        content.contains("## TASKS.md contract"),
        "README should contain TASKS.md contract section"
    );

    // The newly initialized folder must render in the sidebar too.
    let rendered = render_to_string(&app);
    assert!(
        rendered.contains("new-folder"),
        "sidebar must render the initialized folder; rendered: {rendered}"
    );
}

/// Test that initializing a folder via the event path is idempotent.
#[tokio::test]
async fn test_initialize_folder_idempotent() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let folder = temp_dir.path().join("idempotent-folder");
    fs::create_dir_all(&folder).expect("failed to create folder");

    let workspace_path = temp_dir.path().join("workspace.toml");
    let mut app = new_app();
    app.workspace_path_override = Some(workspace_path);

    initialize_folder_via_event(&mut app, &folder).await;

    let output1 = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&folder)
        .output()
        .expect("git failed");
    let hash1 = String::from_utf8_lossy(&output1.stdout).trim().to_string();

    // Initialize again through the same event path (should be idempotent, and
    // must not duplicate the folder in opened_folders).
    initialize_folder_via_event(&mut app, &folder).await;

    let output2 = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&folder)
        .output()
        .expect("git failed");
    let hash2 = String::from_utf8_lossy(&output2.stdout).trim().to_string();

    assert_eq!(
        hash1, hash2,
        "commit hash should not change after second init"
    );
    assert_eq!(
        app.opened_folders.iter().filter(|f| *f == &folder).count(),
        1,
        "re-initializing an already-opened folder must not duplicate it"
    );
}

/// Test closing a folder via the real `CloseFolderConfirmed` event: verify the
/// folder is removed from `opened_folders` AND no longer rendered in the
/// sidebar, while the other folder remains.
#[tokio::test]
async fn test_close_folder_removes_from_sidebar() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let folder_a = temp_dir.path().join("folder-a");
    let folder_b = temp_dir.path().join("folder-b");
    fs::create_dir_all(&folder_a).expect("failed to create folder-a");
    fs::create_dir_all(&folder_b).expect("failed to create folder-b");

    makina::folder_init::initialize_folder(&folder_a).expect("failed to init folder A");
    makina::folder_init::initialize_folder(&folder_b).expect("failed to init folder B");

    let workspace_path = temp_dir.path().join("workspace.toml");
    let mut app = new_app();
    app.workspace_path_override = Some(workspace_path.clone());

    open_folder_via_event(&mut app, &folder_a).await;
    open_folder_via_event(&mut app, &folder_b).await;
    assert_eq!(app.opened_folders.len(), 2);

    // Sanity: both folders visible in the sidebar before closing.
    let before = render_to_string(&app);
    assert!(before.contains("folder-a"));
    assert!(before.contains("folder-b"));

    close_folder_via_event(&mut app, &folder_b).await;

    assert_eq!(app.opened_folders.len(), 1);
    assert_eq!(app.opened_folders[0], folder_a);
    assert!(!app.opened_folders.contains(&folder_b));

    // The Folder node count must have dropped from 2 to 1.
    let folder_node_count = app
        .visible_tree_nodes()
        .iter()
        .filter(|n| matches!(n, TreeNode::Folder { .. }))
        .count();
    assert_eq!(
        folder_node_count, 1,
        "closing folder B must remove its Folder node from the sidebar tree"
    );

    let after = render_to_string(&app);
    assert!(
        after.contains("folder-a"),
        "folder A must remain in the sidebar after closing folder B"
    );
    assert!(
        !after.contains("folder-b"),
        "folder B must be removed from the sidebar after closing it; rendered: {after}"
    );

    // Closing must also persist the removal to workspace.toml (not just the
    // in-memory App state).
    let saved = Workspace::load_from(&workspace_path).expect("failed to load workspace");
    assert!(
        !saved.opened_folders.contains(&folder_b),
        "closing a folder must persist the removal to workspace.toml"
    );
    assert!(saved.opened_folders.contains(&folder_a));
}

/// Test workspace persistence: save and load via the `Workspace` API directly
/// (no App involved) — the lowest-level persistence contract the event
/// handlers build on.
#[tokio::test]
async fn test_workspace_persistence() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let workspace_path = temp_dir.path().join("workspace.toml");
    let folder_a = temp_dir.path().join("folder-a");
    let folder_b = temp_dir.path().join("folder-b");
    fs::create_dir_all(&folder_a).expect("failed to create folder-a");
    fs::create_dir_all(&folder_b).expect("failed to create folder-b");

    let mut ws = Workspace::new();
    ws.add_folder(folder_a.clone());
    ws.add_folder(folder_b.clone());
    ws.save_to(&workspace_path)
        .expect("failed to save workspace");

    assert!(workspace_path.exists(), "workspace.toml should be created");

    let loaded = Workspace::load_from(&workspace_path).expect("failed to load workspace");
    assert_eq!(loaded.opened_folders.len(), 2);
    assert!(loaded.opened_folders.contains(&folder_a));
    assert!(loaded.opened_folders.contains(&folder_b));
}

/// Test that opening folders across two separate `App` instances (simulating
/// an app restart) restores `opened_folders` from the persisted
/// workspace.toml — i.e. persistence survives a restart, driven end-to-end
/// through the real `OpenFolderSelected` event on the first "session" and a
/// fresh `App` reading `workspace.toml` on the second.
#[tokio::test]
async fn test_app_restart_restores_opened_folders_from_workspace() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let workspace_path = temp_dir.path().join("workspace.toml");
    let folder_a = temp_dir.path().join("folder-a");
    let folder_b = temp_dir.path().join("folder-b");
    fs::create_dir_all(&folder_a).expect("failed to create folder-a");
    fs::create_dir_all(&folder_b).expect("failed to create folder-b");

    makina::folder_init::initialize_folder(&folder_a).expect("failed to init folder A");
    makina::folder_init::initialize_folder(&folder_b).expect("failed to init folder B");

    // "Session 1": open both folders through the real event path.
    {
        let mut app = new_app();
        app.workspace_path_override = Some(workspace_path.clone());
        open_folder_via_event(&mut app, &folder_a).await;
        open_folder_via_event(&mut app, &folder_b).await;
        assert_eq!(app.opened_folders.len(), 2);
    }
    // `app` (and its in-memory state) is dropped here — simulating quitting
    // Makina entirely.

    // "Session 2": a brand new App restores its opened folders from the
    // persisted workspace.toml, mirroring what main.rs does at startup.
    let restored_ws = Workspace::load_from(&workspace_path).expect("failed to load workspace");
    let opened_folders: Vec<PathBuf> = restored_ws.opened_folders.iter().cloned().collect();

    let api = Arc::new(PlaceholderApi);
    let mut app2 = App::new(api, vec![], folder_a.clone());
    app2.workspace_path_override = Some(workspace_path);
    app2.opened_folders = opened_folders;
    app2.workspace = restored_ws;

    assert_eq!(app2.opened_folders.len(), 2);
    assert!(app2.opened_folders.contains(&folder_a));
    assert!(app2.opened_folders.contains(&folder_b));

    let rendered = render_to_string(&app2);
    assert!(rendered.contains("folder-a"), "rendered: {rendered}");
    assert!(rendered.contains("folder-b"), "rendered: {rendered}");
}

/// Test the 3-level tree structure end-to-end: opening a folder (real event),
/// discovering its plan (real discovery handler), and expanding the plan node
/// so its task appears as a `PlanTaskInFolder` node — the full Folder → Plan →
/// Task hierarchy the sidebar renders.
#[tokio::test]
async fn test_sidebar_tree_three_level_structure() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let folder_a = temp_dir.path().join("folder-a");
    fs::create_dir_all(&folder_a).expect("failed to create folder-a");
    makina::folder_init::initialize_folder(&folder_a).expect("failed to init folder A");

    // Write a plan with a real TASKS.md so a task preview is parsed.
    let plan_dir = folder_a.join("docs").join("plans").join("0001-test-plan");
    fs::create_dir_all(&plan_dir).expect("failed to create plan dir");
    fs::write(plan_dir.join("SCOPE.md"), "# Scope\n").expect("write SCOPE.md");
    fs::write(plan_dir.join("ARCHITECTURE.md"), "# Architecture\n").expect("write ARCHITECTURE.md");
    fs::write(
        plan_dir.join("TASKS.md"),
        "### my-task — Do the thing\n\n- **Depends on:** none\n- **Done when:** it is done\n",
    )
    .expect("write TASKS.md");

    let workspace_path = temp_dir.path().join("workspace.toml");
    let mut app = new_app();
    app.workspace_path_override = Some(workspace_path);

    open_folder_via_event(&mut app, &folder_a).await;

    // Folder 0 starts expanded (per PlansDiscoveredPerFolder's handler), so
    // the plan and its task should already be visible without extra toggling.
    let nodes = app.visible_tree_nodes();

    assert!(
        nodes
            .iter()
            .any(|n| matches!(n, TreeNode::Folder { folder_idx: 0 })),
        "should have a Folder node"
    );
    assert!(
        nodes.iter().any(|n| matches!(
            n,
            TreeNode::PlanInFolder {
                folder_idx: 0,
                plan_idx: 0
            }
        )),
        "should have a PlanInFolder node for the discovered plan"
    );
    assert!(
        nodes.iter().any(|n| matches!(
            n,
            TreeNode::PlanTaskInFolder {
                folder_idx: 0,
                plan_idx: 0,
                task_idx: 0
            }
        )),
        "should have a PlanTaskInFolder node for the plan's task"
    );

    let rendered = render_to_string(&app);
    assert!(rendered.contains("folder-a"), "rendered: {rendered}");
    assert!(rendered.contains("0001-test-plan"), "rendered: {rendered}");
    assert!(rendered.contains("my-task"), "rendered: {rendered}");
}

/// Test that empty (uninitialized, no docs/plans) folders are tracked and
/// rendered with the distinct "empty" sidebar treatment.
#[tokio::test]
async fn test_empty_folder_tracking() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let empty_folder = temp_dir.path().join("empty");
    fs::create_dir_all(&empty_folder).expect("failed to create empty folder");

    let workspace_path = temp_dir.path().join("workspace.toml");
    let mut app = new_app();
    app.workspace_path_override = Some(workspace_path);

    open_folder_via_event(&mut app, &empty_folder).await;

    assert_eq!(app.opened_folders.len(), 1);
    assert_eq!(app.opened_folders[0], empty_folder);
    assert_eq!(
        app.plans_by_folder.get(&0).map(Vec::len).unwrap_or(0),
        0,
        "an uninitialized folder should discover zero plans"
    );

    let rendered = render_to_string(&app);
    assert!(rendered.contains("empty"), "rendered: {rendered}");
    assert!(
        rendered.contains("Initialize F"),
        "an empty folder must render the distinct empty-folder hint; rendered: {rendered}"
    );
}

/// Integration test: complete workflow with multiple folders, exercising the
/// full happy path end-to-end through the real event wiring: open A, open B,
/// initialize C, close B — and verify that EVERY step's effect on
/// `opened_folders` is faithfully persisted to workspace.toml at that point
/// in time (not just the final in-memory state).
#[tokio::test]
async fn test_multi_folder_complete_workflow() {
    let temp_dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let workspace_path = temp_dir.path().join("workspace.toml");
    let folder_a = temp_dir.path().join("folder-a");
    let folder_b = temp_dir.path().join("folder-b");
    let folder_c = temp_dir.path().join("folder-c");

    fs::create_dir_all(&folder_a).expect("failed to create folder-a");
    fs::create_dir_all(&folder_b).expect("failed to create folder-b");
    fs::create_dir_all(&folder_c).expect("failed to create folder-c");

    // Step 1: Initialize folder A and B ahead of time (as if from a previous
    // session), then bring up the App and point it at a fresh workspace file.
    makina::folder_init::initialize_folder(&folder_a).expect("failed to init folder A");
    makina::folder_init::initialize_folder(&folder_b).expect("failed to init folder B");
    add_plan(&folder_a, "0001-plan-a");
    add_plan(&folder_b, "0001-plan-b");

    let mut app = new_app();
    app.workspace_path_override = Some(workspace_path.clone());

    // Step 2: Open Folder A, then Folder B, through the real event path.
    open_folder_via_event(&mut app, &folder_a).await;
    open_folder_via_event(&mut app, &folder_b).await;

    assert_eq!(app.opened_folders, vec![folder_a.clone(), folder_b.clone()]);

    // Sidebar shows both folders with their plans.
    app.collapsed_folders.remove(&1); // expand folder B too, for the assertion
    let rendered = render_to_string(&app);
    assert!(rendered.contains("folder-a"), "rendered: {rendered}");
    assert!(rendered.contains("folder-b"), "rendered: {rendered}");
    assert!(rendered.contains("0001-plan-a"), "rendered: {rendered}");
    assert!(rendered.contains("0001-plan-b"), "rendered: {rendered}");

    // Workspace file already reflects A + B after step 2 (no explicit save
    // needed — the event handlers persist on every open/close/initialize).
    let after_open = Workspace::load_from(&workspace_path).expect("load after open");
    assert_eq!(after_open.opened_folders.len(), 2);
    assert!(after_open.opened_folders.contains(&folder_a));
    assert!(after_open.opened_folders.contains(&folder_b));

    // Step 3: Initialize folder C (new folder) through the real event path —
    // this both bootstraps it AND adds/persists it to opened_folders.
    initialize_folder_via_event(&mut app, &folder_c).await;

    assert_eq!(app.opened_folders.len(), 3);
    assert!(app.opened_folders.contains(&folder_c));
    assert!(folder_c.join(".git").exists());
    assert!(
        folder_c
            .join("docs")
            .join("plans")
            .join("README.md")
            .exists()
    );

    let after_init = Workspace::load_from(&workspace_path).expect("load after init");
    assert_eq!(after_init.opened_folders.len(), 3);
    assert!(after_init.opened_folders.contains(&folder_c));

    // Step 4: Close folder B through the real event path.
    close_folder_via_event(&mut app, &folder_b).await;

    // Step 5: Verify final in-memory state.
    assert_eq!(app.opened_folders.len(), 2);
    assert!(app.opened_folders.contains(&folder_a));
    assert!(app.opened_folders.contains(&folder_c));
    assert!(!app.opened_folders.contains(&folder_b));

    // The sidebar must no longer render folder B or its plan.
    let after_close_render = render_to_string(&app);
    assert!(after_close_render.contains("folder-a"));
    assert!(after_close_render.contains("folder-c"));
    assert!(
        !after_close_render.contains("folder-b"),
        "closed folder B must not appear in the sidebar; rendered: {after_close_render}"
    );
    assert!(
        !after_close_render.contains("0001-plan-b"),
        "closed folder B's plan must not appear in the sidebar; rendered: {after_close_render}"
    );

    // Step 6 (the bug this test previously missed): reload workspace.toml
    // AFTER closing B and verify the removal was persisted — i.e. B is GONE
    // from disk, not still present from a stale pre-close save.
    let final_ws = Workspace::load_from(&workspace_path).expect("failed to load final workspace");
    let final_folders: Vec<PathBuf> = final_ws.opened_folders.iter().cloned().collect();

    assert_eq!(
        final_folders.len(),
        2,
        "closing folder B must persist to exactly 2 folders remaining; got {final_folders:?}"
    );
    assert!(final_folders.contains(&folder_a));
    assert!(final_folders.contains(&folder_c));
    assert!(
        !final_folders.contains(&folder_b),
        "closing folder B must remove it from the persisted workspace.toml, \
        not merely from in-memory opened_folders; persisted: {final_folders:?}"
    );
}
