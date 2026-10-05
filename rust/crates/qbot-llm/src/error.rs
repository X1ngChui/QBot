use std::time::Duration;

use crate::conversation::ConversationError;

/// The only error type the upper layers see. Vendor errors are mapped here by the adapter.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LlmError {
    #[error("authentication failed")]
    Auth,
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("provider overloaded or unavailable")]
    Unavailable,
    #[error("request timed out")]
    Timeout,
    #[error("network failure: {0}")]
    Network(String),
    #[error("provider rejected the request: {0}")]
    InvalidRequest(String),
    #[error("conversation exceeds the model context window")]
    ContextTooLong,
    #[error("response blocked by a content filter")]
    ContentFiltered,
    #[error("the stream ended before completion")]
    StreamInterrupted,
    /// The provider answered, but not in a shape the contract allows.
    #[error("malformed provider output: {0}")]
    Protocol(String),
    #[error("provider does not support {0}")]
    Unsupported(&'static str),
    /// The caller handed over a conversation that breaks the contract.
    #[error("invalid conversation: {0}")]
    InvalidConversation(#[from] ConversationError),
    /// The account's plan allowance is used up; retrying will not help until it renews.
    #[error("provider allowance used up: {0}")]
    QuotaExhausted(String),
    #[error("provider error {status}: {message}")]
    Other { status: u16, message: String },
}

impl LlmError {
    /// Whether repeating the same request may succeed. Only transient failures qualify.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            LlmError::RateLimited { .. }
                | LlmError::Unavailable
                | LlmError::Timeout
                | LlmError::Network(_)
        )
    }

    /// Short stable label for metrics.
    pub fn class(&self) -> &'static str {
        match self {
            LlmError::Auth => "auth",
            LlmError::RateLimited { .. } => "rate_limited",
            LlmError::Unavailable => "unavailable",
            LlmError::Timeout => "timeout",
            LlmError::Network(_) => "network",
            LlmError::InvalidRequest(_) => "invalid_request",
            LlmError::ContextTooLong => "context_too_long",
            LlmError::ContentFiltered => "content_filtered",
            LlmError::StreamInterrupted => "stream_interrupted",
            LlmError::Protocol(_) => "protocol",
            LlmError::Unsupported(_) => "unsupported",
            LlmError::InvalidConversation(_) => "invalid_conversation",
            LlmError::QuotaExhausted(_) => "quota_exhausted",
            LlmError::Other { .. } => "other",
        }
    }
}
