//! Integration test for **log-tracing-transition-events** — verifies that the
//! supervisor's `task_driver` / `develop_until_gates_pass` emit structured
//! `tracing` events at the state-transition and gate-output sites, so a per-task
//! `tracing` subscriber has something to capture.
//!
//! # Strategy
//!
//! 1. Install a capturing `tracing` collector (a small custom `Subscriber`) as
//!    the process-global default.  It records, for every event, the message
//!    (`"task state transition"` / `"gates passed"` / `"gate failed"`) and the
//!    `task`, `from`, `to`, `gate`, and `exit_code` field values.
//! 2. Drive a single task through `run_graph` (the orchestrator's real path)
//!    with `NoopBackend` and a temp repo — mirrors
//!    `crates/makina-core/tests/supervisor_audit_registry.rs`.
//! 3. Assert the captured events include the task's state transitions
//!    (Ready → InProgress, InProgress → InReview) and at least one gate-output
//!    record (`"gates passed"`, emitted by `develop_until_gates_pass` on the
//!    `GateOutcome::Passed` arm — the default config has an empty gate list, so
//!    gates pass on the first round).
//!
//! These emissions are **additive**: they do not change `EventSink` behavior.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::DefaultGuard;
use tracing::{Event, Metadata, Subscriber};

use makina_core::actors::{RunControl, run_graph};
use makina_core::api::RunId;
use makina_core::audit::NoopAuditRegistry;
use makina_core::backend::AgentBackend;
use makina_core::backend::noop::NoopBackend;
use makina_core::config::{Config, GlobalConfig, ProjectConfig};
use makina_core::interpreter::StructuredTextInterpreter;
use makina_core::task::{Task, TaskGraph, TaskId, TaskState};
use makina_core::worktree::WorktreeManager;

// ── Capturing tracing collector ───────────────────────────────────────────────

/// One captured `tracing` event: its message plus the string-formatted values of
/// the fields we care about (`task`, `from`, `to`, `gate`, `exit_code`).
#[derive(Debug, Clone)]
struct CapturedEvent {
    message: String,
    fields: HashMap<String, String>,
}

impl CapturedEvent {
    fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }
}

/// A `Visit`or that records every field as `name -> formatted-value`.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: HashMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            // The event's message is recorded under the `message` field as a
            // `Debug` value; strip the surrounding quotes for readability.
            self.message = rendered.trim_matches('"').to_string();
        } else {
            self.fields.insert(field.name().to_string(), rendered);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
}

/// Process-global capturing collector: records every event into a shared `Vec`.
#[derive(Clone, Default)]
struct CapturingCollector {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl CapturingCollector {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn captured(&self) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .expect("collector mutex must not be poisoned")
            .clone()
    }
}

impl Subscriber for CapturingCollector {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attrs: &Attributes<'_>) -> Id {
        // No span tracking needed; hand back a stable, non-zero id.
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("collector mutex must not be poisoned")
            .push(CapturedEvent {
                message: visitor.message,
                fields: visitor.fields,
            });
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// Install the collector as the **process-global** default subscriber so events
/// emitted from the `JoinSet`'s worker threads (not just the test thread) are
/// captured.  Returns the guard; events are captured for the guard's lifetime.
fn install(collector: CapturingCollector) -> DefaultGuard {
    tracing::subscriber::set_default(collector)
}

// ── Temp-repo helpers (mirror supervisor_audit_registry.rs) ────────────────────

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

// ── Acceptance test ────────────────────────────────────────────────────────────

/// A capturing `tracing` collector installed over a single `run_graph` task with
/// `NoopBackend` in a `tempdir` must capture the task's state transitions and at
/// least one gate-output record.
#[tokio::test(flavor = "current_thread")]
async fn run_graph_emits_tracing_transition_and_gate_events() {
    let repo_dir = setup_temp_repo();
    let repo_root = repo_dir.path().to_path_buf();

    let task_id_str = "trace-task";
    let slug = "trace-slug";
    let run_uid = "trace-run-uid";
    let run_id = RunId(7);

    // NoopBackend: developer responds, reviewer approves → task runs to Done.
    let backend: Arc<dyn AgentBackend> = Arc::new(NoopBackend::with_responses(vec![
        "Implemented the feature.".into(),
        r#"{"verdict":"approve"}"#.into(),
    ]));

    let collector = CapturingCollector::new();
    let probe = collector.clone();
    // Hold the guard for the whole run so worker-thread events are captured.
    let _guard = install(collector);

    let graph = Arc::new(tokio::sync::Mutex::new(TaskGraph {
        slug: slug.into(),
        tasks: vec![task(task_id_str)],
    }));

    let worktree_manager = WorktreeManager::new(repo_root.clone(), "develop".into());

    // Default config → empty gate list → gates pass on the first round.
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
        Arc::clone(&backend),
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

    assert_eq!(
        report.outcomes,
        vec![(TaskId::new(task_id_str), TaskState::Done)],
        "task must reach Done via run_graph"
    );

    let events = probe.captured();

    // Helper: was a `task state transition` event captured for our task with the
    // given `from`/`to` (matched against the `Debug` form of the `TaskState`)?
    let saw_transition = |from: &str, to: &str| {
        events.iter().any(|e| {
            e.message == "task state transition"
                && e.field("task") == Some(task_id_str)
                && e.field("from") == Some(from)
                && e.field("to") == Some(to)
        })
    };

    assert!(
        saw_transition("Ready", "InProgress"),
        "must capture the Ready → InProgress transition; got {events:#?}"
    );
    assert!(
        saw_transition("InProgress", "InReview"),
        "must capture the InProgress → InReview transition; got {events:#?}"
    );

    // At least one gate-output record. With the default (empty) gate list the
    // gates pass on the first round, emitting `"gates passed"`.
    let saw_gate_output = events.iter().any(|e| {
        (e.message == "gates passed" || e.message == "gate failed")
            && e.field("task") == Some(task_id_str)
    });
    assert!(
        saw_gate_output,
        "must capture at least one gate-output record (`gates passed` or `gate failed`); got {events:#?}"
    );
}
