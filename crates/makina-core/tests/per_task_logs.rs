//! Integration test for **log-per-task-files** — verifies that the per-task
//! `task_slug` span attached to each spawned driver future (in `scheduler`'s
//! `JoinSet::spawn`) routes that task's tracing records to its own per-task log
//! file, `.makina/runs/{run_uid}/logs/{task_slug}.log`.
//!
//! # Strategy
//!
//! 1. Install a span-field routing tracing subscriber (process-global, since the
//!    driver future runs on a spawned `JoinSet` task). The routing layer mirrors
//!    the `makina` binary's `RunFileLayer`: it stashes each span's `run_uid` /
//!    `task_slug` fields into the span extensions on `on_new_span`, then on each
//!    event walks the span scope and appends the record to
//!    `paths::task_log(repo_root, run_uid, task_slug)` when a `task_slug` is in
//!    scope (falling back to the per-run `run.log` otherwise). `RunFileLayer`
//!    itself lives downstream in `makina` and cannot be imported here, so the
//!    test reimplements the minimal routing behavior it asserts against.
//! 2. Drive a single task through `run_graph` (the orchestrator's real path) with
//!    `NoopBackend` and a temp repo. The driver future is tagged with the
//!    `task` span carrying `task_slug = %task_id.0` by the production code under
//!    test.
//! 3. Assert `paths::task_log(repo_root, run_uid, task_id)` exists and contains
//!    the task's state transitions (the `"task state transition"` records) and a
//!    gate-output line (the `"gates passed"` record; with the default empty gate
//!    list, gates still run and pass).
//!
//! Mirrors `tests/supervisor_audit_registry.rs` for the temp-repo + `run_graph`
//! harness.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use chrono::Utc;

use makina_core::actors::{RunControl, run_graph};
use makina_core::api::RunId;
use makina_core::audit::NoopAuditRegistry;
use makina_core::backend::AgentBackend;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::interpreter::StructuredTextInterpreter;
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::registry;
use tracing_subscriber::registry::LookupSpan;

// ── Span-field routing layer (mirror of `makina::log::RunFileLayer`) ────────────

/// Span-extension value: the `run_uid` a span (and its children) belong to.
#[derive(Clone)]
struct RunUid(String);

/// Span-extension value: the `task_slug` a span (and its children) belong to.
#[derive(Clone)]
struct TaskSlug(String);

/// Pulls the `run_uid` / `task_slug` strings out of a span's / event's fields.
#[derive(Default)]
struct FieldVisitor {
    run_uid: Option<String>,
    task_slug: Option<String>,
    message: String,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "run_uid" => self.run_uid = Some(value.to_owned()),
            "task_slug" => self.task_slug = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            // The `%run_uid` / `%task_slug` (`Display`) forms arrive as Debug.
            "run_uid" => {
                self.run_uid = Some(format!("{value:?}").trim_matches('"').to_owned());
            }
            "task_slug" => {
                self.task_slug = Some(format!("{value:?}").trim_matches('"').to_owned());
            }
            "message" => self.message = format!("{value:?}"),
            _ => {}
        }
    }
}

/// A test routing layer that appends each event to its destination log file,
/// keyed by the nearest `task_slug` (per-task file) / `run_uid` (run-level file)
/// in the event's span scope. Mirrors `RunFileLayer`.
struct TestRunFileLayer {
    repo_root: std::path::PathBuf,
    writers: Mutex<HashMap<std::path::PathBuf, std::fs::File>>,
}

impl TestRunFileLayer {
    fn new(repo_root: std::path::PathBuf) -> Self {
        Self {
            repo_root,
            writers: Mutex::new(HashMap::new()),
        }
    }

    fn append(&self, run_uid: &str, task_slug: Option<&str>, line: &str) {
        use std::io::Write as _;
        if makina_core::paths::run_logs_dir(&self.repo_root, run_uid).is_err() {
            return;
        }
        let path = match task_slug {
            Some(slug) => makina_core::paths::task_log(&self.repo_root, run_uid, slug),
            None => makina_core::paths::run_dir(&self.repo_root, run_uid)
                .join("logs")
                .join("run.log"),
        };
        let Ok(mut writers) = self.writers.lock() else {
            return;
        };
        let file = match writers.entry(path.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let Ok(file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                else {
                    return;
                };
                e.insert(file)
            }
        };
        let _ = writeln!(file, "{line}");
    }
}

impl<S> Layer<S> for TestRunFileLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            if let Some(run_uid) = visitor.run_uid {
                span.extensions_mut().insert(RunUid(run_uid));
            }
            if let Some(task_slug) = visitor.task_slug {
                span.extensions_mut().insert(TaskSlug(task_slug));
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let (run_uid, task_slug) = match ctx.event_scope(event) {
            Some(scope) => scope.from_root().fold((None, None), |(run, task), span| {
                let ext = span.extensions();
                (
                    ext.get::<RunUid>().map(|r| r.0.clone()).or(run),
                    ext.get::<TaskSlug>().map(|t| t.0.clone()).or(task),
                )
            }),
            None => (None, None),
        };

        let Some(run_uid) = run_uid else {
            return;
        };

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let meta = event.metadata();
        let line = format!("{} {} {}", meta.level(), meta.target(), visitor.message);
        self.append(&run_uid, task_slug.as_deref(), &line);
    }
}

// ── Temp-repo helpers (mirror tests/supervisor_audit_registry.rs) ────────────────

fn setup_temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("should create temp dir");
    let path = dir.path();

    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
    run_git(path, &["commit", "--allow-empty", "-m", "Initial commit"]);

    let current_branch = String::from_utf8(
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

    if current_branch != "develop" {
        run_git(path, &["branch", "-m", &current_branch, "develop"]);
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
    assert!(
        status.success(),
        "git {args:?} in {path:?} exited with {:?}",
        status.code()
    );
}

fn task(id: &str) -> Task {
    let now = Utc::now();
    Task {
        id: TaskId::new(id),
        title: format!("Task {id}"),
        description: format!("Implement {id}."),
        done_when: format!("{id} is implemented"),
        depends_on: vec![],
        section: None,
        state: TaskState::New,
        gate_iterations: 0,
        review_iterations: 0,
        created_at: now,
        updated_at: now,
        started_at: None,
        finished_at: None,
    }
}

// ── Acceptance test ──────────────────────────────────────────────────────────────

/// The per-task `task_slug` span attached to the driver future routes the task's
/// tracing records to `paths::task_log(repo_root, run_uid, task_slug)`.
///
/// This test:
/// 1. Installs the span-field routing subscriber (global, since the driver runs
///    on a spawned `JoinSet` task).
/// 2. Drives one task through `run_graph` with `NoopBackend` to `Done`.
/// 3. Asserts the per-task log file exists and contains the task's state
///    transitions (`"task state transition"`) plus a gate-output line
///    (`"gates passed"`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_task_logs() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let task_id_str = "per-task-log-task";
    let slug = "per-task-log-slug";
    let run_uid = "01HXPERTASKLOGS00000000001";
    let run_id = RunId(7);

    // Install the routing subscriber globally for the duration of this test
    // process: the driver future runs on a spawned `JoinSet` task (a different
    // thread under the multi-thread runtime), so a thread-local default would not
    // see its events. `set_global_default` (rather than the `!Send` guard from
    // `set_default`) keeps this `async` test future `Send`; each integration-test
    // file is its own process, so the process-global install is safe.
    let subscriber = registry().with(TestRunFileLayer::new(repo_root.clone()));
    tracing::subscriber::set_global_default(subscriber).expect("set global tracing subscriber");

    // NoopBackend: developer responds with any text, reviewer approves.
    let backend: Arc<dyn AgentBackend> = Arc::new(NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]));

    let graph = Arc::new(tokio::sync::Mutex::new(TaskGraph {
        slug: slug.into(),
        tasks: vec![task(task_id_str)],
    }));

    let worktree_manager = WorktreeManager::new(repo_root.clone(), "develop".into());

    let config = Config::resolve(GlobalConfig::default(), ProjectConfig::default());

    let control = RunControl {
        run: run_id,
        sink: Arc::new(|_| {}),
        pause: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel: tokio_util::sync::CancellationToken::new(),
    };

    let report = run_graph(
        Arc::clone(&graph),
        worktree_manager,
        config,
        backend,
        control,
        Arc::new(NoopAuditRegistry),
        slug.to_string(),
        run_uid.to_string(),
        String::new(),
        Arc::new(StructuredTextInterpreter::new()),
    )
    .await
    .expect("run_graph must not error");

    // Task must reach Done so we know the driver ran past the transition + gate
    // emissions.
    assert_eq!(
        report.outcomes,
        vec![(TaskId::new(task_id_str), TaskState::Done)],
        "task must reach Done via run_graph"
    );

    // ── Assert: the per-task log file exists and holds the task's records ────────
    let task_log = makina_core::paths::task_log(&repo_root, run_uid, task_id_str);
    assert!(
        task_log.exists(),
        "per-task log file must exist at {task_log:?}"
    );

    let contents = std::fs::read_to_string(&task_log)
        .unwrap_or_else(|e| panic!("read per-task log {task_log:?}: {e}"));

    assert!(
        contents.contains("task state transition"),
        "per-task log must contain the task's state transitions, got: {contents:?}"
    );
    assert!(
        contents.contains("gates passed"),
        "per-task log must contain a gate-output line (gates ran), got: {contents:?}"
    );
}
