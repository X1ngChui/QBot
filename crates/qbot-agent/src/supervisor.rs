//! Admission and lifetime of runs.
//!
//! Every eligible trigger becomes its own independent run; runs of one group may overlap. The
//! supervisor decides *before a run exists* whether a trigger is allowed (mute, block), bounds
//! the number of active-plus-waiting runs, bounds how many call the model at once, and gives
//! each run one end-to-end deadline that queue time counts against.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use qbot_context::{RunEnd, Transcript};
use qbot_core::{GroupId, RunId};
use qbot_llm::Usage;
use tokio::sync::{Notify, Semaphore, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::env::{ContextSource, EnvError, GroupPolicy, RunSummary, Trigger};
use crate::run::{RunDeps, RunInput, RunReport, execute};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// Active plus waiting runs, timers included.
    pub capacity: usize,
    /// Runs allowed to be past the queue (and so calling the model) at once.
    pub concurrency: usize,
    /// One deadline per run, measured from admission.
    pub reply_deadline: Duration,
}

#[derive(Debug, Clone)]
pub struct TriggerRequest {
    pub group: GroupId,
    pub trigger: Trigger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    Muted,
    /// The triggering member is blocked. No run, transcript or model call is created.
    Blocked,
    Overloaded,
    ShuttingDown,
    Environment(EnvError),
}

#[derive(Debug)]
pub struct RunHandle {
    pub run: RunId,
    done: oneshot::Receiver<RunReport>,
}

impl RunHandle {
    /// The final report, or `None` if the supervisor was torn down without finishing the run.
    pub async fn finished(self) -> Option<RunReport> {
        self.done.await.ok()
    }
}

struct Inner {
    cfg: SupervisorConfig,
    deps: Arc<RunDeps>,
    source: Arc<dyn ContextSource>,
    policy: Arc<dyn GroupPolicy>,
    slots: AtomicUsize,
    permits: Semaphore,
    cancel: CancellationToken,
    tasks: TaskTracker,
    freed: Notify,
}

/// A claimed unit of capacity. Timers reserve one *before* claiming a due row, so a claimed
/// row can always start. Dropping it releases the capacity.
pub struct Reservation {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation").finish_non_exhaustive()
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.inner.slots.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("active", &self.active())
            .finish_non_exhaustive()
    }
}

impl Supervisor {
    pub fn new(
        cfg: SupervisorConfig,
        deps: Arc<RunDeps>,
        source: Arc<dyn ContextSource>,
        policy: Arc<dyn GroupPolicy>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                permits: Semaphore::new(cfg.concurrency.max(1)),
                cfg,
                deps,
                source,
                policy,
                slots: AtomicUsize::new(0),
                cancel: CancellationToken::new(),
                tasks: TaskTracker::new(),
                freed: Notify::new(),
            }),
        }
    }

    /// Runs currently holding capacity (active or waiting).
    pub fn active(&self) -> usize {
        self.inner.slots.load(Ordering::SeqCst)
    }

    /// Notified when a run finishes and so frees capacity. Releasing an unused reservation does
    /// not notify: the only waiter is the scheduler, which is the one releasing it.
    pub fn freed(&self) -> &Notify {
        &self.inner.freed
    }

    pub fn reserve(&self) -> Option<Reservation> {
        let inner = &self.inner;
        inner
            .slots
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < inner.cfg.capacity).then_some(n + 1)
            })
            .ok()
            .map(|_| Reservation {
                inner: Arc::clone(&self.inner),
            })
    }

    pub async fn submit(&self, request: TriggerRequest) -> Result<RunHandle, Rejected> {
        self.check_gates(&request).await?;
        let reservation = self.reserve().ok_or(Rejected::Overloaded)?;
        self.start(reservation, request).await
    }

    /// Start a run on capacity reserved earlier (timers). Gates still apply; a rejection
    /// releases the reservation.
    pub async fn submit_reserved(
        &self,
        reservation: Reservation,
        request: TriggerRequest,
    ) -> Result<RunHandle, Rejected> {
        self.check_gates(&request).await?;
        self.start(reservation, request).await
    }

    /// Cancel every run and wait for them to finish. Unfinished tool calls are recorded as
    /// interrupted.
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
    }

    async fn check_gates(&self, request: &TriggerRequest) -> Result<(), Rejected> {
        if self.inner.cancel.is_cancelled() {
            return Err(Rejected::ShuttingDown);
        }
        let policy = &self.inner.policy;
        if policy
            .is_muted(request.group)
            .await
            .map_err(Rejected::Environment)?
        {
            return Err(Rejected::Muted);
        }
        if let Trigger::Addressed { sender, .. } = &request.trigger
            && policy
                .is_blocked(request.group, *sender)
                .await
                .map_err(Rejected::Environment)?
        {
            return Err(Rejected::Blocked);
        }
        Ok(())
    }

    async fn start(
        &self,
        reservation: Reservation,
        request: TriggerRequest,
    ) -> Result<RunHandle, Rejected> {
        let inner = Arc::clone(&self.inner);
        let finished = Arc::clone(&self.inner);
        // The run exists (and is recorded) from admission, so a run that never gets to start is
        // still visible. A failure here drops the reservation.
        let run = inner
            .deps
            .log
            .begin(request.group, &request.trigger)
            .await
            .map_err(Rejected::Environment)?;
        let deadline = Instant::now() + inner.cfg.reply_deadline;
        let (tx, rx) = oneshot::channel();
        let task = async move {
            let report = drive(&inner, run, request, deadline).await;
            drop(reservation);
            finished.freed.notify_one();
            let _ = tx.send(report);
        };
        self.inner.tasks.spawn(task);
        Ok(RunHandle { run, done: rx })
    }
}

async fn drive(inner: &Inner, run: RunId, request: TriggerRequest, deadline: Instant) -> RunReport {
    let permit = tokio::select! {
        () = inner.cancel.cancelled() => return finish_unstarted(inner, run, request.group, RunEnd::Cancelled).await,
        () = tokio::time::sleep_until(deadline) => return finish_unstarted(inner, run, request.group, RunEnd::Deadline).await,
        permit = inner.permits.acquire() => permit,
    };
    let Ok(_permit) = permit else {
        return finish_unstarted(inner, run, request.group, RunEnd::Cancelled).await;
    };
    // A permit won at the very instant the deadline passes must not start a model call.
    if Instant::now() >= deadline {
        return finish_unstarted(inner, run, request.group, RunEnd::Deadline).await;
    }
    let context = match inner.source.open(request.group, &request.trigger).await {
        Ok(context) => context,
        Err(_) => return finish_unstarted(inner, run, request.group, RunEnd::Environment).await,
    };
    let input = RunInput {
        run,
        group: request.group,
        trigger: request.trigger,
        context,
        deadline,
    };
    execute(&inner.deps, &input, &inner.cancel).await
}

/// Close the record of a run that never reached the model.
async fn finish_unstarted(inner: &Inner, run: RunId, group: GroupId, end: RunEnd) -> RunReport {
    let summary = RunSummary {
        end,
        error: None,
        usage: Usage::ZERO,
        turns: 0,
        tool_calls: 0,
        sends: 0,
    };
    let end = match inner.deps.log.finish(group, run, &summary).await {
        Ok(()) => end,
        Err(_) => RunEnd::Environment,
    };
    unstarted(run, group, end)
}

fn unstarted(run: RunId, group: GroupId, end: RunEnd) -> RunReport {
    RunReport {
        run,
        group,
        end,
        error: None,
        usage: Usage::ZERO,
        turns: 0,
        tool_calls: 0,
        sends: 0,
        transcript: Transcript::new(),
    }
}
