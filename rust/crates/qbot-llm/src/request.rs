use async_trait::async_trait;

use crate::capability::ProviderInfo;
use crate::conversation::{Content, ConvItem, Conversation};
use crate::error::LlmError;
use crate::response::Continuation;

/// A tool the model may call. The schema is JSON Schema for the arguments object.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    Off,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    pub max_output_tokens: u32,
    pub reasoning: ReasoningEffort,
    pub temperature: Option<f32>,
}

/// Image bytes for [`Content::Image`] keys.
#[async_trait]
pub trait MediaStore: Send + Sync {
    async fn load(&self, key: &str) -> Result<LoadedMedia, LlmError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedMedia {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// One model call: the complete logical conversation plus an optional continuation.
///
/// The conversation is always complete. A provider that keeps state uses the continuation to
/// send only the new tail; one that does not simply ignores it.
pub struct Request<'a> {
    pub conversation: &'a Conversation,
    pub tools: &'a [ToolSpec],
    pub tool_choice: ToolChoice,
    pub parallel_tool_calls: bool,
    pub params: Params,
    pub continuation: Option<&'a Continuation>,
    pub media: Option<&'a dyn MediaStore>,
}

impl std::fmt::Debug for Request<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("items", &self.conversation.len())
            .field("tools", &self.tools.len())
            .field("tool_choice", &self.tool_choice)
            .finish_non_exhaustive()
    }
}

/// Checks every adapter runs before touching the network. It protects only what the provider
/// cannot enforce uniformly for us: the pairing invariant of our own transcript, and image
/// input on providers that cannot carry it. Everything the provider itself rejects (unknown
/// tool names, output limits, context size) is left to its typed error.
pub fn validate_request(info: &ProviderInfo, request: &Request<'_>) -> Result<(), LlmError> {
    request.conversation.validate()?;
    let has_images = request.conversation.items().iter().any(|item| match item {
        ConvItem::Message(message) => message.content.iter().any(is_image),
        ConvItem::ToolResult(output) => output.content.iter().any(is_image),
        ConvItem::Assistant(_) => false,
    });
    if has_images {
        if info.capabilities.image_input.is_none() {
            return Err(LlmError::Unsupported("image input"));
        }
        if request.media.is_none() {
            return Err(LlmError::InvalidRequest(
                "images require a media store".into(),
            ));
        }
    }
    Ok(())
}

fn is_image(content: &Content) -> bool {
    matches!(content, Content::Image { .. })
}
