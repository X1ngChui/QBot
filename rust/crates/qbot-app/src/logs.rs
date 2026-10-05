//! Recent warnings and errors kept in memory for `/logs`, next to the normal log output.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use qbot_commands::{LogLine, RecentLogs};
use qbot_core::UnixMillis;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// The newest warnings and errors, at most `capacity` of them.
#[derive(Debug)]
pub struct LogBuffer {
    lines: Mutex<VecDeque<LogLine>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            lines: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
            capacity,
        })
    }

    fn push(&self, line: LogLine) {
        if self.capacity == 0 {
            return;
        }
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == self.capacity {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// A `tracing` layer that feeds this buffer.
    pub fn layer(self: &Arc<Self>) -> CaptureLayer {
        CaptureLayer(Arc::clone(self))
    }
}

impl RecentLogs for LogBuffer {
    fn recent(&self, limit: usize) -> Vec<LogLine> {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        let skip = lines.len().saturating_sub(limit);
        lines.iter().skip(skip).cloned().collect()
    }
}

/// Collects an event's message and fields into one line.
struct OneLine(String);

impl Visit for OneLine {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.insert_str(0, value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.insert_str(0, &format!("{value:?}"));
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

#[derive(Debug)]
pub struct CaptureLayer(Arc<LogBuffer>);

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let level = *event.metadata().level();
        // ERROR sorts below WARN: everything at WARN or more severe.
        if level > Level::WARN {
            return;
        }
        let mut text = OneLine(String::new());
        event.record(&mut text);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        self.0.push(LogLine {
            at: UnixMillis::new(now),
            level: level.as_str().to_owned(),
            text: text.0,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    #[test]
    fn warnings_and_errors_are_kept_newest_last_within_capacity() {
        let buffer = LogBuffer::new(2);
        let subscriber = tracing_subscriber::registry().with(buffer.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("not kept");
            tracing::warn!(group = 7, "first");
            tracing::error!(error = "boom", "second");
            tracing::warn!("third");
        });
        let lines = buffer.recent(10);
        assert_eq!(
            lines
                .iter()
                .map(|l| (l.level.as_str(), l.text.as_str()))
                .collect::<Vec<_>>(),
            [("ERROR", "second error=boom"), ("WARN", "third")]
        );
        assert_eq!(buffer.recent(1).len(), 1);
        assert_eq!(buffer.recent(1)[0].text, "third");
    }
}
