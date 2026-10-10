use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};
use qbot_core::{AccountId, Clock, GroupId, UnixMillis};
use qbot_i18n::Locales;
use qbot_memory::IdentityStore;
use qbot_memory::facts::{DecayPolicy, FactStore, decay as decay_facts};
use qbot_sched::{JobError, JobKind, JobRunner};

use crate::backup::{BackupConfig, create_verified_backup, newest_backup_age, rotate_backups};
use crate::ports::{Extractor, Housekeeping, PlatformCache, ReportSink, ReportSource};
use crate::report::render_report;

#[derive(Debug, Clone)]
pub struct OpsConfig {
    /// `None`: backups are switched off.
    pub backup: Option<BackupConfig>,
    /// A name nobody has vouched for and no evidence has supported for this long is dropped.
    pub alias_unused: Duration,
    /// Picture descriptions older than this are forgotten (and so redone when next needed).
    pub description_ttl: Duration,
    /// Finished runs and finished scheduled tasks older than this are deleted. `None` keeps
    /// them forever.
    pub records_keep: Option<Duration>,
    pub owners: Vec<AccountId>,
    pub zone: TimeZone,
    /// How facts fade when nothing confirms them.
    pub fact_decay: DecayPolicy,
}

impl OpsConfig {
    /// The nightly upkeep around the deployment's choices (backups, how long records are kept,
    /// who gets the report, the time zone, how facts fade). A name candidate nothing has supported
    /// for one default fact half-life is dropped: a lead from chat fades like a fact. A cached
    /// picture description is dropped after 15 days (it serves reposts, which come within days,
    /// and must not grow with the archive; what was archived keeps its words).
    pub fn new(
        backup: Option<BackupConfig>,
        records_keep: Option<Duration>,
        owners: Vec<AccountId>,
        zone: TimeZone,
        fact_decay: DecayPolicy,
    ) -> Self {
        const DAY: Duration = Duration::from_secs(86_400);
        Self {
            backup,
            alias_unused: fact_decay.default,
            description_ttl: DAY * 15,
            records_keep,
            owners,
            zone,
            fact_decay,
        }
    }
}

pub struct Operations {
    cfg: OpsConfig,
    clock: Arc<dyn Clock>,
    extractor: Arc<dyn Extractor>,
    housekeeping: Arc<dyn Housekeeping>,
    identity: Arc<dyn IdentityStore>,
    facts: Arc<dyn FactStore>,
    report: Arc<dyn ReportSource>,
    sink: Arc<dyn ReportSink>,
    locales: Locales,
    platform_cache: Option<Arc<dyn PlatformCache>>,
}

impl std::fmt::Debug for Operations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Operations").finish_non_exhaustive()
    }
}

/// The ports and settings [`Operations`] is built from.
#[allow(missing_debug_implementations)]
pub struct Parts {
    pub cfg: OpsConfig,
    pub clock: Arc<dyn Clock>,
    pub extractor: Arc<dyn Extractor>,
    pub housekeeping: Arc<dyn Housekeeping>,
    pub identity: Arc<dyn IdentityStore>,
    pub facts: Arc<dyn FactStore>,
    pub report: Arc<dyn ReportSource>,
    pub sink: Arc<dyn ReportSink>,
    pub locales: Locales,
    /// `None`: the platform's file cache is left alone.
    pub platform_cache: Option<Arc<dyn PlatformCache>>,
}

fn failed(error: impl std::fmt::Display) -> JobError {
    JobError(error.to_string())
}

impl Operations {
    pub fn new(parts: Parts) -> Self {
        Self {
            cfg: parts.cfg,
            clock: parts.clock,
            extractor: parts.extractor,
            housekeeping: parts.housekeeping,
            identity: parts.identity,
            facts: parts.facts,
            report: parts.report,
            sink: parts.sink,
            locales: parts.locales,
            platform_cache: parts.platform_cache,
        }
    }

    fn before(&self, age: Duration) -> UnixMillis {
        let now = self.clock.now();
        UnixMillis::new(
            now.get()
                .saturating_sub(i64::try_from(age.as_millis()).unwrap_or(i64::MAX)),
        )
    }

    /// Extract every group's complete slices. A group that fails does not stop the others; the
    /// errors are reported together.
    pub async fn extract_all(&self) -> Result<usize, JobError> {
        let groups = self.housekeeping.groups().await.map_err(failed)?;
        let (mut episodes, mut errors) = (0, Vec::new());
        for group in groups {
            match self.extractor.extract_group(group).await {
                Ok(n) => episodes += n,
                Err(error) => errors.push(format!("group {}: {error}", group.get())),
            }
        }
        if errors.is_empty() {
            Ok(episodes)
        } else {
            Err(JobError(errors.join("; ")))
        }
    }

    /// Drop stale name candidates, facts nothing confirms any more, and stale picture
    /// descriptions.
    pub async fn decay(&self) -> Result<(), JobError> {
        let expired = self
            .identity
            .expire_candidates(self.before(self.cfg.alias_unused))
            .await
            .map_err(failed)?;
        let forgotten = self
            .housekeeping
            .expire_descriptions(self.before(self.cfg.description_ttl))
            .await
            .map_err(failed)?;
        let faded = decay_facts(&*self.facts, &self.cfg.fact_decay, self.clock.now())
            .await
            .map_err(failed)?;
        tracing::info!(expired, forgotten, faded, "decay finished");
        Ok(())
    }

    /// Delete finished runs and finished timers older than the record retention.
    pub async fn cleanup(&self) -> Result<(), JobError> {
        let (timers, runs) = match self.cfg.records_keep {
            Some(keep) => (
                self.housekeeping
                    .delete_finished_timers(self.before(keep))
                    .await
                    .map_err(failed)?,
                self.housekeeping
                    .delete_runs(self.before(keep))
                    .await
                    .map_err(failed)?,
            ),
            None => (0, 0),
        };
        // Best effort: the platform may be disconnected at night. Failing would retry the whole
        // pipeline (another extraction pass, another backup) for a cache the next night cleans.
        if let Some(cache) = &self.platform_cache
            && let Err(error) = cache.clean().await
        {
            tracing::warn!(%error, "the platform's file cache was not cleaned");
        }
        tracing::info!(timers, runs, "cleanup finished");
        Ok(())
    }

    pub async fn backup(&self) -> Result<(), JobError> {
        let Some(cfg) = &self.cfg.backup else {
            return Err(JobError(
                "backups are switched off (maintenance.backups = 0)".into(),
            ));
        };
        let made = create_verified_backup(cfg).await.map_err(failed)?;
        let removed = rotate_backups(&cfg.dir, &cfg.prefix, cfg.keep);
        tracing::info!(path = %made.path.display(), bytes = made.bytes, rotated_out = removed, "backup verified");
        Ok(())
    }

    /// The whole pipeline in order. Every stage runs even if an earlier one failed (a failed
    /// extraction must not cost a night's backup). The job fails, and is retried, if decay, the
    /// backup or cleanup did; a failed extraction is logged and left to the group's next filled
    /// batch or the next night, because a retry of the pipeline would also take another backup
    /// and rotate an older one out.
    pub async fn nightly(&self) -> Result<(), JobError> {
        let mut failures = Vec::new();
        match self.extract_all().await {
            Ok(episodes) => tracing::info!(episodes, "extraction finished"),
            Err(error) => {
                tracing::error!(error = %error.0, "extraction failed; retried with the next batch or night")
            }
        }
        if let Err(error) = self.decay().await {
            failures.push(format!("decay: {error}"));
        }
        if self.cfg.backup.is_some()
            && let Err(error) = self.backup().await
        {
            failures.push(format!("backup: {error}"));
        }
        if let Err(error) = self.cleanup().await {
            failures.push(format!("cleanup: {error}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(JobError(failures.join("; ")))
        }
    }

    /// The previous local day, as `[from, to)` and its date.
    fn yesterday(&self) -> Option<(UnixMillis, UnixMillis, String)> {
        let now = Timestamp::from_millisecond(self.clock.now().get())
            .ok()?
            .to_zoned(self.cfg.zone.clone());
        let today = now.start_of_day().ok()?;
        let before = today
            .checked_sub(Span::new().days(1))
            .ok()?
            .start_of_day()
            .ok()?;
        let millis = |z: &jiff::Zoned| UnixMillis::new(z.timestamp().as_millisecond());
        Some((
            millis(&before),
            millis(&today),
            before.strftime("%Y-%m-%d").to_string(),
        ))
    }

    /// Build yesterday's report and send it to every owner. Owners are all tried; any failure
    /// fails the job so it is retried.
    pub async fn daily_report(&self) -> Result<(), JobError> {
        if self.cfg.owners.is_empty() {
            return Err(JobError(
                "there is nobody to send the report to (bot.owners is empty)".into(),
            ));
        }
        let (from, to, date) = self
            .yesterday()
            .ok_or_else(|| JobError("cannot work out the report's day".into()))?;
        let data = self.report.collect(from, to).await.map_err(failed)?;
        let backups_on = self.cfg.backup.is_some();
        let age = self
            .cfg
            .backup
            .as_ref()
            .and_then(|b| newest_backup_age(&b.dir, &b.prefix));
        let text = render_report(&self.locales, &date, &data, backups_on, age);
        let mut errors = Vec::new();
        for owner in &self.cfg.owners {
            if let Err(error) = self.sink.send(*owner, &text).await {
                errors.push(format!("{}: {error}", owner.get()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(JobError(format!(
                "report not delivered to {}",
                errors.join("; ")
            )))
        }
    }
}

#[async_trait]
impl JobRunner for Operations {
    async fn run(&self, kind: JobKind, group: Option<GroupId>) -> Result<(), JobError> {
        match (kind, group) {
            (JobKind::Nightly, _) => self.nightly().await,
            (JobKind::Extract, Some(group)) => {
                self.extractor.extract_group(group).await.map(|_| ())
            }
            (JobKind::Extract, None) => Err(JobError("an extract job needs a group".into())),
            (JobKind::Decay, _) => self.decay().await,
            (JobKind::Backup, _) => self.backup().await,
            (JobKind::Cleanup, _) => self.cleanup().await,
            (JobKind::Report, _) => self.daily_report().await,
        }
    }
}
