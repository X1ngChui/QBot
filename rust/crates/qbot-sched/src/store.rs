//! Durable timer storage. The in-memory implementation defines the semantics the Postgres
//! implementation must reproduce.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::Chain;
use qbot_core::{ChainId, GroupId, TimerId, UnixMillis};

use crate::timer::{
    JobKind, NewWake, Timer, TimerKind, TimerOutcome, TimerState, WakeEdit, WakeSpec,
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
    /// Idempotent jobs returned to pending.
    pub requeued: usize,
}

#[async_trait]
pub trait TimerStore: Send + Sync {
    /// Insert a task, enforcing the per-group pending bound atomically with the insert.
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
    async fn cancel_pending_wake(&self, group: GroupId, id: TimerId) -> Result<Timer, StoreError>;
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
    /// Startup recovery. Safe only while this process holds the exclusive runtime lease.
    async fn recover(&self) -> Result<Recovery, StoreError>;
}

#[derive(Default)]
struct Inner {
    timers: BTreeMap<TimerId, Timer>,
    next_id: u64,
    recurrences: BTreeMap<String, UnixMillis>,
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
        let pending = inner
            .timers
            .values()
            .filter(|t| is_wake_of(t, new.group) && t.state.is_pending())
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
        if let (Some(intent), TimerKind::Wake(spec)) = (edit.intent, &mut timer.kind) {
            spec.intent = intent;
        }
        Ok(timer.clone())
    }

    async fn cancel_pending_wake(&self, group: GroupId, id: TimerId) -> Result<Timer, StoreError> {
        let mut inner = self.lock();
        let timer = inner
            .timers
            .get_mut(&id)
            .filter(|t| is_wake_of(t, group))
            .ok_or(StoreError::NotFound)?;
        if !timer.state.is_pending() {
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
        Ok(self
            .lock()
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

    async fn recover(&self) -> Result<Recovery, StoreError> {
        let mut inner = self.lock();
        let mut report = Recovery::default();
        for timer in inner.timers.values_mut() {
            if !matches!(timer.state, TimerState::Claimed { .. }) {
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
