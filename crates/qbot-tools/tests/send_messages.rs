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
        turn = turn.call("send_messages", args);
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
async fn a_face_copied_from_the_chat_with_its_name_is_the_same_face() {
    let r = rig(vec![]);
    let results = try_calls(&r, vec![send("[face:14:smile]"), send("[face:]")]).await;
    assert_eq!(results[0].outcome, Outcome::Ok, "{}", text_of(&results[0]));
    assert_eq!(r.world.sent()[0], [OutSegment::Face(14)]);
    assert_ne!(results[1].outcome, Outcome::Ok, "a face needs its number");
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
        "send_messages",
        json!({ "messages": [" [dice] "], "end_turn": false }),
    )));
    r.fake.push(reply(call(
        "send_messages",
        json!({ "messages": ["nice roll"] }),
    )));
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
    r.world
        .fail_next_send(DeliveryError::Unconfirmed("no echo".into()));
    let results = try_calls(&r, vec![send("x")]).await;
    assert!(text_of(&results[0]).contains("do not send it again"));

    let r = rig(vec![]);
    r.world
        .fail_next_send(DeliveryError::Unavailable("gateway down".into()));
    let results = try_calls(&r, vec![send("x")]).await;
    assert_eq!(results[0].outcome, Outcome::Error(ErrorKind::Unavailable));
}

#[tokio::test]
async fn only_a_message_that_may_be_in_the_group_counts_as_sent() {
    // Refused by the platform: nothing reached the group, so the slot is given back and the
    // run did not speak.
    let r = rig(vec![]);
    r.world
        .fail_next_send(DeliveryError::Rejected("muted by the platform".into()));
    r.fake.push(reply(
        qbot_llm::fake::FakeReply::new().call("send_messages", send("x")),
    ));
    r.fake.push(done());
    let report = r.ask(1, 1, "hello bot").await;
    assert_eq!((report.sends, report.end), (0, RunEnd::Completed));

    // Unconfirmed: the message may be there, so it counts.
    let r = rig(vec![]);
    r.world
        .fail_next_send(DeliveryError::Unconfirmed("no echo".into()));
    r.fake.push(reply(
        qbot_llm::fake::FakeReply::new().call("send_messages", send("x")),
    ));
    r.fake.push(done());
    assert_eq!(r.ask(1, 1, "hello bot").await.sends, 1);

    // Four refused sends leave the whole budget of four.
    let r = rig(vec![]);
    let mut calls = Vec::new();
    for _ in 0..4 {
        calls.push(send("x"));
    }
    for i in 0..4 {
        calls.push(send(&i.to_string()));
    }
    let mut turn = qbot_llm::fake::FakeReply::new();
    for args in calls {
        turn = turn.call("send_messages", args);
    }
    r.fake.push(reply(turn));
    r.fake.push(done());
    for _ in 0..4 {
        r.world
            .fail_next_send(DeliveryError::Unavailable("gateway down".into()));
    }
    let report = r.ask(1, 1, "hello bot").await;
    assert_eq!(report.sends, 4);
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

/// One `send_messages` call with these messages, then done if it did not end the run.
async fn batch(r: &Rig, messages: &[&str], end_turn: Option<bool>) -> qbot_agent::RunReport {
    let mut args = json!({ "messages": messages });
    if let Some(end) = end_turn {
        args["end_turn"] = json!(end);
    }
    r.fake.push(reply(call("send_messages", args)));
    r.fake.push(done());
    r.ask(1, 1, "tell me about it").await
}

fn texts(r: &Rig) -> Vec<String> {
    r.world
        .sent()
        .iter()
        .map(|segments| match segments.as_slice() {
            [OutSegment::Text(text)] => text.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn one_call_sends_several_messages_in_order_and_reports_each() {
    let r = rig(vec![]);
    let report = batch(&r, &["first part", "second part", "and the last"], None).await;
    assert_eq!(texts(&r), ["first part", "second part", "and the last"]);
    let results = r.last_run_results();
    assert_eq!(results.len(), 1, "one tool call, one result");
    assert_eq!(results[0].outcome, Outcome::Ok);
    let lines: Vec<String> = text_of(&results[0]).lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 3);
    for (line, text) in lines
        .iter()
        .zip(["first part", "second part", "and the last"])
    {
        assert!(
            line.starts_with("sent [msg:") && line.ends_with(text),
            "{line}"
        );
    }
    // Every message counts, the call ends the run, and the model was asked once.
    assert_eq!(
        (report.sends, report.end, report.turns),
        (3, RunEnd::Delivered, 1)
    );
    assert_eq!(report.tool_calls, 1);
}

#[tokio::test]
async fn the_model_continues_only_after_every_message_is_echoed() {
    let r = rig(vec![]);
    r.world.set_echo_delay(std::time::Duration::from_millis(50));
    r.fake.push(reply(call(
        "send_messages",
        json!({ "messages": ["one", "two"], "end_turn": false }),
    )));
    r.fake.push(reply(call("stay_silent", json!({}))));
    let report = r.ask(1, 1, "go on").await;
    assert_eq!(report.sends, 2);
    // The next turn sees both delivered messages in the tool result, and no echo of them again
    // as new chat.
    let followup = format!("{:?}", r.fake.recorded()[1].conversation);
    assert!(
        followup.contains("one") && followup.contains("two"),
        "{followup}"
    );
    let echoes_as_chat = r.fake.recorded()[1]
        .conversation
        .items()
        .iter()
        .filter(|i| matches!(i, qbot_llm::ConvItem::Message(m) if format!("{m:?}").contains("bot: one")))
        .count();
    assert_eq!(
        echoes_as_chat, 0,
        "the run's own messages are not shown again"
    );
}

#[tokio::test]
async fn a_mistake_in_any_message_sends_none_of_them() {
    let r = rig(vec![]);
    r.world.say(2, 2, "hi");
    batch(&r, &["fine", "[at:77] who?", "also fine"], None).await;
    assert!(r.world.sent().is_empty(), "nothing went out");
    let result = &r.last_run_results()[0];
    assert_eq!(result.outcome, Outcome::Refused(RefusalReason::NotAllowed));
    let text = text_of(result);
    assert!(
        text.starts_with("message 2:") && text.contains("nothing was sent"),
        "{text}"
    );

    let r = rig(vec![]);
    batch(&r, &["ok", "[dice] and words"], None).await;
    assert!(r.world.sent().is_empty());
    assert_eq!(
        r.last_run_results()[0].outcome,
        Outcome::Error(ErrorKind::InvalidArguments)
    );

    let r = rig(vec![]);
    batch(&r, &[], None).await;
    assert!(r.world.sent().is_empty());
    assert!(text_of(&r.last_run_results()[0]).contains("at least one message"));
}

#[tokio::test]
async fn a_batch_larger_than_what_is_left_of_the_limit_is_refused_whole() {
    // The rig allows four messages a run: two now leave two.
    let r = rig(vec![]);
    let two = call(
        "send_messages",
        json!({ "messages": ["a", "b"], "end_turn": false }),
    );
    let three = call(
        "send_messages",
        json!({ "messages": ["c", "d", "e"], "end_turn": false }),
    );
    r.fake.push(reply(two.clone()));
    r.fake.push(reply(three));
    r.fake.push(reply(two));
    r.fake.push(done());
    let report = r.ask(1, 1, "talk").await;
    assert_eq!(texts(&r), ["a", "b", "a", "b"], "the three never went out");
    let results = r.last_run_results();
    assert_eq!(
        results[1].outcome,
        Outcome::Refused(RefusalReason::LimitReached)
    );
    assert!(
        text_of(&results[1]).contains("only 2 more"),
        "{}",
        text_of(&results[1])
    );
    assert_eq!(report.sends, 4);
}

#[tokio::test]
async fn a_failure_stops_the_batch_and_nothing_is_sent_twice() {
    // Refused by the platform: the first went out, the second did not, the third was not tried.
    let r = rig(vec![]);
    r.world
        .fail_send_after(1, DeliveryError::Rejected("muted".into()));
    let report = batch(&r, &["one", "two", "three"], None).await;
    assert_eq!(texts(&r), ["one"]);
    let result = &r.last_run_results()[0];
    assert_eq!(result.outcome, Outcome::Error(ErrorKind::Execution));
    let text = text_of(result);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with("sent [msg:") && lines[0].ends_with("one"),
        "{text}"
    );
    assert!(
        lines[1].starts_with("message 2 failed:") && lines[1].contains("muted"),
        "{text}"
    );
    assert!(
        lines[2].starts_with("messages 3 to 3 were not sent"),
        "{text}"
    );
    assert_eq!(report.sends, 1, "only what reached the group counts");
    assert_eq!(report.turns, 2, "the failed call did not end the run");

    // Unconfirmed: it may be in the group, so it counts, is not retried, and the rest wait.
    let r = rig(vec![]);
    r.world
        .fail_send_after(0, DeliveryError::Unconfirmed("no echo".into()));
    let report = batch(&r, &["one", "two"], None).await;
    assert!(r.world.sent().is_empty(), "the sim never delivered it");
    let text = text_of(&r.last_run_results()[0]);
    assert!(
        text.contains("message 1 failed:") && text.contains("do not send it again"),
        "{text}"
    );
    assert!(text.contains("messages 2 to 2 were not sent"), "{text}");
    assert_eq!(report.sends, 1);

    // No connection at all: unavailable, nothing counts.
    let r = rig(vec![]);
    r.world
        .fail_send_after(0, DeliveryError::Unavailable("gateway down".into()));
    let report = batch(&r, &["one", "two"], None).await;
    assert_eq!(
        r.last_run_results()[0].outcome,
        Outcome::Error(ErrorKind::Unavailable)
    );
    assert_eq!((report.sends, report.end), (0, RunEnd::Completed));
}

#[tokio::test]
async fn end_turn_false_lets_the_run_go_on_after_a_batch_with_a_dice_roll() {
    let r = rig(vec![]);
    r.fake.push(reply(call(
        "send_messages",
        json!({ "messages": ["let's see", "[dice]"], "end_turn": false }),
    )));
    r.fake.push(reply(call(
        "send_messages",
        json!({ "messages": ["there it is"] }),
    )));
    let report = r.ask(1, 1, "roll for me").await;
    assert_eq!(r.world.sent()[1], [OutSegment::Dice]);
    let first = text_of(&r.last_run_results()[0]);
    assert!(first.contains("[dice result:"), "{first}");
    assert_eq!(
        (report.turns, report.sends, report.end),
        (2, 3, RunEnd::Delivered)
    );
}
