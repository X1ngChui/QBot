#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The task tools with repeating tasks: created only when a member asks, from structured
//! arguments, and carried on by the scheduler without the model.

mod common;

use std::time::Duration;

use common::*;
use qbot_context::{ErrorKind, Outcome, RefusalReason};
use qbot_core::UnixMillis;
use qbot_sched::{Origin, Rule, TimeOfDay, TimerState, When};
use serde_json::json;
use tokio::time::sleep;

async fn one_call(r: &Rig, tool: &str, args: serde_json::Value) -> qbot_context::ToolResult {
    r.fake.push(reply(call(tool, args)));
    r.fake.push(done());
    r.ask(1, 1, "hello").await;
    r.last_run_results().remove(0)
}

#[tokio::test(start_paused = true)]
async fn the_repeat_arguments_are_typed_and_described_in_the_schema() {
    let r = rig(vec![]);
    let tools = qbot_agent::ToolSet::new()
        .with(qbot_tools::ScheduleTask(r.service.clone()))
        .unwrap()
        .with(qbot_tools::UpdateScheduledTask(r.service.clone()))
        .unwrap();
    for spec in tools.specs() {
        let props = &spec.schema["properties"];
        for field in ["repeat", "repeat_minutes", "repeat_time", "repeat_weekdays"] {
            assert!(
                props[field]["description"].is_string(),
                "{}.{field}: {}",
                spec.name,
                spec.schema
            );
        }
        let schema = spec.schema.to_string();
        for word in [
            "\"interval\"",
            "\"daily\"",
            "\"weekly\"",
            "\"mon\"",
            "\"sun\"",
        ] {
            assert!(schema.contains(word), "{}: {schema}", spec.name);
        }
        let required = spec.schema["required"].to_string();
        assert!(
            !required.contains("repeat"),
            "repeat is optional: {required}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_member_asking_for_a_recurring_task_gets_one_repeating_task() {
    let r = rig(vec![]);
    // 09:00 in Shanghai is 01:00Z; today's has passed (it is 16:00 there), so tomorrow's.
    let daily = one_call(
        &r,
        "schedule_task",
        json!({ "intent": "say good morning", "repeat": "daily", "repeat_time": "09:00" }),
    )
    .await;
    assert_eq!(daily.outcome, Outcome::Ok, "{}", text_of(&daily));
    assert!(
        text_of(&daily).contains(
            "task 1 (pending, repeats every day at 09:00 Asia/Shanghai) next due 2027-01-16T01:00:00Z: say good morning"
        ),
        "{}",
        text_of(&daily)
    );
    let repeat = r.timer(1).await.wake().unwrap().repeat.clone().unwrap();
    assert_eq!(
        (repeat.rule, repeat.zone.name()),
        (Rule::Daily(TimeOfDay::new(9, 0).unwrap()), "Asia/Shanghai")
    );

    // 2027-01-15 is a Friday; Monday 20:30 in Shanghai is 12:30Z on the 18th.
    let weekly = one_call(
        &r,
        "schedule_task",
        json!({ "intent": "weekly topic", "repeat": "weekly", "repeat_weekdays": ["thu", "mon"], "repeat_time": "20:30" }),
    )
    .await;
    assert!(
        text_of(&weekly).contains(
            "repeats every mon, thu at 20:30 Asia/Shanghai) next due 2027-01-18T12:30:00Z"
        ),
        "{}",
        text_of(&weekly)
    );

    let interval = one_call(
        &r,
        "schedule_task",
        json!({ "intent": "drink water", "repeat": "interval", "repeat_minutes": 120, "delay_seconds": 1800 }),
    )
    .await;
    assert!(
        text_of(&interval).contains("repeats every 2h) next due 2027-01-15T08:30:00Z"),
        "the first run is where it was asked for: {}",
        text_of(&interval)
    );
    assert_eq!(r.store.all().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn an_ordinary_task_never_becomes_a_repeating_one() {
    let r = rig(vec![]);
    let once = one_call(
        &r,
        "schedule_task",
        json!({ "intent": "check the oven", "delay_seconds": 600 }),
    )
    .await;
    assert_eq!(once.outcome, Outcome::Ok);
    assert!(!text_of(&once).contains("repeats"), "{}", text_of(&once));
    assert_eq!(r.timer(1).await.wake().unwrap().repeat, None);
    // Changing its time or intent keeps it a one-time task.
    let updated = one_call(
        &r,
        "update_task",
        json!({ "id": 1, "intent": "check the oven again", "delay_seconds": 900 }),
    )
    .await;
    assert_eq!(updated.outcome, Outcome::Ok);
    assert_eq!(r.timer(1).await.wake().unwrap().repeat, None);
}

#[tokio::test(start_paused = true)]
async fn bad_repeat_arguments_are_explained_and_create_nothing() {
    let r = rig(vec![]);
    for args in [
        json!({ "intent": "x", "delay_seconds": 600, "repeat_time": "09:00" }),
        json!({ "intent": "x", "repeat": "interval" }),
        json!({ "intent": "x", "repeat": "interval", "repeat_minutes": 0 }),
        json!({ "intent": "x", "repeat": "interval", "repeat_minutes": 30 }),
        json!({ "intent": "x", "repeat": "interval", "repeat_minutes": 60, "repeat_time": "09:00" }),
        json!({ "intent": "x", "repeat": "daily" }),
        json!({ "intent": "x", "repeat": "daily", "repeat_time": "9am" }),
        json!({ "intent": "x", "repeat": "daily", "repeat_time": "24:00" }),
        json!({ "intent": "x", "repeat": "daily", "repeat_time": "09:00", "repeat_weekdays": ["mon"] }),
        json!({ "intent": "x", "repeat": "weekly", "repeat_time": "09:00" }),
        json!({ "intent": "x", "repeat": "weekly", "repeat_time": "09:00", "repeat_weekdays": [] }),
        json!({ "intent": "x", "repeat": "weekly", "repeat_time": "09:00", "repeat_weekdays": ["funday"] }),
        json!({ "intent": "x", "repeat": "monthly", "repeat_time": "09:00" }),
        json!({ "intent": "x", "repeat": "daily", "repeat_time": "09:00", "delay_seconds": 60 }),
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
async fn only_a_members_request_starts_a_repeating_task() {
    let r = rig(vec![]);
    let task = r
        .service
        .create(
            group(),
            "check in",
            When::After(Duration::from_secs(600)),
            None,
            Origin::Owner,
        )
        .await
        .unwrap();
    let other = r
        .service
        .create(
            group(),
            "later",
            When::After(Duration::from_secs(7200)),
            None,
            Origin::Owner,
        )
        .await
        .unwrap();
    // The task's own run tries to start a repeating task, and to make another one repeat.
    r.fake.push(reply(
        call(
            "schedule_task",
            json!({ "intent": "check in daily", "repeat": "daily", "repeat_time": "10:00" }),
        )
        .call(
            "update_task",
            json!({ "id": other.id.get(), "repeat": "daily", "repeat_time": "10:00" }),
        ),
    ));
    r.fake.push(done());
    let handle = r.start();
    sleep(Duration::from_secs(602)).await;
    let results = r.last_run_results();
    assert_eq!(results.len(), 2);
    for result in &results {
        assert_eq!(
            result.outcome,
            Outcome::Refused(RefusalReason::NotAllowed),
            "{}",
            text_of(result)
        );
    }
    assert!(
        r.store
            .all()
            .iter()
            .all(|t| t.wake().unwrap().repeat.is_none()),
        "no repeating task exists"
    );
    assert!(matches!(
        r.timer(task.id.get()).await.state,
        TimerState::Done(_)
    ));
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_repeating_task_wakes_the_model_each_time_without_being_scheduled_again() {
    let r = rig(vec![]);
    let created = one_call(
        &r,
        "schedule_task",
        json!({ "intent": "post the hourly status", "repeat": "interval", "repeat_minutes": 60 }),
    )
    .await;
    assert_eq!(created.outcome, Outcome::Ok, "{}", text_of(&created));
    let handle = r.start();
    for n in 1..=3 {
        // Each run only speaks; none schedules anything.
        r.fake.push(reply(call(
            "send_messages",
            json!({ "messages": [format!("status {n}")] }),
        )));
        sleep(Duration::from_secs(3600)).await;
        sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(r.world.sent().len(), 3);
    assert_eq!(r.store.all().len(), 1, "still the one task");
    let timer = r.timer(1).await;
    assert_eq!(
        (timer.state.clone(), timer.due_at),
        (
            TimerState::Pending,
            UnixMillis::new(START).plus(Duration::from_secs(4 * 3600))
        )
    );
    let wake_prompt = format!("{:?}", r.fake.recorded().last().unwrap().conversation);
    assert!(
        wake_prompt.contains("task 1 woke up (it repeats)"),
        "{wake_prompt}"
    );

    // Listing shows the rule, the next time and how the last run went.
    let listed = text_of(&one_call(&r, "list_tasks", json!({})).await);
    assert!(
        listed.contains(
            "repeats every 1h) next due 2027-01-15T12:00:00Z: post the hourly status; last run, due 2027-01-15T11:00:00Z: delivered"
        ),
        "{listed}"
    );
    // Cancelling ends it and says so.
    let cancelled = one_call(&r, "cancel_task", json!({ "id": 1 })).await;
    assert!(
        text_of(&cancelled).contains("Nothing more is scheduled for it"),
        "{}",
        text_of(&cancelled)
    );
    sleep(Duration::from_secs(3 * 3600)).await;
    assert_eq!(r.world.sent().len(), 3, "no run after cancelling");
    r.shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_member_can_change_a_tasks_repeat_rule_in_this_group_only() {
    let r = rig(vec![]);
    one_call(
        &r,
        "schedule_task",
        json!({ "intent": "stretch", "delay_seconds": 600 }),
    )
    .await;
    let made = one_call(
        &r,
        "update_task",
        json!({ "id": 1, "repeat": "weekly", "repeat_weekdays": ["fri"], "repeat_time": "18:00" }),
    )
    .await;
    assert_eq!(made.outcome, Outcome::Ok, "{}", text_of(&made));
    // Friday 18:00 in Shanghai is 10:00Z today.
    assert!(
        text_of(&made)
            .contains("repeats every fri at 18:00 Asia/Shanghai) next due 2027-01-15T10:00:00Z"),
        "{}",
        text_of(&made)
    );
    let theirs = r
        .service
        .create(
            other_group(),
            "theirs",
            When::After(Duration::from_secs(600)),
            None,
            Origin::Owner,
        )
        .await
        .unwrap();
    let foreign = one_call(
        &r,
        "update_task",
        json!({ "id": theirs.id.get(), "repeat": "daily", "repeat_time": "08:00" }),
    )
    .await;
    assert_eq!(foreign.outcome, Outcome::Error(ErrorKind::InvalidArguments));
    assert_eq!(r.timer(theirs.id.get()).await.wake().unwrap().repeat, None);
}

#[tokio::test(start_paused = true)]
async fn only_a_members_request_changes_or_ends_a_repeating_task() {
    let r = rig(vec![]);
    let series = r
        .service
        .create_repeating(
            group(),
            "post the daily topic",
            Rule::Daily(TimeOfDay::new(20, 0).unwrap()),
            None,
            None,
            Origin::Owner,
        )
        .await
        .unwrap();
    let once = r
        .service
        .create(
            group(),
            "a one-time check",
            When::After(Duration::from_secs(7200)),
            None,
            Origin::Owner,
        )
        .await
        .unwrap();
    r.service
        .create(
            group(),
            "wake up",
            When::After(Duration::from_secs(600)),
            None,
            Origin::Owner,
        )
        .await
        .unwrap();
    // A task's own run tries to rewrite and end the series, and changes the one-shot task.
    r.fake.push(reply(
        call(
            "update_task",
            json!({ "id": series.id.get(), "intent": "something else" }),
        )
        .call("cancel_task", json!({ "id": series.id.get() }))
        .call(
            "update_task",
            json!({ "id": once.id.get(), "intent": "a one-time check, later" }),
        ),
    ));
    r.fake.push(done());
    let handle = r.start();
    sleep(Duration::from_secs(602)).await;
    let results = r.last_run_results();
    assert_eq!(
        results.iter().map(|r| r.outcome).collect::<Vec<_>>(),
        [
            Outcome::Refused(RefusalReason::NotAllowed),
            Outcome::Refused(RefusalReason::NotAllowed),
            Outcome::Ok
        ]
    );
    let kept = r.timer(series.id.get()).await;
    assert_eq!(
        (kept.state.clone(), kept.wake().unwrap().intent.as_str()),
        (TimerState::Pending, "post the daily topic")
    );
    // A member asking ends it.
    let cancelled = one_call(&r, "cancel_task", json!({ "id": series.id.get() })).await;
    assert_eq!(cancelled.outcome, Outcome::Ok, "{}", text_of(&cancelled));
    r.shutdown.cancel();
    handle.await.unwrap();
}
