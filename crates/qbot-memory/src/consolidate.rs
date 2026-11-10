//! Applying what an episode found: references go to the name ledger, statements become changes
//! to what is remembered, and a statement about a person known only by a name is held until the
//! name is some member's. Idempotent: an episode records a reference and supports a memory at
//! most once, and a new statement equal to a current memory supports it, so applying the same
//! episode again changes nothing, and a crash between storing an episode and applying it loses
//! nothing (the episode stays unapplied until applied).
//!
//! Room follows the conversation: after an episode is applied, each subject it touched that is
//! over its capacity gives up its memories worth least, weighed as of the episode's end, never
//! what the episode itself touched. Nothing else ages memory; nothing runs on a schedule.

use std::collections::BTreeSet;
use std::sync::Arc;

use qbot_core::{AccountId, GroupId};

use crate::episode::Episode;
use crate::findings::{MemoryAction, MemoryFinding};
use crate::memories::{Change, Durability, MemoryId, MemoryStore, Statement, Support};
use crate::names::{HeldClaim, NameStore, Reference, name_key};
use crate::store::{EpisodeStore, MemoryError};
use crate::worth::{Capacity, HalfLives};

pub struct Consolidator {
    names: Arc<dyn NameStore>,
    memories: Arc<dyn MemoryStore>,
    room: Option<Room>,
}

/// What capacity needs: the group's activity, how fast memories lose rank, and the room.
struct Room {
    episodes: Arc<dyn EpisodeStore>,
    half_lives: HalfLives,
    capacity: Capacity,
}

impl std::fmt::Debug for Consolidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Consolidator").finish_non_exhaustive()
    }
}

impl Consolidator {
    pub fn new(names: Arc<dyn NameStore>, memories: Arc<dyn MemoryStore>) -> Self {
        Self {
            names,
            memories,
            room: None,
        }
    }

    /// Keep each subject within `capacity` as episodes are applied, weighing memories on the
    /// activity of `episodes`. Without it, memories accumulate.
    pub fn with_capacity(
        mut self,
        episodes: Arc<dyn EpisodeStore>,
        half_lives: HalfLives,
        capacity: Capacity,
    ) -> Self {
        self.room = Some(Room {
            episodes,
            half_lives,
            capacity,
        });
        self
    }

    pub async fn apply(&self, episode: &Episode) -> Result<(), MemoryError> {
        let source = &episode.episode;
        let (group, at) = (source.group, source.ended);
        let references: Vec<Reference> = source
            .findings
            .references
            .iter()
            .map(|r| Reference {
                episode: episode.id,
                message: r.message,
                ordinal: r.ordinal,
                speaker: r.speaker,
                text: r.text.clone(),
                referent: r.referent,
                basis: r.basis,
                grounds: r.grounds,
                anchored: r.anchored,
                shown_by: r.shown_by.clone(),
                at,
            })
            .collect();
        if !references.is_empty() {
            self.names.record(group, &references).await?;
        }
        let held: Vec<HeldClaim> = source
            .findings
            .memories
            .iter()
            .filter_map(|f| {
                Some(HeldClaim {
                    name: f.name.clone()?,
                    text: f.text.clone()?,
                    durability: f.durability.unwrap_or(Durability::Ongoing),
                    support: support(f, episode, at),
                })
            })
            .collect();
        if !held.is_empty() {
            self.names.hold(group, &held).await?;
        }
        let stated = || source.findings.memories.iter().filter(|f| f.name.is_none());
        let changes: Vec<Change> = stated().filter_map(|f| change(f, episode, at)).collect();
        let mut touched: BTreeSet<MemoryId> = BTreeSet::new();
        let mut subjects: BTreeSet<Option<AccountId>> = BTreeSet::new();
        if !changes.is_empty() {
            touched.extend(self.memories.apply(group, &changes).await?);
            subjects.extend(stated().map(|f| f.subject));
        }
        // A name these references settled frees what was held for it.
        for (account, memory) in self.file_held(group).await? {
            touched.insert(memory);
            subjects.insert(Some(account));
        }
        let Some(room) = &self.room else {
            return Ok(());
        };
        if subjects.is_empty() {
            return Ok(());
        }
        let activity = room.episodes.activity(group).await?;
        for subject in subjects {
            let memories = self.memories.about(group, subject).await?;
            let evict =
                room.capacity
                    .evictions(&memories, &touched, &room.half_lives, &activity, at);
            if !evict.is_empty() {
                let evicted = self.memories.evict(group, &evict).await?;
                tracing::info!(
                    group = group.get(),
                    evicted,
                    "memories worth least made room"
                );
            }
        }
        Ok(())
    }

    /// Remember what was held about a name that is now one member's: the member it was set
    /// for, ruled to be, or that separate conversations agree on. A name two members have a
    /// claim to frees nothing. Returns the members and the memories this supported or created.
    pub async fn file_held(
        &self,
        group: GroupId,
    ) -> Result<Vec<(AccountId, MemoryId)>, MemoryError> {
        let held = self.names.held(group).await?;
        if held.is_empty() {
            return Ok(Vec::new());
        }
        let names = self.names.names(group, None).await?;
        let mut filed = Vec::new();
        for (id, claim) in held {
            let Ok(key) = name_key(&claim.name) else {
                continue;
            };
            let owners: Vec<AccountId> = names
                .iter()
                .filter(|n| n.key == key && (n.vouched() || n.corroborated()))
                .map(|n| n.account)
                .collect();
            let [account] = owners.as_slice() else {
                continue;
            };
            let statement = Statement {
                subject: Some(*account),
                term: None,
                text: claim.text.clone(),
                durability: claim.durability,
                support: claim.support.clone(),
            };
            let memories = self
                .memories
                .apply(group, &[Change::New(statement)])
                .await?;
            self.names.file(group, id, *account).await?;
            filed.extend(memories.into_iter().map(|memory| (*account, memory)));
        }
        if !filed.is_empty() {
            tracing::info!(
                group = group.get(),
                filed = filed.len(),
                "statements held for a name are remembered about its member"
            );
        }
        Ok(filed)
    }
}

fn support(f: &MemoryFinding, episode: &Episode, at: qbot_core::UnixMillis) -> Support {
    Support {
        episode: episode.id,
        message: f.message,
        author: Some(f.author),
        quote: f.quote.clone(),
        at,
    }
}

/// The change one validated statement makes, at the episode's end.
fn change(f: &MemoryFinding, episode: &Episode, at: qbot_core::UnixMillis) -> Option<Change> {
    let support = support(f, episode, at);
    let statement = || Statement {
        subject: f.subject,
        term: f.term.clone(),
        text: f.text.clone().unwrap_or_default(),
        durability: f.durability.unwrap_or(Durability::Ongoing),
        support: support.clone(),
    };
    Some(match (f.action, f.id) {
        (MemoryAction::New, _) => Change::New(statement()),
        (MemoryAction::Same, Some(id)) => Change::Same {
            id,
            subject: f.subject,
            support,
        },
        (MemoryAction::Changed, Some(id)) => Change::Replace {
            id,
            with: statement(),
        },
        (MemoryAction::Ended, Some(id)) => Change::End {
            id,
            subject: f.subject,
            at,
        },
        // Validation requires an id for these; one stored without it changes nothing.
        (_, None) => return None,
    })
}
