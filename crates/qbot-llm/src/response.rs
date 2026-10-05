use std::ops::AddAssign;
use std::time::Duration;

use qbot_context::AssistantTurn;
use qbot_core::CallId;
use serde::{Deserialize, Serialize};

use crate::capability::ProviderId;
use crate::conversation::{ConvDigest, ConvItem, Conversation, digest_items};

/// Prompt-cache accounting. Absence is typed, never a silent zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheUsage {
    NotReported,
    Reported { hit_tokens: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// All input tokens, cached or not.
    pub input_tokens: u32,
    /// All output tokens, including reasoning when the provider counts it there.
    pub output_tokens: u32,
    pub cache: CacheUsage,
    /// Informational; already included in `output_tokens`.
    pub reasoning_tokens: Option<u32>,
    /// False when the provider sent no usage block; the counts are then zero, not measured.
    pub reported: bool,
}

impl Usage {
    pub const ZERO: Usage = Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache: CacheUsage::Reported { hit_tokens: 0 },
        reasoning_tokens: None,
        reported: true,
    };

    pub fn cached_tokens(&self) -> Option<u32> {
        match self.cache {
            CacheUsage::Reported { hit_tokens } => Some(hit_tokens),
            CacheUsage::NotReported => None,
        }
    }

    pub fn uncached_input_tokens(&self) -> Option<u32> {
        self.cached_tokens()
            .map(|hit| self.input_tokens.saturating_sub(hit))
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, other: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache = match (self.cache, other.cache) {
            (CacheUsage::Reported { hit_tokens: a }, CacheUsage::Reported { hit_tokens: b }) => {
                CacheUsage::Reported {
                    hit_tokens: a.saturating_add(b),
                }
            }
            _ => CacheUsage::NotReported,
        };
        self.reasoning_tokens = match (self.reasoning_tokens, other.reasoning_tokens) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        self.reported = self.reported && other.reported;
    }
}

/// Why generation stopped. A content-filter stop is an error, not a finish reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The model finished its turn with no tool calls.
    Stop,
    /// The turn ends in tool calls that need results.
    ToolCalls,
    /// The output limit cut the turn short. Complete parts are kept; no truncated call survives.
    Length,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseMeta {
    pub model: String,
    pub provider_response_id: Option<String>,
    pub latency: Duration,
    /// HTTP-level attempts, counting the one that succeeded.
    pub attempts: u32,
}

/// Opaque state a provider needs to continue a conversation. Callers store it and pass it back
/// unchanged; whether it carries a server handle or nothing at all is the adapter's business.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Continuation {
    pub(crate) provider: ProviderId,
    pub(crate) model: String,
    /// Number of conversation items the provider has already seen, including this response.
    pub(crate) covers: usize,
    pub(crate) digest: ConvDigest,
    pub(crate) handle: Option<String>,
}

impl Continuation {
    pub(crate) fn after(
        provider: ProviderId,
        model: String,
        conversation: &Conversation,
        turn: &AssistantTurn,
        handle: Option<String>,
    ) -> Self {
        let mut items: Vec<ConvItem> = conversation.items().to_vec();
        items.push(ConvItem::Assistant(turn.clone()));
        Self {
            provider,
            model,
            covers: items.len(),
            digest: digest_items(&items),
            handle,
        }
    }

    pub fn handle(&self) -> Option<&str> {
        self.handle.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub turn: AssistantTurn,
    pub finish: FinishReason,
    pub usage: Usage,
    pub continuation: Continuation,
    pub meta: ResponseMeta,
}

impl Response {
    pub fn calls(&self) -> impl Iterator<Item = &qbot_context::ToolCall> {
        self.turn.calls()
    }
}

/// Incremental output. Deltas are previews for live display; [`StreamEvent::Completed`] is the
/// authoritative result and is always last. A provider that cannot stream natively emits only
/// `Completed`.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    /// Visible reasoning summary text, where the provider exposes one.
    ReasoningDelta(String),
    CallStarted {
        id: CallId,
        name: String,
    },
    CallArguments {
        id: CallId,
        fragment: String,
    },
    Completed(Box<Response>),
}
