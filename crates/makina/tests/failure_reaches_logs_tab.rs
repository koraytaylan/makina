//! A reported failure must end up in the Logs tab.
//!
//! This closes the gap that made "Open failed: invalid command" impossible to
//! diagnose. Failures were written to a one-line status slot that clipped them
//! to the terminal width and was overwritten by the next message, and they were
//! never handed to `tracing` at all — so the Logs tab, the one surface built to
//! keep a durable record, stayed empty for exactly the events an operator would
//! open it to read.
//!
//! The test drives the whole loop rather than any one half of it: install the
//! real [`makina::log::TuiLogLayer`], raise a failure through `App::update`, and
//! drain the channel back into the app the way the event loop does. Asserting
//! only that `update` calls `tracing::error!` would not prove the record
//! survives the layer; asserting only that the layer forwards records would not
//! prove failures are handed to it.

use std::sync::Arc;

use async_trait::async_trait;
use makina::app::{App, AppEvent, FailureNotice};
use makina_core::api::{Api, ApiError, Command, CommandOutcome, EventStream, RunId, RunView};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry;

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

fn test_app() -> App {
    let api: Arc<dyn Api> = Arc::new(StubApi);
    App::new(api, vec![], std::path::PathBuf::from("."))
}

/// Raising a failure writes it through `tracing` into the Logs tab's buffer,
/// with the **whole** reason intact, and raises the modal.
#[test]
fn a_reported_failure_reaches_the_logs_tab_in_full() {
    let (layer, mut rx) = makina::log::tui_log_channel();
    let mut app = test_app();

    // The exact shape that used to be unreadable: an `ApiError::InvalidCommand`
    // renders as `invalid command: {reason}`, and the reason is the only part
    // worth having.
    let detail = "invalid command: plan `0007-Demo` is already open from \
                  `docs/plans/0007-Demo`; refusing a second live run that would \
                  share artifacts or worktrees";

    let subscriber = registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        app.update(AppEvent::ReportFailure(FailureNotice::new(
            "Could not start 0007-Demo",
            detail,
        )));
    });

    assert!(app.is_failure_notice(), "the modal must be raised");

    // Drain the channel back into the app exactly as the event loop's arm does.
    let record = rx
        .try_recv()
        .expect("the failure must have been forwarded to the TUI log channel");
    assert_eq!(
        record.level,
        tracing::Level::ERROR,
        "a failure must be logged at ERROR so it is visible at every filter level"
    );
    app.update(AppEvent::LogRecordArrived {
        record: Box::new(record),
    });

    let logged = app
        .filtered_log_entries()
        .into_iter()
        .find(|entry| entry.message.contains("Could not start 0007-Demo"))
        .expect("the failure must be in the Logs tab buffer");

    assert!(
        logged.message.contains("refusing a second live run"),
        "the tail of the reason must survive into the log — truncating it is the \
         defect this guards; got: {:?}",
        logged.message
    );
}
