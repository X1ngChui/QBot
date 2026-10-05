#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::sim::{MemorySink, SimWorld, renderer};
use qbot_agent::{
    Archive, ContextSource, Delivery, Effect, GroupPolicy, OutSegment, RunDeps, RunInput,
    RunLimits, RunLog, RunReport, Supervisor, SupervisorConfig, Tool, ToolCx, ToolError,
    ToolOutput, ToolSet, Trigger,
};
use qbot_core::{AccountId, GroupId, MessageId, SystemClock};
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_llm::{
    EventStream, LlmError, Params, Provider, ProviderInfo, ReasoningEffort, Request, Response,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub fn group() -> GroupId {
    GroupId::new(555).unwrap()
}

/// Records the order in which tools start, to assert scheduling.
pub type Order = Arc<Mutex<Vec<String>>>;

#[derive(Deserialize, JsonSchema)]
pub struct EchoArgs {
    pub text: String,
}

pub struct Echo(pub Order);

#[async_trait]
impl Tool for Echo {
    type Args = EchoArgs;
    const NAME: &'static str = "echo";
    fn description(&self) -> String {
        "Return the text".into()
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    async fn call(&self, _cx: &ToolCx<'_>, args: EchoArgs) -> Result<ToolOutput, ToolError> {
        self.0.lock().unwrap().push(format!("echo:{}", args.text));
        Ok(ToolOutput::text(format!("echo: {}", args.text)))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct SlowArgs {
    pub ms: u64,
}

pub struct Slow(pub Order);

#[async_trait]
impl Tool for Slow {
    type Args = SlowArgs;
    const NAME: &'static str = "slow";
    fn description(&self) -> String {
        "Sleep".into()
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    async fn call(&self, _cx: &ToolCx<'_>, args: SlowArgs) -> Result<ToolOutput, ToolError> {
        self.0.lock().unwrap().push(format!("slow:{}", args.ms));
        tokio::time::sleep(Duration::from_millis(args.ms)).await;
        Ok(ToolOutput::text("slept"))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct SayArgs {
    pub text: Option<String>,
    pub dice: Option<bool>,
    pub end_turn: Option<bool>,
}

/// A send tool over the `Delivery` port: claims a send slot, delivers, waits for the echo and
/// reports the echoed text, which is how random results become visible.
pub struct Say {
    pub delivery: Arc<dyn Delivery>,
    pub order: Order,
    pub max_sends: u32,
}

#[async_trait]
impl Tool for Say {
    type Args = SayArgs;
    const NAME: &'static str = "say";
    fn description(&self) -> String {
        "Send a message".into()
    }
    fn effect(&self) -> Effect {
        Effect::Send
    }
    async fn call(&self, cx: &ToolCx<'_>, args: SayArgs) -> Result<ToolOutput, ToolError> {
        self.order.lock().unwrap().push("say".into());
        if !cx.state.try_claim_send(self.max_sends) {
            return Err(ToolError::refused(
                qbot_context::RefusalReason::LimitReached,
                "send limit reached",
            ));
        }
        let segments = if args.dice == Some(true) {
            vec![OutSegment::Dice]
        } else {
            vec![OutSegment::Text(args.text.unwrap_or_default())]
        };
        let delivered = self
            .delivery
            .send(cx.group, segments)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        cx.state.note_observed(delivered.echo.message);
        Ok(ToolOutput::text(format!(
            "sent [msg:{}] {}",
            delivered.echo.message.get(),
            delivered.echo.text
        ))
        .ends_run(args.end_turn.unwrap_or(true)))
    }
}

pub struct Harness {
    pub world: Arc<SimWorld>,
    pub sink: Arc<MemorySink>,
    pub fake: Arc<FakeProvider>,
    pub deps: Arc<RunDeps>,
    pub order: Order,
}

pub fn params() -> Params {
    Params {
        max_output_tokens: 1000,
        reasoning: ReasoningEffort::Low,
        temperature: None,
    }
}

pub fn tool_set(world: &Arc<SimWorld>, order: &Order, max_sends: u32) -> ToolSet {
    ToolSet::new()
        .with(Echo(order.clone()))
        .unwrap()
        .with(Slow(order.clone()))
        .unwrap()
        .with(Say {
            delivery: world.clone(),
            order: order.clone(),
            max_sends,
        })
        .unwrap()
}

pub fn harness(script: Vec<Step>) -> Harness {
    harness_with(script, RunLimits::default(), false, 4)
}

pub fn harness_with(
    script: Vec<Step>,
    limits: RunLimits,
    stateful: bool,
    max_sends: u32,
) -> Harness {
    let fake = if stateful {
        FakeProvider::new(script).stateful()
    } else {
        FakeProvider::new(script)
    };
    harness_from(fake, limits, max_sends)
}

/// A harness around a provider that was set up by the caller.
pub fn harness_from(fake: FakeProvider, limits: RunLimits, max_sends: u32) -> Harness {
    let world = SimWorld::new(group());
    let order: Order = Arc::default();
    let fake = Arc::new(fake);
    let sink = MemorySink::new();
    let deps = Arc::new(RunDeps {
        provider: fake.clone(),
        tools: tool_set(&world, &order, max_sends),
        archive: world.clone() as Arc<dyn Archive>,
        log: world.clone() as Arc<dyn RunLog>,
        sink: sink.clone(),
        renderer: renderer(),
        clock: Arc::new(SystemClock),
        limits,
        params: params(),
        media: None,
    });
    Harness {
        world,
        sink,
        fake,
        deps,
        order,
    }
}

pub fn addressed(sender: i64, message: i64) -> Trigger {
    Trigger::Addressed {
        message: MessageId::new(message).unwrap(),
        sender: AccountId::new(sender).unwrap(),
    }
}

/// Run one run directly (no supervisor) with a generous deadline.
pub async fn run_once(h: &Harness, trigger: Trigger) -> RunReport {
    run_with_deadline(
        h,
        trigger,
        Duration::from_secs(600),
        &CancellationToken::new(),
    )
    .await
}

pub async fn run_with_deadline(
    h: &Harness,
    trigger: Trigger,
    within: Duration,
    cancel: &CancellationToken,
) -> RunReport {
    let context = h.world.open(group(), &trigger).await.unwrap();
    let run = h.deps.log.begin(group(), &trigger).await.unwrap();
    let input = RunInput {
        run,
        group: group(),
        trigger,
        context,
        deadline: Instant::now() + within,
    };
    qbot_agent::execute(&h.deps, &input, cancel).await
}

pub fn supervisor(h: &Harness, cfg: SupervisorConfig) -> Supervisor {
    Supervisor::new(
        cfg,
        h.deps.clone(),
        h.world.clone() as Arc<dyn ContextSource>,
        h.world.clone() as Arc<dyn GroupPolicy>,
    )
}

pub fn cfg(capacity: usize, concurrency: usize, deadline_secs: u64) -> SupervisorConfig {
    SupervisorConfig {
        capacity,
        concurrency,
        reply_deadline: Duration::from_secs(deadline_secs),
    }
}

pub fn say(text: &str) -> FakeReply {
    FakeReply::new().call("say", json!({ "text": text }))
}

pub fn reply_ok(reply: FakeReply) -> Step {
    Step::Reply(reply)
}

/// A provider that never answers.
pub struct Hang(pub ProviderInfo);

#[async_trait]
impl Provider for Hang {
    fn info(&self) -> &ProviderInfo {
        &self.0
    }
    async fn respond(&self, _: Request<'_>) -> Result<Response, LlmError> {
        std::future::pending().await
    }
    async fn stream(&self, _: Request<'_>) -> Result<EventStream, LlmError> {
        std::future::pending().await
    }
}
