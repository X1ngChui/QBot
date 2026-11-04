//! What the bot remembers about members and the group: short statements with their evidence.
//!
//! A memory is one statement about one member (or the group: a term it uses, something true of
//! it) in the bot's words, and every line that supported it: who wrote it and the quote. A later
//! statement can support it (the same), replace it (a new memory; the old one is kept as
//! replaced, with when) or end it. Nothing here estimates truth: what a reader is shown is the
//! provenance (who said it, how often, when), and [`crate::worth`] ranks memories where room is
//! limited.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::episode::EpisodeId;
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

/// How long a memory of its kind typically stays relevant unsaid: it sets how fast the memory
/// loses rank in its subject's conversation, never whether it is true.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryState {
    Current,
    /// A later statement replaced it; `by` is gone when that memory was deleted.
    Replaced {
        by: Option<MemoryId>,
        at: UnixMillis,
    },
    /// Someone said it no longer holds.
    Ended {
        at: UnixMillis,
    },
}

/// One line that supported a memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Support {
    pub episode: EpisodeId,
    pub message: MessageId,
    /// Who wrote the line; `None` when that is no longer known.
    pub author: Option<AccountId>,
    pub quote: String,
    pub at: UnixMillis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: MemoryId,
    pub group: GroupId,
    /// `None`: the group.
    pub subject: Option<AccountId>,
    /// For a group term: the term.
    pub term: Option<String>,
    pub text: String,
    pub durability: Durability,
    pub state: MemoryState,
    pub first: UnixMillis,
    /// The latest support.
    pub last: UnixMillis,
    /// One per episode, oldest first.
    pub support: Vec<Support>,
}

impl Memory {
    pub fn is_current(&self) -> bool {
        self.state == MemoryState::Current
    }

    /// Distinct episodes that supported it.
    pub fn episodes(&self) -> u32 {
        u32::try_from(self.support.len()).unwrap_or(u32::MAX)
    }

    /// Of those, the episodes in which its subject said it themselves.
    pub fn own_word(&self) -> u32 {
        let own = self
            .support
            .iter()
            .filter(|s| self.subject.is_some() && s.author == self.subject)
            .count();
        u32::try_from(own).unwrap_or(u32::MAX)
    }

    /// The term and text as one line: `term: text` for a term.
    pub fn display(&self) -> String {
        match &self.term {
            Some(term) => format!("{term}: {}", self.text),
            None => self.text.clone(),
        }
    }
}

/// What "the same memory" means: the term and text compatibility-normalized, whitespace collapsed
/// and lower-cased. Two current memories of one subject never share it.
pub fn memory_key(term: Option<&str>, text: &str) -> String {
    let whole = match term {
        Some(term) => format!("{term}: {text}"),
        None => text.to_owned(),
    };
    let folded: String = whole.nfkc().collect::<String>().to_lowercase();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A statement to remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub subject: Option<AccountId>,
    pub term: Option<String>,
    pub text: String,
    pub durability: Durability,
    pub support: Support,
}

/// One change an episode makes. Changes naming a memory apply only to a current memory of that
/// subject in that group; one that names anything else is ignored, except that a replacement is
/// then remembered as new.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Remembered; the same as a current memory of the subject (by [`memory_key`]), it supports
    /// that one.
    New(Statement),
    /// Said again.
    Same {
        id: MemoryId,
        subject: Option<AccountId>,
        support: Support,
    },
    /// Holds no longer: `with` replaces it.
    Replace { id: MemoryId, with: Statement },
    /// Holds no longer, and nothing replaces it.
    End {
        id: MemoryId,
        subject: Option<AccountId>,
        at: UnixMillis,
    },
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Apply one episode's changes in order, atomically. Applying the same changes again changes
    /// nothing: each episode supports a memory at most once, a new statement equal to a current
    /// memory supports it, and a memory already replaced or ended is not current. Returns the
    /// memories the changes supported or created.
    async fn apply(&self, group: GroupId, changes: &[Change])
    -> Result<Vec<MemoryId>, MemoryError>;

    /// Every memory about `subject` in the group, in every state, by id.
    async fn about(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
    ) -> Result<Vec<Memory>, MemoryError>;

    /// The current memories about any of `subjects` in the group, by id.
    async fn current(
        &self,
        group: GroupId,
        subjects: &[Option<AccountId>],
    ) -> Result<Vec<Memory>, MemoryError>;

    async fn get(&self, group: GroupId, id: MemoryId) -> Result<Option<Memory>, MemoryError>;

    /// Delete a memory and its evidence. Whether it existed in the group.
    async fn delete(&self, group: GroupId, id: MemoryId) -> Result<bool, MemoryError>;

    /// Delete every memory about `subject` in the group; how many.
    async fn delete_about(&self, group: GroupId, subject: AccountId) -> Result<u64, MemoryError>;

    /// Delete memories to make room; how many existed.
    async fn evict(&self, group: GroupId, ids: &[MemoryId]) -> Result<u64, MemoryError>;
}

#[derive(Default)]
struct Inner {
    next: i64,
    memories: BTreeMap<MemoryId, Memory>,
}

impl Inner {
    fn current_mut(
        &mut self,
        group: GroupId,
        id: MemoryId,
        subject: Option<AccountId>,
    ) -> Option<&mut Memory> {
        self.memories
            .get_mut(&id)
            .filter(|m| m.group == group && m.subject == subject && m.is_current())
    }

    fn support(memory: &mut Memory, support: &Support) -> bool {
        if memory.support.iter().any(|s| s.episode == support.episode) {
            return false;
        }
        memory.support.push(support.clone());
        memory.support.sort_by_key(|s| (s.at, s.episode));
        memory.first = memory.first.min(support.at);
        memory.last = memory.last.max(support.at);
        true
    }

    /// The memory a statement is: the current one with its key, supported, or a new one.
    fn remember(&mut self, group: GroupId, statement: &Statement) -> MemoryId {
        let key = memory_key(statement.term.as_deref(), &statement.text);
        if let Some(existing) = self.memories.values_mut().find(|m| {
            m.group == group
                && m.subject == statement.subject
                && m.is_current()
                && memory_key(m.term.as_deref(), &m.text) == key
        }) {
            Self::support(existing, &statement.support);
            return existing.id;
        }
        self.next += 1;
        let id = MemoryId(self.next);
        self.memories.insert(
            id,
            Memory {
                id,
                group,
                subject: statement.subject,
                term: statement.term.clone(),
                text: statement.text.clone(),
                durability: statement.durability,
                state: MemoryState::Current,
                first: statement.support.at,
                last: statement.support.at,
                support: vec![statement.support.clone()],
            },
        );
        id
    }
}

/// The reference implementation.
#[derive(Default)]
pub struct InMemoryMemoryStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for InMemoryMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryMemoryStore")
            .finish_non_exhaustive()
    }
}

impl InMemoryMemoryStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl MemoryStore for InMemoryMemoryStore {
    async fn apply(
        &self,
        group: GroupId,
        changes: &[Change],
    ) -> Result<Vec<MemoryId>, MemoryError> {
        let mut inner = self.lock();
        let mut touched = Vec::new();
        for change in changes {
            match change {
                Change::New(statement) => touched.push(inner.remember(group, statement)),
                Change::Same {
                    id,
                    subject,
                    support,
                } => {
                    if let Some(memory) = inner.current_mut(group, *id, *subject) {
                        Inner::support(memory, support);
                        touched.push(*id);
                    }
                }
                Change::Replace { id, with } => {
                    let replaced = inner.current_mut(group, *id, with.subject).is_some();
                    let new = inner.remember(group, with);
                    touched.push(new);
                    if replaced
                        && new != *id
                        && let Some(old) = inner.memories.get_mut(id)
                    {
                        old.state = MemoryState::Replaced {
                            by: Some(new),
                            at: with.support.at,
                        };
                    }
                }
                Change::End { id, subject, at } => {
                    if let Some(memory) = inner.current_mut(group, *id, *subject) {
                        memory.state = MemoryState::Ended { at: *at };
                    }
                }
            }
        }
        touched.sort();
        touched.dedup();
        Ok(touched)
    }

    async fn about(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
    ) -> Result<Vec<Memory>, MemoryError> {
        Ok(self
            .lock()
            .memories
            .values()
            .filter(|m| m.group == group && m.subject == subject)
            .cloned()
            .collect())
    }

    async fn current(
        &self,
        group: GroupId,
        subjects: &[Option<AccountId>],
    ) -> Result<Vec<Memory>, MemoryError> {
        Ok(self
            .lock()
            .memories
            .values()
            .filter(|m| m.group == group && m.is_current() && subjects.contains(&m.subject))
            .cloned()
            .collect())
    }

    async fn get(&self, group: GroupId, id: MemoryId) -> Result<Option<Memory>, MemoryError> {
        Ok(self
            .lock()
            .memories
            .get(&id)
            .filter(|m| m.group == group)
            .cloned())
    }

    async fn delete(&self, group: GroupId, id: MemoryId) -> Result<bool, MemoryError> {
        let mut inner = self.lock();
        if inner.memories.get(&id).is_none_or(|m| m.group != group) {
            return Ok(false);
        }
        inner.memories.remove(&id);
        for memory in inner.memories.values_mut() {
            if let MemoryState::Replaced { by, .. } = &mut memory.state
                && *by == Some(id)
            {
                *by = None;
            }
        }
        Ok(true)
    }

    async fn delete_about(&self, group: GroupId, subject: AccountId) -> Result<u64, MemoryError> {
        let ids: Vec<MemoryId> = self
            .lock()
            .memories
            .values()
            .filter(|m| m.group == group && m.subject == Some(subject))
            .map(|m| m.id)
            .collect();
        let mut gone = 0;
        for id in ids {
            gone += u64::from(self.delete(group, id).await?);
        }
        Ok(gone)
    }

    async fn evict(&self, group: GroupId, ids: &[MemoryId]) -> Result<u64, MemoryError> {
        let mut gone = 0;
        for id in ids {
            gone += u64::from(self.delete(group, *id).await?);
        }
        Ok(gone)
    }
}
