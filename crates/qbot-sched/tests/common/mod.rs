#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::sim::{MemorySink, SimWorld, renderer};
use qbot_agent::{
    Archive, ContextSource, Effect, GroupPolicy, RunDeps, RunLimits, RunLog, Supervisor,
    SupervisorConfig, Tool, ToolCx, ToolError, ToolOutput, ToolSet,
};
use qbot_core::{Clock, GroupId, TimerId, UnixMillis};
use qbot_llm::ReasoningEffort;
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_sched::{
    JobError, JobKind, JobRunner, MemoryTimerStore, Scheduler, SchedulerConfig, TaskLimits,
    TaskService, Timer, TimerState, TokioClock,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const START: i64 = 1_800_000_000_000;

pub fn group() -> GroupId {
    GroupId::new(555).unwrap()
}

pub fn other_group() -> GroupId {
    GroupId::new(777).unwrap()
}

#[derive(Deserialize, JsonSchema)]
pub struct HoldArgs {
    pub ms: u64,
}

/// Occupies a run for a while, so capacity and per-group exclusion can be observed.
pub struct Hold;

#[async_trait]
impl Tool for Hold {
    type Args = HoldArgs;
    const NAME: &'static str = "hold";
    fn description(&self) -> String {
        "Wait".into()
    }
    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![("ms", "How long to wait, in milliseconds.".into())]
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    async fn call(&self, _: &ToolCx<'_>, args: HoldArgs) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(Duration::from_millis(args.ms)).await;
        Ok(ToolOutput::text("held"))
    }
}

pub fn hold(ms: u64) -> Step {
    Step::Reply(FakeReply::new().call("hold", json!({ "ms": ms })))
}

pub fn done() -> Step {
    Step::Reply(FakeReply::new().text("done"))
}

/// A job runner whose results are scripted; records every run.
#[derive(Default)]
pub struct ScriptedJobs {
    pub results: Mutex<VecDeque<Result<(), JobError>>>,
    pub runs: Mutex<Vec<(JobKind, Option<GroupId>, UnixMillis)>>,
    pub clock: Mutex<Option<Arc<dyn Clock>>>,
    pub hold: Mutex<Duration>,
}

#[async_trait]
impl JobRunner for ScriptedJobs {
    async fn run(&self, kind: JobKind, group: Option<GroupId>) -> Result<(), JobError> {
        let now = self
            .clock
            .lock()
            .unwrap()
            .as_ref()
            .map_or(UnixMillis::new(0), |c| c.now());
        self.runs.lock().unwrap().push((kind, group, now));
        let hold = *self.hold.lock().unwrap();
        if !hold.is_zero() {
            tokio::time::sleep(hold).await;
        }
        self.results.lock().unwrap().pop_front().unwrap_or(Ok(()))
    }
}

pub struct Rig {
    pub world: Arc<SimWorld>,
    pub fake: Arc<FakeProvider>,
    pub sink: Arc<MemorySink>,
    pub store: Arc<MemoryTimerStore>,
    pub clock: Arc<TokioClock>,
    pub service: TaskService,
    pub supervisor: Supervisor,
    pub scheduler: Arc<Scheduler>,
    pub jobs: Arc<ScriptedJobs>,
    pub shutdown: CancellationToken,
}

pub fn rig(script: Vec<Step>, capacity: usize) -> Rig {
    rig_with(script, capacity, SchedulerConfig::default())
}

pub fn rig_with(script: Vec<Step>, capacity: usize, cfg: SchedulerConfig) -> Rig {
    let world = SimWorld::new(group());
    let fake = Arc::new(FakeProvider::new(script));
    let sink = MemorySink::new();
    let clock = Arc::new(TokioClock::new(UnixMillis::new(START)));
    let deps = Arc::new(RunDeps {
        provider: fake.clone(),
        tools: ToolSet::new().with(Hold).unwrap(),
        archive: world.clone() as Arc<dyn Archive>,
        log: world.clone() as Arc<dyn RunLog>,
        sink: sink.clone(),
        renderer: renderer(),
        clock: clock.clone(),
        limits: RunLimits::default(),
        reasoning: ReasoningEffort::Low,
        media: None,
    });
    let supervisor = Supervisor::new(
        SupervisorConfig {
            capacity,
            concurrency: 8,
            reply_deadline: Duration::from_secs(3600),
        },
        deps,
        world.clone() as Arc<dyn ContextSource>,
        world.clone() as Arc<dyn GroupPolicy>,
    );
    let store = Arc::new(MemoryTimerStore::new());
    let wake = Arc::new(Notify::new());
    let service = TaskService::new(
        store.clone(),
        clock.clone(),
        TaskLimits::default(),
        wake.clone(),
    );
    let jobs = Arc::new(ScriptedJobs::default());
    *jobs.clock.lock().unwrap() = Some(clock.clone());
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        supervisor.clone(),
        clock.clone(),
        jobs.clone(),
        cfg,
        wake,
    ));
    Rig {
        world,
        fake,
        sink,
        store,
        clock,
        service,
        supervisor,
        scheduler,
        jobs,
        shutdown: CancellationToken::new(),
    }
}

impl Rig {
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let scheduler = self.scheduler.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move { scheduler.run(shutdown).await })
    }

    pub async fn state(&self, id: TimerId) -> TimerState {
        use qbot_sched::TimerStore;
        self.store.get(id).await.unwrap().unwrap().state
    }

    pub async fn timer(&self, id: TimerId) -> Timer {
        use qbot_sched::TimerStore;
        self.store.get(id).await.unwrap().unwrap()
    }
}

pub fn mins(n: u64) -> Duration {
    Duration::from_secs(n * 60)
}
