#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use qbot_agent::Chain;
use qbot_core::{ChainId, TimerId};
use qbot_sched::{Origin, TaskError, TimerState, When};

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
            .update(group(), t.id, None, None)
            .await
            .unwrap_err(),
        TaskError::NothingToChange
    );
    assert_eq!(
        r.service
            .update(group(), t.id, Some(""), None)
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
            .update(group(), t.id, Some("x"), None)
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
