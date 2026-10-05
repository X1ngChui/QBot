#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::*;
use qbot_core::{GroupId, UnixMillis};
use qbot_sched::{NewWake, Origin, StoreError, TimerOutcome, TimerStore};
use qbot_store::PgTimerStore;

#[tokio::test]
async fn the_postgres_store_satisfies_the_same_contract_as_the_memory_store() {
    let db = db!();
    let store = PgTimerStore::new(db.pool().clone(), ManualClock::new(T0));
    qbot_sched::conformance::run(&store).await;
    db.drop_db().await;
}

fn wake(group: i64, intent: &str) -> NewWake {
    NewWake {
        group: GroupId::new(group).unwrap(),
        intent: intent.into(),
        chain: None,
        origin: Origin::Model,
    }
}

#[tokio::test]
async fn concurrent_claims_never_hand_one_timer_to_two_workers() {
    let db = db!();
    let store = Arc::new(PgTimerStore::new(db.pool().clone(), ManualClock::new(T0)));
    for i in 0..60 {
        store
            .insert_wake(UnixMillis::new(T0), wake(1 + i, "due"), 50)
            .await
            .unwrap();
    }
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move {
                let mut claimed = Vec::new();
                while let Some(timer) = store
                    .claim_due_wake(UnixMillis::new(T0 + 1), &HashSet::new())
                    .await
                    .unwrap()
                {
                    claimed.push(timer.id);
                }
                claimed
            })
        })
        .collect();
    let mut all = Vec::new();
    for worker in workers {
        all.extend(worker.await.unwrap());
    }
    let unique: HashSet<_> = all.iter().copied().collect();
    assert_eq!(
        (all.len(), unique.len()),
        (60, 60),
        "every timer claimed exactly once"
    );
    for id in all {
        store.finish(id, TimerOutcome::SkippedMuted).await.unwrap();
    }
    db.drop_db().await;
}

#[tokio::test]
async fn the_pending_bound_holds_under_concurrent_inserts() {
    let db = db!();
    let store = Arc::new(PgTimerStore::new(db.pool().clone(), ManualClock::new(T0)));
    let tasks: Vec<_> = (0..20)
        .map(|i| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .insert_wake(UnixMillis::new(T0 + 600_000), wake(7, &format!("t{i}")), 5)
                    .await
            })
        })
        .collect();
    let mut ok = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => ok += 1,
            Err(error) => assert_eq!(error, StoreError::TooManyPending { limit: 5 }),
        }
    }
    assert_eq!(ok, 5, "exactly the bound, however the inserts interleave");
    db.drop_db().await;
}
