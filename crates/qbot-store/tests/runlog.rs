#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use qbot_agent::{Chain, RunLog, RunSummary, Trigger};
use qbot_context::{
    AssistantPart, AssistantTurn, ChatBatch, ChatLine, Instruction, InstructionRole, Item, Meta,
    Outcome, Part, RunEnd, Speaker, ToolCall, ToolResult, Transcript,
};
use qbot_core::{AccountId, CallId, ChainId, GroupId, ItemSeq, MessageId, TimerId, UnixMillis};
use qbot_llm::Usage;
use qbot_store::{PgRunLog, StoreError};
use serde_json::json;

fn group() -> GroupId {
    GroupId::new(42).unwrap()
}

fn addressed() -> Trigger {
    Trigger::Addressed {
        message: MessageId::new(7).unwrap(),
        sender: AccountId::new(9).unwrap(),
    }
}

fn items_with_pending_call() -> Vec<Item> {
    let call = |id: &str| {
        Item::Assistant(AssistantTurn::new(vec![
            AssistantPart::Text("let me look".into()),
            AssistantPart::Call(ToolCall {
                id: CallId::new(id).unwrap(),
                name: "search_history".into(),
                arguments: json!({ "query": "x" }),
            }),
        ]))
    };
    vec![
        Item::Instruction(Instruction {
            role: InstructionRole::System,
            text: "rules".into(),
            template_hash: "h".into(),
        }),
        Item::Chat(ChatBatch::new(vec![ChatLine {
            message: MessageId::new(1).unwrap(),
            speaker: Speaker::Bot,
            at: UnixMillis::new(T0),
            text: "earlier".into(),
        }])),
        call("c1"),
    ]
}

async fn write(log: &PgRunLog, run: qbot_core::RunId, items: &[Item]) {
    for (i, item) in items.iter().enumerate() {
        log.append(group(), run, ItemSeq::new(i as u32), item)
            .await
            .unwrap();
    }
}

fn summary(end: RunEnd) -> RunSummary {
    RunSummary {
        end,
        error: None,
        usage: Usage {
            input_tokens: 120,
            output_tokens: 30,
            cache: qbot_llm::CacheUsage::Reported { hit_tokens: 100 },
            reasoning_tokens: Some(5),
            reported: true,
        },
        turns: 2,
        tool_calls: 1,
        sends: 1,
    }
}

#[tokio::test]
async fn a_run_round_trips_exactly_and_ids_survive_across_runs() {
    let db = db!();
    let clock = ManualClock::new(T0);
    let log = PgRunLog::new(db.pool().clone(), clock.clone());

    let mut transcript = Transcript::new();
    for item in items_with_pending_call() {
        transcript.append(item).unwrap();
    }
    transcript
        .append(Item::ToolResult(ToolResult {
            call_id: CallId::new("c1").unwrap(),
            outcome: Outcome::Ok,
            content: vec![
                Part::Text("found it".into()),
                Part::Image { key: "pic".into() },
            ],
        }))
        .unwrap();
    transcript
        .append(Item::Meta(Meta::RunEnded(RunEnd::Delivered)))
        .unwrap();

    let run = log.begin(group(), &addressed()).await.unwrap();
    let second = log.begin(group(), &addressed()).await.unwrap();
    assert!(second.get() > run.get(), "ids increase and never repeat");
    write(&log, run, transcript.items()).await;
    clock.advance(std::time::Duration::from_secs(3));
    log.finish(group(), run, &summary(RunEnd::Delivered))
        .await
        .unwrap();

    assert_eq!(
        log.load_transcript(run).await.unwrap(),
        transcript,
        "the stored record rebuilds the exact transcript"
    );
    let record = log.record(run).await.unwrap().unwrap();
    assert_eq!(record.group, group());
    assert_eq!(
        (record.trigger_kind, record.end),
        (qbot_store::TriggerKind::Addressed, Some(RunEnd::Delivered))
    );
    assert_eq!(record.ended.unwrap().get() - record.started.get(), 3000);
    assert_eq!(
        (
            record.input_tokens,
            record.cached_tokens,
            record.output_tokens
        ),
        (Some(120), Some(100), Some(30))
    );
    assert_eq!(
        (record.turns, record.tool_calls, record.sends),
        (Some(2), Some(1), Some(1))
    );
    assert_eq!(
        log.record(second).await.unwrap().unwrap().end,
        None,
        "a run that has not finished has no end"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn wake_triggers_are_recorded_with_their_chain() {
    let db = db!();
    let log = PgRunLog::new(db.pool().clone(), ManualClock::new(T0));
    let trigger = Trigger::Wake {
        timer: TimerId::new(3),
        intent: "check the oven".into(),
        chain: Chain {
            id: ChainId::new(1),
            depth: 2,
        },
    };
    let run = log.begin(group(), &trigger).await.unwrap();
    let stored: serde_json::Value = sqlx::query_scalar("SELECT trigger FROM run WHERE run_id = $1")
        .bind(run.get() as i64)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        stored,
        json!({ "timer": 3, "intent": "check the oven", "chain": 1, "depth": 2 })
    );
    db.drop_db().await;
}

#[tokio::test]
async fn items_are_append_only_dense_and_a_run_finishes_once() {
    let db = db!();
    let log = PgRunLog::new(db.pool().clone(), ManualClock::new(T0));
    let run = log.begin(group(), &addressed()).await.unwrap();
    let items = items_with_pending_call();
    log.append(group(), run, ItemSeq::new(0), &items[0])
        .await
        .unwrap();
    assert!(
        log.append(group(), run, ItemSeq::new(0), &items[1])
            .await
            .is_err(),
        "a stored item cannot be replaced"
    );

    // A gap is detected when the record is read back.
    log.append(group(), run, ItemSeq::new(2), &items[1])
        .await
        .unwrap();
    assert!(matches!(
        log.load_items(run).await,
        Err(StoreError::Corrupt(_))
    ));

    assert!(
        log.finish(group(), run, &summary(RunEnd::Completed))
            .await
            .is_ok()
    );
    assert!(
        log.finish(group(), run, &summary(RunEnd::Completed))
            .await
            .is_err(),
        "finishing is final"
    );
    assert!(
        log.finish(
            group(),
            qbot_core::RunId::new(9_999),
            &summary(RunEnd::Completed)
        )
        .await
        .is_err()
    );
    let other = log.begin(group(), &addressed()).await.unwrap();
    assert!(
        log.finish(
            GroupId::new(43).unwrap(),
            other,
            &summary(RunEnd::Completed)
        )
        .await
        .is_err(),
        "scoped to the run's group"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn recovery_closes_open_runs_by_interrupting_unresolved_calls() {
    let db = db!();
    let clock = ManualClock::new(T0);
    let log = PgRunLog::new(db.pool().clone(), clock.clone());

    let crashed = log.begin(group(), &addressed()).await.unwrap();
    let partial = items_with_pending_call();
    write(&log, crashed, &partial).await;

    let finished = log.begin(group(), &addressed()).await.unwrap();
    log.finish(group(), finished, &summary(RunEnd::Completed))
        .await
        .unwrap();

    // Crash after the loop wrote its final item but before the run row was closed.
    let late = log.begin(group(), &addressed()).await.unwrap();
    let mut done = Transcript::new();
    done.append(partial[0].clone()).unwrap();
    done.append(Item::Meta(Meta::RunEnded(RunEnd::Delivered)))
        .unwrap();
    write(&log, late, done.items()).await;

    clock.advance(std::time::Duration::from_secs(60));
    assert_eq!(log.recover_open_runs().await.unwrap(), 2);

    let transcript = log.load_transcript(crashed).await.unwrap();
    assert!(
        transcript.is_quiescent(),
        "no request is left without a result"
    );
    let results: Vec<_> = transcript
        .items()
        .iter()
        .filter_map(|i| {
            if let Item::ToolResult(r) = i {
                Some(r.outcome)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(results, [Outcome::Interrupted]);
    assert!(matches!(
        transcript.items()[transcript.len() - 2],
        Item::Meta(Meta::CallsInterrupted {
            interrupted_calls: 1
        })
    ));
    assert!(matches!(
        transcript.items().last(),
        Some(Item::Meta(Meta::RunEnded(RunEnd::Interrupted)))
    ));
    assert_eq!(
        log.record(crashed).await.unwrap().unwrap().end,
        Some(RunEnd::Interrupted)
    );

    assert_eq!(
        log.record(late).await.unwrap().unwrap().end,
        Some(RunEnd::Delivered),
        "its recorded end is kept"
    );
    assert_eq!(
        log.load_items(late).await.unwrap().len(),
        2,
        "nothing was added to it"
    );
    assert_eq!(
        log.record(finished).await.unwrap().unwrap().end,
        Some(RunEnd::Completed),
        "finished runs are untouched"
    );
    assert_eq!(
        log.recover_open_runs().await.unwrap(),
        0,
        "recovery is idempotent"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn a_log_that_breaks_its_own_invariants_is_closed_rather_than_blocking_startup() {
    let db = db!();
    let log = PgRunLog::new(db.pool().clone(), ManualClock::new(T0));
    let run = log.begin(group(), &addressed()).await.unwrap();
    // A result with no call can never be loaded as a transcript.
    let orphan = Item::ToolResult(ToolResult {
        call_id: CallId::new("ghost").unwrap(),
        outcome: Outcome::Ok,
        content: vec![],
    });
    log.append(group(), run, ItemSeq::new(0), &orphan)
        .await
        .unwrap();
    assert_eq!(log.recover_open_runs().await.unwrap(), 1);
    assert_eq!(
        log.record(run).await.unwrap().unwrap().end,
        Some(RunEnd::Interrupted)
    );
    assert_eq!(
        log.load_items(run).await.unwrap().len(),
        1,
        "the invalid record is left as it was"
    );
    db.drop_db().await;
}
