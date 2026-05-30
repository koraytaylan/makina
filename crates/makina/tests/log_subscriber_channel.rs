//! Tracing→mpsc **TUI-channel** layer (task `log-subscriber-tui-channel`).
//!
//! The TUI surfaces `warn!`/`error!` (and friends) in its error/log pane. Rather
//! than poll the per-run log files, the [`makina::log::TuiLogLayer`] converts
//! each `tracing::Event` into a self-contained
//! [`makina_core::log_record::LogRecord`] and `try_send`s it onto a **bounded**
//! `tokio::sync::mpsc` channel the TUI drains in its `tokio::select!` loop.
//!
//! These tests prove two things:
//! 1. the TUI-channel layer composes onto the same registry as the per-run file
//!    layer (the two-layer `.with(file).with(tui)` shape `main.rs` installs)
//!    builds and initializes without panicking; and
//! 2. an emitted `warn!` is forwarded as a `LogRecord` whose message contains
//!    the event text.

use makina::log::{RunFileLayer, tui_log_channel};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Composing the per-run **file** layer and the **TUI-channel** layer onto one
/// registry and `.try_init()`-ing it succeeds (`Ok`) and does not panic — this
/// is the exact `main.rs` shape: `registry().with(file).with(tui).init()`.
///
/// `try_init` (rather than `init`) is used so a global subscriber already
/// installed by another test in the same process does not abort this one; we
/// assert it returns `Ok` (this is the first/only global install in this test
/// binary's process).
#[test]
fn composes_two_layers_without_panic() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let (tui_layer, _rx) = tui_log_channel();

    let result = registry()
        .with(RunFileLayer::new(tmp.path().to_path_buf()))
        .with(tui_layer)
        .try_init();

    assert!(
        result.is_ok(),
        "composing the file + TUI-channel layers and try_init()-ing must succeed, got {result:?}"
    );
}

/// Installing the TUI-channel layer and emitting `warn!("probe")` forwards a
/// `LogRecord` onto the channel whose message contains `"probe"`.
///
/// Uses `with_default` (a scoped subscriber) so it is independent of any global
/// subscriber installed by [`composes_two_layers_without_panic`]; the layer
/// writes synchronously (`try_send`), so the record is available via
/// `rx.try_recv()` immediately after the event is emitted.
#[test]
fn forwards_warn_to_channel() {
    let (tui_layer, mut rx) = tui_log_channel();

    let subscriber = registry().with(tui_layer);
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!("probe");
    });

    let record = rx
        .try_recv()
        .expect("the warn! event must have been forwarded onto the channel");
    assert_eq!(
        record.level,
        tracing::Level::WARN,
        "the forwarded record must carry the WARN level"
    );
    assert!(
        record.message.contains("probe"),
        "the forwarded record's message must contain \"probe\", got: {:?}",
        record.message
    );
}
