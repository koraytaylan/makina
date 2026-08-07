//! Tracing→mpsc **TUI-channel** layer (task `log-subscriber-tui-channel`).
//!
//! The TUI's Logs tab shows everything the process logs. Rather than poll the
//! per-run log files, the [`makina::log::TuiLogLayer`] converts each
//! `tracing::Event` into a self-contained
//! [`makina_core::log_record::LogRecord`] and `try_send`s it onto a **bounded**
//! `tokio::sync::mpsc` channel the TUI drains in its `tokio::select!` loop.
//!
//! These tests prove three things:
//! 1. the TUI-channel layer composes onto the same registry as the per-run file
//!    layer (the two-layer `.with(file).with(tui)` shape `main.rs` installs)
//!    builds and initializes without panicking;
//! 2. an emitted `warn!` is forwarded as a `LogRecord` whose message contains
//!    the event text; and
//! 3. composed with the file layer exactly as `main.rs` composes them, a record
//!    still reaches the channel carrying its span-scope attribution — the two
//!    layers share one stash, and it has to survive their composition.

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

/// With both layers composed the way `main.rs` composes them, a record still
/// arrives attributed to its run and task.
///
/// Both layers stash the span's routing keys into the same span extensions, so
/// this is the composition that would break first if one of them stopped — and
/// the Logs tab's project/task filters would silently have nothing to match on.
#[test]
fn attribution_survives_the_two_layer_composition() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let (tui_layer, mut rx) = tui_log_channel();

    let subscriber = registry()
        .with(RunFileLayer::new(tmp.path().to_path_buf()))
        .with(tui_layer);
    tracing::subscriber::with_default(subscriber, || {
        let run = tracing::info_span!(
            "run_graph",
            run_uid = %"01HXCOMPOSED000000000001",
            project_root = %"/repo/alpha",
        );
        let _run_guard = run.enter();
        let task = tracing::info_span!("task", task_slug = %"task-a");
        let _task_guard = task.enter();
        tracing::info!("composed");
    });

    let record = rx.try_recv().expect("the event must reach the TUI channel");
    assert_eq!(record.message, "composed");
    assert_eq!(record.run_uid.as_deref(), Some("01HXCOMPOSED000000000001"));
    assert_eq!(record.task_slug.as_deref(), Some("task-a"));
    assert_eq!(
        record.project_root.as_deref(),
        Some(std::path::Path::new("/repo/alpha"))
    );
}
