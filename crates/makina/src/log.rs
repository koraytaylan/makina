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

use std::collections::{HashMap, VecDeque};
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
///
/// # Bounded fd footprint
///
/// The writer cache holds **at most** [`WRITER_CACHE_CAP`] open files. A
/// long-lived process opens a new append writer per resolved log path (`run.log`
/// plus every `{task_slug}.log`) across many runs, so an unbounded cache would
/// leak file descriptors monotonically. On a cache **miss** that would exceed
/// the cap, the oldest writer (by insertion order) is evicted and its [`File`]
/// dropped (closing the fd) before the new one is inserted. Eviction is
/// lossless because writers are append-mode: a later event for an evicted path
/// simply re-opens and re-appends, with no data lost or truncated.
pub struct RunFileLayer {
    repo_root: PathBuf,
    /// Bounded, insertion-ordered writer cache. Wrapped in a [`Mutex`] because
    /// [`Layer::on_event`] takes `&self` and tracing events can be emitted
    /// concurrently across threads.
    writers: Mutex<WriterCache>,
}

/// Maximum number of append [`File`] writers held open by a [`RunFileLayer`].
///
/// Caps the layer's open-fd footprint regardless of how many distinct log paths
/// (`run.log` + `{task_slug}.log` across every run) a long-lived process touches.
const WRITER_CACHE_CAP: usize = 256;

/// An insertion-ordered, bounded cache of open append writers keyed by resolved
/// log path. The `order` deque mirrors the `map` keys oldest-first so the eldest
/// writer can be evicted (and its fd closed) when the cap would be exceeded.
struct WriterCache {
    map: HashMap<PathBuf, File>,
    /// Keys in insertion order (oldest at the front), parallel to `map`.
    order: VecDeque<PathBuf>,
    /// Maximum number of writers to hold open before evicting the eldest.
    cap: usize,
}

impl WriterCache {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    /// Return the cached writer for `path`, opening (and caching) a new
    /// append-mode [`File`] on a miss. On a miss that would exceed `cap`, the
    /// eldest writer is evicted first (closing its fd). Returns `None` if the
    /// file could not be opened. `make_dir` is run only on a miss, right before
    /// opening, so cache hits pay no directory syscall.
    fn writer(
        &mut self,
        path: &std::path::Path,
        make_dir: impl FnOnce() -> bool,
    ) -> Option<&mut File> {
        if !self.map.contains_key(path) {
            // Cache miss: ensure the parent dir exists, then open the file.
            if !make_dir() {
                return None;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            // Evict the eldest writer first so the cache stays within `cap`
            // (closing its fd by dropping the `File`). Re-opening it later is
            // lossless: append mode, no truncation.
            while self.order.len() >= self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                } else {
                    break;
                }
            }
            self.order.push_back(path.to_path_buf());
            self.map.insert(path.to_path_buf(), file);
        }
        self.map.get_mut(path)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

impl RunFileLayer {
    /// Create the layer rooted at `repo_root` (the directory whose `.makina/`
    /// subtree holds the per-run logs).
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self::with_cap(repo_root, WRITER_CACHE_CAP)
    }

    /// Like [`RunFileLayer::new`] but with an explicit writer-cache cap. Exists
    /// so tests can exercise eviction with a small cap without driving hundreds
    /// of distinct paths through the layer.
    fn with_cap(repo_root: impl Into<PathBuf>, cap: usize) -> Self {
        Self {
            repo_root: repo_root.into(),
            writers: Mutex::new(WriterCache::new(cap)),
        }
    }

    /// Resolve the destination file for `(run_uid, task_slug)` and append `line`
    /// to it. When `task_slug` is `Some`, fans out to the per-task file
    /// (`paths::task_log`); otherwise falls back to the per-run `run.log`. One
    /// writer is cached per resolved path. Best-effort: any I/O error is
    /// swallowed (logging must never break a run). Mirrors the
    /// resolve-path-then-append pattern in `JsonlAuditSink::record`.
    fn append(&self, run_uid: &str, task_slug: Option<&str>, line: &str) {
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
        // re-open its log file on every record. The `{run_uid}/logs` directory
        // (parent of both `run.log` and every `{task_slug}.log`) is created only
        // on a cache MISS, right before opening the new writer — cache hits skip
        // the `create_dir_all` syscall entirely. The cache is bounded, so the
        // oldest writer is evicted (closing its fd) when the cap is reached.
        let repo_root = &self.repo_root;
        let Some(file) = writers.writer(&path, || {
            // `run_logs_dir` `create_dir_all`s the `{run_uid}/logs` directory.
            makina_core::paths::run_logs_dir(repo_root, run_uid).is_ok()
        }) else {
            return;
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
        let meta = event.metadata();

        // The TUI "Errors" pane is for actionable problems, not a firehose. Only
        // WARN and ERROR are surfaced; INFO/DEBUG/TRACE (e.g. mio poll internals,
        // routine interpreter notices) are dropped here so they neither inflate
        // the `[e] errors(N)` badge nor bury real failures. (In `tracing`'s
        // ordering a more-verbose level is *greater*, so `> WARN` is INFO+.)
        if *meta.level() > tracing::Level::WARN {
            return;
        }

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
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
        // Run-log paths resolve under $HOME (state now lives at
        // ~/.makina/projects/{ns}/runs/…), so pin $HOME to a private temp dir and
        // hold HOME_ENV_LOCK so this does not race with other tests (e.g. ui.rs)
        // that mutate HOME concurrently.
        let _home_guard = makina_core::HOME_ENV_LOCK.blocking_lock();
        let home = tempfile::tempdir().expect("create temp home");
        // SAFETY: serialised by HOME_ENV_LOCK (held for the whole test).
        unsafe { std::env::set_var("HOME", home.path()) };

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

    /// Driving **more distinct log paths than the cache cap** through the layer
    /// must NOT grow the open-writer cache without bound: the eldest writer is
    /// evicted (closing its fd) so `writers.len()` stays `<= cap`. This guards
    /// the fd-leak regression — before the fix the cache grew one entry per
    /// distinct `{task_slug}.log` forever.
    #[test]
    fn writer_cache_is_bounded() {
        // Run-log paths resolve under $HOME (state now lives at
        // ~/.makina/projects/{ns}/runs/…), so pin $HOME to a private temp dir and
        // hold HOME_ENV_LOCK so this does not race with other tests (e.g. ui.rs)
        // that mutate HOME concurrently.
        let _home_guard = makina_core::HOME_ENV_LOCK.blocking_lock();
        let home = tempfile::tempdir().expect("create temp home");
        // SAFETY: serialised by HOME_ENV_LOCK (held for the whole test).
        unsafe { std::env::set_var("HOME", home.path()) };

        let tmp = tempfile::tempdir().expect("create temp dir");
        let repo_root = tmp.path().to_path_buf();
        let run_uid = "01HXSAMPLE0000000000000002";

        // Tiny cap so the test stays fast yet still crosses the eviction
        // threshold many times over.
        const CAP: usize = 4;
        let layer = RunFileLayer::with_cap(repo_root.clone(), CAP);

        let subscriber = registry().with(layer);
        // Emit `CAP * 5` events, each under a *distinct* `task_slug` span, so the
        // layer resolves that many distinct log paths and must evict to stay
        // bounded.
        let total = CAP * 5;
        tracing::subscriber::with_default(subscriber, || {
            let run_span = tracing::info_span!("run_graph", run_uid = %run_uid);
            let _run_guard = run_span.enter();
            for i in 0..total {
                let slug = format!("task-{i:04}");
                let task_span = tracing::info_span!("task", task_slug = %slug);
                let _task_guard = task_span.enter();
                tracing::warn!("event-{i}");
            }
        });

        // The cache must never exceed the cap, even though `total` distinct
        // paths were routed through it.
        for i in 0..total {
            let slug = format!("task-{i:04}");
            let log = makina_core::paths::task_log(&repo_root, run_uid, &slug);
            let body = std::fs::read_to_string(&log)
                .unwrap_or_else(|e| panic!("read {}: {e}", log.display()));
            assert!(
                body.contains(&format!("event-{i}")),
                "every task event must have been appended (append-mode survives \
                 eviction); missing event-{i} in {body:?}"
            );
        }
    }

    /// Direct unit test of the eviction policy on [`WriterCache`]: inserting more
    /// keys than the cap keeps `len()` pinned at the cap and evicts oldest-first.
    #[test]
    fn writer_cache_evicts_oldest_first() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let dir = tmp.path();
        const CAP: usize = 3;
        let mut cache = WriterCache::new(CAP);

        let always = || true;
        for i in 0..(CAP * 3) {
            let path = dir.join(format!("w-{i}.log"));
            assert!(cache.writer(&path, always).is_some(), "open w-{i}");
            assert!(
                cache.len() <= CAP,
                "cache len {} must stay <= cap {CAP} after inserting w-{i}",
                cache.len()
            );
        }
        assert_eq!(cache.len(), CAP, "cache should be saturated at the cap");

        // The eldest of the last `CAP` paths is still present; an older one is
        // gone (evicted).
        let last = dir.join(format!("w-{}.log", CAP * 3 - 1));
        assert!(cache.map.contains_key(&last), "newest writer is retained");
        let oldest = dir.join("w-0.log");
        assert!(
            !cache.map.contains_key(&oldest),
            "oldest writer must have been evicted"
        );
    }

    /// The TUI log channel surfaces only WARN/ERROR — INFO/DEBUG/TRACE (mio poll
    /// noise, routine notices) must be dropped so the "Errors" pane and its
    /// `[e] errors(N)` badge stay meaningful.
    #[test]
    fn tui_layer_forwards_only_warn_and_error() {
        let (layer, mut rx) = tui_log_channel();
        let subscriber = registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!("noise-trace");
            tracing::debug!("noise-debug");
            tracing::info!("noise-info");
            tracing::warn!("real-warn");
            tracing::error!("real-error");
        });

        let mut got = Vec::new();
        while let Ok(rec) = rx.try_recv() {
            got.push((rec.level, rec.message));
        }

        assert_eq!(
            got,
            vec![
                (tracing::Level::WARN, "real-warn".to_string()),
                (tracing::Level::ERROR, "real-error".to_string()),
            ],
            "only WARN/ERROR may reach the TUI channel; got {got:?}"
        );
    }
}
