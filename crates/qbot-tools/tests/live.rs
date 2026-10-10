//! Live check of web search end to end: the real text model decides to call `web_search`, the
//! real tool queries the real search provider, and the model answers from the result. Ignored by
//! default; run with `cargo test -p qbot-tools --test live -- --ignored --nocapture`. Keys are
//! read from `deploy/secrets` (or `QBOT_LIVE_SECRETS_DIR`) into memory only.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use qbot_agent::{ChatView, RunState, Tool, ToolCx, ToolSet, Trigger};
use qbot_core::{AccountId, GroupId, MessageId, RunId, SystemClock};
use qbot_llm::responses::{
    KeySource, ReqwestTransport, ResponsesConfig, ResponsesProvider, RetryPolicy,
};
use qbot_llm::search::{SearchDepth, TavilyConfig, TavilySearch};
use qbot_llm::{
    Content, ConvItem, Conversation, FinishReason, Message, Provider, ReasoningEffort, Request,
    Role, ToolChoice, ToolOutput as WireOutput, ToolStatus,
};
use qbot_tools::{WebSearchArgs, WebSearchTool};

fn secret(file: &str) -> String {
    let dir = std::env::var_os("QBOT_LIVE_SECRETS_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/secrets"),
        PathBuf::from,
    );
    let path = dir.join(file);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read secret file {}: {}", path.display(), e.kind()))
        .trim()
        .to_owned()
}

fn text(role: Role, text: &str) -> ConvItem {
    ConvItem::Message(Message {
        role,
        content: vec![Content::Text(text.into())],
    })
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn the_model_searches_the_web_and_answers_from_the_results() {
    let search = Arc::new(TavilySearch::new(
        TavilyConfig {
            max_results: 5,
            depth: SearchDepth::Basic,
            timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            extract: qbot_llm::search::ExtractConfig {
                depth: qbot_llm::search::SearchDepth::Basic,
                chunks_per_source: 3,
            },
        },
        Arc::new(
            ReqwestTransport::new(
                "https://api.tavily.com",
                KeySource::Static(secret("search_api_key")),
                &std::env::var("QBOT_LIVE_SEARCH_PROXY")
                    .map_or(qbot_llm::net::Route::Direct, qbot_llm::net::Route::Proxy),
            )
            .unwrap(),
        ),
    ));
    let tool = WebSearchTool::new(search);
    let specs = ToolSet::new().with(tool.clone()).unwrap().specs();

    let mut cfg = ResponsesConfig::deepseek("deepseek-flash");
    cfg.timeout = Some(Duration::from_secs(120));
    let model = ResponsesProvider::new(
        cfg,
        Arc::new(
            ReqwestTransport::new(
                "https://api.deepseek.com",
                KeySource::Static(secret("text_api_key")),
                &qbot_llm::net::Route::Direct,
            )
            .unwrap(),
        ),
    )
    .unwrap();
    let request = |conversation| Request {
        conversation,
        tools: &specs,
        tool_choice: ToolChoice::Auto,
        parallel_tool_calls: true,
        reasoning: ReasoningEffort::Low,
        continuation: None,
        media: None,
    };

    let conv = Conversation::new(vec![
        text(
            Role::System,
            "You answer questions. Your own knowledge may be outdated: for facts about releases \
             and dates, use web_search first, then answer in one sentence that includes the date.",
        ),
        text(
            Role::User,
            "On what date was Rust 1.85.0, the release that stabilized the 2024 edition, published?",
        ),
    ]);
    let first = model.respond(request(&conv)).await.unwrap();
    let calls: Vec<_> = first.calls().cloned().collect();
    eprintln!(
        "[web] calls={:?}",
        calls
            .iter()
            .map(|c| c.arguments.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(first.finish, FinishReason::ToolCalls);
    assert!(calls.iter().all(|c| c.name == "web_search"));

    // Each call runs through the real tool, exactly as the run loop would.
    let trigger = Trigger::Addressed {
        message: MessageId::new(1).unwrap(),
        sender: AccountId::new(2).unwrap(),
    };
    let (view, state, clock) = (ChatView::default(), RunState::default(), SystemClock);
    let cx = ToolCx {
        group: GroupId::new(1).unwrap(),
        run: RunId::new(1),
        trigger: &trigger,
        view: &view,
        state: &state,
        clock: &clock,
    };
    let mut items = conv.items().to_vec();
    items.push(ConvItem::Assistant(first.turn.clone()));
    for call in &calls {
        let args: WebSearchArgs = serde_json::from_value(call.arguments.clone()).unwrap();
        let out = tool.call(&cx, args).await.unwrap();
        let qbot_context::Part::Text(result) = &out.content[0] else {
            panic!()
        };
        eprintln!(
            "[web] tool result: {} chars, starts {:?}",
            result.len(),
            &result[..result.len().min(160)]
        );
        assert!(result.starts_with("Web results for"), "{result}");
        items.push(ConvItem::ToolResult(WireOutput {
            call_id: call.id.clone(),
            status: ToolStatus::Ok,
            content: vec![Content::Text(result.clone())],
        }));
    }
    let conv = Conversation::new(items);
    let answer = model.respond(request(&conv)).await.unwrap();
    let said = answer.turn.text();
    eprintln!("[web] answer={said:?} usage={:?}", answer.usage);
    assert_eq!(answer.finish, FinishReason::Stop);
    let lower = said.to_lowercase();
    assert!(lower.contains("2025"), "{said}");
    assert!(
        lower.contains("february 20")
            || lower.contains("feb 20")
            || lower.contains("20 february")
            || lower.contains("2025-02-20"),
        "{said}"
    );
}

fn tavily_reader() -> Arc<TavilySearch> {
    Arc::new(TavilySearch::new(
        TavilyConfig {
            max_results: 5,
            depth: SearchDepth::Basic,
            timeout: Duration::from_secs(60),
            retry: RetryPolicy::default(),
            extract: qbot_llm::search::ExtractConfig {
                depth: SearchDepth::Basic,
                chunks_per_source: 3,
            },
        },
        Arc::new(
            ReqwestTransport::new(
                "https://api.tavily.com",
                KeySource::Static(secret("search_api_key")),
                &std::env::var("QBOT_LIVE_SEARCH_PROXY")
                    .map_or(qbot_llm::net::Route::Direct, qbot_llm::net::Route::Proxy),
            )
            .unwrap(),
        ),
    ))
}

async fn read_live(url: &str, question: Option<&str>, max_chars: usize) -> String {
    use qbot_tools::{ReadUrl, ReadUrlArgs};
    let trigger = Trigger::Addressed {
        message: MessageId::new(1).unwrap(),
        sender: AccountId::new(2).unwrap(),
    };
    let (view, state, clock) = (ChatView::default(), RunState::default(), SystemClock);
    let cx = ToolCx {
        group: GroupId::new(1).unwrap(),
        run: RunId::new(1),
        trigger: &trigger,
        view: &view,
        state: &state,
        clock: &clock,
    };
    let out = ReadUrl::new(tavily_reader(), max_chars)
        .call(
            &cx,
            ReadUrlArgs {
                url: url.into(),
                question: question.map(str::to_owned),
            },
        )
        .await
        .unwrap();
    let qbot_context::Part::Text(text) = &out.content[0] else {
        panic!()
    };
    let preview: String = text.chars().skip(150).take(220).collect();
    eprintln!(
        "[read_url] {url} question={question:?}: {} chars; cut={}; text after header: {preview:?}",
        text.chars().count(),
        text.contains("[cut here")
    );
    text.clone()
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_long_article_is_read_whole_with_structure_or_by_question() {
    let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
    let whole = read_live(url, None, 8000).await;
    assert!(
        whole.contains(", as Markdown:"),
        "the page under its address"
    );
    assert!(whole.contains("Rust"));
    assert!(
        whole.contains("\n#")
            || whole.contains("\n- ")
            || whole.contains("\n|")
            || whole.contains("]("),
        "Markdown structure survives"
    );
    assert!(
        whole.contains("[cut here"),
        "a long article is cut and says so"
    );

    let asked = read_live(
        url,
        Some("Who created Rust, and at which company did it start?"),
        8000,
    )
    .await;
    assert!(asked.contains("Graydon"), "the relevant passage comes back");
    assert!(asked.contains("Mozilla"));
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_chinese_page_and_a_pdf_can_be_read() {
    let zh = read_live(
        "https://zh.wikipedia.org/wiki/%E8%A5%BF%E6%B9%96",
        Some("\u{897f}\u{6e56}\u{4f4d}\u{4e8e}\u{54ea}\u{4e2a}\u{57ce}\u{5e02}"),
        8000,
    )
    .await;
    assert!(
        zh.contains("\u{676d}\u{5dde}"),
        "the passage names the city"
    );

    let pdf = read_live(
        "https://arxiv.org/pdf/1706.03762",
        Some("What BLEU score does the big Transformer reach on English-to-German translation?"),
        8000,
    )
    .await;
    assert!(pdf.contains("28.4"), "the number from the paper's text");
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn an_unreachable_page_is_reported_as_a_readable_answer() {
    let text = read_live("https://qbot-no-such-host.invalid/page", None, 8000).await;
    assert!(
        text.starts_with("could not read") || text.contains("no readable text"),
        "{text}"
    );
}
