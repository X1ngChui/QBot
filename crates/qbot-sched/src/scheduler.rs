//! The single tick loop that fires due timers.
//!
//! A due group task is claimed only after supervisor capacity has been reserved for it, so a
//! claimed task can always start; without capacity it simply stays pending. Group tasks are
//! at-most-once (a claim is never replayed); background jobs are idempotent and retried with
//! backoff. One loop sleeps until the next due instant and is woken early by task changes and
//! by freed capacity. On shutdown a running job is dropped where it is: its claim stays, and
//! the next start puts it back in the queue.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::FutureExt;
use qbot_agent::{Rejected, Supervisor, Trigger, TriggerRequest};
use qbot_context::RunEnd;
use qbot_core::{Clock, GroupId, TimerId, UnixMillis};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::{AbortOnDropHandle, TaskTracker};

use crate::store::TimerStore;
use crate::timer::{JobKind, Timer, TimerKind, TimerOutcome};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct JobError(pub String);

/// Executes background jobs. Jobs must be idempotent: they can run again after a crash.
#[async_trait]
pub trait JobRunner: Send + Sync {
    async fn run(&self, kind: JobKind, group: Option<GroupId>) -> Result<(), JobError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// How long a job may run before another worker may take it over.
    pub job_lease: Duration,
    pub max_job_attempts: u32,
    /// Delay before retry `n` (1-based); the last entry repeats.
    pub job_backoff: Vec<Duration>,
    pub max_concurrent_jobs: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            job_lease: Duration::from_secs(15 * 60),
            max_job_attempts: 5,
            job_backoff: [60, 300, 1800, 3600].map(Duration::from_secs).to_vec(),
            max_concurrent_jobs: 2,
        }
    }
}

#[derive(Default)]
struct Active {
    wake_groups: HashSet<GroupId>,
    jobs: usize,
}

/// How long to wait before trying the timer store again after it failed.
const STORE_RETRY: Duration = Duration::from_secs(30);

pub struct Scheduler {
    store: Arc<dyn TimerStore>,
    supervisor: Supervisor,
    clock: Arc<dyn Clock>,
    jobs: Arc<dyn JobRunner>,
    cfg: SchedulerConfig,
    wake: Arc<Notify>,
    active: Arc<Mutex<Active>>,
    tasks: TaskTracker,
    /// Drops running jobs when the loop stops.
    stop: CancellationToken,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler").finish_non_exhaustive()
    }
}

/// What one tick did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tick {
    pub wakes_started: usize,
    pub jobs_started: usize,
}

impl Scheduler {
    /// `wake` must be the same `Notify` given to the `TaskService`.
    pub fn new(
        store: Arc<dyn TimerStore>,
        supervisor: Supervisor,
        clock: Arc<dyn Clock>,
        jobs: Arc<dyn JobRunner>,
        cfg: SchedulerConfig,
        wake: Arc<Notify>,
    ) -> Self {
        Self {
            store,
            supervisor,
            clock,
            jobs,
            cfg,
            wake,
            active: Arc::default(),
            tasks: TaskTracker::new(),
            stop: CancellationToken::new(),
        }
    }

    /// Recover from a previous process, then tick until `shutdown`.
    pub async fn run(&self, shutdown: CancellationToken) {
        while let Err(error) = self.store.recover().await {
            tracing::error!(%error, "scheduled work could not be recovered; retrying");
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(STORE_RETRY) => {}
            }
        }
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            // One reading for the tick and for what comes due after it: a timer that comes due
            // between two readings would otherwise look due-but-blocked and wait for a
            // notification that never comes.
            let now = self.clock.now();
            if let Err(error) = self.tick_at(now).await {
                // The store is unreachable for now: try again shortly rather than stop
                // scheduled work until a restart.
                tracing::error!(%error, "scheduled work could not be claimed; retrying");
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(STORE_RETRY) => continue,
                }
            }
            let next_due = self.next_future_due(now).await;
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = async {
                    match next_due {
                        Some(wait) => tokio::time::sleep(wait).await,
                        None => std::future::pending().await,
                    }
                } => {}
                () = self.wake.notified() => {}
                () = self.supervisor.freed().notified() => {}
            }
        }
        // A job can run for many minutes (an extraction pass, a backup). Its claim stays when it
        // is dropped, and startup recovery queues it again; a group task's run ends with the
        // supervisor's shutdown.
        self.stop.cancel();
        self.join().await;
    }

    /// Wait for the wake and job tasks this scheduler started.
    pub async fn join(&self) {
        self.tasks.close();
        self.tasks.wait().await;
        self.tasks.reopen();
    }

    /// Fire everything that is due and can start now.
    pub async fn tick(&self) -> Result<Tick, crate::store::StoreError> {
        self.tick_at(self.clock.now()).await
    }

    async fn tick_at(&self, now: UnixMillis) -> Result<Tick, crate::store::StoreError> {
        let mut tick = Tick::default();
        loop {
            let Some(reservation) = self.supervisor.reserve() else {
                break;
            };
            let exclude = self
                .active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .wake_groups
                .clone();
            let Some(timer) = self.store.claim_due_wake(now, &exclude).await? else {
                break;
            };
            self.start_wake(timer, reservation);
            tick.wakes_started += 1;
        }
        loop {
            {
                let active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
                if active.jobs >= self.cfg.max_concurrent_jobs {
                    break;
                }
            }
            let Some(timer) = self.store.claim_due_job(now, self.cfg.job_lease).await? else {
                break;
            };
            self.start_job(timer);
            tick.jobs_started += 1;
        }
        Ok(tick)
    }

    /// Time until the next timer that came due after the tick at `ticked`. `None` means
    /// nothing is scheduled, or whatever is due was already due at the tick and is blocked (no
    /// capacity, group busy, job slots full); the loop then waits for a notification instead of
    /// polling, because every change that can unblock work notifies it. A store that cannot say
    /// is asked again shortly: waiting only for notifications would leave a later timer unfired
    /// until something else happened.
    async fn next_future_due(&self, ticked: UnixMillis) -> Option<Duration> {
        match self.store.next_due().await {
            Ok(Some(due)) if due > ticked => Some(due.since(self.clock.now())),
            Ok(_) => None,
            Err(error) => {
                tracing::error!(%error, "the next due timer could not be read; retrying");
                Some(STORE_RETRY)
            }
        }
    }

    fn start_wake(&self, timer: Timer, reservation: qbot_agent::Reservation) {
        let TimerKind::Wake(spec) = timer.kind else {
            return;
        };
        self.active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .wake_groups
            .insert(spec.group);
        let (store, supervisor, wake, active) = (
            self.store.clone(),
            self.supervisor.clone(),
            self.wake.clone(),
            self.active.clone(),
        );
        let id = timer.id;
        self.spawn(async move {
            let request = TriggerRequest {
                group: spec.group,
                trigger: Trigger::Wake {
                    timer: id,
                    intent: spec.intent,
                    chain: spec.chain,
                },
            };
            let outcome = match supervisor.submit_reserved(reservation, request).await {
                Ok(handle) => Some(TimerOutcome::Ran(
                    handle
                        .finished()
                        .await
                        .map_or(RunEnd::Cancelled, |report| report.end),
                )),
                Err(Rejected::Muted) => Some(TimerOutcome::SkippedMuted),
                // Shutting down: leave the claim in place; startup recovery marks it interrupted.
                Err(Rejected::ShuttingDown) => None,
                Err(_) => Some(TimerOutcome::Ran(RunEnd::Environment)),
            };
            if let Some(outcome) = outcome
                && let Err(error) = store.finish(id, outcome).await
            {
                // The claim stays; startup recovery marks the task interrupted.
                tracing::error!(%error, task = id.get(), "a finished task could not be recorded");
            }
            active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .wake_groups
                .remove(&spec.group);
            wake.notify_one();
        });
    }

    fn start_job(&self, timer: Timer) {
        let TimerKind::Job { kind, group } = timer.kind else {
            return;
        };
        self.active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .jobs += 1;
        let (store, jobs, wake, active, clock) = (
            self.store.clone(),
            self.jobs.clone(),
            self.wake.clone(),
            self.active.clone(),
            self.clock.clone(),
        );
        let (cfg, id, attempts) = (self.cfg.clone(), timer.id, timer.attempts);
        let stop = self.stop.clone();
        self.spawn(async move {
            // The job's slot is given back however the task ends, a panicking job included.
            let _slot = Slot { active, wake };
            // While the job runs its lease is kept fresh, so a long job is not mistaken for a
            // dead one and taken over by a second run. A failed extension is tried again at the
            // next beat: the lease is three beats long, so one database blip does not let it
            // lapse. The heartbeat ends with the job.
            let heartbeat = {
                let (store, clock, lease) = (store.clone(), clock.clone(), cfg.job_lease);
                AbortOnDropHandle::new(tokio::spawn(async move {
                    let every = (lease / 3).max(Duration::from_millis(1));
                    loop {
                        tokio::time::sleep(every).await;
                        if let Err(error) = store.extend_lease(id, clock.now().plus(lease)).await {
                            tracing::warn!(%error, ?kind, "a running job's lease could not be extended");
                        }
                    }
                }))
            };
            // A panic in a job is a failure like any other, so it is retried and finally given
            // up on instead of leaving its claim to be taken again and again.
            let run = std::panic::AssertUnwindSafe(jobs.run(kind, group)).catch_unwind();
            let result = tokio::select! {
                () = stop.cancelled() => return,
                result = run => result.unwrap_or_else(|panic| {
                    let what = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_owned())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_default();
                    Err(JobError(format!("the job panicked: {what}")))
                }),
            };
            drop(heartbeat);
            let recorded = match result {
                Ok(()) => store.finish(id, TimerOutcome::JobOk).await,
                Err(error) if attempts >= cfg.max_job_attempts => {
                    tracing::error!(?kind, attempts, error = %error.0, "job failed; giving up");
                    store.finish(id, TimerOutcome::JobFailed(error.0)).await
                }
                Err(error) => {
                    tracing::warn!(?kind, attempts, error = %error.0, "job failed; will retry");
                    store
                        .retry_job(id, clock.now().plus(backoff(&cfg, attempts)))
                        .await
                }
            };
            if let Err(error) = recorded {
                // The claim stays and its lease runs out, so the job is taken up again: it is
                // idempotent.
                tracing::error!(%error, ?kind, "a job's outcome could not be recorded");
            }
        });
    }

    fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        self.tasks.spawn(task);
    }
}

/// A running job's place among `max_concurrent_jobs`, released on drop.
struct Slot {
    active: Arc<Mutex<Active>>,
    wake: Arc<Notify>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .jobs -= 1;
        self.wake.notify_one();
    }
}

fn backoff(cfg: &SchedulerConfig, attempts: u32) -> Duration {
    let index = usize::try_from(attempts.saturating_sub(1)).unwrap_or(usize::MAX);
    cfg.job_backoff
        .get(index)
        .or(cfg.job_backoff.last())
        .copied()
        .unwrap_or(Duration::from_secs(60))
}

/// A clock that follows tokio's (possibly paused) time, for deterministic scheduler tests.
#[derive(Debug, Clone, Copy)]
pub struct TokioClock {
    start_wall: UnixMillis,
    start: tokio::time::Instant,
}

impl TokioClock {
    pub fn new(start_wall: UnixMillis) -> Self {
        Self {
            start_wall,
            start: tokio::time::Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> UnixMillis {
        self.start_wall.plus(self.start.elapsed())
    }
}

/// Task ids are plain `TimerId`s; this alias keeps scheduler signatures readable.
pub type TaskId = TimerId;
