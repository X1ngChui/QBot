//! The storage port for episodes and the archive lines they are read from. The in-memory
//! reference implementation is [`crate::InMemoryStore`].

use async_trait::async_trait;
use qbot_core::{GroupId, MessageId, UnixMillis};

use crate::episode::{Episode, EpisodeId, Hit, NewEpisode};
use crate::slice::SliceLine;
use crate::worth::Activity;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    /// The new episode's range overlaps an existing episode of the same group.
    #[error("episode range overlaps an existing episode")]
    Overlap,
    /// The range is reversed (first after last).
    #[error("episode range is invalid")]
    InvalidRange,
    #[error("no such episode")]
    NotFound,
    #[error("storage failure: {0}")]
    Backend(String),
}

/// What a group's view of its history hangs on between two batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowState {
    /// When the group's last batch ran: what runs are shown of who is who and what the group
    /// knows is as of then.
    pub batched: UnixMillis,
    /// Where a run that the provider found too long moved the start of the raw part; 0 when no
    /// run did since the batch.
    pub cut: u64,
}

#[async_trait]
pub trait EpisodeStore: Send + Sync {
    /// Every group with archived lines.
    async fn groups(&self) -> Result<Vec<GroupId>, MemoryError>;

    /// The group's window state; `None` before its first batch.
    async fn window(&self, group: GroupId) -> Result<Option<WindowState>, MemoryError>;

    /// Move the start of the group's raw part to `ordinal`, never back. Holds until the next
    /// batch.
    async fn cut(&self, group: GroupId, ordinal: u64, at: UnixMillis) -> Result<(), MemoryError>;

    /// The `limit` newest episodes of the group that end before `ordinal`, in chat order.
    async fn before(
        &self,
        group: GroupId,
        ordinal: u64,
        limit: usize,
    ) -> Result<Vec<Episode>, MemoryError>;

    /// Archived lines with ordinals in `first..=last`, ascending. Only lines that exist.
    async fn lines(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<SliceLine>, MemoryError>;

    /// For each of `messages`, the newest line of the group with that message id and an ordinal
    /// below `before`, oldest first: the lines a quote points at.
    async fn quoted(
        &self,
        group: GroupId,
        messages: &[MessageId],
        before: u64,
    ) -> Result<Vec<SliceLine>, MemoryError>;

    /// The highest archived ordinal of the group, 0 when empty.
    async fn last_ordinal(&self, group: GroupId) -> Result<u64, MemoryError>;

    /// The last ordinal covered by the group's newest episode or skipped stretch; 0 when there
    /// is none. Everything up to it was read by a batch.
    async fn covered_through(&self, group: GroupId) -> Result<u64, MemoryError>;

    /// Store one episode with the embedding of its summary. Ranges of one group never overlap,
    /// neither each other nor a skipped stretch. (The batch stores its episodes with its
    /// commit; this is for seeding.)
    async fn insert(
        &self,
        episode: &NewEpisode,
        vector: &[f32],
        embed_model: &str,
    ) -> Result<Episode, MemoryError>;

    async fn get(&self, group: GroupId, id: EpisodeId) -> Result<Option<Episode>, MemoryError>;

    /// The group's conversation as a clock for salience: every episode's end and participants.
    async fn activity(&self, group: GroupId) -> Result<Activity, MemoryError>;

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

    /// Episodes of the group that have no embedding from `embed_model` at width `dims`, oldest
    /// first, at most `limit`: what recall cannot find until they are embedded.
    async fn unembedded(
        &self,
        group: GroupId,
        embed_model: &str,
        dims: usize,
        limit: usize,
    ) -> Result<Vec<Episode>, MemoryError>;

    /// Make `vector` (from `embed_model`) the episode's embedding. An episode has one: another
    /// model's is replaced.
    async fn set_embedding(
        &self,
        id: EpisodeId,
        embed_model: &str,
        vector: &[f32],
    ) -> Result<(), MemoryError>;

    /// Every episode of the group no farther than `max_distance` (cosine) from `query`, closest
    /// first. Which of them are returned to the model, and in what order, is decided by
    /// [`Recall`](crate::Recall).
    async fn search(
        &self,
        group: GroupId,
        embed_model: &str,
        query: &[f32],
        max_distance: f32,
    ) -> Result<Vec<Hit>, MemoryError>;
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
