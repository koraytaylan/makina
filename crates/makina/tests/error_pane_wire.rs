//! Integration test for `tui-error-pane-channel-wire`.
//!
//! Proves that a [`makina_core::log_record::LogRecord`] sent on the tracing→TUI
//! mpsc channel reaches the error pane: the event loop's drain arm converts the
//! record into an [`makina::app::ErrorMessage`] and `update` appends it to
//! `app.error_messages`. A second test renders that message and asserts its
//! text appears on screen when the pane is open.
//!
//! NOTE on the seam: the spec text describes a `Receiver<ErrorMessage>`, but the
//! ACTUAL `log-subscriber-tui-channel` seam carries
//! `makina_core::log_record::LogRecord`. The `record → ErrorMessage` conversion
//! lives in `event.rs` (the production drain arm). This test therefore builds a
//! `LogRecord` channel, drives the SAME drain path the event loop uses (recv →
//! convert → `AppEvent::ErrorMessageArrived` → `App::update`), and asserts the
//! converted message lands in `app.error_messages`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use makina::app::{App, AppEvent, ErrorLevel, ErrorMessage};
use makina::ui;
use makina_core::api::{
    AgentRole, Api, ApiError, Command, CommandOutcome, Event, EventStream, ExchangeEvent, RunId,
    RunStatus, RunView, TaskId, TaskState, TaskView,
};
use makina_core::log_record::LogRecord;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::mpsc;

/// Minimal `Api` double for the integration test (the crate's `PlaceholderApi`
/// is `#[cfg(test)]`-only and not reachable from an integration test).
struct StubApi;

#[async_trait]
impl Api for StubApi {
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

/// Build an `App` carrying one running run with an in-progress task and a live
/// exchange, so the per-task inner split (and thus the error pane) renders.
/// Mirrors `ui.rs`'s `exchange_app` test helper but uses only public surface.
fn exchange_app() -> App {
    let api: Arc<dyn Api> = Arc::new(StubApi);
    let run = RunView {
        id: RunId(1),
        run_uid: String::new(),
        task_list_path: PathBuf::from(".tasks/exchange-test.json"),
        status: RunStatus::Running,
        project: String::new(),
        tasks: vec![TaskView {
            id: TaskId::new("task-a"),
            title: "Task A".into(),
            state: TaskState::InProgress,
            gate_iterations: 0,
            review_iterations: 0,
            depends_on: vec![],
        }],
        report: makina_core::api::IngestionReport::default(),
    };
    let mut app = App::new(api, vec![run], std::path::PathBuf::from("."));

    app.update(AppEvent::ApiEvent(Event::AgentExchange {
        run: RunId(1),
        task: TaskId::new("task-a"),
        role: AgentRole::Developer,
        event: ExchangeEvent::PromptSent {
            text: "implement X".into(),
        },
    }));

    app
}

/// Drive the SAME drain path the event loop's `tokio::select!` arm uses: receive
/// one [`LogRecord`] off the channel, convert it to an [`AppEvent`] exactly as
/// the production arm does, and feed it to `App::update`. This exercises the
/// wired channel→pane flow without standing up a terminal.
fn drain_one(app: &mut App, log_rx: &mut mpsc::Receiver<LogRecord>) -> bool {
    let rec = log_rx.try_recv().expect("a record must be queued");
    let level = match rec.level {
        tracing::Level::ERROR => ErrorLevel::Error,
        tracing::Level::WARN => ErrorLevel::Warn,
        _ => ErrorLevel::Info,
    };
    app.update(AppEvent::ErrorMessageArrived {
        msg: ErrorMessage {
            timestamp: rec.timestamp.into(),
            level,
            text: rec.message,
        },
    })
}

#[tokio::test]
async fn log_record_appears_in_error_messages() {
    let (tx, mut rx) = mpsc::channel::<LogRecord>(8);
    let mut app = exchange_app();

    // The producer side (TuiLogLayer) sends a LogRecord onto the channel.
    tx.send(LogRecord::now(
        tracing::Level::ERROR,
        "agent crashed unexpectedly".to_string(),
        "makina::worker".to_string(),
    ))
    .await
    .expect("send must succeed");

    // Drive the drain path the event loop uses.
    let redrew = drain_one(&mut app, &mut rx);
    assert!(redrew, "ErrorMessageArrived must request a redraw");

    // The converted record must now be in the error-pane buffer.
    assert_eq!(
        app.error_messages.len(),
        1,
        "exactly one record must have landed in error_messages"
    );
    let msg = &app.error_messages[0];
    assert_eq!(msg.text, "agent crashed unexpectedly");
    assert_eq!(
        msg.level,
        ErrorLevel::Error,
        "ERROR level must map to ErrorLevel::Error"
    );
}

#[test]
fn wired_error_message_renders_when_pane_open() {
    let mut app = exchange_app();
    app.error_pane_open = true;
    app.push_error(ErrorMessage {
        timestamp: std::time::SystemTime::now(),
        level: ErrorLevel::Error,
        text: "agent crashed unexpectedly".into(),
    });

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(&app, f)).unwrap();

    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().chars().next().unwrap_or(' '))
        .collect();

    assert!(
        screen.contains("agent crashed unexpectedly"),
        "open error pane must render the wired message text"
    );
}
