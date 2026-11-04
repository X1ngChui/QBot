//! How much a memory is worth where room is limited (what a run is shown, what a full subject
//! keeps), on its group's own conversational clock. Worth is never shown and says nothing about
//! truth; what readers see is a memory's provenance.
//!
//! `worth = n / (n + 1) * 0.5^(k / h)`: `n` distinct supporting episodes (said again counts),
//! `k` episodes its subject took part in since the last support (for a group memory, every
//! episode of the group), `h` the half-life of its durability in episodes. A quiet member's or
//! group's clock does not move, so what they said keeps its place; a memory replaced or ended is
//! worth nothing beside current ones.

use std::collections::BTreeSet;

use qbot_core::{AccountId, UnixMillis};

use crate::memories::{Durability, Memory, MemoryId, MemoryState};

/// The moment ranking is computed for in what replies are shown: the start of `now`'s UTC day, so
/// ageing changes the cached prompt prefix at most once a day.
pub fn as_of_day(now: UnixMillis) -> UnixMillis {
    const DAY_MS: i64 = 86_400_000;
    UnixMillis::new(now.get() - now.get().rem_euclid(DAY_MS))
}

/// A group's conversation as a clock: when each of its episodes ended and who took part, in chat
/// order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    episodes: Vec<(UnixMillis, Vec<AccountId>)>,
}

impl Activity {
    pub fn new(mut episodes: Vec<(UnixMillis, Vec<AccountId>)>) -> Self {
        episodes.sort_by_key(|(ended, _)| *ended);
        Self { episodes }
    }

    /// Episodes ended after `after` and no later than `until` that bear on a memory about
    /// `subject`: those the member took part in, or for the group, every one.
    pub fn since(&self, subject: Option<AccountId>, after: UnixMillis, until: UnixMillis) -> u32 {
        let count = self
            .episodes
            .iter()
            .filter(|(ended, _)| *ended > after && *ended <= until)
            .filter(|(_, people)| subject.is_none_or(|s| people.contains(&s)))
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }
}

/// Episodes per halving of rank, by durability.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HalfLives {
    pub lasting: f64,
    pub ongoing: f64,
    pub current: f64,
}

impl Default for HalfLives {
    /// A lasting fact outlives hundreds of its subject's conversations, a taste dozens, what
    /// someone is doing these weeks a handful.
    fn default() -> Self {
        Self {
            lasting: 200.0,
            ongoing: 40.0,
            current: 10.0,
        }
    }
}

impl HalfLives {
    pub fn of(&self, durability: Durability) -> f64 {
        match durability {
            Durability::Lasting => self.lasting,
            Durability::Ongoing => self.ongoing,
            Durability::Current => self.current,
        }
    }

    /// The memory's worth as of `until`. A replaced or ended memory is worth 0.
    pub fn worth(&self, memory: &Memory, activity: &Activity, until: UnixMillis) -> f64 {
        if !memory.is_current() {
            return 0.0;
        }
        let n = f64::from(memory.episodes());
        let k = f64::from(activity.since(memory.subject, memory.last, until));
        let h = self.of(memory.durability).max(f64::MIN_POSITIVE);
        n / (n + 1.0) * 0.5_f64.powf(k / h)
    }

    /// `memories` best first: the more worth, then the more recently supported, then the newer.
    pub fn ranked<'a>(
        &self,
        memories: &'a [Memory],
        activity: &Activity,
        until: UnixMillis,
    ) -> Vec<&'a Memory> {
        let mut scored: Vec<(&Memory, f64)> = memories
            .iter()
            .map(|m| (m, self.worth(m, activity, until)))
            .collect();
        scored.sort_by(|(a, x), (b, y)| {
            y.total_cmp(x)
                .then(b.last.cmp(&a.last))
                .then(b.id.cmp(&a.id))
        });
        scored.into_iter().map(|(m, _)| m).collect()
    }
}

/// Memories one subject keeps at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub per_member: usize,
    pub group: usize,
}

impl Default for Capacity {
    /// Bounds what a busy group accumulates and what the extractor reads about each member.
    fn default() -> Self {
        Self {
            per_member: 30,
            group: 120,
        }
    }
}

impl Capacity {
    pub fn of(&self, subject: Option<AccountId>) -> usize {
        if subject.is_some() {
            self.per_member
        } else {
            self.group
        }
    }

    /// What one subject's memories give up to fit its room, as of `until`: replaced and ended
    /// memories first (the longest closed first), then current ones by least worth, never one in
    /// `keep` (what the episode being applied touched). A subject may stay over its room by what
    /// `keep` holds.
    pub fn evictions(
        &self,
        memories: &[Memory],
        keep: &BTreeSet<MemoryId>,
        half_lives: &HalfLives,
        activity: &Activity,
        until: UnixMillis,
    ) -> Vec<MemoryId> {
        let Some(subject) = memories.first().map(|m| m.subject) else {
            return Vec::new();
        };
        let room = self.of(subject);
        if memories.len() <= room {
            return Vec::new();
        }
        let closed = |m: &Memory| match m.state {
            MemoryState::Current => None,
            MemoryState::Replaced { at, .. } | MemoryState::Ended { at } => Some(at),
        };
        let mut candidates: Vec<(&Memory, f64)> = memories
            .iter()
            .filter(|m| !keep.contains(&m.id))
            .map(|m| (m, half_lives.worth(m, activity, until)))
            .collect();
        // Closed ones first, oldest closed first; then the least worth, the least recently
        // supported, the older.
        candidates.sort_by(|(a, x), (b, y)| {
            closed(b)
                .is_some()
                .cmp(&closed(a).is_some())
                .then(closed(a).cmp(&closed(b)))
                .then(x.total_cmp(y))
                .then(a.last.cmp(&b.last))
                .then(a.id.cmp(&b.id))
        });
        candidates
            .into_iter()
            .take(memories.len() - room)
            .map(|(m, _)| m.id)
            .collect()
    }
}
