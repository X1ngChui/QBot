#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use qbot_agent::{RunLog, RunSummary, Trigger};
use qbot_context::RunEnd;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_llm::Usage;
use qbot_store::{NewLine, NewSpeaker, PgAdmin, PgArchive, PgGroupPolicy, PgRunLog};

fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn account(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

async fn say(archive: &PgArchive, g: GroupId, message: i64, who: i64) {
    archive
        .append(NewLine {
            group: g,
            message: MessageId::new(message).unwrap(),
            speaker: NewSpeaker::Member(account(who)),
            at: UnixMillis::new(T0),
            text: format!("m{message}"),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn blocks_roster_and_mute_state_are_readable() {
    let db = db!();
    let clock = ManualClock::new(T0);
    let admin = PgAdmin::new(db.pool().clone(), clock.clone());
    let policy = PgGroupPolicy::new(db.pool().clone(), clock.clone());
    let archive = PgArchive::new(db.pool().clone());
    let g = group(10);

    assert!(!admin.is_muted(g).await.unwrap());
    policy.set_muted(g, true).await.unwrap();
    assert!(admin.is_muted(g).await.unwrap());

    for (m, who) in [(1, 501), (2, 502), (3, 502), (4, 502), (5, 503)] {
        say(&archive, g, m, who).await;
    }
    let (total, rows) = admin.roster(g, 2).await.unwrap();
    assert_eq!(total, 3);
    assert_eq!(
        rows.iter()
            .map(|r| (r.account.get(), r.messages))
            .collect::<Vec<_>>(),
        [(502, 3), (501, 1)]
    );
    assert_eq!(
        admin
            .message_count(g, &[account(501), account(503)])
            .await
            .unwrap(),
        2
    );
    assert_eq!(admin.member_number(g, account(502)).await.unwrap(), Some(2));
    assert_eq!(admin.member_number(g, account(999)).await.unwrap(), None);

    policy.block(g, account(501), None).await.unwrap();
    policy
        .block(g, account(502), Some(UnixMillis::new(T0 + 60_000)))
        .await
        .unwrap();
    assert_eq!(admin.blocks(g).await.unwrap().len(), 2);
    clock.advance(Duration::from_secs(120));
    let remaining = admin.blocks(g).await.unwrap();
    assert_eq!(remaining.len(), 1, "an expired block is no longer listed");
    assert_eq!(remaining[0].account, account(501));
    db.drop_db().await;
}

#[tokio::test]
async fn usage_and_top_requesters_aggregate_runs() {
    let db = db!();
    let clock = ManualClock::new(T0);
    let admin = PgAdmin::new(db.pool().clone(), clock.clone());
    let log = PgRunLog::new(db.pool().clone(), clock.clone());
    let g = group(11);
    let summary = |tokens: u32| RunSummary {
        end: RunEnd::Delivered,
        error: None,
        usage: Usage {
            input_tokens: tokens,
            output_tokens: tokens / 10,
            ..Usage::ZERO
        },
        turns: 1,
        tool_calls: 1,
        sends: 1,
    };
    for (sender, tokens) in [(501, 1000), (501, 2000), (502, 500)] {
        let trigger = Trigger::Addressed {
            message: MessageId::new(1).unwrap(),
            sender: account(sender),
        };
        let run = log.begin(g, &trigger).await.unwrap();
        log.finish(g, run, &summary(tokens)).await.unwrap();
    }
    let top = admin
        .top_requesters(g, UnixMillis::new(0), 5)
        .await
        .unwrap();
    assert_eq!(
        top.iter()
            .map(|t| (t.account.get(), t.runs, t.input_tokens))
            .collect::<Vec<_>>(),
        [(501, 2, 3000), (502, 1, 500)]
    );
    assert!(
        admin
            .top_requesters(group(12), UnixMillis::new(0), 5)
            .await
            .unwrap()
            .is_empty()
    );
    // Usage events come from the usage sink; with none recorded the totals are zero, not an error.
    assert_eq!(
        admin
            .usage(Some(g), UnixMillis::new(0))
            .await
            .unwrap()
            .model_calls,
        0
    );
    db.drop_db().await;
}

#[tokio::test]
async fn the_daily_report_and_housekeeping_read_what_happened() {
    use qbot_agent::Trigger as T;
    use qbot_core::TimerId;
    let db = db!();
    let clock = ManualClock::new(T0);
    let admin = PgAdmin::new(db.pool().clone(), clock.clone());
    let archive = PgArchive::new(db.pool().clone());
    let log = PgRunLog::new(db.pool().clone(), clock.clone());
    let (g1, g2) = (group(21), group(22));

    assert_eq!(admin.groups().await.unwrap(), Vec::<GroupId>::new());
    say(&archive, g1, 1, 501).await;
    say(&archive, g2, 2, 502).await;
    assert_eq!(admin.groups().await.unwrap(), [g1, g2]);

    let finish = |end: RunEnd, error: Option<&'static str>| RunSummary {
        end,
        error,
        usage: Usage {
            input_tokens: 100,
            output_tokens: 10,
            ..Usage::ZERO
        },
        turns: 1,
        tool_calls: 0,
        sends: 0,
    };
    for (g, end, error) in [
        (g1, RunEnd::Delivered, None),
        (g1, RunEnd::ModelError, Some("timeout")),
        (g2, RunEnd::Delivered, None),
    ] {
        let trigger = T::Addressed {
            message: MessageId::new(1).unwrap(),
            sender: account(501),
        };
        let run = log.begin(g, &trigger).await.unwrap();
        log.finish(g, run, &finish(end, error)).await.unwrap();
    }
    sqlx::query("INSERT INTO usage_event (at_ms, group_id, run_id, turn, kind, status, input_tokens, cached_tokens, output_tokens, latency_ms) VALUES ($1, 21, 1, 0, 'model', 'ok', 100, 60, 10, 5), ($1, 21, 1, 0, 'model', 'timeout', NULL, NULL, NULL, 5)")
        .bind(T0)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO usage_event (at_ms, group_id, run_id, turn, kind, tool, status, latency_ms) VALUES ($1, 21, 1, 0, 'tool', 'send_message', 'ok', 1), ($1, 21, 1, 0, 'tool', 'search_history', 'error', 1)")
        .bind(T0)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO timer (kind, due_ms, state, created_ms, job_kind, outcome, outcome_detail) VALUES ('job', 0, 'done', $1, 'backup', 'job_failed', 'disk full'), ('job', $1, 'pending', $1, 'nightly', NULL, NULL)")
        .bind(T0)
        .execute(db.pool())
        .await
        .unwrap();

    let report = admin
        .report(UnixMillis::new(T0 - 1000), UnixMillis::new(T0 + 1_000_000))
        .await
        .unwrap();
    assert_eq!(
        (
            report.runs,
            report.runs_addressed,
            report.runs_wake,
            report.groups_active
        ),
        (3, 3, 0, 2)
    );
    assert_eq!(
        report.run_ends,
        [("delivered".to_owned(), 2), ("model_error".to_owned(), 1)]
    );
    assert_eq!(
        (
            report.model_calls,
            report.failed_model_calls,
            report.input_tokens,
            report.cached_tokens
        ),
        (2, 1, 100, 60)
    );
    assert_eq!(
        (report.tool_calls, report.tool_calls_not_ok, report.messages),
        (2, 1, 2)
    );
    assert_eq!(
        (report.groups_new, report.jobs_pending, report.jobs_failed),
        (2, 1, 1)
    );
    // Outside the window nothing is counted as activity.
    let empty = admin
        .report(
            UnixMillis::new(T0 + 5_000_000),
            UnixMillis::new(T0 + 6_000_000),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            empty.runs,
            empty.model_calls,
            empty.messages,
            empty.groups_new
        ),
        (0, 0, 0, 0)
    );

    // Housekeeping removes only what is over and old.
    assert_eq!(
        admin
            .delete_finished_timers(UnixMillis::new(T0 - 1))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        admin
            .delete_finished_timers(UnixMillis::new(T0 + 1))
            .await
            .unwrap(),
        1,
        "the finished job; the pending one stays"
    );
    assert_eq!(admin.delete_runs(UnixMillis::new(T0 - 1)).await.unwrap(), 0);
    assert_eq!(
        admin
            .delete_runs(UnixMillis::new(T0 + 1_000_000))
            .await
            .unwrap(),
        3
    );
    let _ = TimerId::new(1);
    db.drop_db().await;
}
