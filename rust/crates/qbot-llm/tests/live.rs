//! Live checks against the real providers. Every test is ignored by default: they cost money and
//! need credentials. Run them explicitly:
//!
//! ```text
//! cargo test -p qbot-llm --test live -- --ignored --test-threads 1
//! ```
//!
//! Credentials are read from the deployment's secret files (`deploy/secrets/text_api_key`,
//! `vision_api_key`, `embedding_api_key`, or the directory in `QBOT_LIVE_SECRETS_DIR`) into
//! memory only; nothing here prints them. Endpoints and models default to the production choice
//! and can be changed with `QBOT_LIVE_TEXT_ENDPOINT`, `QBOT_LIVE_TEXT_MODEL`,
//! `QBOT_LIVE_EMBEDDING_ENDPOINT`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use qbot_llm::embedding::{EmbeddingConfig, HttpEmbedder};
use qbot_llm::responses::{KeySource, ReqwestTransport, ResponsesConfig, ResponsesProvider};
use qbot_llm::{
    CacheUsage, Content, ConvItem, Conversation, Embedder, FinishReason, LlmError, LoadedMedia,
    MediaStore, Message, Params, Provider, ReasoningEffort, Request, Response, Role, StreamEvent,
    ToolChoice, ToolOutput, ToolSpec, ToolStatus, collect,
};
use serde_json::json;

fn secrets_dir() -> PathBuf {
    std::env::var_os("QBOT_LIVE_SECRETS_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/secrets"),
        PathBuf::from,
    )
}

/// A secret's value, trimmed. The value is never shown, also not on failure.
fn secret(file: &str) -> String {
    let path = secrets_dir().join(file);
    let value = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read secret file {}: {}", path.display(), e.kind()));
    let value = value.trim().to_owned();
    assert!(!value.is_empty(), "secret file {} is empty", path.display());
    value
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn deepseek_with(key_file: &str, key: Option<String>) -> ResponsesProvider {
    let endpoint = env_or("QBOT_LIVE_TEXT_ENDPOINT", "https://api.deepseek.com");
    let key = key.unwrap_or_else(|| secret(key_file));
    let transport =
        ReqwestTransport::new(endpoint, KeySource::Static(key), Duration::from_secs(10)).unwrap();
    let mut cfg = ResponsesConfig::deepseek(env_or("QBOT_LIVE_TEXT_MODEL", "deepseek-flash"));
    cfg.timeout = Some(Duration::from_secs(120));
    ResponsesProvider::new(cfg, Arc::new(transport)).unwrap()
}

fn deepseek() -> ResponsesProvider {
    deepseek_with("text_api_key", None)
}

fn params(max_output_tokens: u32) -> Params {
    Params {
        max_output_tokens,
        reasoning: ReasoningEffort::Low,
        temperature: None,
    }
}

fn msg(role: Role, text: &str) -> ConvItem {
    ConvItem::Message(Message {
        role,
        content: vec![Content::Text(text.into())],
    })
}

fn request<'a>(conversation: &'a Conversation, tools: &'a [ToolSpec]) -> Request<'a> {
    Request {
        conversation,
        tools,
        tool_choice: ToolChoice::Auto,
        parallel_tool_calls: true,
        params: params(400),
        continuation: None,
        media: None,
    }
}

fn weather_tool() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "get_weather".into(),
        description: "Current weather for a city.".into(),
        schema: json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
            "additionalProperties": false
        }),
    }]
}

fn report(what: &str, response: &Response) {
    eprintln!(
        "[{what}] model={} finish={:?} attempts={} latency={:?} input={} output={} reasoning={:?} cache={:?} reported={} text={:?}",
        response.meta.model,
        response.finish,
        response.meta.attempts,
        response.meta.latency,
        response.usage.input_tokens,
        response.usage.output_tokens,
        response.usage.reasoning_tokens,
        response.usage.cache,
        response.usage.reported,
        response.turn.text(),
    );
}

fn assert_usage(response: &Response) {
    let usage = response.usage;
    assert!(usage.reported, "the provider reports usage");
    assert!(
        usage.input_tokens > 0 && usage.output_tokens > 0,
        "{usage:?}"
    );
    if let CacheUsage::Reported { hit_tokens } = usage.cache {
        assert!(hit_tokens <= usage.input_tokens, "{usage:?}");
    }
    if let Some(reasoning) = usage.reasoning_tokens {
        assert!(reasoning <= usage.output_tokens, "{usage:?}");
    }
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_plain_question_gets_a_text_answer_with_usage() {
    let provider = deepseek();
    let conv = Conversation::new(vec![
        msg(
            Role::System,
            "You answer in one word, lower case, no punctuation.",
        ),
        msg(Role::User, "What is the capital of France?"),
    ]);
    let response = provider.respond(request(&conv, &[])).await.unwrap();
    report("plain", &response);
    assert_eq!(response.finish, FinishReason::Stop);
    assert!(response.turn.text().to_lowercase().contains("paris"));
    assert_eq!(response.calls().count(), 0);
    assert_usage(&response);
    assert!(response.meta.provider_response_id.is_some());
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_tool_call_and_its_result_continue_the_conversation_by_full_replay() {
    let provider = deepseek();
    let tools = weather_tool();
    let conv = Conversation::new(vec![
        msg(
            Role::System,
            "You are a weather assistant. Always use the get_weather tool before answering.",
        ),
        msg(Role::Developer, "Answer in English."),
        msg(Role::User, "What's the weather like in Paris right now?"),
    ]);
    let first = provider.respond(request(&conv, &tools)).await.unwrap();
    report("tool call", &first);
    assert_eq!(first.finish, FinishReason::ToolCalls);
    let calls: Vec<_> = first.calls().cloned().collect();
    assert!(!calls.is_empty());
    assert!(calls.iter().all(|c| c.name == "get_weather"));
    assert!(
        calls[0]
            .arguments
            .to_string()
            .to_lowercase()
            .contains("paris"),
        "{:?}",
        calls[0].arguments
    );

    // The tool's answer goes back with the whole conversation (stateless replay), including the
    // model's own turn and any reasoning it must see again.
    let mut items = conv.items().to_vec();
    items.push(ConvItem::Assistant(first.turn.clone()));
    for call in &calls {
        items.push(ConvItem::ToolResult(ToolOutput {
            call_id: call.id.clone(),
            status: ToolStatus::Ok,
            content: vec![Content::Text(
                "Paris: 17 degrees Celsius, light rain, wind 12 km/h.".into(),
            )],
        }));
    }
    let conv2 = Conversation::new(items);
    let mut req = request(&conv2, &tools);
    req.continuation = Some(&first.continuation);
    let second = provider.respond(req).await.unwrap();
    report("after tool result", &second);
    assert_eq!(second.finish, FinishReason::Stop);
    let text = second.turn.text().to_lowercase();
    assert!(text.contains("17") && text.contains("rain"), "{text}");
    assert_usage(&second);
    assert!(
        second.usage.input_tokens > first.usage.input_tokens,
        "the second request carried the whole conversation"
    );
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_failed_tool_result_is_understood() {
    let provider = deepseek();
    let tools = weather_tool();
    let conv = Conversation::new(vec![
        msg(
            Role::System,
            "Always use the get_weather tool before answering.",
        ),
        msg(Role::User, "Weather in Oslo?"),
    ]);
    let first = provider.respond(request(&conv, &tools)).await.unwrap();
    let mut items = conv.items().to_vec();
    items.push(ConvItem::Assistant(first.turn.clone()));
    for call in first.calls() {
        items.push(ConvItem::ToolResult(ToolOutput {
            call_id: call.id.clone(),
            status: ToolStatus::Error,
            content: vec![Content::Text("the weather service is unavailable".into())],
        }));
    }
    let conv2 = Conversation::new(items);
    let mut req = request(&conv2, &[]);
    req.tool_choice = ToolChoice::None;
    let second = provider.respond(req).await.unwrap();
    report("after failed tool", &second);
    assert_eq!(second.calls().count(), 0, "tool_choice none is honoured");
    let text = second.turn.text().to_lowercase();
    assert!(
        [
            "unavailable",
            "not available",
            "unable",
            "can't",
            "cannot",
            "couldn't",
            "could not",
            "sorry"
        ]
        .iter()
        .any(|w| text.contains(w)),
        "{text}"
    );
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn streaming_and_non_streaming_agree_in_shape() {
    let provider = deepseek();
    let tools = weather_tool();
    let conv = Conversation::new(vec![
        msg(
            Role::System,
            "Always use the get_weather tool before answering. Call it for every city asked about.",
        ),
        msg(Role::User, "Compare the weather in Rome and in Madrid."),
    ]);
    let whole = provider.respond(request(&conv, &tools)).await.unwrap();
    report("non-streaming", &whole);

    let mut stream = provider.stream(request(&conv, &tools)).await.unwrap();
    let (mut started, mut fragments, mut completed) = (Vec::new(), 0, None);
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamEvent::CallStarted { name, .. } => started.push(name),
            StreamEvent::CallArguments { .. } => fragments += 1,
            StreamEvent::Completed(r) => completed = Some(*r),
            StreamEvent::TextDelta(_) | StreamEvent::ReasoningDelta(_) => {}
        }
    }
    let streamed = completed.expect("a completed event");
    report("streaming", &streamed);
    eprintln!("[streaming] calls started={started:?} argument fragments={fragments}");

    for r in [&whole, &streamed] {
        assert_eq!(r.finish, FinishReason::ToolCalls);
        let mut cities: Vec<String> = r
            .calls()
            .map(|c| c.arguments["city"].as_str().unwrap_or("").to_lowercase())
            .collect();
        cities.sort();
        assert!(
            cities.iter().any(|c| c.contains("madrid"))
                && cities.iter().any(|c| c.contains("rome")),
            "{cities:?}"
        );
        assert_usage(r);
    }
    assert_eq!(
        started.len(),
        streamed.calls().count(),
        "every call in the final response was announced while streaming"
    );

    // A plain text answer streams as text deltas that add up to the final text.
    let conv = Conversation::new(vec![msg(
        Role::User,
        "Count from one to ten in English words, separated by spaces.",
    )]);
    let mut stream = provider.stream(request(&conv, &[])).await.unwrap();
    let (mut deltas, mut text, mut completed) = (0, String::new(), None);
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamEvent::TextDelta(d) => {
                deltas += 1;
                text.push_str(&d);
            }
            StreamEvent::Completed(r) => completed = Some(*r),
            _ => {}
        }
    }
    let completed = completed.unwrap();
    eprintln!("[streaming text] deltas={deltas}");
    assert!(deltas > 1, "the answer arrived in pieces");
    assert_eq!(text, completed.turn.text());
    assert!(text.to_lowercase().contains("ten"));

    // `collect` is what the agent uses; it gives the same response.
    let collected = collect(provider.stream(request(&conv, &[])).await.unwrap())
        .await
        .unwrap();
    assert_eq!(collected.finish, FinishReason::Stop);
    assert_usage(&collected);
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_repeated_long_prefix_is_served_from_the_provider_cache() {
    let provider = deepseek();
    // A long, stable prefix as the agent's instructions are; only the question changes.
    let rules: String = (1..=120)
        .map(|n| {
            format!(
                "Rule {n}: the code word for item number {n} is word-{}.\n",
                n * 7
            )
        })
        .collect();
    let ask = |question: &str| {
        Conversation::new(vec![
            msg(Role::System, &format!("Answer in a few words.\n{rules}")),
            msg(Role::User, question),
        ])
    };
    let (a, b) = (
        ask("What is the code word for item 3?"),
        ask("And for item 5?"),
    );
    let first = provider.respond(request(&a, &[])).await.unwrap();
    report("cache 1", &first);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let second = provider.respond(request(&b, &[])).await.unwrap();
    report("cache 2", &second);
    assert_usage(&first);
    assert_usage(&second);
    assert!(second.turn.text().contains("35"), "{}", second.turn.text());
    match second.usage.cache {
        CacheUsage::Reported { hit_tokens } => assert!(
            hit_tokens > 0,
            "a shared prefix of {} tokens was not cached",
            first.usage.input_tokens
        ),
        CacheUsage::NotReported => panic!("the provider reports cache usage; the adapter lost it"),
    }
}

/// A 96x64 picture: left half red, right half blue.
fn two_colour_png() -> Vec<u8> {
    let (w, h) = (96u32, 64u32);
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for _ in 0..h {
        for x in 0..w {
            data.extend_from_slice(if x < w / 2 {
                &[220, 20, 20]
            } else {
                &[20, 40, 220]
            });
        }
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, w, h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&data)
        .unwrap();
    out
}

struct OnePicture {
    loads: AtomicUsize,
}

#[async_trait]
impl MediaStore for OnePicture {
    async fn load(&self, key: &str) -> Result<LoadedMedia, LlmError> {
        assert_eq!(key, "picture-1");
        self.loads.fetch_add(1, Ordering::SeqCst);
        Ok(LoadedMedia {
            mime: "image/png".into(),
            bytes: two_colour_png(),
        })
    }
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn deepseek_sees_an_uploaded_picture_and_reuses_the_upload() {
    let provider = deepseek_with("vision_api_key", None);
    let media = OnePicture {
        loads: AtomicUsize::new(0),
    };
    let ask = |question: &str| {
        Conversation::new(vec![ConvItem::Message(Message {
            role: Role::User,
            content: vec![
                Content::Text(question.into()),
                Content::Image {
                    key: "picture-1".into(),
                },
            ],
        })])
    };
    let conv = ask("Which two colours does this picture show, and which is on the left? Be brief.");
    let mut req = request(&conv, &[]);
    req.media = Some(&media);
    let first = provider.respond(req).await.unwrap();
    report("image", &first);
    let text = first.turn.text().to_lowercase();
    assert!(text.contains("red") && text.contains("blue"), "{text}");
    assert_usage(&first);

    // The same picture again: the uploaded file is referenced, not uploaded a second time.
    let conv = ask("Is the right half of this picture blue? Answer yes or no.");
    let mut req = request(&conv, &[]);
    req.media = Some(&media);
    let second = provider.respond(req).await.unwrap();
    report("image again", &second);
    assert!(second.turn.text().to_lowercase().contains("yes"));
    eprintln!("[image] media loads={}", media.loads.load(Ordering::SeqCst));
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn dashscope_embeddings_have_the_configured_width_and_rank_by_meaning() {
    let endpoint = env_or(
        "QBOT_LIVE_EMBEDDING_ENDPOINT",
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
    );
    let transport = ReqwestTransport::new(
        endpoint,
        KeySource::Static(secret("embedding_api_key")),
        Duration::from_secs(10),
    )
    .unwrap();
    let embedder = HttpEmbedder::new(EmbeddingConfig::dashscope_v4(2048), Arc::new(transport));
    // Twelve inputs: more than one batch of ten.
    let mut texts: Vec<String> = vec![
        "\u{6211}\u{4eec}\u{5468}\u{672b}\u{53bb}\u{722c}\u{5c71}\u{5427}".into(),
        "\u{8fd9}\u{4e2a}\u{5468}\u{516d}\u{4e00}\u{8d77}\u{53bb}\u{767b}\u{5c71}\u{600e}\u{4e48}\u{6837}".into(),
        "\u{663e}\u{5361}\u{9a71}\u{52a8}\u{53c8}\u{5d29}\u{6e83}\u{4e86}".into(),
    ];
    texts.extend((0..9).map(|n| format!("filler sentence number {n}")));
    let out = embedder.embed(&texts).await.unwrap();
    eprintln!(
        "[embedding] vectors={} dims={} input_tokens={:?} requests={}",
        out.vectors.len(),
        out.vectors[0].len(),
        out.input_tokens,
        out.requests
    );
    assert_eq!(out.vectors.len(), texts.len());
    assert!(out.vectors.iter().all(|v| v.len() == 2048));
    assert_eq!(out.requests, 2, "split into batches of ten");
    let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    for v in &out.vectors {
        assert!((cos(v, v) - 1.0).abs() < 1e-3, "unit length");
    }
    let (hike, hike2, gpu) = (&out.vectors[0], &out.vectors[1], &out.vectors[2]);
    eprintln!(
        "[embedding] cos(hike, hike2)={:.3} cos(hike, gpu)={:.3}",
        cos(hike, hike2),
        cos(hike, gpu)
    );
    assert!(cos(hike, hike2) > cos(hike, gpu) + 0.1);
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn provider_errors_are_normalized() {
    // A wrong key is an authentication error and is not retried.
    let wrong = deepseek_with(
        "text_api_key",
        Some("sk-not-a-real-key-0000000000000000".into()),
    );
    let conv = Conversation::new(vec![msg(Role::User, "hi")]);
    let error = wrong.respond(request(&conv, &[])).await.unwrap_err();
    eprintln!("[errors] wrong key -> {error:?}");
    assert_eq!(error, LlmError::Auth);

    // An unknown model is the caller's mistake, not a transient failure.
    let endpoint = env_or("QBOT_LIVE_TEXT_ENDPOINT", "https://api.deepseek.com");
    let transport = ReqwestTransport::new(
        endpoint,
        KeySource::Static(secret("text_api_key")),
        Duration::from_secs(10),
    )
    .unwrap();
    let mut cfg = ResponsesConfig::deepseek("no-such-model-qbot");
    cfg.timeout = Some(Duration::from_secs(60));
    let unknown = ResponsesProvider::new(cfg, Arc::new(transport)).unwrap();
    let error = unknown.respond(request(&conv, &[])).await.unwrap_err();
    eprintln!("[errors] unknown model -> {error:?}");
    assert!(
        matches!(
            error,
            LlmError::InvalidRequest(_) | LlmError::Other { status: 404, .. }
        ),
        "{error:?}"
    );

    // Context-too-long is not probed: deepseek-flash accepted a 720k-token conversation, so
    // reaching its limit would mean sending megabytes. The simulator tests cover the mapping.

    // The caller's deadline ends a call that cannot finish in time.
    let endpoint = env_or("QBOT_LIVE_TEXT_ENDPOINT", "https://api.deepseek.com");
    let transport = ReqwestTransport::new(
        endpoint,
        KeySource::Static(secret("text_api_key")),
        Duration::from_secs(10),
    )
    .unwrap();
    let mut cfg = ResponsesConfig::deepseek(env_or("QBOT_LIVE_TEXT_MODEL", "deepseek-flash"));
    cfg.timeout = Some(Duration::from_millis(50));
    let hurried = ResponsesProvider::new(cfg, Arc::new(transport)).unwrap();
    let conv = Conversation::new(vec![msg(Role::User, "Write a long poem about the sea.")]);
    let error = hurried.respond(request(&conv, &[])).await.unwrap_err();
    eprintln!("[errors] 50 ms deadline -> {error:?}");
    assert_eq!(error, LlmError::Timeout);
}

/// The real transport, except that the first request fails as an overloaded server would.
struct FailFirst {
    inner: ReqwestTransport,
    failed: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl qbot_llm::responses::Transport for FailFirst {
    async fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
        want_stream: bool,
    ) -> Result<qbot_llm::responses::HttpResponse, LlmError> {
        if !self.failed.swap(true, Ordering::SeqCst) {
            return Ok(qbot_llm::responses::HttpResponse {
                status: 503,
                retry_after: Some(Duration::from_millis(200)),
                body: qbot_llm::responses::HttpBody::Full(
                    br#"{"error":{"message":"overloaded"}}"#.to_vec().into(),
                ),
            });
        }
        self.inner.post(path, body, want_stream).await
    }
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_transient_failure_is_retried_and_the_retry_reaches_the_real_provider() {
    let conv = Conversation::new(vec![msg(Role::User, "Say ok.")]);
    for stream in [false, true] {
        let endpoint = env_or("QBOT_LIVE_TEXT_ENDPOINT", "https://api.deepseek.com");
        let inner = ReqwestTransport::new(
            endpoint,
            KeySource::Static(secret("text_api_key")),
            Duration::from_secs(10),
        )
        .unwrap();
        let transport = FailFirst {
            inner,
            failed: std::sync::atomic::AtomicBool::new(false),
        };
        let mut cfg = ResponsesConfig::deepseek(env_or("QBOT_LIVE_TEXT_MODEL", "deepseek-flash"));
        cfg.timeout = Some(Duration::from_secs(120));
        let provider = ResponsesProvider::new(cfg, Arc::new(transport)).unwrap();
        let response = if stream {
            collect(provider.stream(request(&conv, &[])).await.unwrap())
                .await
                .unwrap()
        } else {
            provider.respond(request(&conv, &[])).await.unwrap()
        };
        report(if stream { "retry (stream)" } else { "retry" }, &response);
        assert_eq!(response.finish, FinishReason::Stop);
        assert_eq!(response.meta.attempts, 2, "one failure, one retry");
    }
}

fn tavily_with(key: String) -> qbot_llm::search::TavilySearch {
    let endpoint = env_or("QBOT_LIVE_SEARCH_ENDPOINT", "https://api.tavily.com");
    let proxy = std::env::var("QBOT_LIVE_SEARCH_PROXY").ok();
    let transport = ReqwestTransport::with_proxy(
        endpoint,
        KeySource::Static(key),
        Duration::from_secs(10),
        proxy.as_deref(),
    )
    .unwrap();
    qbot_llm::search::TavilySearch::new(
        qbot_llm::search::TavilyConfig {
            max_results: 5,
            depth: qbot_llm::search::SearchDepth::Basic,
            timeout: Duration::from_secs(30),
            retry: qbot_llm::responses::RetryPolicy::default(),
            extract: qbot_llm::search::ExtractConfig {
                depth: qbot_llm::search::SearchDepth::Basic,
                chunks_per_source: 3,
            },
        },
        Arc::new(transport),
    )
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn tavily_returns_normalized_results_and_reports_credits() {
    use qbot_llm::search::WebSearch;
    let search = tavily_with(secret("search_api_key"));
    let out = search
        .search("Rust programming language 2024 edition release")
        .await
        .unwrap();
    eprintln!(
        "[tavily] hits={} credits={:?} requests={}",
        out.hits.len(),
        out.credits,
        out.requests
    );
    for hit in &out.hits {
        eprintln!(
            "[tavily]   {} | {} | published={:?} | {} chars",
            hit.title,
            hit.url,
            hit.published,
            hit.content.len()
        );
    }
    assert!(!out.hits.is_empty() && out.hits.len() <= 5);
    assert!(
        out.hits
            .iter()
            .all(|h| h.url.starts_with("http") && !h.title.is_empty())
    );
    assert!(
        out.hits
            .iter()
            .all(|h| !h.content.contains('\n') && !h.content.contains("  ")),
        "excerpts are whitespace-collapsed"
    );
    assert!(
        out.hits
            .iter()
            .any(|h| h.content.to_lowercase().contains("rust")),
        "results are about the query"
    );
    assert_eq!(out.credits, Some(1), "a basic search costs one credit");

    // A Chinese query works as well (the bot's groups mostly write Chinese).
    let zh = search
        .search("\u{676d}\u{5dde} \u{897f}\u{6e56} \u{95e8}\u{7968}")
        .await
        .unwrap();
    eprintln!("[tavily] zh hits={}", zh.hits.len());
    assert!(!zh.hits.is_empty());
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn tavily_rejects_a_wrong_key_as_an_authentication_error() {
    use qbot_llm::search::WebSearch;
    let error = tavily_with("tvly-not-a-real-key-000000".into())
        .search("anything")
        .await
        .unwrap_err();
    eprintln!("[tavily] wrong key -> {error:?}");
    assert_eq!(error, LlmError::Auth);
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn deepseek_forces_tools_only_without_reasoning_and_cannot_return_to_reasoning() {
    let provider = deepseek();
    assert_eq!(
        provider.info().capabilities.forced_tool_choice,
        qbot_llm::ForcedToolChoice::WithoutReasoning
    );
    let tools = weather_tool();
    let conv = Conversation::new(vec![
        msg(Role::System, "Use get_weather before answering."),
        msg(Role::User, "Weather in Paris?"),
    ]);
    // The real API refuses forced choice with thinking on, so the adapter refuses it before
    // sending; with reasoning off it is accepted and the call comes back.
    let mut req = request(&conv, &tools);
    req.tool_choice = ToolChoice::Required;
    assert!(matches!(
        provider.respond(req).await,
        Err(LlmError::Unsupported(_))
    ));
    let mut req = request(&conv, &tools);
    req.tool_choice = ToolChoice::Required;
    req.params.reasoning = ReasoningEffort::Off;
    let forced = provider.respond(req).await.unwrap();
    report("forced, no reasoning", &forced);
    assert!(forced.calls().any(|c| c.name == "get_weather"));

    // Continuing after it: without reasoning works; with reasoning the API refuses.
    let mut items = conv.items().to_vec();
    items.push(ConvItem::Assistant(forced.turn.clone()));
    for call in forced.calls() {
        items.push(ConvItem::ToolResult(ToolOutput {
            call_id: call.id.clone(),
            status: ToolStatus::Ok,
            content: vec![Content::Text("Paris: 17 degrees, light rain.".into())],
        }));
    }
    let next = Conversation::new(items);
    let mut req = request(&next, &tools);
    req.params.reasoning = ReasoningEffort::Off;
    let ok = provider.respond(req).await.unwrap();
    report("after, no reasoning", &ok);
    let thinking = provider.respond(request(&next, &tools)).await.unwrap_err();
    eprintln!("[forced] after, with reasoning -> {thinking:?}");
    assert!(matches!(thinking, LlmError::InvalidRequest(ref m) if m.contains("reasoning_text")));
}
