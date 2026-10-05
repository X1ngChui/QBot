#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use qbot_agent::{RunLimits, UsageDetail};
use qbot_context::{ErrorKind, Item, Meta, Outcome, Part, RefusalReason, RunEnd};
use qbot_llm::fake::{FakeReply, Step};
use qbot_llm::{Content, ConvItem, FinishReason, LlmError, Provider, ReplayPlan};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn tool_results(report: &qbot_agent::RunReport) -> Vec<(&qbot_context::ToolResult,)> {
    report
        .transcript
        .items()
        .iter()
        .filter_map(|i| {
            if let Item::ToolResult(r) = i {
                Some((r,))
            } else {
                None
            }
        })
        .collect()
}

fn text_of(result: &qbot_context::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|p| {
            if let Part::Text(t) = p {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn a_send_ends_the_run_with_a_single_model_call() {
    let h = harness(vec![reply_ok(say("hi"))]);
    h.world.say(1, 1, "hello bot");
    let report = run_once(&h, addressed(1, 101)).await;

    assert_eq!(report.end, RunEnd::Delivered);
    assert_eq!(report.turns, 1);
    assert_eq!(report.sends, 1);
    assert_eq!(
        h.fake.recorded().len(),
        1,
        "no extra round just to say finish"
    );
    assert_eq!(h.world.sent().len(), 1);
    assert!(matches!(
        report.transcript.items().last(),
        Some(Item::Meta(Meta::RunEnded(RunEnd::Delivered)))
    ));
}

#[tokio::test]
async fn end_turn_false_lets_the_model_continue_after_a_send() {
    let first = FakeReply::new().call("say", json!({ "text": "one", "end_turn": false }));
    let h = harness(vec![
        reply_ok(first),
        reply_ok(FakeReply::new().text("done")),
    ]);
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.end, RunEnd::Completed);
    assert_eq!(report.turns, 2);
    assert_eq!(h.world.sent().len(), 1, "assistant text is never delivered");
}

#[tokio::test]
async fn a_tool_free_turn_ends_silently() {
    let h = harness(vec![reply_ok(FakeReply::new().text("I will not reply"))]);
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.end, RunEnd::Completed);
    assert!(h.world.sent().is_empty());
    assert_eq!(report.sends, 0);
}

#[tokio::test(start_paused = true)]
async fn reads_run_concurrently_and_sends_run_after_them_in_call_order() {
    let turn = FakeReply::new()
        .call("say", json!({ "text": "first", "end_turn": false }))
        .call("slow", json!({ "ms": 1000 }))
        .call("say", json!({ "text": "second", "end_turn": false }))
        .call("slow", json!({ "ms": 1000 }))
        .call("echo", json!({ "text": "x" }));
    let h = harness(vec![
        reply_ok(turn),
        reply_ok(FakeReply::new().text("done")),
    ]);
    h.world.say(1, 1, "hello");
    let started = tokio::time::Instant::now();
    let report = run_once(&h, addressed(1, 101)).await;

    assert_eq!(
        started.elapsed(),
        Duration::from_secs(1),
        "two 1s reads overlap"
    );
    let order = h.order.lock().unwrap().clone();
    let first_say = order.iter().position(|s| s == "say").unwrap();
    assert!(
        order[..first_say].len() == 3,
        "all three reads start before the first send: {order:?}"
    );
    assert_eq!(order[first_say..], ["say", "say"]);

    // Results come back in call order whatever the execution order was.
    let results: Vec<String> = tool_results(&report)
        .iter()
        .map(|(r,)| text_of(r))
        .collect();
    assert!(results[0].starts_with("sent"), "{results:?}");
    assert_eq!(results[1], "slept");
    assert!(results[2].starts_with("sent"));
    assert_eq!(results[4], "echo: x");
    let sent: Vec<_> = h.world.sent();
    assert_eq!(sent[0], [qbot_agent::OutSegment::Text("first".into())]);
    assert_eq!(sent[1], [qbot_agent::OutSegment::Text("second".into())]);
}

#[tokio::test]
async fn bad_calls_get_their_own_results_and_never_reject_the_batch() {
    let turn = FakeReply::new()
        .call("echo", json!({ "text": "ok" }))
        .call("echo", json!({ "wrong": 1 }))
        .call("slow", json!({ "ms": "not a number" }));
    let h = harness(vec![
        reply_ok(turn),
        reply_ok(FakeReply::new().text("done")),
    ]);
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;

    let results = tool_results(&report);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].0.outcome, Outcome::Ok);
    assert_eq!(
        results[1].0.outcome,
        Outcome::Error(ErrorKind::InvalidArguments)
    );
    assert_eq!(
        results[2].0.outcome,
        Outcome::Error(ErrorKind::InvalidArguments)
    );
    assert_eq!(
        report.end,
        RunEnd::Completed,
        "the model saw every result and carried on"
    );
    assert_eq!(report.turns, 2);
}

#[tokio::test]
async fn every_requested_call_runs_with_no_local_count_cap() {
    let mut turn = FakeReply::new();
    for i in 0..40 {
        turn = turn.call("echo", json!({ "text": i.to_string() }));
    }
    let h = harness(vec![
        reply_ok(turn),
        reply_ok(FakeReply::new().text("done")),
    ]);
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.tool_calls, 40);
    assert!(
        tool_results(&report)
            .iter()
            .all(|(r,)| r.outcome == Outcome::Ok)
    );
}

#[tokio::test]
async fn a_large_tool_result_is_passed_through_untouched() {
    // Tools bound their own output when it matters; the loop does not second-guess them.
    let big = "x".repeat(200_000);
    let h = harness(vec![
        reply_ok(FakeReply::new().call("echo", json!({ "text": big }))),
        reply_ok(FakeReply::new().text("done")),
    ]);
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    let results = tool_results(&report);
    assert_eq!(text_of(results[0].0).len(), 200_000 + "echo: ".len());
}

#[tokio::test]
async fn a_send_beyond_the_limit_is_refused_by_the_tool_not_the_loop() {
    let h = harness_with(
        vec![
            reply_ok(
                FakeReply::new()
                    .call("say", json!({"text":"a","end_turn":false}))
                    .call("say", json!({"text":"b","end_turn":false})),
            ),
            reply_ok(FakeReply::new().text("done")),
        ],
        RunLimits::default(),
        false,
        1,
    );
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    let results = tool_results(&report);
    assert_eq!(results[0].0.outcome, Outcome::Ok);
    assert_eq!(
        results[1].0.outcome,
        Outcome::Refused(RefusalReason::LimitReached)
    );
    assert_eq!(report.sends, 1);
}

#[tokio::test]
async fn new_chat_is_folded_in_between_turns_without_duplicating_the_echo() {
    let first = FakeReply::new().call("say", json!({ "text": "hello back", "end_turn": false }));
    let h = harness(vec![
        reply_ok(first),
        reply_ok(FakeReply::new().text("done")),
    ]);
    h.world.say(1, 1, "hello");
    h.world.chatter_after_next_send(2, 2, "me too");
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.end, RunEnd::Completed);

    let second_request = &h.fake.recorded()[1].conversation;
    let rendered: Vec<String> = second_request
        .items()
        .iter()
        .filter_map(|i| match i {
            ConvItem::Message(m) => m.content.iter().find_map(|c| {
                if let Content::Text(t) = c {
                    Some(t.clone())
                } else {
                    None
                }
            }),
            _ => None,
        })
        .collect();
    let joined = rendered.join("\n");
    assert!(
        joined.contains("member:2: me too"),
        "the arrival is shown: {joined}"
    );
    assert!(
        !joined.contains("bot: hello back"),
        "the bot's own echo is not shown a second time"
    );
    let acknowledged = second_request.items().iter().any(|i| match i {
        ConvItem::ToolResult(o) => format!("{o:?}").contains("sent [msg:"),
        _ => false,
    });
    assert!(acknowledged, "the tool result already reported the echo");
}

#[tokio::test]
async fn dice_results_are_observed_through_the_echo() {
    let first = FakeReply::new().call("say", json!({ "dice": true, "end_turn": false }));
    let h = harness(vec![
        reply_ok(first),
        reply_ok(FakeReply::new().text("nice roll")),
    ]);
    h.world.say(1, 1, "roll a dice");
    let report = run_once(&h, addressed(1, 101)).await;
    let results = tool_results(&report);
    let text = text_of(results[0].0);
    assert!(
        text.contains("[dice:"),
        "the model learns the real result: {text}"
    );
    assert_eq!(report.end, RunEnd::Completed);
}

#[tokio::test]
async fn a_blocked_member_stays_visible_as_marked_context() {
    let h = harness(vec![reply_ok(say("hi"))]);
    h.world.block(9);
    h.world.say(9, 9, "spam from blocked");
    h.world.say(1, 1, "hello bot");
    run_once(&h, addressed(1, 102)).await;
    let conv = &h.fake.recorded()[0].conversation;
    let chat = conv.items().iter().find_map(|i| match i {
        ConvItem::Message(m) => m.content.iter().find_map(|c| match c {
            Content::Text(t) if t.contains("[msg:") => Some(t.clone()),
            _ => None,
        }),
        _ => None,
    });
    let chat = chat.unwrap();
    assert!(
        chat.contains("member:9 (blocked: do not reply): spam from blocked"),
        "{chat}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_deadline_ends_a_hung_model_call_with_a_valid_transcript() {
    let mut h = harness(vec![]);
    let info = h.fake.info().clone();
    let deps = Arc::get_mut(&mut h.deps).unwrap();
    deps.provider = Arc::new(Hang(info));
    h.world.say(1, 1, "hello");
    let report = run_with_deadline(
        &h,
        addressed(1, 101),
        Duration::from_secs(30),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(report.end, RunEnd::Deadline);
    assert!(report.transcript.is_quiescent());
}

#[tokio::test]
async fn a_provider_error_ends_the_run_and_is_reported() {
    let h = harness(vec![Step::Fail(LlmError::RateLimited {
        retry_after: None,
    })]);
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.end, RunEnd::ModelError);
    assert_eq!(
        report.error,
        Some(LlmError::RateLimited { retry_after: None })
    );
    assert_eq!(
        h.sink
            .events()
            .iter()
            .filter(|e| matches!(
                e.detail,
                UsageDetail::Model {
                    error: Some("rate_limited"),
                    ..
                }
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn the_step_limit_ends_a_run_that_never_stops_calling_tools() {
    let limits = RunLimits { max_turns: 3 };
    let script = (0..5)
        .map(|i| reply_ok(FakeReply::new().call("echo", json!({ "text": i.to_string() }))))
        .collect();
    let h = harness_with(script, limits, false, 4);
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.end, RunEnd::StepLimit);
    assert_eq!(report.turns, 3);
    assert_eq!(h.fake.recorded().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn cancelling_mid_tool_closes_pending_calls_as_interrupted() {
    let turn = FakeReply::new().call("slow", json!({ "ms": 10_000 }));
    let h = harness(vec![reply_ok(turn)]);
    h.world.say(1, 1, "hello");
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        canceller.cancel();
    });
    let report = run_with_deadline(&h, addressed(1, 101), Duration::from_secs(600), &cancel).await;
    assert_eq!(report.end, RunEnd::Cancelled);
    assert!(report.transcript.is_quiescent());
    let results = tool_results(&report);
    assert_eq!(results[0].0.outcome, Outcome::Interrupted);
}

#[tokio::test]
async fn the_run_log_mirrors_the_transcript_and_a_log_failure_ends_the_run() {
    let h = harness(vec![reply_ok(say("hi"))]);
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    let logged = h.world.logged(report.run);
    let items: Vec<_> = logged.iter().map(|(_, i)| i.clone()).collect();
    assert_eq!(items, report.transcript.items());
    assert!(
        logged
            .iter()
            .enumerate()
            .all(|(i, (seq, _))| seq.index() == i),
        "dense sequence numbers"
    );

    let h = harness(vec![reply_ok(say("hi"))]);
    h.world.fail_run_log(true);
    h.world.say(1, 1, "hello");
    assert_eq!(
        run_once(&h, addressed(1, 101)).await.end,
        RunEnd::Environment
    );
}

#[tokio::test]
async fn the_prefix_and_tools_stay_identical_across_turns_and_continuations_use_deltas() {
    let script = vec![
        reply_ok(FakeReply::new().call("echo", json!({"text":"a"}))),
        reply_ok(FakeReply::new().call("echo", json!({"text":"b"}))),
        reply_ok(FakeReply::new().text("done")),
    ];
    let h = harness_with(script, RunLimits::default(), true, 4);
    h.world.say(1, 1, "hello");
    run_once(&h, addressed(1, 101)).await;

    let recorded = h.fake.recorded();
    assert_eq!(recorded.len(), 3);
    assert!(
        recorded
            .iter()
            .all(|r| r.tool_names == recorded[0].tool_names),
        "tools never change mid-run"
    );
    for pair in recorded.windows(2) {
        let n = pair[0].conversation.len();
        assert_eq!(
            pair[0].conversation.digest_prefix(n),
            pair[1].conversation.digest_prefix(n),
            "append-only prefix"
        );
    }
    assert_eq!(recorded[0].plan, ReplayPlan::Full);
    assert!(matches!(recorded[1].plan, ReplayPlan::Delta { .. }));
    assert!(matches!(recorded[2].plan, ReplayPlan::Delta { .. }));
}

#[tokio::test]
async fn earlier_turns_are_never_rewritten_so_every_request_extends_the_last() {
    let script = vec![
        reply_ok(FakeReply::new().call("say", json!({"text":"hello","end_turn":false}))),
        reply_ok(FakeReply::new().call("echo", json!({"text":"x"}))),
        reply_ok(FakeReply::new().call("echo", json!({"text":"y"}))),
        reply_ok(FakeReply::new().text("done")),
    ];
    let h = harness(script);
    h.world.say(1, 1, "hello");
    run_once(&h, addressed(1, 101)).await;

    let recorded = h.fake.recorded();
    for pair in recorded.windows(2) {
        let n = pair[0].conversation.len();
        assert_eq!(
            pair[0].conversation.digest_prefix(n),
            pair[1].conversation.digest_prefix(n),
            "an earlier request is an exact prefix of the next, so the provider cache keeps hitting"
        );
    }
    let last = &recorded[3].conversation;
    let all_ok = last.items().iter().all(|i| match i {
        ConvItem::ToolResult(o) => o.status == qbot_llm::ToolStatus::Ok,
        _ => true,
    });
    assert!(all_ok);
}

#[tokio::test]
async fn usage_events_cover_models_tools_and_the_run() {
    let h = harness(vec![
        reply_ok(FakeReply::new().call("echo", json!({"text":"a"}))),
        reply_ok(say("hi")),
    ]);
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    let events = h.sink.events();
    let models = events
        .iter()
        .filter(|e| matches!(e.detail, UsageDetail::Model { .. }))
        .count();
    let tools: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let UsageDetail::Tool { name, .. } = &e.detail {
                Some(name.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(models, 2);
    assert_eq!(tools, ["echo", "say"]);
    match &events.last().unwrap().detail {
        UsageDetail::RunEnded {
            end,
            usage,
            turns,
            tool_calls,
        } => {
            assert_eq!((*end, *turns, *tool_calls), (RunEnd::Delivered, 2, 2));
            assert_eq!(*usage, report.usage);
            assert!(usage.input_tokens > 0);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(FinishReason::ToolCalls, FinishReason::ToolCalls);
}

#[tokio::test]
async fn a_wake_trigger_carries_its_intent_as_a_lower_trust_note() {
    let h = harness(vec![reply_ok(FakeReply::new().text("nothing to say"))]);
    h.world.say(1, 1, "hello");
    let wake = qbot_agent::Trigger::Wake {
        timer: qbot_core::TimerId::new(7),
        intent: "check on the build".into(),
        chain: qbot_agent::Chain {
            id: qbot_core::ChainId::new(1),
            depth: 0,
        },
    };
    run_once(&h, wake).await;
    let conv = &h.fake.recorded()[0].conversation;
    let note = conv.items().iter().find_map(|i| match i {
        ConvItem::Message(m) if m.role == qbot_llm::Role::User => {
            m.content.iter().find_map(|c| match c {
                Content::Text(t) if t.contains("stored intent") => Some(t.clone()),
                _ => None,
            })
        }
        _ => None,
    });
    assert!(note.unwrap().contains("check on the build"));
}

#[tokio::test]
async fn a_context_too_long_error_falls_back_to_recaps_of_raw_episodes_once() {
    let h = harness(vec![
        Step::Fail(LlmError::ContextTooLong),
        reply_ok(say("hi")),
    ]);
    for text in ["line one", "line two", "line three", "hello bot"] {
        h.world.say(1, 1, text);
    }
    h.world.set_recaps(vec![qbot_agent::Recap {
        lines: 0..2,
        text: "they planned a picnic".into(),
        when: qbot_agent::RecapWhen::Overflow,
    }]);
    let report = run_once(&h, addressed(1, 103)).await;
    assert_eq!(report.end, RunEnd::Delivered);

    let recorded = h.fake.recorded();
    assert_eq!(recorded.len(), 2, "one retry after compacting");
    let (first, second) = (
        format!("{:?}", recorded[0].conversation),
        format!("{:?}", recorded[1].conversation),
    );
    assert!(first.contains("line one") && !first.contains("picnic"));
    assert!(second.contains("picnic"), "{second}");
    assert!(!second.contains("line one") && !second.contains("line two"));
    assert!(second.contains("line three") && second.contains("hello bot"));

    // The originals stay in the transcript; the summary names exactly their chat item.
    let items = report.transcript.items();
    let summary = items
        .iter()
        .find_map(|i| match i {
            Item::Summary(s) => Some(s),
            _ => None,
        })
        .unwrap();
    assert_eq!(summary.to.get(), summary.from.get() + 1);
    assert!(matches!(&items[summary.from.index()], Item::Chat(c) if c.lines().len() == 2));
    assert_eq!(
        items.iter().filter(|i| matches!(i, Item::Chat(_))).count(),
        2
    );
}

#[tokio::test]
async fn context_too_long_without_recaps_or_after_compacting_ends_the_run() {
    let h = harness(vec![Step::Fail(LlmError::ContextTooLong)]);
    h.world.say(1, 1, "hello bot");
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.end, RunEnd::ModelError);
    assert_eq!(h.fake.recorded().len(), 1);

    let h = harness(vec![
        Step::Fail(LlmError::ContextTooLong),
        Step::Fail(LlmError::ContextTooLong),
        reply_ok(say("never")),
    ]);
    h.world.say(1, 1, "old");
    h.world.say(1, 1, "hello bot");
    h.world.set_recaps(vec![qbot_agent::Recap {
        lines: 0..1,
        text: "earlier".into(),
        when: qbot_agent::RecapWhen::Overflow,
    }]);
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.end, RunEnd::ModelError);
    assert_eq!(report.error, Some(LlmError::ContextTooLong));
    assert_eq!(h.fake.recorded().len(), 2, "compaction is applied once");
}

#[tokio::test]
async fn older_batches_are_shown_as_their_summaries_from_the_first_request() {
    let h = harness(vec![reply_ok(say("hi"))]);
    for text in ["old one", "old two", "recent", "hello bot"] {
        h.world.say(1, 1, text);
    }
    h.world.set_recaps(vec![
        qbot_agent::Recap {
            lines: 0..2,
            text: "they argued about lunch".into(),
            when: qbot_agent::RecapWhen::Open,
        },
        qbot_agent::Recap {
            lines: 2..3,
            text: "fallback only".into(),
            when: qbot_agent::RecapWhen::Overflow,
        },
    ]);
    let report = run_once(&h, addressed(1, 103)).await;
    assert_eq!(report.end, RunEnd::Delivered);

    let recorded = h.fake.recorded();
    assert_eq!(recorded.len(), 1, "no error was needed to apply the policy");
    let seen = format!("{:?}", recorded[0].conversation);
    assert!(seen.contains("argued about lunch"), "{seen}");
    assert!(!seen.contains("old one") && !seen.contains("old two"));
    assert!(seen.contains("recent") && seen.contains("hello bot"));
    assert!(
        !seen.contains("fallback only"),
        "an overflow recap waits for an overflow"
    );

    // The summarized lines are still in the transcript, hidden behind the summary.
    let items = report.transcript.items();
    assert!(
        items
            .iter()
            .any(|i| matches!(i, Item::Chat(c) if c.lines().iter().any(|l| l.text == "old one")))
    );
    assert_eq!(
        items
            .iter()
            .filter(|i| matches!(i, Item::Summary(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn text_without_a_tool_call_is_pointed_out_once_and_the_next_turn_must_call_a_tool() {
    let h = harness(vec![
        reply_ok(FakeReply::new().text("sure, see you at eight")),
        reply_ok(say("sure, see you at eight")),
    ]);
    h.world.set_undelivered_note("Your text was not delivered.");
    h.world.say(1, 1, "are we meeting tonight?");
    let report = run_once(&h, addressed(1, 101)).await;

    assert_eq!(report.end, RunEnd::Delivered);
    assert_eq!(h.world.sent().len(), 1, "the reply reached the group");
    let recorded = h.fake.recorded();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].tool_choice, qbot_llm::ToolChoice::Auto);
    assert_eq!(
        recorded[1].tool_choice,
        qbot_llm::ToolChoice::Required,
        "the follow-up turn must call a tool"
    );
    assert!(
        format!("{:?}", recorded[1].conversation).contains("Your text was not delivered."),
        "the model is told why"
    );
}

#[tokio::test]
async fn the_note_is_given_once_and_a_silent_turn_stays_silent() {
    // After the note the model calls some other tool and then writes text again: the note is
    // not repeated, and the run ends there with nothing sent.
    let h = harness(vec![
        reply_ok(FakeReply::new().text("thinking out loud")),
        reply_ok(FakeReply::new().call("echo", json!({ "text": "x" }))),
        reply_ok(FakeReply::new().text("still only text")),
        reply_ok(say("never reached")),
    ]);
    h.world.set_undelivered_note("Your text was not delivered.");
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.end, RunEnd::Completed);
    let recorded = h.fake.recorded();
    assert_eq!(recorded.len(), 3);
    assert_eq!(
        recorded[2].tool_choice,
        qbot_llm::ToolChoice::Auto,
        "forced only once"
    );
    assert!(h.world.sent().is_empty());

    // A turn that writes nothing (here only whitespace) and calls no tool is plain silence: no
    // note, one call.
    let h = harness(vec![reply_ok(FakeReply::new().text("  "))]);
    h.world.set_undelivered_note("Your text was not delivered.");
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.end, RunEnd::Completed);
    assert_eq!(h.fake.recorded().len(), 1);
}

#[tokio::test]
async fn a_provider_that_forces_tools_only_without_reasoning_keeps_reasoning_off_afterwards() {
    use qbot_llm::{ForcedToolChoice, ReasoningEffort};
    let fake = qbot_llm::fake::FakeProvider::new(vec![
        reply_ok(FakeReply::new().text("the answer, as text")),
        reply_ok(FakeReply::new().call("echo", json!({ "text": "x" }))),
        reply_ok(say("the answer")),
    ])
    .forced_tool_choice(ForcedToolChoice::WithoutReasoning);
    let h = harness_from(fake, RunLimits::default(), 4);
    h.world.set_undelivered_note("Your text was not delivered.");
    h.world.say(1, 1, "hello");
    let report = run_once(&h, addressed(1, 101)).await;
    assert_eq!(report.end, RunEnd::Delivered);
    let reasoning: Vec<_> = h
        .fake
        .recorded()
        .iter()
        .map(|r| r.params.reasoning)
        .collect();
    assert_eq!(
        reasoning,
        [
            ReasoningEffort::Low,
            ReasoningEffort::Off,
            ReasoningEffort::Off
        ],
        "off for the forced turn and for the rest of the run"
    );
}
