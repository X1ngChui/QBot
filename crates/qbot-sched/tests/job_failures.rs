#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Jobs that go wrong while they run: a lease extension that fails once, and a job that panics.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use qbot_core::{GroupId, TimerId, UnixMillis};
use qbot_sched::{
    JobKind, MemoryTimerStore, NewWake, Recovery, Scheduler, SchedulerConfig, StoreError, Timer,
    TimerOutcome, TimerStore, WakeEdit,
};
use tokio::time::sleep;

/// The memory store, except that the first `extend_lease` fails (a database blip).
struct FlakyLease {
    inner: Arc<MemoryTimerStore>,
    fail: AtomicUsize,
}

#[async_trait]
impl TimerStore for FlakyLease {
    async fn insert_wake(&self, d: UnixMillis, n: NewWake, m: usize) -> Result<Timer, StoreError> {
        self.inner.insert_wake(d, n, m).await
    }
    async fn insert_job(
        &self,
        d: UnixMillis,
        k: JobKind,
        g: Option<GroupId>,
    ) -> Result<Timer, StoreError> {
        self.inner.insert_job(d, k, g).await
    }
    async fn get(&self, id: TimerId) -> Result<Option<Timer>, StoreError> {
        self.inner.get(id).await
    }
    async fn list_active_wakes(
        &self,
        g: GroupId,
        o: usize,
        l: usize,
    ) -> Result<Vec<Timer>, StoreError> {
        self.inner.list_active_wakes(g, o, l).await
    }
    async fn edit_pending_wake(
        &self,
        g: GroupId,
        id: TimerId,
        e: WakeEdit,
    ) -> Result<Timer, StoreError> {
        self.inner.edit_pending_wake(g, id, e).await
    }
    async fn cancel_pending_wake(&self, g: GroupId, id: TimerId) -> Result<Timer, StoreError> {
        self.inner.cancel_pending_wake(g, id).await
    }
    async fn claim_due_wake(
        &self,
        now: UnixMillis,
        ex: &HashSet<GroupId>,
    ) -> Result<Option<Timer>, StoreError> {
        self.inner.claim_due_wake(now, ex).await
    }
    async fn claim_due_job(
        &self,
        now: UnixMillis,
        lease: Duration,
    ) -> Result<Option<Timer>, StoreError> {
        self.inner.claim_due_job(now, lease).await
    }
    async fn finish(&self, id: TimerId, o: TimerOutcome) -> Result<(), StoreError> {
        self.inner.finish(id, o).await
    }
    async fn retry_job(&self, id: TimerId, at: UnixMillis) -> Result<(), StoreError> {
        self.inner.retry_job(id, at).await
    }
    async fn extend_lease(&self, id: TimerId, until: UnixMillis) -> Result<(), StoreError> {
        if self
            .fail
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(StoreError::Backend("pool timed out".into()));
        }
        self.inner.extend_lease(id, until).await
    }
    async fn fire_recurrence(
        &self,
        n: &str,
        o: UnixMillis,
        k: JobKind,
    ) -> Result<bool, StoreError> {
        self.inner.fire_recurrence(n, o, k).await
    }
    async fn seed_recurrence(&self, n: &str, o: UnixMillis) -> Result<(), StoreError> {
        self.inner.seed_recurrence(n, o).await
    }
    async fn next_due(&self) -> Result<Option<UnixMillis>, StoreError> {
        self.inner.next_due().await
    }
    async fn recover(&self) -> Result<Recovery, StoreError> {
        self.inner.recover().await
    }
}

#[tokio::test(start_paused = true)]
async fn one_failed_lease_extension_does_not_let_a_running_job_start_again() {
    let cfg = SchedulerConfig {
        job_lease: Duration::from_secs(30),
        ..SchedulerConfig::default()
    };
    let r = rig_with(vec![], 4, cfg.clone());
    // A long job: a nightly pass or a backup.
    *r.jobs.hold.lock().unwrap() = Duration::from_secs(300);
    let flaky = Arc::new(FlakyLease {
        inner: r.store.clone(),
        fail: AtomicUsize::new(1),
    });
    flaky
        .insert_job(UnixMillis::new(START), JobKind::Nightly, None)
        .await
        .unwrap();
    let scheduler = Arc::new(Scheduler::new(
        flaky.clone(),
        r.supervisor.clone(),
        r.clock.clone(),
        r.jobs.clone(),
        cfg,
        Arc::new(tokio::sync::Notify::new()),
    ));
    let handle = tokio::spawn({
        let (scheduler, shutdown) = (scheduler.clone(), r.shutdown.clone());
        async move { scheduler.run(shutdown).await }
    });
    sleep(Duration::from_secs(120)).await;
    let runs = r.jobs.runs.lock().unwrap().len();
    r.shutdown.cancel();
    handle.await.unwrap();
    // The first run is still inside its 300 s hold, yet the job was started again.
    assert_eq!(
        runs, 1,
        "the job was started {runs} times while its first run was still going"
    );
}

/// A job that panics (a bug on one group's data, say).
struct Panicking(AtomicUsize);

#[async_trait]
impl qbot_sched::JobRunner for Panicking {
    async fn run(&self, _: JobKind, _: Option<GroupId>) -> Result<(), qbot_sched::JobError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("a bug in the job");
    }
}

#[tokio::test(start_paused = true)]
async fn a_panicking_job_fails_like_an_error_and_is_given_up_on() {
    let cfg = SchedulerConfig {
        job_lease: Duration::from_secs(30),
        ..SchedulerConfig::default()
    };
    let r = rig_with(vec![], 4, cfg.clone());
    let id = r
        .store
        .insert_job(UnixMillis::new(START), JobKind::Extract, Some(group()))
        .await
        .unwrap()
        .id;
    let jobs = Arc::new(Panicking(AtomicUsize::new(0)));
    let scheduler = Arc::new(Scheduler::new(
        r.store.clone(),
        r.supervisor.clone(),
        r.clock.clone(),
        jobs.clone(),
        cfg,
        Arc::new(tokio::sync::Notify::new()),
    ));
    let handle = tokio::spawn({
        let (scheduler, shutdown) = (scheduler.clone(), r.shutdown.clone());
        async move { scheduler.run(shutdown).await }
    });
    // The backoff before the fifth attempt adds up to well under three hours.
    sleep(Duration::from_secs(3 * 3600)).await;
    let runs = jobs.0.load(Ordering::SeqCst);
    let timer = r.timer(id).await;
    r.shutdown.cancel();
    handle.await.unwrap();
    assert_eq!(runs, 5, "retried up to the attempt limit, then given up");
    assert!(
        matches!(&timer.state, qbot_sched::TimerState::Done(TimerOutcome::JobFailed(why)) if why.contains("panicked")),
        "{:?}",
        timer.state
    );
}
