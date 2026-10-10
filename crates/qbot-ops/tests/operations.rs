#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use qbot_core::{AccountId, Clock, GroupId, UnixMillis};
use qbot_i18n::Locales;
use qbot_memory::{IdentityPolicy, MemoryIdentityStore};
use qbot_ops::{
    Extractor, Housekeeping, Operations, OpsConfig, OpsError, Parts, ReportSink, ReportSource,
    render_report,
};
use qbot_sched::{JobError, JobKind, JobRunner};
use qbot_store::ReportData;

#[derive(Debug)]
struct Fixed(AtomicI64);
impl Clock for Fixed {
    fn now(&self) -> UnixMillis {
        UnixMillis::new(self.0.load(Ordering::SeqCst))
    }
}

fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn owner(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

#[derive(Default)]
struct Extract {
    failing: Mutex<Vec<GroupId>>,
    done: Mutex<Vec<GroupId>>,
}

#[async_trait]
impl Extractor for Extract {
    async fn extract_group(&self, g: GroupId) -> Result<usize, JobError> {
        if self.failing.lock().unwrap().contains(&g) {
            return Err(JobError("the model is down".into()));
        }
        self.done.lock().unwrap().push(g);
        Ok(2)
    }
}

#[derive(Default)]
struct House {
    calls: Mutex<Vec<(String, i64)>>,
    /// Deleting finished timers fails.
    broken: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Housekeeping for House {
    async fn groups(&self) -> Result<Vec<GroupId>, OpsError> {
        Ok(vec![group(1), group(2), group(3)])
    }
    async fn expire_descriptions(&self, before: UnixMillis) -> Result<u64, OpsError> {
        self.calls
            .lock()
            .unwrap()
            .push(("descriptions".into(), before.get()));
        Ok(1)
    }
    async fn delete_finished_timers(&self, before: UnixMillis) -> Result<u64, OpsError> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(OpsError("the database is gone".into()));
        }
        self.calls
            .lock()
            .unwrap()
            .push(("timers".into(), before.get()));
        Ok(1)
    }
    async fn delete_runs(&self, before: UnixMillis) -> Result<u64, OpsError> {
        self.calls
            .lock()
            .unwrap()
            .push(("runs".into(), before.get()));
        Ok(1)
    }
}

struct Source(ReportData, Mutex<Vec<(i64, i64)>>);

#[async_trait]
impl ReportSource for Source {
    async fn collect(&self, from: UnixMillis, to: UnixMillis) -> Result<ReportData, OpsError> {
        self.1.lock().unwrap().push((from.get(), to.get()));
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct Sink {
    sent: Mutex<Vec<(i64, String)>>,
    refuse: Mutex<Vec<i64>>,
}

#[async_trait]
impl ReportSink for Sink {
    async fn send(&self, to: AccountId, text: &str) -> Result<(), OpsError> {
        if self.refuse.lock().unwrap().contains(&to.get()) {
            return Err(OpsError("not connected".into()));
        }
        self.sent.lock().unwrap().push((to.get(), text.to_owned()));
        Ok(())
    }
}

fn data() -> ReportData {
    ReportData {
        runs: 12,
        runs_addressed: 10,
        runs_wake: 2,
        run_ends: vec![("delivered".into(), 9), ("deadline".into(), 3)],
        error_classes: vec![("timeout".into(), 3)],
        model_calls: 30,
        failed_model_calls: 3,
        input_tokens: 2000,
        cached_tokens: 1500,
        output_tokens: 300,
        tool_calls: 20,
        tool_calls_not_ok: 1,
        messages: 480,
        groups_active: 3,
        groups_new: 1,
        groups_muted: 1,
        episodes: 4,
        jobs_pending: 0,
        jobs_failed: 1,
        tasks_interrupted: 0,
    }
}

/// The platform's cache: counts cleanings, and fails when told to.
#[derive(Default)]
struct Cache {
    cleaned: Mutex<u32>,
    down: Mutex<bool>,
}

#[async_trait::async_trait]
impl qbot_ops::PlatformCache for Cache {
    async fn clean(&self) -> Result<(), qbot_ops::OpsError> {
        if *self.down.lock().unwrap() {
            return Err(qbot_ops::OpsError("not connected".into()));
        }
        *self.cleaned.lock().unwrap() += 1;
        Ok(())
    }
}

struct Rig {
    ops: Operations,
    cache: Arc<Cache>,
    extract: Arc<Extract>,
    house: Arc<House>,
    sink: Arc<Sink>,
    source: Arc<Source>,
}

/// 2026-10-04 12:00 in Shanghai.
const NOON: i64 = 1_791_086_400_000;

fn rig(owners: Vec<AccountId>) -> Rig {
    let extract = Arc::new(Extract::default());
    let house = Arc::new(House::default());
    let sink = Arc::new(Sink::default());
    let source = Arc::new(Source(data(), Mutex::default()));
    let cache = Arc::new(Cache::default());
    let ops = Operations::new(Parts {
        cfg: OpsConfig {
            backup: None,
            alias_unused: Duration::from_secs(30 * 86_400),
            description_ttl: Duration::from_secs(15 * 86_400),
            records_keep: Some(Duration::from_secs(90 * 86_400)),
            owners,
            zone: jiff::tz::TimeZone::get("Asia/Shanghai").unwrap(),
            fact_decay: qbot_memory::facts::DecayPolicy::default(),
        },
        clock: Arc::new(Fixed(AtomicI64::new(NOON))),
        extractor: extract.clone(),
        housekeeping: house.clone(),
        identity: Arc::new(MemoryIdentityStore::new(IdentityPolicy::default())),
        facts: Arc::new(qbot_memory::facts::MemoryFactStore::new()),
        report: source.clone(),
        sink: sink.clone(),
        locales: Locales::english(),
        platform_cache: Some(cache.clone()),
    });
    Rig {
        ops,
        cache,
        extract,
        house,
        sink,
        source,
    }
}

#[tokio::test]
async fn the_nightly_pipeline_runs_every_stage_even_after_a_failure() {
    let r = rig(vec![owner(1)]);
    r.extract.failing.lock().unwrap().push(group(2));
    r.ops
        .run(JobKind::Nightly, None)
        .await
        .expect("a failed extraction waits for the next batch or night, not a retry of the night");
    assert_eq!(
        *r.extract.done.lock().unwrap(),
        [group(1), group(3)],
        "the other groups were still extracted"
    );
    // Decay and cleanup ran after the failed extraction, with cutoffs counted back from now.
    let day = 86_400_000;
    assert_eq!(
        *r.house.calls.lock().unwrap(),
        [
            ("descriptions".to_owned(), NOON - 15 * day),
            ("timers".to_owned(), NOON - 90 * day),
            ("runs".to_owned(), NOON - 90 * day)
        ]
    );

    // A failed upkeep stage fails the night, so it is retried.
    r.house.broken.store(true, Ordering::SeqCst);
    let error = r.ops.run(JobKind::Nightly, None).await.unwrap_err();
    assert!(error.0.contains("cleanup: "), "{error}");
}

#[tokio::test]
async fn a_clean_night_succeeds_and_skips_backup_when_it_is_switched_off() {
    let r = rig(vec![owner(1)]);
    r.ops.run(JobKind::Nightly, None).await.unwrap();
    assert_eq!(r.extract.done.lock().unwrap().len(), 3);
    // Running the backup job itself while backups are off is an error, not a silent no-op.
    let error = r.ops.run(JobKind::Backup, None).await.unwrap_err();
    assert!(error.0.contains("switched off"), "{error}");
    assert!(
        r.ops.run(JobKind::Extract, None).await.is_err(),
        "an extract job needs a group"
    );
    assert_eq!(r.ops.run(JobKind::Extract, Some(group(9))).await, Ok(()));
}

#[tokio::test]
async fn the_daily_report_covers_the_previous_local_day_and_reaches_every_owner() {
    let r = rig(vec![owner(1), owner(2)]);
    r.ops.run(JobKind::Report, None).await.unwrap();
    // Yesterday in Shanghai: 2026-10-03 00:00 to 2026-10-04 00:00 local.
    let day = 86_400_000;
    let midnight = NOON - 12 * 3_600_000;
    assert_eq!(*r.source.1.lock().unwrap(), [(midnight - day, midnight)]);
    let sent = r.sink.sent.lock().unwrap().clone();
    assert_eq!(sent.iter().map(|s| s.0).collect::<Vec<_>>(), [1, 2]);
    assert!(
        sent[0].1.starts_with("Daily report | 2026-10-03\n"),
        "{}",
        sent[0].1
    );
}

#[tokio::test]
async fn an_owner_who_cannot_be_reached_fails_the_job_without_blocking_the_others() {
    let r = rig(vec![owner(1), owner(2), owner(3)]);
    r.sink.refuse.lock().unwrap().push(2);
    let error = r.ops.run(JobKind::Report, None).await.unwrap_err();
    assert!(error.0.contains("2: not connected"), "{error}");
    assert_eq!(
        r.sink
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.0)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    let nobody = rig(vec![]);
    assert!(
        nobody
            .ops
            .run(JobKind::Report, None)
            .await
            .unwrap_err()
            .0
            .contains("nobody")
    );
}

#[test]
fn the_report_reads_as_a_short_summary() {
    let text = render_report(
        &Locales::english(),
        "2026-10-03",
        &data(),
        true,
        Some(Duration::from_secs(5 * 3600 + 20)),
    );
    assert_eq!(
        text,
        "Daily report | 2026-10-03\n\
         Replies: 12 (10 addressed, 2 scheduled)\n\
         How runs ended: delivered 9, deadline 3\n\
         Model errors: timeout 3\n\
         Model calls: 30 (3 failed)\n\
         Tokens: 2000 in (75% from cache, 1500 tokens), 300 out\n\
         Tool calls: 20 (1 not ok)\n\
         Chat: 480 messages archived; replied in 3 groups\n\
         Groups: 1 new, 1 muted\n\
         Memory: 4 new episodes\n\
         Jobs: 0 pending, 1 failed; 0 scheduled tasks interrupted\n\
         Last backup: 5 hours ago"
    );
    let quiet = render_report(
        &Locales::english(),
        "d",
        &ReportData::default(),
        false,
        None,
    );
    assert!(
        !quiet.contains("backup")
            && !quiet.contains("How runs ended")
            && quiet.contains("Tokens: 0 in (0% from cache"),
        "{quiet}"
    );
    assert!(
        render_report(&Locales::english(), "d", &ReportData::default(), true, None)
            .ends_with("Last backup: none found")
    );
}

#[tokio::test]
async fn the_nightly_cleanup_asks_the_platform_to_clean_its_cache_best_effort() {
    let r = rig(vec![owner(1)]);
    r.ops.run(JobKind::Nightly, None).await.unwrap();
    assert_eq!(*r.cache.cleaned.lock().unwrap(), 1);
    // A platform that cannot be reached does not fail the night.
    *r.cache.down.lock().unwrap() = true;
    r.ops.run(JobKind::Cleanup, None).await.unwrap();
    assert_eq!(*r.cache.cleaned.lock().unwrap(), 1);
}
