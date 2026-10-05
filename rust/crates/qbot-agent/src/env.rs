//! The ports through which a run touches the world. Production wires these to the gateway and
//! the database; tests use the simulated implementations in [`crate::sim`].

use std::time::Duration;

use async_trait::async_trait;
use qbot_context::{ChatLine, Instruction, Item, Outcome, RunEnd};
use qbot_core::{AccountId, ChainId, GroupId, ItemSeq, MessageId, RunId, TimerId};
use qbot_llm::Usage;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("environment failure: {0}")]
pub struct EnvError(pub String);

/// Position in a group's archive. Strictly increasing per group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ArchiveCursor(pub u64);

#[derive(Debug, Clone, PartialEq)]
pub struct ArchivedLine {
    pub seq: ArchiveCursor,
    pub line: ChatLine,
}

#[async_trait]
pub trait Archive: Send + Sync {
    /// Lines archived after `cursor`, oldest first.
    async fn since(
        &self,
        group: GroupId,
        cursor: ArchiveCursor,
    ) -> Result<Vec<ArchivedLine>, EnvError>;
    /// Lines matching `query`, newest first.
    async fn search(
        &self,
        group: GroupId,
        query: &HistoryQuery,
        limit: usize,
    ) -> Result<Vec<ChatLine>, EnvError>;
}

/// A condition on a line's text. Terms match as case-insensitive substrings, so text in
/// languages written without spaces (Chinese) is found wherever it occurs; no word splitting is
/// involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextQuery {
    Contains(String),
    Not(Box<TextQuery>),
    /// Every condition holds (an empty list holds always).
    All(Vec<TextQuery>),
    /// At least one condition holds (an empty list never holds).
    Any(Vec<TextQuery>),
}

impl TextQuery {
    pub fn matches(&self, text: &str) -> bool {
        match self {
            TextQuery::Contains(term) => text.to_lowercase().contains(&term.to_lowercase()),
            TextQuery::Not(inner) => !inner.matches(text),
            TextQuery::All(all) => all.iter().all(|q| q.matches(text)),
            TextQuery::Any(any) => any.iter().any(|q| q.matches(text)),
        }
    }

    /// Whether every way of matching requires some term to be present. A query that is only
    /// exclusions (`-spam`) would match nearly every line, so it is not a search.
    pub fn requires_a_term(&self) -> bool {
        match self {
            TextQuery::Contains(_) => true,
            TextQuery::Not(_) => false,
            TextQuery::All(all) => all.iter().any(TextQuery::requires_a_term),
            TextQuery::Any(any) => !any.is_empty() && any.iter().all(TextQuery::requires_a_term),
        }
    }
}

/// A search of a group's chat history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    pub text: TextQuery,
    /// Only lines of the member with this number.
    pub speaker: Option<qbot_core::MemberNo>,
}

/// Members' names as the platform shows them in a group right now.
///
/// This is the authoritative name of a member in a group, their group display name: the group
/// nickname they set for this group (OneBot `card`), or their account nickname (`nickname`) where
/// they set none, as QQ shows it. It is read live, never stored,
/// so it is never stale; stored names (`qbot_memory` aliases) are what people call someone
/// besides it.
#[async_trait]
pub trait Directory: Send + Sync {
    /// `None` when the platform cannot say (no connection, not a member): a name is a courtesy,
    /// never a reason to fail.
    async fn display_name(&self, group: GroupId, account: AccountId) -> Option<String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chain {
    pub id: ChainId,
    pub depth: u32,
}

/// Why a run exists.
#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    Addressed {
        message: MessageId,
        sender: AccountId,
    },
    Wake {
        timer: TimerId,
        intent: String,
        chain: Chain,
    },
}

/// Everything a run starts from, produced by the prompt layer.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenedContext {
    /// Stable instructions: identical across runs of a group so the provider cache can hit.
    pub instructions: Vec<Instruction>,
    /// The chat window at trigger time.
    pub window: Vec<ChatLine>,
    /// Lower-trust note about the trigger (for example a task intent), appended after the window.
    pub trigger_note: Option<Instruction>,
    pub cursor: ArchiveCursor,
    /// Episode summaries standing in for contiguous ranges of window lines. Ranges are disjoint,
    /// ascending and never cover the trigger message.
    pub recaps: Vec<Recap>,
    /// Told to the model when it ends a turn with text but no tool call: the text was meant for
    /// the group and never reached it. Followed by one turn that must call a tool. `None` ends
    /// such a turn silently.
    pub undelivered_note: Option<Instruction>,
}

/// An episode's summary that replaces window lines `lines` in the model's view. The lines stay
/// in the transcript; only the view changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recap {
    pub lines: std::ops::Range<usize>,
    pub text: String,
    pub when: RecapWhen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecapWhen {
    /// From the start of the run: the history policy shows these lines only as a summary.
    Open,
    /// Only if the provider finds the context too long despite the policy: the fallback.
    Overflow,
}

#[async_trait]
pub trait ContextSource: Send + Sync {
    async fn open(&self, group: GroupId, trigger: &Trigger) -> Result<OpenedContext, EnvError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutSegment {
    Text(String),
    At(AccountId),
    Reply(MessageId),
    Face(u32),
    /// A platform dice roll; the result only exists in the echo.
    Dice,
    /// A platform rock-paper-scissors; the result only exists in the echo.
    Rps,
    /// A member's contact card. Only members of the group the run belongs to.
    Contact(AccountId),
}

/// A sent message as the platform echoed it back, which is the only way to observe
/// nondeterministic results such as dice.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivered {
    pub echo: ChatLine,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    #[error("the platform rejected the message: {0}")]
    Rejected(String),
    #[error("the message was sent but its echo was not observed")]
    EchoMissing,
    #[error("delivery is unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait Delivery: Send + Sync {
    /// Send one message and wait for its echo.
    async fn send(
        &self,
        group: GroupId,
        segments: Vec<OutSegment>,
    ) -> Result<Delivered, DeliveryError>;
}

#[async_trait]
pub trait GroupPolicy: Send + Sync {
    async fn is_muted(&self, group: GroupId) -> Result<bool, EnvError>;
    async fn is_blocked(&self, group: GroupId, account: AccountId) -> Result<bool, EnvError>;
}

/// How a run ended, for the durable run record.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub end: RunEnd,
    /// Class label of the provider error behind `RunEnd::ModelError`.
    pub error: Option<&'static str>,
    pub usage: Usage,
    pub turns: u32,
    pub tool_calls: u32,
    pub sends: u32,
}

/// Durable, append-only record of every run: its start, each item, and its end.
#[async_trait]
pub trait RunLog: Send + Sync {
    /// Record that a run exists and allocate its id. Ids are unique across restarts.
    async fn begin(&self, group: GroupId, trigger: &Trigger) -> Result<RunId, EnvError>;
    async fn append(
        &self,
        group: GroupId,
        run: RunId,
        seq: ItemSeq,
        item: &Item,
    ) -> Result<(), EnvError>;
    async fn finish(
        &self,
        group: GroupId,
        run: RunId,
        summary: &RunSummary,
    ) -> Result<(), EnvError>;
}

/// Informational usage and latency records. Nothing reads these to refuse work.
pub trait UsageSink: Send + Sync {
    fn record(&self, event: UsageEvent);
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageEvent {
    pub group: GroupId,
    pub run: RunId,
    pub turn: u32,
    pub latency: Duration,
    pub detail: UsageDetail,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UsageDetail {
    Model {
        provider: String,
        model: String,
        usage: Option<Usage>,
        attempts: u32,
        error: Option<&'static str>,
    },
    Tool {
        name: String,
        outcome: Outcome,
    },
    RunEnded {
        end: RunEnd,
        usage: Usage,
        turns: u32,
        tool_calls: u32,
    },
}
