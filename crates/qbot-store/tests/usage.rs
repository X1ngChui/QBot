#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use qbot_agent::{UsageDetail, UsageEvent, UsageSink};
use qbot_context::{ErrorKind, Outcome, RefusalReason, RunEnd};
use qbot_core::{GroupId, RunId, UnixMillis};
use qbot_llm::{Attribution, CacheUsage, CallKind, Meter, ProviderCall, Purpose, Usage};
use qbot_store::{PgAdmin, PgUsageSink};
use sqlx::Row;

fn group() -> GroupId {
    GroupId::new(5).unwrap()
}

fn event(turn: u32, latency_ms: u64, detail: UsageDetail) -> UsageEvent {
    UsageEvent {
        group: group(),
        run: RunId::new(11),
        turn,
        latency: Duration::from_millis(latency_ms),
        detail,
    }
}

fn usage(input: u32, cached: Option<u32>, output: u32) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cache: cached.map_or(CacheUsage::NotReported, |hit_tokens| CacheUsage::Reported {
            hit_tokens,
        }),
        reasoning_tokens: Some(3),
        reported: true,
    }
}

fn reply(turn: u32) -> Option<Attribution> {
    Some(Attribution {
        purpose: Purpose::Reply,
        group: group(),
        run: Some(RunId::new(11)),
        turn: Some(turn),
    })
}

fn background(purpose: Purpose) -> Option<Attribution> {
    Some(Attribution::group(purpose, group()))
}

fn call(
    attribution: Option<Attribution>,
    kind: CallKind,
    requests: u32,
    tokens: Option<Usage>,
    credits: Option<u32>,
    error: Option<&'static str>,
) -> ProviderCall {
    ProviderCall {
        attribution,
        kind,
        provider: "p".into(),
        model: "m".into(),
        error,
        requests,
        tokens,
        credits,
        latency: Duration::from_millis(100),
    }
}

#[tokio::test]
async fn run_tool_and_provider_events_are_stored_and_flushed_on_shutdown() {
    let db = db!();
    let recorder = PgUsageSink::start(db.pool().clone(), ManualClock::new(T0));
    let sink = recorder.sink();
    Meter::record(
        &*sink,
        call(
            reply(1),
            CallKind::Model,
            2,
            Some(usage(1000, Some(900), 50)),
            None,
            None,
        ),
    );
    Meter::record(
        &*sink,
        call(
            reply(2),
            CallKind::Model,
            3,
            None,
            None,
            Some("rate_limited"),
        ),
    );
    for outcome in [
        Outcome::Ok,
        Outcome::Refused(RefusalReason::LimitReached),
        Outcome::Error(ErrorKind::Timeout),
    ] {
        UsageSink::record(
            &*sink,
            event(
                1,
                10,
                UsageDetail::Tool {
                    name: "send_messages".into(),
                    outcome,
                },
            ),
        );
    }
    UsageSink::record(
        &*sink,
        event(
            3,
            0,
            UsageDetail::RunEnded {
                end: RunEnd::Delivered,
                usage: usage(2200, Some(1900), 70),
                turns: 3,
                tool_calls: 3,
            },
        ),
    );
    Meter::record(
        &*sink,
        call(
            background(Purpose::Extraction),
            CallKind::Embedding,
            2,
            Some(usage(40, None, 0)),
            None,
            None,
        ),
    );
    Meter::record(
        &*sink,
        call(reply(2), CallKind::Search, 1, None, Some(2), None),
    );
    recorder.shutdown().await;
    // After shutdown: dropped silently.
    Meter::record(&*sink, call(None, CallKind::Model, 1, None, None, None));

    let rows = sqlx::query(
        "SELECT kind, purpose, group_id, run_id, turn, status, input_tokens, cached_tokens, \
         requests, count, credits, latency_ms, at_ms FROM usage_event ORDER BY id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 8);
    let first = &rows[0];
    assert_eq!(
        (
            first.get::<String, _>("kind"),
            first.get::<Option<String>, _>("purpose"),
            first.get::<Option<i64>, _>("run_id"),
            first.get::<Option<i32>, _>("turn"),
        ),
        ("model".into(), Some("reply".into()), Some(11), Some(1))
    );
    assert_eq!(
        (
            first.get::<Option<i64>, _>("input_tokens"),
            first.get::<Option<i64>, _>("cached_tokens"),
            first.get::<Option<i32>, _>("requests"),
            first.get::<i64, _>("latency_ms"),
            first.get::<i64, _>("at_ms")
        ),
        (Some(1000), Some(900), Some(2), 100, T0)
    );
    let failed = &rows[1];
    assert_eq!(failed.get::<String, _>("status"), "rate_limited");
    assert_eq!(
        (
            failed.get::<Option<i64>, _>("input_tokens"),
            failed.get::<Option<i32>, _>("requests")
        ),
        (None, Some(3)),
        "a failed call keeps its requests and reports no tokens"
    );
    let statuses: Vec<String> = rows[2..5].iter().map(|r| r.get("status")).collect();
    assert_eq!(statuses, ["ok", "refused:limitreached", "error:timeout"]);
    assert_eq!(rows[5].get::<String, _>("status"), "delivered");
    assert_eq!(rows[5].get::<Option<i32>, _>("count"), Some(3));
    let embedding = &rows[6];
    assert_eq!(
        (
            embedding.get::<Option<String>, _>("purpose"),
            embedding.get::<Option<i64>, _>("group_id"),
            embedding.get::<Option<i64>, _>("run_id"),
            embedding.get::<Option<i64>, _>("cached_tokens"),
        ),
        (Some("extraction".into()), Some(5), None, None),
        "background work has a group and no run"
    );
    assert_eq!(rows[7].get::<Option<i64>, _>("credits"), Some(2));
    db.drop_db().await;
}

#[tokio::test]
async fn the_summary_keeps_work_and_kind_apart_and_reports_what_was_not_reported() {
    let db = db!();
    let clock = ManualClock::new(T0);
    let recorder = PgUsageSink::start(db.pool().clone(), clock.clone());
    let sink = recorder.sink();
    let record = |c: ProviderCall| Meter::record(&*sink, c);
    // Replies: two model turns, one retried once; a web search; a page read with no credits.
    record(call(
        reply(1),
        CallKind::Model,
        2,
        Some(usage(1000, Some(800), 40)),
        None,
        None,
    ));
    record(call(
        reply(2),
        CallKind::Model,
        1,
        Some(usage(1200, Some(1100), 20)),
        None,
        None,
    ));
    record(call(reply(1), CallKind::Search, 1, None, Some(1), None));
    record(call(
        reply(2),
        CallKind::Extract,
        2,
        None,
        None,
        Some("timeout"),
    ));
    // Memory and pictures, whose provider does not report its cache.
    record(call(
        background(Purpose::Extraction),
        CallKind::Model,
        1,
        Some(usage(5000, None, 600)),
        None,
        None,
    ));
    record(call(
        background(Purpose::Extraction),
        CallKind::Embedding,
        3,
        Some(usage(90, None, 0)),
        None,
        None,
    ));
    record(call(
        background(Purpose::Picture),
        CallKind::Model,
        1,
        Some(usage(700, None, 30)),
        None,
        None,
    ));
    // A wiring mistake is still counted, as unattributed.
    record(call(None, CallKind::Model, 1, None, None, Some("auth")));
    recorder.shutdown().await;

    let admin = PgAdmin::new(db.pool().clone(), clock);
    let all = admin
        .usage(None, UnixMillis::new(T0), UnixMillis::new(T0 + 1))
        .await
        .unwrap();
    let order: Vec<_> = all.calls.iter().map(|l| (l.purpose, l.kind)).collect();
    assert_eq!(
        order,
        [
            (Some(Purpose::Reply), CallKind::Model),
            (Some(Purpose::Reply), CallKind::Search),
            (Some(Purpose::Reply), CallKind::Extract),
            (Some(Purpose::Extraction), CallKind::Model),
            (Some(Purpose::Extraction), CallKind::Embedding),
            (Some(Purpose::Picture), CallKind::Model),
            (None, CallKind::Model),
        ]
    );
    let replies = all.line(Purpose::Reply, CallKind::Model).unwrap();
    assert_eq!(
        (
            replies.calls,
            replies.requests,
            replies.input_tokens,
            replies.cached_tokens,
            replies.output_tokens
        ),
        (2, 3, 2200, Some(1900), 60),
        "calls once each, requests with their retries"
    );
    let search = all.line(Purpose::Reply, CallKind::Search).unwrap();
    assert_eq!((search.credits, search.input_tokens), (Some(1), 0));
    let page = all.line(Purpose::Reply, CallKind::Extract).unwrap();
    assert_eq!((page.failed, page.credits), (1, None));
    let memory = all.line(Purpose::Extraction, CallKind::Model).unwrap();
    assert_eq!(memory.cached_tokens, None, "not reported, not zero");
    assert_eq!(
        all.line(Purpose::Extraction, CallKind::Embedding)
            .unwrap()
            .input_tokens,
        90
    );

    // One group's view and an empty window.
    let mine = admin
        .usage(Some(group()), UnixMillis::new(T0), UnixMillis::new(T0 + 1))
        .await
        .unwrap();
    assert_eq!(
        mine.calls.len(),
        6,
        "the unattributed call belongs to no group"
    );
    let later = admin
        .usage(None, UnixMillis::new(T0 + 1), UnixMillis::new(T0 + 2))
        .await
        .unwrap();
    assert!(later.calls.is_empty());

    let daily = sqlx::query(
        "SELECT sum(calls)::bigint AS calls, sum(requests)::bigint AS requests FROM usage_provider_daily",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        (
            daily.get::<i64, _>("calls"),
            daily.get::<i64, _>("requests")
        ),
        (8, 12)
    );
    db.drop_db().await;
}

#[tokio::test]
async fn recording_never_blocks_and_every_batch_is_flushed() {
    let db = db!();
    let recorder = PgUsageSink::start(db.pool().clone(), ManualClock::new(T0));
    let sink = recorder.sink();
    for i in 0..1000 {
        Meter::record(
            &*sink,
            call(
                reply(i),
                CallKind::Model,
                1,
                Some(usage(10, Some(5), 1)),
                None,
                None,
            ),
        );
    }
    recorder.shutdown().await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_event")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 1000, "more than one batch, all flushed");
    db.drop_db().await;
}
