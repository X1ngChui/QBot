//! Maintenance schedules: jobs that repeat by a rule, kept by the scheduler from its
//! configuration.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use qbot_core::UnixMillis;
use qbot_sched::{JobError, JobKind, Repeat, SchedulerConfig, TimerStore, Zone};
use tokio::time::sleep;

const SETTLE: Duration = Duration::from_secs(1);

fn daily(time: &str) -> Repeat {
    Repeat {
        rule: format!("daily@{time}").parse().unwrap(),
        zone: Zone::get("UTC").unwrap(),
    }
}

/// The nightly run at 02:30 and the report at midnight. The rig starts at 08:00 UTC, so the
/// first report is 16 hours away and the first nightly run 18 and a half.
fn nightly_and_report() -> SchedulerConfig {
    SchedulerConfig {
        schedules: vec![
            (JobKind::Nightly, daily("02:30")),
            (JobKind::Report, daily("00:00")),
        ],
        ..SchedulerConfig::default()
    }
}

fn hours(n: u64) -> Duration {
    Duration::from_secs(n * 3600)
}

/// Every job run so far, as its kind and the minutes since the rig started.
fn runs(r: &Rig) -> Vec<(JobKind, i64)> {
    r.jobs
        .runs
        .lock()
        .unwrap()
        .iter()
        .map(|(kind, _, at)| (*kind, (at.get() - START) / 60_000))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_schedule_runs_once_at_each_occurrence_and_never_for_the_past() {
    let r = rig_with(vec![], 4, nightly_and_report());
    let handle = r.start();
    sleep(hours(1)).await;
    assert_eq!(runs(&r), [], "02:30 and 00:00 today were before the start");

    sleep(hours(49)).await;
    assert_eq!(
        runs(&r),
        [
            (JobKind::Report, 16 * 60),
            (JobKind::Nightly, 18 * 60 + 30),
            (JobKind::Report, 40 * 60),
            (JobKind::Nightly, 42 * 60 + 30),
        ]
    );
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn occurrences_missed_while_down_are_made_up_by_one_run() {
    let r = rig_with(vec![], 4, nightly_and_report());
    // The schedules exist, then the process is gone for three days.
    r.scheduler.start().await.unwrap();
    tokio::time::advance(hours(72)).await;

    // Starting again keeps the jobs there are: each runs once, then at its next occurrence.
    let handle = r.start();
    sleep(SETTLE).await;
    assert_eq!(
        runs(&r),
        [(JobKind::Report, 72 * 60), (JobKind::Nightly, 72 * 60)]
    );
    sleep(hours(24)).await;
    assert_eq!(
        runs(&r)[2..],
        [(JobKind::Report, 88 * 60), (JobKind::Nightly, 90 * 60 + 30)]
    );
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_scheduled_job_given_up_on_runs_again_at_its_next_occurrence() {
    let r = rig_with(
        vec![],
        4,
        SchedulerConfig {
            schedules: vec![(JobKind::Nightly, daily("02:30"))],
            ..SchedulerConfig::default()
        },
    );
    r.jobs
        .results
        .lock()
        .unwrap()
        .extend((0..5).map(|_| Err(JobError("disk full".into()))));
    let handle = r.start();
    sleep(hours(24)).await;
    let failed = runs(&r);
    assert_eq!(failed.len(), 5, "retried with backoff, then given up on");
    assert_eq!(failed[0], (JobKind::Nightly, 18 * 60 + 30));
    assert_eq!(
        r.store.next_due().await.unwrap(),
        Some(UnixMillis::new(START).plus(hours(42) + Duration::from_secs(1800))),
        "the schedule goes on"
    );

    sleep(hours(24)).await;
    assert_eq!(
        runs(&r)[5..],
        [(JobKind::Nightly, 42 * 60 + 30)],
        "the next night starts with its attempts afresh"
    );
    r.shutdown.cancel();
    handle.await.unwrap();
}
