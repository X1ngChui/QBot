//! The tool contract. A tool declares typed arguments; the JSON schema, the parser and the
//! validation come from that one type, so they cannot drift apart.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use qbot_context::{ChatLine, MemberStanding, Part, RefusalReason, Speaker};
use qbot_core::{AccountId, Clock, GroupId, MemberNo, MessageId, RunId};
use qbot_llm::ToolSpec;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::env::Trigger;

/// How a tool interacts with the world; decides scheduling inside one model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// No side effects; runs concurrently with other reads.
    Read,
    /// Mutates internal state; runs sequentially in call order, after reads.
    Write,
    /// Produces user-visible output; runs sequentially in call order, after reads.
    Send,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("refused: {message}")]
    Refused {
        reason: RefusalReason,
        message: String,
    },
    #[error("failed: {0}")]
    Failed(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
}

impl ToolError {
    pub fn refused(reason: RefusalReason, message: impl Into<String>) -> Self {
        ToolError::Refused {
            reason,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolOutput {
    pub content: Vec<Part>,
    /// The run ends after this turn without another model call.
    pub ends_run: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Part::Text(text.into())],
            ends_run: false,
        }
    }

    pub fn ends_run(mut self, ends: bool) -> Self {
        self.ends_run = ends;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Participant {
    pub account: AccountId,
    pub standing: MemberStanding,
}

/// What a run has seen of the chat: who the member numbers are and which messages exist.
/// Tools validate model-supplied references against it.
#[derive(Debug, Clone, Default)]
pub struct ChatView {
    roster: BTreeMap<MemberNo, Participant>,
    messages: HashMap<MessageId, Speaker>,
}

impl ChatView {
    pub fn absorb(&mut self, lines: &[ChatLine]) {
        for line in lines {
            self.messages.insert(line.message, line.speaker);
            if let Speaker::Member {
                account,
                number,
                standing,
            } = line.speaker
            {
                self.roster
                    .insert(number, Participant { account, standing });
            }
        }
    }

    pub fn participant(&self, number: MemberNo) -> Option<Participant> {
        self.roster.get(&number).copied()
    }

    pub fn has_message(&self, id: MessageId) -> bool {
        self.messages.contains_key(&id)
    }
}

/// Counters and observations shared by the tools of one run.
#[derive(Debug, Default)]
pub struct RunState {
    sends: AtomicU32,
    observed: Mutex<HashSet<MessageId>>,
}

impl RunState {
    /// Claim one send slot; false once `max` sends have been claimed.
    pub fn try_claim_send(&self, max: u32) -> bool {
        self.sends
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < max).then_some(n + 1)
            })
            .is_ok()
    }

    pub fn sends(&self) -> u32 {
        self.sends.load(Ordering::SeqCst)
    }

    /// Record a message the model has already been told about through a tool result, so the
    /// run does not show it again as a new arrival.
    pub fn note_observed(&self, id: MessageId) {
        self.observed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id);
    }

    pub fn is_observed(&self, id: MessageId) -> bool {
        self.observed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&id)
    }
}

impl std::fmt::Debug for ToolCx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCx")
            .field("group", &self.group)
            .field("run", &self.run)
            .finish_non_exhaustive()
    }
}

pub struct ToolCx<'a> {
    pub group: GroupId,
    pub run: RunId,
    pub trigger: &'a Trigger,
    pub view: &'a ChatView,
    pub state: &'a RunState,
    pub clock: &'a dyn Clock,
}

#[async_trait]
pub trait Tool: Send + Sync + 'static {
    type Args: DeserializeOwned + JsonSchema + Send;
    const NAME: &'static str;

    fn description(&self) -> String;
    /// Descriptions of the parameters, by name; a nested field is `outer.inner` (through arrays
    /// too). Kept out of the argument types so model-facing wording lives in one place.
    fn parameters(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
    fn effect(&self) -> Effect;
    async fn call(&self, cx: &ToolCx<'_>, args: Self::Args) -> Result<ToolOutput, ToolError>;
}

/// Object-safe form of [`Tool`], so a [`ToolSet`] can hold different argument types.
#[async_trait]
pub trait ErasedTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn spec(&self) -> ToolSpec;
    fn effect(&self) -> Effect;
    async fn call(&self, cx: &ToolCx<'_>, arguments: Value) -> Result<ToolOutput, ToolError>;
}

struct Typed<T>(T);

#[async_trait]
impl<T: Tool> ErasedTool for Typed<T> {
    fn name(&self) -> &'static str {
        T::NAME
    }

    fn spec(&self) -> ToolSpec {
        let schema = qbot_llm::schema::tool_schema::<T::Args>(self.0.parameters());
        ToolSpec {
            name: T::NAME.to_owned(),
            description: self.0.description(),
            schema,
        }
    }

    fn effect(&self) -> Effect {
        self.0.effect()
    }

    async fn call(&self, cx: &ToolCx<'_>, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args: T::Args = serde_json::from_value(arguments)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        self.0.call(cx, args).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("tool {0:?} is registered twice")]
pub struct DuplicateTool(pub &'static str);

/// The fixed tool set of a run. Its specs are identical for the whole run.
#[derive(Clone, Default)]
pub struct ToolSet {
    tools: Vec<Arc<dyn ErasedTool>>,
}

impl std::fmt::Debug for ToolSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.tools.iter().map(|t| t.name()))
            .finish()
    }
}

impl ToolSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with<T: Tool>(mut self, tool: T) -> Result<Self, DuplicateTool> {
        if self.get(T::NAME).is_some() {
            return Err(DuplicateTool(T::NAME));
        }
        self.tools.push(Arc::new(Typed(tool)));
        Ok(self)
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn ErasedTool>> {
        self.tools.iter().find(|t| t.name() == name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
