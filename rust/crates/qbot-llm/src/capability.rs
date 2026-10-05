use serde::{Deserialize, Serialize};

/// How a capability is delivered to the upper layers.
///
/// Both variants give identical semantics. `Emulated` means the adapter reconstructs the
/// behavior locally because the vendor does not provide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Realization {
    Native,
    Emulated,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How the model's reasoning shows up, when it has any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningInfo {
    /// The model produces reasoning content at all.
    pub produces: bool,
    /// Reasoning items must be replayed for a tool loop to stay coherent. Adapters do this
    /// themselves from the transcript; the upper layers only keep the parts.
    pub replay_required: bool,
    /// A human-readable summary is exposed (diagnostic only, never replayed).
    pub visible_summary: bool,
    /// [`ReasoningEffort`](crate::ReasoningEffort) changes behavior.
    pub effort_control: bool,
}

/// Required features are [`Realization`]s: a provider that cannot deliver one natively or by
/// emulation cannot be used. Optional features are `Option`/`bool` and are typed as absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub streaming: Realization,
    pub tool_calls: Realization,
    pub parallel_tool_calls: Realization,
    /// `Native`: the provider keeps conversation state and accepts a handle.
    /// `Emulated`: the adapter replays the full conversation every call.
    pub continuation: Realization,
    pub developer_role: Realization,
    pub reasoning: ReasoningInfo,
    /// Image content in messages and tool results; `None` means unavailable.
    pub image_input: Option<Realization>,
    /// Usage reports how many input tokens were served from the prompt cache.
    pub cache_metrics: bool,
    /// When `ToolChoice::Required` and `ToolChoice::Named` are honored. `Auto` and `None` always
    /// are (`None` is emulated by declaring no tools).
    pub forced_tool_choice: ForcedToolChoice,
    /// A sampling temperature may be set.
    pub temperature: bool,
}

/// Whether a provider can be made to call a tool. A property of each adapter and dialect,
/// measured against the real API, not assumed from another API family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForcedToolChoice {
    Always,
    /// Only with reasoning off (`ReasoningEffort::Off`), and once a conversation has a turn
    /// made without reasoning, later turns must stay without it. DeepSeek's Responses API
    /// (checked 2026-10-05, deepseek-flash and deepseek-v4-pro, every effort): a forced
    /// `tool_choice` with thinking on is refused ("Thinking mode does not support this
    /// tool_choice"), and a thinking turn after a non-thinking one is refused ("The
    /// `reasoning_text` in the thinking mode must be passed back to the API").
    WithoutReasoning,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: ProviderId,
    pub model: String,
    pub capabilities: Capabilities,
}
