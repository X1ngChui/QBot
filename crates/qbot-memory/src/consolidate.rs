//! Applying what an episode found: name uses go to the name index, statements become changes to
//! what is remembered. Idempotent: an episode records a name use and supports a memory at most
//! once, and a new statement equal to a current memory supports it, so applying the same episode
//! again changes nothing, and a crash between storing an episode and applying it loses nothing
//! (the episode stays unapplied until applied).
//!
//! Room follows the conversation: after an episode is applied, each subject it touched that is
//! over its capacity gives up its memories worth least, weighed as of the episode's end, never
//! what the episode itself touched. Nothing else ages memory; nothing runs on a schedule.

use std::collections::BTreeSet;
use std::sync::Arc;

use qbot_core::AccountId;

use crate::episode::Episode;
use crate::findings::{MemoryAction, MemoryFinding};
use crate::memories::{Change, Durability, MemoryId, MemoryStore, Statement, Support};
use crate::names::{NameStore, NameUse, UseSource, strongest_per_name};
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
        let uses = strongest_per_name(
            source
                .findings
                .names
                .iter()
                .map(|n| NameUse {
                    account: n.account,
                    text: n.name.clone(),
                    basis: n.basis,
                    source: Some(UseSource {
                        episode: episode.id,
                        message: n.message,
                        quote: n.quote.clone(),
                    }),
                    anchored: n.anchored,
                    at,
                })
                .collect(),
        );
        if !uses.is_empty() {
            self.names.record(group, &uses).await?;
        }
        let changes: Vec<Change> = source
            .findings
            .memories
            .iter()
            .filter_map(|f| change(f, episode, at))
            .collect();
        if changes.is_empty() {
            return Ok(());
        }
        let touched: BTreeSet<MemoryId> = self
            .memories
            .apply(group, &changes)
            .await?
            .into_iter()
            .collect();
        let Some(room) = &self.room else {
            return Ok(());
        };
        let subjects: BTreeSet<Option<AccountId>> =
            source.findings.memories.iter().map(|f| f.subject).collect();
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
}

/// The change one validated statement makes, at the episode's end.
fn change(f: &MemoryFinding, episode: &Episode, at: qbot_core::UnixMillis) -> Option<Change> {
    let support = Support {
        episode: episode.id,
        message: f.message,
        author: Some(f.author),
        quote: f.quote.clone(),
        at,
    };
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
