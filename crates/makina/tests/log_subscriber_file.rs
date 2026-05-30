//! Per-run **file** layer of the tracing subscriber (task `log-subscriber-file`).
//!
//! The subscriber is installed once at process startup, but run ids are
//! allocated lazily per `OpenRun` and several runs can be open at once — so the
//! file destination cannot be a static path chosen at install time. The
//! [`makina::log::RunFileLayer`] resolves it per event from the current span's
//! `run_uid` field, appending each event to
//! `.makina/runs/{run_uid}/logs/run.log` (the dir resolved via
//! `makina_core::paths::run_logs_dir`).
//!
//! This test proves the routing end to end: install the layer over a temp
//! repo-root, enter a span carrying a sample `run_uid`, emit a `warn!`, and
//! assert the per-run log file contains the message.

use makina::log::RunFileLayer;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry;

/// Installing the file layer and emitting a `warn!` inside a `run_uid`-carrying
/// span writes the message to that run's `.makina/runs/{run_uid}/logs/` file.
#[test]
fn writes_warn_to_per_run_log() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let repo_root = tmp.path().to_path_buf();
    let run_uid = "01HXSAMPLE0000000000000001";

    // Build the subscriber with ONLY the per-run file layer, rooted at the temp
    // repo, and install it for the duration of the closure.
    let subscriber = registry().with(RunFileLayer::new(repo_root.clone()));
    tracing::subscriber::with_default(subscriber, || {
        // Enter a span carrying the sample `run_uid` so the event is routed.
        let span = tracing::info_span!("run_graph", run_uid = %run_uid);
        let _guard = span.enter();
        tracing::warn!("probe");
    });

    // The layer writes synchronously (std::fs append) — no async flush needed —
    // but `with_default` has also dropped the subscriber by now, so all writes
    // have completed. Resolve the same path the layer used and assert content.
    let logs_dir = makina_core::paths::run_dir(&repo_root, run_uid).join("logs");
    let log_file = logs_dir.join("run.log");

    assert!(
        log_file.exists(),
        "expected per-run log file at {}",
        log_file.display()
    );

    let contents = std::fs::read_to_string(&log_file).expect("read per-run log file");
    assert!(
        contents.contains("probe"),
        "per-run log under {} must contain \"probe\", got: {contents:?}",
        logs_dir.display()
    );
}
