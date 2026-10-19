//! Durable timer storage. The in-memory implementation defines the semantics the Postgres
//! implementation must reproduce.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::Chain;
use qbot_core::{ChainId, GroupId, TimerId, UnixMillis};

use qbot_context::RunEnd;

use crate::timer::{
    JobKind, LastRun, NewWake, Timer, TimerKind, TimerOutcome, TimerState, WakeEdit, WakeSpec,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("the group already has {limit} pending tasks")]
    TooManyPending { limit: usize },
    #[error("no such timer")]
    NotFound,
    #[error("the timer is no longer pending")]
    NotPending(TimerState),
    #[error("storage failure: {0}")]
    Backend(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Recovery {
    /// At-most-once timers that were claimed when the process died.
    pub interrupted: usize,
    /// Repeating tasks whose occurrence was interrupted, pending again at a later occurrence.
    pub rescheduled: usize,
    /// Idempotent jobs returned to pending.
    pub requeued: usize,
}

#[async_trait]
pub trait TimerStore: Send + Sync {
    /// Insert a task, enforcing the per-group pending bound atomically with the insert. A
    /// repeating task whose occurrence is running counts as pending.
    async fn insert_wake(
        &self,
        due_at: UnixMillis,
        new: NewWake,
        max_pending: usize,
    ) -> Result<Timer, StoreError>;
    async fn insert_job(
        &self,
        due_at: UnixMillis,
        kind: JobKind,
        group: Option<GroupId>,
    ) -> Result<Timer, StoreError>;
    async fn get(&self, id: TimerId) -> Result<Option<Timer>, StoreError>;
    /// Pending and running tasks of a group, soonest first.
    async fn list_active_wakes(
        &self,
        group: GroupId,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Timer>, StoreError>;
    async fn edit_pending_wake(
        &self,
        group: GroupId,
        id: TimerId,
        edit: WakeEdit,
    ) -> Result<Timer, StoreError>;
    /// Cancel a pending task, or a repeating task whose occurrence is running: that occurrence
    /// is not cut off, and nothing is scheduled after it. A running one-shot task is not
    /// cancelled.
    async fn cancel_wake(&self, group: GroupId, id: TimerId) -> Result<Timer, StoreError>;
    /// Claim the oldest due pending task whose group is not in `exclude`. The claim is
    /// single-attempt: it is never replayed.
    async fn claim_due_wake(
        &self,
        now: UnixMillis,
        exclude: &HashSet<GroupId>,
    ) -> Result<Option<Timer>, StoreError>;
    /// Claim the oldest due job, or one whose lease expired.
    async fn claim_due_job(
        &self,
        now: UnixMillis,
        lease: Duration,
    ) -> Result<Option<Timer>, StoreError>;
    async fn finish(&self, id: TimerId, outcome: TimerOutcome) -> Result<(), StoreError>;
    /// Record how the running occurrence `occurrence` of a repeating task ended and return the
    /// task to pending, due at `next`. One compare-and-set on the claim: a second call for the
    /// same occurrence finds nothing to change (`NotPending`). A task cancelled while the
    /// occurrence ran keeps its cancellation and only records the outcome.
    async fn finish_occurrence(
        &self,
        id: TimerId,
        occurrence: UnixMillis,
        outcome: TimerOutcome,
        next: UnixMillis,
    ) -> Result<(), StoreError>;
    /// Return a claimed job to pending, due again at `at`.
    async fn retry_job(&self, id: TimerId, at: UnixMillis) -> Result<(), StoreError>;
    /// Push a claimed job's lease out to `until`, so a long job is not taken over while it runs.
    async fn extend_lease(&self, id: TimerId, until: UnixMillis) -> Result<(), StoreError>;
    /// Turn one occurrence of a recurring schedule into a job due at `occurrence`, unless that
    /// occurrence (or a later one) has already been fired. Returns whether a job was created.
    /// The check and the insert are one atomic step.
    async fn fire_recurrence(
        &self,
        name: &str,
        occurrence: UnixMillis,
        kind: JobKind,
    ) -> Result<bool, StoreError>;
    /// Record that `occurrence` of a schedule needs no job (a new schedule starts from now, not
    /// from the past). Does nothing if the schedule already has a record.
    async fn seed_recurrence(&self, name: &str, occurrence: UnixMillis) -> Result<(), StoreError>;
    /// Earliest instant at which something may need attention.
    async fn next_due(&self) -> Result<Option<UnixMillis>, StoreError>;
    /// Startup recovery at `now`. Safe only while this process holds the exclusive runtime lease.
    /// A claimed one-shot task is marked interrupted; a claimed repeating task records its
    /// occurrence as interrupted and is pending again at its first occurrence after `now`, so the
    /// interrupted occurrence is not replayed and the series goes on.
    async fn recover(&self, now: UnixMillis) -> Result<Recovery, StoreError>;
}

#[derive(Default)]
struct Inner {
    timers: BTreeMap<TimerId, Timer>,
    next_id: u64,
    recurrences: BTreeMap<String, UnixMillis>,
    /// Reads of the next due instant that fail, as an unreachable database would.
    next_due_failures: usize,
}

#[derive(Default)]
pub struct MemoryTimerStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for MemoryTimerStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTimerStore").finish_non_exhaustive()
    }
}

impl MemoryTimerStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Make the next `count` reads of the next due instant fail, for tests of a store outage.
    pub fn fail_next_due(&self, count: usize) {
        self.lock().next_due_failures = count;
    }

    /// Every timer, for assertions.
    pub fn all(&self) -> Vec<Timer> {
        self.lock().timers.values().cloned().collect()
    }
}

fn is_wake_of(timer: &Timer, group: GroupId) -> bool {
    timer.wake().is_some_and(|w| w.group == group)
}

#[async_trait]
impl TimerStore for MemoryTimerStore {
    async fn insert_wake(
        &self,
        due_at: UnixMillis,
        new: NewWake,
        max_pending: usize,
    ) -> Result<Timer, StoreError> {
        let mut inner = self.lock();
        // A repeating task whose occurrence runs is pending again soon, so it counts too.
        let pending = inner
            .timers
            .values()
            .filter(|t| {
                is_wake_of(t, new.group)
                    && (t.state.is_pending()
                        || (matches!(t.state, TimerState::Claimed { .. })
                            && t.wake().is_some_and(|w| w.repeat.is_some())))
            })
            .count();
        if pending >= max_pending {
            return Err(StoreError::TooManyPending { limit: max_pending });
        }
        inner.next_id += 1;
        let id = TimerId::new(inner.next_id);
        let chain = new.chain.unwrap_or(Chain {
            id: ChainId::new(id.get()),
            depth: 0,
        });
        let timer = Timer {
            id,
            due_at,
            kind: TimerKind::Wake(WakeSpec {
                group: new.group,
                intent: new.intent,
                chain,
                origin: new.origin,
                repeat: new.repeat,
                last: None,
            }),
            state: TimerState::Pending,
            attempts: 0,
        };
        inner.timers.insert(id, timer.clone());
        Ok(timer)
    }

    async fn insert_job(
        &self,
        due_at: UnixMillis,
        kind: JobKind,
        group: Option<GroupId>,
    ) -> Result<Timer, StoreError> {
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = TimerId::new(inner.next_id);
        let timer = Timer {
            id,
            due_at,
            kind: TimerKind::Job { kind, group },
            state: TimerState::Pending,
            attempts: 0,
        };
        inner.timers.insert(id, timer.clone());
        Ok(timer)
    }

    async fn get(&self, id: TimerId) -> Result<Option<Timer>, StoreError> {
        Ok(self.lock().timers.get(&id).cloned())
    }

    async fn list_active_wakes(
        &self,
        group: GroupId,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Timer>, StoreError> {
        let inner = self.lock();
        let mut active: Vec<&Timer> = inner
            .timers
            .values()
            .filter(|t| is_wake_of(t, group) && t.state.is_active())
            .collect();
        active.sort_by_key(|t| (t.due_at, t.id));
        Ok(active
            .into_iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn edit_pending_wake(
        &self,
        group: GroupId,
        id: TimerId,
        edit: WakeEdit,
    ) -> Result<Timer, StoreError> {
        let mut inner = self.lock();
        let timer = inner
            .timers
            .get_mut(&id)
            .filter(|t| is_wake_of(t, group))
            .ok_or(StoreError::NotFound)?;
        if !timer.state.is_pending() {
            return Err(StoreError::NotPending(timer.state.clone()));
        }
        if let Some(due_at) = edit.due_at {
            timer.due_at = due_at;
        }
        if let TimerKind::Wake(spec) = &mut timer.kind {
            if let Some(intent) = edit.intent {
                spec.intent = intent;
            }
            if let Some(repeat) = edit.repeat {
                spec.repeat = Some(repeat);
            }
        }
        Ok(timer.clone())
    }

    async fn cancel_wake(&self, group: GroupId, id: TimerId) -> Result<Timer, StoreError> {
        let mut inner = self.lock();
        let timer = inner
            .timers
            .get_mut(&id)
            .filter(|t| is_wake_of(t, group))
            .ok_or(StoreError::NotFound)?;
        let repeating = timer.wake().is_some_and(|w| w.repeat.is_some());
        let cancellable = match timer.state {
            TimerState::Pending => true,
            TimerState::Claimed { .. } => repeating,
            _ => false,
        };
        if !cancellable {
            return Err(StoreError::NotPending(timer.state.clone()));
        }
        timer.state = TimerState::Cancelled;
        Ok(timer.clone())
    }

    async fn claim_due_wake(
        &self,
        now: UnixMillis,
        exclude: &HashSet<GroupId>,
    ) -> Result<Option<Timer>, StoreError> {
        let mut inner = self.lock();
        let next = inner
            .timers
            .values()
            .filter(|t| {
                t.state.is_pending()
                    && t.due_at <= now
                    && t.wake().is_some_and(|w| !exclude.contains(&w.group))
            })
            .min_by_key(|t| (t.due_at, t.id))
            .map(|t| t.id);
        Ok(next.and_then(|id| {
            let timer = inner.timers.get_mut(&id)?;
            timer.state = TimerState::Claimed { lease_until: None };
            timer.attempts += 1;
            Some(timer.clone())
        }))
    }

    async fn claim_due_job(
        &self,
        now: UnixMillis,
        lease: Duration,
    ) -> Result<Option<Timer>, StoreError> {
        let mut inner = self.lock();
        let next = inner
            .timers
            .values()
            .filter(|t| match (&t.kind, &t.state) {
                (TimerKind::Job { .. }, TimerState::Pending) => t.due_at <= now,
                (
                    TimerKind::Job { .. },
                    TimerState::Claimed {
                        lease_until: Some(until),
                    },
                ) => *until <= now,
                _ => false,
            })
            .min_by_key(|t| (t.due_at, t.id))
            .map(|t| t.id);
        Ok(next.and_then(|id| {
            let timer = inner.timers.get_mut(&id)?;
            timer.state = TimerState::Claimed {
                lease_until: Some(now.plus(lease)),
            };
            timer.attempts += 1;
            Some(timer.clone())
        }))
    }

    async fn finish(&self, id: TimerId, outcome: TimerOutcome) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let timer = inner.timers.get_mut(&id).ok_or(StoreError::NotFound)?;
        if !matches!(timer.state, TimerState::Claimed { .. }) {
            return Err(StoreError::NotPending(timer.state.clone()));
        }
        timer.state = TimerState::Done(outcome);
        Ok(())
    }

    async fn finish_occurrence(
        &self,
        id: TimerId,
        occurrence: UnixMillis,
        outcome: TimerOutcome,
        next: UnixMillis,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let timer = inner.timers.get_mut(&id).ok_or(StoreError::NotFound)?;
        let matches = timer.due_at == occurrence
            && timer.wake().is_some_and(|w| w.repeat.is_some())
            && matches!(
                timer.state,
                TimerState::Claimed { .. } | TimerState::Cancelled
            );
        if !matches {
            return Err(StoreError::NotPending(timer.state.clone()));
        }
        if matches!(timer.state, TimerState::Claimed { .. }) {
            timer.state = TimerState::Pending;
            timer.due_at = next;
        }
        if let TimerKind::Wake(spec) = &mut timer.kind {
            spec.last = Some(LastRun {
                due_at: occurrence,
                outcome,
            });
        }
        Ok(())
    }

    async fn retry_job(&self, id: TimerId, at: UnixMillis) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let timer = inner.timers.get_mut(&id).ok_or(StoreError::NotFound)?;
        if !matches!(
            (&timer.kind, &timer.state),
            (TimerKind::Job { .. }, TimerState::Claimed { .. })
        ) {
            return Err(StoreError::NotPending(timer.state.clone()));
        }
        timer.state = TimerState::Pending;
        timer.due_at = at;
        Ok(())
    }

    async fn extend_lease(&self, id: TimerId, until: UnixMillis) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let timer = inner.timers.get_mut(&id).ok_or(StoreError::NotFound)?;
        match (&timer.kind, &mut timer.state) {
            (TimerKind::Job { .. }, TimerState::Claimed { lease_until }) => {
                *lease_until = Some(until);
                Ok(())
            }
            _ => Err(StoreError::NotPending(timer.state.clone())),
        }
    }

    async fn fire_recurrence(
        &self,
        name: &str,
        occurrence: UnixMillis,
        kind: JobKind,
    ) -> Result<bool, StoreError> {
        let mut inner = self.lock();
        if inner
            .recurrences
            .get(name)
            .is_some_and(|last| *last >= occurrence)
        {
            return Ok(false);
        }
        inner.recurrences.insert(name.to_owned(), occurrence);
        inner.next_id += 1;
        let id = TimerId::new(inner.next_id);
        inner.timers.insert(
            id,
            Timer {
                id,
                due_at: occurrence,
                kind: TimerKind::Job { kind, group: None },
                state: TimerState::Pending,
                attempts: 0,
            },
        );
        Ok(true)
    }

    async fn seed_recurrence(&self, name: &str, occurrence: UnixMillis) -> Result<(), StoreError> {
        self.lock()
            .recurrences
            .entry(name.to_owned())
            .or_insert(occurrence);
        Ok(())
    }

    async fn next_due(&self) -> Result<Option<UnixMillis>, StoreError> {
        let mut inner = self.lock();
        if inner.next_due_failures > 0 {
            inner.next_due_failures -= 1;
            return Err(StoreError::Backend("unreachable".into()));
        }
        Ok(inner
            .timers
            .values()
            .filter_map(|t| match &t.state {
                TimerState::Pending => Some(t.due_at),
                TimerState::Claimed {
                    lease_until: Some(until),
                } => Some(*until),
                _ => None,
            })
            .min())
    }

    async fn recover(&self, now: UnixMillis) -> Result<Recovery, StoreError> {
        let mut inner = self.lock();
        let mut report = Recovery::default();
        for timer in inner.timers.values_mut() {
            if !matches!(timer.state, TimerState::Claimed { .. }) {
                continue;
            }
            if let TimerKind::Wake(spec) = &mut timer.kind
                && let Some(repeat) = &spec.repeat
            {
                spec.last = Some(LastRun {
                    due_at: timer.due_at,
                    outcome: TimerOutcome::Ran(RunEnd::Interrupted),
                });
                timer.due_at = repeat.next_after(timer.due_at, now);
                timer.state = TimerState::Pending;
                report.rescheduled += 1;
                continue;
            }
            match timer.kind.delivery() {
                crate::timer::Delivery::AtMostOnce => {
                    timer.state = TimerState::Interrupted;
                    report.interrupted += 1;
                }
                crate::timer::Delivery::Idempotent => {
                    timer.state = TimerState::Pending;
                    report.requeued += 1;
                }
            }
        }
        Ok(report)
    }
}
