//! Adapter for the OpenAI Responses wire format, with a DeepSeek dialect.
//!
//! The adapter turns the normalized [`Request`](crate::Request) into a Responses call and the
//! reply back into a normalized [`Response`](crate::Response). What the vendor lacks is
//! emulated here: a stateless API gets the full conversation every call, rebuilt
//! deterministically from the transcript, with the provider's own raw output items echoed back
//! so reasoning stays paired with its message.

mod adapter;
mod parse;
mod retry;
pub mod sim;
mod stream;
mod transport;
mod wire;

use std::time::Duration;

pub use adapter::ResponsesProvider;
pub use retry::post_with_retry;
pub use transport::{
    ByteStream, HttpBody, HttpResponse, KeyResolver, KeySource, ReqwestTransport, Transport, Upload,
};

/// Vendor dialect differences the wire layer handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Standard,
    /// No developer role; different reasoning-effort strings; no server-side state.
    DeepSeek,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateMode {
    /// `store: false`; every call carries the full conversation.
    Stateless,
    /// `store: true`; follow-up calls send only new items plus `previous_response_id`.
    ServerState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Extra attempts after the first, for transient failures before any response body.
    pub retries: u32,
    pub base: Duration,
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            retries: 2,
            base: Duration::from_millis(500),
            jitter: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsesConfig {
    pub model: String,
    pub flavor: Flavor,
    pub state: StateMode,
    /// Deadline for one whole call, retries included. `None` leaves deadlines to the caller,
    /// as the agent run does with its own.
    pub timeout: Option<Duration>,
    pub retry: RetryPolicy,
}

impl ResponsesConfig {
    pub fn deepseek(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            flavor: Flavor::DeepSeek,
            state: StateMode::Stateless,
            timeout: None,
            retry: RetryPolicy::default(),
        }
    }

    pub fn standard(model: impl Into<String>, state: StateMode) -> Self {
        Self {
            model: model.into(),
            flavor: Flavor::Standard,
            state,
            timeout: None,
            retry: RetryPolicy::default(),
        }
    }
}
