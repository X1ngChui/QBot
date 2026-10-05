#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use qbot_agent::GroupPolicy;
use qbot_core::{AccountId, GroupId, UnixMillis};
use qbot_store::PgGroupPolicy;

fn account(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

#[tokio::test]
async fn mute_and_block_gate_admission_with_expiry() {
    let db = db!();
    let clock = ManualClock::new(T0);
    let policy = PgGroupPolicy::new(db.pool().clone(), clock.clone());
    let g = GroupId::new(1).unwrap();
    let other = GroupId::new(2).unwrap();

    assert!(
        !policy.is_muted(g).await.unwrap(),
        "an unknown group is not muted"
    );
    policy.set_muted(g, true).await.unwrap();
    assert!(policy.is_muted(g).await.unwrap());
    assert!(!policy.is_muted(other).await.unwrap(), "per group");
    policy.set_muted(g, false).await.unwrap();
    assert!(!policy.is_muted(g).await.unwrap());

    assert!(!policy.is_blocked(g, account(9)).await.unwrap());
    policy
        .block(g, account(9), Some(UnixMillis::new(T0 + 10_000)))
        .await
        .unwrap();
    assert!(policy.is_blocked(g, account(9)).await.unwrap());
    assert!(
        !policy.is_blocked(other, account(9)).await.unwrap(),
        "per group"
    );
    assert!(
        !policy.is_blocked(g, account(8)).await.unwrap(),
        "per account"
    );
    clock.advance(Duration::from_secs(11));
    assert!(
        !policy.is_blocked(g, account(9)).await.unwrap(),
        "the block expired"
    );

    policy.block(g, account(9), None).await.unwrap();
    clock.advance(Duration::from_secs(10_000_000));
    assert!(
        policy.is_blocked(g, account(9)).await.unwrap(),
        "no expiry means until removed"
    );
    assert!(policy.unblock(g, account(9)).await.unwrap());
    assert!(
        !policy.unblock(g, account(9)).await.unwrap(),
        "nothing left to remove"
    );
    assert!(!policy.is_blocked(g, account(9)).await.unwrap());
    db.drop_db().await;
}
