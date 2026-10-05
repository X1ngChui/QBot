//! Wall-clock instants as plain UTC milliseconds.

use serde::{Deserialize, Serialize};

/// A UTC instant in milliseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMillis(i64);

impl UnixMillis {
    pub const fn new(millis: i64) -> Self {
        Self(millis)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

impl UnixMillis {
    pub fn plus(self, duration: std::time::Duration) -> Self {
        let millis = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
        Self(self.0.saturating_add(millis))
    }

    /// Time from `earlier` to `self`, or zero if `earlier` is later.
    pub fn since(self, earlier: UnixMillis) -> std::time::Duration {
        let diff = self.0.saturating_sub(earlier.0);
        std::time::Duration::from_millis(u64::try_from(diff).unwrap_or(0))
    }
}
