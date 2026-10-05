#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use qbot_agent::{Rejected, Trigger, TriggerRequest};
use qbot_context::RunEnd;
use qbot_core::TimerId;
use qbot_llm::fake::{FakeReply, Step};
use qbot_llm::{ConvItem, LlmError};
use qbot_sched::{
    JobError, JobKind, Origin, SchedulerConfig, TimerOutcome, TimerState, TimerStore, When,
};
use tokio::time::sleep;

const SETTLE: Duration = Duration::from_secs(1);

async fn wake_in(r: &Rig, intent: &str, minutes: u64) -> TimerId {
    r.service
        .create(
            group(),
            intent,
            When::After(mins(minutes)),
            None,
            Origin::Model,
        )
        .await
        .unwrap()
        .id
}

#[tokio::test(start_paused = true)]
async fn a_task_fires_at_its_due_time_with_its_intent_and_fresh_context() {
    let r = rig(vec![done()], 4);
    r.world.say(1, 1, "earlier chat");
    let id = wake_in(&r, "ask how the build went", 10).await;
    let handle = r.start();

    sleep(mins(10) - Duration::from_secs(1)).await;
    assert_eq!(
        r.state(id).await,
        TimerState::Pending,
        "not before it is due"
    );
    sleep(mins(1)).await;
    sleep(SETTLE).await;

    assert_eq!(
        r.state(id).await,
        TimerState::Done(TimerOutcome::Ran(RunEnd::Completed))
    );
    let conv = &r.fake.recorded()[0].conversation;
    let text = format!("{conv:?}");
    assert!(
        text.contains("ask how the build went"),
        "the stored intent reaches the model"
    );
    assert!(
        text.contains("earlier chat"),
        "and so does the current chat"
    );
    assert!(
        conv.items().iter().any(|i| matches!(i, ConvItem::Message(m) if m.role == qbot_llm::Role::User && format!("{m:?}").contains("stored intent"))),
        "the intent is lower-trust user-level content"
    );
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn updating_and_cancelling_replan_the_scheduler() {
    let r = rig(vec![done()], 4);
    let moved = wake_in(&r, "moved", 10).await;
    let cancelled = wake_in(&r, "cancelled", 11).await;
    let handle = r.start();
    sleep(SETTLE).await;

    r.service
        .update(group(), moved, None, Some(When::After(mins(20))))
        .await
        .unwrap();
    r.service.cancel(group(), cancelled).await.unwrap();
    sleep(mins(12)).await;
    assert_eq!(
        r.state(moved).await,
        TimerState::Pending,
        "the earlier time no longer applies"
    );
    assert_eq!(r.state(cancelled).await, TimerState::Cancelled);
    assert!(r.fake.recorded().is_empty());

    sleep(mins(10)).await;
    assert!(matches!(r.state(moved).await, TimerState::Done(_)));
    assert_eq!(r.fake.recorded().len(), 1);
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn without_capacity_a_due_task_stays_pending_and_is_never_claimed() {
    let r = rig(vec![hold(600_000), done()], 1);
    let first = wake_in(&r, "first", 10).await;
    let second = wake_in(&r, "second", 10).await;
    // Different groups would be independent; use two groups so exclusion is not the reason.
    let _ = (first, second);
    let handle = r.start();

    sleep(mins(10) + SETTLE).await;
    let states = [r.state(first).await, r.state(second).await];
    let claimed = states
        .iter()
        .filter(|s| matches!(s, TimerState::Claimed { .. }))
        .count();
    let pending = states.iter().filter(|s| s.is_pending()).count();
    assert_eq!(
        (claimed, pending),
        (1, 1),
        "capacity 1: one runs, the other waits unclaimed: {states:?}"
    );
    assert_eq!(r.supervisor.active(), 1);

    sleep(mins(11)).await;
    assert!(matches!(r.state(first).await, TimerState::Done(_)));
    assert!(
        matches!(r.state(second).await, TimerState::Done(_)),
        "it fired once capacity was freed"
    );
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn one_task_per_group_is_in_flight_at_a_time() {
    let r = rig(vec![hold(300_000), done(), done()], 8);
    let a = wake_in(&r, "a", 10).await;
    let b = wake_in(&r, "b", 10).await;
    let handle = r.start();

    sleep(mins(10) + SETTLE).await;
    let (sa, sb) = (r.state(a).await, r.state(b).await);
    assert!(
        matches!(sa, TimerState::Claimed { .. }) && sb.is_pending(),
        "{sa:?} {sb:?}"
    );
    assert_eq!(r.fake.recorded().len(), 1);

    sleep(mins(6)).await;
    assert!(matches!(r.state(a).await, TimerState::Done(_)));
    assert!(matches!(r.state(b).await, TimerState::Done(_)));
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_muted_group_skips_the_task_without_calling_the_model() {
    let r = rig(vec![done()], 4);
    let id = wake_in(&r, "speak", 10).await;
    r.world.set_muted(true);
    let handle = r.start();
    sleep(mins(10) + SETTLE).await;
    assert_eq!(
        r.state(id).await,
        TimerState::Done(TimerOutcome::SkippedMuted)
    );
    assert!(r.fake.recorded().is_empty());
    assert!(!TimerOutcome::SkippedMuted.is_failure());
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn run_outcomes_map_to_success_or_failure() {
    let r = rig(vec![Step::Fail(LlmError::Unavailable)], 4);
    let id = wake_in(&r, "will fail", 10).await;
    let handle = r.start();
    sleep(mins(10) + SETTLE).await;
    let TimerState::Done(outcome) = r.state(id).await else {
        panic!("not done")
    };
    assert_eq!(outcome, TimerOutcome::Ran(RunEnd::ModelError));
    assert!(outcome.is_failure());
    for ok in [RunEnd::Completed, RunEnd::Delivered, RunEnd::StepLimit] {
        assert!(!TimerOutcome::Ran(ok).is_failure());
    }
    for bad in [
        RunEnd::Deadline,
        RunEnd::ModelError,
        RunEnd::Cancelled,
        RunEnd::Environment,
    ] {
        assert!(TimerOutcome::Ran(bad).is_failure());
    }
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_unfinished_claim_is_interrupted_on_restart_never_replayed() {
    let r = rig(vec![done()], 4);
    let wake = wake_in(&r, "claimed when the process died", 10).await;
    let job = r
        .store
        .insert_job(r.clock_now(), JobKind::Decay, Some(group()))
        .await
        .unwrap()
        .id;
    // Simulate a crash: both are claimed and nothing finishes them.
    let now = qbot_core::Clock::now(&*r.clock).plus(mins(11));
    r.store
        .claim_due_wake(now, &Default::default())
        .await
        .unwrap()
        .expect("claimed");
    r.store
        .claim_due_job(now, Duration::from_secs(900))
        .await
        .unwrap()
        .expect("claimed");

    let handle = r.start();
    sleep(SETTLE).await;
    assert_eq!(
        r.state(wake).await,
        TimerState::Interrupted,
        "an at-most-once claim is never replayed"
    );
    assert!(r.fake.recorded().is_empty());
    assert!(
        matches!(r.state(job).await, TimerState::Done(TimerOutcome::JobOk)),
        "an idempotent job runs again"
    );
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn jobs_retry_with_backoff_and_then_fail_for_good() {
    let r = rig(vec![], 4);
    r.jobs
        .results
        .lock()
        .unwrap()
        .extend((0..10).map(|_| Err(JobError("disk full".into()))));
    let id = r
        .store
        .insert_job(r.clock_now(), JobKind::Backup, None)
        .await
        .unwrap()
        .id;
    let started = r.clock_now();
    let handle = r.start();
    sleep(Duration::from_secs(3 * 3600)).await;

    let runs = r.jobs.runs.lock().unwrap().clone();
    assert_eq!(runs.len(), 5, "max attempts");
    let gaps: Vec<u64> = runs
        .windows(2)
        .map(|w| w[1].2.since(w[0].2).as_secs())
        .collect();
    assert_eq!(gaps, [60, 300, 1800, 3600]);
    assert!(runs[0].2.since(started) < Duration::from_secs(2));
    assert_eq!(
        r.state(id).await,
        TimerState::Done(TimerOutcome::JobFailed("disk full".into()))
    );
    assert_eq!(r.timer(id).await.attempts, 5);
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_job_that_succeeds_after_a_failure_is_done() {
    let r = rig(vec![], 4);
    r.jobs
        .results
        .lock()
        .unwrap()
        .extend([Err(JobError("flaky".into())), Ok(())]);
    let id = r
        .store
        .insert_job(r.clock_now(), JobKind::Extract, Some(group()))
        .await
        .unwrap()
        .id;
    let handle = r.start();
    sleep(mins(3)).await;
    assert_eq!(r.state(id).await, TimerState::Done(TimerOutcome::JobOk));
    assert_eq!(r.timer(id).await.attempts, 2);
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_expired_job_lease_lets_the_job_be_taken_over() {
    let r = rig(vec![], 4);
    let id = r
        .store
        .insert_job(r.clock_now(), JobKind::Nightly, Some(group()))
        .await
        .unwrap()
        .id;
    let now = r.clock_now();
    let first = r
        .store
        .claim_due_job(now, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.id, id);
    assert!(
        r.store
            .claim_due_job(now.plus(Duration::from_secs(30)), Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "lease still held"
    );
    let taken = r
        .store
        .claim_due_job(now.plus(Duration::from_secs(61)), Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.attempts, 2);
}

#[tokio::test(start_paused = true)]
async fn concurrent_jobs_are_bounded() {
    let cfg = SchedulerConfig {
        max_concurrent_jobs: 2,
        ..SchedulerConfig::default()
    };
    let r = rig_with(vec![], 4, cfg);
    *r.jobs.hold.lock().unwrap() = Duration::from_secs(100);
    for _ in 0..3 {
        r.store
            .insert_job(r.clock_now(), JobKind::Cleanup, None)
            .await
            .unwrap();
    }
    let handle = r.start();
    sleep(Duration::from_secs(10)).await;
    assert_eq!(r.jobs.runs.lock().unwrap().len(), 2);
    sleep(Duration::from_secs(120)).await;
    assert_eq!(
        r.jobs.runs.lock().unwrap().len(),
        3,
        "the third starts when a slot frees"
    );
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutting_down_cancels_a_running_task_and_records_it() {
    let r = rig(vec![hold(100_000_000)], 4);
    let id = wake_in(&r, "long", 10).await;
    let handle = r.start();
    sleep(mins(10) + SETTLE).await;
    assert!(matches!(r.state(id).await, TimerState::Claimed { .. }));

    r.shutdown.cancel();
    r.supervisor.shutdown().await;
    handle.await.unwrap();
    assert_eq!(
        r.state(id).await,
        TimerState::Done(TimerOutcome::Ran(RunEnd::Cancelled))
    );
    assert_eq!(r.supervisor.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn addressed_runs_and_tasks_share_one_capacity() {
    let r = rig(vec![hold(100_000_000)], 1);
    r.world.say(1, 1, "hi");
    let held = r
        .supervisor
        .submit(TriggerRequest {
            group: group(),
            trigger: qbot_agent::Trigger::Addressed {
                message: qbot_core::MessageId::new(101).unwrap(),
                sender: qbot_core::AccountId::new(1).unwrap(),
            },
        })
        .await
        .unwrap();
    let id = wake_in(&r, "waits for room", 10).await;
    let handle = r.start();
    sleep(mins(11)).await;
    assert!(
        r.state(id).await.is_pending(),
        "the addressed run holds the only slot"
    );
    let again = r.supervisor.submit(TriggerRequest {
        group: group(),
        trigger: Trigger::Addressed {
            message: qbot_core::MessageId::new(102).unwrap(),
            sender: qbot_core::AccountId::new(2).unwrap(),
        },
    });
    assert_eq!(again.await.unwrap_err(), Rejected::Overloaded);
    drop(held);
    r.shutdown.cancel();
    r.supervisor.shutdown().await;
    handle.await.unwrap();
}

impl Rig {
    fn clock_now(&self) -> qbot_core::UnixMillis {
        qbot_core::Clock::now(&*self.clock)
    }
}

#[allow(dead_code)]
fn unused(_: FakeReply) {}

#[tokio::test(start_paused = true)]
async fn a_job_longer_than_its_lease_is_not_taken_over_while_it_runs() {
    let cfg = SchedulerConfig {
        job_lease: Duration::from_secs(30),
        ..SchedulerConfig::default()
    };
    let r = rig_with(vec![], 4, cfg);
    *r.jobs.hold.lock().unwrap() = Duration::from_secs(300);
    let id = r
        .store
        .insert_job(r.clock_now(), JobKind::Backup, None)
        .await
        .unwrap()
        .id;
    let handle = r.start();
    sleep(Duration::from_secs(200)).await;
    assert_eq!(
        r.jobs.runs.lock().unwrap().len(),
        1,
        "ten leases have lapsed on paper, yet it ran once"
    );
    assert!(matches!(r.state(id).await, TimerState::Claimed { .. }));
    sleep(Duration::from_secs(200)).await;
    assert!(matches!(
        r.state(id).await,
        TimerState::Done(TimerOutcome::JobOk)
    ));
    assert_eq!(r.jobs.runs.lock().unwrap().len(), 1);
    r.shutdown.cancel();
    handle.await.unwrap();
}

/// A clock that moves one millisecond forward with every reading.
#[derive(Debug)]
struct Stepping(std::sync::atomic::AtomicI64);
impl qbot_core::Clock for Stepping {
    fn now(&self) -> qbot_core::UnixMillis {
        qbot_core::UnixMillis::new(self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }
}

#[tokio::test(start_paused = true)]
async fn a_job_that_comes_due_between_two_clock_readings_is_not_stranded() {
    // The tick reads START and finds nothing due; the job is due at START + 1, which the next
    // reading already shows. It must count as coming due, not as due-but-blocked.
    let r = rig(vec![], 4);
    let id = r
        .store
        .insert_job(
            qbot_core::UnixMillis::new(START + 1),
            JobKind::Extract,
            Some(group()),
        )
        .await
        .unwrap()
        .id;
    let scheduler = std::sync::Arc::new(qbot_sched::Scheduler::new(
        r.store.clone(),
        r.supervisor.clone(),
        std::sync::Arc::new(Stepping(std::sync::atomic::AtomicI64::new(START))),
        r.jobs.clone(),
        SchedulerConfig::default(),
        std::sync::Arc::new(tokio::sync::Notify::new()),
    ));
    let handle = tokio::spawn({
        let (scheduler, shutdown) = (scheduler.clone(), r.shutdown.clone());
        async move { scheduler.run(shutdown).await }
    });
    sleep(mins(1)).await;
    assert_eq!(r.state(id).await, TimerState::Done(TimerOutcome::JobOk));
    r.shutdown.cancel();
    handle.await.unwrap();
}
