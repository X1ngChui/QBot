#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_llm::responses::sim::{SimServer, SimStep};
use qbot_llm::responses::{ResponsesConfig, ResponsesProvider, RetryPolicy};
use qbot_llm::{
    Content, ConvItem, Conversation, Message, Provider, ReasoningEffort, Request, Response, Role,
    ToolChoice, ToolOutput, ToolSpec, ToolStatus,
};
use serde_json::json;

pub fn tools() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "send_message".into(),
        description: "Send a message to the group".into(),
        schema: json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
            "additionalProperties": false
        }),
    }]
}

pub fn message(role: Role, text: &str) -> ConvItem {
    ConvItem::Message(Message {
        role,
        content: vec![Content::Text(text.into())],
    })
}

pub fn base_conversation() -> Conversation {
    Conversation::new(vec![
        message(Role::System, "You are a group member."),
        message(Role::Developer, "Persona: friendly."),
        message(Role::User, "[msg:1] member:1: hello bot"),
    ])
}

pub fn request<'a>(conversation: &'a Conversation, tools: &'a [ToolSpec]) -> Request<'a> {
    Request {
        conversation,
        tools,
        tool_choice: ToolChoice::Auto,
        parallel_tool_calls: true,
        reasoning: ReasoningEffort::Low,
        media: None,
    }
}

/// The conversation after the model's turn and its tool results.
pub fn extend(conversation: &Conversation, response: &Response, result: &str) -> Conversation {
    let mut items = conversation.items().to_vec();
    items.push(ConvItem::Assistant(response.turn.clone()));
    for call in response.calls() {
        items.push(ConvItem::ToolResult(ToolOutput {
            call_id: call.id.clone(),
            status: ToolStatus::Ok,
            content: vec![Content::Text(result.into())],
        }));
    }
    Conversation::new(items)
}

pub fn first_reply() -> FakeReply {
    FakeReply::new()
        .reasoning("thinking")
        .call("send_message", json!({ "text": "hi there" }))
}

pub fn second_reply() -> FakeReply {
    FakeReply::new().text("all done")
}

pub fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        retries: 2,
        base: Duration::from_millis(100),
        jitter: false,
        attempt_timeout: None,
    }
}

pub fn sim_provider(script: Vec<SimStep>) -> (Arc<SimServer>, Arc<ResponsesProvider>) {
    let server = Arc::new(SimServer::new(script));
    let mut cfg = ResponsesConfig::new("deepseek-test");
    cfg.retry = fast_retry();
    let provider = ResponsesProvider::new(cfg, server.clone());
    (server, Arc::new(provider))
}

/// [`sim_provider`] recording its calls on a meter.
pub fn metered_provider(
    script: Vec<SimStep>,
) -> (
    Arc<SimServer>,
    Arc<ResponsesProvider>,
    Arc<qbot_llm::MemoryMeter>,
) {
    let (server, provider) = sim_provider(script);
    let meter = qbot_llm::MemoryMeter::new();
    let provider = Arc::into_inner(provider)
        .expect("only reference")
        .with_meter(meter.clone());
    (server, Arc::new(provider), meter)
}

pub fn sim_steps(replies: &[FakeReply]) -> Vec<SimStep> {
    replies.iter().cloned().map(SimStep::Reply).collect()
}

/// Every provider implementation, each loaded with the same script.
pub fn all_providers(replies: &[FakeReply]) -> Vec<(&'static str, Arc<dyn Provider>)> {
    let fake_steps = || replies.iter().cloned().map(Step::Reply).collect::<Vec<_>>();
    vec![
        ("fake", Arc::new(FakeProvider::new(fake_steps()))),
        ("deepseek", sim_provider(sim_steps(replies)).1),
    ]
}
