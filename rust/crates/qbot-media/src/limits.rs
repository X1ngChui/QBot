use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

/// At most `limit` events in any window of the given length.
#[derive(Debug)]
pub struct SlidingWindow {
    window: Duration,
    hits: VecDeque<Instant>,
}

impl SlidingWindow {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            hits: VecDeque::new(),
        }
    }

    fn trim(&mut self, now: Instant) {
        while self
            .hits
            .front()
            .is_some_and(|t| now.duration_since(*t) > self.window)
        {
            self.hits.pop_front();
        }
    }

    pub fn is_idle(&mut self) -> bool {
        self.trim(Instant::now());
        self.hits.is_empty()
    }

    /// Use one slot if fewer than `limit` are in use.
    pub fn take(&mut self, limit: u32) -> bool {
        let now = Instant::now();
        self.trim(now);
        if self.hits.len() >= usize::try_from(limit).unwrap_or(usize::MAX) {
            return false;
        }
        self.hits.push_back(now);
        true
    }
}
