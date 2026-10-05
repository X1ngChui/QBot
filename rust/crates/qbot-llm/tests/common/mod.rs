#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_llm::responses::sim::{SimServer, SimStep};
use qbot_llm::responses::{ResponsesConfig, ResponsesProvider, RetryPolicy, StateMode};
use qbot_llm::{
    Content, Continuation, ConvItem, Conversation, Message, Params, Provider, ReasoningEffort,
    Request, Response, Role, ToolChoice, ToolOutput, ToolSpec, ToolStatus,
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

pub fn params() -> Params {
    Params {
        max_output_tokens: 1000,
        reasoning: ReasoningEffort::Low,
        temperature: None,
    }
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

pub fn request<'a>(
    conversation: &'a Conversation,
    tools: &'a [ToolSpec],
    continuation: Option<&'a Continuation>,
) -> Request<'a> {
    Request {
        conversation,
        tools,
        tool_choice: ToolChoice::Auto,
        parallel_tool_calls: true,
        params: params(),
        continuation,
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
    }
}

pub fn sim_provider(
    deepseek: bool,
    state: StateMode,
    script: Vec<SimStep>,
    chunk: usize,
) -> (Arc<SimServer>, Arc<ResponsesProvider>) {
    let server = Arc::new(SimServer::new(script).with_chunk_size(chunk));
    let mut cfg = if deepseek {
        ResponsesConfig::deepseek("deepseek-test")
    } else {
        ResponsesConfig::standard("gpt-test", state)
    };
    cfg.retry = fast_retry();
    let provider = ResponsesProvider::new(cfg, server.clone()).unwrap();
    (server, Arc::new(provider))
}

pub fn sim_steps(replies: &[FakeReply]) -> Vec<SimStep> {
    replies.iter().cloned().map(SimStep::Reply).collect()
}

/// Every provider implementation, each loaded with the same script.
pub fn all_providers(replies: &[FakeReply]) -> Vec<(&'static str, Arc<dyn Provider>)> {
    let fake_steps = || replies.iter().cloned().map(Step::Reply).collect::<Vec<_>>();
    vec![
        ("fake-stateless", Arc::new(FakeProvider::new(fake_steps()))),
        (
            "fake-stateful",
            Arc::new(FakeProvider::new(fake_steps()).stateful()),
        ),
        (
            "deepseek",
            sim_provider(true, StateMode::Stateless, sim_steps(replies), 7).1,
        ),
        (
            "openai-stateless",
            sim_provider(false, StateMode::Stateless, sim_steps(replies), 13).1,
        ),
        (
            "openai-server-state",
            sim_provider(false, StateMode::ServerState, sim_steps(replies), 64).1,
        ),
    ]
}
