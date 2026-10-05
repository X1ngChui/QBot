#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use qbot_context::{ErrorKind, Outcome, RefusalReason, RunEnd};
use qbot_core::UnixMillis;
use qbot_sched::{Origin, TimerOutcome, TimerState, When};
use serde_json::json;
use tokio::time::sleep;

async fn one_call(r: &Rig, tool: &str, args: serde_json::Value) -> qbot_context::ToolResult {
    r.fake.push(reply(call(tool, args)));
    r.fake.push(done());
    r.ask(1, 1, "hello").await;
    r.last_run_results().remove(0)
}

#[tokio::test(start_paused = true)]
async fn scheduling_creates_a_pending_group_task_with_a_readable_time() {
    let r = rig(vec![]);
    let result = one_call(
        &r,
        "schedule_task",
        json!({ "intent": "check the oven", "delay_seconds": 600 }),
    )
    .await;
    assert_eq!(result.outcome, Outcome::Ok);
    assert!(
        text_of(&result).contains("task 1 (pending) due 2027-01-15T08:10:00Z: check the oven"),
        "{}",
        text_of(&result)
    );

    let timer = r.timer(1).await;
    let spec = timer.wake().unwrap();
    assert_eq!(
        (spec.group, spec.chain.depth, spec.origin),
        (group(), 0, Origin::Model)
    );
    assert_eq!(timer.due_at, UnixMillis::new(START + 600_000));
}

#[tokio::test(start_paused = true)]
async fn run_at_takes_an_rfc3339_timestamp_with_any_offset() {
    let r = rig(vec![]);
    let result = one_call(
        &r,
        "schedule_task",
        json!({ "intent": "x", "run_at": "2027-01-15T17:20:00+09:00" }),
    )
    .await;
    assert!(
        text_of(&result).contains("2027-01-15T08:20:00Z"),
        "{}",
        text_of(&result)
    );
}

#[tokio::test(start_paused = true)]
async fn bad_scheduling_arguments_are_explained_to_the_model() {
    let r = rig(vec![]);
    for args in [
        json!({ "intent": "x" }),
        json!({ "intent": "x", "run_at": "2027-01-15T09:00:00Z", "delay_seconds": 600 }),
        json!({ "intent": "x", "run_at": "tomorrow at noon" }),
        json!({ "intent": "x", "delay_seconds": 60 }),
        json!({ "intent": "  ", "delay_seconds": 600 }),
    ] {
        let result = one_call(&r, "schedule_task", args.clone()).await;
        assert_eq!(
            result.outcome,
            Outcome::Error(ErrorKind::InvalidArguments),
            "{args}: {}",
            text_of(&result)
        );
    }
    assert!(r.store.all().is_empty(), "nothing was created");
}

#[tokio::test(start_paused = true)]
async fn listing_getting_updating_and_cancelling_are_scoped_to_the_group() {
    let r = rig(vec![]);
    let mine = r
        .service
        .create(
            group(),
            "mine",
            When::After(Duration::from_secs(900)),
            None,
            Origin::Owner,
        )
        .await
        .unwrap();
    let theirs = r
        .service
        .create(
            other_group(),
            "theirs",
            When::After(Duration::from_secs(900)),
            None,
            Origin::Owner,
        )
        .await
        .unwrap();

    let listing = one_call(&r, "list_tasks", json!({})).await;
    assert!(text_of(&listing).contains("mine") && !text_of(&listing).contains("theirs"));

    let foreign = one_call(&r, "get_task", json!({ "id": theirs.id.get() })).await;
    assert_eq!(foreign.outcome, Outcome::Error(ErrorKind::InvalidArguments));
    let foreign = one_call(&r, "cancel_task", json!({ "id": theirs.id.get() })).await;
    assert_eq!(foreign.outcome, Outcome::Error(ErrorKind::InvalidArguments));

    let updated = one_call(
        &r,
        "update_task",
        json!({ "id": mine.id.get(), "intent": "mine, revised", "delay_seconds": 1200 }),
    )
    .await;
    assert!(
        text_of(&updated).contains("mine, revised") && text_of(&updated).contains("08:20:00Z"),
        "{}",
        text_of(&updated)
    );
    let nothing = one_call(&r, "update_task", json!({ "id": mine.id.get() })).await;
    assert_eq!(nothing.outcome, Outcome::Error(ErrorKind::InvalidArguments));

    let cancelled = one_call(&r, "cancel_task", json!({ "id": mine.id.get() })).await;
    assert_eq!(cancelled.outcome, Outcome::Ok);
    let again = one_call(&r, "cancel_task", json!({ "id": mine.id.get() })).await;
    assert_eq!(again.outcome, Outcome::Refused(RefusalReason::Conflict));
    assert_eq!(
        r.timer(theirs.id.get()).await.state,
        TimerState::Pending,
        "the other group's task is untouched"
    );
}

#[tokio::test(start_paused = true)]
async fn listings_page_five_at_a_time() {
    let r = rig(vec![]);
    for i in 0..7 {
        r.service
            .create(
                group(),
                &format!("task {i}"),
                When::After(Duration::from_secs(600 + i * 60)),
                None,
                Origin::Owner,
            )
            .await
            .unwrap();
    }
    let first = text_of(&one_call(&r, "list_tasks", json!({})).await);
    assert_eq!(first.lines().count(), 6, "{first}");
    assert!(first.contains("more tasks on page 1"));
    let second = text_of(&one_call(&r, "list_tasks", json!({ "page": 1 })).await);
    assert_eq!(second.lines().count(), 2);
    assert!(text_of(&one_call(&r, "list_tasks", json!({ "page": 5 })).await).contains("no tasks"));
}

#[tokio::test(start_paused = true)]
async fn a_task_the_model_schedules_wakes_it_later_and_it_can_speak_then() {
    let r = rig(vec![]);
    r.fake.push(reply(call(
        "schedule_task",
        json!({ "intent": "remind them about the meeting", "delay_seconds": 600 }),
    )));
    r.fake.push(done());
    r.ask(1, 1, "remind us in ten minutes").await;
    let handle = r.start();

    // The woken run speaks.
    r.fake.push(reply(call(
        "send_message",
        json!({ "text": "Meeting time!" }),
    )));
    sleep(Duration::from_secs(601)).await;
    sleep(Duration::from_secs(1)).await;

    assert_eq!(
        r.timer(1).await.state,
        TimerState::Done(TimerOutcome::Ran(RunEnd::Delivered))
    );
    assert_eq!(r.world.sent().len(), 1);
    let wake_prompt = format!("{:?}", r.fake.recorded().last().unwrap().conversation);
    assert!(wake_prompt.contains("remind them about the meeting"));
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn follow_ups_continue_the_chain_and_the_depth_guard_refuses_loops() {
    let r = rig(vec![]);
    let root = r
        .service
        .create(
            group(),
            "keep watching",
            When::After(Duration::from_secs(600)),
            None,
            Origin::Owner,
        )
        .await
        .unwrap();
    r.fake.push(reply(call(
        "schedule_task",
        json!({ "intent": "keep watching, again", "delay_seconds": 600 }),
    )));
    r.fake.push(done());
    let handle = r.start();
    sleep(Duration::from_secs(601)).await;
    sleep(Duration::from_secs(1)).await;

    let follow_up = r.timer(2).await;
    let chain = follow_up.wake().unwrap().chain;
    assert_eq!(
        (chain.id.get(), chain.depth),
        (root.id.get(), 1),
        "same chain, one deeper"
    );
    assert_eq!(follow_up.wake().unwrap().origin, Origin::Model);

    // Only the task under test should fire next, so drop the follow-up created above.
    r.service.cancel(group(), follow_up.id).await.unwrap();
    // A task already at the depth limit cannot schedule another follow-up.
    let deepest = qbot_agent::Chain {
        id: chain.id,
        depth: 23,
    };
    let at_limit = r
        .service
        .create(
            group(),
            "last link",
            When::After(Duration::from_secs(600)),
            Some(deepest),
            Origin::Model,
        )
        .await
        .unwrap();
    r.fake.push(reply(call(
        "schedule_task",
        json!({ "intent": "one more", "delay_seconds": 600 }),
    )));
    r.fake.push(done());
    sleep(Duration::from_secs(700)).await;
    let results = r.last_run_results();
    assert_eq!(
        results[0].outcome,
        Outcome::Refused(RefusalReason::LimitReached),
        "{}",
        text_of(&results[0])
    );
    assert!(matches!(
        r.timer(at_limit.id.get()).await.state,
        TimerState::Done(TimerOutcome::Ran(_))
    ));
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn history_search_returns_newest_first_with_ids_and_bounds_its_own_output() {
    let r = rig(vec![]);
    r.world.say(2, 2, "the Deploy failed");
    r.world.say(3, 3, "unrelated");
    let second = r.world.say(2, 2, "deploy is fixed now");
    let found = text_of(&one_call(&r, "search_history", json!({ "query": "DEPLOY" })).await);
    let lines: Vec<_> = found.lines().collect();
    assert_eq!(lines.len(), 2, "{found}");
    assert!(lines[0].starts_with(&format!(
        "[msg:{}] member:2: deploy is fixed",
        second.message.get()
    )));
    assert!(lines[1].contains("the Deploy failed"));

    assert_eq!(
        text_of(
            &one_call(
                &r,
                "search_history",
                json!({ "query": "nothing like this" })
            )
            .await
        ),
        "no matches"
    );
    for args in [
        json!({ "query": " " }),
        json!({ "query": "x", "limit": 0 }),
        json!({ "query": "x", "limit": 51 }),
    ] {
        assert_eq!(
            one_call(&r, "search_history", args).await.outcome,
            Outcome::Error(ErrorKind::InvalidArguments)
        );
    }
}

#[tokio::test]
async fn history_search_understands_logical_queries_and_a_speaker() {
    let r = rig(vec![]);
    r.world.say(2, 2, "printer broken");
    r.world.say(3, 3, "the copier and the printer");
    r.world.say(2, 2, "scanner works");
    r.world.say(3, 3, "scanner too");
    let found = |args| {
        let r = &r;
        async move {
            text_of(&one_call(r, "search_history", args).await)
                .lines()
                .map(|l| l.split_once(": ").map_or(l, |(_, t)| t).to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        found(json!({ "query": "(printer OR scanner) -copier" })).await,
        ["scanner too", "scanner works", "printer broken"]
    );
    assert_eq!(
        found(json!({ "query": "printer OR scanner", "speaker": 2 })).await,
        ["scanner works", "printer broken"],
        "only member 2"
    );
    // A malformed query is explained to the model, with where it goes wrong.
    let bad = one_call(&r, "search_history", json!({ "query": "(printer OR" })).await;
    assert_eq!(bad.outcome, Outcome::Error(ErrorKind::InvalidArguments));
    assert!(text_of(&bad).contains("character"), "{}", text_of(&bad));
    let only_not = one_call(&r, "search_history", json!({ "query": "-printer" })).await;
    assert!(text_of(&only_not).contains("only excludes"));
}

#[tokio::test(start_paused = true)]
async fn tool_specs_come_from_the_argument_types() {
    let r = rig(vec![]);
    r.fake.push(done());
    r.ask(1, 1, "hi").await;
    let names = &r.fake.recorded()[0].tool_names;
    assert_eq!(
        names,
        &[
            "send_message",
            "stay_silent",
            "search_history",
            "schedule_task",
            "list_tasks",
            "get_task",
            "update_task",
            "cancel_task"
        ]
    );
}
