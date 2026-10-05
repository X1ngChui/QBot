//! The canonical conversation model for one agent run.
//!
//! A [`Transcript`] is an append-only log of typed [`Item`]s. Everything the model sees is
//! derived from it by the pure function [`project`]; provider wire formats are produced from the
//! resulting [`View`] by adapters elsewhere. This crate performs no IO.

mod chat;
mod error;
mod item;
mod project;
mod transcript;

pub use chat::{ChatBatch, ChatLine, MemberStanding, Speaker};
pub use error::{AppendError, LoadError, ProjectError};
pub use item::{
    AssistantPart, AssistantTurn, ErrorKind, Instruction, InstructionRole, Item, Meta,
    NativeReplay, OpaqueReasoning, Outcome, Part, RefusalReason, RunEnd, Summary, ToolCall,
    ToolResult,
};
pub use project::{View, ViewDigest, ViewItem, project};
pub use transcript::Transcript;
