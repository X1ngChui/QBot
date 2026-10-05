//! The behavior every `TimerStore` must have. The in-memory store defines it; durable stores
//! run the same assertions, so they cannot drift.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::time::Duration;

use qbot_agent::Chain;
use qbot_context::RunEnd;
use qbot_core::{ChainId, GroupId, TimerId, UnixMillis};

use crate::store::{StoreError, TimerStore};
use crate::timer::{JobKind, NewWake, Origin, TimerKind, TimerOutcome, TimerState, WakeEdit};

fn at(seconds: i64) -> UnixMillis {
    UnixMillis::new(1_800_000_000_000 + seconds * 1000)
}

fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn wake(group: GroupId, intent: &str) -> NewWake {
    NewWake {
        group,
        intent: intent.into(),
        chain: None,
        origin: Origin::Model,
    }
}

pub async fn run(store: &dyn TimerStore) {
    inserting(store).await;
    claiming_wakes(store).await;
    editing_and_cancelling(store).await;
    finishing(store).await;
    jobs(store).await;
    next_due_and_recovery(store).await;
    leases_and_recurrences(store).await;
}

/// Finish everything still pending, so each step starts from an empty store.
async fn drain(store: &dyn TimerStore) {
    while let Some(t) = store
        .claim_due_wake(at(100_000_000), &HashSet::new())
        .await
        .unwrap()
    {
        store
            .finish(t.id, TimerOutcome::SkippedMuted)
            .await
            .unwrap();
    }
    while let Some(t) = store
        .claim_due_job(at(100_000_000), Duration::from_secs(1))
        .await
        .unwrap()
    {
        store.finish(t.id, TimerOutcome::JobOk).await.unwrap();
    }
}

async fn inserting(store: &dyn TimerStore) {
    let a = group(1001);
    let root = store
        .insert_wake(at(600), wake(a, "root"), 3)
        .await
        .unwrap();
    let spec = root.wake().unwrap();
    assert_eq!(
        (spec.chain.id.get(), spec.chain.depth),
        (root.id.get(), 0),
        "a root chain starts at itself"
    );
    assert_eq!(
        (spec.intent.as_str(), spec.origin, spec.group),
        ("root", Origin::Model, a)
    );
    assert_eq!(root.state, TimerState::Pending);
    assert_eq!((root.due_at, root.attempts), (at(600), 0));
    assert_eq!(store.get(root.id).await.unwrap().unwrap(), root);
    assert!(store.get(TimerId::new(9_999_999)).await.unwrap().is_none());

    let child = NewWake {
        chain: Some(Chain {
            id: ChainId::new(root.id.get()),
            depth: 4,
        }),
        origin: Origin::Owner,
        ..wake(a, "child")
    };
    let child = store.insert_wake(at(700), child, 3).await.unwrap();
    let chain = child.wake().unwrap().chain;
    assert_eq!(
        (chain.id.get(), chain.depth, child.wake().unwrap().origin),
        (root.id.get(), 4, Origin::Owner)
    );

    // The pending bound is atomic with the insert and per group.
    store
        .insert_wake(at(800), wake(a, "third"), 3)
        .await
        .unwrap();
    assert_eq!(
        store
            .insert_wake(at(900), wake(a, "fourth"), 3)
            .await
            .unwrap_err(),
        StoreError::TooManyPending { limit: 3 }
    );
    store
        .insert_wake(at(900), wake(group(1002), "other group"), 3)
        .await
        .unwrap();

    // Only pending tasks count against it.
    store.cancel_pending_wake(a, root.id).await.unwrap();
    store
        .insert_wake(at(900), wake(a, "fits again"), 3)
        .await
        .unwrap();
}

async fn claiming_wakes(store: &dyn TimerStore) {
    drain(store).await;
    let (a, b) = (group(2001), group(2002));
    let late = store
        .insert_wake(at(300), wake(a, "late"), 50)
        .await
        .unwrap();
    let early = store
        .insert_wake(at(100), wake(a, "early"), 50)
        .await
        .unwrap();
    let other = store
        .insert_wake(at(200), wake(b, "other"), 50)
        .await
        .unwrap();
    let future = store
        .insert_wake(at(10_000), wake(a, "future"), 50)
        .await
        .unwrap();

    let none = HashSet::new();
    assert!(
        store.claim_due_wake(at(50), &none).await.unwrap().is_none(),
        "nothing is due yet"
    );

    // Oldest due first; a claim is single-attempt.
    let first = store
        .claim_due_wake(at(1000), &none)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.id, early.id);
    assert_eq!(
        (first.state.clone(), first.attempts),
        (TimerState::Claimed { lease_until: None }, 1)
    );

    // Groups in `exclude` are skipped, not blocked.
    let skip_b: HashSet<_> = [b].into_iter().collect();
    let second = store
        .claim_due_wake(at(1000), &skip_b)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.id, late.id);
    let third = store
        .claim_due_wake(at(1000), &none)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(third.id, other.id);
    assert!(
        store
            .claim_due_wake(at(1000), &none)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.get(future.id).await.unwrap().unwrap().state,
        TimerState::Pending
    );

    // Listing: soonest first, running tasks included, paged.
    let listed = store.list_active_wakes(a, 0, 10).await.unwrap();
    let ids: Vec<_> = listed.iter().map(|t| t.id).collect();
    assert_eq!(ids, [early.id, late.id, future.id]);
    assert_eq!(
        store.list_active_wakes(a, 1, 1).await.unwrap()[0].id,
        late.id
    );
    assert!(
        store
            .list_active_wakes(group(2999), 0, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn editing_and_cancelling(store: &dyn TimerStore) {
    drain(store).await;
    let (a, b) = (group(3001), group(3002));
    let t = store
        .insert_wake(at(500), wake(a, "water"), 50)
        .await
        .unwrap();

    let edit = WakeEdit {
        intent: Some("water the ferns".into()),
        due_at: Some(at(900)),
    };
    assert_eq!(
        store
            .edit_pending_wake(b, t.id, edit.clone())
            .await
            .unwrap_err(),
        StoreError::NotFound,
        "scoped to the group"
    );
    let edited = store.edit_pending_wake(a, t.id, edit).await.unwrap();
    assert_eq!(
        (edited.wake().unwrap().intent.as_str(), edited.due_at),
        ("water the ferns", at(900))
    );
    let only_time = store
        .edit_pending_wake(
            a,
            t.id,
            WakeEdit {
                intent: None,
                due_at: Some(at(950)),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        only_time.wake().unwrap().intent,
        "water the ferns",
        "unset fields are kept"
    );

    assert_eq!(
        store.cancel_pending_wake(b, t.id).await.unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(
        store
            .cancel_pending_wake(a, TimerId::new(9_999_999))
            .await
            .unwrap_err(),
        StoreError::NotFound
    );
    let cancelled = store.cancel_pending_wake(a, t.id).await.unwrap();
    assert_eq!(cancelled.state, TimerState::Cancelled);
    assert!(matches!(
        store.cancel_pending_wake(a, t.id).await.unwrap_err(),
        StoreError::NotPending(TimerState::Cancelled)
    ));
    assert!(matches!(
        store
            .edit_pending_wake(a, t.id, WakeEdit::default())
            .await
            .unwrap_err(),
        StoreError::NotPending(_)
    ));

    // A claimed task can no longer be changed.
    let running = store
        .insert_wake(at(1), wake(a, "running"), 50)
        .await
        .unwrap();
    store
        .claim_due_wake(at(10), &HashSet::new())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        store.cancel_pending_wake(a, running.id).await.unwrap_err(),
        StoreError::NotPending(TimerState::Claimed { .. })
    ));
}

async fn finishing(store: &dyn TimerStore) {
    drain(store).await;
    let a = group(4001);
    let outcomes = [
        TimerOutcome::Ran(RunEnd::Completed),
        TimerOutcome::Ran(RunEnd::Delivered),
        TimerOutcome::Ran(RunEnd::StepLimit),
        TimerOutcome::Ran(RunEnd::Deadline),
        TimerOutcome::Ran(RunEnd::ModelError),
        TimerOutcome::Ran(RunEnd::Cancelled),
        TimerOutcome::Ran(RunEnd::Environment),
        TimerOutcome::Ran(RunEnd::Interrupted),
        TimerOutcome::SkippedMuted,
        TimerOutcome::JobOk,
        TimerOutcome::JobFailed("disk full: /var".into()),
    ];
    for outcome in outcomes {
        let t = store.insert_wake(at(0), wake(a, "f"), 50).await.unwrap();
        assert!(
            matches!(
                store.finish(t.id, outcome.clone()).await.unwrap_err(),
                StoreError::NotPending(TimerState::Pending)
            ),
            "only a claimed timer can finish"
        );
        let claimed = store
            .claim_due_wake(at(5), &HashSet::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, t.id);
        store.finish(t.id, outcome.clone()).await.unwrap();
        assert_eq!(
            store.get(t.id).await.unwrap().unwrap().state,
            TimerState::Done(outcome)
        );
        assert!(
            matches!(
                store.finish(t.id, TimerOutcome::JobOk).await.unwrap_err(),
                StoreError::NotPending(TimerState::Done(_))
            ),
            "finishing is final"
        );
    }
    assert_eq!(
        store
            .finish(TimerId::new(9_999_999), TimerOutcome::JobOk)
            .await
            .unwrap_err(),
        StoreError::NotFound
    );
}

async fn jobs(store: &dyn TimerStore) {
    drain(store).await;
    let lease = Duration::from_secs(60);
    let job = store
        .insert_job(at(100), JobKind::Decay, Some(group(5001)))
        .await
        .unwrap();
    assert_eq!(
        job.kind,
        TimerKind::Job {
            kind: JobKind::Decay,
            group: Some(group(5001))
        }
    );
    let global = store
        .insert_job(at(110), JobKind::Backup, None)
        .await
        .unwrap();
    assert_eq!(
        global.kind,
        TimerKind::Job {
            kind: JobKind::Backup,
            group: None
        }
    );

    assert!(
        store.claim_due_job(at(99), lease).await.unwrap().is_none(),
        "not due"
    );
    let claimed = store.claim_due_job(at(100), lease).await.unwrap().unwrap();
    assert_eq!(claimed.id, job.id);
    assert_eq!(
        claimed.state,
        TimerState::Claimed {
            lease_until: Some(at(160))
        }
    );
    assert_eq!(claimed.attempts, 1);
    // Wakes are never handed to the job worker.
    let w = store
        .insert_wake(at(0), wake(group(5002), "w"), 50)
        .await
        .unwrap();
    let next = store.claim_due_job(at(120), lease).await.unwrap().unwrap();
    assert_eq!(
        next.id, global.id,
        "the held job is skipped while its lease lasts"
    );
    assert!(store.claim_due_job(at(121), lease).await.unwrap().is_none());
    assert_eq!(
        store.get(w.id).await.unwrap().unwrap().state,
        TimerState::Pending
    );

    // An expired lease lets another worker take the job over.
    let taken = store.claim_due_job(at(161), lease).await.unwrap().unwrap();
    assert_eq!((taken.id, taken.attempts), (job.id, 2));
    assert_eq!(
        taken.state,
        TimerState::Claimed {
            lease_until: Some(at(221))
        }
    );

    store.finish(global.id, TimerOutcome::JobOk).await.unwrap();

    // Retrying returns a claimed job to pending, due later.
    store.retry_job(job.id, at(500)).await.unwrap();
    let pending = store.get(job.id).await.unwrap().unwrap();
    assert_eq!(
        (pending.state, pending.due_at, pending.attempts),
        (TimerState::Pending, at(500), 2)
    );
    assert!(store.claim_due_job(at(499), lease).await.unwrap().is_none());
    assert_eq!(
        store
            .claim_due_job(at(500), lease)
            .await
            .unwrap()
            .unwrap()
            .attempts,
        3
    );
    assert!(
        matches!(
            store.retry_job(w.id, at(1)).await.unwrap_err(),
            StoreError::NotPending(_)
        ),
        "wakes are not retried"
    );
    store.finish(job.id, TimerOutcome::JobOk).await.unwrap();
}

async fn next_due_and_recovery(store: &dyn TimerStore) {
    drain(store).await;
    // Earlier steps left some timers claimed; recovery clears them, so counts below are fresh.
    store.recover().await.unwrap();
    drain(store).await;
    assert_eq!(store.next_due().await.unwrap(), None);

    let g = group(6001);
    let soon = store
        .insert_wake(at(2_000_100), wake(g, "soon"), 50)
        .await
        .unwrap();
    store
        .insert_wake(at(2_000_900), wake(g, "later"), 50)
        .await
        .unwrap();
    assert_eq!(store.next_due().await.unwrap(), Some(at(2_000_100)));

    let job = store
        .insert_job(at(2_000_000), JobKind::Report, None)
        .await
        .unwrap();
    let claimed = store
        .claim_due_job(at(2_000_000), Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, job.id);
    assert_eq!(
        store.next_due().await.unwrap(),
        Some(at(2_000_030)),
        "a held job's lease expiry needs attention"
    );

    // Recovery: an at-most-once claim is interrupted, an idempotent one requeued.
    let running = store
        .claim_due_wake(at(2_000_200), &HashSet::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.id, soon.id);
    let report = store.recover().await.unwrap();
    assert_eq!((report.interrupted, report.requeued), (1, 1));
    assert_eq!(
        store.get(soon.id).await.unwrap().unwrap().state,
        TimerState::Interrupted
    );
    assert_eq!(
        store.get(job.id).await.unwrap().unwrap().state,
        TimerState::Pending
    );
    assert_eq!(
        store.recover().await.unwrap(),
        Default::default(),
        "recovery is idempotent"
    );
}

async fn leases_and_recurrences(store: &dyn TimerStore) {
    drain(store).await;

    // A claimed job's lease can be extended, and only while it is claimed.
    let job = store
        .insert_job(at(0), JobKind::Backup, None)
        .await
        .unwrap();
    assert!(
        matches!(
            store.extend_lease(job.id, at(10)).await,
            Err(StoreError::NotPending(_))
        ),
        "pending, not claimed"
    );
    let claimed = store
        .claim_due_job(at(0), Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, job.id);
    store.extend_lease(job.id, at(100)).await.unwrap();
    assert!(
        store
            .claim_due_job(at(50), Duration::from_secs(5))
            .await
            .unwrap()
            .is_none(),
        "the extended lease keeps it from being taken over"
    );
    let taken = store
        .claim_due_job(at(101), Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (taken.id, taken.attempts),
        (job.id, 2),
        "once the extended lease lapses it is taken over"
    );
    store.finish(job.id, TimerOutcome::JobOk).await.unwrap();
    assert!(matches!(
        store.extend_lease(job.id, at(200)).await,
        Err(StoreError::NotPending(_))
    ));
    assert_eq!(
        store.extend_lease(TimerId::new(9_999_999), at(1)).await,
        Err(StoreError::NotFound)
    );

    // A recurrence fires each occurrence once, in order, and never for the past of a new schedule.
    assert!(
        store
            .fire_recurrence("nightly", at(1000), JobKind::Nightly)
            .await
            .unwrap()
    );
    assert!(
        !store
            .fire_recurrence("nightly", at(1000), JobKind::Nightly)
            .await
            .unwrap(),
        "the same occurrence twice"
    );
    assert!(
        !store
            .fire_recurrence("nightly", at(900), JobKind::Nightly)
            .await
            .unwrap(),
        "an earlier one after a later one"
    );
    assert!(
        store
            .fire_recurrence("nightly", at(2000), JobKind::Nightly)
            .await
            .unwrap()
    );
    assert!(
        store
            .fire_recurrence("report", at(1000), JobKind::Report)
            .await
            .unwrap(),
        "schedules are independent"
    );
    store.seed_recurrence("fresh", at(5000)).await.unwrap();
    assert!(
        !store
            .fire_recurrence("fresh", at(5000), JobKind::Cleanup)
            .await
            .unwrap(),
        "a seeded occurrence is considered done"
    );
    store.seed_recurrence("fresh", at(1)).await.unwrap();
    assert!(
        !store
            .fire_recurrence("fresh", at(4000), JobKind::Cleanup)
            .await
            .unwrap(),
        "seeding never moves a schedule back"
    );
    assert!(
        store
            .fire_recurrence("fresh", at(6000), JobKind::Cleanup)
            .await
            .unwrap()
    );

    let mut fired = Vec::new();
    while let Some(t) = store
        .claim_due_job(at(100_000), Duration::from_secs(1))
        .await
        .unwrap()
    {
        let TimerKind::Job { kind, .. } = t.kind else {
            panic!()
        };
        fired.push((t.due_at, kind));
        store.finish(t.id, TimerOutcome::JobOk).await.unwrap();
    }
    assert_eq!(
        fired,
        [
            (at(1000), JobKind::Nightly),
            (at(1000), JobKind::Report),
            (at(2000), JobKind::Nightly),
            (at(6000), JobKind::Cleanup)
        ]
    );
}
