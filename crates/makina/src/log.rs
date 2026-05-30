//! Per-run **file** layer of the tracing subscriber.
//!
//! # Why a custom layer
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

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

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
