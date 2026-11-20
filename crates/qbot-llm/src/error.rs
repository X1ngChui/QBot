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
    /// The request is longer than the model's context window. `limit` and `requested` are the
    /// sizes in tokens when the provider's message states them.
    #[error("conversation exceeds the model context window")]
    ContextTooLong {
        limit: Option<u64>,
        requested: Option<u64>,
    },
    #[error("response blocked by a content filter")]
    ContentFiltered,
    /// The provider answered, but not in a shape the contract allows.
    #[error("malformed provider output: {0}")]
    Protocol(String),
    /// The model wrote a tool call whose arguments cannot be read (not JSON, or not an object),
    /// for example a quotation mark left unescaped inside a string. The answer is lost, not
    /// wrong: nothing ran, and the same request asked again usually comes back readable.
    #[error("unreadable tool call: {0}")]
    MalformedCall(String),
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
    /// A context-length rejection, with the sizes its message states. Providers word it as
    /// "maximum context length is N tokens ... you requested M tokens" (or "resulted in M").
    pub fn context_too_long(message: &str) -> Self {
        let lower = message.to_ascii_lowercase();
        let number_after = |marker: &str| -> Option<u64> {
            let at = lower.find(marker)? + marker.len();
            let digits: String = lower[at..]
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .filter(char::is_ascii_digit)
                .collect();
            digits.parse().ok()
        };
        LlmError::ContextTooLong {
            limit: number_after("context length is")
                .or_else(|| number_after("context window is"))
                .or_else(|| number_after("maximum context length")),
            requested: number_after("requested")
                .or_else(|| number_after("resulted in"))
                .or_else(|| number_after("your request has")),
        }
    }

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
            LlmError::ContextTooLong { .. } => "context_too_long",
            LlmError::ContentFiltered => "content_filtered",
            LlmError::Protocol(_) => "protocol",
            LlmError::MalformedCall(_) => "malformed_call",
            LlmError::Unsupported(_) => "unsupported",
            LlmError::InvalidConversation(_) => "invalid_conversation",
            LlmError::QuotaExhausted(_) => "quota_exhausted",
            LlmError::Other { .. } => "other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LlmError;

    #[test]
    fn a_context_length_message_gives_its_sizes_when_it_states_them() {
        assert_eq!(
            LlmError::context_too_long(
                "This model's maximum context length is 1,048,576 tokens. However, you requested \
                 1200000 tokens (1199000 in the messages, 1000 in the completion)."
            ),
            LlmError::ContextTooLong {
                limit: Some(1_048_576),
                requested: Some(1_200_000)
            }
        );
        assert_eq!(
            LlmError::context_too_long(
                "maximum context length is 65536 tokens, however your messages resulted in 70000 tokens"
            ),
            LlmError::ContextTooLong {
                limit: Some(65_536),
                requested: Some(70_000)
            }
        );
        assert_eq!(
            LlmError::context_too_long("the input is too long"),
            LlmError::ContextTooLong {
                limit: None,
                requested: None
            }
        );
    }
}
