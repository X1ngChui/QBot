#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The whole stack on Postgres: archive, policy, run log, usage, timers, scheduler, supervisor,
//! tools and the fake provider. Only the platform (delivery) and the prompt are stand-ins.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use qbot_agent::{
    Archive, ContextSource, Delivered, Delivery, DeliveryError, EnvError, GroupPolicy,
    OpenedContext, OutSegment, Rejected, RunDeps, RunLimits, RunLog, Supervisor, SupervisorConfig,
    Trigger, TriggerRequest,
};
use qbot_context::{
    AssistantPart, AssistantTurn, ChatBatch, Instruction, InstructionRole, Item, Outcome, RunEnd,
    Speaker, ToolCall,
};
use qbot_core::{AccountId, CallId, Clock, GroupId, MessageId, SystemClock, TimerId, UnixMillis};
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_llm::{Params, PlainRenderer, ReasoningEffort};
use qbot_sched::{
    JobError, JobKind, JobRunner, NewWake, Origin, Scheduler, SchedulerConfig, TaskLimits,
    TaskService, TimerOutcome, TimerState, TimerStore,
};
use qbot_store::{
    Appended, NewLine, NewSpeaker, PgArchive, PgGroupPolicy, PgRunLog, PgTimerStore, PgUsageSink,
    UsageRecorder,
};
use serde_json::json;
use sqlx::Row;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn group() -> GroupId {
    GroupId::new(555).unwrap()
}

/// The platform stand-in: a sent message is echoed back into the archive.
struct PgDelivery {
    archive: PgArchive,
    next: AtomicI64,
}

#[async_trait]
impl Delivery for PgDelivery {
    async fn send(
        &self,
        group: GroupId,
        segments: Vec<OutSegment>,
    ) -> Result<Delivered, DeliveryError> {
        let text: String = segments
            .iter()
            .map(|s| match s {
                OutSegment::Text(t) => t.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        let message = MessageId::new(self.next.fetch_add(1, Ordering::SeqCst)).unwrap();
        let line = NewLine {
            group,
            message,
            speaker: NewSpeaker::Bot,
            at: SystemClock.now(),
            text,
        };
        match self
            .archive
            .append(line)
            .await
            .map_err(|e| DeliveryError::Unavailable(e.to_string()))?
        {
            Appended::Stored { line, .. } => Ok(Delivered { echo: line }),
            Appended::Duplicate => Err(DeliveryError::EchoMissing),
        }
    }
}

/// The prompt stand-in: stable instructions plus the recent window from the archive.
struct PgContext {
    archive: PgArchive,
}

#[async_trait]
impl ContextSource for PgContext {
    async fn open(&self, group: GroupId, trigger: &Trigger) -> Result<OpenedContext, EnvError> {
        let (window, cursor) = self
            .archive
            .recent(group, 50)
            .await
            .map_err(|e| EnvError(e.to_string()))?;
        let instruction = |role, text: &str| Instruction {
            role,
            text: text.into(),
            template_hash: "test".into(),
        };
        Ok(OpenedContext {
            instructions: vec![instruction(
                InstructionRole::System,
                "You are a group member.",
            )],
            window,
            trigger_note: match trigger {
                Trigger::Wake { intent, .. } => Some(instruction(
                    InstructionRole::Trigger,
                    &format!("task intent: {intent}"),
                )),
                Trigger::Addressed { .. } => None,
            },
            cursor,
            recaps: Vec::new(),
            undelivered_note: None,
        })
    }
}

struct NoJobs;

#[async_trait]
impl JobRunner for NoJobs {
    async fn run(&self, _: JobKind, _: Option<GroupId>) -> Result<(), JobError> {
        Ok(())
    }
}

struct Stack {
    db: TestDb,
    archive: PgArchive,
    policy: PgGroupPolicy,
    log: PgRunLog,
    timers: Arc<PgTimerStore>,
    fake: Arc<FakeProvider>,
    supervisor: Supervisor,
    scheduler: Arc<Scheduler>,
    service: TaskService,
    recorder: Option<UsageRecorder>,
    shutdown: CancellationToken,
}

fn stack_on(db: TestDb, script: Vec<Step>) -> Stack {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let pool = db.pool().clone();
    let archive = PgArchive::new(pool.clone(), clock.clone());
    let policy = PgGroupPolicy::new(pool.clone(), clock.clone());
    let log = PgRunLog::new(pool.clone(), clock.clone());
    let timers = Arc::new(PgTimerStore::new(pool.clone(), clock.clone()));
    let recorder = PgUsageSink::start(pool, clock.clone());
    let wake = Arc::new(Notify::new());
    let service = TaskService::new(
        timers.clone(),
        clock.clone(),
        TaskLimits::default(),
        wake.clone(),
    );
    let delivery = Arc::new(PgDelivery {
        archive: archive.clone(),
        next: AtomicI64::new(1_000_000),
    });
    let tools = qbot_tools::standard_tools(
        delivery,
        Arc::new(archive.clone()) as Arc<dyn Archive>,
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
    let fake = Arc::new(FakeProvider::new(script));
    let deps = Arc::new(RunDeps {
        provider: fake.clone(),
        tools,
        archive: Arc::new(archive.clone()),
        log: Arc::new(log.clone()) as Arc<dyn RunLog>,
        sink: recorder.sink(),
        renderer: Arc::new(PlainRenderer),
        clock: clock.clone(),
        limits: RunLimits::default(),
        params: Params {
            max_output_tokens: 1000,
            reasoning: ReasoningEffort::Low,
            temperature: None,
        },
        media: None,
    });
    let supervisor = Supervisor::new(
        SupervisorConfig {
            capacity: 8,
            concurrency: 4,
            reply_deadline: Duration::from_secs(60),
        },
        deps,
        Arc::new(PgContext {
            archive: archive.clone(),
        }) as Arc<dyn ContextSource>,
        Arc::new(policy.clone()) as Arc<dyn GroupPolicy>,
    );
    let scheduler = Arc::new(Scheduler::new(
        timers.clone(),
        supervisor.clone(),
        clock,
        Arc::new(NoJobs),
        SchedulerConfig::default(),
        wake,
    ));
    Stack {
        db,
        archive,
        policy,
        log,
        timers,
        fake,
        supervisor,
        scheduler,
        service,
        recorder: Some(recorder),
        shutdown: CancellationToken::new(),
    }
}

async fn stack(script: Vec<Step>) -> Option<Stack> {
    TestDb::try_new().await.map(|db| stack_on(db, script))
}

impl Stack {
    async fn say(&self, message: i64, account: i64, text: &str) -> MessageId {
        let id = MessageId::new(message).unwrap();
        let line = NewLine {
            group: group(),
            message: id,
            speaker: NewSpeaker::Member(AccountId::new(account).unwrap()),
            at: SystemClock.now(),
            text: text.into(),
        };
        assert!(matches!(
            self.archive.append(line).await.unwrap(),
            Appended::Stored { .. }
        ));
        id
    }

    async fn ask(
        &self,
        message: MessageId,
        account: i64,
    ) -> Result<qbot_agent::RunReport, Rejected> {
        let request = TriggerRequest {
            group: group(),
            trigger: Trigger::Addressed {
                message,
                sender: AccountId::new(account).unwrap(),
            },
        };
        Ok(self
            .supervisor
            .submit(request)
            .await?
            .finished()
            .await
            .unwrap())
    }

    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    async fn finish(mut self) {
        self.shutdown.cancel();
        if let Some(recorder) = self.recorder.take() {
            recorder.shutdown().await;
        }
        self.db.drop_db().await;
    }
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn send(text: &str) -> Step {
    Step::Reply(FakeReply::new().call("send_message", json!({ "text": text })))
}

#[tokio::test]
async fn a_run_is_recorded_completely_on_postgres() {
    let Some(mut s) = stack(vec![send("hello back")]).await else {
        return;
    };
    let hello = s.say(1, 501, "hello bot").await;
    let report = s.ask(hello, 501).await.unwrap();
    assert_eq!(report.end, RunEnd::Delivered);

    // The echo of the bot's own message is in the archive.
    let (lines, _) = s.archive.recent(group(), 10).await.unwrap();
    assert_eq!(lines.len(), 2);
    assert!(matches!(lines[1].speaker, Speaker::Bot) && lines[1].text == "hello back");

    // The durable record is the exact transcript, and the run row summarizes it.
    assert_eq!(
        s.log.load_transcript(report.run).await.unwrap(),
        report.transcript
    );
    let record = s.log.record(report.run).await.unwrap().unwrap();
    assert_eq!(
        (record.end, record.turns, record.tool_calls, record.sends),
        (Some(RunEnd::Delivered), Some(1), Some(1), Some(1))
    );
    assert!(record.input_tokens.unwrap() > 0);

    // Usage rows land once the writer is flushed.
    s.recorder.take().unwrap().shutdown().await;
    let kinds: Vec<(String, i64)> =
        sqlx::query("SELECT kind, count(*) AS n FROM usage_event GROUP BY kind ORDER BY kind")
            .fetch_all(s.db.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get("kind"), r.get("n")))
            .collect();
    assert_eq!(
        kinds,
        [
            ("model".to_owned(), 1),
            ("run".to_owned(), 1),
            ("tool".to_owned(), 1)
        ]
    );
    let daily = sqlx::query("SELECT calls, input_tokens FROM usage_model_daily")
        .fetch_one(s.db.pool())
        .await
        .unwrap();
    assert_eq!(daily.get::<i64, _>("calls"), 1);
    s.finish().await;
}

#[tokio::test]
async fn a_blocked_member_never_creates_a_run_but_stays_in_context() {
    let Some(s) = stack(vec![send("hi")]).await else {
        return;
    };
    s.policy
        .block(group(), AccountId::new(900).unwrap(), None)
        .await
        .unwrap();
    let spam = s.say(1, 900, "buy my stuff").await;
    assert_eq!(s.ask(spam, 900).await.unwrap_err(), Rejected::Blocked);
    assert_eq!(s.count("run").await, 0, "no run, no transcript, no row");
    assert!(s.fake.recorded().is_empty());

    let question = s.say(2, 501, "what was that?").await;
    s.ask(question, 501).await.unwrap();
    let prompt = format!("{:?}", s.fake.recorded()[0].conversation);
    assert!(
        prompt.contains("member:1 (blocked: do not reply): buy my stuff"),
        "{prompt}"
    );
    s.finish().await;
}

#[tokio::test]
async fn muting_stops_admission_and_a_task_wakes_the_agent_once_it_is_due() {
    let Some(s) = stack(vec![Step::Reply(FakeReply::new().text("nothing to add"))]).await else {
        return;
    };
    s.say(1, 501, "remember the meeting").await;
    // Bypass the minimum delay the way an owner tool would not: insert a task that is due now.
    let task = s
        .timers
        .insert_wake(
            SystemClock.now(),
            NewWake {
                group: group(),
                intent: "ask about the meeting".into(),
                chain: None,
                origin: Origin::Owner,
            },
            50,
        )
        .await
        .unwrap();

    s.policy.set_muted(group(), true).await.unwrap();
    assert_eq!(
        s.ask(MessageId::new(1).unwrap(), 501).await.unwrap_err(),
        Rejected::Muted
    );
    s.policy.set_muted(group(), false).await.unwrap();

    let (scheduler, shutdown) = (s.scheduler.clone(), s.shutdown.clone());
    let handle = tokio::spawn(async move { scheduler.run(shutdown).await });
    let timers = s.timers.clone();
    eventually("the task to finish", || {
        let timers = timers.clone();
        async move {
            matches!(
                timers.get(task.id).await.unwrap().unwrap().state,
                TimerState::Done(_)
            )
        }
    })
    .await;
    assert_eq!(
        s.timers.get(task.id).await.unwrap().unwrap().state,
        TimerState::Done(TimerOutcome::Ran(RunEnd::Completed))
    );
    assert!(
        format!("{:?}", s.fake.recorded()[0].conversation)
            .contains("task intent: ask about the meeting")
    );
    let run = sqlx::query("SELECT trigger_kind, trigger FROM run")
        .fetch_one(s.db.pool())
        .await
        .unwrap();
    assert_eq!(run.get::<String, _>("trigger_kind"), "wake");
    assert_eq!(
        run.get::<serde_json::Value, _>("trigger")["intent"],
        "ask about the meeting"
    );
    s.shutdown.cancel();
    handle.await.unwrap();
    s.finish().await;
}

#[tokio::test]
async fn after_a_crash_claimed_tasks_are_interrupted_and_open_runs_are_closed_never_replayed() {
    // Process one: a task is claimed and a run is in flight with an unanswered tool call.
    let Some(db) = TestDb::try_new().await else {
        return;
    };
    let before = stack_on(db, vec![]);
    let task = before
        .timers
        .insert_wake(
            UnixMillis::new(1),
            NewWake {
                group: group(),
                intent: "was running".into(),
                chain: None,
                origin: Origin::Model,
            },
            50,
        )
        .await
        .unwrap();
    before
        .timers
        .claim_due_wake(SystemClock.now(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    let run = before
        .log
        .begin(
            group(),
            &Trigger::Wake {
                timer: task.id,
                intent: "was running".into(),
                chain: qbot_agent::Chain {
                    id: qbot_core::ChainId::new(task.id.get()),
                    depth: 0,
                },
            },
        )
        .await
        .unwrap();
    let call = Item::Assistant(AssistantTurn::new(vec![AssistantPart::Call(ToolCall {
        id: CallId::new("c1").unwrap(),
        name: "send_message".into(),
        arguments: json!({}),
    })]));
    before
        .log
        .append(
            group(),
            run,
            qbot_core::ItemSeq::new(0),
            &Item::Chat(ChatBatch::new(vec![])),
        )
        .await
        .unwrap();
    before
        .log
        .append(group(), run, qbot_core::ItemSeq::new(1), &call)
        .await
        .unwrap();
    let Stack {
        db,
        shutdown,
        recorder,
        ..
    } = before;
    shutdown.cancel();
    recorder.unwrap().shutdown().await;

    // Process two: same database, fresh objects, a provider that would answer if it were called.
    let after = stack_on(
        db,
        vec![Step::Reply(FakeReply::new().text("should never run"))],
    );
    let recovery = after.timers.recover().await.unwrap();
    let closed = after.log.recover_open_runs().await.unwrap();
    assert_eq!((recovery.interrupted, recovery.requeued, closed), (1, 0, 1));
    assert_eq!(
        after.timers.get(task.id).await.unwrap().unwrap().state,
        TimerState::Interrupted
    );
    let transcript = after.log.load_transcript(run).await.unwrap();
    assert!(transcript.is_quiescent());
    assert!(
        transcript
            .items()
            .iter()
            .any(|i| matches!(i, Item::ToolResult(r) if r.outcome == Outcome::Interrupted))
    );

    let (scheduler, shutdown) = (after.scheduler.clone(), after.shutdown.clone());
    let handle = tokio::spawn(async move { scheduler.run(shutdown).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        after.fake.recorded().is_empty(),
        "the interrupted task was not replayed"
    );
    assert_eq!(
        after.timers.get(task.id).await.unwrap().unwrap().state,
        TimerState::Interrupted
    );
    after.shutdown.cancel();
    handle.await.unwrap();
    let _ = (&after.service, TimerId::new(0));
    after.finish().await;
}

#[tokio::test]
async fn run_ids_keep_increasing_across_a_restart() {
    let Some(db) = TestDb::try_new().await else {
        return;
    };
    let first = stack_on(db, vec![Step::Reply(FakeReply::new().text("one"))]);
    let m = first.say(1, 501, "hello").await;
    let a = first.ask(m, 501).await.unwrap().run;
    let Stack {
        db,
        shutdown,
        recorder,
        ..
    } = first;
    shutdown.cancel();
    recorder.unwrap().shutdown().await;

    let second = stack_on(db, vec![Step::Reply(FakeReply::new().text("two"))]);
    let m = second.say(2, 501, "hello again").await;
    let b = second.ask(m, 501).await.unwrap().run;
    assert!(
        b.get() > a.get(),
        "ids come from the database, so a restart cannot reuse one"
    );
    second.finish().await;
}
