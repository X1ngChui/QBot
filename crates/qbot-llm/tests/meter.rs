//! Provider calls are recorded once each, with every request they made and the work they
//! served, also when they fail or are cut off.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use qbot_core::{GroupId, RunId};
use qbot_llm::responses::sim::SimStep;
use qbot_llm::responses::{HttpResponse, ResponsesConfig, ResponsesProvider, StateMode, Transport};
use qbot_llm::{
    Attribution, CacheUsage, CallKind, LlmError, MemoryMeter, Provider, Purpose, attributed,
};
use serde_json::Value;

fn group() -> GroupId {
    GroupId::new(4242).unwrap()
}

#[tokio::test]
async fn a_call_carries_the_work_it_was_made_for() {
    let (_, provider, meter) =
        metered_provider(true, StateMode::Stateless, sim_steps(&[first_reply()]));
    let (tools, conv) = (tools(), base_conversation());
    let reply = Attribution {
        purpose: Purpose::Reply,
        group: group(),
        run: Some(RunId::new(7)),
        turn: Some(2),
    };
    let response = attributed(reply, provider.respond(request(&conv, &tools, None)))
        .await
        .unwrap();

    let calls = meter.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.attribution, Some(reply));
    assert_eq!(
        (call.kind, call.provider.as_str(), call.model.as_str()),
        (CallKind::Model, "deepseek", "deepseek-test")
    );
    assert_eq!((call.requests, call.error, call.credits), (1, None, None));
    assert_eq!(
        call.tokens,
        Some(response.usage),
        "the tokens of the one response, as the caller saw them"
    );
}

#[tokio::test]
async fn a_call_outside_any_work_is_still_recorded_unattributed() {
    let (_, provider, meter) =
        metered_provider(true, StateMode::Stateless, sim_steps(&[first_reply()]));
    let (tools, conv) = (tools(), base_conversation());
    provider
        .respond(request(&conv, &tools, None))
        .await
        .unwrap();
    assert_eq!(meter.calls().len(), 1);
    assert_eq!(meter.calls()[0].attribution, None);
}

#[tokio::test(start_paused = true)]
async fn a_failed_call_counts_every_request_and_no_tokens() {
    let steps = vec![
        SimStep::Http {
            status: 503,
            retry_after: None,
            code: "server_error",
            message: "busy",
        };
        3
    ];
    let (_, provider, meter) = metered_provider(true, StateMode::Stateless, steps);
    let (tools, conv) = (tools(), base_conversation());
    let work = Attribution::group(Purpose::Extraction, group());
    let error = attributed(work, provider.respond(request(&conv, &tools, None)))
        .await
        .unwrap_err();
    assert_eq!(error, LlmError::Unavailable);
    let calls = meter.calls();
    assert_eq!(calls.len(), 1, "one call however many retries");
    assert_eq!(calls[0].requests, 3, "the first attempt and both retries");
    assert_eq!(calls[0].error, Some("unavailable"));
    assert_eq!(calls[0].tokens, None, "no response was read");
    assert_eq!(calls[0].attribution, Some(work));
}

struct Hang;

#[async_trait]
impl Transport for Hang {
    async fn post(&self, _: &str, _: &Value) -> Result<HttpResponse, LlmError> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn a_call_cut_off_by_its_caller_is_recorded_as_cancelled() {
    let meter = MemoryMeter::new();
    let provider = ResponsesProvider::new(ResponsesConfig::deepseek("m"), Arc::new(Hang))
        .unwrap()
        .with_meter(meter.clone());
    let (tools, conv) = (tools(), base_conversation());
    let work = Attribution::group(Purpose::Picture, group());
    let cut = tokio::time::timeout(
        Duration::from_secs(1),
        attributed(work, provider.respond(request(&conv, &tools, None))),
    )
    .await;
    assert!(cut.is_err(), "the caller gave up");
    let calls = meter.calls();
    assert_eq!(calls.len(), 1, "recorded when dropped");
    assert_eq!(
        (calls[0].requests, calls[0].error),
        (1, Some("cancelled")),
        "the request went out"
    );
    assert_eq!(calls[0].attribution, Some(work));
}

#[tokio::test]
async fn a_provider_without_cache_figures_says_so() {
    let (_, provider, meter) = metered_provider(
        false,
        StateMode::Stateless,
        vec![SimStep::Raw(serde_json::json!({
            "id": "resp_1",
            "status": "completed",
            "model": "gpt-test",
            "output": [{
                "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }],
            "usage": {"input_tokens": 10, "output_tokens": 2}
        }))],
    );
    let (tools, conv) = (tools(), base_conversation());
    provider
        .respond(request(&conv, &tools, None))
        .await
        .unwrap();
    assert_eq!(
        meter.calls()[0].tokens.map(|t| t.cache),
        Some(CacheUsage::NotReported)
    );
}

#[tokio::test]
async fn an_unreadable_answer_is_still_counted_with_its_tokens() {
    let raw = SimStep::Raw(serde_json::json!({
        "id": "r", "status": "completed", "model": "m",
        "output": [{"type": "function_call", "id": "fc", "call_id": "c1", "name": "send_messages",
                    "arguments": "{\"messages\": [\"he said \"hi\"\"]}", "status": "completed"}],
        "usage": {"input_tokens": 100, "output_tokens": 20}
    }));
    let (_, provider, meter) = metered_provider(true, StateMode::Stateless, vec![raw]);
    let (tools, conv) = (tools(), base_conversation());
    let error = provider
        .respond(request(&conv, &tools, None))
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::MalformedCall(_)), "{error:?}");
    let call = &meter.calls()[0];
    assert_eq!(call.error, Some("malformed_call"));
    assert_eq!(
        call.tokens.map(|t| (t.input_tokens, t.output_tokens)),
        Some((100, 20)),
        "billed tokens are recorded although the answer was unusable"
    );
}
