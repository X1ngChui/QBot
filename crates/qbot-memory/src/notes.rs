//! Notes people write by hand about a member of a group.
//!
//! Notes are not memory: nothing here is inferred, scored or decayed, and extraction never reads
//! or writes them. A note is exactly what someone typed, kept until someone removes it. Learned
//! facts live in [`crate::facts`]; the two stores never touch each other.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, UnixMillis};

use crate::store::MemoryError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoteId(i64);

impl NoteId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: NoteId,
    pub group: GroupId,
    /// The account the note is about.
    pub account: AccountId,
    pub text: String,
    /// Who wrote it (or last edited it).
    pub author: AccountId,
    pub created: UnixMillis,
    pub updated: UnixMillis,
}

#[async_trait]
pub trait NoteStore: Send + Sync {
    /// The notes about these accounts in the group, oldest first within each account, accounts
    /// in the order given. This order is the numbering commands show.
    async fn notes(&self, group: GroupId, accounts: &[AccountId])
    -> Result<Vec<Note>, MemoryError>;

    async fn add(
        &self,
        group: GroupId,
        account: AccountId,
        text: &str,
        author: AccountId,
        at: UnixMillis,
    ) -> Result<Note, MemoryError>;

    /// Replace a note's text. Whether the note existed in the group.
    async fn edit(
        &self,
        group: GroupId,
        id: NoteId,
        text: &str,
        author: AccountId,
        at: UnixMillis,
    ) -> Result<bool, MemoryError>;

    /// Whether the note existed in the group.
    async fn remove(&self, group: GroupId, id: NoteId) -> Result<bool, MemoryError>;

    /// Remove every note about these accounts in the group; how many.
    async fn clear(&self, group: GroupId, accounts: &[AccountId]) -> Result<u64, MemoryError>;
}

#[derive(Debug, Default)]
pub struct MemoryNoteStore {
    inner: Mutex<(i64, BTreeMap<NoteId, Note>)>,
}

impl MemoryNoteStore {
    fn lock(&self) -> MutexGuard<'_, (i64, BTreeMap<NoteId, Note>)> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl NoteStore for MemoryNoteStore {
    async fn notes(
        &self,
        group: GroupId,
        accounts: &[AccountId],
    ) -> Result<Vec<Note>, MemoryError> {
        let inner = self.lock();
        Ok(accounts
            .iter()
            .flat_map(|account| {
                inner
                    .1
                    .values()
                    .filter(move |n| n.group == group && n.account == *account)
                    .cloned()
            })
            .collect())
    }

    async fn add(
        &self,
        group: GroupId,
        account: AccountId,
        text: &str,
        author: AccountId,
        at: UnixMillis,
    ) -> Result<Note, MemoryError> {
        let mut inner = self.lock();
        inner.0 += 1;
        let note = Note {
            id: NoteId(inner.0),
            group,
            account,
            text: text.to_owned(),
            author,
            created: at,
            updated: at,
        };
        inner.1.insert(note.id, note.clone());
        Ok(note)
    }

    async fn edit(
        &self,
        group: GroupId,
        id: NoteId,
        text: &str,
        author: AccountId,
        at: UnixMillis,
    ) -> Result<bool, MemoryError> {
        let mut inner = self.lock();
        match inner.1.get_mut(&id).filter(|n| n.group == group) {
            Some(note) => {
                note.text = text.to_owned();
                note.author = author;
                note.updated = at;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn remove(&self, group: GroupId, id: NoteId) -> Result<bool, MemoryError> {
        let mut inner = self.lock();
        if inner.1.get(&id).is_some_and(|n| n.group == group) {
            inner.1.remove(&id);
            return Ok(true);
        }
        Ok(false)
    }

    async fn clear(&self, group: GroupId, accounts: &[AccountId]) -> Result<u64, MemoryError> {
        let mut inner = self.lock();
        let before = inner.1.len();
        inner
            .1
            .retain(|_, n| !(n.group == group && accounts.contains(&n.account)));
        Ok((before - inner.1.len()) as u64)
    }
}
