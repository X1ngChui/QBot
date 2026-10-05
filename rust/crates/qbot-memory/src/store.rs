//! The storage port for episodes, plus a reference in-memory implementation.

use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use qbot_core::{GroupId, UnixMillis};

use crate::episode::{Episode, EpisodeId, Hit, NewEpisode};
use crate::slice::SliceLine;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    /// The new episode's range overlaps an existing episode of the same group.
    #[error("episode range overlaps an existing episode")]
    Overlap,
    /// The range is reversed (first after last).
    #[error("episode range is invalid")]
    InvalidRange,
    #[error("storage failure: {0}")]
    Backend(String),
}

#[async_trait]
pub trait EpisodeStore: Send + Sync {
    /// Archived lines with ordinals in `first..=last`, ascending. Only lines that exist.
    async fn lines(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<SliceLine>, MemoryError>;

    /// The highest archived ordinal of the group, 0 when empty.
    async fn last_ordinal(&self, group: GroupId) -> Result<u64, MemoryError>;

    /// The last ordinal owned by the group's newest episode, 0 when there is none.
    async fn covered_through(&self, group: GroupId) -> Result<u64, MemoryError>;

    /// Store one episode with the embedding of its summary. Ranges of one group never overlap.
    async fn insert(
        &self,
        episode: &NewEpisode,
        vector: &[f32],
        embed_model: &str,
    ) -> Result<Episode, MemoryError>;

    async fn get(&self, group: GroupId, id: EpisodeId) -> Result<Option<Episode>, MemoryError>;

    /// Episodes of the group whose findings have not been applied yet, oldest first.
    async fn unconsolidated(&self, group: GroupId) -> Result<Vec<Episode>, MemoryError>;

    /// Record that an episode's findings have been applied.
    async fn mark_consolidated(&self, id: EpisodeId) -> Result<(), MemoryError>;

    /// Episodes of the group lying wholly inside the ordinal range, in order.
    async fn within(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<Episode>, MemoryError>;

    /// Episodes of the group whose last line lies in the ordinal range (they may begin before
    /// it), in order.
    async fn ending_within(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<Episode>, MemoryError>;

    /// Nearest episodes of the group by cosine distance, closest first, no farther than
    /// `max_distance`.
    async fn search(
        &self,
        group: GroupId,
        embed_model: &str,
        query: &[f32],
        limit: usize,
        max_distance: f32,
    ) -> Result<Vec<Hit>, MemoryError>;
}

#[derive(Default)]
struct Inner {
    lines: Vec<(GroupId, SliceLine)>,
    episodes: Vec<(Episode, String, Vec<f32>)>,
    next_id: i64,
    consolidated: std::collections::HashSet<EpisodeId>,
}

/// Reference implementation: defines the semantics durable stores must reproduce.
#[derive(Default)]
pub struct MemoryEpisodeStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for MemoryEpisodeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryEpisodeStore").finish_non_exhaustive()
    }
}

impl MemoryEpisodeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add archive lines (the real archive is a separate table).
    pub fn add_lines(&self, group: GroupId, lines: impl IntoIterator<Item = SliceLine>) {
        self.lock()
            .lines
            .extend(lines.into_iter().map(|l| (group, l)));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        1.0
    } else {
        1.0 - dot / (na * nb)
    }
}

#[async_trait]
impl EpisodeStore for MemoryEpisodeStore {
    async fn lines(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<SliceLine>, MemoryError> {
        let mut found: Vec<SliceLine> = self
            .lock()
            .lines
            .iter()
            .filter(|(g, l)| *g == group && (first_ordinal..=last_ordinal).contains(&l.ordinal))
            .map(|(_, l)| l.clone())
            .collect();
        found.sort_by_key(|l| l.ordinal);
        Ok(found)
    }

    async fn last_ordinal(&self, group: GroupId) -> Result<u64, MemoryError> {
        Ok(self
            .lock()
            .lines
            .iter()
            .filter(|(g, _)| *g == group)
            .map(|(_, l)| l.ordinal)
            .max()
            .unwrap_or(0))
    }

    async fn covered_through(&self, group: GroupId) -> Result<u64, MemoryError> {
        Ok(self
            .lock()
            .episodes
            .iter()
            .filter(|(e, _, _)| e.episode.group == group)
            .map(|(e, _, _)| e.episode.last_ordinal)
            .max()
            .unwrap_or(0))
    }

    async fn insert(
        &self,
        episode: &NewEpisode,
        vector: &[f32],
        embed_model: &str,
    ) -> Result<Episode, MemoryError> {
        let mut inner = self.lock();
        let overlaps = |e: &NewEpisode| {
            e.group == episode.group
                && e.first_ordinal <= episode.last_ordinal
                && episode.first_ordinal <= e.last_ordinal
        };
        if episode.first_ordinal > episode.last_ordinal || episode.first_batch > episode.last_batch
        {
            return Err(MemoryError::InvalidRange);
        }
        if inner.episodes.iter().any(|(e, _, _)| overlaps(&e.episode)) {
            return Err(MemoryError::Overlap);
        }
        inner.next_id += 1;
        let stored = Episode {
            id: EpisodeId::new(inner.next_id),
            created: UnixMillis::new(0),
            episode: episode.clone(),
        };
        inner
            .episodes
            .push((stored.clone(), embed_model.to_owned(), vector.to_vec()));
        Ok(stored)
    }

    async fn unconsolidated(&self, group: GroupId) -> Result<Vec<Episode>, MemoryError> {
        let inner = self.lock();
        let mut found: Vec<Episode> = inner
            .episodes
            .iter()
            .map(|(e, _, _)| e)
            .filter(|e| e.episode.group == group && !inner.consolidated.contains(&e.id))
            .cloned()
            .collect();
        found.sort_by_key(|e| e.id);
        Ok(found)
    }

    async fn mark_consolidated(&self, id: EpisodeId) -> Result<(), MemoryError> {
        self.lock().consolidated.insert(id);
        Ok(())
    }

    async fn get(&self, group: GroupId, id: EpisodeId) -> Result<Option<Episode>, MemoryError> {
        Ok(self
            .lock()
            .episodes
            .iter()
            .find(|(e, _, _)| e.id == id && e.episode.group == group)
            .map(|(e, _, _)| e.clone()))
    }

    async fn within(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<Episode>, MemoryError> {
        let mut found: Vec<Episode> = self
            .lock()
            .episodes
            .iter()
            .filter(|(e, _, _)| {
                e.episode.group == group
                    && e.episode.first_ordinal >= first_ordinal
                    && e.episode.last_ordinal <= last_ordinal
            })
            .map(|(e, _, _)| e.clone())
            .collect();
        found.sort_by_key(|e| e.episode.first_ordinal);
        Ok(found)
    }

    async fn ending_within(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<Episode>, MemoryError> {
        let mut found: Vec<Episode> = self
            .lock()
            .episodes
            .iter()
            .filter(|(e, _, _)| {
                e.episode.group == group
                    && (first_ordinal..=last_ordinal).contains(&e.episode.last_ordinal)
            })
            .map(|(e, _, _)| e.clone())
            .collect();
        found.sort_by_key(|e| e.episode.first_ordinal);
        Ok(found)
    }

    async fn search(
        &self,
        group: GroupId,
        embed_model: &str,
        query: &[f32],
        limit: usize,
        max_distance: f32,
    ) -> Result<Vec<Hit>, MemoryError> {
        let mut hits: Vec<Hit> = self
            .lock()
            .episodes
            .iter()
            .filter(|(e, model, v)| {
                e.episode.group == group && model == embed_model && v.len() == query.len()
            })
            .map(|(e, _, v)| Hit {
                episode: e.clone(),
                distance: cosine_distance(query, v),
            })
            .filter(|h| h.distance <= max_distance)
            .collect();
        hits.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then(a.episode.id.cmp(&b.episode.id))
        });
        hits.truncate(limit);
        Ok(hits)
    }
}
