#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use qbot_agent::Chain;
use qbot_core::{ChainId, TimerId};
use qbot_sched::{Origin, Rule, TaskError, TimeOfDay, TimerState, Weekdays, When};

#[tokio::test(start_paused = true)]
async fn only_loop_guards_are_enforced_at_creation() {
    let r = rig(vec![], 4);
    let create = |intent: &'static str, when| {
        let service = r.service.clone();
        async move {
            service
                .create(group(), intent, when, None, Origin::Model)
                .await
        }
    };
    // The 5-minute minimum is the self-wake guard.
    assert!(matches!(
        create("x", When::After(mins(4))).await,
        Err(TaskError::TooSoon { .. })
    ));
    assert!(create("x", When::After(mins(5))).await.is_ok());
    assert_eq!(
        create("   ", When::After(mins(10))).await.unwrap_err(),
        TaskError::EmptyIntent
    );

    // No horizon cap and no intent length cap.
    let far = std::time::Duration::from_secs(3 * 365 * 24 * 3600);
    assert!(
        create("a year or three away", When::After(far))
            .await
            .is_ok()
    );
    let long: &'static str = Box::leak("y".repeat(5000).into_boxed_str());
    assert!(create(long, When::After(mins(10))).await.is_ok());

    // An absolute time is judged the same way.
    let past = qbot_core::UnixMillis::new(START - 1000);
    assert!(matches!(
        create("x", When::At(past)).await,
        Err(TaskError::TooSoon { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn chains_inherit_and_are_depth_bounded() {
    let r = rig(vec![], 4);
    let root = r
        .service
        .create(group(), "root", When::After(mins(10)), None, Origin::Owner)
        .await
        .unwrap();
    let spec = root.wake().unwrap();
    assert_eq!((spec.chain.id.get(), spec.chain.depth), (root.id.get(), 0));

    let child = r
        .service
        .create(
            group(),
            "child",
            When::After(mins(10)),
            Some(spec.chain),
            Origin::Model,
        )
        .await
        .unwrap();
    let child_chain = child.wake().unwrap().chain;
    assert_eq!((child_chain.id, child_chain.depth), (spec.chain.id, 1));

    let deepest = Chain {
        id: ChainId::new(9),
        depth: 24,
    };
    assert_eq!(
        r.service
            .create(
                group(),
                "too deep",
                When::After(mins(10)),
                Some(deepest),
                Origin::Model
            )
            .await
            .unwrap_err(),
        TaskError::ChainTooDeep { limit: 24 }
    );
    let ok = Chain {
        id: ChainId::new(9),
        depth: 23,
    };
    assert!(
        r.service
            .create(
                group(),
                "just fits",
                When::After(mins(10)),
                Some(ok),
                Origin::Model
            )
            .await
            .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn the_pending_bound_is_per_group() {
    let r = rig(vec![], 4);
    for i in 0..50 {
        r.service
            .create(
                group(),
                &format!("t{i}"),
                When::After(mins(10)),
                None,
                Origin::Model,
            )
            .await
            .unwrap();
    }
    assert_eq!(
        r.service
            .create(
                group(),
                "one too many",
                When::After(mins(10)),
                None,
                Origin::Model
            )
            .await
            .unwrap_err(),
        TaskError::TooManyPending { limit: 50 }
    );
    assert!(
        r.service
            .create(
                other_group(),
                "other group",
                When::After(mins(10)),
                None,
                Origin::Model
            )
            .await
            .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn update_and_cancel_apply_to_pending_tasks_of_the_same_group_only() {
    let r = rig(vec![], 4);
    let t = r
        .service
        .create(
            group(),
            "water the plants",
            When::After(mins(10)),
            None,
            Origin::Model,
        )
        .await
        .unwrap();

    assert_eq!(
        r.service.get(other_group(), t.id).await.unwrap_err(),
        TaskError::NotFound
    );
    assert_eq!(
        r.service.cancel(other_group(), t.id).await.unwrap_err(),
        TaskError::NotFound
    );
    assert_eq!(
        r.service
            .update(group(), t.id, None, None, None)
            .await
            .unwrap_err(),
        TaskError::NothingToChange
    );
    assert_eq!(
        r.service
            .update(group(), t.id, Some(""), None, None)
            .await
            .unwrap_err(),
        TaskError::EmptyIntent
    );
    let updated = r
        .service
        .update(
            group(),
            t.id,
            Some("water the ferns"),
            Some(When::After(mins(30))),
            None,
        )
        .await
        .unwrap();
    assert_eq!(updated.wake().unwrap().intent, "water the ferns");
    assert!(updated.due_at > t.due_at);

    r.service.cancel(group(), t.id).await.unwrap();
    assert_eq!(r.state(t.id).await, TimerState::Cancelled);
    assert_eq!(
        r.service.cancel(group(), t.id).await.unwrap_err(),
        TaskError::NotPending
    );
    assert_eq!(
        r.service
            .update(group(), t.id, Some("x"), None, None)
            .await
            .unwrap_err(),
        TaskError::NotPending
    );
    assert_eq!(
        r.service.get(group(), TimerId::new(999)).await.unwrap_err(),
        TaskError::NotFound
    );
    assert!(
        r.service.list(group(), 0, 10).await.unwrap().is_empty(),
        "cancelled tasks are not active"
    );
}

#[tokio::test(start_paused = true)]
async fn listing_is_soonest_first_and_paged() {
    let r = rig(vec![], 4);
    for m in [30, 10, 20] {
        r.service
            .create(
                group(),
                &format!("in {m}"),
                When::After(mins(m)),
                None,
                Origin::Model,
            )
            .await
            .unwrap();
    }
    let all: Vec<_> = r
        .service
        .list(group(), 0, 10)
        .await
        .unwrap()
        .iter()
        .map(|t| t.wake().unwrap().intent.clone())
        .collect();
    assert_eq!(all, ["in 10", "in 20", "in 30"]);
    let page = r.service.list(group(), 1, 1).await.unwrap();
    assert_eq!(page[0].wake().unwrap().intent, "in 20");
}

#[tokio::test]
async fn the_in_memory_store_satisfies_the_timer_store_contract() {
    qbot_sched::conformance::run(&qbot_sched::MemoryTimerStore::new()).await;
}

#[tokio::test(start_paused = true)]
async fn repeating_tasks_are_checked_and_start_where_asked() {
    let r = rig(vec![], 4);
    let hour = std::time::Duration::from_secs(3600);
    let start = qbot_core::UnixMillis::new(START);
    let create = |rule: Rule, when: Option<When>| {
        let service = r.service.clone();
        async move {
            service
                .create_repeating(group(), "repeat", rule, when, None, Origin::Model)
                .await
        }
    };
    // The shortest interval is an hour: every occurrence is a model run in the group.
    assert_eq!(
        create(Rule::interval(mins(59)).unwrap(), None)
            .await
            .unwrap_err(),
        TaskError::IntervalTooShort { min: hour }
    );
    let interval = create(Rule::interval(hour).unwrap(), None).await.unwrap();
    assert_eq!(interval.due_at, start.plus(hour), "one interval from now");
    let at_start = create(Rule::interval(hour).unwrap(), Some(When::After(mins(7))))
        .await
        .unwrap();
    assert_eq!(
        at_start.due_at,
        start.plus(mins(7)),
        "an interval starts exactly there"
    );
    // START is 08:00 UTC. A daily time already past today, or too soon, starts tomorrow.
    let day = 24 * hour;
    let morning = Rule::Daily(TimeOfDay::new(8, 3).unwrap());
    assert_eq!(
        create(morning, None).await.unwrap().due_at,
        start.plus(day + mins(3))
    );
    let evening = Rule::Daily(TimeOfDay::new(20, 0).unwrap());
    assert_eq!(
        create(evening, None).await.unwrap().due_at,
        start.plus(12 * hour)
    );
    // A start is the earliest the first occurrence may be, judged like a one-shot time.
    assert_eq!(
        create(evening, Some(When::After(13 * hour)))
            .await
            .unwrap()
            .due_at,
        start.plus(day + 12 * hour)
    );
    assert!(matches!(
        create(evening, Some(When::After(mins(1)))).await,
        Err(TaskError::TooSoon { .. })
    ));
    // 2027-01-15 is a Friday; the next Monday or Wednesday is the 18th.
    let weekly = Rule::Weekly(
        Weekdays::from_codes(["mon", "wed"]).unwrap(),
        TimeOfDay::new(8, 0).unwrap(),
    );
    let w = create(weekly, None).await.unwrap();
    assert_eq!(w.due_at, start.plus(3 * day));
    let spec = w.wake().unwrap();
    assert_eq!(
        (
            spec.repeat.as_ref().unwrap().rule,
            spec.repeat.as_ref().unwrap().zone.name()
        ),
        (weekly, "UTC"),
        "the rule keeps the zone it was made in"
    );
    assert_eq!(
        create(evening, None)
            .await
            .map(|t| t.wake().unwrap().chain.depth),
        Ok(0)
    );
}

#[tokio::test(start_paused = true)]
async fn a_repeating_task_is_changed_and_cancelled_in_its_own_group_only() {
    let r = rig(vec![], 4);
    let hour = std::time::Duration::from_secs(3600);
    let start = qbot_core::UnixMillis::new(START);
    let t = r
        .service
        .create_repeating(
            group(),
            "status",
            Rule::interval(hour).unwrap(),
            None,
            None,
            Origin::Owner,
        )
        .await
        .unwrap();
    for wrong in [
        r.service.get(other_group(), t.id).await.unwrap_err(),
        r.service
            .update(other_group(), t.id, Some("x"), None, None)
            .await
            .unwrap_err(),
        r.service.cancel(other_group(), t.id).await.unwrap_err(),
    ] {
        assert_eq!(wrong, TaskError::NotFound);
    }
    assert!(
        r.service
            .list(other_group(), 0, 10)
            .await
            .unwrap()
            .is_empty()
    );

    // A new time moves the next occurrence; an interval then counts from it.
    let moved = r
        .service
        .update(group(), t.id, None, Some(When::After(mins(90))), None)
        .await
        .unwrap();
    assert_eq!(moved.due_at, start.plus(mins(90)));
    assert!(moved.wake().unwrap().repeat.is_some(), "still repeating");
    // A new rule replaces the old one and sets the next occurrence by it.
    let daily = Rule::Daily(TimeOfDay::new(20, 0).unwrap());
    let ruled = r
        .service
        .update(group(), t.id, None, None, Some(daily))
        .await
        .unwrap();
    assert_eq!(
        (
            ruled.wake().unwrap().repeat.as_ref().unwrap().rule,
            ruled.due_at
        ),
        (daily, start.plus(12 * hour))
    );
    assert_eq!(
        r.service
            .update(
                group(),
                t.id,
                None,
                None,
                Some(Rule::interval(mins(30)).unwrap())
            )
            .await
            .unwrap_err(),
        TaskError::IntervalTooShort { min: hour }
    );
    // A one-shot task can be made to repeat.
    let once = r
        .service
        .create(group(), "once", When::After(mins(10)), None, Origin::Model)
        .await
        .unwrap();
    let made = r
        .service
        .update(group(), once.id, None, None, Some(daily))
        .await
        .unwrap();
    assert_eq!(made.id, once.id);
    assert!(made.wake().unwrap().repeat.is_some());

    assert_eq!(
        r.service.cancel(group(), t.id).await.unwrap().state,
        TimerState::Cancelled
    );
    assert_eq!(
        r.service.cancel(group(), t.id).await.unwrap_err(),
        TaskError::NotPending
    );
}

#[tokio::test(start_paused = true)]
async fn repeating_tasks_count_against_the_pending_bound() {
    let r = rig(vec![], 4);
    let limit = r.service.limits().max_pending_per_group;
    let hour = std::time::Duration::from_secs(3600);
    for _ in 0..limit {
        r.service
            .create_repeating(
                group(),
                "r",
                Rule::interval(hour).unwrap(),
                None,
                None,
                Origin::Model,
            )
            .await
            .unwrap();
    }
    assert_eq!(
        r.service
            .create_repeating(
                group(),
                "r",
                Rule::interval(hour).unwrap(),
                None,
                None,
                Origin::Model
            )
            .await
            .unwrap_err(),
        TaskError::TooManyPending { limit }
    );
}
