//! A self-contained, transport-friendly record of a single tracing event.
//!
//! [`LogRecord`] is the unit the TUI log channel carries: the `makina` binary's
//! tracing→mpsc layer (task `log-subscriber-tui-channel`) converts each
//! `tracing::Event` into one of these and `try_send`s it onto a bounded channel
//! that the error/log pane drains. Keeping it here in `makina-core` (rather than
//! in the TUI crate) lets both the producer (the layer) and any future consumer
//! refer to the same shape, and keeps it independent of any ratatui types.
//!
//! It is deliberately **owned and self-describing**: a `String` message and
//! target plus a captured timestamp and [`tracing::Level`], so it can outlive
//! the borrowed `tracing::Event` it was built from and cross a channel boundary.
//!
//! # Attribution
//!
//! Beyond the message itself a record carries **where it came from**: the
//! project root, run uid, and task slug of the span scope it was emitted
//! inside. The TUI's Logs tab filters on exactly those axes, so they have to
//! travel with the record — by the time the TUI drains the channel the span
//! scope that produced the event is long gone. Every field is optional because
//! process-level diagnostics are emitted outside any run span.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

/// A single captured log event, owned and ready to send across a channel.
///
/// Produced by the tracing→mpsc TUI layer from a `tracing::Event`; consumed by
/// the TUI to render the error/log pane.
#[derive(Debug, Clone)]
pub struct LogRecord {
    /// When the event was captured (wall-clock UTC).
    pub timestamp: DateTime<Utc>,
    /// The event's verbosity level (`ERROR`/`WARN`/`INFO`/`DEBUG`/`TRACE`).
    pub level: tracing::Level,
    /// The flattened, human-readable message (the event's `message` field plus
    /// any other rendered fields).
    pub message: String,
    /// The event's target (typically the emitting module path).
    pub target: String,
    /// The repository root of the project this event belongs to, when it was
    /// emitted inside a run span that carried one.
    pub project_root: Option<PathBuf>,
    /// The run this event belongs to, when emitted inside a `run_uid` span.
    pub run_uid: Option<String>,
    /// The task this event belongs to, when emitted inside a `task_slug` span.
    pub task_slug: Option<String>,
}

impl LogRecord {
    /// Construct a [`LogRecord`] with the given parts, stamping it with the
    /// current wall-clock time and no span attribution.
    ///
    /// Use [`LogRecord::with_scope`] to attach the project/run/task the event
    /// was emitted under.
    pub fn now(level: tracing::Level, message: String, target: String) -> Self {
        Self {
            timestamp: Utc::now(),
            level,
            message,
            target,
            project_root: None,
            run_uid: None,
            task_slug: None,
        }
    }

    /// Attach the span-scope attribution resolved for this event.
    #[must_use]
    pub fn with_scope(
        mut self,
        project_root: Option<PathBuf>,
        run_uid: Option<String>,
        task_slug: Option<String>,
    ) -> Self {
        self.project_root = project_root;
        self.run_uid = run_uid;
        self.task_slug = task_slug;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_captures_fields_and_a_timestamp() {
        let before = Utc::now();
        let rec = LogRecord::now(
            tracing::Level::WARN,
            "probe".to_string(),
            "makina::log".to_string(),
        );
        let after = Utc::now();

        assert_eq!(rec.level, tracing::Level::WARN);
        assert_eq!(rec.message, "probe");
        assert_eq!(rec.target, "makina::log");
        assert!(rec.timestamp >= before && rec.timestamp <= after);
        assert_eq!(rec.project_root, None, "no scope attached by `now`");
        assert_eq!(rec.run_uid, None);
        assert_eq!(rec.task_slug, None);
    }

    #[test]
    fn with_scope_attaches_attribution() {
        let rec = LogRecord::now(
            tracing::Level::INFO,
            "probe".to_string(),
            "makina::log".to_string(),
        )
        .with_scope(
            Some(PathBuf::from("/repo")),
            Some("01HXRUN".to_string()),
            Some("task-a".to_string()),
        );

        assert_eq!(
            rec.project_root.as_deref(),
            Some(std::path::Path::new("/repo"))
        );
        assert_eq!(rec.run_uid.as_deref(), Some("01HXRUN"));
        assert_eq!(rec.task_slug.as_deref(), Some("task-a"));
    }
}
