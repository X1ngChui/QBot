//! Group task management: the one place limits are checked, used by the model's tools and the
//! owner's commands alike.

use std::sync::Arc;
use std::time::Duration;

use qbot_agent::Chain;
use qbot_core::{Clock, GroupId, TimerId, UnixMillis};
use tokio::sync::Notify;

use crate::repeat::{Repeat, Rule, Zone};
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
    /// Shortest interval of a repeating task. Every occurrence is a model run in the group, and a
    /// repeating task never ends by itself, so a shorter period would be a standing flood of runs.
    pub min_interval: Duration,
}

impl Default for TaskLimits {
    fn default() -> Self {
        Self {
            min_delay: Duration::from_secs(300),
            max_pending_per_group: 50,
            max_chain_depth: 24,
            min_interval: Duration::from_secs(3600),
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
    #[error("a repeating task's interval must be at least {min:?}")]
    IntervalTooShort { min: Duration },
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
    /// The bot's time zone: a new repeat rule reads its times of day here.
    zone: Zone,
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
        zone: Zone,
    ) -> Self {
        Self {
            store,
            clock,
            limits,
            wake,
            zone,
        }
    }

    pub fn limits(&self) -> &TaskLimits {
        &self.limits
    }

    pub fn zone(&self) -> &Zone {
        &self.zone
    }

    /// A one-shot task due at `when`.
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
        self.insert(group, intent, due_at, None, parent, origin)
            .await
    }

    /// A task repeating by `rule` in the bot's zone. Its first occurrence is the first one at or
    /// after `start` (an interval starts exactly there), or, without `start`, one interval from
    /// now or the first matching time from the earliest allowed instant.
    pub async fn create_repeating(
        &self,
        group: GroupId,
        intent: &str,
        rule: Rule,
        start: Option<When>,
        parent: Option<Chain>,
        origin: Origin,
    ) -> Result<Timer, TaskError> {
        let intent = self.check_intent(intent)?;
        let repeat = self.check_rule(rule)?;
        let due_at = self.first_due(&repeat, start)?;
        self.insert(group, intent, due_at, Some(repeat), parent, origin)
            .await
    }

    async fn insert(
        &self,
        group: GroupId,
        intent: String,
        due_at: UnixMillis,
        repeat: Option<Repeat>,
        parent: Option<Chain>,
        origin: Origin,
    ) -> Result<Timer, TaskError> {
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
                    repeat,
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

    /// Change a pending task. A new time is when it next comes due; a repeating task's later
    /// occurrences follow its rule from there (an interval counts from the new time). A new
    /// `rule` makes the task repeat by it, from `when` as for [`Self::create_repeating`].
    pub async fn update(
        &self,
        group: GroupId,
        id: TimerId,
        intent: Option<&str>,
        when: Option<When>,
        rule: Option<Rule>,
    ) -> Result<Timer, TaskError> {
        if intent.is_none() && when.is_none() && rule.is_none() {
            return Err(TaskError::NothingToChange);
        }
        let intent = intent.map(|i| self.check_intent(i)).transpose()?;
        let edit = match rule {
            Some(rule) => {
                let repeat = self.check_rule(rule)?;
                WakeEdit {
                    intent,
                    due_at: Some(self.first_due(&repeat, when)?),
                    repeat: Some(repeat),
                }
            }
            None => WakeEdit {
                intent,
                due_at: when.map(|w| self.check_time(w)).transpose()?,
                repeat: None,
            },
        };
        let timer = self.store.edit_pending_wake(group, id, edit).await?;
        self.wake.notify_one();
        Ok(timer)
    }

    /// Cancel a pending task, or end a repeating one whose occurrence is running (that run goes
    /// on to its end; nothing follows it).
    pub async fn cancel(&self, group: GroupId, id: TimerId) -> Result<Timer, TaskError> {
        let timer = self.store.cancel_wake(group, id).await?;
        self.wake.notify_one();
        Ok(timer)
    }

    fn check_rule(&self, rule: Rule) -> Result<Repeat, TaskError> {
        if let Rule::Interval(period) = rule
            && period < self.limits.min_interval
        {
            return Err(TaskError::IntervalTooShort {
                min: self.limits.min_interval,
            });
        }
        Ok(Repeat {
            rule,
            zone: self.zone.clone(),
        })
    }

    fn first_due(&self, repeat: &Repeat, start: Option<When>) -> Result<UnixMillis, TaskError> {
        if let Some(start) = start {
            return Ok(repeat.first_at_or_after(self.check_time(start)?));
        }
        let now = self.clock.now();
        let earliest = now.plus(self.limits.min_delay);
        Ok(match repeat.rule {
            Rule::Interval(period) => now.plus(period).max(earliest),
            Rule::Daily(_) | Rule::Weekly(..) => repeat.first_at_or_after(earliest),
        })
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
