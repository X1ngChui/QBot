//! Finding episodes by meaning.

use std::sync::Arc;
use std::time::Duration;

use qbot_core::{Clock, GroupId, UnixMillis};
use qbot_llm::{Embedder, LlmError};

use crate::episode::Hit;
use crate::store::{EpisodeStore, MemoryError};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecallParams {
    pub limit: usize,
    /// Episodes farther than this (cosine distance) are not relevant enough to be candidates. A
    /// quality choice, tuned per embedding model; below 1, so every candidate is similar.
    pub max_distance: f32,
    /// How fast an episode's weight fades with age: it halves every `half_life` since the
    /// episode ended. Zero switches the decay off (similarity alone).
    pub half_life: Duration,
}

impl Default for RecallParams {
    fn default() -> Self {
        Self {
            limit: 5,
            max_distance: 0.45,
            half_life: Duration::from_secs(180 * 86_400),
        }
    }
}

/// How useful a candidate is now: its similarity (`1 - cosine distance`) times
/// `0.5^(age / half_life)`, that is `exp(-ln 2 / half_life * age)`. There is no age cutoff: an
/// old episode only weighs less, so a strong old match still beats a weak recent one.
pub fn score(hit: &Hit, now: UnixMillis, half_life: Duration) -> f64 {
    let similarity = 1.0 - f64::from(hit.distance);
    if half_life.is_zero() {
        return similarity;
    }
    let age = now.since(hit.episode.episode.ended).as_secs_f64();
    similarity * 0.5_f64.powf(age / half_life.as_secs_f64())
}

/// The candidates best first, cut to `limit`: by [`score`], then the more recent episode, then
/// the higher id, so the order never depends on how the store returned them.
pub fn rank(mut hits: Vec<Hit>, params: &RecallParams, now: UnixMillis) -> Vec<Hit> {
    hits.sort_by(|a, b| {
        score(b, now, params.half_life)
            .total_cmp(&score(a, now, params.half_life))
            .then(b.episode.episode.ended.cmp(&a.episode.episode.ended))
            .then(b.episode.id.cmp(&a.episode.id))
    });
    hits.truncate(params.limit);
    hits
}

#[derive(Debug, thiserror::Error)]
pub enum RecallError {
    #[error("embedding failed: {0}")]
    Embed(#[from] LlmError),
    #[error(transparent)]
    Store(#[from] MemoryError),
}

pub struct Recall {
    store: Arc<dyn EpisodeStore>,
    embedder: Arc<dyn Embedder>,
    clock: Arc<dyn Clock>,
    params: RecallParams,
}

impl std::fmt::Debug for Recall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recall").finish_non_exhaustive()
    }
}

impl Recall {
    pub fn new(
        store: Arc<dyn EpisodeStore>,
        embedder: Arc<dyn Embedder>,
        clock: Arc<dyn Clock>,
        params: RecallParams,
    ) -> Self {
        Self {
            store,
            embedder,
            clock,
            params,
        }
    }

    pub async fn recall(&self, group: GroupId, question: &str) -> Result<Vec<Hit>, RecallError> {
        let embedded = self.embedder.embed(&[question.to_owned()]).await?;
        let vector = embedded.vectors.into_iter().next().unwrap_or_default();
        let candidates = self
            .store
            .search(
                group,
                &self.embedder.info().model,
                &vector,
                self.params.max_distance,
            )
            .await?;
        Ok(rank(candidates, &self.params, self.clock.now()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::episode::{Episode, EpisodeId, NewEpisode};
    use qbot_core::MessageId;

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 2_000 * DAY;

    /// Episode `id`, ended `days` ago, at cosine `distance` from the question.
    fn hit(id: i64, days: i64, distance: f32) -> Hit {
        let message = MessageId::new(1).unwrap();
        let ended = UnixMillis::new(NOW - days * DAY);
        Hit {
            episode: Episode {
                id: EpisodeId::new(id),
                created: ended,
                episode: NewEpisode {
                    group: GroupId::new(1).unwrap(),
                    first_batch: 0,
                    last_batch: 0,
                    first_ordinal: 1,
                    last_ordinal: 10,
                    batch_lines: 10,
                    first_message: message,
                    last_message: message,
                    started: ended,
                    ended,
                    line_count: 10,
                    participants: Vec::new(),
                    title: format!("e{id}"),
                    summary: String::new(),
                    evidence: Vec::new(),
                    method: String::new(),
                    model: String::new(),
                    findings: Default::default(),
                },
            },
            distance,
        }
    }

    fn order(hits: Vec<Hit>, limit: usize, half_life_days: u64) -> Vec<String> {
        let params = RecallParams {
            limit,
            max_distance: 0.9,
            half_life: Duration::from_secs(half_life_days * 86_400),
        };
        rank(hits, &params, UnixMillis::new(NOW))
            .into_iter()
            .map(|h| h.episode.episode.title)
            .collect()
    }

    #[test]
    fn the_score_halves_every_half_life() {
        let half = Duration::from_secs(180 * 86_400);
        let now = UnixMillis::new(NOW);
        let fresh = score(&hit(1, 0, 0.2), now, half);
        assert!((fresh - 0.8).abs() < 1e-6);
        assert!((score(&hit(1, 180, 0.2), now, half) - 0.4).abs() < 1e-6);
        assert!((score(&hit(1, 360, 0.2), now, half) - 0.2).abs() < 1e-6);
        assert!(
            (score(&hit(1, 360, 0.2), now, Duration::ZERO) - 0.8).abs() < 1e-6,
            "no decay: similarity alone"
        );
    }

    #[test]
    fn a_somewhat_newer_slightly_less_similar_episode_outranks_a_very_old_one() {
        // 0.8 a year ago (0.8 * 0.5^(365/180) = 0.20) against 0.7 ten days ago (0.67).
        assert_eq!(
            order(vec![hit(1, 365, 0.2), hit(2, 10, 0.3)], 5, 180),
            ["e2", "e1"]
        );
        // Equally similar: the newer first.
        assert_eq!(
            order(vec![hit(1, 100, 0.3), hit(2, 20, 0.3)], 5, 180),
            ["e2", "e1"]
        );
    }

    #[test]
    fn a_small_age_difference_does_not_override_a_clear_relevance_gap() {
        // 0.9 a month ago (0.80) against 0.6 today (0.60).
        assert_eq!(
            order(vec![hit(1, 30, 0.1), hit(2, 0, 0.4)], 5, 180),
            ["e1", "e2"]
        );
    }

    #[test]
    fn a_very_strong_old_match_still_surfaces() {
        // 0.98 three months ago (0.69) beats 0.56 today.
        assert_eq!(
            order(vec![hit(2, 0, 0.44), hit(1, 90, 0.02)], 1, 180),
            ["e1"]
        );
        // No age cutoff: a five-year-old match is still returned, behind newer ones.
        assert_eq!(
            order(vec![hit(1, 5 * 365, 0.05), hit(2, 5, 0.4)], 5, 180),
            ["e2", "e1"]
        );
    }

    #[test]
    fn the_order_is_deterministic() {
        let hits = vec![
            hit(7, 50, 0.30),
            hit(5, 50, 0.30),
            hit(6, 10, 0.35),
            hit(8, 400, 0.30),
        ];
        let mut reversed = hits.clone();
        reversed.reverse();
        let expected = ["e6", "e7", "e5", "e8"];
        assert_eq!(order(hits, 5, 180), expected);
        assert_eq!(order(reversed, 5, 180), expected);
        // Without decay, equal scores fall back to the more recent episode, then the higher id.
        assert_eq!(
            order(
                vec![hit(1, 30, 0.3), hit(2, 10, 0.3), hit(3, 10, 0.3)],
                5,
                0
            ),
            ["e3", "e2", "e1"]
        );
    }
}
