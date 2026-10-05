//! A scripted [`WebSearch`] for tests: answers in order, records the queries.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;

use super::{PageRead, PageReader, SearchResults, WebSearch};
use crate::capability::ProviderId;
use crate::error::LlmError;

pub struct FakeSearch {
    id: ProviderId,
    script: Mutex<VecDeque<Result<SearchResults, LlmError>>>,
    queries: Mutex<Vec<String>>,
    pages: Mutex<VecDeque<Result<PageRead, LlmError>>>,
    reads: Mutex<Vec<(String, Option<String>)>>,
}

impl std::fmt::Debug for FakeSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeSearch").finish_non_exhaustive()
    }
}

impl FakeSearch {
    pub fn new(script: impl IntoIterator<Item = Result<SearchResults, LlmError>>) -> Self {
        Self {
            id: ProviderId::new("fake-search"),
            script: Mutex::new(script.into_iter().collect()),
            queries: Mutex::default(),
            pages: Mutex::default(),
            reads: Mutex::default(),
        }
    }

    /// Answers for page reads, in order.
    pub fn with_pages(self, pages: impl IntoIterator<Item = Result<PageRead, LlmError>>) -> Self {
        *self.pages.lock().unwrap_or_else(PoisonError::into_inner) = pages.into_iter().collect();
        self
    }

    /// Every page read asked for: the URL and the question, if any.
    pub fn reads(&self) -> Vec<(String, Option<String>)> {
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn queries(&self) -> Vec<String> {
        self.queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl WebSearch for FakeSearch {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    async fn search(&self, query: &str) -> Result<SearchResults, LlmError> {
        self.queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(query.to_owned());
        self.script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| {
                Err(LlmError::Protocol(
                    "the fake search script is exhausted".into(),
                ))
            })
    }
}

#[async_trait]
impl PageReader for FakeSearch {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    async fn read(&self, url: &str, question: Option<&str>) -> Result<PageRead, LlmError> {
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((url.to_owned(), question.map(str::to_owned)));
        self.pages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| {
                Err(LlmError::Protocol(
                    "the fake page script is exhausted".into(),
                ))
            })
    }
}
