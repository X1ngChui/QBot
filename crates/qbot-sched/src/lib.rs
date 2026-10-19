//! Durable timers: group tasks that wake the agent, and idempotent background jobs, in one
//! model with one scheduler. Storage is a trait; the in-memory store defines the semantics.

pub mod conformance;
mod recurring;
mod repeat;
mod scheduler;
mod service;
mod store;
mod timer;

pub use recurring::{Recurrence, RecurrenceError, Recurring};
pub use repeat::{
    Repeat, RepeatError, Rule, TimeOfDay, WEEKDAY_CODES, Weekdays, Zone, interval_text,
};
pub use scheduler::{JobError, JobRunner, Scheduler, SchedulerConfig, TaskId, Tick, TokioClock};
pub use service::{TaskError, TaskLimits, TaskService, When};
pub use store::{MemoryTimerStore, Recovery, StoreError, TimerStore};
pub use timer::{
    Delivery, JobKind, LastRun, NewWake, Origin, Timer, TimerKind, TimerOutcome, TimerState,
    WakeEdit, WakeSpec,
};
