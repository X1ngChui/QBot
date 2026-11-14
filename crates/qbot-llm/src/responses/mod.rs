//! Adapter for DeepSeek's Responses API.
//!
//! The adapter turns the normalized [`Request`](crate::Request) into a Responses call and the
//! reply back into a normalized [`Response`](crate::Response). The API keeps no state: every
//! call carries the full conversation, rebuilt deterministically from the transcript, with the
//! provider's own raw output items echoed back so reasoning stays paired with its message. It
//! has no developer role (such a message goes as a user message) and takes images only as
//! uploaded files.

mod adapter;
mod parse;
mod retry;
pub mod sim;
mod transport;
mod wire;

use std::time::Duration;

pub use adapter::ResponsesProvider;
pub use retry::post_with_retry;
pub use transport::{HttpResponse, KeyResolver, KeySource, ReqwestTransport, Transport, Upload};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Extra attempts after the first, for transient failures before any response body.
    pub retries: u32,
    pub base: Duration,
    pub jitter: bool,
    /// Longest one HTTP request may take before it counts as timed out and is tried again. A
    /// provider that accepts a connection and then sends nothing would otherwise use up the
    /// whole call's deadline on its first attempt. `None` leaves an attempt to the call deadline.
    pub attempt_timeout: Option<Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            retries: 2,
            base: Duration::from_millis(500),
            jitter: true,
            attempt_timeout: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesConfig {
    pub model: String,
    /// Deadline for one whole call, retries included. `None` leaves deadlines to the caller,
    /// as the agent run does with its own.
    pub timeout: Option<Duration>,
    pub retry: RetryPolicy,
}

impl ResponsesConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            timeout: None,
            retry: RetryPolicy::default(),
        }
    }
}
