//! Group task management: the one place limits are checked, used by the model's tools and the
//! owner's commands alike.

use std::sync::Arc;
use std::time::Duration;

use qbot_agent::Chain;
use qbot_core::{Clock, GroupId, TimerId, UnixMillis};
use tokio::sync::Notify;

use crate::store::{StoreError, TimerStore};
use crate::timer::{NewWake, Origin, Timer, WakeEdit};

/// Bounds that exist to stop the model scheduling itself in a loop, not to police size.
/// There is deliberately no horizon cap and no intent length cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskLimits {
    /// Earliest a task may fire after creation; with the chain depth it bounds self-wake loops.
    pub min_delay: Duration,
    pub max_pending_per_group: usize,
    pub max_chain_depth: u32,
}

impl Default for TaskLimits {
    fn default() -> Self {
        Self {
            min_delay: Duration::from_secs(300),
            max_pending_per_group: 50,
            max_chain_depth: 24,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    At(UnixMillis),
    After(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskError {
    #[error("the intent must not be empty")]
    EmptyIntent,
    #[error("the time must be at least {earliest:?}")]
    TooSoon { earliest: UnixMillis },
    #[error("the group already has {limit} pending tasks")]
    TooManyPending { limit: usize },
    #[error("follow-up chain is already {limit} tasks deep")]
    ChainTooDeep { limit: u32 },
    #[error("no such task in this group")]
    NotFound,
    #[error("only pending tasks can be changed")]
    NotPending,
    #[error("nothing to change")]
    NothingToChange,
    #[error("storage failure: {0}")]
    Storage(String),
}

impl From<StoreError> for TaskError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::TooManyPending { limit } => TaskError::TooManyPending { limit },
            StoreError::NotFound => TaskError::NotFound,
            StoreError::NotPending(_) => TaskError::NotPending,
            StoreError::Backend(message) => TaskError::Storage(message),
        }
    }
}

#[derive(Clone)]
pub struct TaskService {
    store: Arc<dyn TimerStore>,
    clock: Arc<dyn Clock>,
    limits: TaskLimits,
    wake: Arc<Notify>,
}

impl std::fmt::Debug for TaskService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskService")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl TaskService {
    /// `wake` is notified after every change so the scheduler re-plans its sleep.
    pub fn new(
        store: Arc<dyn TimerStore>,
        clock: Arc<dyn Clock>,
        limits: TaskLimits,
        wake: Arc<Notify>,
    ) -> Self {
        Self {
            store,
            clock,
            limits,
            wake,
        }
    }

    pub fn limits(&self) -> &TaskLimits {
        &self.limits
    }

    pub async fn create(
        &self,
        group: GroupId,
        intent: &str,
        when: When,
        parent: Option<Chain>,
        origin: Origin,
    ) -> Result<Timer, TaskError> {
        let intent = self.check_intent(intent)?;
        let due_at = self.check_time(when)?;
        let chain = match parent {
            Some(parent) => {
                let depth = parent.depth + 1;
                if depth > self.limits.max_chain_depth {
                    return Err(TaskError::ChainTooDeep {
                        limit: self.limits.max_chain_depth,
                    });
                }
                Some(Chain {
                    id: parent.id,
                    depth,
                })
            }
            None => None,
        };
        let timer = self
            .store
            .insert_wake(
                due_at,
                NewWake {
                    group,
                    intent,
                    chain,
                    origin,
                },
                self.limits.max_pending_per_group,
            )
            .await?;
        self.wake.notify_one();
        Ok(timer)
    }

    pub async fn list(
        &self,
        group: GroupId,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Timer>, TaskError> {
        Ok(self.store.list_active_wakes(group, offset, limit).await?)
    }

    pub async fn get(&self, group: GroupId, id: TimerId) -> Result<Timer, TaskError> {
        match self.store.get(id).await? {
            Some(timer) if timer.wake().is_some_and(|w| w.group == group) => Ok(timer),
            _ => Err(TaskError::NotFound),
        }
    }

    pub async fn update(
        &self,
        group: GroupId,
        id: TimerId,
        intent: Option<&str>,
        when: Option<When>,
    ) -> Result<Timer, TaskError> {
        if intent.is_none() && when.is_none() {
            return Err(TaskError::NothingToChange);
        }
        let edit = WakeEdit {
            intent: intent.map(|i| self.check_intent(i)).transpose()?,
            due_at: when.map(|w| self.check_time(w)).transpose()?,
        };
        let timer = self.store.edit_pending_wake(group, id, edit).await?;
        self.wake.notify_one();
        Ok(timer)
    }

    pub async fn cancel(&self, group: GroupId, id: TimerId) -> Result<Timer, TaskError> {
        let timer = self.store.cancel_pending_wake(group, id).await?;
        self.wake.notify_one();
        Ok(timer)
    }

    fn check_intent(&self, intent: &str) -> Result<String, TaskError> {
        let trimmed = intent.trim();
        if trimmed.is_empty() {
            return Err(TaskError::EmptyIntent);
        }
        Ok(trimmed.to_owned())
    }

    fn check_time(&self, when: When) -> Result<UnixMillis, TaskError> {
        let now = self.clock.now();
        let due = match when {
            When::At(at) => at,
            When::After(delay) => now.plus(delay),
        };
        let earliest = now.plus(self.limits.min_delay);
        if due < earliest {
            return Err(TaskError::TooSoon { earliest });
        }
        Ok(due)
    }
}
