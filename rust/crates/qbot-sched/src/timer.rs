//! The timer model: one durable concept for group tasks and background jobs.

use qbot_agent::Chain;
use qbot_context::RunEnd;
use qbot_core::{GroupId, TimerId, UnixMillis};

/// Who created a group task. Recorded for observability; it grants nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Model,
    Owner,
}

/// A group task: wake the agent with fresh context at `due_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeSpec {
    pub group: GroupId,
    pub intent: String,
    pub chain: Chain,
    pub origin: Origin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    /// The whole nightly pipeline, in order: extraction, decay, backup, cleanup.
    Nightly,
    /// Extract every complete slice of one group.
    Extract,
    Decay,
    Backup,
    Report,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerKind {
    Wake(WakeSpec),
    Job {
        kind: JobKind,
        group: Option<GroupId>,
    },
}

/// What happens if the process dies while a timer is claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// A duplicate would be visible to users (a QQ send), so a claim is never replayed; the
    /// timer is marked interrupted instead.
    AtMostOnce,
    /// Safe to run again; the lease expires and the job is retried.
    Idempotent,
}

impl TimerKind {
    pub fn delivery(&self) -> Delivery {
        match self {
            TimerKind::Wake(_) => Delivery::AtMostOnce,
            TimerKind::Job { .. } => Delivery::Idempotent,
        }
    }

    pub fn group(&self) -> Option<GroupId> {
        match self {
            TimerKind::Wake(spec) => Some(spec.group),
            TimerKind::Job { group, .. } => *group,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerOutcome {
    Ran(RunEnd),
    /// The group was muted when the task came due; nothing ran.
    SkippedMuted,
    JobOk,
    JobFailed(String),
}

impl TimerOutcome {
    pub fn is_failure(&self) -> bool {
        match self {
            TimerOutcome::Ran(end) => matches!(
                end,
                RunEnd::Deadline
                    | RunEnd::ModelError
                    | RunEnd::Cancelled
                    | RunEnd::Environment
                    | RunEnd::Interrupted
            ),
            TimerOutcome::JobFailed(_) => true,
            TimerOutcome::SkippedMuted | TimerOutcome::JobOk => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerState {
    Pending,
    /// Running. Jobs also carry the lease deadline after which another worker may take over.
    Claimed {
        lease_until: Option<UnixMillis>,
    },
    Done(TimerOutcome),
    Cancelled,
    /// The process died while the timer was claimed. Terminal for at-most-once timers.
    Interrupted,
}

impl TimerState {
    pub fn is_pending(&self) -> bool {
        matches!(self, TimerState::Pending)
    }

    pub fn is_active(&self) -> bool {
        matches!(self, TimerState::Pending | TimerState::Claimed { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timer {
    pub id: TimerId,
    pub due_at: UnixMillis,
    pub kind: TimerKind,
    pub state: TimerState,
    /// Claims so far (retries of idempotent jobs count).
    pub attempts: u32,
}

impl Timer {
    pub fn wake(&self) -> Option<&WakeSpec> {
        match &self.kind {
            TimerKind::Wake(spec) => Some(spec),
            TimerKind::Job { .. } => None,
        }
    }
}

/// A new group task. `chain: None` starts a new chain rooted at the task itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWake {
    pub group: GroupId,
    pub intent: String,
    pub chain: Option<Chain>,
    pub origin: Origin,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WakeEdit {
    pub intent: Option<String>,
    pub due_at: Option<UnixMillis>,
}
