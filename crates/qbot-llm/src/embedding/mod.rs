//! Text embeddings: the second model capability next to chat.
//!
//! The contract mirrors [`Provider`](crate::Provider): callers see one normalized interface and
//! the adapter absorbs vendor limits. `embed` accepts any number of texts and returns exactly one
//! unit-length vector per text, in order; splitting into the vendor's request size is the
//! adapter's business.

mod fake;
mod http;
pub mod sim;

use async_trait::async_trait;

use crate::capability::ProviderId;
use crate::error::LlmError;

pub use fake::{FakeEmbedder, hash_embed};
pub use http::{EmbeddingConfig, HttpEmbedder};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedderInfo {
    pub id: ProviderId,
    pub model: String,
    /// Width of every returned vector.
    pub dims: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Embeddings {
    /// One unit-length vector per input text, in input order.
    pub vectors: Vec<Vec<f32>>,
    /// Input tokens the provider reported, when it reports them.
    pub input_tokens: Option<u64>,
    /// HTTP requests it took (batches, plus retries).
    pub requests: u32,
}

#[async_trait]
pub trait Embedder: Send + Sync {
    fn info(&self) -> &EmbedderInfo;

    async fn embed(&self, texts: &[String]) -> Result<Embeddings, LlmError>;
}

/// Scale to unit length. A zero or non-finite vector is a provider fault, not something to guess.
pub(crate) fn normalize(vector: &mut [f32]) -> Result<(), LlmError> {
    if vector.iter().any(|v| !v.is_finite()) {
        return Err(LlmError::Protocol(
            "embedding contains a non-finite value".into(),
        ));
    }
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm == 0.0 {
        return Err(LlmError::Protocol("embedding is the zero vector".into()));
    }
    for v in vector {
        *v /= norm;
    }
    Ok(())
}

/// Cosine distance between two unit vectors, in `0.0..=2.0`.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    1.0 - a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
}
