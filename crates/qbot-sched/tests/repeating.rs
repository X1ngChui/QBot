#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Repeating group tasks under the scheduler: one timer that comes due again by its rule, with
//! no help from the model.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use qbot_context::RunEnd;
use qbot_core::{Clock, TimerId, UnixMillis};
use qbot_llm::LlmError;
use qbot_llm::fake::Step;
use qbot_sched::{
    LastRun, Origin, Rule, Scheduler, SchedulerConfig, TimeOfDay, TimerKind, TimerOutcome,
    TimerState, TimerStore, When,
};
use tokio::time::sleep;

const SETTLE: Duration = Duration::from_secs(1);
const HOUR: Duration = Duration::from_secs(3600);

fn at(offset: Duration) -> UnixMillis {
    UnixMillis::new(START).plus(offset)
}

async fn hourly(r: &Rig, intent: &str) -> TimerId {
    r.service
        .create_repeating(
            group(),
            intent,
            Rule::interval(HOUR).unwrap(),
            None,
            None,
            Origin::Model,
        )
        .await
        .unwrap()
        .id
}

fn wakes(r: &Rig) -> usize {
    r.store
        .all()
        .iter()
        .filter(|t| matches!(t.kind, TimerKind::Wake(_)))
        .count()
}

#[tokio::test(start_paused = true)]
async fn a_repeating_task_runs_again_and_again_as_one_task() {
    let r = rig((0..3).map(|_| done()).collect(), 4);
    let id = hourly(&r, "post the hourly status").await;
    assert_eq!(r.timer(id).await.due_at, at(HOUR), "one interval from now");
    let handle = r.start();

    sleep(3 * HOUR + SETTLE).await;
    assert_eq!(r.fake.recorded().len(), 3, "three occurrences ran");
    let timer = r.timer(id).await;
    assert_eq!(
        (timer.state.clone(), timer.due_at),
        (TimerState::Pending, at(4 * HOUR))
    );
    assert_eq!(
        timer.wake().unwrap().last,
        Some(LastRun {
            due_at: at(3 * HOUR),
            outcome: TimerOutcome::Ran(RunEnd::Completed)
        })
    );
    assert_eq!(
        wakes(&r),
        1,
        "no task was created for the later occurrences"
    );
    let conv = format!("{:?}", r.fake.recorded()[2].conversation);
    assert!(conv.contains("post the hourly status"));
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_one_shot_task_still_runs_once() {
    let r = rig(vec![done(), done()], 4);
    let id = r
        .service
        .create(group(), "once", When::After(HOUR), None, Origin::Model)
        .await
        .unwrap()
        .id;
    assert_eq!(r.timer(id).await.wake().unwrap().repeat, None);
    let handle = r.start();
    sleep(5 * HOUR).await;
    assert_eq!(
        r.state(id).await,
        TimerState::Done(TimerOutcome::Ran(RunEnd::Completed))
    );
    assert_eq!(r.fake.recorded().len(), 1);
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_late_or_long_run_does_not_shift_the_schedule() {
    // Each occurrence holds its run for 20 minutes before it ends.
    let script = (0..2)
        .flat_map(|_| [hold(20 * 60 * 1000), done()])
        .collect();
    let r = rig(script, 4);
    let id = hourly(&r, "a slow check").await;
    let handle = r.start();
    sleep(HOUR + mins(25)).await;
    assert_eq!(
        r.timer(id).await.due_at,
        at(2 * HOUR),
        "the next occurrence counts from the schedule, not from when the run ended"
    );
    sleep(HOUR).await;
    assert_eq!(r.timer(id).await.due_at, at(3 * HOUR));
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_failed_or_skipped_occurrence_keeps_the_task_going() {
    let r = rig(vec![Step::Fail(LlmError::Unavailable), done()], 4);
    let id = hourly(&r, "keeps going").await;
    let handle = r.start();
    sleep(HOUR + SETTLE).await;
    let timer = r.timer(id).await;
    assert_eq!(
        (timer.state.clone(), timer.due_at),
        (TimerState::Pending, at(2 * HOUR))
    );
    assert_eq!(
        timer.wake().unwrap().last.as_ref().unwrap().outcome,
        TimerOutcome::Ran(RunEnd::ModelError)
    );
    // Muted for one occurrence: skipped without a model call, and still scheduled.
    r.world.set_muted(true);
    sleep(HOUR).await;
    let timer = r.timer(id).await;
    assert_eq!(
        (timer.state.clone(), timer.due_at),
        (TimerState::Pending, at(3 * HOUR))
    );
    assert_eq!(
        timer.wake().unwrap().last.as_ref().unwrap().outcome,
        TimerOutcome::SkippedMuted
    );
    r.world.set_muted(false);
    sleep(HOUR).await;
    assert_eq!(
        r.timer(id)
            .await
            .wake()
            .unwrap()
            .last
            .as_ref()
            .unwrap()
            .outcome,
        TimerOutcome::Ran(RunEnd::Completed)
    );
    assert_eq!(r.fake.recorded().len(), 2);
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn occurrences_missed_while_down_run_once_then_the_schedule_goes_on() {
    let r = rig((0..3).map(|_| done()).collect(), 4);
    let id = hourly(&r, "missed while down").await;
    // The process is down from before the first occurrence until five and a half hours later.
    sleep(5 * HOUR + mins(30)).await;
    let handle = r.start();
    sleep(SETTLE).await;
    assert_eq!(r.fake.recorded().len(), 1, "one run, not a burst of five");
    assert_eq!(
        r.timer(id).await.due_at,
        at(6 * HOUR),
        "the next occurrence after now, on the original phase"
    );
    sleep(mins(30)).await;
    assert_eq!(r.fake.recorded().len(), 2);
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_occurrence_running_when_the_process_died_is_not_replayed_and_the_task_survives() {
    let r = rig(vec![done()], 4);
    let id = hourly(&r, "survives a restart").await;
    // The previous process claimed the first occurrence and died.
    let claimed = r
        .store
        .claim_due_wake(at(HOUR), &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, id);
    sleep(HOUR + mins(10)).await;

    let handle = r.start();
    sleep(SETTLE).await;
    assert!(
        r.fake.recorded().is_empty(),
        "the interrupted occurrence is not run again"
    );
    let timer = r.timer(id).await;
    assert_eq!(
        (timer.state.clone(), timer.due_at),
        (TimerState::Pending, at(2 * HOUR))
    );
    assert_eq!(
        timer.wake().unwrap().last,
        Some(LastRun {
            due_at: at(HOUR),
            outcome: TimerOutcome::Ran(RunEnd::Interrupted)
        })
    );
    sleep(HOUR).await;
    assert_eq!(r.fake.recorded().len(), 1, "the next occurrence runs");
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn cancelling_during_an_occurrence_lets_it_finish_and_ends_the_task() {
    let r = rig(vec![hold(20 * 60 * 1000), done(), done()], 4);
    let id = hourly(&r, "cancel me").await;
    let handle = r.start();
    sleep(HOUR + mins(10)).await;
    assert!(matches!(r.state(id).await, TimerState::Claimed { .. }));
    let cancelled = r.service.cancel(group(), id).await.unwrap();
    assert_eq!(cancelled.state, TimerState::Cancelled);
    assert!(r.service.list(group(), 0, 10).await.unwrap().is_empty());

    sleep(mins(15)).await;
    let timer = r.timer(id).await;
    assert_eq!(timer.state, TimerState::Cancelled);
    assert_eq!(
        timer.wake().unwrap().last.as_ref().unwrap().outcome,
        TimerOutcome::Ran(RunEnd::Completed),
        "the running occurrence was not cut off"
    );
    sleep(3 * HOUR).await;
    assert_eq!(r.fake.recorded().len(), 2, "nothing ran after it");
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn two_schedulers_on_one_store_run_each_occurrence_once() {
    let r = rig((0..8).map(|_| done()).collect(), 8);
    let id = hourly(&r, "only once each").await;
    let second = Arc::new(Scheduler::new(
        r.store.clone(),
        r.supervisor.clone(),
        r.clock.clone(),
        r.jobs.clone(),
        SchedulerConfig::default(),
        Arc::new(tokio::sync::Notify::new()),
    ));
    let first = r.start();
    let other = tokio::spawn({
        let (second, shutdown) = (second.clone(), r.shutdown.clone());
        async move { second.run(shutdown).await }
    });
    // Both also tick at the moment each occurrence comes due.
    for hour in 1..=3 {
        sleep(at(hour * HOUR).since(r.clock.now())).await;
        let (a, b) = tokio::join!(r.scheduler.tick(), second.tick());
        let started = a.unwrap().wakes_started + b.unwrap().wakes_started;
        assert!(
            started <= 1,
            "occurrence {hour} was started {started} times"
        );
        sleep(SETTLE).await;
    }
    assert_eq!(r.fake.recorded().len(), 3);
    assert_eq!(r.timer(id).await.due_at, at(4 * HOUR));
    assert_eq!(wakes(&r), 1);
    r.shutdown.cancel();
    first.await.unwrap();
    other.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_daily_task_comes_due_at_its_time_each_day() {
    // START is 2027-01-15T08:00:00Z, and the rig's zone is UTC.
    let r = rig((0..3).map(|_| done()).collect(), 4);
    let id = r
        .service
        .create_repeating(
            group(),
            "good morning",
            Rule::Daily(TimeOfDay::new(9, 30).unwrap()),
            None,
            None,
            Origin::Owner,
        )
        .await
        .unwrap()
        .id;
    let day = 24 * HOUR;
    assert_eq!(r.timer(id).await.due_at, at(HOUR + mins(30)));
    let handle = r.start();
    sleep(HOUR + mins(30) + SETTLE).await;
    assert_eq!(r.timer(id).await.due_at, at(day + HOUR + mins(30)));
    sleep(day).await;
    assert_eq!(r.fake.recorded().len(), 2);
    assert_eq!(r.timer(id).await.due_at, at(2 * day + HOUR + mins(30)));
    r.shutdown.cancel();
    handle.await.unwrap();
}
