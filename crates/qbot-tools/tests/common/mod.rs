#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use qbot_agent::sim::{MemorySink, SimWorld, renderer};
use qbot_agent::{
    Archive, ContextSource, Delivery, GroupPolicy, RunDeps, RunLimits, RunLog, RunReport,
    Supervisor, SupervisorConfig, Trigger, TriggerRequest,
};
use qbot_context::{Item, ToolResult};
use qbot_core::{AccountId, GroupId, MessageId, TimerId, UnixMillis};
use qbot_llm::ReasoningEffort;
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_sched::{
    JobError, JobKind, JobRunner, MemoryTimerStore, Scheduler, SchedulerConfig, TaskLimits,
    TaskService, Timer, TimerStore, TokioClock,
};
use qbot_tools::standard_tools;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const START: i64 = 1_800_000_000_000; // 2027-01-15T08:00:00Z

pub fn group() -> GroupId {
    GroupId::new(555).unwrap()
}

pub fn other_group() -> GroupId {
    GroupId::new(777).unwrap()
}

struct NoJobs;

#[async_trait::async_trait]
impl JobRunner for NoJobs {
    async fn run(&self, _: JobKind, _: Option<GroupId>) -> Result<(), JobError> {
        Ok(())
    }
}

pub struct Rig {
    pub world: Arc<SimWorld>,
    pub fake: Arc<FakeProvider>,
    pub store: Arc<MemoryTimerStore>,
    pub service: TaskService,
    pub supervisor: Supervisor,
    pub scheduler: Arc<Scheduler>,
    pub shutdown: CancellationToken,
}

pub fn rig(script: Vec<Step>) -> Rig {
    let world = SimWorld::new(group());
    let fake = Arc::new(FakeProvider::new(script));
    let clock = Arc::new(TokioClock::new(UnixMillis::new(START)));
    let store = Arc::new(MemoryTimerStore::new());
    let wake = Arc::new(Notify::new());
    let service = TaskService::new(
        store.clone(),
        clock.clone(),
        TaskLimits::default(),
        wake.clone(),
    );
    let tools = standard_tools(
        world.clone() as Arc<dyn Delivery>,
        world.clone() as Arc<dyn Archive>,
        service.clone(),
        qbot_tools::ToolSettings {
            max_sends_per_run: 4,
            search: qbot_tools::SearchSettings {
                default_limit: 8,
                max_limit: 50,
            },
        },
    )
    .unwrap();
    let deps = Arc::new(RunDeps {
        provider: fake.clone(),
        tools,
        archive: world.clone() as Arc<dyn Archive>,
        log: world.clone() as Arc<dyn RunLog>,
        sink: MemorySink::new(),
        renderer: renderer(),
        clock: clock.clone(),
        limits: RunLimits::default(),
        reasoning: ReasoningEffort::Low,
        media: None,
    });
    let supervisor = Supervisor::new(
        SupervisorConfig {
            capacity: 8,
            concurrency: 4,
            reply_deadline: Duration::from_secs(3600),
        },
        deps,
        world.clone() as Arc<dyn ContextSource>,
        world.clone() as Arc<dyn GroupPolicy>,
    );
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        supervisor.clone(),
        clock,
        Arc::new(NoJobs),
        SchedulerConfig::default(),
        wake,
    ));
    Rig {
        world,
        fake,
        store,
        service,
        supervisor,
        scheduler,
        shutdown: CancellationToken::new(),
    }
}

impl Rig {
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let (scheduler, shutdown) = (self.scheduler.clone(), self.shutdown.clone());
        tokio::spawn(async move { scheduler.run(shutdown).await })
    }

    /// Run one addressed trigger to completion.
    pub async fn ask(&self, sender: i64, number: u32, text: &str) -> RunReport {
        let line = self.world.say(sender, number, text);
        let request = TriggerRequest {
            group: group(),
            trigger: Trigger::Addressed {
                message: line.message,
                sender: AccountId::new(sender).unwrap(),
            },
        };
        self.supervisor
            .submit(request)
            .await
            .unwrap()
            .finished()
            .await
            .unwrap()
    }

    pub async fn timer(&self, id: u64) -> Timer {
        self.store.get(TimerId::new(id)).await.unwrap().unwrap()
    }

    /// Tool results of the most recently started run, from the durable run log.
    pub fn last_run_results(&self) -> Vec<ToolResult> {
        let run = *self.world.logged_runs().last().unwrap();
        results_of(
            &self
                .world
                .logged(run)
                .into_iter()
                .map(|(_, i)| i)
                .collect::<Vec<_>>(),
        )
    }
}

pub fn results_of(items: &[Item]) -> Vec<ToolResult> {
    items
        .iter()
        .filter_map(|i| {
            if let Item::ToolResult(r) = i {
                Some(r.clone())
            } else {
                None
            }
        })
        .collect()
}

pub fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|p| {
            if let qbot_context::Part::Text(t) = p {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect()
}

pub fn call(tool: &str, args: Value) -> FakeReply {
    FakeReply::new().call(tool, args)
}

pub fn reply(r: FakeReply) -> Step {
    Step::Reply(r)
}

pub fn done() -> Step {
    Step::Reply(FakeReply::new().text("done"))
}

pub fn send(text: &str) -> Value {
    json!({ "text": text, "end_turn": false })
}

pub fn msg(id: i64) -> MessageId {
    MessageId::new(id).unwrap()
}
