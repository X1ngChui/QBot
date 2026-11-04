//! The behavior every `IdentityStore` must have. The store must be built with
//! `IdentityPolicy::default()` (invitations last 600 s).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::identity::{IdentityError, InvitationState, LinkError, LinkOutcome};
use crate::identity_store::IdentityStore;

fn acct(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn msg(n: i64) -> MessageId {
    MessageId::new(n).unwrap()
}

fn at(seconds: i64) -> UnixMillis {
    UnixMillis::new(1_800_000_000_000 + seconds * 1000)
}

pub async fn run(store: &dyn IdentityStore) {
    links_belong_to_their_group(store).await;
    unlinking(store).await;
    invitations(store).await;
}

async fn links_belong_to_their_group(store: &dyn IdentityStore) {
    let (a, b, c, d) = (acct(1), acct(2), acct(3), acct(4));
    let (g, h) = (group(100), group(101));
    for x in [a, b, c, d] {
        store.seen(x, at(0)).await.unwrap();
    }
    store.seen(a, at(99)).await.unwrap();
    assert!(store.is_known(a).await.unwrap());
    assert!(!store.is_known(acct(999)).await.unwrap());
    assert_eq!(
        store.linked(g, a).await.unwrap(),
        vec![a],
        "alone by default"
    );
    assert_eq!(
        store.link(g, a, acct(999), at(1)).await.unwrap_err(),
        IdentityError::UnknownAccount
    );
    assert_eq!(
        store.link(g, a, a, at(1)).await.unwrap_err(),
        IdentityError::Link(LinkError::SelfLink)
    );

    assert_eq!(
        store.link(g, b, a, at(1)).await.unwrap(),
        LinkOutcome::Linked
    );
    assert_eq!(store.linked(g, a).await.unwrap(), vec![a, b]);
    assert_eq!(store.linked(g, b).await.unwrap(), vec![a, b]);
    assert_eq!(
        store.link(g, a, b, at(2)).await.unwrap(),
        LinkOutcome::AlreadyLinked
    );
    assert_eq!(
        store.linked(h, a).await.unwrap(),
        vec![a],
        "a link made in one group is not seen in another"
    );

    // Links chain within a group, and only within it.
    store.link(h, b, c, at(3)).await.unwrap();
    assert_eq!(store.linked(g, a).await.unwrap(), vec![a, b]);
    assert_eq!(store.linked(h, a).await.unwrap(), vec![a]);
    assert_eq!(store.linked(h, c).await.unwrap(), vec![b, c]);
    store.link(g, d, b, at(4)).await.unwrap();
    assert_eq!(store.linked(g, d).await.unwrap(), vec![a, b, d]);
    assert_eq!(
        store.link(g, a, d, at(5)).await.unwrap(),
        LinkOutcome::AlreadyLinked,
        "linked through another account"
    );
}

async fn unlinking(store: &dyn IdentityStore) {
    let (x, y, z) = (acct(11), acct(12), acct(13));
    let (g, h) = (group(110), group(111));
    for a in [x, y, z] {
        store.seen(a, at(0)).await.unwrap();
    }
    assert_eq!(
        store.unlink(g, x).await.unwrap_err(),
        IdentityError::NotLinked,
        "a lone account has nothing to unlink"
    );
    assert_eq!(
        store.unlink(g, acct(998)).await.unwrap_err(),
        IdentityError::UnknownAccount
    );
    store.link(g, x, y, at(1)).await.unwrap();
    store.link(g, y, z, at(1)).await.unwrap();
    store.link(h, x, y, at(1)).await.unwrap();
    assert_eq!(
        store.unlink(h, z).await.unwrap_err(),
        IdentityError::NotLinked,
        "z is linked only in the other group"
    );

    // Unlinking the middle account leaves the others with only their own links.
    store.unlink(g, y).await.unwrap();
    for a in [x, y, z] {
        assert_eq!(store.linked(g, a).await.unwrap(), vec![a]);
    }
    assert_eq!(
        store.linked(h, x).await.unwrap(),
        vec![x, y],
        "the other group's link stays"
    );
}

async fn invitations(store: &dyn IdentityStore) {
    let g = group(500);
    let (a, b, c, d) = (acct(51), acct(52), acct(53), acct(54));
    for (i, x) in [a, b, c, d].into_iter().enumerate() {
        store.seen(x, at(i as i64)).await.unwrap();
    }

    assert_eq!(
        store.invite(g, a, a, msg(1), at(100)).await.unwrap_err(),
        IdentityError::Link(LinkError::SelfLink)
    );
    let inv = store.invite(g, a, b, msg(10), at(100)).await.unwrap();
    assert_eq!(
        (inv.initiator, inv.target, inv.state),
        (a, b, InvitationState::Pending)
    );
    assert_eq!(
        (inv.initiator_linked.clone(), inv.target_linked.clone()),
        (vec![a], vec![b])
    );
    // One pending invitation per account per group, on either side.
    for (x, y) in [(a, c), (c, a), (b, c), (c, b), (b, a)] {
        assert_eq!(
            store.invite(g, x, y, msg(11), at(101)).await.unwrap_err(),
            IdentityError::Link(LinkError::Busy),
            "{x:?}->{y:?}"
        );
    }
    assert!(
        store
            .invite(group(501), a, c, msg(11), at(101))
            .await
            .is_ok(),
        "other groups are independent"
    );

    // Rules checked at confirmation.
    assert_eq!(
        store.confirm(g, a, msg(20), at(110)).await.unwrap_err(),
        IdentityError::Link(LinkError::NotTheTarget),
        "only the invited account confirms"
    );
    assert_eq!(
        store.confirm(g, d, msg(20), at(110)).await.unwrap_err(),
        IdentityError::Link(LinkError::NoInvitation)
    );
    // Message ids are hashes, not a sequence: a confirmation whose id is lower than the
    // invitation's is as good as any.
    let applied = store.confirm(g, b, msg(9), at(110)).await.unwrap();
    assert_eq!(applied, LinkOutcome::Linked);
    assert_eq!(store.linked(g, a).await.unwrap(), vec![a, b]);
    assert_eq!(
        store.linked(group(501), a).await.unwrap(),
        vec![a],
        "linked in the group it was confirmed in only"
    );
    assert_eq!(
        store.confirm(g, b, msg(9), at(111)).await.unwrap(),
        LinkOutcome::AlreadyLinked,
        "replaying the confirming message is a no-op"
    );
    assert_eq!(
        store.confirm(g, b, msg(21), at(112)).await.unwrap_err(),
        IdentityError::Link(LinkError::NoInvitation),
        "a different message finds nothing pending"
    );
    assert_eq!(
        store.invite(g, a, b, msg(30), at(200)).await.unwrap_err(),
        IdentityError::Link(LinkError::AlreadyLinked)
    );
    store.cancel_invitation(group(501), a).await.unwrap();
    assert!(
        store
            .invite(group(501), b, a, msg(31), at(200))
            .await
            .is_ok(),
        "a link elsewhere does not make them linked here"
    );
    store.cancel_invitation(group(501), a).await.unwrap();

    // Cancel frees both sides.
    store.invite(g, c, d, msg(40), at(300)).await.unwrap();
    assert!(store.cancel_invitation(g, d).await.unwrap());
    assert!(!store.cancel_invitation(g, d).await.unwrap());
    assert!(
        store.invite(g, d, c, msg(41), at(301)).await.is_ok(),
        "free again after a cancel"
    );
    store.cancel_invitation(g, c).await.unwrap();

    // Expiry: ten minutes is the end.
    let (e, f) = (acct(55), acct(56));
    store.seen(e, at(0)).await.unwrap();
    store.seen(f, at(0)).await.unwrap();
    store.invite(g, e, f, msg(50), at(1000)).await.unwrap();
    assert_eq!(
        store
            .confirm(g, f, msg(60), at(1000 + 600))
            .await
            .unwrap_err(),
        IdentityError::Link(LinkError::Expired)
    );
    assert_eq!(
        store.linked(g, e).await.unwrap(),
        vec![e],
        "an expired invitation links nothing"
    );
    assert!(
        store.invite(g, e, f, msg(70), at(1700)).await.is_ok(),
        "an expired invitation no longer blocks"
    );
    store.cancel_invitation(g, e).await.unwrap();

    // Staleness: if either side's links in the group changed since the invitation, it no longer
    // applies. Links in other groups are no part of it.
    let (h, i, j) = (acct(57), acct(58), acct(59));
    for x in [h, i, j] {
        store.seen(x, at(0)).await.unwrap();
    }
    store.invite(g, h, i, msg(80), at(2000)).await.unwrap();
    store.link(group(502), i, j, at(2001)).await.unwrap();
    store.cancel_invitation(g, h).await.unwrap();
    store.invite(g, h, i, msg(81), at(2002)).await.unwrap();
    store.link(group(502), h, j, at(2003)).await.unwrap();
    assert_eq!(
        store.confirm(g, i, msg(82), at(2004)).await.unwrap(),
        LinkOutcome::Linked,
        "links elsewhere do not make it stale"
    );
    store.unlink(g, i).await.unwrap();
    store.invite(g, h, i, msg(83), at(2005)).await.unwrap();
    store.link(g, i, j, at(2006)).await.unwrap();
    assert_eq!(
        store.confirm(g, i, msg(90), at(2010)).await.unwrap_err(),
        IdentityError::Link(LinkError::Stale)
    );
    assert_eq!(store.linked(g, h).await.unwrap(), vec![h]);
    assert!(
        store.invite(g, h, i, msg(91), at(2020)).await.is_ok(),
        "a stale invitation is closed and can be redone"
    );
}
