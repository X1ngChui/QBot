//! A stand-in embeddings endpoint for adapter tests: enforces the per-request batch ceiling the
//! way the real one does (a vague 400), can shuffle result order, and can corrupt answers.

use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{Value, json};

use super::hash_embed;
use crate::error::LlmError;
use crate::responses::{HttpBody, HttpResponse, Transport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corruption {
    None,
    WrongCount,
    RepeatedIndex,
    WrongWidth,
    NotANumber,
    ZeroVector,
}

pub struct SimEmbeddingServer {
    max_batch: usize,
    dims: usize,
    inner: Mutex<Inner>,
}

struct Inner {
    requests: Vec<Value>,
    shuffle: bool,
    corruption: Corruption,
    fail_with: Vec<(u16, Option<std::time::Duration>)>,
}

impl std::fmt::Debug for SimEmbeddingServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimEmbeddingServer").finish_non_exhaustive()
    }
}

impl SimEmbeddingServer {
    pub fn new(max_batch: usize, dims: usize) -> Self {
        Self {
            max_batch,
            dims,
            inner: Mutex::new(Inner {
                requests: Vec::new(),
                shuffle: false,
                corruption: Corruption::None,
                fail_with: Vec::new(),
            }),
        }
    }

    pub fn shuffled(self) -> Self {
        self.lock().shuffle = true;
        self
    }

    pub fn set_corruption(&self, corruption: Corruption) {
        self.lock().corruption = corruption;
    }

    /// Answer the next requests with these HTTP statuses (and Retry-After) before succeeding.
    pub fn fail_first(&self, failures: Vec<(u16, Option<std::time::Duration>)>) {
        self.lock().fail_with = failures;
    }

    pub fn requests(&self) -> Vec<Value> {
        self.lock().requests.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn error(status: u16, retry_after: Option<std::time::Duration>, message: &str) -> HttpResponse {
    HttpResponse {
        status,
        retry_after,
        body: HttpBody::Full(Bytes::from(
            json!({ "error": { "message": message } }).to_string(),
        )),
    }
}

#[async_trait]
impl Transport for SimEmbeddingServer {
    async fn post(
        &self,
        path: &str,
        body: &Value,
        _want_stream: bool,
    ) -> Result<HttpResponse, LlmError> {
        let mut inner = self.lock();
        inner.requests.push(body.clone());
        if path != "/embeddings" {
            return Ok(error(404, None, "unknown path"));
        }
        if !inner.fail_with.is_empty() {
            let (status, retry_after) = inner.fail_with.remove(0);
            return Ok(error(status, retry_after, "try later"));
        }
        let inputs: Vec<String> = body
            .get("input")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if inputs.is_empty() || inputs.len() > self.max_batch {
            // Deliberately unhelpful, like the real endpoint.
            return Ok(error(400, None, "InvalidParameter"));
        }
        let mut data: Vec<Value> = inputs
            .iter()
            .enumerate()
            .map(
                |(index, text)| json!({ "index": index, "embedding": hash_embed(text, self.dims) }),
            )
            .collect();
        match inner.corruption {
            Corruption::None => {}
            Corruption::WrongCount => {
                data.pop();
            }
            Corruption::RepeatedIndex => {
                if let Some(last) = data.last_mut() {
                    last["index"] = json!(0);
                }
            }
            Corruption::WrongWidth => data[0]["embedding"] = json!([0.5, 0.5]),
            Corruption::NotANumber => data[0]["embedding"][0] = json!("nan"),
            Corruption::ZeroVector => data[0]["embedding"] = json!(vec![0.0; self.dims]),
        }
        if inner.shuffle {
            data.reverse();
        }
        let tokens: u64 = inputs.iter().map(|t| t.len() as u64 / 4 + 1).sum();
        let answer =
            json!({ "data": data, "usage": { "prompt_tokens": tokens, "total_tokens": tokens } });
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: HttpBody::Full(Bytes::from(answer.to_string())),
        })
    }
}
