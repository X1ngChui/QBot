//! OpenAI-compatible `/embeddings` adapter (DashScope's compatible mode among others).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Embedder, EmbedderInfo, Embeddings, normalize};
use crate::capability::ProviderId;
use crate::error::LlmError;
use crate::meter::{self, CallKind, Meter};
use crate::response::{CacheUsage, Usage};
use crate::responses::{RetryPolicy, Transport, post_with_retry};

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
    /// An OpenAI-compatible embeddings endpoint. One request (a batch) is given 30 seconds,
    /// retries included.
    pub fn new(model: impl Into<String>, dims: usize, max_batch: usize) -> Self {
        Self {
            id: ProviderId::new("embedding"),
            model: model.into(),
            dims,
            max_batch,
            timeout: std::time::Duration::from_secs(30),
            retry: RetryPolicy::default(),
        }
    }

    /// Alibaba DashScope `text-embedding-v4`: 10 inputs per request; 2048 dimensions measured
    /// to separate related from unrelated Chinese text better than 1024.
    pub fn dashscope_v4(dims: usize) -> Self {
        Self {
            id: ProviderId::new("dashscope"),
            ..Self::new("text-embedding-v4", dims, 10)
        }
    }
}

pub struct HttpEmbedder {
    info: EmbedderInfo,
    cfg: EmbeddingConfig,
    transport: Arc<dyn Transport>,
    meter: Option<Arc<dyn Meter>>,
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
            meter: None,
        }
    }

    /// Record every call on `meter`.
    pub fn with_meter(mut self, meter: Arc<dyn Meter>) -> Self {
        self.meter = Some(meter);
        self
    }

    async fn batch(&self, chunk: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
        let body = json!({ "model": self.cfg.model, "input": chunk, "dimensions": self.cfg.dims });
        let response =
            post_with_retry(&*self.transport, &self.cfg.retry, "/embeddings", &body).await?;
        let value: Value = serde_json::from_slice(&response.body)
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
        if let Some(tokens) = value
            .get("usage")
            .and_then(|u| u.get("total_tokens").or_else(|| u.get("prompt_tokens")))
            .and_then(Value::as_u64)
        {
            meter::add_tokens(Usage {
                input_tokens: u32::try_from(tokens).unwrap_or(u32::MAX),
                output_tokens: 0,
                cache: CacheUsage::NotReported,
                reasoning_tokens: None,
                reported: true,
            });
        }
        Ok(vectors)
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    fn info(&self) -> &EmbedderInfo {
        &self.info
    }

    async fn embed(&self, texts: &[String]) -> Result<Embeddings, LlmError> {
        let call = async {
            let mut vectors = Vec::with_capacity(texts.len());
            for chunk in texts.chunks(self.cfg.max_batch.max(1)) {
                vectors.extend(self.batch(chunk).await?);
            }
            Ok(Embeddings { vectors })
        };
        meter::measured(
            self.meter.as_ref(),
            CallKind::Embedding,
            self.info.id.as_str(),
            &self.info.model,
            call,
        )
        .await
    }
}
