#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use qbot_context::RunEnd;
use qbot_core::{GroupId, UnixMillis};
use qbot_sched::{
    LastRun, NewWake, Origin, Repeat, Rule, StoreError, TimerOutcome, TimerState, TimerStore, Zone,
};
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
        repeat: None,
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

fn hourly(group: i64, intent: &str) -> NewWake {
    NewWake {
        repeat: Some(Repeat {
            rule: Rule::interval(Duration::from_secs(3600)).unwrap(),
            zone: Zone::get("UTC").unwrap(),
        }),
        ..wake(group, intent)
    }
}

#[tokio::test]
async fn racing_workers_claim_and_finish_each_occurrence_of_a_repeating_task_once() {
    let db = db!();
    let store = Arc::new(PgTimerStore::new(db.pool().clone(), ManualClock::new(T0)));
    let task = store
        .insert_wake(UnixMillis::new(T0), hourly(9, "hourly"), 50)
        .await
        .unwrap();
    let hour = 3_600_000;
    for n in 0..3 {
        let occurrence = UnixMillis::new(T0 + n * hour);
        let now = UnixMillis::new(T0 + n * hour + 1);
        // Eight workers race for the due occurrence: one gets it.
        let claims: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move { store.claim_due_wake(now, &HashSet::new()).await })
            })
            .collect();
        let mut claimed = 0;
        for claim in claims {
            if claim.await.unwrap().unwrap().is_some() {
                claimed += 1;
            }
        }
        assert_eq!(claimed, 1, "occurrence {n} was claimed {claimed} times");
        // Eight attempts to record it: one moves the task on, the rest find nothing to do.
        let next = UnixMillis::new(T0 + (n + 1) * hour);
        let finishes: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move {
                    store
                        .finish_occurrence(
                            task.id,
                            occurrence,
                            TimerOutcome::Ran(RunEnd::Completed),
                            next,
                        )
                        .await
                })
            })
            .collect();
        let mut recorded = 0;
        for finish in finishes {
            match finish.await.unwrap() {
                Ok(()) => recorded += 1,
                Err(error) => assert!(matches!(error, StoreError::NotPending(_)), "{error:?}"),
            }
        }
        assert_eq!(recorded, 1);
        let timer = store.get(task.id).await.unwrap().unwrap();
        assert_eq!(
            (timer.state, timer.due_at, timer.attempts),
            (TimerState::Pending, next, u32::try_from(n + 1).unwrap())
        );
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM timer")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1, "one row for the whole series");
    db.drop_db().await;
}

#[tokio::test]
async fn a_repeating_task_survives_a_restart_without_replaying_its_occurrence() {
    let db = db!();
    let before = PgTimerStore::new(db.pool().clone(), ManualClock::new(T0));
    let task = before
        .insert_wake(UnixMillis::new(T0), hourly(9, "survives"), 50)
        .await
        .unwrap();
    before
        .claim_due_wake(UnixMillis::new(T0), &HashSet::new())
        .await
        .unwrap()
        .unwrap();
    drop(before);

    // A new process, two and a half hours later.
    let restart = UnixMillis::new(T0 + 9_000_000);
    let after = PgTimerStore::new(db.pool().clone(), ManualClock::new(restart.get()));
    let report = after.recover(restart).await.unwrap();
    assert_eq!((report.interrupted, report.rescheduled), (0, 1));
    let timer = after.get(task.id).await.unwrap().unwrap();
    assert_eq!(
        (timer.state.clone(), timer.due_at),
        (TimerState::Pending, UnixMillis::new(T0 + 3 * 3_600_000))
    );
    assert_eq!(
        timer.wake().unwrap().last,
        Some(LastRun {
            due_at: UnixMillis::new(T0),
            outcome: TimerOutcome::Ran(RunEnd::Interrupted)
        })
    );
    assert!(
        after
            .claim_due_wake(restart, &HashSet::new())
            .await
            .unwrap()
            .is_none(),
        "nothing is due until the next occurrence"
    );
    db.drop_db().await;
}
