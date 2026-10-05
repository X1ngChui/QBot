//! Tavily (`POST /search`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{PageRead, PageReader, SearchHit, SearchResults, WebSearch};
use crate::capability::ProviderId;
use crate::error::LlmError;
use crate::responses::{HttpBody, RetryPolicy, Transport, post_with_retry};

/// Tavily's search depth: `advanced` costs two credits and returns better excerpts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDepth {
    Basic,
    Advanced,
}

impl SearchDepth {
    fn as_str(self) -> &'static str {
        match self {
            SearchDepth::Basic => "basic",
            SearchDepth::Advanced => "advanced",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TavilyConfig {
    /// Results asked for per search (Tavily accepts 0 to 20).
    pub max_results: u32,
    pub depth: SearchDepth,
    /// Deadline for one search or page read, retries included.
    pub timeout: Duration,
    pub retry: RetryPolicy,
    pub extract: ExtractConfig,
}

impl TavilyConfig {
    /// Five results a search and three passages a page: enough to answer from, little enough
    /// to read. One search or page read is given 20 seconds, retries included.
    pub fn new(depth: SearchDepth, extract_depth: SearchDepth) -> Self {
        Self {
            max_results: 5,
            depth,
            timeout: Duration::from_secs(20),
            retry: RetryPolicy::default(),
            extract: ExtractConfig {
                depth: extract_depth,
                chunks_per_source: 3,
            },
        }
    }
}

/// Page reading (`POST /extract`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractConfig {
    /// `advanced` handles harder pages (tables, embedded content) for twice the credits.
    pub depth: SearchDepth,
    /// Passages returned when a question is given (Tavily accepts 1 to 5).
    pub chunks_per_source: u32,
}

pub struct TavilySearch {
    id: ProviderId,
    cfg: TavilyConfig,
    transport: Arc<dyn Transport>,
}

impl std::fmt::Debug for TavilySearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TavilySearch")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

/// Tavily's API.
pub const ENDPOINT: &str = "https://api.tavily.com";

impl TavilySearch {
    pub fn new(cfg: TavilyConfig, transport: Arc<dyn Transport>) -> Self {
        Self {
            id: ProviderId::new("tavily"),
            cfg,
            transport,
        }
    }

    async fn post(&self, query: &str) -> Result<SearchResults, LlmError> {
        let body = json!({
            "query": query,
            "search_depth": self.cfg.depth.as_str(),
            "max_results": self.cfg.max_results,
            "include_answer": false,
            "include_raw_content": false,
            "include_usage": true,
        });
        let (response, requests) =
            post_with_retry(&*self.transport, &self.cfg.retry, "/search", &body, false)
                .await
                .map_err(plan_limit)?;
        let HttpBody::Full(bytes) = response.body else {
            return Err(LlmError::Protocol("expected a full body".into()));
        };
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmError::Protocol(format!("search response is not JSON: {e}")))?;
        let results = value
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| LlmError::Protocol("search response has no results list".into()))?;
        let text = |item: &Value, key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default()
        };
        let hits = results
            .iter()
            .map(|item| SearchHit {
                title: text(item, "title"),
                url: text(item, "url"),
                content: text(item, "content"),
                published: Some(text(item, "published_date")).filter(|d| !d.is_empty()),
            })
            .filter(|hit| !hit.url.is_empty())
            .take(self.cfg.max_results as usize)
            .collect();
        let credits = value
            .get("usage")
            .and_then(|u| u.get("credits"))
            .and_then(Value::as_u64)
            .and_then(|c| u32::try_from(c).ok());
        Ok(SearchResults {
            hits,
            credits,
            requests,
        })
    }
}

impl TavilySearch {
    async fn extract(&self, url: &str, question: Option<&str>) -> Result<PageRead, LlmError> {
        let mut body = json!({
            "urls": [url],
            "extract_depth": self.cfg.extract.depth.as_str(),
            "format": "markdown",
            "include_images": false,
            "include_usage": true,
        });
        if let Some(question) = question {
            body["query"] = json!(question);
            body["chunks_per_source"] = json!(self.cfg.extract.chunks_per_source);
        }
        let (response, _) =
            post_with_retry(&*self.transport, &self.cfg.retry, "/extract", &body, false)
                .await
                .map_err(plan_limit)?;
        let HttpBody::Full(bytes) = response.body else {
            return Err(LlmError::Protocol("expected a full body".into()));
        };
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmError::Protocol(format!("extract response is not JSON: {e}")))?;
        let credits = value
            .get("usage")
            .and_then(|u| u.get("credits"))
            .and_then(Value::as_u64)
            .and_then(|c| u32::try_from(c).ok());
        // One URL was asked for, so at most one result; order is not guaranteed in general.
        if let Some(read) = value
            .get("results")
            .and_then(Value::as_array)
            .and_then(|r| r.first())
        {
            let content = read
                .get("raw_content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            return Ok(PageRead::Read { content, credits });
        }
        let reason = value
            .get("failed_results")
            .and_then(Value::as_array)
            .and_then(|f| f.first())
            .and_then(|f| f.get("error"))
            .and_then(Value::as_str)
            .filter(|e| !e.trim().is_empty())
            .unwrap_or("the page could not be extracted")
            .trim()
            .to_owned();
        Ok(PageRead::Failed { reason })
    }
}

#[async_trait]
impl PageReader for TavilySearch {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    async fn read(&self, url: &str, question: Option<&str>) -> Result<PageRead, LlmError> {
        tokio::time::timeout(self.cfg.timeout, self.extract(url, question))
            .await
            .map_err(|_| LlmError::Timeout)?
    }
}

/// Tavily refuses with 432 when the plan's credits are used up and 433 when the pay-as-you-go
/// limit is reached: limits of the account, not transient failures.
fn plan_limit(error: LlmError) -> LlmError {
    match error {
        LlmError::Other {
            status: 432 | 433,
            message,
        } => LlmError::QuotaExhausted(message),
        other => other,
    }
}

#[async_trait]
impl WebSearch for TavilySearch {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    async fn search(&self, query: &str) -> Result<SearchResults, LlmError> {
        tokio::time::timeout(self.cfg.timeout, self.post(query))
            .await
            .map_err(|_| LlmError::Timeout)?
    }
}
