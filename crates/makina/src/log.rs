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
//! 1. On `on_new_span`, reads the span's `run_uid` and/or `task_slug` fields (if
//!    present) and stashes them in the span's [extensions]; and
//! 2. On `on_event`, walks the event's span scope to find the nearest stashed
//!    `run_uid` (and nearest `task_slug`), resolves the destination via
//!    [`makina_core::paths`], and **appends** the formatted line — to the
//!    per-task file `{run_uid}/logs/{task_slug}.log` when a `task_slug` span is
//!    in scope, otherwise to the per-run `{run_uid}/logs/run.log`.
//!
//! The resolve-path-then-append shape mirrors `JsonlAuditSink::record`: a
//! best-effort `create_dir_all` + an `OpenOptions` append. Any I/O failure is
//! swallowed — logging must never break a run.
//!
//! # Per-task routing (task `log-per-task-routing-layer`)
//!
//! Span-field-to-distinct-file routing is **not** a built-in
//! `tracing_subscriber` feature, so [`RunFileLayer`] does it: the per-task span
//! (`tracing::info_span!("task", task_slug = %driver_id.0)`) carries a
//! `task_slug` field, stashed alongside `run_uid` in the span extensions. On
//! each event the layer walks the scope and, if a `task_slug` is in scope, fans
//! the record out to `paths::task_log(repo_root, run_uid, task_slug)`; events
//! with no task span fall back to the per-run `run.log`. One writer is
//! opened/cached per `task_slug` (and one for the run-level log) so repeated
//! events do not re-open the file on every record.
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

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;

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

/// The span-extension value: the `task_slug` a span (and its children) belong
/// to. Set by the per-task driver span (`info_span!("task", task_slug = …)`),
/// it keys the per-task log-file fan-out in [`RunFileLayer::on_event`].
#[derive(Clone)]
struct TaskSlug(String);

/// A [`Visit`]or that pulls the `run_uid` / `task_slug` strings out of a span's
/// fields or an event's fields. Records both the routing keys (for spans) and a
/// flattened human-readable message buffer (for events).
#[derive(Default)]
struct FieldVisitor {
    /// Captured `run_uid`, if the visited field set carried one.
    run_uid: Option<String>,
    /// Captured `task_slug`, if the visited field set carried one.
    task_slug: Option<String>,
    /// Accumulated `key=value` text for all other fields (event rendering).
    message: String,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "run_uid" => self.run_uid = Some(value.to_owned()),
            "task_slug" => self.task_slug = Some(value.to_owned()),
            _ => self.record_debug(field, &value),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // The `%run_uid` / `%task_slug` (`Display`) forms arrive here as Debug;
        // capture them too, stripping the `Debug` quoting.
        match field.name() {
            "run_uid" => {
                self.run_uid = Some(format!("{value:?}").trim_matches('"').to_owned());
            }
            "task_slug" => {
                self.task_slug = Some(format!("{value:?}").trim_matches('"').to_owned());
            }
            "message" => {
                // Render the message without a `message=` prefix for readability.
                let _ = write!(self.message, "{value:?} ");
            }
            name => {
                let _ = write!(self.message, "{name}={value:?} ");
            }
        }
    }
}

/// A per-run / per-task file logging layer (see the module docs).
///
/// Built over a `repo_root`; each event is appended either to the per-task file
/// `{repo_root}/.makina/runs/{run_uid}/logs/{task_slug}.log` (when a `task_slug`
/// span is in scope) or to the per-run `{repo_root}/.makina/runs/{run_uid}/logs/
/// run.log`, where `run_uid` / `task_slug` are read from the current span scope.
///
/// One append-mode [`File`] writer is opened and **cached** per destination
/// (keyed by its resolved path), so a hot task does not re-open its log on every
/// record.
pub struct RunFileLayer {
    repo_root: PathBuf,
    /// Open append writers, keyed by their resolved log-file path. Wrapped in a
    /// [`Mutex`] because [`Layer::on_event`] takes `&self` and tracing events
    /// can be emitted concurrently across threads.
    writers: Mutex<HashMap<PathBuf, File>>,
}

impl RunFileLayer {
    /// Create the layer rooted at `repo_root` (the directory whose `.makina/`
    /// subtree holds the per-run logs).
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            writers: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve the destination file for `(run_uid, task_slug)` and append `line`
    /// to it. When `task_slug` is `Some`, fans out to the per-task file
    /// (`paths::task_log`); otherwise falls back to the per-run `run.log`. One
    /// writer is cached per resolved path. Best-effort: any I/O error is
    /// swallowed (logging must never break a run). Mirrors the
    /// resolve-path-then-append pattern in `JsonlAuditSink::record`.
    fn append(&self, run_uid: &str, task_slug: Option<&str>, line: &str) {
        // `run_logs_dir` `create_dir_all`s the `{run_uid}/logs` directory (the
        // parent of both `run.log` and every `{task_slug}.log`).
        let Ok(_logs_dir) = makina_core::paths::run_logs_dir(&self.repo_root, run_uid) else {
            return;
        };
        let path = match task_slug {
            Some(slug) => makina_core::paths::task_log(&self.repo_root, run_uid, slug),
            None => makina_core::paths::run_dir(&self.repo_root, run_uid)
                .join("logs")
                .join("run.log"),
        };

        let Ok(mut writers) = self.writers.lock() else {
            return;
        };
        // Open-and-cache one writer per resolved path so a hot task does not
        // re-open its log file on every record.
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

impl<S> Layer<S> for RunFileLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    /// Stash the span's `run_uid` and/or `task_slug` (if any) into its
    /// extensions so child events can find them by walking the span scope.
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

    /// Route the event to its file: find the nearest `run_uid` (required) and
    /// the nearest `task_slug` (optional) in the event's span scope, render the
    /// event, and append it. When a `task_slug` is in scope the record fans out
    /// to that task's `{task_slug}.log`; otherwise it falls back to `run.log`.
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Walk from the innermost span outward; the first stashed key wins.
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
        self.append(&run_uid, task_slug.as_deref(), &line);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::registry;

    /// Two events emitted under two distinct `task_slug` spans each land in
    /// their own `{task_slug}.log`, while an event emitted with no task span (but
    /// still inside the `run_uid` span) falls back to the per-run `run.log`.
    #[test]
    fn per_task_routing() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let repo_root = tmp.path().to_path_buf();
        let run_uid = "01HXSAMPLE0000000000000001";

        let subscriber = registry().with(RunFileLayer::new(repo_root.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let run_span = tracing::info_span!("run_graph", run_uid = %run_uid);
            let _run_guard = run_span.enter();

            // Event under the per-task span for `task-a`.
            {
                let task_span = tracing::info_span!("task", task_slug = %"task-a");
                let _task_guard = task_span.enter();
                tracing::warn!("alpha");
            }

            // Event under the per-task span for `task-b`.
            {
                let task_span = tracing::info_span!("task", task_slug = %"task-b");
                let _task_guard = task_span.enter();
                tracing::warn!("bravo");
            }

            // Event with no task span — falls back to the per-run log.
            tracing::warn!("runlevel");
        });

        let task_a_log = makina_core::paths::task_log(&repo_root, run_uid, "task-a");
        let task_b_log = makina_core::paths::task_log(&repo_root, run_uid, "task-b");
        let run_log = makina_core::paths::run_dir(&repo_root, run_uid)
            .join("logs")
            .join("run.log");

        let a = std::fs::read_to_string(&task_a_log).expect("read task-a log");
        let b = std::fs::read_to_string(&task_b_log).expect("read task-b log");
        let run = std::fs::read_to_string(&run_log).expect("read run log");

        // Each task event lands in its own file, and only there.
        assert!(
            a.contains("alpha"),
            "task-a log must contain \"alpha\", got: {a:?}"
        );
        assert!(
            !a.contains("bravo") && !a.contains("runlevel"),
            "task-a log must contain ONLY its own event, got: {a:?}"
        );

        assert!(
            b.contains("bravo"),
            "task-b log must contain \"bravo\", got: {b:?}"
        );
        assert!(
            !b.contains("alpha") && !b.contains("runlevel"),
            "task-b log must contain ONLY its own event, got: {b:?}"
        );

        // The span-less (task-less) event falls back to the per-run log.
        assert!(
            run.contains("runlevel"),
            "run-level log must contain \"runlevel\", got: {run:?}"
        );
        assert!(
            !run.contains("alpha") && !run.contains("bravo"),
            "run-level log must NOT contain per-task events, got: {run:?}"
        );
    }
}
