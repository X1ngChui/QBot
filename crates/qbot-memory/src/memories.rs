//! What the bot knows about the people of a group and the group itself: one store of memories.
//!
//! A memory is a name a member is called by or a statement about a member (possibly involving a
//! second member) or the group, each with the lines that support it and its history: a later
//! statement can replace it or end it, and the old one is kept as no longer holding. A memory
//! comes from chat (the nightly batch read it), from a ruling over a name's uses, or from a
//! person who entered it by hand; what a person entered or removed by hand outranks everything
//! read from chat.
//!
//! Evidence is kept as [`Observation`]s: one line telling one thing about one subject. The batch
//! stores every observation it keeps; a memory's support is the observations attached to it. An
//! observation not attached yet is pending: a statement about a person known only by a name
//! nobody has placed, a use of such a name, or one a command overtook while the batch was
//! reading.
//!
//! Nothing here estimates truth: a reader is shown provenance (who said it, how often, when),
//! and [`crate::worth`] ranks statements where room is limited.

use std::collections::BTreeSet;

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::episode::{EpisodeId, NewEpisode};
use crate::names::{Grounds, NameBasis, name_key};
use crate::store::MemoryError;

/// The longest statement kept. A memory is one short sentence: it is shown in the prompts of
/// runs about its subject, and longer text is a summary, which episodes are for.
pub const MAX_MEMORY_CHARS: usize = 200;
/// The longest group term kept: a word or phrase.
pub const MAX_TERM_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryId(i64);

impl MemoryId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> i64 {
        self.0
    }

    /// How models and members read and write the id: `m12`.
    pub fn label(self) -> String {
        format!("m{}", self.0)
    }

    /// `m12` or `12`.
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.trim();
        let digits = digits
            .strip_prefix('m')
            .or_else(|| digits.strip_prefix('M'))
            .unwrap_or(digits);
        digits.parse().ok().filter(|n| *n > 0).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObservationId(i64);

impl ObservationId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

/// How long a statement of its kind typically stays relevant unsaid: it sets how fast the
/// statement loses rank in its subject's conversation, never whether it is true.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Where someone lives, works or studies, family, pets, a lasting trait or boundary.
    Lasting,
    /// Tastes, hobbies, long-running interests and habits.
    Ongoing,
    /// What someone is playing, reading, working towards these weeks.
    Current,
}

impl Durability {
    pub fn as_str(self) -> &'static str {
        match self {
            Durability::Lasting => "lasting",
            Durability::Ongoing => "ongoing",
            Durability::Current => "current",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "lasting" => Durability::Lasting,
            "ongoing" => Durability::Ongoing,
            "current" => Durability::Current,
            _ => return None,
        })
    }
}

/// What a memory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A name the member is called by.
    Name,
    /// Something about the member (or the group).
    Statement,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Name => "name",
            Kind::Statement => "statement",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "name" => Kind::Name,
            "statement" => Kind::Statement,
            _ => return None,
        })
    }
}

/// Where a memory came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    /// The batch read it from chat.
    Chat,
    /// A ruling over a name's uses across conversations.
    Ruling,
    /// A person entered it by hand.
    Hand { by: AccountId },
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Chat => "chat",
            Origin::Ruling => "ruling",
            Origin::Hand { .. } => "hand",
        }
    }

    pub fn is_hand(self) -> bool {
        matches!(self, Origin::Hand { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryState {
    Current,
    /// A later statement (or a correction by hand) replaced it; `by` is gone when that memory was
    /// deleted.
    Replaced {
        by: Option<MemoryId>,
        at: UnixMillis,
    },
    /// Someone said, or a person marked, that it no longer holds.
    Ended {
        at: UnixMillis,
    },
}

/// Whom an observation is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum About {
    Member(AccountId),
    Group,
    /// A person the lines name but nobody has placed yet: the name as written.
    Name(String),
}

impl About {
    /// The subject a memory about it would have; `None` for the group, and for a name nobody
    /// has placed (it has no memories).
    pub fn subject(&self) -> Option<AccountId> {
        match self {
            About::Member(account) => Some(*account),
            About::Group | About::Name(_) => None,
        }
    }
}

/// What a line tells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Said {
    /// It uses a name for the person.
    Name {
        /// The name as written.
        text: String,
        /// How the line uses it.
        basis: NameBasis,
        grounds: Grounds,
        /// The chat's structure ties the name to the member: the line is said to them (it
        /// replies to them or mentions them), or they reply to it.
        anchored: bool,
        /// The lines that showed who was meant.
        shown_by: Vec<MessageId>,
    },
    /// It says something about them.
    Statement {
        text: String,
        /// For a group term: the term.
        term: Option<String>,
        durability: Durability,
        /// A second member the statement involves.
        with: Option<AccountId>,
    },
}

/// One line telling one thing about one subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub id: ObservationId,
    pub episode: EpisodeId,
    pub message: MessageId,
    /// The line's place in the group's chat.
    pub ordinal: u64,
    /// Who wrote the line.
    pub author: AccountId,
    pub quote: String,
    pub at: UnixMillis,
    pub about: About,
    pub said: Said,
}

impl Observation {
    /// The key it is compared by: a name's [`name_key`], a statement's [`memory_key`].
    pub fn key(&self) -> String {
        match &self.said {
            Said::Name { text, .. } => name_key(text).unwrap_or_default(),
            Said::Statement { text, term, .. } => memory_key(term.as_deref(), text),
        }
    }

    pub fn is_name(&self) -> bool {
        matches!(self.said, Said::Name { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: MemoryId,
    pub group: GroupId,
    /// `None`: the group.
    pub subject: Option<AccountId>,
    pub kind: Kind,
    /// A second member a statement involves.
    pub with: Option<AccountId>,
    /// For a group term: the term.
    pub term: Option<String>,
    /// The statement, or the name as most recently written.
    pub text: String,
    /// Statements only; names are kept as lasting.
    pub durability: Durability,
    pub origin: Origin,
    pub state: MemoryState,
    pub first: UnixMillis,
    /// The latest support, or when a person entered it.
    pub last: UnixMillis,
    /// What supports it, oldest first.
    pub support: Vec<Observation>,
}

impl Memory {
    pub fn is_current(&self) -> bool {
        self.state == MemoryState::Current
    }

    /// Distinct episodes that supported it.
    pub fn episodes(&self) -> u32 {
        let distinct: BTreeSet<EpisodeId> = self.support.iter().map(|o| o.episode).collect();
        u32::try_from(distinct.len()).unwrap_or(u32::MAX)
    }

    /// Whether `author` speaks for this memory: its subject, or the second member it involves.
    pub fn is_party(&self, author: AccountId) -> bool {
        self.subject == Some(author) || self.with == Some(author)
    }

    /// Of the supporting episodes, those in which a party to it said it themselves.
    pub fn own_word(&self) -> u32 {
        let own: BTreeSet<EpisodeId> = self
            .support
            .iter()
            .filter(|o| self.subject.is_some() && self.is_party(o.author))
            .map(|o| o.episode)
            .collect();
        u32::try_from(own.len()).unwrap_or(u32::MAX)
    }

    /// The term and text as one line: `term: text` for a term.
    pub fn display(&self) -> String {
        match &self.term {
            Some(term) => format!("{term}: {}", self.text),
            None => self.text.clone(),
        }
    }

    /// The key two memories of one subject are the same by.
    pub fn key(&self) -> String {
        match self.kind {
            Kind::Name => name_key(&self.text).unwrap_or_default(),
            Kind::Statement => memory_key(self.term.as_deref(), &self.text),
        }
    }
}

/// What "the same statement" means: the term and text compatibility-normalized, whitespace
/// collapsed and lower-cased.
pub fn memory_key(term: Option<&str>, text: &str) -> String {
    let whole = match term {
        Some(term) => format!("{term}: {text}"),
        None => text.to_owned(),
    };
    let folded: String = whole.nfkc().collect::<String>().to_lowercase();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A memory a person asked to be forgotten: it is never learned again. Its text is kept so that
/// the batch can recognize a restatement in other words; nothing else ever shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suppression {
    pub subject: Option<AccountId>,
    pub kind: Kind,
    pub text: String,
    pub key: String,
    /// The lines the forgotten memory rested on: nothing is read from them again for its
    /// subject.
    pub messages: Vec<MessageId>,
    pub by: AccountId,
    pub at: UnixMillis,
}

/// A memory a person enters by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandMemory {
    pub subject: Option<AccountId>,
    pub kind: Kind,
    pub term: Option<String>,
    pub text: String,
    pub by: AccountId,
    pub at: UnixMillis,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandError {
    /// The subject already has this as a current memory.
    #[error("already known as {}", .0.label())]
    Known(MemoryId),
    /// No current memory with that id about that subject.
    #[error("no such memory")]
    NotFound,
    /// Entered by hand, so only a person can change it, and this one may not.
    #[error("not allowed")]
    Refused,
    #[error("storage failure: {0}")]
    Backend(String),
}

impl From<MemoryError> for HandError {
    fn from(error: MemoryError) -> Self {
        HandError::Backend(error.to_string())
    }
}

// ---- the batch's commit ---------------------------------------------------------------------

/// An observation a commit stores: which of the commit's episodes it cites, and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewObservation {
    /// Index into [`Commit::episodes`].
    pub episode: usize,
    pub message: MessageId,
    pub ordinal: u64,
    pub author: AccountId,
    pub quote: String,
    pub at: UnixMillis,
    pub about: About,
    pub said: Said,
}

/// An observation a change refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Obs {
    /// Index into [`Commit::observations`].
    New(usize),
    Stored(ObservationId),
}

/// A memory a change refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Existing(MemoryId),
    /// The `n`th [`Op::Create`] among the same subject's changes (from 0).
    Created(usize),
}

/// A memory a commit creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMemory {
    pub subject: Option<AccountId>,
    pub kind: Kind,
    pub with: Option<AccountId>,
    pub term: Option<String>,
    pub text: String,
    pub durability: Durability,
    pub origin: Origin,
    /// When what it says was said.
    pub at: UnixMillis,
}

/// One change to one subject's memories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// A new memory. A name the subject already has as a current memory is that one instead.
    Create(NewMemory),
    /// The observation supports the memory.
    Support { obs: Obs, memory: Target },
    /// The memory holds no longer: replaced by `by`, or ended.
    Close {
        memory: MemoryId,
        by: Option<Target>,
        at: UnixMillis,
    },
    /// Two memories said the same thing: `from`'s support moves to `into`, and `from` is gone.
    Merge { from: MemoryId, into: Target },
    /// A pending observation the batch rejected.
    Drop(ObservationId),
    /// An observation about a name now placed with a member is about that member.
    Refile { obs: Obs, account: AccountId },
    /// Room: the memory gives way.
    Evict(MemoryId),
}

/// One subject's changes: applied together or, when a command overtook the batch for this
/// subject, not at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    /// `None`: the group.
    pub subject: Option<AccountId>,
    pub ops: Vec<Op>,
}

/// Everything one batch of one group writes, applied all at once or not at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub group: GroupId,
    /// When the batch read what it decided on: a command since then wins over its changes.
    pub read: UnixMillis,
    /// When the batch ran: replies are shown who is who as of then.
    pub at: UnixMillis,
    /// The new episodes, each with its embedding and the embedding model.
    pub episodes: Vec<(NewEpisode, Vec<f32>, String)>,
    /// Lines the batch gave up on for good: they count as covered without an episode.
    pub skipped: Option<(u64, u64, String)>,
    /// Every new observation kept; one no change attaches stays pending.
    pub observations: Vec<NewObservation>,
    pub changes: Vec<Changes>,
}

/// What a commit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Committed {
    pub episodes: usize,
    /// Subjects whose changes a command overtook: their observations stay pending.
    pub deferred: usize,
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Every memory about `subject` (a member's account, or `None` for the group), in every
    /// state, by id.
    async fn about(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
    ) -> Result<Vec<Memory>, MemoryError>;

    /// The current memories of `kind` about any of `subjects`, by id.
    async fn current(
        &self,
        group: GroupId,
        subjects: &[Option<AccountId>],
        kind: Kind,
    ) -> Result<Vec<Memory>, MemoryError>;

    /// Every current name of every member of the group, by id.
    async fn names(&self, group: GroupId) -> Result<Vec<Memory>, MemoryError>;

    async fn memory(&self, group: GroupId, id: MemoryId) -> Result<Option<Memory>, MemoryError>;

    /// The group's pending observations, in chat order.
    async fn pending(&self, group: GroupId) -> Result<Vec<Observation>, MemoryError>;

    /// What was forgotten in the group, oldest first.
    async fn suppressions(&self, group: GroupId) -> Result<Vec<Suppression>, MemoryError>;

    /// Apply a batch: its episodes (each range of a group taken once), its observations and the
    /// changes, and record the batch's time. All or nothing. A subject whose changes refer to a
    /// memory that is gone or no longer current, or that something was forgotten about since
    /// [`Commit::read`], is left as it is: its new observations stay pending, except those citing
    /// lines of what was forgotten since, which are dropped.
    async fn commit(&self, commit: &Commit) -> Result<Committed, MemoryError>;

    /// Enter a memory by hand. A name or statement the subject has as a current memory is refused
    /// as known; a name a person set for another member is refused as taken. Lifts what forgot it.
    async fn add(&self, group: GroupId, memory: &HandMemory) -> Result<MemoryId, HandError>;

    /// Correct a current memory by hand: a new one replaces it, entered by `by`.
    async fn fix(
        &self,
        group: GroupId,
        id: MemoryId,
        text: &str,
        by: AccountId,
        at: UnixMillis,
    ) -> Result<MemoryId, HandError>;

    /// Mark a current memory as no longer holding.
    async fn end(&self, group: GroupId, id: MemoryId, at: UnixMillis) -> Result<(), HandError>;

    /// Delete a memory and what supports it, and remember that it was forgotten so it is never
    /// learned again. Returns what was deleted.
    async fn forget(
        &self,
        group: GroupId,
        id: MemoryId,
        by: AccountId,
        at: UnixMillis,
    ) -> Result<Option<Memory>, MemoryError>;
}

/// Delete `memories` one by one through [`MemoryStore::forget`]: what `/memory forget all` does.
pub async fn forget_all(
    store: &dyn MemoryStore,
    group: GroupId,
    memories: &[MemoryId],
    by: AccountId,
    at: UnixMillis,
) -> Result<u64, MemoryError> {
    let mut gone = 0;
    for id in memories {
        if store.forget(group, *id, by, at).await?.is_some() {
            gone += 1;
        }
    }
    Ok(gone)
}

/// The suppression forgetting `memory` records.
pub fn suppression_of(memory: &Memory, by: AccountId, at: UnixMillis) -> Suppression {
    let mut messages: Vec<MessageId> = memory.support.iter().map(|o| o.message).collect();
    messages.sort();
    messages.dedup();
    Suppression {
        subject: memory.subject,
        kind: memory.kind,
        text: memory.display(),
        key: memory.key(),
        messages,
        by,
        at,
    }
}
