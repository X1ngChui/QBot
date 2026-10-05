//! Web search: a capability next to chat and embeddings.
//!
//! Called directly by the `web_search` tool rather than through a model's built-in search, so
//! results land in the transcript as an ordinary tool result (billed once, at a position the run
//! controls, inside the cached prefix of later turns). Callers see one normalized result shape;
//! vendors live behind [`WebSearch`].

mod fake;
mod tavily;

use async_trait::async_trait;

use crate::capability::ProviderId;
use crate::error::LlmError;

pub use fake::FakeSearch;
pub use tavily::{ExtractConfig, SearchDepth, TavilyConfig, TavilySearch};

/// One result: where it is, what it is called, and the provider's extract of the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    /// Whitespace-collapsed excerpt.
    pub content: String,
    /// Publication date, when the provider knows it.
    pub published: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResults {
    /// Most relevant first, at most the configured number.
    pub hits: Vec<SearchHit>,
    /// Plan credits the provider charged, when it reports them.
    pub credits: Option<u32>,
    /// HTTP requests it took, retries included.
    pub requests: u32,
}

#[async_trait]
pub trait WebSearch: Send + Sync {
    fn id(&self) -> &ProviderId;

    async fn search(&self, query: &str) -> Result<SearchResults, LlmError>;
}

/// What reading one page gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageRead {
    /// The page's text as Markdown: the whole page, or with a question only the parts most
    /// relevant to it. May be empty when the page has no readable text.
    Read {
        content: String,
        /// Plan credits the provider charged, when it reports them.
        credits: Option<u32>,
    },
    /// The provider answered but could not read this page (unreachable, blocked, not a
    /// document it understands). An ordinary outcome, not a provider failure.
    Failed { reason: String },
}

/// Reads web pages for the model. Errors are failures of the provider itself (key, plan,
/// network); a page that cannot be read is [`PageRead::Failed`].
#[async_trait]
pub trait PageReader: Send + Sync {
    fn id(&self) -> &ProviderId;

    /// Read `url`. With `question`, the provider returns the passages most relevant to it.
    async fn read(&self, url: &str, question: Option<&str>) -> Result<PageRead, LlmError>;
}
