#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use qbot_core::UnixMillis;
use qbot_memory::identity::{IdentityPolicy, Invitation, InvitationState, LinkError};
use qbot_memory::{IdentityStore, MemoryIdentityStore};

#[tokio::test]
async fn the_in_memory_store_satisfies_the_identity_contract() {
    qbot_memory::identity_conformance::run(&MemoryIdentityStore::new(IdentityPolicy::default()))
        .await;
}

#[tokio::test]
async fn the_invitation_lifetime_is_policy_too() {
    use qbot_core::{AccountId, GroupId, MessageId};
    let policy = IdentityPolicy {
        invitation_ttl: Duration::from_secs(30),
    };
    let store = MemoryIdentityStore::new(policy);
    let (a, b, g) = (
        AccountId::new(1).unwrap(),
        AccountId::new(2).unwrap(),
        GroupId::new(1).unwrap(),
    );
    let t = UnixMillis::new(1_800_000_000_000);
    store.seen(a, t).await.unwrap();
    store.seen(b, t).await.unwrap();
    store
        .invite(g, a, b, MessageId::new(1).unwrap(), t)
        .await
        .unwrap();
    let late = t.plus(Duration::from_secs(31));
    assert!(
        store
            .confirm(g, b, MessageId::new(2).unwrap(), late)
            .await
            .is_err(),
        "a 30 s lifetime has passed"
    );
}

#[test]
fn the_pure_confirmation_rules_do_not_need_a_store() {
    let t = UnixMillis::new(1_800_000_000_000);
    let ttl = Duration::from_secs(600);
    let inv = Invitation {
        group: qbot_core::GroupId::new(1).unwrap(),
        initiator: qbot_core::AccountId::new(1).unwrap(),
        target: qbot_core::AccountId::new(2).unwrap(),
        created_by: qbot_core::MessageId::new(10).unwrap(),
        created_at: t,
        initiator_linked: vec![qbot_core::AccountId::new(1).unwrap()],
        target_linked: vec![qbot_core::AccountId::new(2).unwrap()],
        state: InvitationState::Pending,
    };
    let accounts = |ids: &[i64]| -> Vec<qbot_core::AccountId> {
        ids.iter()
            .map(|id| qbot_core::AccountId::new(*id).unwrap())
            .collect()
    };
    let confirm = |who: i64, after: u64, initiator: &[i64], target: &[i64]| {
        inv.check_confirm(
            qbot_core::AccountId::new(who).unwrap(),
            t.plus(Duration::from_secs(after)),
            ttl,
            &accounts(initiator),
            &accounts(target),
        )
    };
    assert_eq!(confirm(2, 5, &[1], &[2]), Ok(()));
    assert_eq!(confirm(1, 5, &[1], &[2]), Err(LinkError::NotTheTarget));
    assert_eq!(confirm(2, 600, &[1], &[2]), Err(LinkError::Expired));
    assert_eq!(confirm(2, 5, &[1, 3], &[2]), Err(LinkError::Stale));
    assert_eq!(confirm(2, 5, &[1], &[2, 3]), Err(LinkError::Stale));
}
