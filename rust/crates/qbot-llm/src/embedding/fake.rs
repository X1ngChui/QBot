use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;

use super::{Embedder, EmbedderInfo, Embeddings, normalize};
use crate::capability::ProviderId;
use crate::error::LlmError;

/// A deterministic bag-of-tokens embedding: texts that share words are close, unrelated texts
/// are far. Good enough to test retrieval and segmentation mechanics offline; it is not a
/// language model and says nothing about real embedding quality.
pub fn hash_embed(text: &str, dims: usize) -> Vec<f32> {
    let mut vector = vec![0.0f32; dims.max(1)];
    let lowered = text.to_lowercase();
    let mut word = String::new();
    let mut add = |token: &str| {
        let hash = fnv1a(token.as_bytes());
        let index = (hash % vector.len() as u64) as usize;
        vector[index] += if (hash >> 63) == 0 { 1.0 } else { -1.0 };
    };
    for c in lowered.chars() {
        if c.is_ascii_alphanumeric() {
            word.push(c);
            continue;
        }
        if !word.is_empty() {
            add(&word);
            word.clear();
        }
        // Every other non-space character is its own token, so unsegmented scripts still work.
        if !c.is_whitespace() && !c.is_ascii() {
            add(&c.to_string());
        }
    }
    if !word.is_empty() {
        add(&word);
    }
    if normalize(&mut vector).is_err() {
        // No tokens at all: a fixed unit vector keeps empty texts valid.
        vector.iter_mut().for_each(|v| *v = 0.0);
        vector[0] = 1.0;
    }
    vector
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[derive(Default)]
struct Inner {
    calls: Vec<Vec<String>>,
    fail_next: Option<LlmError>,
}

pub struct FakeEmbedder {
    info: EmbedderInfo,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for FakeEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeEmbedder")
            .field("dims", &self.info.dims)
            .finish_non_exhaustive()
    }
}

impl FakeEmbedder {
    pub fn new(dims: usize) -> Self {
        Self {
            info: EmbedderInfo {
                id: ProviderId::new("fake-embedder"),
                model: "fake-embed".into(),
                dims,
            },
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The same embedder reporting another model name: vectors from it are a different index.
    pub fn named(mut self, model: &str) -> Self {
        self.info.model = model.into();
        self
    }

    /// Every `embed` call so far, with its texts.
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.lock().calls.clone()
    }

    pub fn fail_next(&self, error: LlmError) {
        self.lock().fail_next = Some(error);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl Embedder for FakeEmbedder {
    fn info(&self) -> &EmbedderInfo {
        &self.info
    }

    async fn embed(&self, texts: &[String]) -> Result<Embeddings, LlmError> {
        let mut inner = self.lock();
        inner.calls.push(texts.to_vec());
        if let Some(error) = inner.fail_next.take() {
            return Err(error);
        }
        Ok(Embeddings {
            vectors: texts
                .iter()
                .map(|t| hash_embed(t, self.info.dims))
                .collect(),
            input_tokens: Some(texts.iter().map(|t| t.len() as u64 / 4 + 1).sum()),
            requests: 1,
        })
    }
}
