use qbot_core::{CallId, ItemSeq};
use serde::{Deserialize, Serialize};

use crate::chat::ChatBatch;

/// Provider-private reasoning, replayed only to the provider that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpaqueReasoning {
    pub provider: String,
    pub payload: serde_json::Value,
    /// Human-readable summary when the provider exposes one. Diagnostic only; never replayed.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: CallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// One element of a model response, in the order the model produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AssistantPart {
    Reasoning(OpaqueReasoning),
    Text(String),
    Call(ToolCall),
}

/// Provider-native output kept so the same provider can be replayed byte-for-byte. Adapters
/// use it only when `provider` matches their own id; otherwise they rebuild the turn from
/// [`AssistantTurn::parts`], which stay the normalized source of truth for everything above.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeReplay {
    pub provider: String,
    pub payload: serde_json::Value,
}

/// One complete model response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantTurn {
    parts: Vec<AssistantPart>,
    #[serde(default)]
    replay: Option<NativeReplay>,
}

impl AssistantTurn {
    pub fn new(parts: Vec<AssistantPart>) -> Self {
        Self {
            parts,
            replay: None,
        }
    }

    pub fn with_replay(mut self, replay: NativeReplay) -> Self {
        self.replay = Some(replay);
        self
    }

    pub fn parts(&self) -> &[AssistantPart] {
        &self.parts
    }

    pub fn replay(&self) -> Option<&NativeReplay> {
        self.replay.as_ref()
    }

    pub fn calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.parts.iter().filter_map(|part| match part {
            AssistantPart::Call(call) => Some(call),
            _ => None,
        })
    }

    /// Concatenated text parts.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Part {
    Text(String),
    Image { key: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidArguments,
    Execution,
    Unavailable,
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    LimitReached,
    NotAllowed,
    Conflict,
}

/// How a tool call ended. Never inferred from result text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Error(ErrorKind),
    Refused(RefusalReason),
    /// The run was interrupted before the call finished; its effect is unknown.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: CallId,
    pub outcome: Outcome,
    pub content: Vec<Part>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionRole {
    System,
    Developer,
    /// Lower-trust text describing why the run started (for example a task's stored intent).
    /// Delivered to the model as user-level content, never with developer authority.
    Trigger,
    /// Reference material derived from chat (learned group knowledge). The system wrote it, but
    /// from what members said, so it is delivered as user-level content like the chat itself.
    Reference,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    pub role: InstructionRole,
    pub text: String,
    /// Hash of the template that produced the text, so the prefix is attributable.
    pub template_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEnd {
    Completed,
    Delivered,
    StepLimit,
    Deadline,
    ModelError,
    Cancelled,
    /// The archive, context source or run log failed.
    Environment,
    /// The process died mid-run; recorded by startup recovery, never by a live run.
    Interrupted,
}

impl RunEnd {
    pub fn as_str(self) -> &'static str {
        match self {
            RunEnd::Completed => "completed",
            RunEnd::Delivered => "delivered",
            RunEnd::StepLimit => "step_limit",
            RunEnd::Deadline => "deadline",
            RunEnd::ModelError => "model_error",
            RunEnd::Cancelled => "cancelled",
            RunEnd::Environment => "environment",
            RunEnd::Interrupted => "interrupted",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "completed" => RunEnd::Completed,
            "delivered" => RunEnd::Delivered,
            "step_limit" => RunEnd::StepLimit,
            "deadline" => RunEnd::Deadline,
            "model_error" => RunEnd::ModelError,
            "cancelled" => RunEnd::Cancelled,
            "environment" => RunEnd::Environment,
            "interrupted" => RunEnd::Interrupted,
            _ => return None,
        })
    }
}

/// Internal bookkeeping; never sent to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Meta {
    /// Calls still unresolved when the run ended, closed as `Interrupted`.
    #[serde(alias = "resumed")]
    CallsInterrupted {
        interrupted_calls: u32,
    },
    RunEnded(RunEnd),
}

/// Replaces the items `from..to` in the model's view. The originals stay in the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub from: ItemSeq,
    pub to: ItemSeq,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Item {
    Instruction(Instruction),
    Chat(ChatBatch),
    Assistant(AssistantTurn),
    ToolResult(ToolResult),
    Meta(Meta),
    Summary(Summary),
}

impl Item {
    pub fn kind(&self) -> &'static str {
        match self {
            Item::Instruction(_) => "instruction",
            Item::Chat(_) => "chat",
            Item::Assistant(_) => "assistant",
            Item::ToolResult(_) => "tool_result",
            Item::Meta(_) => "meta",
            Item::Summary(_) => "summary",
        }
    }
}
