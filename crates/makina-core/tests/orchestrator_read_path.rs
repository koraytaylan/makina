//! Integration tests for **orchestrator-read-path** (plan 0002 §0011).
//!
//! Acceptance criterion: an integration test opens a run, demonstrates that a
//! pre-existing `.tasks/{slug}.json` artifact is used as the source of truth
//! instead of re-interpreting the `.md` file, and verifies that
//! `recover_for_resume` is applied (formerly `InProgress` tasks become `Ready`).
//!
//! # Test coverage
//!
//! 1. **Resume from artifact** — a persisted graph with `Done`/`InProgress`/`New`
//!    tasks is loaded on `OpenRun`; the registered graph preserves the `Done` task
//!    and shows the formerly `InProgress` task as `Ready` (not `New`), even though
//!    the `.md` file would have produced a different graph if interpreted.
//! 2. **Fresh path unchanged** — when no artifact exists, `OpenRun` reads the
//!    `.md`, interprets it, and seed-persists all tasks as `New` (the existing
//!    `orchestrator-seed-write` behavior).
//!
//! # Test-strategy compliance
//!
//! - Backend is always `NoopBackend` — no real agent CLI, no model call.
//! - Each test uses a fresh temporary git repo (`tempfile`).
//! - No arbitrary sleeps.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use chrono::Utc;

use makina_core::api::{Api, Command as ApiCommand, CommandOutcome, RunStatus};
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::dependency::EdgeInferrer;
use makina_core::interpreter::StructuredTextInterpreter;
use makina_core::orchestrator::CoreApi;
use makina_core::persist::{load_graph, persist_graph, tasks_path};
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

// ── Temp-repo helpers ─────────────────────────────────────────────────────────

/// Create a minimal git repository in a fresh tempdir on a `develop` branch.
fn setup_temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path();

    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);

    let branch = String::from_utf8(
        Command::new("git")
            .args(["-C", &path.to_string_lossy()])
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("git rev-parse HEAD")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();

    if branch != "develop" {
        run_git(path, &["branch", "-m", &branch, "develop"]);
    }

    dir
}

fn run_git(path: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    assert!(status.success(), "git {args:?} failed");
}

/// Build a `CoreApi` over the deterministic interpreter + `NoopBackend` +
/// a temp-repo `WorktreeManager` + a no-gate `Config`.
fn build_api(repo_root: PathBuf) -> CoreApi {
    let interpreter = Arc::new(EdgeInferrer::new(
        Arc::new(StructuredTextInterpreter::new()),
    ));
    let backend = Arc::new(NoopBackend::with_responses(vec![
        "Implemented.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]));
    let wm = WorktreeManager::new(repo_root, "develop".into());
    let config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());
    CoreApi::new(interpreter, backend, wm, config)
}

/// Build a well-formed `Task` with the given `id` and `state`.
fn make_task(id: &str, state: TaskState, depends_on: Vec<&str>) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: format!("Task {id}"),
        description: format!("Implements {id}."),
        done_when: "done".to_string(),
        depends_on: depends_on.into_iter().map(TaskId::new).collect(),
        section: None,
        state,
        gate_iterations: 0,
        review_iterations: 0,
        created_at: now,
        updated_at: now,
        started_at: if state == TaskState::InProgress
            || state == TaskState::InReview
            || state == TaskState::Done
        {
            Some(now)
        } else {
            None
        },
        finished_at: if state == TaskState::Done || state == TaskState::Failed {
            Some(now)
        } else {
            None
        },
    }
}

// ── Test 1: Resume from persisted artifact ───────────────────────────────────

/// **Acceptance (orchestrator-read-path):**
///
/// Sequence:
/// 1. Build a `TaskGraph` (slug `read-path-resume`) with:
///    - `task-done`       → `Done`
///    - `task-in-flight`  → `InProgress`
///    - `task-new`        → `New` (depends on task-done)
///    Persist it to `repo_root/.tasks/read-path-resume.json`.
/// 2. Write a `.md` file whose stem is `read-path-resume` but whose interpreted
///    content would differ from the persisted graph (single task `md-only-task`)
///    — so we can prove the artifact (not the `.md`) was used.
/// 3. Issue `OpenRun` on that `.md` file.
/// 4. Assert the registered run's graph:
///    - Has 3 tasks (from the persisted artifact, not the 1-task `.md`).
///    - `task-done` is still `Done`.
///    - `task-in-flight` is now `Ready` (recover_for_resume applied).
///    - `task-new` is `New`.
#[tokio::test]
async fn open_run_resumes_from_artifact_and_ignores_md() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let slug = "read-path-resume";

    // 1. Build and persist the "prior run" graph.
    let persisted_graph = TaskGraph {
        slug: slug.to_string(),
        tasks: vec![
            make_task("task-done", TaskState::Done, vec![]),
            make_task("task-in-flight", TaskState::InProgress, vec!["task-done"]),
            make_task("task-new", TaskState::New, vec!["task-done"]),
        ],
    };
    persist_graph(&persisted_graph, &repo_root)
        .await
        .expect("persist_graph must succeed");

    // Verify the artifact exists.
    let artifact_path = tasks_path(&repo_root, slug);
    assert!(
        artifact_path.exists(),
        ".tasks/{slug}.json must exist after persist_graph"
    );

    // 2. Write a `.md` whose stem matches the slug but whose content is DIFFERENT
    //    from the persisted graph.  If `open_run` re-interprets this file, it
    //    would produce a single task `md-only-task` — which we can detect below.
    let md_dir = tempfile::tempdir().expect("create md tempdir");
    let md_path = md_dir.path().join(format!("{slug}.md"));
    let decoy_md = r#"# Decoy — Task List

This file would produce a single task if re-interpreted.

---

## 0001 — Decoy

### md-only-task — The MD-only task
Do nothing useful.
- **Depends on:** —
- **Done when:** nothing.
"#;
    std::fs::write(&md_path, decoy_md).expect("write decoy .md");

    // 3. Issue OpenRun.
    let api = build_api(repo_root.clone());
    let outcome = api
        .execute(ApiCommand::OpenRun {
            task_list_path: md_path.clone(),
        })
        .await
        .expect("OpenRun must succeed");

    let run_id = match outcome {
        CommandOutcome::RunOpened { run } => run,
        other => panic!("expected RunOpened, got {other:?}"),
    };

    // 4. Inspect the registered graph.
    let view = api
        .run(run_id)
        .await
        .expect("run(id) must return the registered run");

    // Must have come from the artifact (3 tasks), NOT the .md (1 task).
    assert_eq!(
        view.tasks.len(),
        3,
        "graph must have 3 tasks (from artifact), not 1 (from .md); got: {:?}",
        view.tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );

    // The task ids must match the persisted artifact.
    let task_ids: std::collections::HashSet<_> =
        view.tasks.iter().map(|t| t.id.0.as_str()).collect();
    assert!(
        task_ids.contains("task-done"),
        "task-done must be present; got {:?}",
        task_ids
    );
    assert!(
        task_ids.contains("task-in-flight"),
        "task-in-flight must be present"
    );
    assert!(task_ids.contains("task-new"), "task-new must be present");
    assert!(
        !task_ids.contains("md-only-task"),
        "md-only-task must NOT be present (artifact was used, not the .md)"
    );

    // task-done must still be Done.
    let done_task = view
        .tasks
        .iter()
        .find(|t| t.id.0 == "task-done")
        .expect("task-done must exist");
    assert_eq!(
        done_task.state,
        makina_core::api::TaskState::Done,
        "task-done must still be Done after resume"
    );

    // task-in-flight must have been reset to Ready by recover_for_resume.
    let in_flight = view
        .tasks
        .iter()
        .find(|t| t.id.0 == "task-in-flight")
        .expect("task-in-flight must exist");
    assert_eq!(
        in_flight.state,
        makina_core::api::TaskState::Ready,
        "task-in-flight must be Ready (recover_for_resume applied), not InProgress"
    );

    // task-new must stay New.
    let new_task = view
        .tasks
        .iter()
        .find(|t| t.id.0 == "task-new")
        .expect("task-new must exist");
    assert_eq!(
        new_task.state,
        makina_core::api::TaskState::New,
        "task-new must still be New"
    );

    // The run must be Pending (not started yet).
    assert_eq!(
        view.status,
        RunStatus::Pending,
        "resumed run must start Pending"
    );
}

// ── Test 2: Fresh path is unchanged when no artifact exists ──────────────────

/// **Acceptance (orchestrator-seed-write fresh path, preserved):**
///
/// When NO artifact exists for the slug, `OpenRun` reads the `.md`, interprets
/// it, registers the run, and seed-persists all tasks as `New` — before any
/// `StartRun`.  This is the unchanged `orchestrator-seed-write` behavior.
#[tokio::test]
async fn open_run_fresh_path_seeds_artifact_when_no_artifact_exists() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let slug = "read-path-fresh";

    // Verify no artifact exists yet.
    let artifact_path = tasks_path(&repo_root, slug);
    assert!(
        !artifact_path.exists(),
        ".tasks/{slug}.json must not exist before OpenRun"
    );

    // Write a valid .md.
    let md_dir = tempfile::tempdir().expect("create md tempdir");
    let md_path = md_dir.path().join(format!("{slug}.md"));
    let md_content = r#"# Fresh — Task List

A minimal list for the fresh-path test.

---

## 0001 — Foundation

### alpha — Alpha task
Create alpha.
- **Depends on:** —
- **Done when:** alpha done.

### beta — Beta task
Create beta.
- **Depends on:** alpha
- **Done when:** beta done.
"#;
    std::fs::write(&md_path, md_content).expect("write .md");

    // Issue OpenRun (no artifact → fresh path).
    let api = build_api(repo_root.clone());
    let outcome = api
        .execute(ApiCommand::OpenRun {
            task_list_path: md_path.clone(),
        })
        .await
        .expect("OpenRun must succeed");

    assert!(
        matches!(outcome, CommandOutcome::RunOpened { .. }),
        "expected RunOpened, got {outcome:?}"
    );

    // Artifact must now exist on disk.
    assert!(
        artifact_path.exists(),
        ".tasks/{slug}.json must exist immediately after OpenRun (seed-persist)"
    );

    // All tasks must be in `New` state.
    let loaded = load_graph(&repo_root, slug)
        .await
        .expect("load_graph must not error")
        .expect(".tasks/{slug}.json must be loadable");

    assert_eq!(loaded.slug, slug);
    assert_eq!(loaded.tasks.len(), 2, "both tasks must be persisted");
    for task in &loaded.tasks {
        assert_eq!(
            task.state,
            TaskState::New,
            "task `{}` must be New before StartRun; got {:?}",
            task.id,
            task.state
        );
    }
}

// ── Test 3: Corrupt/unreadable artifact → falls back to fresh interpret + seed ─

/// **Crash-recovery property (corrupt artifact):**
///
/// If `.tasks/{slug}.json` exists but contains garbage (not valid JSON),
/// `OpenRun` must NOT fail.  Instead it must fall back to reading + interpreting
/// the `.md` file, register a fresh graph, and seed-persist a valid artifact.
///
/// Sequence:
/// 1. Create `.tasks/` and write `"{ not json"` to `.tasks/corrupt-fallback.json`.
/// 2. Write a valid `.md` (stem = `corrupt-fallback`) with two known tasks
///    (`cf-task-one`, `cf-task-two`) that are DISTINCT from any artifact content.
/// 3. Issue `OpenRun` on the `.md`.
/// 4. Assert: `OpenRun` succeeds (no error).
/// 5. Assert: the registered graph has 2 tasks with ids matching the `.md`
///    (`cf-task-one`, `cf-task-two`), all in `New` state — proving fallback.
/// 6. Assert: the on-disk artifact is now valid JSON (overwritten by seed-persist).
#[tokio::test]
async fn open_run_falls_back_to_fresh_interpret_when_artifact_is_corrupt() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let slug = "corrupt-fallback";

    // 1. Write garbage to the artifact path (not valid JSON).
    let artifact_path = tasks_path(&repo_root, slug);
    let tasks_dir = artifact_path.parent().expect("artifact has parent dir");
    std::fs::create_dir_all(tasks_dir).expect("create tasks dir");
    std::fs::write(&artifact_path, b"{ not json").expect("write corrupt artifact");
    assert!(
        artifact_path.exists(),
        "corrupt artifact must exist before OpenRun"
    );

    // 2. Write a valid .md whose interpreted content is distinct from any
    //    artifact: two tasks `cf-task-one` and `cf-task-two`.
    let md_dir = tempfile::tempdir().expect("create md tempdir");
    let md_path = md_dir.path().join(format!("{slug}.md"));
    let md_content = r#"# Corrupt-Fallback — Task List

A list whose artifact is corrupt; open_run must fall back to this .md.

---

## 0001 — Foundation

### cf-task-one — First fallback task
Do the first thing.
- **Depends on:** —
- **Done when:** first thing done.

### cf-task-two — Second fallback task
Do the second thing.
- **Depends on:** cf-task-one
- **Done when:** second thing done.
"#;
    std::fs::write(&md_path, md_content).expect("write fallback .md");

    // 3. Issue OpenRun — the corrupt artifact must NOT cause a failure.
    let api = build_api(repo_root.clone());
    let outcome = api
        .execute(ApiCommand::OpenRun {
            task_list_path: md_path.clone(),
        })
        .await
        .expect("OpenRun must succeed even when the persisted artifact is corrupt");

    // 4. Assert: RunOpened (not an error).
    let run_id = match outcome {
        CommandOutcome::RunOpened { run } => run,
        other => panic!("expected RunOpened, got {other:?}"),
    };

    // 5. Assert: the registered graph came from the .md (fresh interpret).
    let view = api
        .run(run_id)
        .await
        .expect("run(id) must return the registered run");

    assert_eq!(
        view.tasks.len(),
        2,
        "graph must have 2 tasks from the .md fallback; got: {:?}",
        view.tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );

    let task_ids: std::collections::HashSet<_> =
        view.tasks.iter().map(|t| t.id.0.as_str()).collect();
    assert!(
        task_ids.contains("cf-task-one"),
        "cf-task-one must be present (from .md); got {:?}",
        task_ids
    );
    assert!(
        task_ids.contains("cf-task-two"),
        "cf-task-two must be present (from .md); got {:?}",
        task_ids
    );

    for task in &view.tasks {
        assert_eq!(
            task.state,
            makina_core::api::TaskState::New,
            "task `{}` must be New (fresh interpret); got {:?}",
            task.id.0,
            task.state
        );
    }

    // 6. Assert: the artifact was overwritten by seed-persist and is now valid.
    let reloaded = load_graph(&repo_root, slug)
        .await
        .expect("load_graph must succeed after seed-persist overwrote the corrupt artifact")
        .expect("artifact must exist after seed-persist");
    assert_eq!(reloaded.slug, slug);
    assert_eq!(
        reloaded.tasks.len(),
        2,
        "re-persisted artifact must have 2 tasks"
    );
}

// ── Test 4: Validate-failing artifact → falls back to fresh interpret + seed ──

/// **Crash-recovery property (validate-failing artifact):**
///
/// If `.tasks/{slug}.json` exists and is valid JSON but fails `TaskGraph::validate()`
/// (e.g. a task whose `depends_on` references a non-existent task id — a dangling
/// edge), `OpenRun` must NOT fail.  It must fall back to the `.md`, register a
/// fresh graph, and seed-persist a valid artifact.
///
/// Sequence:
/// 1. Build a `TaskGraph` with a dangling dependency (`vf-task-a` depends on
///    `ghost-task` which does not exist) and serialize it to
///    `.tasks/validate-fail.json`.  Crucially, the task ids in the artifact
///    (`vf-task-a`) differ from those in the `.md` (`vf-md-one`, `vf-md-two`) so
///    we can prove which source was used.
/// 2. Write a valid `.md` (stem = `validate-fail`) with two distinct tasks.
/// 3. Issue `OpenRun`.
/// 4. Assert: `OpenRun` succeeds.
/// 5. Assert: the registered graph has 2 tasks from the `.md` (NOT from the
///    dangling-edge artifact), all `New`.
#[tokio::test]
async fn open_run_falls_back_to_fresh_interpret_when_artifact_fails_validation() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let slug = "validate-fail";

    // 1. Build a structurally-valid JSON TaskGraph that fails validate():
    //    `vf-task-a` has a depends_on referencing `ghost-task` which is absent.
    //    This trips TaskGraphError::UnresolvedDependency.
    let now = chrono::Utc::now();
    let invalid_graph = TaskGraph {
        slug: slug.to_string(),
        tasks: vec![Task {
            id: TaskId::new("vf-task-a"),
            title: "VF Task A".to_string(),
            description: "Has a dangling dependency.".to_string(),
            done_when: "done".to_string(),
            depends_on: vec![TaskId::new("ghost-task")], // dangling — ghost-task absent
            section: None,
            state: TaskState::New,
            gate_iterations: 0,
            review_iterations: 0,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        }],
    };

    // Confirm validate() rejects this graph (belt-and-suspenders: ensures we're
    // actually testing the right failure mode).
    assert!(
        invalid_graph.validate().is_err(),
        "the hand-crafted artifact graph must fail validate()"
    );

    // Write the invalid but parseable JSON to disk.
    let artifact_path = tasks_path(&repo_root, slug);
    let tasks_dir = artifact_path.parent().expect("artifact has parent dir");
    std::fs::create_dir_all(tasks_dir).expect("create tasks dir");
    let json =
        serde_json::to_string_pretty(&invalid_graph).expect("invalid_graph must serialize to JSON");
    std::fs::write(&artifact_path, json.as_bytes()).expect("write validate-failing artifact");
    assert!(
        artifact_path.exists(),
        "validate-failing artifact must exist before OpenRun"
    );

    // 2. Write a valid .md with two tasks DISTINCT from the artifact (`vf-task-a`).
    let md_dir = tempfile::tempdir().expect("create md tempdir");
    let md_path = md_dir.path().join(format!("{slug}.md"));
    let md_content = r#"# Validate-Fail — Task List

This .md should be used when the artifact fails validation.

---

## 0001 — Foundation

### vf-md-one — First MD task
Do the first thing.
- **Depends on:** —
- **Done when:** first thing done.

### vf-md-two — Second MD task
Do the second thing.
- **Depends on:** vf-md-one
- **Done when:** second thing done.
"#;
    std::fs::write(&md_path, md_content).expect("write fallback .md");

    // 3. Issue OpenRun — the validate-failing artifact must NOT cause a failure.
    let api = build_api(repo_root.clone());
    let outcome = api
        .execute(ApiCommand::OpenRun {
            task_list_path: md_path.clone(),
        })
        .await
        .expect("OpenRun must succeed even when the persisted artifact fails validation");

    // 4. Assert: RunOpened (not an error).
    let run_id = match outcome {
        CommandOutcome::RunOpened { run } => run,
        other => panic!("expected RunOpened, got {other:?}"),
    };

    // 5. Assert: the registered graph came from the .md (fresh interpret).
    let view = api
        .run(run_id)
        .await
        .expect("run(id) must return the registered run");

    assert_eq!(
        view.tasks.len(),
        2,
        "graph must have 2 tasks from the .md fallback (not 1 from the artifact); got: {:?}",
        view.tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );

    let task_ids: std::collections::HashSet<_> =
        view.tasks.iter().map(|t| t.id.0.as_str()).collect();
    assert!(
        task_ids.contains("vf-md-one"),
        "vf-md-one must be present (from .md fallback); got {:?}",
        task_ids
    );
    assert!(
        task_ids.contains("vf-md-two"),
        "vf-md-two must be present (from .md fallback); got {:?}",
        task_ids
    );
    assert!(
        !task_ids.contains("vf-task-a"),
        "vf-task-a must NOT be present (artifact was discarded); got {:?}",
        task_ids
    );

    for task in &view.tasks {
        assert_eq!(
            task.state,
            makina_core::api::TaskState::New,
            "task `{}` must be New (fresh interpret); got {:?}",
            task.id.0,
            task.state
        );
    }
}
