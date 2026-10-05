#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use futures_util::StreamExt;
use qbot_context::{AssistantPart, AssistantTurn, OpaqueReasoning, ToolCall};
use qbot_core::CallId;
use qbot_llm::Provider;
use qbot_llm::fake::FakeProvider;
use qbot_llm::{
    CacheUsage, Content, ConvItem, Conversation, FinishReason, LlmError, StreamEvent, collect,
};
use serde_json::json;

fn scripts() -> Vec<qbot_llm::fake::FakeReply> {
    vec![first_reply(), second_reply()]
}

#[tokio::test]
async fn a_tool_loop_behaves_identically_on_every_provider() {
    for (name, provider) in all_providers(&scripts()) {
        let tools = tools();
        let conv0 = base_conversation();
        let first = provider
            .respond(request(&conv0, &tools, None))
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        assert_eq!(first.finish, FinishReason::ToolCalls, "{name}");
        assert_eq!(first.calls().count(), 1, "{name}");
        let call = first.calls().next().unwrap();
        assert_eq!(call.name, "send_message", "{name}");
        assert_eq!(call.arguments, json!({ "text": "hi there" }), "{name}");
        assert!(
            first
                .turn
                .parts()
                .iter()
                .any(|p| matches!(p, AssistantPart::Reasoning(_))),
            "{name}"
        );
        assert!(
            first.usage.reported && first.usage.input_tokens > 0,
            "{name}"
        );
        assert_eq!(
            first.usage.cached_tokens(),
            Some(0),
            "{name}: nothing cached on the first call"
        );

        let conv1 = extend(&conv0, &first, "sent #2");
        let second = provider
            .respond(request(&conv1, &tools, Some(&first.continuation)))
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(second.finish, FinishReason::Stop, "{name}");
        assert_eq!(second.turn.text(), "all done", "{name}");
        assert!(
            matches!(second.usage.cache, CacheUsage::Reported { hit_tokens } if hit_tokens > 0),
            "{name}: the shared prefix must be reported as cached"
        );
    }
}

#[tokio::test]
async fn streaming_and_non_streaming_agree() {
    for ((name, plain), (_, streaming)) in all_providers(&scripts())
        .into_iter()
        .zip(all_providers(&scripts()))
    {
        let tools = tools();
        let conv0 = base_conversation();
        let a = plain.respond(request(&conv0, &tools, None)).await.unwrap();
        let mut stream = streaming
            .stream(request(&conv0, &tools, None))
            .await
            .unwrap();

        let (mut text, mut args, mut starts, mut last) = (String::new(), String::new(), 0, None);
        let mut completed = None;
        while let Some(event) = stream.next().await {
            assert!(completed.is_none(), "{name}: event after Completed");
            match event.unwrap() {
                StreamEvent::TextDelta(t) => text.push_str(&t),
                StreamEvent::ReasoningDelta(_) => {}
                StreamEvent::CallStarted { name: tool, .. } => {
                    assert_eq!(tool, "send_message");
                    starts += 1;
                }
                StreamEvent::CallArguments { fragment, .. } => args.push_str(&fragment),
                StreamEvent::Completed(response) => completed = Some(*response),
            }
            last = Some(());
        }
        let b = completed.unwrap_or_else(|| panic!("{name}: no Completed event"));
        assert!(last.is_some());
        assert_eq!(starts, 1, "{name}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&args).unwrap(),
            json!({ "text": "hi there" }),
            "{name}"
        );
        assert!(text.is_empty());

        assert_eq!(a.turn.parts(), b.turn.parts(), "{name}");
        assert_eq!(a.finish, b.finish, "{name}");
        assert_eq!(a.usage, b.usage, "{name}");
        let strip = |c: &qbot_llm::Continuation| {
            let mut value = serde_json::to_value(c).unwrap();
            value.as_object_mut().unwrap().remove("handle");
            value
        };
        assert_eq!(strip(&a.continuation), strip(&b.continuation), "{name}");
        assert_eq!(
            a.continuation.handle().is_some(),
            b.continuation.handle().is_some(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn text_deltas_add_up_to_the_final_text() {
    for (name, provider) in all_providers(&[second_reply()]) {
        let tools = tools();
        let conv = base_conversation();
        let mut stream = provider.stream(request(&conv, &tools, None)).await.unwrap();
        let mut text = String::new();
        let mut done = None;
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                StreamEvent::TextDelta(t) => text.push_str(&t),
                StreamEvent::Completed(r) => done = Some(*r),
                _ => {}
            }
        }
        assert_eq!(text, done.unwrap().turn.text(), "{name}");
    }
}

#[tokio::test]
async fn collect_enforces_the_stream_contract() {
    let provider = FakeProvider::new([second_reply().into()]);
    let tools = tools();
    let conv = base_conversation();
    let response = collect(provider.stream(request(&conv, &tools, None)).await.unwrap())
        .await
        .unwrap();
    assert_eq!(response.turn.text(), "all done");

    let empty: qbot_llm::EventStream = Box::pin(futures_util::stream::iter(Vec::new()));
    assert_eq!(
        collect(empty).await.unwrap_err(),
        LlmError::StreamInterrupted
    );
}

#[tokio::test]
async fn a_broken_transcript_fails_the_same_way_before_any_network() {
    for (name, provider) in all_providers(&scripts()) {
        let tools = tools();

        // A call with no result.
        let mut conv = base_conversation();
        let first = provider
            .respond(request(&conv, &tools, None))
            .await
            .unwrap();
        let mut items = conv.items().to_vec();
        items.push(ConvItem::Assistant(first.turn.clone()));
        conv = Conversation::new(items);
        assert!(
            matches!(
                provider.respond(request(&conv, &tools, None)).await,
                Err(LlmError::InvalidConversation(_))
            ),
            "{name}: unanswered call"
        );
    }
}

#[tokio::test]
async fn images_without_support_or_media_are_rejected_uniformly() {
    for (name, provider) in all_providers(&[second_reply()]) {
        let tools = tools();
        let mut items = base_conversation().items().to_vec();
        items.push(ConvItem::Message(qbot_llm::Message {
            role: qbot_llm::Role::User,
            content: vec![Content::Image {
                key: "pic-1".into(),
            }],
        }));
        let conv = Conversation::new(items);
        let err = provider
            .respond(request(&conv, &tools, None))
            .await
            .unwrap_err();
        let supported = provider.info().capabilities.image_input.is_some();
        match (supported, &err) {
            (false, LlmError::Unsupported(_)) | (true, LlmError::InvalidRequest(_)) => {}
            _ => panic!("{name}: unexpected {err:?}"),
        }
    }
}

#[tokio::test]
async fn a_transcript_survives_a_provider_switch() {
    // A turn produced by one provider is replayed to every other: provider-private reasoning is
    // dropped, text and calls are rebuilt, and the result is still a valid request.
    let producer = FakeProvider::new([first_reply().into()]);
    let tools = tools();
    let conv0 = base_conversation();
    let first = producer
        .respond(request(&conv0, &tools, None))
        .await
        .unwrap();
    let conv1 = extend(&conv0, &first, "sent #2");

    for (name, provider) in all_providers(&[second_reply()]) {
        let response = provider
            .respond(request(&conv1, &tools, Some(&first.continuation)))
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(response.turn.text(), "all done", "{name}");
    }
}

#[tokio::test]
async fn foreign_reasoning_and_hand_built_turns_are_rebuilt_from_parts() {
    let turn = AssistantTurn::new(vec![
        AssistantPart::Reasoning(OpaqueReasoning {
            provider: "someone-else".into(),
            payload: json!({ "type": "reasoning", "id": "rs_x" }),
            summary: None,
        }),
        AssistantPart::Text("let me check".into()),
        AssistantPart::Call(ToolCall {
            id: CallId::new("call-a").unwrap(),
            name: "send_message".into(),
            arguments: json!({ "text": "x" }),
        }),
    ]);
    let mut items = base_conversation().items().to_vec();
    items.push(ConvItem::Assistant(turn));
    items.push(ConvItem::ToolResult(qbot_llm::ToolOutput {
        call_id: CallId::new("call-a").unwrap(),
        status: qbot_llm::ToolStatus::Ok,
        content: vec![Content::Text("ok".into())],
    }));
    let conv = Conversation::new(items);
    let tools = tools();
    for (name, provider) in all_providers(&[second_reply()]) {
        provider
            .respond(request(&conv, &tools, None))
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}
