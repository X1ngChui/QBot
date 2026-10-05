#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use qbot_store::{LeaseError, RuntimeLease};
use tokio_util::sync::CancellationToken;

const KEY: i64 = 0x5142_4F54;

#[tokio::test]
async fn only_one_instance_holds_the_lease_and_release_frees_it() {
    let db = db!();
    let mut first = RuntimeLease::acquire(&db.url, KEY).await.unwrap();
    assert!(first.is_held().await);
    assert!(matches!(
        RuntimeLease::acquire(&db.url, KEY).await,
        Err(LeaseError::Held)
    ));
    assert!(
        RuntimeLease::acquire(&db.url, KEY + 1).await.is_ok(),
        "other keys are independent"
    );

    let watch = first.watch(Duration::from_millis(50), CancellationToken::new());
    assert!(
        matches!(
            RuntimeLease::acquire(&db.url, KEY).await,
            Err(LeaseError::Held)
        ),
        "held while watched"
    );
    watch.release().await;
    // The server frees the session lock when the connection closes; allow it a moment.
    let mut reacquired = false;
    for _ in 0..50 {
        if RuntimeLease::acquire(&db.url, KEY).await.is_ok() {
            reacquired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(reacquired, "the lease is free after release");
    db.drop_db().await;
}

#[tokio::test]
async fn losing_the_connection_is_reported_so_the_process_can_shut_down() {
    let db = db!();
    let lease = RuntimeLease::acquire(&db.url, KEY).await.unwrap();
    let lost = CancellationToken::new();
    let watch = lease.watch(Duration::from_millis(50), lost.clone());

    // Kill the lease's backend from the server side, as a network drop or failover would.
    let killed: bool = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype = 'advisory' AND objid = $1::oid \
         AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) LIMIT 1",
    )
    .bind(KEY)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(killed);
    tokio::time::timeout(Duration::from_secs(5), lost.cancelled())
        .await
        .expect("loss is noticed");
    watch.release().await;
    db.drop_db().await;
}
