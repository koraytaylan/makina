//! Tracing-subscriber layers for Makina.
//!
//! This module provides two [`tracing_subscriber::Layer`]s composed onto one
//! registry in [`crate::main`]:
//!
//! - [`RunFileLayer`] — the per-run **file** layer (span-keyed routing), and
//! - [`TuiLogLayer`] — the **TUI-channel** layer that `try_send`s a
//!   [`makina_core::log_record::LogRecord`] per event onto a bounded mpsc
//!   channel the TUI drains. Build it (plus its receiver) with
//!   [`tui_log_channel`].
//!
//! # Why a custom file layer
//!
//! The subscriber is installed **once** at process startup ([`crate::main`]),
//! but run ids are allocated **lazily** per `OpenRun` (the orchestrator's
//! `next_id`/`run_uid`) and several runs can be open at once. So the file
//! destination **cannot** be a single static path chosen at install time —
//! it must be resolved per event from the run the event belongs to.
//!
//! This module provides [`RunFileLayer`], a [`tracing_subscriber::Layer`] that:
//!
//! 1. On `on_new_span`, reads the span's `run_uid` field (if present) and
//!    stashes it in the span's [extensions]; and
//! 2. On `on_event`, walks the event's span scope to find the nearest stashed
//!    `run_uid`, resolves the per-run logs dir via
//!    [`makina_core::paths::run_logs_dir`], and **appends** the formatted line
//!    to `{run_uid}/logs/run.log`.
//!
//! The resolve-path-then-append shape mirrors `JsonlAuditSink::record`: a
//! best-effort `create_dir_all` (via the paths helper) + an `OpenOptions`
//! append. Any I/O failure is swallowed — logging must never break a run.
//!
//! # MVP scope
//!
//! Concurrent runs are supported via **span-keyed routing**: each event is
//! routed to the file of whichever run's span it was emitted inside, so two
//! runs open at once write to two distinct `run.log` files. An event emitted
//! **outside** any `run_uid`-carrying span is dropped by this layer (it has no
//! run to attribute it to); such events still reach any other installed layer.
//!
//! [extensions]: tracing_subscriber::registry::SpanRef::extensions

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;

use makina_core::log_record::LogRecord;
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Capacity of the bounded tracing→TUI channel.
///
/// Bounded so a stalled / slow TUI consumer cannot grow unbounded memory: when
/// the channel is full the [`TuiLogLayer`] **drops** the record rather than
/// block the emitting thread (logging must never stall a run).
pub const TUI_LOG_CHANNEL_CAPACITY: usize = 256;

/// The span-extension value: the `run_uid` a span (and its children) belong to.
#[derive(Clone)]
struct RunUid(String);

/// A [`Visit`]or that pulls a `run_uid` string out of a span's fields or an
/// event's fields. Records both the `run_uid` key (for spans) and a flattened
/// human-readable message buffer (for events).
#[derive(Default)]
struct FieldVisitor {
    /// Captured `run_uid`, if the visited field set carried one.
    run_uid: Option<String>,
    /// Accumulated `key=value` text for all other fields (event rendering).
    message: String,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "run_uid" {
            self.run_uid = Some(value.to_owned());
        } else {
            self.record_debug(field, &value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // The `%run_uid` / `Display` form arrives here as Debug; capture it too.
        if field.name() == "run_uid" {
            self.run_uid = Some(format!("{value:?}").trim_matches('"').to_owned());
            return;
        }
        if field.name() == "message" {
            // Render the message without a `message=` prefix for readability.
            let _ = write!(self.message, "{value:?} ");
        } else {
            let _ = write!(self.message, "{}={value:?} ", field.name());
        }
    }
}

/// A per-run file logging layer (see the module docs).
///
/// Built over a `repo_root`; each event is appended to
/// `{repo_root}/.makina/runs/{run_uid}/logs/run.log`, where `run_uid` is read
/// from the current span scope.
pub struct RunFileLayer {
    repo_root: PathBuf,
}

impl RunFileLayer {
    /// Create the layer rooted at `repo_root` (the directory whose `.makina/`
    /// subtree holds the per-run logs).
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    /// Resolve the per-run log file and append `line` to it. Best-effort: any
    /// I/O error is swallowed (logging must never break a run). Mirrors the
    /// resolve-path-then-append pattern in `JsonlAuditSink::record`.
    fn append(&self, run_uid: &str, line: &str) {
        // `run_logs_dir` `create_dir_all`s and returns the logs directory.
        let Ok(dir) = makina_core::paths::run_logs_dir(&self.repo_root, run_uid) else {
            return;
        };
        let path = dir.join("run.log");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

impl<S> Layer<S> for RunFileLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    /// Stash the span's `run_uid` (if any) into its extensions so child events
    /// can find it by walking the span scope.
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        if let Some(run_uid) = visitor.run_uid
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(RunUid(run_uid));
        }
    }

    /// Route the event to its run's file: find the nearest `run_uid` in the
    /// event's span scope, render the event, and append it.
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Walk from the innermost span outward; the first stashed RunUid wins.
        let run_uid = ctx.event_scope(event).and_then(|scope| {
            scope.from_root().fold(None, |acc, span| {
                span.extensions()
                    .get::<RunUid>()
                    .map(|r| r.0.clone())
                    .or(acc)
            })
        });

        let Some(run_uid) = run_uid else {
            // No run to attribute this event to — drop it from the file layer.
            return;
        };

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let meta = event.metadata();
        let line = format!(
            "{} {} {}",
            meta.level(),
            meta.target(),
            visitor.message.trim_end()
        );
        self.append(&run_uid, &line);
    }
}

// ── TUI channel layer (task log-subscriber-tui-channel) ─────────────────────────

/// A tracing [`Layer`] that forwards each event to the TUI over a **bounded**
/// [`tokio::sync::mpsc`] channel.
///
/// # Why a channel layer
///
/// The TUI's error/log pane needs to surface `warn!`/`error!` (and friends) as
/// they happen. Rather than have the TUI poll the per-run log files, this layer
/// converts each `tracing::Event` into a self-contained
/// [`makina_core::log_record::LogRecord`] and `try_send`s it onto a channel the
/// TUI drains in its `tokio::select!` loop.
///
/// # Bounded, non-blocking, re-entrancy-safe
///
/// The channel is **bounded** ([`TUI_LOG_CHANNEL_CAPACITY`]). On a full channel
/// the record is **dropped** ([`mpsc::error::TrySendError::Full`]) — the layer
/// never blocks the emitting thread. Critically, the drop path does **not**
/// itself emit a `tracing::warn!` (or any tracing event): doing so from inside
/// the layer would re-enter the subscriber and could recurse. The dropped
/// record is simply discarded.
///
/// Unlike [`RunFileLayer`], this layer does **not** require a `run_uid` span:
/// every event (even those emitted outside any run span) is forwarded, so
/// process-level diagnostics still reach the TUI.
pub struct TuiLogLayer {
    sender: mpsc::Sender<LogRecord>,
}

impl TuiLogLayer {
    /// Create the layer from the sending half of a bounded channel.
    ///
    /// Pair with [`tui_log_channel`], which builds the channel at the spec's
    /// capacity and hands the [`mpsc::Receiver`] to the TUI.
    pub fn new(sender: mpsc::Sender<LogRecord>) -> Self {
        Self { sender }
    }
}

impl<S> Layer<S> for TuiLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let meta = event.metadata();
        let record = LogRecord::now(
            *meta.level(),
            visitor.message.trim_end().to_owned(),
            meta.target().to_owned(),
        );

        // Non-blocking: on a full channel, DROP the record. Do NOT emit a
        // tracing event here (it would re-enter this layer and could recurse).
        let _ = self.sender.try_send(record);
    }
}

/// Build the bounded tracing→TUI channel at the spec capacity
/// ([`TUI_LOG_CHANNEL_CAPACITY`]).
///
/// Returns the [`TuiLogLayer`] (compose it into the subscriber registry) and the
/// [`mpsc::Receiver`] (hand it to the TUI event loop to drain).
pub fn tui_log_channel() -> (TuiLogLayer, mpsc::Receiver<LogRecord>) {
    let (tx, rx) = mpsc::channel(TUI_LOG_CHANNEL_CAPACITY);
    (TuiLogLayer::new(tx), rx)
}
