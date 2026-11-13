#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use qbot_core::{AccountId, Clock, UnixMillis};
use qbot_i18n::Locales;
use qbot_llm::{CallKind, Purpose};
use qbot_ops::{
    Housekeeping, Operations, OpsConfig, OpsError, Parts, ReportSink, ReportSource, render_report,
};
use qbot_store::{ReportData, UsageLine, UsageSummary};

#[derive(Debug)]
struct Fixed(AtomicI64);
impl Clock for Fixed {
    fn now(&self) -> UnixMillis {
        UnixMillis::new(self.0.load(Ordering::SeqCst))
    }
}

fn owner(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

#[derive(Default)]
struct House {
    calls: Mutex<Vec<(String, i64)>>,
    /// Deleting finished timers fails.
    broken: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Housekeeping for House {
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

fn line(
    purpose: Option<Purpose>,
    kind: CallKind,
    (calls, requests, failed): (u64, u64, u64),
    (input_tokens, cached_tokens, output_tokens): (u64, Option<u64>, u64),
    credits: Option<u64>,
) -> UsageLine {
    UsageLine {
        purpose,
        kind,
        calls,
        requests,
        failed,
        input_tokens,
        cached_tokens,
        output_tokens,
        credits,
    }
}

fn data() -> ReportData {
    ReportData {
        runs: 12,
        runs_addressed: 10,
        runs_wake: 2,
        runs_spontaneous: 0,
        run_ends: vec![("delivered".into(), 9), ("deadline".into(), 3)],
        error_classes: vec![("timeout".into(), 3)],
        usage: UsageSummary {
            runs: 12,
            tool_calls: 20,
            tool_calls_not_ok: 1,
            calls: vec![
                line(
                    Some(Purpose::Reply),
                    CallKind::Model,
                    (30, 32, 3),
                    (2000, Some(1500), 300),
                    None,
                ),
                line(
                    Some(Purpose::Reply),
                    CallKind::Search,
                    (2, 2, 0),
                    (0, None, 0),
                    Some(2),
                ),
                line(
                    Some(Purpose::Extraction),
                    CallKind::Model,
                    (3, 4, 0),
                    (9000, None, 800),
                    None,
                ),
                line(
                    Some(Purpose::Extraction),
                    CallKind::Embedding,
                    (3, 3, 0),
                    (120, None, 0),
                    None,
                ),
                line(
                    Some(Purpose::Picture),
                    CallKind::Model,
                    (5, 5, 1),
                    (4000, Some(0), 200),
                    None,
                ),
                line(
                    Some(Purpose::Reply),
                    CallKind::Extract,
                    (1, 1, 0),
                    (0, None, 0),
                    None,
                ),
            ],
        },
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
    house: Arc<House>,
    sink: Arc<Sink>,
    source: Arc<Source>,
}

/// 2026-10-04 12:00 in Shanghai.
const NOON: i64 = 1_791_086_400_000;

fn rig(owners: Vec<AccountId>) -> Rig {
    let house = Arc::new(House::default());
    let sink = Arc::new(Sink::default());
    let source = Arc::new(Source(data(), Mutex::default()));
    let cache = Arc::new(Cache::default());
    let ops = Operations::new(Parts {
        cfg: OpsConfig {
            backup: None,
            description_ttl: Duration::from_secs(15 * 86_400),
            records_keep: Some(Duration::from_secs(90 * 86_400)),
            owners,
            zone: jiff::tz::TimeZone::get("Asia/Shanghai").unwrap(),
        },
        clock: Arc::new(Fixed(AtomicI64::new(NOON))),
        housekeeping: house.clone(),
        report: source.clone(),
        sink: sink.clone(),
        locales: Locales::english(),
        platform_cache: Some(cache.clone()),
    });
    Rig {
        ops,
        cache,
        house,
        sink,
        source,
    }
}

#[tokio::test]
async fn the_nightly_pipeline_runs_every_stage_even_after_a_failure() {
    let r = rig(vec![owner(1)]);
    r.ops.nightly().await.unwrap();
    // Decay and cleanup ran, with cutoffs counted back from now.
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
    let error = r.ops.nightly().await.unwrap_err();
    assert!(error.0.contains("cleanup: "), "{error}");
}

#[tokio::test]
async fn a_clean_night_succeeds_and_skips_backup_when_it_is_switched_off() {
    let r = rig(vec![owner(1)]);
    r.ops.nightly().await.unwrap();
    // Asking for a backup while backups are off is an error, not a silent no-op.
    let error = r.ops.backup().await.unwrap_err();
    assert!(error.0.contains("switched off"), "{error}");
}

#[tokio::test]
async fn the_daily_report_covers_the_previous_local_day_and_reaches_every_owner() {
    let r = rig(vec![owner(1), owner(2)]);
    r.ops.daily_report().await.unwrap();
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
    let error = r.ops.daily_report().await.unwrap_err();
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
            .daily_report()
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
         Replies: 12 (10 addressed, 2 scheduled, 0 on its own)\n\
         How runs ended: delivered 9, deadline 3\n\
         Model errors: timeout 3\n\
         Tool calls: 20 (1 not ok)\n\
         Replies, model: 30 calls (32 requests, 3 failed); 2000 tokens in (1500 cached, 75%), 300 out\n\
         Replies, web search: 2 calls (2 requests, 0 failed); 2 credits\n\
         Memory, model: 3 calls (4 requests, 0 failed); 9000 tokens in (cache not reported), 800 out\n\
         Memory, embeddings: 3 calls (3 requests, 0 failed); 120 tokens\n\
         Pictures, model: 5 calls (5 requests, 1 failed); 4000 tokens in (0 cached, 0%), 200 out\n\
         Replies, page reading: 1 call (1 request, 0 failed); credits not reported\n\
         All provider calls: 44 (47 requests, 4 failed); 15120 tokens in, 1300 out; 2 credits\n\
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
            && quiet.contains("Provider calls: none"),
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
    r.ops.nightly().await.unwrap();
    assert_eq!(*r.cache.cleaned.lock().unwrap(), 1);
    // A platform that cannot be reached does not fail the night.
    *r.cache.down.lock().unwrap() = true;
    r.ops.cleanup().await.unwrap();
    assert_eq!(*r.cache.cleaned.lock().unwrap(), 1);
}

#[test]
fn counts_of_one_read_in_the_singular() {
    let one = ReportData {
        messages: 1,
        groups_active: 1,
        episodes: 1,
        tasks_interrupted: 1,
        usage: UsageSummary {
            calls: vec![
                line(
                    Some(Purpose::Reply),
                    CallKind::Model,
                    (1, 1, 0),
                    (1, Some(1), 1),
                    None,
                ),
                line(
                    Some(Purpose::Reply),
                    CallKind::Search,
                    (1, 1, 0),
                    (0, None, 0),
                    Some(1),
                ),
            ],
            ..UsageSummary::default()
        },
        ..ReportData::default()
    };
    let text = render_report(
        &Locales::english(),
        "d",
        &one,
        true,
        Some(Duration::from_secs(3600)),
    );
    for expected in [
        "Chat: 1 message archived; replied in 1 group",
        "Memory: 1 new episode\n",
        "0 failed; 1 scheduled task interrupted",
        "Replies, model: 1 call (1 request, 0 failed); 1 token in (1 cached, 100%), 1 out",
        "Replies, web search: 1 call (1 request, 0 failed); 1 credit",
        "All provider calls: 2 (2 requests, 0 failed); 1 token in, 1 out; 1 credit",
        "Last backup: 1 hour ago",
    ] {
        assert!(text.contains(expected), "{expected:?} in:\n{text}");
    }
}
