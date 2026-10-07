//! The background job that turns archived chat into episodes.

use std::sync::Arc;

use async_trait::async_trait;
use qbot_core::{GroupId, SliceGrid};
use qbot_sched::{JobError, JobKind, JobRunner};

use crate::build::EpisodeBuilder;
use crate::consolidate::Consolidator;
use crate::extract::SliceContext;
use crate::store::EpisodeStore;

/// Runs `JobKind::Extract` for a group: extracts every complete slice that has no episode yet,
/// oldest first. Idempotent: an extracted slice is covered and is never extracted again, and a
/// crash between slices loses nothing. A slice that can never be extracted is recorded as
/// skipped, so it does not hold back the group's later slices.
pub struct EpisodeJobs {
    store: Arc<dyn EpisodeStore>,
    builder: EpisodeBuilder,
    slices: SliceGrid,
    consolidator: Option<Arc<Consolidator>>,
    /// One extraction at a time: a filled batch and the nightly run may both ask for a group,
    /// and the second must find the first's episodes rather than write them again.
    running: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for EpisodeJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpisodeJobs")
            .field("slices", &self.slices)
            .finish_non_exhaustive()
    }
}

fn failed(error: impl std::fmt::Display) -> JobError {
    JobError(error.to_string())
}

impl EpisodeJobs {
    pub fn new(store: Arc<dyn EpisodeStore>, builder: EpisodeBuilder, slices: SliceGrid) -> Self {
        Self {
            store,
            builder,
            slices,
            consolidator: None,
            running: tokio::sync::Mutex::new(()),
        }
    }

    /// Apply each new episode's findings to identity and facts as it is stored.
    pub fn with_consolidator(mut self, consolidator: Arc<Consolidator>) -> Self {
        self.consolidator = Some(consolidator);
        self
    }

    /// Apply the findings of every stored episode not applied yet (after a crash between storing
    /// and applying, say).
    async fn consolidate_pending(&self, group: GroupId) -> Result<(), JobError> {
        let Some(consolidator) = &self.consolidator else {
            return Ok(());
        };
        for episode in self.store.unconsolidated(group).await.map_err(failed)? {
            consolidator.apply(&episode).await.map_err(failed)?;
            self.store
                .mark_consolidated(episode.id)
                .await
                .map_err(failed)?;
        }
        Ok(())
    }

    /// Embed the group's episodes that the current embedding index lacks: after the embedding
    /// model or width changes, recall finds an episode again once this has run. Only the
    /// embedding is redone; the episode itself (summary, evidence, findings) stays as it is.
    async fn index_group(&self, group: GroupId) -> Result<usize, JobError> {
        // Episodes embedded per round; the embedder splits them into requests it accepts.
        const ROUND: usize = 64;
        let (model, dims) = self.builder.index();
        let (model, mut indexed) = (model.to_owned(), 0);
        loop {
            let missing = self
                .store
                .unembedded(group, &model, dims, ROUND)
                .await
                .map_err(failed)?;
            if missing.is_empty() {
                break;
            }
            let vectors = self.builder.embed(&missing).await.map_err(failed)?;
            if vectors.len() != missing.len() {
                return Err(JobError(format!(
                    "{} episodes embedded as {} vectors",
                    missing.len(),
                    vectors.len()
                )));
            }
            for (episode, vector) in missing.iter().zip(&vectors) {
                self.store
                    .set_embedding(episode.id, &model, vector)
                    .await
                    .map_err(failed)?;
            }
            indexed += missing.len();
        }
        if indexed > 0 {
            tracing::info!(
                group = group.get(),
                indexed,
                model,
                "episodes embedded for recall"
            );
        }
        Ok(indexed)
    }

    /// Extract all complete slices of `group`; returns how many episodes were stored. Episodes
    /// the current embedding index lacks are embedded first.
    pub async fn extract_group(&self, group: GroupId) -> Result<usize, JobError> {
        let _one_at_a_time = self.running.lock().await;
        self.index_group(group).await?;
        self.consolidate_pending(group).await?;
        let mut stored = 0;
        loop {
            let covered = self.store.covered_through(group).await.map_err(failed)?;
            let last = self.store.last_ordinal(group).await.map_err(failed)?;
            let Some(plan) = self.slices.next(covered, last) else {
                return Ok(stored);
            };

            let target = self
                .store
                .lines(group, *plan.target.start(), *plan.target.end())
                .await
                .map_err(failed)?;
            let expected = (plan.target.end() - plan.target.start() + 1) as usize;
            if target.len() != expected {
                return Err(JobError(format!(
                    "slice {:?} has {} of {expected} lines",
                    plan.target,
                    target.len()
                )));
            }
            let previous = match &plan.previous {
                Some(range) => self
                    .store
                    .lines(group, *range.start(), *range.end())
                    .await
                    .map_err(failed)?,
                None => Vec::new(),
            };
            let next = match &plan.next {
                Some(range) => self
                    .store
                    .lines(group, *range.start(), *range.end())
                    .await
                    .map_err(failed)?,
                None => Vec::new(),
            };

            let ctx = SliceContext {
                previous: &previous,
                target: &target,
                next: &next,
            };
            let built = match self
                .builder
                .build(group, &plan, self.slices.grid.lines_per_batch, &ctx)
                .await
            {
                Ok(built) => built,
                Err(error) if error.is_permanent() => {
                    tracing::error!(
                        group = group.get(),
                        lines = ?plan.target,
                        %error,
                        "slice skipped: it can never be extracted; its lines stay verbatim"
                    );
                    self.store
                        .skip_slice(
                            group,
                            *plan.target.start(),
                            *plan.target.end(),
                            &error.to_string(),
                        )
                        .await
                        .map_err(failed)?;
                    continue;
                }
                Err(error) => return Err(failed(error)),
            };
            self.store
                .insert(&built.episode, &built.vector, &built.embed_model)
                .await
                .map_err(failed)?;
            tracing::info!(
                group = group.get(),
                lines = ?plan.target,
                attempts = built.attempts,
                input_tokens = built.usage.input_tokens,
                output_tokens = built.usage.output_tokens,
                "episode stored"
            );
            self.consolidate_pending(group).await?;
            stored += 1;
        }
    }
}

#[async_trait]
impl JobRunner for EpisodeJobs {
    async fn run(&self, kind: JobKind, group: Option<GroupId>) -> Result<(), JobError> {
        match (kind, group) {
            (JobKind::Extract, Some(group)) => self.extract_group(group).await.map(|_| ()),
            (JobKind::Extract, None) => Err(JobError("an extract job needs a group".into())),
            (other, _) => Err(JobError(format!("{other:?} is not an episode job"))),
        }
    }
}
