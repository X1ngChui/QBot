//! Recent warnings and errors kept in memory for `/logs`, next to the normal log output.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use qbot_commands::{LogLine, RecentLogs};
use qbot_core::{GroupId, UnixMillis};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// Recent warnings and errors `/logs` can show. They also go to the normal log output; this
/// is only the tail an owner can read from the chat.
pub const KEPT_LINES: usize = 200;

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
    fn recent(&self, group: GroupId, limit: usize) -> Vec<LogLine> {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        let mut shown: Vec<LogLine> = lines
            .iter()
            .rev()
            .filter(|line| line.group.is_none_or(|g| g == group))
            .take(limit)
            .cloned()
            .collect();
        shown.reverse();
        shown
    }
}

/// Collects an event's message and fields, and the group it names.
///
/// Messages are fixed text and numbers are ids and counts. Text fields (an error, a detail) can
/// quote a group's chat, so they are kept only on a line about a group, which `/logs` shows in
/// that group alone; a line about no group keeps its message and numbers, and its full text is
/// in the process log.
#[derive(Default)]
struct OneLine {
    message: String,
    numbers: String,
    texts: String,
    group: Option<GroupId>,
}

impl OneLine {
    fn number(&mut self, field: &Field, value: impl std::fmt::Display) {
        let _ = write!(self.numbers, " {}={value}", field.name());
    }

    fn text(&self) -> String {
        match self.group {
            Some(_) => format!("{}{}{}", self.message, self.numbers, self.texts),
            None => format!("{}{}", self.message, self.numbers),
        }
    }
}

impl Visit for OneLine {
    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "group" {
            self.group = GroupId::new(value).ok();
        }
        self.number(field, value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "group" {
            self.group = i64::try_from(value).ok().and_then(|v| GroupId::new(v).ok());
        }
        self.number(field, value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.number(field, value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            let _ = write!(self.texts, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            let _ = write!(self.texts, " {}={value:?}", field.name());
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
        let mut line = OneLine::default();
        event.record(&mut line);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        self.0.push(LogLine {
            at: UnixMillis::new(now),
            group: line.group,
            level: level.as_str().to_owned(),
            text: line.text(),
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
        let g = GroupId::new(7).unwrap();
        let lines = buffer.recent(g, 10);
        assert_eq!(
            lines
                .iter()
                .map(|l| (l.level.as_str(), l.text.as_str()))
                .collect::<Vec<_>>(),
            [("ERROR", "second"), ("WARN", "third")]
        );
        assert_eq!(buffer.recent(g, 1).len(), 1);
        assert_eq!(buffer.recent(g, 1)[0].text, "third");
    }

    #[test]
    fn a_group_sees_its_own_lines_and_those_about_no_group_only() {
        let buffer = LogBuffer::new(10);
        let subscriber = tracing_subscriber::registry().with(buffer.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(group = 7_i64, "here");
            tracing::warn!(group = 8_u64, detail = "their chat", "there");
            tracing::error!(attempts = 3, error = "a quote of some chat", "everywhere");
        });
        let texts = |g: i64| {
            buffer
                .recent(GroupId::new(g).unwrap(), 10)
                .into_iter()
                .map(|l| l.text)
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(7), ["here group=7", "everywhere attempts=3"]);
        assert_eq!(
            texts(8),
            ["there group=8 detail=their chat", "everywhere attempts=3"]
        );
        assert_eq!(
            texts(9),
            ["everywhere attempts=3"],
            "a line about no group carries no text that could quote a chat"
        );
    }
}
