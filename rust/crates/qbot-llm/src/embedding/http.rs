//! OpenAI-compatible `/embeddings` adapter (DashScope's compatible mode among others).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Embedder, EmbedderInfo, Embeddings, normalize};
use crate::capability::ProviderId;
use crate::error::LlmError;
use crate::responses::{HttpBody, RetryPolicy, Transport, post_with_retry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingConfig {
    pub id: ProviderId,
    pub model: String,
    pub dims: usize,
    /// Most inputs the endpoint accepts per request. Exceeding it is a vague 400, so the
    /// adapter splits on its own and no caller needs to know.
    pub max_batch: usize,
    /// Deadline for one request (a batch), retries included.
    pub timeout: std::time::Duration,
    pub retry: RetryPolicy,
}

impl EmbeddingConfig {
    /// Alibaba DashScope `text-embedding-v4`: 10 inputs per request; 2048 dimensions measured
    /// to separate related from unrelated Chinese text better than 1024.
    pub fn dashscope_v4(dims: usize) -> Self {
        Self {
            id: ProviderId::new("dashscope"),
            model: "text-embedding-v4".into(),
            dims,
            max_batch: 10,
            timeout: std::time::Duration::from_secs(30),
            retry: RetryPolicy::default(),
        }
    }
}

pub struct HttpEmbedder {
    info: EmbedderInfo,
    cfg: EmbeddingConfig,
    transport: Arc<dyn Transport>,
}

impl std::fmt::Debug for HttpEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpEmbedder")
            .field("model", &self.cfg.model)
            .finish_non_exhaustive()
    }
}

impl HttpEmbedder {
    pub fn new(cfg: EmbeddingConfig, transport: Arc<dyn Transport>) -> Self {
        let info = EmbedderInfo {
            id: cfg.id.clone(),
            model: cfg.model.clone(),
            dims: cfg.dims,
        };
        Self {
            info,
            cfg,
            transport,
        }
    }

    async fn batch(&self, chunk: &[String]) -> Result<(Vec<Vec<f32>>, Option<u64>, u32), LlmError> {
        let body = json!({ "model": self.cfg.model, "input": chunk, "dimensions": self.cfg.dims });
        let (response, requests) = post_with_retry(
            &*self.transport,
            &self.cfg.retry,
            "/embeddings",
            &body,
            false,
        )
        .await?;
        let HttpBody::Full(bytes) = response.body else {
            return Err(LlmError::Protocol("expected a full body".into()));
        };
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmError::Protocol(format!("response is not JSON: {e}")))?;
        let data = value
            .get("data")
            .and_then(Value::as_array)
            .filter(|d| d.len() == chunk.len())
            .ok_or_else(|| {
                LlmError::Protocol("embedding response has the wrong number of vectors".into())
            })?;
        let mut slots: Vec<Option<Vec<f32>>> = vec![None; chunk.len()];
        for item in data {
            let index = item
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
                .filter(|i| *i < chunk.len())
                .ok_or_else(|| {
                    LlmError::Protocol("embedding response index is out of range".into())
                })?;
            let values = item
                .get("embedding")
                .and_then(Value::as_array)
                .filter(|v| v.len() == self.cfg.dims)
                .ok_or_else(|| {
                    LlmError::Protocol("embedding response violates the vector width".into())
                })?;
            let mut vector = values
                .iter()
                .map(|v| v.as_f64().map(|f| f as f32))
                .collect::<Option<Vec<f32>>>()
                .ok_or_else(|| LlmError::Protocol("embedding contains a non-number".into()))?;
            normalize(&mut vector)?;
            if slots[index].replace(vector).is_some() {
                return Err(LlmError::Protocol(
                    "embedding response repeats an index".into(),
                ));
            }
        }
        let vectors = slots
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                LlmError::Protocol("embedding response does not cover every input".into())
            })?;
        let tokens = value
            .get("usage")
            .and_then(|u| u.get("total_tokens").or_else(|| u.get("prompt_tokens")))
            .and_then(Value::as_u64);
        Ok((vectors, tokens, requests))
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    fn info(&self) -> &EmbedderInfo {
        &self.info
    }

    async fn embed(&self, texts: &[String]) -> Result<Embeddings, LlmError> {
        let mut vectors = Vec::with_capacity(texts.len());
        let mut tokens: Option<u64> = None;
        let mut requests = 0;
        for chunk in texts.chunks(self.cfg.max_batch.max(1)) {
            let (batch, used, sent) = self.batch(chunk).await?;
            vectors.extend(batch);
            tokens = match (tokens, used) {
                (Some(a), Some(b)) => Some(a + b),
                (None, Some(b)) if vectors.len() == chunk.len() => Some(b),
                _ => None,
            };
            requests += sent;
        }
        Ok(Embeddings {
            vectors,
            input_tokens: tokens,
            requests,
        })
    }
}
