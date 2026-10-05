//! One slice to one episode.

use std::sync::Arc;

use qbot_core::{AccountId, GroupId, SlicePlan};
use qbot_llm::{Embedder, LlmError, Usage};

use crate::episode::NewEpisode;
use crate::extract::{EpisodeExtractor, ExtractError, METHOD, SliceContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderConfig {
    /// Language the summaries are written in.
    pub language: String,
}

impl Default for BuilderConfig {
    fn default() -> Self {
        Self {
            language: "English".into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("the slice has no lines")]
    EmptyTarget,
    #[error(transparent)]
    Extract(#[from] ExtractError),
    #[error("embedding failed: {0}")]
    Embed(#[from] LlmError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    pub episode: NewEpisode,
    /// Embedding of the episode's title and summary.
    pub vector: Vec<f32>,
    pub embed_model: String,
    pub usage: Usage,
    pub attempts: u32,
}

pub struct EpisodeBuilder {
    extractor: EpisodeExtractor,
    embedder: Arc<dyn Embedder>,
    cfg: BuilderConfig,
}

impl std::fmt::Debug for EpisodeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpisodeBuilder").finish_non_exhaustive()
    }
}

impl EpisodeBuilder {
    pub fn new(
        extractor: EpisodeExtractor,
        embedder: Arc<dyn Embedder>,
        cfg: BuilderConfig,
    ) -> Self {
        Self {
            extractor,
            embedder,
            cfg,
        }
    }

    /// Summarize the slice in `ctx.target`, which must be exactly the lines of `plan.target`.
    /// The episode owns that range and nothing else; context lines never enter it.
    pub async fn build(
        &self,
        group: GroupId,
        plan: &SlicePlan,
        batch_lines: u32,
        ctx: &SliceContext<'_>,
    ) -> Result<Built, BuildError> {
        let (Some(first), Some(last)) = (ctx.target.first(), ctx.target.last()) else {
            return Err(BuildError::EmptyTarget);
        };
        let extracted = self.extractor.extract(ctx, &self.cfg.language).await?;
        for reason in &extracted.dropped {
            tracing::info!(group = group.get(), %reason, "a proposed finding was not kept");
        }
        let mut participants: Vec<AccountId> =
            ctx.target.iter().filter_map(|l| l.speaker).collect();
        participants.sort();
        participants.dedup();
        let episode = NewEpisode {
            group,
            first_batch: plan.first_batch,
            last_batch: plan.last_batch,
            first_ordinal: *plan.target.start(),
            last_ordinal: *plan.target.end(),
            batch_lines,
            first_message: first.message,
            last_message: last.message,
            started: first.at,
            ended: last.at,
            line_count: u32::try_from(ctx.target.len()).unwrap_or(u32::MAX),
            participants,
            title: extracted.title,
            summary: extracted.summary,
            evidence: extracted.evidence,
            method: METHOD.to_owned(),
            model: extracted.model,
            findings: extracted.findings,
        };
        let embedded = self
            .embedder
            .embed(&[format!("{}\n{}", episode.title, episode.summary)])
            .await?;
        let vector = embedded.vectors.into_iter().next().unwrap_or_default();
        Ok(Built {
            episode,
            vector,
            embed_model: self.embedder.info().model.clone(),
            usage: extracted.usage,
            attempts: extracted.attempts,
        })
    }
}
