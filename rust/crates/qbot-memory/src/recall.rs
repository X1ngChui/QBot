//! Finding episodes by meaning.

use std::sync::Arc;

use qbot_core::GroupId;
use qbot_llm::{Embedder, LlmError};

use crate::episode::Hit;
use crate::store::{EpisodeStore, MemoryError};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecallParams {
    pub limit: usize,
    /// Episodes farther than this (cosine distance) are not relevant enough to return. A
    /// quality choice, tuned per embedding model.
    pub max_distance: f32,
}

impl Default for RecallParams {
    fn default() -> Self {
        Self {
            limit: 5,
            max_distance: 0.45,
        }
    }
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
        params: RecallParams,
    ) -> Self {
        Self {
            store,
            embedder,
            params,
        }
    }

    pub async fn recall(&self, group: GroupId, question: &str) -> Result<Vec<Hit>, RecallError> {
        let embedded = self.embedder.embed(&[question.to_owned()]).await?;
        let vector = embedded.vectors.into_iter().next().unwrap_or_default();
        Ok(self
            .store
            .search(
                group,
                &self.embedder.info().model,
                &vector,
                self.params.limit,
                self.params.max_distance,
            )
            .await?)
    }
}
