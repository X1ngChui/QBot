#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use futures_util::StreamExt;
use qbot_llm::fake::FakeReply;
use qbot_llm::responses::sim::SimStep;
use qbot_llm::responses::{HttpResponse, ResponsesConfig, ResponsesProvider, StateMode, Transport};
use qbot_llm::{
    Content, ConvItem, Conversation, FinishReason, LlmError, MediaStore, Provider, ReasoningEffort,
    StreamEvent, ToolChoice,
};
use serde_json::{Value, json};

#[tokio::test]
async fn deepseek_request_shape() {
    let (server, provider) =
        sim_provider(true, StateMode::Stateless, sim_steps(&[second_reply()]), 64);
    let tools = tools();
    let conv = base_conversation();
    provider
        .respond(request(&conv, &tools, None))
        .await
        .unwrap();

    let body = &server.requests()[0];
    assert_eq!(body["model"], "deepseek-test");
    assert_eq!(body["stream"], false);
    assert_eq!(body["store"], false);
    assert_eq!(body["parallel_tool_calls"], true);
    assert!(
        body.get("max_output_tokens").is_none(),
        "the output length is left to the provider"
    );
    assert_eq!(body["reasoning"], json!({ "effort": "low" }));
    assert!(
        body.get("include").is_none(),
        "no encrypted-content include on the DeepSeek dialect"
    );
    assert!(body.get("previous_response_id").is_none());
    assert!(body.get("tool_choice").is_none());
    assert_eq!(
        body["tools"][0],
        json!({
            "type": "function", "name": "send_message", "description": "Send a message to the group",
            "parameters": tools[0].schema, "strict": false,
        })
    );
    // The developer role is emulated as a user message; plain text stays a string.
    assert_eq!(
        body["input"][0],
        json!({ "role": "system", "content": "You are a group member." })
    );
    assert_eq!(
        body["input"][1],
        json!({ "role": "user", "content": "Persona: friendly." })
    );
}

#[tokio::test]
async fn reasoning_effort_maps_per_dialect() {
    for (deepseek, effort, expected) in [
        (true, ReasoningEffort::Off, Some("none")),
        (true, ReasoningEffort::Medium, Some("high")),
        (true, ReasoningEffort::High, Some("max")),
        (false, ReasoningEffort::Off, None),
        (false, ReasoningEffort::Medium, Some("medium")),
    ] {
        let (server, provider) = sim_provider(
            deepseek,
            StateMode::Stateless,
            sim_steps(&[second_reply()]),
            64,
        );
        let (tools, conv) = (tools(), base_conversation());
        let mut req = request(&conv, &tools, None);
        req.reasoning = effort;
        provider.respond(req).await.unwrap();
        let sent = server.requests()[0]["reasoning"]["effort"]
            .as_str()
            .map(str::to_owned);
        assert_eq!(sent.as_deref(), expected, "deepseek={deepseek} {effort:?}");
    }
}

#[tokio::test]
async fn own_output_items_are_echoed_back_verbatim() {
    let (server, provider) = sim_provider(
        true,
        StateMode::Stateless,
        sim_steps(&[first_reply(), second_reply()]),
        64,
    );
    let (tools, conv0) = (tools(), base_conversation());
    let first = provider
        .respond(request(&conv0, &tools, None))
        .await
        .unwrap();
    let native = first
        .turn
        .replay()
        .expect("native replay is kept")
        .payload
        .clone();

    let conv1 = extend(&conv0, &first, "sent #2");
    provider
        .respond(request(&conv1, &tools, Some(&first.continuation)))
        .await
        .unwrap();

    let input = server.requests()[1]["input"].as_array().unwrap().clone();
    let echoed: Vec<Value> = input[3..5].to_vec();
    assert_eq!(
        Value::Array(echoed),
        native,
        "reasoning and function_call items, ids included"
    );
    assert_eq!(
        input[5],
        json!({ "type": "function_call_output", "call_id": first.calls().next().unwrap().id.as_str(), "output": "sent #2" })
    );
    assert!(server.requests()[1].get("previous_response_id").is_none());
}

#[tokio::test]
async fn standard_stateless_asks_for_encrypted_reasoning_and_keeps_the_developer_role() {
    let (server, provider) = sim_provider(
        false,
        StateMode::Stateless,
        sim_steps(&[second_reply()]),
        64,
    );
    let (tools, conv) = (tools(), base_conversation());
    provider
        .respond(request(&conv, &tools, None))
        .await
        .unwrap();
    let body = &server.requests()[0];
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["input"][1]["role"], "developer");
}

#[tokio::test]
async fn server_state_sends_only_the_delta_and_falls_back_when_the_prefix_changes() {
    let replies = [first_reply(), second_reply(), second_reply()];
    let (server, provider) = sim_provider(false, StateMode::ServerState, sim_steps(&replies), 64);
    let (tools, conv0) = (tools(), base_conversation());
    let first = provider
        .respond(request(&conv0, &tools, None))
        .await
        .unwrap();
    assert_eq!(server.requests()[0]["store"], true);
    let handle = first.continuation.handle().unwrap().to_owned();
    assert_eq!(
        Some(handle.as_str()),
        first.meta.provider_response_id.as_deref()
    );

    let conv1 = extend(&conv0, &first, "sent #2");
    let second = provider
        .respond(request(&conv1, &tools, Some(&first.continuation)))
        .await
        .unwrap();
    let delta = &server.requests()[1];
    assert_eq!(delta["previous_response_id"], handle.as_str());
    assert_eq!(
        delta["input"].as_array().unwrap().len(),
        1,
        "only the new tool output is sent"
    );
    assert_eq!(delta["input"][0]["type"], "function_call_output");

    // Changing an earlier item (as elision would) invalidates the continuation: full replay.
    let mut items = conv1.items().to_vec();
    items[2] = common::message(qbot_llm::Role::User, "[msg:1] member:1: hello bot (edited)");
    let edited = Conversation::new(items);
    provider
        .respond(request(&edited, &tools, Some(&second.continuation)))
        .await
        .unwrap();
    let full = &server.requests()[2];
    assert!(full.get("previous_response_id").is_none());
    assert_eq!(full["input"].as_array().unwrap().len(), 6);
}

#[tokio::test]
async fn a_forgotten_server_state_handle_is_absorbed_with_one_full_resend() {
    let (_, first_provider) = sim_provider(
        false,
        StateMode::ServerState,
        sim_steps(&[first_reply()]),
        64,
    );
    let (server, provider) = sim_provider(
        false,
        StateMode::ServerState,
        sim_steps(&[second_reply()]),
        64,
    );
    let (tools, conv0) = (tools(), base_conversation());
    let first = first_provider
        .respond(request(&conv0, &tools, None))
        .await
        .unwrap();

    // The second server has never seen this response id.
    let conv1 = extend(&conv0, &first, "sent #2");
    let response = provider
        .respond(request(&conv1, &tools, Some(&first.continuation)))
        .await
        .unwrap();
    assert_eq!(response.turn.text(), "all done");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].get("previous_response_id").is_some());
    assert!(requests[1].get("previous_response_id").is_none());
    assert_eq!(response.meta.attempts, 2);
}

#[tokio::test]
async fn forced_tool_choice_follows_capabilities() {
    let (tools, conv) = (tools(), base_conversation());

    let (_, deepseek) = sim_provider(true, StateMode::Stateless, sim_steps(&[second_reply()]), 64);
    let mut req = request(&conv, &tools, None);
    req.tool_choice = ToolChoice::Required;
    assert_eq!(
        deepseek.respond(req).await.unwrap_err(),
        LlmError::Unsupported("forced tool choice with reasoning on")
    );
    // With reasoning off DeepSeek takes it.
    let (server, deepseek) =
        sim_provider(true, StateMode::Stateless, sim_steps(&[second_reply()]), 64);
    let mut req = request(&conv, &tools, None);
    req.tool_choice = ToolChoice::Required;
    req.reasoning = ReasoningEffort::Off;
    deepseek.respond(req).await.unwrap();
    assert_eq!(server.requests()[0]["tool_choice"], json!("required"));

    let (server, standard) = sim_provider(
        false,
        StateMode::Stateless,
        sim_steps(&[first_reply(), second_reply()]),
        64,
    );
    let mut req = request(&conv, &tools, None);
    req.tool_choice = ToolChoice::Named("send_message".into());
    standard.respond(req).await.unwrap();
    let body = &server.requests()[0];
    assert_eq!(
        body["tool_choice"],
        json!({ "type": "function", "name": "send_message" })
    );

    // `None` is emulated by declaring no tools.
    let mut req = request(&conv, &tools, None);
    req.tool_choice = ToolChoice::None;
    standard.respond(req).await.unwrap();
    assert!(server.requests()[1].get("tools").is_none());
}

struct OneImage;

#[async_trait]
impl MediaStore for OneImage {
    async fn load(&self, key: &str) -> Result<qbot_llm::request::LoadedMedia, LlmError> {
        assert_eq!(key, "pic-1");
        Ok(qbot_llm::request::LoadedMedia {
            mime: "image/png".into(),
            bytes: vec![1, 2, 3],
        })
    }
}

#[tokio::test]
async fn images_become_data_urls_on_providers_that_accept_them() {
    let (server, provider) = sim_provider(
        false,
        StateMode::Stateless,
        sim_steps(&[second_reply()]),
        64,
    );
    let tools = tools();
    let mut items = base_conversation().items().to_vec();
    items.push(ConvItem::Message(qbot_llm::Message {
        role: qbot_llm::Role::User,
        content: vec![
            Content::Text("look".into()),
            Content::Image {
                key: "pic-1".into(),
            },
        ],
    }));
    let conv = Conversation::new(items);
    let media = OneImage;
    let mut req = request(&conv, &tools, None);
    req.media = Some(&media);
    provider.respond(req).await.unwrap();
    let last = server.requests()[0]["input"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        last["content"][0],
        json!({ "type": "input_text", "text": "look" })
    );
    assert_eq!(
        last["content"][1],
        json!({ "type": "input_image", "image_url": "data:image/png;base64,AQID" })
    );
}

#[tokio::test]
async fn deepseek_takes_images_as_uploaded_files_and_uploads_each_once() {
    let (server, provider) = sim_provider(
        true,
        StateMode::Stateless,
        sim_steps(&[second_reply(), second_reply()]),
        64,
    );
    let tools = tools();
    let mut items = base_conversation().items().to_vec();
    items.push(ConvItem::Message(qbot_llm::Message {
        role: qbot_llm::Role::User,
        content: vec![
            Content::Text("look".into()),
            Content::Image {
                key: "pic-1".into(),
            },
        ],
    }));
    let conv = Conversation::new(items);
    let media = OneImage;
    for _ in 0..2 {
        let mut req = request(&conv, &tools, None);
        req.media = Some(&media);
        provider.respond(req).await.unwrap();
    }
    let uploads = server.uploads();
    assert_eq!(uploads.len(), 1, "the second request reuses the file");
    let (id, fields, bytes) = &uploads[0];
    assert_eq!(*bytes, 3);
    assert!(
        fields.contains(&("purpose".to_owned(), "user_data".to_owned())),
        "{fields:?}"
    );
    assert!(
        fields
            .iter()
            .any(|(k, v)| k == "expires_after[seconds]" && v == "2592000"),
        "{fields:?}"
    );
    for sent in server.requests() {
        let last = sent["input"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(
            last["content"][1],
            json!({ "type": "input_image", "file_id": id })
        );
    }
}

// ----- errors, retries, malformed output -----

fn http(status: u16, code: &'static str) -> SimStep {
    SimStep::Http {
        status,
        retry_after: None,
        code,
        message: "nope",
    }
}

async fn respond_with(steps: Vec<SimStep>) -> Result<qbot_llm::Response, LlmError> {
    let (_, provider) = sim_provider(true, StateMode::Stateless, steps, 64);
    let (tools, conv) = (tools(), base_conversation());
    provider.respond(request(&conv, &tools, None)).await
}

#[tokio::test]
async fn http_errors_are_normalized() {
    assert_eq!(
        respond_with(vec![http(401, "invalid_api_key")])
            .await
            .unwrap_err(),
        LlmError::Auth
    );
    assert_eq!(
        respond_with(vec![http(400, "context_length_exceeded")])
            .await
            .unwrap_err(),
        LlmError::ContextTooLong
    );
    assert_eq!(
        respond_with(vec![http(400, "content_policy_violation")])
            .await
            .unwrap_err(),
        LlmError::ContentFiltered
    );
    assert!(matches!(
        respond_with(vec![http(400, "bad")]).await.unwrap_err(),
        LlmError::InvalidRequest(_)
    ));
    assert!(matches!(
        respond_with(vec![http(418, "teapot")]).await.unwrap_err(),
        LlmError::Other { status: 418, .. }
    ));
    // An account without balance is an account limit: typed, and not retried.
    let error = respond_with(vec![http(402, "insufficient_balance")])
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::QuotaExhausted(_)), "{error:?}");
    assert!(!error.is_transient());
}

#[tokio::test(start_paused = true)]
async fn transient_errors_retry_with_backoff_and_honor_retry_after() {
    let steps = vec![
        SimStep::Http {
            status: 429,
            retry_after: Some(Duration::from_secs(3)),
            code: "rate_limit_exceeded",
            message: "slow",
        },
        http(503, "server_error"),
        second_reply().into(),
    ];
    let started = tokio::time::Instant::now();
    let response = respond_with(steps).await.unwrap();
    assert_eq!(response.meta.attempts, 3);
    // 3 s (Retry-After beats 100 ms) + 200 ms exponential backoff.
    assert_eq!(started.elapsed(), Duration::from_millis(3200));
}

#[tokio::test(start_paused = true)]
async fn retries_are_bounded() {
    let steps = vec![
        http(500, "server_error"),
        http(500, "server_error"),
        http(500, "server_error"),
        second_reply().into(),
    ];
    assert_eq!(
        respond_with(steps).await.unwrap_err(),
        LlmError::Unavailable
    );
}

#[tokio::test]
async fn non_transient_errors_are_not_retried() {
    let (server, provider) = sim_provider(
        true,
        StateMode::Stateless,
        vec![http(401, "invalid_api_key"), second_reply().into()],
        64,
    );
    let (tools, conv) = (tools(), base_conversation());
    assert_eq!(
        provider
            .respond(request(&conv, &tools, None))
            .await
            .unwrap_err(),
        LlmError::Auth
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn terminal_failure_and_truncation_are_normalized() {
    assert_eq!(
        respond_with(vec![SimStep::Failed {
            code: "server_error"
        }])
        .await
        .unwrap_err(),
        LlmError::Unavailable
    );

    let cut = FakeReply::new().text("a long answer that got cu");
    let response = respond_with(vec![SimStep::Incomplete {
        reply: cut,
        reason: "max_output_tokens",
    }])
    .await
    .unwrap();
    assert_eq!(response.finish, FinishReason::Length);
    assert_eq!(response.turn.text(), "a long answer that got cu");

    let filtered = FakeReply::new().text("x");
    assert_eq!(
        respond_with(vec![SimStep::Incomplete {
            reply: filtered,
            reason: "content_filter"
        }])
        .await
        .unwrap_err(),
        LlmError::ContentFiltered
    );
}

#[tokio::test]
async fn malformed_output_is_a_protocol_error_not_a_guess() {
    let call = |id: &str, args: &str| json!({ "type": "function_call", "id": format!("fc_{id}"), "call_id": id, "name": "send_message", "arguments": args, "status": "completed" });
    let raw = |output: Vec<Value>| {
        SimStep::Raw(
            json!({ "id": "r", "status": "completed", "model": "m", "output": output, "usage": { "input_tokens": 1, "output_tokens": 1 } }),
        )
    };

    assert!(matches!(
        respond_with(vec![raw(vec![call("a", "{}"), call("a", "{}")])]).await,
        Err(LlmError::Protocol(_))
    ));
    assert!(matches!(
        respond_with(vec![raw(vec![call("a", "not json")])]).await,
        Err(LlmError::Protocol(_))
    ));
    assert!(matches!(
        respond_with(vec![raw(vec![call("a", "[1]")])]).await,
        Err(LlmError::Protocol(_))
    ));
    assert!(matches!(
        respond_with(vec![raw(vec![call("a b", "{}")])]).await,
        Err(LlmError::Protocol(_))
    ));
    let pending = json!({ "type": "message", "id": "m", "status": "in_progress", "content": [] });
    assert!(matches!(
        respond_with(vec![raw(vec![pending])]).await,
        Err(LlmError::Protocol(_))
    ));
}

#[tokio::test]
async fn missing_usage_is_typed_not_zero_filled() {
    let raw = SimStep::Raw(json!({ "id": "r", "status": "completed", "model": "m", "output": [] }));
    let response = respond_with(vec![raw]).await.unwrap();
    assert!(!response.usage.reported);
    assert_eq!(response.usage.cached_tokens(), None);
    // An empty-args call is accepted as an empty object.
    let call = json!({ "type": "function_call", "id": "fc", "call_id": "c1", "name": "send_message", "arguments": "", "status": "completed" });
    let raw = SimStep::Raw(
        json!({ "id": "r", "status": "completed", "output": [call], "usage": { "input_tokens": 5, "output_tokens": 2 } }),
    );
    let response = respond_with(vec![raw]).await.unwrap();
    assert_eq!(response.calls().next().unwrap().arguments, json!({}));
    assert_eq!(
        response.usage.cached_tokens(),
        None,
        "no cached_tokens field means not reported"
    );
}

// ----- streaming failures -----

async fn drain(steps: Vec<SimStep>) -> Vec<Result<StreamEvent, LlmError>> {
    let (_, provider) = sim_provider(true, StateMode::Stateless, steps, 5);
    let (tools, conv) = (tools(), base_conversation());
    let stream = provider.stream(request(&conv, &tools, None)).await.unwrap();
    stream.collect().await
}

#[tokio::test]
async fn stream_errors_end_the_stream_without_retrying() {
    let events = drain(vec![
        SimStep::StreamError {
            code: "rate_limit_exceeded",
        },
        second_reply().into(),
    ])
    .await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].as_ref().unwrap_err(),
        &LlmError::RateLimited { retry_after: None }
    );
}

#[tokio::test]
async fn a_stream_cut_before_the_terminal_event_is_interrupted() {
    let reply = FakeReply::new().text("partial text here");
    let events = drain(vec![SimStep::CutStream { reply, events: 2 }]).await;
    assert!(events.len() >= 2);
    assert!(events[..events.len() - 1].iter().all(Result::is_ok));
    assert_eq!(
        events.last().unwrap().as_ref().unwrap_err(),
        &LlmError::StreamInterrupted
    );
}

struct Hang;

#[async_trait]
impl Transport for Hang {
    async fn post(&self, _: &str, _: &Value, _: bool) -> Result<HttpResponse, LlmError> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn a_hung_connection_times_out() {
    let mut cfg = ResponsesConfig::deepseek("m");
    cfg.timeout = Some(Duration::from_secs(5));
    let provider = ResponsesProvider::new(cfg, Arc::new(Hang)).unwrap();
    let (tools, conv) = (tools(), base_conversation());
    assert_eq!(
        provider
            .respond(request(&conv, &tools, None))
            .await
            .unwrap_err(),
        LlmError::Timeout
    );
    assert!(matches!(
        provider.stream(request(&conv, &tools, None)).await,
        Err(LlmError::Timeout)
    ));
}

#[test]
fn the_deepseek_dialect_cannot_be_given_server_state() {
    let mut cfg = ResponsesConfig::deepseek("m");
    cfg.state = StateMode::ServerState;
    assert!(matches!(
        ResponsesProvider::new(cfg, Arc::new(Hang)),
        Err(LlmError::Unsupported(_))
    ));
}

#[test]
fn capabilities_describe_native_versus_emulated() {
    use qbot_llm::Realization::{Emulated, Native};
    let hang: Arc<dyn Transport> = Arc::new(Hang);
    let deepseek = ResponsesProvider::new(ResponsesConfig::deepseek("m"), hang.clone()).unwrap();
    let caps = deepseek.info().capabilities;
    assert_eq!(
        (caps.continuation, caps.developer_role, caps.image_input),
        (Emulated, Emulated, Some(Emulated))
    );
    let openai =
        ResponsesProvider::new(ResponsesConfig::standard("m", StateMode::ServerState), hang)
            .unwrap();
    let caps = openai.info().capabilities;
    assert_eq!(
        (caps.continuation, caps.developer_role, caps.image_input),
        (Native, Native, Some(Native))
    );
}
