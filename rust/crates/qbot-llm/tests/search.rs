#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use qbot_llm::LlmError;
use qbot_llm::responses::{HttpBody, HttpResponse, RetryPolicy, Transport};
use qbot_llm::search::{SearchDepth, TavilyConfig, TavilySearch, WebSearch};
use serde_json::{Value, json};

/// Answers each POST with the next scripted `(status, body)` and records what was sent.
struct Scripted {
    replies: Mutex<VecDeque<(u16, Value)>>,
    sent: Mutex<Vec<(String, Value)>>,
}

impl Scripted {
    fn new(replies: impl IntoIterator<Item = (u16, Value)>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().collect()),
            sent: Mutex::default(),
        })
    }
}

#[async_trait]
impl Transport for Scripted {
    async fn post(&self, path: &str, body: &Value, _: bool) -> Result<HttpResponse, LlmError> {
        self.sent
            .lock()
            .unwrap()
            .push((path.to_owned(), body.clone()));
        let (status, body) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted reply");
        Ok(HttpResponse {
            status,
            retry_after: None,
            body: HttpBody::Full(serde_json::to_vec(&body).unwrap().into()),
        })
    }
}

fn tavily(transport: Arc<Scripted>, max_results: u32) -> TavilySearch {
    TavilySearch::new(
        TavilyConfig {
            max_results,
            depth: SearchDepth::Basic,
            timeout: Duration::from_secs(5),
            retry: RetryPolicy {
                retries: 2,
                base: Duration::from_millis(1),
                jitter: false,
            },
            extract: qbot_llm::search::ExtractConfig {
                depth: qbot_llm::search::SearchDepth::Basic,
                chunks_per_source: 3,
            },
        },
        transport,
    )
}

fn result(n: u32) -> Value {
    json!({
        "title": format!("Result  {n}"),
        "url": format!("https://example.org/{n}"),
        "content": format!("line one\n\n  line   two of {n}"),
        "score": 0.9,
    })
}

#[tokio::test]
async fn a_search_sends_the_configured_request_and_normalizes_results() {
    let transport = Scripted::new([(
        200,
        json!({
            "query": "q",
            "results": [
                result(1),
                {"title": "no link", "url": "", "content": "dropped"},
                {"title": "dated", "url": "https://example.org/d", "content": "x", "published_date": "2026-10-01"},
                result(3),
            ],
            "usage": {"credits": 1},
            "response_time": 0.8
        }),
    )]);
    let search = tavily(transport.clone(), 2);
    let out = search.search("rust async runtimes").await.unwrap();

    let (path, body) = transport.sent.lock().unwrap()[0].clone();
    assert_eq!(path, "/search");
    assert_eq!(
        body,
        json!({
            "query": "rust async runtimes",
            "search_depth": "basic",
            "max_results": 2,
            "include_answer": false,
            "include_raw_content": false,
            "include_usage": true,
        })
    );
    assert_eq!(
        out.hits.len(),
        2,
        "never more than asked for; results without a link are dropped"
    );
    assert_eq!(out.hits[0].title, "Result 1");
    assert_eq!(out.hits[0].content, "line one line two of 1");
    assert_eq!(out.hits[0].published, None);
    assert_eq!(out.hits[1].published.as_deref(), Some("2026-10-01"));
    assert_eq!((out.credits, out.requests), (Some(1), 1));
}

#[tokio::test]
async fn provider_failures_are_typed_and_only_transient_ones_retried() {
    // Overloaded, then fine: retried.
    let transport = Scripted::new([
        (503, json!({"detail": {"error": "busy"}})),
        (200, json!({"results": [result(1)]})),
    ]);
    let out = tavily(transport, 5).search("q").await.unwrap();
    assert_eq!((out.requests, out.credits), (2, None));

    // A bad key, a used-up plan and a pay-as-you-go cap are the account's state: no retry.
    for (status, expected) in [
        (401, "auth"),
        (432, "quota_exhausted"),
        (433, "quota_exhausted"),
        (400, "invalid_request"),
    ] {
        let transport = Scripted::new([(status, json!({"detail": {"error": "nope"}}))]);
        let error = tavily(transport.clone(), 5).search("q").await.unwrap_err();
        assert_eq!(error.class(), expected, "{status}: {error:?}");
        assert_eq!(
            transport.sent.lock().unwrap().len(),
            1,
            "{status} is not retried"
        );
    }

    // A body without a results list is the provider breaking its contract.
    let transport = Scripted::new([(200, json!({"answer": "x"}))]);
    assert!(matches!(
        tavily(transport, 5).search("q").await,
        Err(LlmError::Protocol(_))
    ));
}

#[tokio::test]
async fn reading_a_page_asks_for_markdown_and_passes_the_question_only_when_given() {
    use qbot_llm::search::{PageRead, PageReader};
    let page = json!({
        "results": [{"url": "https://example.org/a", "raw_content": "# Title\n\n- one\n- two\n"}],
        "failed_results": [],
        "usage": {"credits": 1}
    });
    let transport = Scripted::new([(200, page.clone()), (200, page)]);
    let reader = tavily(transport.clone(), 5);

    let whole = reader.read("https://example.org/a", None).await.unwrap();
    assert_eq!(
        whole,
        PageRead::Read {
            content: "# Title\n\n- one\n- two".into(),
            credits: Some(1)
        },
        "Markdown structure is kept, only the ends are trimmed"
    );
    let asked = reader
        .read("https://example.org/a", Some("what is listed?"))
        .await
        .unwrap();
    assert!(matches!(asked, PageRead::Read { .. }));

    let sent = transport.sent.lock().unwrap().clone();
    assert_eq!(sent[0].0, "/extract");
    assert_eq!(
        sent[0].1,
        json!({
            "urls": ["https://example.org/a"],
            "extract_depth": "basic",
            "format": "markdown",
            "include_images": false,
            "include_usage": true,
        })
    );
    assert_eq!(sent[1].1["query"], "what is listed?");
    assert_eq!(sent[1].1["chunks_per_source"], 3);
}

#[tokio::test]
async fn a_page_that_cannot_be_read_is_an_outcome_not_an_error() {
    use qbot_llm::search::{PageRead, PageReader};
    let transport = Scripted::new([
        (
            200,
            json!({"results": [], "failed_results": [{"url": "https://x.invalid/", "error": "Failed to fetch url"}]}),
        ),
        (200, json!({"results": [], "failed_results": []})),
        (432, json!({"detail": {"error": "plan limit"}})),
    ]);
    let reader = tavily(transport, 5);
    assert_eq!(
        reader.read("https://x.invalid/", None).await.unwrap(),
        PageRead::Failed {
            reason: "Failed to fetch url".into()
        }
    );
    assert!(matches!(
        reader.read("https://x.invalid/", None).await.unwrap(),
        PageRead::Failed { .. }
    ));
    assert_eq!(
        reader
            .read("https://x.invalid/", None)
            .await
            .unwrap_err()
            .class(),
        "quota_exhausted",
        "a provider failure is still an error"
    );
}
