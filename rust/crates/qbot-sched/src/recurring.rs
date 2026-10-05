//! Recurring schedules: a cron expression, evaluated in a time zone, that produces a job at each
//! occurrence.
//!
//! Firing goes through the timer store in one atomic step per occurrence, so an occurrence is
//! turned into a job exactly once however many times the process restarts. A schedule that was
//! missed while the process was down fires once, for its most recent occurrence, when the
//! process comes back (a missed nightly run catches up rather than being skipped); a schedule
//! that has never run starts from now and does not fire for the past.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;
use qbot_core::{Clock, UnixMillis};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::store::{StoreError, TimerStore};
use crate::timer::JobKind;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecurrenceError {
    #[error("`{expression}` is not a valid cron expression: {reason}")]
    BadCron { expression: String, reason: String },
    #[error("`{0}` is not a known time zone")]
    BadZone(String),
}

#[derive(Debug, Clone)]
pub struct Recurrence {
    pub name: String,
    pub cron: Cron,
    pub kind: JobKind,
}

impl Recurrence {
    /// `expression` is standard five-field cron (minute hour day month weekday).
    pub fn new(name: &str, expression: &str, kind: JobKind) -> Result<Self, RecurrenceError> {
        let cron = Cron::from_str(expression).map_err(|e| RecurrenceError::BadCron {
            expression: expression.to_owned(),
            reason: e.to_string(),
        })?;
        Ok(Self {
            name: name.to_owned(),
            cron,
            kind,
        })
    }
}

pub struct Recurring {
    store: Arc<dyn TimerStore>,
    clock: Arc<dyn Clock>,
    zone: Tz,
    schedules: Vec<Recurrence>,
    wake: Arc<Notify>,
}

impl std::fmt::Debug for Recurring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recurring")
            .field("schedules", &self.schedules.len())
            .finish_non_exhaustive()
    }
}

/// `at` in the zone, truncated to the whole second. Cron has no finer unit, and croner carries
/// the input's fraction into the occurrences it returns, so without truncation two readings in
/// the same second would name different "latest" occurrences and fire one occurrence twice.
fn local(zone: Tz, at: UnixMillis) -> DateTime<Tz> {
    zone.from_utc_datetime(
        &DateTime::<Utc>::from_timestamp_millis(at.get().div_euclid(1000) * 1000)
            .unwrap_or_default()
            .naive_utc(),
    )
}

impl Recurring {
    /// `wake` is the notification the scheduler waits on; it is nudged whenever a job is created.
    pub fn new(
        store: Arc<dyn TimerStore>,
        clock: Arc<dyn Clock>,
        zone: &str,
        schedules: Vec<Recurrence>,
        wake: Arc<Notify>,
    ) -> Result<Self, RecurrenceError> {
        let zone: Tz = zone
            .parse()
            .map_err(|_| RecurrenceError::BadZone(zone.to_owned()))?;
        Ok(Self {
            store,
            clock,
            zone,
            schedules,
            wake,
        })
    }

    fn previous(&self, schedule: &Recurrence, now: UnixMillis) -> Option<UnixMillis> {
        let at = schedule
            .cron
            .find_previous_occurrence(&local(self.zone, now), true)
            .ok()?;
        Some(UnixMillis::new(at.timestamp_millis()))
    }

    fn next(&self, schedule: &Recurrence, now: UnixMillis) -> Option<UnixMillis> {
        let at = schedule
            .cron
            .find_next_occurrence(&local(self.zone, now), false)
            .ok()?;
        Some(UnixMillis::new(at.timestamp_millis()))
    }

    /// Bring every schedule up to date: start new ones from now, and fire the latest occurrence a
    /// schedule missed. Returns how many jobs were created.
    pub async fn catch_up(&self) -> Result<usize, StoreError> {
        let now = self.clock.now();
        let mut created = 0;
        for schedule in &self.schedules {
            let Some(latest) = self.previous(schedule, now) else {
                continue;
            };
            self.store.seed_recurrence(&schedule.name, latest).await?;
            if self
                .store
                .fire_recurrence(&schedule.name, latest, schedule.kind)
                .await?
            {
                created += 1;
            }
        }
        if created > 0 {
            self.wake.notify_one();
        }
        Ok(created)
    }

    /// Fire every occurrence that has come due. Returns how many jobs were created.
    pub async fn fire_due(&self) -> Result<usize, StoreError> {
        self.fire_through(self.clock.now()).await
    }

    /// Fire the latest occurrence at or before `now` of every schedule that has not fired it.
    async fn fire_through(&self, now: UnixMillis) -> Result<usize, StoreError> {
        let mut created = 0;
        for schedule in &self.schedules {
            let Some(latest) = self.previous(schedule, now) else {
                continue;
            };
            if self
                .store
                .fire_recurrence(&schedule.name, latest, schedule.kind)
                .await?
            {
                created += 1;
            }
        }
        if created > 0 {
            self.wake.notify_one();
        }
        Ok(created)
    }

    /// The next occurrence of any schedule after `now`.
    fn next_occurrence(&self, now: UnixMillis) -> Option<UnixMillis> {
        self.schedules
            .iter()
            .filter_map(|s| self.next(s, now))
            .min()
    }

    /// The time until the next occurrence of any schedule.
    pub fn until_next(&self) -> Option<Duration> {
        let now = self.clock.now();
        self.next_occurrence(now).map(|at| at.since(now))
    }

    /// Catch up, then fire each occurrence as it arrives, until `shutdown`.
    pub async fn run(&self, shutdown: CancellationToken) -> Result<(), StoreError> {
        // A long sleep is re-evaluated at least hourly, which also absorbs clock changes.
        const LONGEST: Duration = Duration::from_secs(3600);
        while let Err(error) = self.catch_up().await {
            tracing::error!(%error, "recurring schedules could not catch up; retrying");
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
        loop {
            let now = self.clock.now();
            let target = self
                .next_occurrence(now)
                .filter(|at| at.since(now) <= LONGEST);
            let wait = target.map_or(LONGEST, |at| at.since(now));
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(wait) => {}
            }
            // A sleep can end a moment before the occurrence it waited for. That occurrence is
            // fired regardless: asking the clock again could find it just past and wait a day.
            let now = self.clock.now();
            let at = target.map_or(now, |at| now.max(at));
            // A store that fails is tried again shortly, for the same occurrence: firing is
            // idempotent, and a schedule must not stop until a restart.
            while let Err(error) = self.fire_through(at).await {
                tracing::error!(%error, "a recurring job could not be created; retrying");
                tokio::select! {
                    () = shutdown.cancelled() => return Ok(()),
                    () = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
            }
        }
    }
}
