//! Names people use for one another, as a reference index.
//!
//! A name is not a belief about an account but a record of *uses*: in this episode, this line
//! used the name for this member, as their own name, to address them, or talking about them; or a
//! person set it with `/name`. Whom a name refers to is read from all its uses together
//! ([`KnownName`] per member and name), so a name used for one member eight times and for another
//! once reads exactly so, and a name used for several members is visibly shared.
//!
//! What a member is *shown* as (their group display name) is not a use: the archive records it
//! with every line they send.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::episode::EpisodeId;
use crate::store::MemoryError;

/// The longest name stored. A name is a few words; this keeps absurd input out of the index.
pub const MAX_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameTextError {
    #[error("a name must not be empty")]
    Empty,
    #[error("a name is at most {MAX_NAME_CHARS} characters")]
    TooLong,
}

/// The form names are compared in: compatibility-normalized (full-width and half-width forms
/// agree), whitespace collapsed, lower-cased.
pub fn name_key(text: &str) -> Result<String, NameTextError> {
    let folded: String = text.nfkc().collect::<String>().to_lowercase();
    let collapsed = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err(NameTextError::Empty);
    }
    if collapsed.chars().count() > MAX_NAME_CHARS {
        return Err(NameTextError::TooLong);
    }
    Ok(collapsed)
}

/// A name as it is kept and shown: as written, with its whitespace collapsed.
pub fn tidy_name(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How a line used a name for a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameBasis {
    /// A person set it with `/name`: vouched for, not inferred.
    Set,
    /// The member gave it as their own name ("call me X").
    Own,
    /// Someone addressed the member by it, on a line that mentions or answers them.
    Addressed,
    /// Someone used it talking about the member.
    About,
}

impl NameBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            NameBasis::Set => "set",
            NameBasis::Own => "own",
            NameBasis::Addressed => "addressed",
            NameBasis::About => "about",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "set" => NameBasis::Set,
            "own" => NameBasis::Own,
            "addressed" => NameBasis::Addressed,
            "about" => NameBasis::About,
            _ => return None,
        })
    }

    /// The stronger of two uses in one episode: the member's own word, then addressing them,
    /// then talk about them.
    fn strength(self) -> u8 {
        match self {
            NameBasis::Set => 3,
            NameBasis::Own => 2,
            NameBasis::Addressed => 1,
            NameBasis::About => 0,
        }
    }
}

/// The line a learned use comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseSource {
    pub episode: EpisodeId,
    pub message: MessageId,
    pub quote: String,
}

/// One use of a name for a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameUse {
    pub account: AccountId,
    /// As written.
    pub text: String,
    pub basis: NameBasis,
    /// `None` exactly for [`NameBasis::Set`].
    pub source: Option<UseSource>,
    pub at: UnixMillis,
}

/// What the uses of one name for one member add up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownName {
    pub account: AccountId,
    /// As most recently written.
    pub text: String,
    pub key: String,
    /// A person set it.
    pub set: bool,
    /// Distinct episodes in which the member gave it as their own, someone addressed them by
    /// it, someone talked about them by it.
    pub own: u32,
    pub addressed: u32,
    pub about: u32,
    /// The latest use.
    pub last: UnixMillis,
}

impl KnownName {
    /// Episodes in which the name was used for the member.
    pub fn uses(&self) -> u32 {
        self.own + self.addressed + self.about
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error(transparent)]
    Text(#[from] NameTextError),
    /// A person already set the name for a member not among those given.
    #[error("the name is already set for another member")]
    Taken,
    #[error("storage failure: {0}")]
    Backend(String),
}

impl From<MemoryError> for NameError {
    fn from(error: MemoryError) -> Self {
        NameError::Backend(error.to_string())
    }
}

#[async_trait]
pub trait NameStore: Send + Sync {
    /// Record learned uses. A use of a name a person removed from that member is not recorded,
    /// nor one already recorded for that member, name and episode; returns how many were.
    async fn record(&self, group: GroupId, uses: &[NameUse]) -> Result<u64, MemoryError>;

    /// A person sets a name for each of `accounts` (one account, or the accounts of one person in
    /// the group), lifting a removal. Refused when a person already set it for another member.
    async fn set(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<(), NameError>;

    /// A person removes a name from `accounts`: its uses are deleted and later learned uses are
    /// not recorded. Returns whether any use was deleted.
    async fn remove(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<bool, NameError>;

    /// The group's names, per member and name; for the given accounts only, or all of them.
    /// Ordered by account, then key.
    async fn names(
        &self,
        group: GroupId,
        accounts: Option<&[AccountId]>,
    ) -> Result<Vec<KnownName>, MemoryError>;
}

/// The uses one episode makes, one per member and name, the strongest kept.
pub fn strongest_per_name(uses: Vec<NameUse>) -> Vec<NameUse> {
    let mut best: BTreeMap<(AccountId, String), NameUse> = BTreeMap::new();
    for one in uses {
        let Ok(key) = name_key(&one.text) else {
            continue;
        };
        match best.get(&(one.account, key.clone())) {
            Some(kept) if kept.basis.strength() >= one.basis.strength() => {}
            _ => {
                best.insert((one.account, key), one);
            }
        }
    }
    best.into_values().collect()
}

/// Sum the uses of each member and name. `uses` are `(key, use)`.
pub fn summarize<'a>(uses: impl IntoIterator<Item = (&'a str, &'a NameUse)>) -> Vec<KnownName> {
    let mut out: BTreeMap<(AccountId, String), KnownName> = BTreeMap::new();
    for (key, one) in uses {
        let entry = out
            .entry((one.account, key.to_owned()))
            .or_insert_with(|| KnownName {
                account: one.account,
                text: one.text.clone(),
                key: key.to_owned(),
                set: false,
                own: 0,
                addressed: 0,
                about: 0,
                last: one.at,
            });
        match one.basis {
            NameBasis::Set => entry.set = true,
            NameBasis::Own => entry.own += 1,
            NameBasis::Addressed => entry.addressed += 1,
            NameBasis::About => entry.about += 1,
        }
        if one.at >= entry.last {
            entry.last = one.at;
            entry.text = one.text.clone();
        }
    }
    out.into_values().collect()
}

#[derive(Default)]
struct Inner {
    /// `(group, account, key, episode)`.
    uses: BTreeMap<(GroupId, AccountId, String, Option<EpisodeId>), NameUse>,
    removed: BTreeSet<(GroupId, AccountId, String)>,
}

/// The reference implementation.
#[derive(Default)]
pub struct MemoryNameStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for MemoryNameStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryNameStore").finish_non_exhaustive()
    }
}

impl MemoryNameStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl NameStore for MemoryNameStore {
    async fn record(&self, group: GroupId, uses: &[NameUse]) -> Result<u64, MemoryError> {
        let mut inner = self.lock();
        let mut recorded = 0;
        for one in uses {
            let (Ok(key), Some(source)) = (name_key(&one.text), &one.source) else {
                continue;
            };
            if one.basis == NameBasis::Set
                || inner.removed.contains(&(group, one.account, key.clone()))
            {
                continue;
            }
            let id = (group, one.account, key, Some(source.episode));
            if let std::collections::btree_map::Entry::Vacant(slot) = inner.uses.entry(id) {
                slot.insert(NameUse {
                    text: tidy_name(&one.text),
                    ..one.clone()
                });
                recorded += 1;
            }
        }
        Ok(recorded)
    }

    async fn set(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<(), NameError> {
        let key = name_key(text)?;
        let mut inner = self.lock();
        let taken = inner.uses.iter().any(|((g, account, k, episode), _)| {
            *g == group && *k == key && episode.is_none() && !accounts.contains(account)
        });
        if taken {
            return Err(NameError::Taken);
        }
        for account in accounts {
            inner.removed.remove(&(group, *account, key.clone()));
            inner.uses.insert(
                (group, *account, key.clone(), None),
                NameUse {
                    account: *account,
                    text: tidy_name(text),
                    basis: NameBasis::Set,
                    source: None,
                    at,
                },
            );
        }
        Ok(())
    }

    async fn remove(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        _at: UnixMillis,
    ) -> Result<bool, NameError> {
        let key = name_key(text)?;
        let mut inner = self.lock();
        let before = inner.uses.len();
        inner.uses.retain(|(g, account, k, _), _| {
            !(*g == group && *k == key && accounts.contains(account))
        });
        for account in accounts {
            inner.removed.insert((group, *account, key.clone()));
        }
        Ok(inner.uses.len() < before)
    }

    async fn names(
        &self,
        group: GroupId,
        accounts: Option<&[AccountId]>,
    ) -> Result<Vec<KnownName>, MemoryError> {
        let inner = self.lock();
        Ok(summarize(
            inner
                .uses
                .iter()
                .filter(|((g, account, _, _), _)| {
                    *g == group && accounts.is_none_or(|wanted| wanted.contains(account))
                })
                .map(|((_, _, key, _), one)| (key.as_str(), one)),
        ))
    }
}
