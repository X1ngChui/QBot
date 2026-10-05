//! The source of "now". Injected everywhere so time-dependent behavior is testable.

use crate::time::UnixMillis;

pub trait Clock: Send + Sync {
    fn now(&self) -> UnixMillis;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UnixMillis {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        UnixMillis::new(millis)
    }
}
