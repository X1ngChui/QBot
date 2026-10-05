//! Durable timers: group tasks that wake the agent, and idempotent background jobs, in one
//! model with one scheduler. Storage is a trait; the in-memory store defines the semantics.

pub mod conformance;
mod recurring;
mod scheduler;
mod service;
mod store;
mod timer;

pub use recurring::{Recurrence, RecurrenceError, Recurring};
pub use scheduler::{JobError, JobRunner, Scheduler, SchedulerConfig, TaskId, Tick, TokioClock};
pub use service::{TaskError, TaskLimits, TaskService, When};
pub use store::{MemoryTimerStore, Recovery, StoreError, TimerStore};
pub use timer::{
    Delivery, JobKind, NewWake, Origin, Timer, TimerKind, TimerOutcome, TimerState, WakeEdit,
    WakeSpec,
};
