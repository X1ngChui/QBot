#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use qbot_agent::{DeliveryError, OutSegment};
use qbot_context::{ErrorKind, Outcome, RefusalReason, RunEnd};
use qbot_core::AccountId;
use serde_json::json;

/// One model turn making `calls`, then done; returns the results the model saw.
async fn try_calls(r: &Rig, calls: Vec<serde_json::Value>) -> Vec<qbot_context::ToolResult> {
    let mut turn = qbot_llm::fake::FakeReply::new();
    for args in calls {
        turn = turn.call("send_message", args);
    }
    r.fake.push(reply(turn));
    r.fake.push(done());
    r.ask(1, 1, "hello bot").await;
    r.last_run_results()
}

fn account(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

#[tokio::test]
async fn the_chat_markers_become_message_segments() {
    let r = rig(vec![]);
    let hello = r.world.say(2, 2, "anyone around?");
    let text = format!("[reply:{}][at:2] yes! [face:14]", hello.message.get());
    let results = try_calls(&r, vec![send(&text)]).await;
    assert_eq!(results[0].outcome, Outcome::Ok, "{}", text_of(&results[0]));
    assert!(text_of(&results[0]).starts_with("sent [msg:"));
    assert_eq!(
        r.world.sent()[0],
        [
            OutSegment::Reply(hello.message),
            OutSegment::At(account(2)),
            OutSegment::Text(" yes! ".into()),
            OutSegment::Face(14)
        ]
    );
}

#[tokio::test]
async fn ordinary_brackets_stay_text() {
    let r = rig(vec![]);
    let results = try_calls(&r, vec![send("[\u{7b11}] see [image] and [1] [at: x")]).await;
    assert_eq!(results[0].outcome, Outcome::Ok, "{}", text_of(&results[0]));
    assert_eq!(
        r.world.sent()[0],
        [OutSegment::Text(
            "[\u{7b11}] see [image] and [1] [at: x".into()
        )]
    );
}

#[tokio::test]
async fn references_the_model_invents_are_refused_not_guessed() {
    let r = rig(vec![]);
    let results = try_calls(
        &r,
        vec![
            send("[at:77] hi"),
            send("[reply:424242] hi"),
            send("[at:0] hi"),
        ],
    )
    .await;
    for result in &results {
        assert_eq!(
            result.outcome,
            Outcome::Refused(RefusalReason::NotAllowed),
            "{}",
            text_of(result)
        );
    }
    assert!(r.world.sent().is_empty());
}

#[tokio::test]
async fn blocking_is_not_enforced_on_what_a_message_may_mention() {
    // Blocking is an admission rule. A mention or quote of a blocked member can be perfectly
    // innocent (talking about them to someone else), so the send tool does not police it.
    let r = rig(vec![]);
    r.world.block(9);
    let spam = r.world.say(9, 9, "buy my stuff");
    let results = try_calls(
        &r,
        vec![
            send("[at:9] please stop"),
            send(&format!("[reply:{}] not interested", spam.message.get())),
        ],
    )
    .await;
    assert!(results.iter().all(|r| r.outcome == Outcome::Ok));
    assert_eq!(r.world.sent().len(), 2);
}

#[tokio::test]
async fn malformed_markers_and_shapes_are_explained_as_invalid_arguments() {
    let r = rig(vec![]);
    let first = r.world.say(2, 2, "a");
    let cases = [
        ("   ".to_owned(), "empty"),
        ("[dice] and more".to_owned(), "whole message"),
        ("hi [contact:2]".to_owned(), "whole message"),
        (
            format!("hello [reply:{}]", first.message.get()),
            "must come first",
        ),
        ("[at:bob] hi".to_owned(), "member number"),
        ("[face:smile]".to_owned(), "face id"),
        ("[at:all] everyone".to_owned(), "not allowed"),
    ];
    let results = try_calls(&r, cases.iter().map(|(t, _)| send(t)).collect()).await;
    for (result, (text, why)) in results.iter().zip(&cases) {
        assert_eq!(
            result.outcome,
            Outcome::Error(ErrorKind::InvalidArguments),
            "{text}: {}",
            text_of(result)
        );
        assert!(text_of(result).contains(why), "{text}: {}", text_of(result));
    }
    assert!(r.world.sent().is_empty());
}

#[tokio::test]
async fn a_model_cannot_choose_a_game_result() {
    let r = rig(vec![]);
    let cases = [
        "[dice:6]",
        "[rps:1]",
        "[dice: 6 ]",
        "[dice:]",
        "[dice result:6]",
        "[rps result:rock]",
        "I rolled [dice:6]",
    ];
    let results = try_calls(&r, cases.iter().map(|t| send(t)).collect()).await;
    for (result, text) in results.iter().zip(cases) {
        assert_eq!(
            result.outcome,
            Outcome::Error(ErrorKind::InvalidArguments),
            "{text}: {}",
            text_of(result)
        );
        assert!(
            text_of(result).contains("cannot choose the result"),
            "{text}: {}",
            text_of(result)
        );
    }
    assert!(
        r.world.sent().is_empty(),
        "nothing that looks like a result reaches the group, not even as text"
    );
}

#[tokio::test]
async fn dice_results_come_back_through_the_echo_and_a_run_can_react() {
    let r = rig(vec![]);
    r.fake.push(reply(call(
        "send_message",
        json!({ "text": " [dice] ", "end_turn": false }),
    )));
    r.fake
        .push(reply(call("send_message", json!({ "text": "nice roll" }))));
    let report = r.ask(1, 1, "roll a dice").await;
    let results = r.last_run_results();
    // The request carries no result: the segment has nowhere to put one.
    assert_eq!(
        r.world.sent()[0],
        [OutSegment::Dice],
        "surrounding spaces are dropped"
    );
    // What the model is told comes from the platform's echo, and the follow-up turn sees it.
    let echoed = text_of(&results[0]);
    assert!(echoed.contains("[dice result:"), "{echoed}");
    let observed = echoed
        .trim_start_matches(|c| c != ']')
        .trim_start_matches(']')
        .trim();
    let followup = format!("{:?}", r.fake.recorded()[1].conversation);
    assert!(followup.contains(observed), "{observed} in {followup}");
    assert_eq!(report.end, RunEnd::Delivered, "end_turn defaults to true");
    assert_eq!(report.turns, 2);
    assert_eq!(r.world.sent().len(), 2);
}

#[tokio::test]
async fn the_per_run_send_limit_is_a_tool_level_refusal() {
    let r = rig(vec![]);
    let calls = (0..5).map(|i| send(&i.to_string())).collect();
    let results = try_calls(&r, calls).await;
    assert_eq!(
        results.iter().filter(|x| x.outcome == Outcome::Ok).count(),
        4
    );
    assert_eq!(
        results[4].outcome,
        Outcome::Refused(RefusalReason::LimitReached)
    );
    assert_eq!(r.world.sent().len(), 4);
}

#[tokio::test]
async fn delivery_failures_are_reported_distinctly() {
    let r = rig(vec![]);
    r.world
        .fail_next_send(DeliveryError::Rejected("muted by the platform".into()));
    let results = try_calls(&r, vec![send("x")]).await;
    assert_eq!(results[0].outcome, Outcome::Error(ErrorKind::Execution));
    assert!(text_of(&results[0]).contains("muted by the platform"));

    let r = rig(vec![]);
    r.world.fail_next_send(DeliveryError::EchoMissing);
    let results = try_calls(&r, vec![send("x")]).await;
    assert!(text_of(&results[0]).contains("do not send it again"));

    let r = rig(vec![]);
    r.world
        .fail_next_send(DeliveryError::Unavailable("gateway down".into()));
    let results = try_calls(&r, vec![send("x")]).await;
    assert_eq!(results[0].outcome, Outcome::Error(ErrorKind::Unavailable));
}

#[tokio::test]
async fn a_send_that_failed_does_not_use_up_the_send_budget() {
    // The validation failure happens before a slot is claimed.
    let r = rig(vec![]);
    let mut calls: Vec<_> = (0..4).map(|_| send("[at:77] hi")).collect();
    calls.push(send("ok"));
    let results = try_calls(&r, calls).await;
    assert_eq!(results[4].outcome, Outcome::Ok);
}

#[tokio::test]
async fn a_members_contact_card_is_sent_alone_and_only_for_members_of_this_chat() {
    let r = rig(vec![]);
    r.world.say(2, 2, "who runs the printer?");
    let results = try_calls(
        &r,
        vec![
            send("[contact:2]"),
            send("[contact:2] here"),
            send("[contact:77]"),
        ],
    )
    .await;
    assert_eq!(results[0].outcome, Outcome::Ok, "{}", text_of(&results[0]));
    assert_eq!(
        results[1].outcome,
        Outcome::Error(ErrorKind::InvalidArguments),
        "a card is a message of its own"
    );
    assert_eq!(
        results[2].outcome,
        Outcome::Refused(RefusalReason::NotAllowed),
        "no card for anyone outside this chat"
    );
    assert_eq!(r.world.sent(), [vec![OutSegment::Contact(account(2))]]);
}
