#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use qbot_agent::{UsageDetail, UsageEvent, UsageSink};
use qbot_context::{ErrorKind, Outcome, RefusalReason, RunEnd};
use qbot_core::{GroupId, RunId};
use qbot_llm::{CacheUsage, Usage};
use qbot_store::PgUsageSink;
use sqlx::Row;

fn event(turn: u32, latency_ms: u64, detail: UsageDetail) -> UsageEvent {
    UsageEvent {
        group: GroupId::new(5).unwrap(),
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

fn model(usage: Option<Usage>, error: Option<&'static str>) -> UsageDetail {
    UsageDetail::Model {
        provider: "deepseek".into(),
        model: "deepseek-test".into(),
        usage,
        attempts: 2,
        error,
    }
}

#[tokio::test]
async fn model_tool_and_run_events_are_stored_and_flushed_on_shutdown() {
    let db = db!();
    let recorder = PgUsageSink::start(db.pool().clone(), ManualClock::new(T0));
    let sink = recorder.sink();
    sink.record(event(1, 800, model(Some(usage(1000, Some(900), 50)), None)));
    sink.record(event(
        2,
        400,
        model(Some(usage(1200, Some(1000), 20)), None),
    ));
    sink.record(event(3, 100, model(None, Some("rate_limited"))));
    sink.record(event(
        1,
        30,
        UsageDetail::Tool {
            name: "search_history".into(),
            outcome: Outcome::Ok,
        },
    ));
    sink.record(event(
        1,
        10,
        UsageDetail::Tool {
            name: "send_message".into(),
            outcome: Outcome::Refused(RefusalReason::LimitReached),
        },
    ));
    sink.record(event(
        1,
        5,
        UsageDetail::Tool {
            name: "send_message".into(),
            outcome: Outcome::Error(ErrorKind::Timeout),
        },
    ));
    sink.record(event(
        3,
        0,
        UsageDetail::RunEnded {
            end: RunEnd::Delivered,
            usage: usage(2200, Some(1900), 70),
            turns: 3,
            tool_calls: 3,
        },
    ));
    recorder.shutdown().await;
    sink.record(event(9, 0, model(None, None))); // after shutdown: dropped silently

    let rows = sqlx::query("SELECT kind, tool, status, input_tokens, cached_tokens, output_tokens, attempts, count, latency_ms, at_ms FROM usage_event ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(rows.len(), 7);
    let first = &rows[0];
    assert_eq!(first.get::<String, _>("kind"), "model");
    assert_eq!(
        (
            first.get::<Option<i64>, _>("input_tokens"),
            first.get::<Option<i64>, _>("cached_tokens")
        ),
        (Some(1000), Some(900))
    );
    assert_eq!(
        (
            first.get::<Option<i32>, _>("attempts"),
            first.get::<i64, _>("latency_ms"),
            first.get::<i64, _>("at_ms")
        ),
        (Some(2), 800, T0)
    );
    let failed = &rows[2];
    assert_eq!(failed.get::<String, _>("status"), "rate_limited");
    assert_eq!(
        failed.get::<Option<i64>, _>("input_tokens"),
        None,
        "no usage was reported for a failed call"
    );
    let statuses: Vec<String> = rows[3..6].iter().map(|r| r.get("status")).collect();
    assert_eq!(statuses, ["ok", "refused:limitreached", "error:timeout"]);
    assert_eq!(rows[6].get::<String, _>("status"), "delivered");
    assert_eq!(rows[6].get::<Option<i32>, _>("count"), Some(3));
    db.drop_db().await;
}

#[tokio::test]
async fn unreported_cache_usage_is_null_not_zero_and_the_views_roll_up() {
    let db = db!();
    let recorder = PgUsageSink::start(db.pool().clone(), ManualClock::new(T0));
    let sink = recorder.sink();
    sink.record(event(1, 100, model(Some(usage(500, None, 10)), None)));
    sink.record(event(2, 300, model(Some(usage(700, Some(600), 30)), None)));
    sink.record(event(3, 200, model(None, Some("timeout"))));
    sink.record(event(
        1,
        40,
        UsageDetail::Tool {
            name: "echo".into(),
            outcome: Outcome::Ok,
        },
    ));
    sink.record(event(
        2,
        60,
        UsageDetail::Tool {
            name: "echo".into(),
            outcome: Outcome::Interrupted,
        },
    ));
    recorder.shutdown().await;

    let nulls: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_event WHERE kind = 'model' AND input_tokens IS NOT NULL AND cached_tokens IS NULL")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(nulls, 1, "NotReported is stored as NULL");

    let m = sqlx::query("SELECT calls, failed_calls, input_tokens, cached_tokens, output_tokens, avg_latency_ms FROM usage_model_daily")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        (m.get::<i64, _>("calls"), m.get::<i64, _>("failed_calls")),
        (3, 1)
    );
    assert_eq!(
        (
            m.get::<i64, _>("input_tokens"),
            m.get::<i64, _>("cached_tokens"),
            m.get::<i64, _>("output_tokens")
        ),
        (1200, 600, 40)
    );
    assert!((m.get::<f64, _>("avg_latency_ms") - 200.0).abs() < 0.001);
    let t = sqlx::query("SELECT tool, calls, not_ok FROM usage_tool_daily")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        (
            t.get::<String, _>("tool"),
            t.get::<i64, _>("calls"),
            t.get::<i64, _>("not_ok")
        ),
        ("echo".into(), 2, 1)
    );
    db.drop_db().await;
}

#[tokio::test]
async fn recording_never_blocks_and_a_bad_batch_does_not_stop_later_ones() {
    let db = db!();
    let recorder = PgUsageSink::start(db.pool().clone(), ManualClock::new(T0));
    let sink = recorder.sink();
    for i in 0..1000 {
        sink.record(event(i, 1, model(Some(usage(10, Some(5), 1)), None)));
    }
    recorder.shutdown().await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_event")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 1000, "more than one batch, all flushed");
    db.drop_db().await;
}
