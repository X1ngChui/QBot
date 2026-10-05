//! A scripted provider that implements the whole contract with no network.
//!
//! It validates every request exactly as a real adapter would, simulates prefix-cache usage
//! from the conversation it is given, optionally keeps "server state" so continuations are
//! exercised, and records everything it was asked. Milestone-4 tests drive the agent loop with it.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream;
use qbot_context::{AssistantPart, AssistantTurn, OpaqueReasoning, ToolCall};
use qbot_core::CallId;
use serde_json::Value;

use crate::capability::{
    Capabilities, ForcedToolChoice, ProviderId, ProviderInfo, Realization, ReasoningInfo,
};
use crate::conversation::{ConvItem, Conversation};
use crate::error::LlmError;
use crate::provider::{EventStream, Provider, ReplayPlan, collect, plan_replay};
use crate::request::{Request, ToolChoice, validate_request};
use crate::response::{
    CacheUsage, Continuation, FinishReason, Response, ResponseMeta, StreamEvent, Usage,
};

#[derive(Debug, Clone, PartialEq)]
pub enum FakePart {
    Reasoning(String),
    Text(String),
    Call { name: String, arguments: Value },
}

/// What the fake model says in one response.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FakeReply {
    parts: Vec<FakePart>,
    finish: Option<FinishReason>,
}

impl FakeReply {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reasoning(mut self, summary: impl Into<String>) -> Self {
        self.parts.push(FakePart::Reasoning(summary.into()));
        self
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.parts.push(FakePart::Text(text.into()));
        self
    }

    pub fn call(mut self, name: impl Into<String>, arguments: Value) -> Self {
        self.parts.push(FakePart::Call {
            name: name.into(),
            arguments,
        });
        self
    }

    pub fn finish(mut self, reason: FinishReason) -> Self {
        self.finish = Some(reason);
        self
    }

    pub fn parts(&self) -> &[FakePart] {
        &self.parts
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Reply(FakeReply),
    Fail(LlmError),
}

impl From<FakeReply> for Step {
    fn from(reply: FakeReply) -> Self {
        Step::Reply(reply)
    }
}

/// One request as the fake saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub conversation: Conversation,
    pub plan: ReplayPlan,
    pub tool_names: Vec<String>,
    pub tool_choice: ToolChoice,
    pub params: crate::Params,
    pub streamed: bool,
}

#[derive(Default)]
struct Inner {
    script: VecDeque<Step>,
    recorded: Vec<Recorded>,
    next_call: u64,
    next_handle: u64,
    handles: HashMap<String, usize>,
    last_conversation: Option<Conversation>,
}

static INSTANCES: AtomicU64 = AtomicU64::new(1);

pub struct FakeProvider {
    instance: u64,
    info: ProviderInfo,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for FakeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeProvider")
            .field("id", &self.info.id)
            .finish_non_exhaustive()
    }
}

impl FakeProvider {
    /// A stateless provider (full replay every call), like DeepSeek.
    pub fn new(script: impl IntoIterator<Item = Step>) -> Self {
        let info = ProviderInfo {
            id: ProviderId::new("fake"),
            model: "fake-model".to_owned(),
            capabilities: Capabilities {
                streaming: Realization::Native,
                tool_calls: Realization::Native,
                parallel_tool_calls: Realization::Native,
                continuation: Realization::Emulated,
                developer_role: Realization::Native,
                reasoning: ReasoningInfo {
                    produces: true,
                    replay_required: false,
                    visible_summary: true,
                    effort_control: true,
                },
                image_input: None,
                cache_metrics: true,
                forced_tool_choice: ForcedToolChoice::Always,
                temperature: true,
            },
        };
        Self {
            instance: INSTANCES.fetch_add(1, Ordering::Relaxed),
            info,
            inner: Mutex::new(Inner {
                script: script.into_iter().collect(),
                ..Inner::default()
            }),
        }
    }

    /// Make the fake keep server-side state, so continuations produce delta plans.
    /// Declare when forced tool choice is honored, as a real provider would.
    pub fn forced_tool_choice(mut self, forced: ForcedToolChoice) -> Self {
        self.info.capabilities.forced_tool_choice = forced;
        self
    }

    pub fn stateful(mut self) -> Self {
        self.info.capabilities.continuation = Realization::Native;
        self
    }

    pub fn push(&self, step: impl Into<Step>) {
        self.lock().script.push_back(step.into());
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.lock().recorded.clone()
    }

    pub fn remaining_steps(&self) -> usize {
        self.lock().script.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn run(
        &self,
        request: &Request<'_>,
        streamed: bool,
    ) -> Result<(Response, FakeReply), LlmError> {
        validate_request(&self.info, request)?;
        let conversation = request.conversation;
        let mut inner = self.lock();
        let plan = plan_replay(&self.info, conversation, request.continuation);
        if let (Some(cont), true) = (
            request.continuation,
            self.info.capabilities.continuation == Realization::Native,
        ) && let Some(handle) = cont.handle()
            && !inner.handles.contains_key(handle)
        {
            return Err(LlmError::InvalidRequest(format!(
                "unknown continuation handle {handle:?}"
            )));
        }
        inner.recorded.push(Recorded {
            conversation: conversation.clone(),
            plan,
            tool_names: request.tools.iter().map(|t| t.name.clone()).collect(),
            tool_choice: request.tool_choice.clone(),
            params: request.params,
            streamed,
        });
        let step = inner
            .script
            .pop_front()
            .ok_or_else(|| LlmError::Protocol("fake provider script exhausted".into()))?;
        let reply = match step {
            Step::Fail(error) => return Err(error),
            Step::Reply(reply) => reply,
        };

        let mut parts = Vec::new();
        for part in &reply.parts {
            parts.push(match part {
                FakePart::Reasoning(summary) => AssistantPart::Reasoning(OpaqueReasoning {
                    provider: self.info.id.as_str().to_owned(),
                    payload: serde_json::json!({ "fake_reasoning": true }),
                    summary: Some(summary.clone()),
                }),
                FakePart::Text(text) => AssistantPart::Text(text.clone()),
                FakePart::Call { name, arguments } => {
                    if !request.tools.iter().any(|tool| &tool.name == name) {
                        return Err(LlmError::Protocol(format!(
                            "fake script calls undeclared tool {name:?}"
                        )));
                    }
                    inner.next_call += 1;
                    let id = CallId::new(format!("fake-call-{}", inner.next_call))
                        .map_err(|e| LlmError::Protocol(e.to_string()))?;
                    AssistantPart::Call(ToolCall {
                        id,
                        name: name.clone(),
                        arguments: arguments.clone(),
                    })
                }
            });
        }
        if matches!(
            request.tool_choice,
            ToolChoice::Required | ToolChoice::Named(_)
        ) && !parts.iter().any(|p| matches!(p, AssistantPart::Call(_)))
        {
            return Err(LlmError::Protocol(
                "fake script ignored a forced tool choice".into(),
            ));
        }
        let turn = AssistantTurn::new(parts);
        let has_calls = turn.calls().next().is_some();
        let finish = reply.finish.clone().unwrap_or(if has_calls {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        });

        let input_tokens: u32 = conversation.items().iter().map(item_tokens).sum();
        let hit_tokens: u32 = match &inner.last_conversation {
            Some(previous) => common_prefix(previous, conversation)
                .iter()
                .map(item_tokens)
                .sum(),
            None => 0,
        };
        let output_tokens =
            (tokens_of(&turn_json(&turn)) + 1).min(request.params.max_output_tokens);
        inner.last_conversation = Some(conversation.clone());

        let handle = if self.info.capabilities.continuation == Realization::Native {
            inner.next_handle += 1;
            let handle = format!("fake-{}-resp-{}", self.instance, inner.next_handle);
            inner.handles.insert(handle.clone(), conversation.len() + 1);
            Some(handle)
        } else {
            None
        };
        let continuation = Continuation::after(
            self.info.id.clone(),
            self.info.model.clone(),
            conversation,
            &turn,
            handle.clone(),
        );
        let response = Response {
            turn,
            finish,
            usage: Usage {
                input_tokens,
                output_tokens,
                cache: CacheUsage::Reported { hit_tokens },
                reasoning_tokens: Some(0),
                reported: true,
            },
            continuation,
            meta: ResponseMeta {
                model: self.info.model.clone(),
                provider_response_id: handle,
                latency: Duration::ZERO,
                attempts: 1,
            },
        };
        Ok((response, reply))
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    async fn respond(&self, request: Request<'_>) -> Result<Response, LlmError> {
        self.run(&request, false).map(|(response, _)| response)
    }

    async fn stream(&self, request: Request<'_>) -> Result<EventStream, LlmError> {
        let (response, _) = self.run(&request, true)?;
        let mut events: Vec<Result<StreamEvent, LlmError>> = Vec::new();
        for part in response.turn.parts() {
            match part {
                AssistantPart::Reasoning(r) => {
                    if let Some(summary) = &r.summary {
                        events.push(Ok(StreamEvent::ReasoningDelta(summary.clone())));
                    }
                }
                AssistantPart::Text(text) => {
                    for chunk in chunks(text, 8) {
                        events.push(Ok(StreamEvent::TextDelta(chunk)));
                    }
                }
                AssistantPart::Call(call) => {
                    events.push(Ok(StreamEvent::CallStarted {
                        id: call.id.clone(),
                        name: call.name.clone(),
                    }));
                    for fragment in chunks(&call.arguments.to_string(), 6) {
                        events.push(Ok(StreamEvent::CallArguments {
                            id: call.id.clone(),
                            fragment,
                        }));
                    }
                }
            }
        }
        events.push(Ok(StreamEvent::Completed(Box::new(response))));
        Ok(Box::pin(stream::iter(events)))
    }
}

/// Convenience for tests that do not care about streaming.
pub async fn respond_streaming(
    provider: &dyn Provider,
    request: Request<'_>,
) -> Result<Response, LlmError> {
    collect(provider.stream(request).await?).await
}

fn chunks(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(size.max(1))
        .map(|c| c.iter().collect())
        .collect()
}

fn turn_json(turn: &AssistantTurn) -> String {
    serde_json::to_string(turn).unwrap_or_default()
}

fn tokens_of(text: &str) -> u32 {
    u32::try_from(text.len() / 4).unwrap_or(u32::MAX).max(1)
}

fn item_tokens(item: &ConvItem) -> u32 {
    tokens_of(&serde_json::to_string(item).unwrap_or_default())
}

fn common_prefix<'a>(a: &'a Conversation, b: &Conversation) -> &'a [ConvItem] {
    let n = a
        .items()
        .iter()
        .zip(b.items())
        .take_while(|(x, y)| x == y)
        .count();
    &a.items()[..n]
}
