//! The provider-facing conversation.
//!
//! A [`Conversation`] is lowered from a `qbot_context::View` by [`Conversation::lower`]. Chat
//! batches, summaries and outcome notes become text through a [`Renderer`], so adapters never
//! see group-chat structure and never invent model-facing wording.

use std::collections::HashSet;

use qbot_context::{
    AssistantTurn, ChatBatch, Instruction, InstructionRole, Outcome, Part, Speaker, ToolResult,
    View, ViewItem,
};
use qbot_core::CallId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Content {
    Text(String),
    /// An image referenced by an opaque key; bytes come from a [`MediaStore`](crate::MediaStore).
    Image {
        key: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

/// How a tool call ended, kept as data so adapters never infer it from text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Refused,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub call_id: CallId,
    pub status: ToolStatus,
    pub content: Vec<Content>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ConvItem {
    Message(Message),
    Assistant(AssistantTurn),
    ToolResult(ToolOutput),
}

/// Model-facing wording for everything the structured view cannot express as text. Supplied by
/// the prompt layer; [`PlainRenderer`] is a neutral English implementation for tests.
pub trait Renderer: Send + Sync {
    fn chat(&self, batch: &ChatBatch) -> String;
    fn summary(&self, text: &str) -> String;
    /// Text prepended to a tool result whose outcome is not a plain success.
    fn outcome_note(&self, outcome: &Outcome) -> Option<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlainRenderer;

impl Renderer for PlainRenderer {
    fn chat(&self, batch: &ChatBatch) -> String {
        batch
            .lines()
            .iter()
            .map(|line| match line.speaker {
                Speaker::Bot => format!("[msg:{}] bot: {}", line.message.get(), line.text),
                Speaker::Member { number, .. } => format!(
                    "[msg:{}] member:{}: {}",
                    line.message.get(),
                    number.get(),
                    line.text
                ),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn summary(&self, text: &str) -> String {
        format!("[summary of earlier conversation]\n{text}")
    }

    fn outcome_note(&self, outcome: &Outcome) -> Option<String> {
        match outcome {
            Outcome::Ok => None,
            Outcome::Error(kind) => Some(format!("[error: {kind:?}]")),
            Outcome::Refused(reason) => Some(format!("[refused: {reason:?}]")),
            Outcome::Interrupted => Some("[interrupted: effect unknown]".to_owned()),
        }
    }
}

/// Identity of a conversation prefix, used to decide whether a continuation still applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConvDigest(String);

impl ConvDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversationError {
    #[error("conversation is empty")]
    Empty,
    #[error("call {0:?} has no result directly after its turn, in call order")]
    UnpairedCall(CallId),
    #[error("result for call {0:?} does not follow its call")]
    OrphanResult(CallId),
    #[error("call id {0:?} is used more than once")]
    DuplicateCall(CallId),
    #[error("the conversation ends on an assistant turn awaiting results")]
    EndsOnPendingCalls,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Conversation {
    items: Vec<ConvItem>,
}

impl Conversation {
    pub fn new(items: Vec<ConvItem>) -> Self {
        Self { items }
    }

    pub fn items(&self) -> &[ConvItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn lower(view: &View, renderer: &dyn Renderer) -> Self {
        let items = view
            .items
            .iter()
            .map(|item| match item {
                ViewItem::Instruction(instruction) => {
                    ConvItem::Message(instruction_message(instruction))
                }
                ViewItem::Chat(batch) => ConvItem::Message(Message {
                    role: Role::User,
                    content: vec![Content::Text(renderer.chat(batch))],
                }),
                ViewItem::Summary(text) => ConvItem::Message(Message {
                    role: Role::User,
                    content: vec![Content::Text(renderer.summary(text))],
                }),
                ViewItem::Assistant(turn) => ConvItem::Assistant(turn.clone()),
                ViewItem::ToolResult(result) => {
                    ConvItem::ToolResult(lower_result(result, renderer))
                }
            })
            .collect();
        Self { items }
    }

    /// Digest of the first `n` items.
    pub fn digest_prefix(&self, n: usize) -> ConvDigest {
        digest_items(&self.items[..n.min(self.items.len())])
    }

    /// Checks the pairing rules every provider relies on: each assistant turn's calls are
    /// answered immediately, in call order, before anything else.
    pub fn validate(&self) -> Result<(), ConversationError> {
        if self.items.is_empty() {
            return Err(ConversationError::Empty);
        }
        let mut seen: HashSet<&CallId> = HashSet::new();
        let mut expected: Vec<&CallId> = Vec::new();
        for item in &self.items {
            match item {
                ConvItem::ToolResult(output) => {
                    if expected.first() != Some(&&output.call_id) {
                        return Err(ConversationError::OrphanResult(output.call_id.clone()));
                    }
                    expected.remove(0);
                }
                other => {
                    if let Some(first) = expected.first() {
                        return Err(ConversationError::UnpairedCall((*first).clone()));
                    }
                    if let ConvItem::Assistant(turn) = other {
                        for call in turn.calls() {
                            if !seen.insert(&call.id) {
                                return Err(ConversationError::DuplicateCall(call.id.clone()));
                            }
                            expected.push(&call.id);
                        }
                    }
                }
            }
        }
        if expected.is_empty() {
            Ok(())
        } else {
            Err(ConversationError::EndsOnPendingCalls)
        }
    }
}

pub(crate) fn digest_items(items: &[ConvItem]) -> ConvDigest {
    let bytes = serde_json::to_vec(items).unwrap_or_default();
    let hash = Sha256::digest(&bytes);
    ConvDigest(hash.iter().map(|b| format!("{b:02x}")).collect())
}

fn instruction_message(instruction: &Instruction) -> Message {
    Message {
        role: match instruction.role {
            InstructionRole::System => Role::System,
            InstructionRole::Developer => Role::Developer,
            InstructionRole::Trigger | InstructionRole::Reference => Role::User,
        },
        content: vec![Content::Text(instruction.text.clone())],
    }
}

fn lower_result(result: &ToolResult, renderer: &dyn Renderer) -> ToolOutput {
    let status = match result.outcome {
        Outcome::Ok => ToolStatus::Ok,
        Outcome::Error(_) => ToolStatus::Error,
        Outcome::Refused(_) => ToolStatus::Refused,
        Outcome::Interrupted => ToolStatus::Interrupted,
    };
    let mut content = Vec::new();
    if let Some(note) = renderer.outcome_note(&result.outcome) {
        content.push(Content::Text(note));
    }
    content.extend(result.content.iter().map(|part| match part {
        Part::Text(text) => Content::Text(text.clone()),
        Part::Image { key } => Content::Image { key: key.clone() },
    }));
    ToolOutput {
        call_id: result.call_id.clone(),
        status,
        content,
    }
}
