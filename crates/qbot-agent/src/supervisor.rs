//! Admission and lifetime of runs.
//!
//! Every eligible trigger becomes its own run; runs of one group may overlap. The supervisor
//! decides *before a run exists* whether a trigger is allowed (mute, block, one spontaneous run
//! per group), bounds the number of active-plus-waiting runs, bounds how many call the model at
//! once, and gives each run one end-to-end deadline that queue time counts against.
//!
//! Runs a member starts in a group take turns: a member's second message while their run is
//! under way gets a run that waits for the first to end before it opens its context, so it sees
//! the first one's reply instead of answering beside it; a further message while one waits
//! folds into the waiting run, which then answers the newest.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use qbot_context::{RunEnd, Transcript};
use qbot_core::{AccountId, GroupId, MessageId, RunId};
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
    /// A spontaneous run of the group is under way; it does not get a second one.
    SpontaneousUnderWay,
    /// The member already has a run waiting for their run under way; the message became that
    /// run's focus (it is in the run's chat, which is read when the run opens).
    Folded,
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
    /// Groups with a spontaneous run admitted and not finished.
    spontaneous: Mutex<HashSet<GroupId>>,
    /// Members with an addressed run admitted and not finished, by group.
    members: Mutex<HashMap<(GroupId, AccountId), Member>>,
}

/// A member's addressed runs in one group.
struct Member {
    /// Held by the member's run that is under way, for its whole length.
    turn: Arc<tokio::sync::Mutex<()>>,
    /// Runs admitted and not finished.
    admitted: usize,
    /// The focus of the run waiting for its turn, which a later message of the member takes over.
    waiting: Option<Arc<Mutex<Focus>>>,
}

/// What a waiting run will answer, and by when. A message folded into the run moves both: the
/// newest message gets a whole deadline, not what is left of the first one's.
#[derive(Debug, Clone, Copy)]
struct Focus {
    message: MessageId,
    deadline: Instant,
}

/// An addressed run's place among its member's runs, held from admission until the run ends.
struct MemberTurn {
    inner: Arc<Inner>,
    key: (GroupId, AccountId),
    turn: Arc<tokio::sync::Mutex<()>>,
    /// The turn itself, for the member's first run: taken at admission, so runs take turns in
    /// the order they were admitted, whichever task happens to run first.
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
    /// Set while the run waits for an earlier run of the member to end.
    focus: Option<Arc<Mutex<Focus>>>,
}

impl MemberTurn {
    /// When the waiting run gives up: its own deadline, or a later one a folded message brought.
    fn deadline(&self, own: Instant) -> Instant {
        self.focus.as_ref().map_or(own, |focus| {
            own.max(
                focus
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .deadline,
            )
        })
    }

    /// What the run answers once its turn has come: its own message, or the newest one folded
    /// into it while it waited. From then on a new message of the member waits for this run.
    fn take_focus(&mut self) -> Option<Focus> {
        let focus = self.focus.take()?;
        let mut members = self
            .inner
            .members
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(member) = members.get_mut(&self.key)
            && member
                .waiting
                .as_ref()
                .is_some_and(|w| Arc::ptr_eq(w, &focus))
        {
            member.waiting = None;
        }
        let taken = *focus.lock().unwrap_or_else(PoisonError::into_inner);
        Some(taken)
    }
}

impl Drop for MemberTurn {
    fn drop(&mut self) {
        let mut members = self
            .inner
            .members
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(member) = members.get_mut(&self.key) else {
            return;
        };
        // A run that ends while still waiting (cancelled, past its deadline, not recorded) takes
        // no more messages.
        if let Some(focus) = &self.focus
            && member
                .waiting
                .as_ref()
                .is_some_and(|w| Arc::ptr_eq(w, focus))
        {
            member.waiting = None;
        }
        member.admitted = member.admitted.saturating_sub(1);
        if member.admitted == 0 {
            members.remove(&self.key);
        }
    }
}
/// A group's one spontaneous run, held from admission until the run ends.
struct SpontaneousClaim {
    inner: Arc<Inner>,
    group: GroupId,
}

impl Drop for SpontaneousClaim {
    fn drop(&mut self) {
        self.inner
            .spontaneous
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.group);
    }
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
                spontaneous: Mutex::default(),
                members: Mutex::default(),
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

    /// Start a run for `request`. An addressed run of a member whose run in the group is under
    /// way waits for that run to end; while one waits, the member's next message folds into it
    /// (`Rejected::Folded`) instead of starting a third. So a member holds at most one run under
    /// way and one waiting in a group, and a quick second message is answered once, by a run that
    /// sees the first reply.
    pub async fn submit(&self, request: TriggerRequest) -> Result<RunHandle, Rejected> {
        self.check_gates(&request).await?;
        let Trigger::Addressed { message, sender } = request.trigger else {
            let reservation = self.reserve().ok_or(Rejected::Overloaded)?;
            return self.start(reservation, request, Holds::default()).await;
        };
        let key = (request.group, sender);
        let (reservation, turn) = {
            let mut members = self
                .inner
                .members
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let deadline = Instant::now() + self.inner.cfg.reply_deadline;
            if let Some(focus) = members.get(&key).and_then(|m| m.waiting.as_ref()) {
                *focus.lock().unwrap_or_else(PoisonError::into_inner) = Focus { message, deadline };
                return Err(Rejected::Folded);
            }
            let reservation = self.reserve().ok_or(Rejected::Overloaded)?;
            let member = members.entry(key).or_insert_with(|| Member {
                turn: Arc::default(),
                admitted: 0,
                waiting: None,
            });
            member.admitted += 1;
            // The member's only run takes its turn now; one behind a run of the member that is
            // admitted and not finished waits for it.
            let held = if member.admitted == 1 {
                Arc::clone(&member.turn).try_lock_owned().ok()
            } else {
                None
            };
            let focus = (member.admitted > 1).then(|| {
                let focus = Arc::new(Mutex::new(Focus { message, deadline }));
                member.waiting = Some(Arc::clone(&focus));
                focus
            });
            let turn = MemberTurn {
                inner: Arc::clone(&self.inner),
                key,
                turn: Arc::clone(&member.turn),
                held,
                focus,
            };
            (reservation, turn)
        };
        let holds = Holds {
            spontaneous: None,
            member: Some(turn),
        };
        self.start(reservation, request, holds).await
    }

    /// Start a spontaneous run, considered because `observed` wrote a line that did not trigger
    /// the bot. `observed` is not the run's initiator and the run is not about their line; it
    /// only decides admission: a blocked member's message never starts a run, of any kind. A
    /// group has at most one spontaneous run at a time, or a lively chat would start several
    /// that talk over each other; other runs are not held back by it, nor it by them.
    pub async fn submit_spontaneous(
        &self,
        group: GroupId,
        observed: AccountId,
    ) -> Result<RunHandle, Rejected> {
        let request = TriggerRequest {
            group,
            trigger: Trigger::Spontaneous,
        };
        self.check_gates(&request).await?;
        if self
            .inner
            .policy
            .is_blocked(group, observed)
            .await
            .map_err(Rejected::Environment)?
        {
            return Err(Rejected::Blocked);
        }
        if !self
            .inner
            .spontaneous
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(group)
        {
            return Err(Rejected::SpontaneousUnderWay);
        }
        let claim = SpontaneousClaim {
            inner: Arc::clone(&self.inner),
            group,
        };
        let reservation = self.reserve().ok_or(Rejected::Overloaded)?;
        let holds = Holds {
            spontaneous: Some(claim),
            member: None,
        };
        self.start(reservation, request, holds).await
    }

    /// Start a run on capacity reserved earlier (timers). Gates still apply; a rejection
    /// releases the reservation.
    pub async fn submit_reserved(
        &self,
        reservation: Reservation,
        request: TriggerRequest,
    ) -> Result<RunHandle, Rejected> {
        self.check_gates(&request).await?;
        self.start(reservation, request, Holds::default()).await
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

    /// Record the run and spawn it. What it `holds` is released when the run ends, or now if it
    /// cannot be recorded.
    async fn start(
        &self,
        reservation: Reservation,
        request: TriggerRequest,
        holds: Holds,
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
        let admitted = Instant::now();
        let deadline = admitted + inner.cfg.reply_deadline;
        let trigger = match &request.trigger {
            Trigger::Addressed { .. } => "addressed",
            Trigger::Wake { .. } => "wake",
            Trigger::Spontaneous => "spontaneous",
        };
        let (tx, rx) = oneshot::channel();
        let Holds {
            spontaneous,
            mut member,
        } = holds;
        let task = async move {
            let report = drive(&inner, run, request, deadline, member.as_mut()).await;
            // One line per run, so the log alone shows whether the bot is replying and how fast.
            tracing::info!(
                group = report.group.get(),
                run = report.run.get(),
                trigger,
                end = report.end.as_str(),
                ms = u64::try_from(admitted.elapsed().as_millis()).unwrap_or(u64::MAX),
                turns = report.turns,
                sends = report.sends,
                input_tokens = report.usage.input_tokens,
                output_tokens = report.usage.output_tokens,
                "run ended"
            );
            drop(spontaneous);
            drop(member);
            drop(reservation);
            finished.freed.notify_one();
            let _ = tx.send(report);
        };
        self.inner.tasks.spawn(task);
        Ok(RunHandle { run, done: rx })
    }
}

/// What an admitted run holds until it ends.
#[derive(Default)]
struct Holds {
    spontaneous: Option<SpontaneousClaim>,
    member: Option<MemberTurn>,
}

async fn drive(
    inner: &Inner,
    run: RunId,
    mut request: TriggerRequest,
    deadline: Instant,
    member: Option<&mut MemberTurn>,
) -> RunReport {
    // A member's runs take turns, before either waits for a model slot. The turn is held until
    // this run ends, so the member's next run opens its context after this one's reply.
    let mut deadline = deadline;
    let _turn = match member {
        Some(member) => {
            let held = match member.held.take() {
                Some(held) => held,
                None => loop {
                    let until = member.deadline(deadline);
                    tokio::select! {
                        // Once cancelled or past the deadline, nothing more starts, whatever
                        // else is ready.
                        biased;
                        () = inner.cancel.cancelled() => return finish_unstarted(inner, run, request.group, RunEnd::Cancelled).await,
                        // A message folded in meanwhile may have moved the deadline on.
                        () = tokio::time::sleep_until(until) => {
                            if member.deadline(deadline) <= until {
                                return finish_unstarted(inner, run, request.group, RunEnd::Deadline).await;
                            }
                        }
                        held = Arc::clone(&member.turn).lock_owned() => break held,
                    }
                },
            };
            if let Some(focus) = member.take_focus() {
                deadline = deadline.max(focus.deadline);
                if let Trigger::Addressed { message, .. } = &mut request.trigger {
                    *message = focus.message;
                }
                // The wait can take most of a deadline: a mute or a block set meanwhile holds.
                if let Trigger::Addressed { sender, .. } = request.trigger {
                    let policy = &inner.policy;
                    let refused = match (
                        policy.is_muted(request.group).await,
                        policy.is_blocked(request.group, sender).await,
                    ) {
                        (Ok(muted), Ok(blocked)) => muted || blocked,
                        (Err(error), _) | (_, Err(error)) => {
                            tracing::warn!(group = request.group.get(), run = run.get(), %error, "run ended: its admission could not be checked again");
                            true
                        }
                    };
                    if refused {
                        return finish_unstarted(inner, run, request.group, RunEnd::Cancelled)
                            .await;
                    }
                }
            }
            Some(held)
        }
        None => None,
    };
    let permit = tokio::select! {
        biased;
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
    // Opening reads the store and asks the platform for names: bounded like the rest of the
    // run, so a stalled platform neither outlives the deadline nor holds up a shutdown.
    let opened = tokio::select! {
        biased;
        () = inner.cancel.cancelled() => return finish_unstarted(inner, run, request.group, RunEnd::Cancelled).await,
        () = tokio::time::sleep_until(deadline) => return finish_unstarted(inner, run, request.group, RunEnd::Deadline).await,
        opened = inner.source.open(request.group, &request.trigger) => opened,
    };
    let context = match opened {
        Ok(context) => context,
        Err(error) => {
            tracing::warn!(
                group = request.group.get(),
                run = run.get(),
                %error,
                "run ended: its context could not be opened"
            );
            return finish_unstarted(inner, run, request.group, RunEnd::Environment).await;
        }
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
        Err(error) => {
            tracing::warn!(group = group.get(), run = run.get(), %error, "a run's end could not be recorded");
            RunEnd::Environment
        }
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
