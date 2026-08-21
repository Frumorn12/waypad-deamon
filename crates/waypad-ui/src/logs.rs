//! A ring buffer of recent log lines, for the control panel to display.
//!
//! The panel exists so a user never has to open a terminal, which means the
//! diagnostics have to come to them. On Windows there is no `journalctl` to
//! fall back on and a daemon started from the Run key has no console at all, so
//! without this its logs go nowhere a person can reach.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tracing::{Event, Level, Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

/// How many lines are kept. Enough to cover a failed pairing or a stream that
/// would not start, and small enough that it is never worth thinking about.
const CAPACITY: usize = 400;

#[derive(Debug, Clone, serde::Serialize)]
pub struct LogLine {
    pub level: String,
    pub target: String,
    pub message: String,
    /// Milliseconds since the process started. Wall-clock time would need a
    /// formatting dependency to say anything the panel can render, and elapsed
    /// time is what someone reading a failure actually wants.
    pub at_ms: u64,
}

/// A handle both the tracing layer and the panel hold.
#[derive(Clone, Debug)]
pub struct LogBuffer {
    lines: Arc<Mutex<VecDeque<LogLine>>>,
    started: std::time::Instant,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::with_capacity(CAPACITY))),
            started: std::time::Instant::now(),
        }
    }

    pub fn push(&self, level: &str, target: &str, message: String) {
        let line = LogLine {
            level: level.to_string(),
            target: target.to_string(),
            message,
            at_ms: self.started.elapsed().as_millis() as u64,
        };
        // A poisoned lock loses the log rather than the daemon: diagnostics are
        // never worth taking the process down for.
        if let Ok(mut lines) = self.lines.lock() {
            if lines.len() == CAPACITY {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    }

    /// The lines recorded after `since_ms`, oldest first.
    ///
    /// Filtered by timestamp rather than by index so the panel can poll without
    /// tracking how many lines were evicted between requests.
    pub fn since(&self, since_ms: u64) -> Vec<LogLine> {
        self.lines
            .lock()
            .map(|lines| {
                lines
                    .iter()
                    .filter(|line| line.at_ms >= since_ms)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Feeds every event into a [`LogBuffer`], alongside whatever else is installed.
pub struct LogBufferLayer {
    buffer: LogBuffer,
}

impl LogBufferLayer {
    pub fn new(buffer: LogBuffer) -> Self {
        Self { buffer }
    }
}

impl<S> Layer<S> for LogBufferLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let metadata = event.metadata();
        self.buffer.push(
            level_name(*metadata.level()),
            metadata.target(),
            visitor.finish(),
        );
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    }
}

/// Renders an event's fields into one line.
///
/// The `message` field leads and everything else follows as `key=value`, which
/// is how the same events read in a terminal — a user comparing the panel with
/// a console should not have to translate between two formats.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<String>,
}

impl MessageVisitor {
    fn finish(self) -> String {
        if self.fields.is_empty() {
            return self.message;
        }
        if self.message.is_empty() {
            return self.fields.join(" ");
        }
        format!("{} {}", self.message, self.fields.join(" "))
    }

    fn record(&mut self, name: &str, value: String) {
        if name == "message" {
            self.message = value;
        } else {
            self.fields.push(format!("{name}={value}"));
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.record(field.name(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record(field.name(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record(field.name(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.record(field.name(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record(field.name(), value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_most_recent_lines() {
        let buffer = LogBuffer::new();
        for index in 0..CAPACITY + 50 {
            buffer.push("info", "test", format!("line {index}"));
        }
        let lines = buffer.since(0);
        assert_eq!(lines.len(), CAPACITY);
        // The oldest are the ones dropped: a user looking at a failure wants
        // what just happened, not what happened an hour ago.
        assert_eq!(lines[0].message, format!("line {}", 50));
        assert_eq!(
            lines.last().unwrap().message,
            format!("line {}", CAPACITY + 49)
        );
    }

    #[test]
    fn since_filters_by_timestamp_so_polling_needs_no_cursor() {
        let buffer = LogBuffer::new();
        buffer.push("info", "test", "first".into());
        let after_first = buffer.since(0).last().unwrap().at_ms;
        std::thread::sleep(std::time::Duration::from_millis(5));
        buffer.push("warn", "test", "second".into());

        let recent = buffer.since(after_first + 1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].message, "second");
        assert_eq!(recent[0].level, "warn");
    }

    #[test]
    fn a_message_and_its_fields_read_as_one_line() {
        let mut visitor = MessageVisitor::default();
        visitor.record("message", "screen stream started".into());
        visitor.record("fps", "60".into());
        visitor.record("codec", "h264".into());
        assert_eq!(visitor.finish(), "screen stream started fps=60 codec=h264");
    }

    #[test]
    fn an_event_with_only_fields_still_says_something() {
        let mut visitor = MessageVisitor::default();
        visitor.record("error", "device gone".into());
        assert_eq!(visitor.finish(), "error=device gone");
    }
}
