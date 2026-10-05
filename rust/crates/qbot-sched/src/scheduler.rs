//! The single tick loop that fires due timers.
//!
//! A due group task is claimed only after supervisor capacity has been reserved for it, so a
//! claimed task can always start; without capacity it simply stays pending. Group tasks are
//! at-most-once (a claim is never replayed); background jobs are idempotent and retried with
//! backoff. One loop sleeps until the next due instant and is woken early by task changes and
//! by freed capacity.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::{Rejected, Supervisor, Trigger, TriggerRequest};
use qbot_context::RunEnd;
use qbot_core::{Clock, GroupId, TimerId, UnixMillis};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

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

pub struct Scheduler {
    store: Arc<dyn TimerStore>,
    supervisor: Supervisor,
    clock: Arc<dyn Clock>,
    jobs: Arc<dyn JobRunner>,
    cfg: SchedulerConfig,
    wake: Arc<Notify>,
    active: Arc<Mutex<Active>>,
    tasks: Mutex<JoinSet<()>>,
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
            tasks: Mutex::new(JoinSet::new()),
        }
    }

    /// Recover from a previous process, then tick until `shutdown`.
    pub async fn run(&self, shutdown: CancellationToken) {
        if self.store.recover().await.is_err() {
            return;
        }
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            if self.tick().await.is_err() {
                break;
            }
            let next_due = self.next_future_due().await;
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
        self.join().await;
    }

    /// Wait for the wake and job tasks this scheduler started.
    pub async fn join(&self) {
        let mut tasks =
            std::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        while tasks.join_next().await.is_some() {}
    }

    /// Fire everything that is due and can start now.
    pub async fn tick(&self) -> Result<Tick, crate::store::StoreError> {
        let now = self.clock.now();
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

    /// Time until the next timer that is due in the future. `None` means nothing is scheduled
    /// ahead, or whatever is due is blocked (no capacity, group busy, job slots full); in both
    /// cases the loop waits for a notification instead of polling, because every change that
    /// can unblock work notifies it.
    async fn next_future_due(&self) -> Option<Duration> {
        let now = self.clock.now();
        match self.store.next_due().await {
            Ok(Some(due)) if due > now => Some(due.since(now)),
            _ => None,
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
            if let Some(outcome) = outcome {
                let _ = store.finish(id, outcome).await;
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
        self.spawn(async move {
            // While the job runs its lease is kept fresh, so a long job is not mistaken for a
            // dead one and taken over by a second run.
            let heartbeat = {
                let (store, clock, lease) = (store.clone(), clock.clone(), cfg.job_lease);
                tokio::spawn(async move {
                    let every = (lease / 3).max(Duration::from_millis(1));
                    loop {
                        tokio::time::sleep(every).await;
                        if store
                            .extend_lease(id, clock.now().plus(lease))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                })
            };
            let result = jobs.run(kind, group).await;
            heartbeat.abort();
            match result {
                Ok(()) => {
                    let _ = store.finish(id, TimerOutcome::JobOk).await;
                }
                Err(error) if attempts >= cfg.max_job_attempts => {
                    let _ = store.finish(id, TimerOutcome::JobFailed(error.0)).await;
                }
                Err(_) => {
                    let _ = store
                        .retry_job(id, clock.now().plus(backoff(&cfg, attempts)))
                        .await;
                }
            }
            active.lock().unwrap_or_else(PoisonError::into_inner).jobs -= 1;
            wake.notify_one();
        });
    }

    fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        self.tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spawn(task);
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
