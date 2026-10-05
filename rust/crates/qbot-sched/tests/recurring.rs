#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use chrono::TimeZone;
use qbot_core::{Clock, UnixMillis};
use qbot_sched::{
    JobKind, MemoryTimerStore, Recurrence, RecurrenceError, Recurring, TimerKind, TimerState,
    TimerStore,
};
use tokio::sync::Notify;

#[derive(Debug)]
struct Manual(AtomicI64);
impl Clock for Manual {
    fn now(&self) -> UnixMillis {
        UnixMillis::new(self.0.load(Ordering::SeqCst))
    }
}

/// 2026-10-04 12:00 in Shanghai (04:00 UTC).
fn noon() -> i64 {
    chrono_tz::Asia::Shanghai
        .with_ymd_and_hms(2026, 10, 4, 12, 0, 0)
        .unwrap()
        .timestamp_millis()
}

fn rig(start: i64) -> (Recurring, Arc<MemoryTimerStore>, Arc<Manual>) {
    let store = Arc::new(MemoryTimerStore::new());
    let clock = Arc::new(Manual(AtomicI64::new(start)));
    let recurring = Recurring::new(
        store.clone(),
        clock.clone(),
        "Asia/Shanghai",
        vec![
            Recurrence::new("nightly", "30 2 * * *", JobKind::Nightly).unwrap(),
            Recurrence::new("report", "0 0 * * *", JobKind::Report).unwrap(),
        ],
        Arc::new(Notify::new()),
    )
    .unwrap();
    (recurring, store, clock)
}

async fn due_jobs(store: &MemoryTimerStore, until: i64) -> Vec<(i64, JobKind)> {
    let mut out = Vec::new();
    while let Some(t) = store
        .claim_due_job(UnixMillis::new(until), Duration::from_secs(1))
        .await
        .unwrap()
    {
        let TimerKind::Job { kind, .. } = t.kind else {
            panic!()
        };
        out.push((t.due_at.get(), kind));
        store
            .finish(t.id, qbot_sched::TimerOutcome::JobOk)
            .await
            .unwrap();
    }
    out
}

#[test]
fn bad_expressions_and_zones_are_typed_errors() {
    assert!(matches!(
        Recurrence::new("x", "every night", JobKind::Nightly),
        Err(RecurrenceError::BadCron { .. })
    ));
    let store: Arc<dyn TimerStore> = Arc::new(MemoryTimerStore::new());
    let clock: Arc<dyn Clock> = Arc::new(Manual(AtomicI64::new(0)));
    assert!(matches!(
        Recurring::new(store, clock, "Mars/Base", vec![], Arc::new(Notify::new())),
        Err(RecurrenceError::BadZone(_))
    ));
}

#[tokio::test]
async fn a_new_schedule_starts_from_now_and_fires_each_occurrence_once() {
    let (recurring, store, clock) = rig(noon());
    // First run ever: nothing fires for the past.
    assert_eq!(recurring.catch_up().await.unwrap(), 0);
    assert!(due_jobs(&store, noon()).await.is_empty());
    // The next report is at midnight (12 hours away), the next nightly run at 02:30 after it.
    assert_eq!(recurring.until_next(), Some(Duration::from_secs(12 * 3600)));

    clock.0.store(noon() + 12 * 3600 * 1000, Ordering::SeqCst);
    assert_eq!(recurring.fire_due().await.unwrap(), 1);
    assert_eq!(
        recurring.fire_due().await.unwrap(),
        0,
        "the same occurrence is not fired twice"
    );
    clock.0.store(noon() + 15 * 3600 * 1000, Ordering::SeqCst);
    assert_eq!(recurring.fire_due().await.unwrap(), 1);
    let jobs = due_jobs(&store, noon() + 20 * 3600 * 1000).await;
    assert_eq!(
        jobs.iter().map(|j| j.1).collect::<Vec<_>>(),
        [JobKind::Report, JobKind::Nightly]
    );
    assert_eq!(
        jobs[0].0,
        noon() + 12 * 3600 * 1000,
        "due at the occurrence itself"
    );
    assert_eq!(jobs[1].0, noon() + 14 * 3600 * 1000 + 30 * 60 * 1000);
}

#[tokio::test]
async fn a_process_that_was_down_catches_up_once_not_for_every_missed_night() {
    let (first, store, clock) = rig(noon());
    first.catch_up().await.unwrap();
    // Three days pass with the process down.
    clock
        .0
        .store(noon() + 3 * 24 * 3600 * 1000, Ordering::SeqCst);
    // A restarted process (same store, new Recurring) catches up one occurrence of each.
    let again = Recurring::new(
        store.clone(),
        clock.clone(),
        "Asia/Shanghai",
        vec![Recurrence::new("nightly", "30 2 * * *", JobKind::Nightly).unwrap()],
        Arc::new(Notify::new()),
    )
    .unwrap();
    assert_eq!(again.catch_up().await.unwrap(), 1);
    assert_eq!(again.catch_up().await.unwrap(), 0);
    let jobs = due_jobs(&store, noon() + 4 * 24 * 3600 * 1000).await;
    assert_eq!(jobs.len(), 1);
    let _ = TimerState::Pending;
}

#[tokio::test(start_paused = true)]
async fn the_loop_sleeps_until_the_occurrence_and_stops_on_shutdown() {
    use qbot_sched::TokioClock;
    let store = Arc::new(MemoryTimerStore::new());
    let clock = Arc::new(TokioClock::new(UnixMillis::new(noon())));
    let wake = Arc::new(Notify::new());
    let recurring = Arc::new(
        Recurring::new(
            store.clone(),
            clock,
            "Asia/Shanghai",
            vec![Recurrence::new("report", "0 0 * * *", JobKind::Report).unwrap()],
            wake.clone(),
        )
        .unwrap(),
    );
    let stop = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn({
        let (recurring, stop) = (recurring.clone(), stop.clone());
        async move { recurring.run(stop).await }
    });
    tokio::time::sleep(Duration::from_secs(11 * 3600)).await;
    assert!(
        store.next_due().await.unwrap().is_none(),
        "not yet midnight"
    );
    tokio::time::sleep(Duration::from_secs(2 * 3600)).await;
    wake.notified().await;
    assert!(
        store.next_due().await.unwrap().is_some(),
        "the job exists after midnight"
    );
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn sub_second_clock_readings_name_the_same_occurrence() {
    // The real clock is never on a whole second; each reading must still agree on which
    // occurrence is the latest, or one occurrence would fire once per reading.
    let (recurring, store, clock) = rig(noon() + 123);
    assert_eq!(recurring.catch_up().await.unwrap(), 0);
    clock.0.store(noon() + 789, Ordering::SeqCst);
    assert_eq!(
        recurring.fire_due().await.unwrap(),
        0,
        "nothing new came due"
    );
    let midnight = noon() + 12 * 3600 * 1000;
    assert_eq!(
        recurring.until_next(),
        Some(Duration::from_millis(12 * 3600 * 1000 - 789))
    );
    clock.0.store(midnight + 5, Ordering::SeqCst);
    assert_eq!(recurring.fire_due().await.unwrap(), 1);
    clock.0.store(midnight + 950, Ordering::SeqCst);
    assert_eq!(recurring.fire_due().await.unwrap(), 0);
    let jobs = due_jobs(&store, midnight + 1000).await;
    assert_eq!(jobs, [(midnight, JobKind::Report)]);
}
