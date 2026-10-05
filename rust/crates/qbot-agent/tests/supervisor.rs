#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use qbot_agent::{Chain, Rejected, Trigger, TriggerRequest};
use qbot_context::{Item, Outcome, RunEnd};
use qbot_core::{ChainId, TimerId};
use qbot_llm::ConvItem;
use qbot_llm::fake::FakeReply;
use serde_json::json;

fn request(sender: i64, message: i64) -> TriggerRequest {
    TriggerRequest {
        group: group(),
        trigger: addressed(sender, message),
    }
}

#[tokio::test]
async fn a_blocked_members_message_never_creates_a_run() {
    let h = harness(vec![reply_ok(say("hi"))]);
    let sup = supervisor(&h, cfg(8, 4, 60));
    h.world.block(9);
    h.world.say(9, 9, "hey bot");

    assert_eq!(
        sup.submit(request(9, 101)).await.unwrap_err(),
        Rejected::Blocked
    );
    assert!(h.fake.recorded().is_empty(), "no model call");
    assert!(h.world.logged_runs().is_empty(), "no run, no transcript");
    assert_eq!(sup.active(), 0, "no capacity consumed");
    assert_eq!(h.fake.remaining_steps(), 1);

    // Their line is still context for someone else's run, marked as blocked.
    h.world.say(1, 1, "what did they say?");
    let report = sup
        .submit(request(1, 102))
        .await
        .unwrap()
        .finished()
        .await
        .unwrap();
    assert_eq!(report.end, RunEnd::Delivered);
    let conv = &h.fake.recorded()[0].conversation;
    let has_marked_line = conv.items().iter().any(|i| match i {
        ConvItem::Message(m) => {
            format!("{m:?}").contains("member:9 (blocked: do not reply): hey bot")
        }
        _ => false,
    });
    assert!(has_marked_line);
}

#[tokio::test]
async fn mute_is_checked_first_and_wakeups_have_no_block_gate() {
    let h = harness(vec![reply_ok(FakeReply::new().text("quiet"))]);
    let sup = supervisor(&h, cfg(8, 4, 60));
    h.world.block(9);
    h.world.set_muted(true);
    assert_eq!(
        sup.submit(request(9, 1)).await.unwrap_err(),
        Rejected::Muted
    );
    let wake = TriggerRequest {
        group: group(),
        trigger: Trigger::Wake {
            timer: TimerId::new(1),
            intent: "x".into(),
            chain: Chain {
                id: ChainId::new(1),
                depth: 0,
            },
        },
    };
    assert_eq!(sup.submit(wake.clone()).await.unwrap_err(), Rejected::Muted);

    h.world.set_muted(false);
    h.world.say(1, 1, "hello");
    let report = sup.submit(wake).await.unwrap().finished().await.unwrap();
    assert_eq!(report.end, RunEnd::Completed);
}

#[tokio::test(start_paused = true)]
async fn capacity_bounds_active_plus_waiting_runs() {
    let hold = |ms: u64| reply_ok(FakeReply::new().call("slow", json!({ "ms": ms })));
    let h = harness(vec![
        hold(1000),
        hold(1000),
        reply_ok(FakeReply::new().text("a")),
        reply_ok(FakeReply::new().text("b")),
    ]);
    let sup = supervisor(&h, cfg(2, 1, 60));
    h.world.say(1, 1, "hello");
    let first = sup.submit(request(1, 1)).await.unwrap();
    let second = sup.submit(request(1, 2)).await.unwrap();
    assert_eq!(sup.active(), 2);
    assert_eq!(
        sup.submit(request(1, 3)).await.unwrap_err(),
        Rejected::Overloaded
    );

    let a = first.finished().await.unwrap();
    let b = second.finished().await.unwrap();
    assert_eq!((a.end, b.end), (RunEnd::Completed, RunEnd::Completed));
    assert_eq!(sup.active(), 0, "capacity is released when runs finish");
    assert!(sup.submit(request(1, 4)).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn concurrency_bounds_runs_calling_the_model_at_once() {
    let slow = |ms: u64| reply_ok(FakeReply::new().call("slow", json!({ "ms": ms })));
    let done = || reply_ok(FakeReply::new().text("ok"));
    let h = harness(vec![slow(1000), done(), slow(1000), done()]);
    let sup = supervisor(&h, cfg(8, 1, 600));
    h.world.say(1, 1, "hello");
    let started = tokio::time::Instant::now();
    let a = sup.submit(request(1, 1)).await.unwrap();
    let b = sup.submit(request(1, 2)).await.unwrap();
    a.finished().await.unwrap();
    b.finished().await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "one at a time: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn overlapping_runs_in_one_group_are_independent() {
    let h = harness(vec![
        reply_ok(say("answer one")),
        reply_ok(say("answer two")),
    ]);
    let sup = supervisor(&h, cfg(8, 4, 60));
    h.world.say(1, 1, "first question");
    h.world.say(2, 2, "second question");
    let a = sup.submit(request(1, 101)).await.unwrap();
    let b = sup.submit(request(2, 102)).await.unwrap();
    let (a, b) = (a.finished().await.unwrap(), b.finished().await.unwrap());

    assert_ne!(a.run, b.run);
    assert_eq!((a.end, b.end), (RunEnd::Delivered, RunEnd::Delivered));
    assert_eq!(h.world.sent().len(), 2, "each trigger got its own reply");
    assert_eq!(h.world.logged_runs().len(), 2);
    let results = |r: &qbot_agent::RunReport| {
        r.transcript
            .items()
            .iter()
            .filter(|i| matches!(i, Item::ToolResult(_)))
            .count()
    };
    assert_eq!((results(&a), results(&b)), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn queue_wait_counts_against_the_deadline() {
    let h = harness(vec![
        reply_ok(FakeReply::new().call("slow", json!({ "ms": 4000 }))),
        reply_ok(FakeReply::new().text("first done")),
        reply_ok(FakeReply::new().call("slow", json!({ "ms": 2000 }))),
    ]);
    let sup = supervisor(&h, cfg(8, 1, 5));
    h.world.say(1, 1, "hello");
    let a = sup.submit(request(1, 1)).await.unwrap();
    let b = sup.submit(request(1, 2)).await.unwrap();
    let a = a.finished().await.unwrap();
    let b = b.finished().await.unwrap();
    assert_eq!(a.end, RunEnd::Completed);
    // It waited 4s for a permit, leaving 1s of its 5s, and then needed 2s.
    assert_eq!(b.end, RunEnd::Deadline);
    assert_eq!(b.turns, 1);
    assert!(b.transcript.is_quiescent());
}

#[tokio::test(start_paused = true)]
async fn a_run_still_queued_at_its_deadline_never_starts() {
    let h = harness(vec![reply_ok(
        FakeReply::new().call("slow", json!({ "ms": 50_000 })),
    )]);
    let sup = supervisor(&h, cfg(8, 1, 5));
    h.world.say(1, 1, "hello");
    let a = sup.submit(request(1, 1)).await.unwrap();
    let b = sup.submit(request(1, 2)).await.unwrap();
    let (a, b) = (a.finished().await.unwrap(), b.finished().await.unwrap());
    assert_eq!(a.end, RunEnd::Deadline);
    assert_eq!((b.end, b.turns), (RunEnd::Deadline, 0));
    assert_eq!(
        h.fake.recorded().len(),
        1,
        "the queued run made no model call"
    );
}

#[tokio::test]
async fn reservations_hold_capacity_and_release_it_on_drop() {
    let h = harness(vec![reply_ok(FakeReply::new().text("ok"))]);
    let sup = supervisor(&h, cfg(1, 1, 60));
    let reservation = sup.reserve().unwrap();
    assert!(sup.reserve().is_none());
    assert_eq!(
        sup.submit(request(1, 1)).await.unwrap_err(),
        Rejected::Overloaded
    );
    drop(reservation);
    assert_eq!(sup.active(), 0);

    h.world.say(1, 1, "hello");
    let reservation = sup.reserve().unwrap();
    let handle = sup
        .submit_reserved(reservation, request(1, 2))
        .await
        .unwrap();
    assert_eq!(handle.finished().await.unwrap().end, RunEnd::Completed);
    assert_eq!(sup.active(), 0);

    // A rejected reserved submit releases its reservation too.
    h.world.set_muted(true);
    let reservation = sup.reserve().unwrap();
    assert_eq!(
        sup.submit_reserved(reservation, request(1, 3))
            .await
            .unwrap_err(),
        Rejected::Muted
    );
    assert_eq!(sup.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn shutdown_cancels_active_runs_and_interrupts_their_calls() {
    let h = harness(vec![reply_ok(
        FakeReply::new().call("slow", json!({ "ms": 100_000 })),
    )]);
    let sup = supervisor(&h, cfg(8, 4, 600));
    h.world.say(1, 1, "hello");
    let handle = sup.submit(request(1, 1)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    sup.shutdown().await;
    let report = handle.finished().await.unwrap();
    assert_eq!(report.end, RunEnd::Cancelled);
    let interrupted = report
        .transcript
        .items()
        .iter()
        .any(|i| matches!(i, Item::ToolResult(r) if r.outcome == Outcome::Interrupted));
    assert!(interrupted);
    assert_eq!(sup.active(), 0);
    assert_eq!(
        sup.submit(request(1, 2)).await.unwrap_err(),
        Rejected::ShuttingDown
    );
}

#[tokio::test]
async fn a_run_that_cannot_be_recorded_is_rejected_and_holds_no_capacity() {
    let h = harness(vec![reply_ok(say("hi"))]);
    let sup = supervisor(&h, cfg(8, 4, 60));
    h.world.say(1, 1, "hello");
    h.world.fail_run_begin(true);
    assert!(matches!(
        sup.submit(request(1, 101)).await.unwrap_err(),
        Rejected::Environment(_)
    ));
    assert_eq!(sup.active(), 0);
    assert!(
        h.fake.recorded().is_empty(),
        "no model call without a durable record"
    );
}

#[tokio::test(start_paused = true)]
async fn every_run_gets_a_finished_record_even_one_that_never_started() {
    let h = harness(vec![reply_ok(
        FakeReply::new().call("slow", json!({ "ms": 50_000 })),
    )]);
    let sup = supervisor(&h, cfg(8, 1, 5));
    h.world.say(1, 1, "hello");
    let a = sup.submit(request(1, 1)).await.unwrap();
    let b = sup.submit(request(1, 2)).await.unwrap();
    let (a, b) = (a.finished().await.unwrap(), b.finished().await.unwrap());
    assert_eq!(h.world.summary(a.run).unwrap().end, RunEnd::Deadline);
    let queued = h.world.summary(b.run).unwrap();
    assert_eq!((queued.end, queued.turns), (RunEnd::Deadline, 0));
}
