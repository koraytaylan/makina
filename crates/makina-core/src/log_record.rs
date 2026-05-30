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
}

impl LogRecord {
    /// Construct a [`LogRecord`] with the given parts, stamping it with the
    /// current wall-clock time.
    pub fn now(level: tracing::Level, message: String, target: String) -> Self {
        Self {
            timestamp: Utc::now(),
            level,
            message,
            target,
        }
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
    }
}
