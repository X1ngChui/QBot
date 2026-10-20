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

fn account(id: i64) -> qbot_core::AccountId {
    qbot_core::AccountId::new(id).unwrap()
}

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

    // Their line is still context for someone else's run.
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
    let has_line = conv.items().iter().any(|i| match i {
        ConvItem::Message(m) => format!("{m:?}").contains("member:9: hey bot"),
        _ => false,
    });
    assert!(has_line);
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
            repeats: false,
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
    let second = sup.submit(request(2, 2)).await.unwrap();
    assert_eq!(sup.active(), 2);
    assert_eq!(
        sup.submit(request(3, 3)).await.unwrap_err(),
        Rejected::Overloaded
    );

    let a = first.finished().await.unwrap();
    let b = second.finished().await.unwrap();
    assert_eq!((a.end, b.end), (RunEnd::Completed, RunEnd::Completed));
    assert_eq!(sup.active(), 0, "capacity is released when runs finish");
    assert!(sup.submit(request(4, 4)).await.is_ok());
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
    let b = sup.submit(request(2, 2)).await.unwrap();
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
async fn a_group_has_one_spontaneous_run_at_a_time_and_other_runs_still_start() {
    let h = harness(vec![
        reply_ok(FakeReply::new().call("slow", json!({ "ms": 4000 }))),
        reply_ok(say("an answer")),
        reply_ok(FakeReply::new().text("quiet")),
        reply_ok(FakeReply::new().text("quiet again")),
    ]);
    let sup = supervisor(&h, cfg(8, 4, 60));
    h.world.say(1, 1, "chatting");
    h.world.say(2, 2, "a question");
    let first = sup.submit_spontaneous(group(), account(1)).await.unwrap();
    assert_eq!(
        sup.submit_spontaneous(group(), account(1))
            .await
            .unwrap_err(),
        Rejected::SpontaneousUnderWay
    );
    let addressed = sup.submit(request(2, 102)).await.unwrap();
    assert_eq!(
        addressed.finished().await.unwrap().end,
        RunEnd::Delivered,
        "an addressed run is not held back by the spontaneous one"
    );
    first.finished().await.unwrap();
    let next = sup.submit_spontaneous(group(), account(1)).await.unwrap();
    next.finished().await.unwrap();

    h.world.block(3);
    assert_eq!(
        sup.submit_spontaneous(group(), account(3))
            .await
            .unwrap_err(),
        Rejected::Blocked,
        "a blocked member's line starts no run"
    );
    assert!(
        sup.submit_spontaneous(group(), account(1)).await.is_ok(),
        "a refusal holds no claim"
    );
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
    let b = sup.submit(request(2, 2)).await.unwrap();
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
    let b = sup.submit(request(2, 2)).await.unwrap();
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
    let b = sup.submit(request(2, 2)).await.unwrap();
    let (a, b) = (a.finished().await.unwrap(), b.finished().await.unwrap());
    assert_eq!(h.world.summary(a.run).unwrap().end, RunEnd::Deadline);
    let queued = h.world.summary(b.run).unwrap();
    assert_eq!((queued.end, queued.turns), (RunEnd::Deadline, 0));
}

#[tokio::test]
async fn opening_a_runs_context_is_bounded_by_the_deadline_and_by_shutdown() {
    // The platform stalls while names are asked for: the run ends at its deadline.
    let h = harness(vec![reply_ok(say("hi"))]);
    h.world.delay_open(Duration::from_secs(3600));
    let sup = supervisor(&h, cfg(8, 4, 1));
    h.world.say(1, 1, "hey bot");
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        sup.submit(request(1, 101)).await.unwrap().finished(),
    )
    .await
    .expect("the deadline ends the run")
    .unwrap();
    assert_eq!(report.end, RunEnd::Deadline);

    // And a shutdown does not wait for it.
    let sup = supervisor(&h, cfg(8, 4, 3600));
    let handle = sup.submit(request(1, 101)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), sup.shutdown())
        .await
        .expect("shutdown does not wait out the open");
    assert_eq!(handle.finished().await.unwrap().end, RunEnd::Cancelled);
    assert!(h.fake.recorded().is_empty(), "no model call");
}

fn addressed_message(trigger: &Trigger) -> i64 {
    match trigger {
        Trigger::Addressed { message, .. } => message.get(),
        other => panic!("not addressed: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_members_second_message_waits_for_their_run_and_sees_its_reply() {
    let h = harness(vec![
        reply_ok(FakeReply::new().call("slow", json!({ "ms": 3000 }))),
        reply_ok(say("hello there")),
        reply_ok(say("pork belly")),
    ]);
    let sup = supervisor(&h, cfg(8, 4, 60));
    h.world.say(1, 1, "hi bot");
    h.world.say(1, 1, "which cut for braised pork?");
    let first = sup.submit(request(1, 101)).await.unwrap();
    let second = sup.submit(request(1, 102)).await.unwrap();
    assert_eq!(sup.active(), 2, "the waiting run holds its place");
    let (a, b) = (
        first.finished().await.unwrap(),
        second.finished().await.unwrap(),
    );
    assert_eq!((a.end, b.end), (RunEnd::Delivered, RunEnd::Delivered));
    let opened: Vec<i64> = h.world.opened().iter().map(addressed_message).collect();
    assert_eq!(opened, [101, 102], "the second opened only after the first");
    let calls = h.fake.recorded();
    assert_eq!(calls.len(), 3);
    let seen = format!("{:?}", calls[2].conversation);
    assert!(
        seen.contains("hello there"),
        "the second run sees the first one's reply: {seen}"
    );
    assert_eq!(sup.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_further_message_folds_into_the_waiting_run_which_answers_the_newest() {
    let h = harness(vec![
        reply_ok(FakeReply::new().call("slow", json!({ "ms": 3000 }))),
        reply_ok(say("first")),
        reply_ok(say("second")),
    ]);
    let sup = supervisor(&h, cfg(8, 4, 60));
    for text in ["hi", "a question", "never mind, another one"] {
        h.world.say(1, 1, text);
    }
    let first = sup.submit(request(1, 101)).await.unwrap();
    let second = sup.submit(request(1, 102)).await.unwrap();
    assert_eq!(
        sup.submit(request(1, 103)).await.unwrap_err(),
        Rejected::Folded
    );
    assert_eq!(sup.active(), 2, "a folded message takes no capacity");
    first.finished().await.unwrap();
    second.finished().await.unwrap();
    let opened: Vec<i64> = h.world.opened().iter().map(addressed_message).collect();
    assert_eq!(
        opened,
        [101, 103],
        "the waiting run took over the newest message"
    );
    assert_eq!(
        h.world.logged_runs().len(),
        2,
        "no run for the folded message"
    );
    assert_eq!(h.world.sent().len(), 2);

    // Once the waiting run has started, the member's next message waits behind it again.
    h.world.say(1, 1, "and later");
    let later = sup.submit(request(1, 104)).await.unwrap();
    assert!(later.finished().await.is_some());
}

#[tokio::test(start_paused = true)]
async fn other_members_and_scheduled_runs_do_not_wait_for_a_members_turn() {
    let h = harness(vec![
        reply_ok(FakeReply::new().call("slow", json!({ "ms": 5000 }))),
        reply_ok(say("for the other member")),
        reply_ok(FakeReply::new().text("task done")),
        reply_ok(say("for the first member")),
    ]);
    let sup = supervisor(&h, cfg(8, 4, 60));
    h.world.say(1, 1, "a slow question");
    h.world.say(2, 2, "a quick one");
    let slow = sup.submit(request(1, 101)).await.unwrap();
    let other = sup.submit(request(2, 102)).await.unwrap();
    let wake = sup
        .submit(TriggerRequest {
            group: group(),
            trigger: Trigger::Wake {
                timer: TimerId::new(1),
                intent: "remind".into(),
                chain: Chain {
                    id: ChainId::new(1),
                    depth: 0,
                },
                repeats: false,
            },
        })
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    assert_eq!(other.finished().await.unwrap().end, RunEnd::Delivered);
    assert!(wake.finished().await.is_some());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "neither waited for the slow run: {:?}",
        started.elapsed()
    );
    assert_eq!(slow.finished().await.unwrap().end, RunEnd::Delivered);
}

#[tokio::test(start_paused = true)]
async fn a_waiting_run_ends_at_its_deadline_or_shutdown_without_a_model_call() {
    let h = harness(vec![reply_ok(
        FakeReply::new().call("slow", json!({ "ms": 50_000 })),
    )]);
    let sup = supervisor(&h, cfg(8, 4, 5));
    h.world.say(1, 1, "hello");
    h.world.say(1, 1, "hello?");
    let first = sup.submit(request(1, 101)).await.unwrap();
    let second = sup.submit(request(1, 102)).await.unwrap();
    let (a, b) = (
        first.finished().await.unwrap(),
        second.finished().await.unwrap(),
    );
    assert_eq!(a.end, RunEnd::Deadline);
    assert_eq!((b.end, b.turns), (RunEnd::Deadline, 0));
    assert_eq!(
        h.fake.recorded().len(),
        1,
        "the waiting run made no model call"
    );
    assert_eq!(h.world.summary(b.run).unwrap().end, RunEnd::Deadline);
    assert_eq!(sup.active(), 0);

    // A run waiting at shutdown is cancelled, and nothing of the member is left behind.
    let h = harness(vec![reply_ok(
        FakeReply::new().call("slow", json!({ "ms": 50_000 })),
    )]);
    let sup = supervisor(&h, cfg(8, 4, 600));
    h.world.say(1, 1, "hello");
    let first = sup.submit(request(1, 101)).await.unwrap();
    let second = sup.submit(request(1, 102)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    sup.shutdown().await;
    assert_eq!(first.finished().await.unwrap().end, RunEnd::Cancelled);
    assert_eq!(second.finished().await.unwrap().end, RunEnd::Cancelled);
    assert_eq!(sup.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_members_runs_take_turns_in_the_order_they_were_admitted() {
    let h = harness(vec![
        reply_ok(FakeReply::new().call("slow", json!({ "ms": 1000 }))),
        reply_ok(say("first")),
        reply_ok(say("second")),
    ]);
    let sup = supervisor(&h, cfg(8, 4, 60));
    for text in ["hi", "a question", "another"] {
        h.world.say(1, 1, text);
    }
    // The first run's start is slower to record, so the second one's task runs first.
    h.world.delay_next_begin(Duration::from_millis(50));
    let (first, second) = tokio::join!(sup.submit(request(1, 101)), sup.submit(request(1, 102)));
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(
        sup.submit(request(1, 103)).await.unwrap_err(),
        Rejected::Folded,
        "never a third run of the member"
    );
    assert_eq!(sup.active(), 2);
    first.finished().await.unwrap();
    second.finished().await.unwrap();
    let opened: Vec<i64> = h.world.opened().iter().map(addressed_message).collect();
    assert_eq!(
        opened,
        [101, 103],
        "admission order, the folded message answered last"
    );
}

#[tokio::test(start_paused = true)]
async fn a_folded_message_gets_a_whole_deadline() {
    let h = harness(vec![
        reply_ok(FakeReply::new().call("slow", json!({ "ms": 4500 }))),
        reply_ok(FakeReply::new().text("first done")),
        reply_ok(FakeReply::new().call("slow", json!({ "ms": 2000 }))),
        reply_ok(say("answered")),
    ]);
    let sup = supervisor(&h, cfg(8, 4, 5));
    for text in ["hi", "a question", "and another"] {
        h.world.say(1, 1, text);
    }
    let first = sup.submit(request(1, 101)).await.unwrap();
    let second = sup.submit(request(1, 102)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        sup.submit(request(1, 103)).await.unwrap_err(),
        Rejected::Folded
    );
    assert_eq!(first.finished().await.unwrap().end, RunEnd::Completed);
    // Its own deadline would have ended it at 5 s, during its 2 s of work from 4.5 s.
    assert_eq!(second.finished().await.unwrap().end, RunEnd::Delivered);
}

#[tokio::test(start_paused = true)]
async fn a_block_or_mute_set_while_a_run_waits_its_turn_holds() {
    let h = harness(vec![
        reply_ok(FakeReply::new().call("slow", json!({ "ms": 3000 }))),
        reply_ok(FakeReply::new().text("done")),
    ]);
    let sup = supervisor(&h, cfg(8, 4, 60));
    h.world.say(1, 1, "hello");
    h.world.say(1, 1, "hello?");
    let first = sup.submit(request(1, 101)).await.unwrap();
    let second = sup.submit(request(1, 102)).await.unwrap();
    h.world.block(1);
    first.finished().await.unwrap();
    let waited = second.finished().await.unwrap();
    assert_eq!((waited.end, waited.turns), (RunEnd::Cancelled, 0));
    assert_eq!(h.fake.recorded().len(), 2, "no model call after the block");
}
