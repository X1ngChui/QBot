//! The behavior every `IdentityStore` must have. The store must be built with
//! `IdentityPolicy::default()` (confirm at 0.75, invitations last 600 s).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::identity::{
    AliasStatus, EvidenceKind, EvidenceRecord, IdentityError, InvitationState, LinkError,
    LinkOutcome, Resolution,
};
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

fn by_hand() -> EvidenceRecord {
    EvidenceRecord {
        kind: EvidenceKind::Manual,
        support: None,
    }
}

fn extracted(episode: i64) -> EvidenceRecord {
    EvidenceRecord {
        kind: EvidenceKind::Extracted,
        support: Some(episode),
    }
}

pub async fn run(store: &dyn IdentityStore) {
    links_belong_to_their_group(store).await;
    unlinking(store).await;
    names(store).await;
    names_of_a_person(store).await;
    expiry(store).await;
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

async fn names(store: &dyn IdentityStore) {
    let g = group(200);
    let other = group(201);
    let (p, q) = (acct(21), acct(22));
    store.seen(p, at(0)).await.unwrap();
    store.seen(q, at(0)).await.unwrap();

    // Normalization: width, case and spacing do not make a different name.
    let alias = store
        .add_evidence(g, "  Ｊｏｈｎ   Smith ", p, by_hand(), at(1))
        .await
        .unwrap();
    assert_eq!(alias.text, "john smith");
    assert_eq!((alias.status, alias.group), (AliasStatus::Confirmed, g));
    assert!(
        (alias.confidence - 1.0).abs() < 1e-6,
        "a name set by hand confirms by itself"
    );
    assert_eq!(
        store.resolve(g, "JOHN SMITH").await.unwrap(),
        Resolution::One(p)
    );
    assert_eq!(
        store.resolve(other, "john smith").await.unwrap(),
        Resolution::Unknown,
        "names are per group"
    );
    assert!(matches!(
        store.add_evidence(g, "   ", p, by_hand(), at(1)).await,
        Err(IdentityError::Name(_))
    ));
    assert!(matches!(
        store
            .add_evidence(g, &"x".repeat(65), p, by_hand(), at(1))
            .await,
        Err(IdentityError::Name(_))
    ));
    assert!(
        store
            .add_evidence(g, &"x".repeat(64), p, by_hand(), at(1))
            .await
            .is_ok()
    );
    assert_eq!(
        store
            .add_evidence(g, "ghost", acct(997), by_hand(), at(1))
            .await
            .unwrap_err(),
        IdentityError::UnknownAccount
    );

    // Extracted evidence accumulates per distinct episode, and alone never confirms.
    let one = store
        .add_evidence(g, "pj", q, extracted(1), at(2))
        .await
        .unwrap();
    assert_eq!(one.status, AliasStatus::Candidate);
    assert!((one.confidence - 0.25).abs() < 1e-6);
    let again = store
        .add_evidence(g, "pj", q, extracted(1), at(3))
        .await
        .unwrap();
    assert!(
        (again.confidence - 0.25).abs() < 1e-6,
        "the same episode twice counts once"
    );
    let two = store
        .add_evidence(g, "pj", q, extracted(2), at(4))
        .await
        .unwrap();
    assert!((two.confidence - 0.4375).abs() < 1e-5);
    for episode in 3..=12 {
        store
            .add_evidence(g, "pj", q, extracted(episode), at(5))
            .await
            .unwrap();
    }
    let capped = store
        .add_evidence(g, "pj", q, extracted(13), at(6))
        .await
        .unwrap();
    assert_eq!(
        capped.status,
        AliasStatus::Candidate,
        "extraction alone cannot confirm a name"
    );
    assert!(
        (capped.confidence - 0.7).abs() < 1e-5,
        "capped below the confirmation line"
    );
    assert_eq!(
        store.resolve(g, "pj").await.unwrap(),
        Resolution::Unknown,
        "only confirmed names resolve"
    );

    // A person's word on top of extraction confirms it.
    let both = store
        .add_evidence(g, "pj", q, by_hand(), at(7))
        .await
        .unwrap();
    assert_eq!(both.status, AliasStatus::Confirmed);
    assert!((both.confidence - 1.0).abs() < 1e-5);

    // A person's word is decisive, and a name refused for taken only when someone else holds it.
    assert_eq!(
        store
            .set_name(g, "john smith", &[q], at(8))
            .await
            .unwrap_err(),
        IdentityError::NameTaken
    );
    assert!(
        store.set_name(g, "john smith", &[p], at(8)).await.is_ok(),
        "the account named may vouch for its own name"
    );
    let [manual] = store
        .set_name(g, "Johnny", &[p], at(9))
        .await
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(
        (manual.status, manual.confidence),
        (AliasStatus::Confirmed, 1.0)
    );

    // Two confirmed targets for one name is ambiguity, never a guess.
    store
        .add_evidence(g, "sam", p, by_hand(), at(10))
        .await
        .unwrap();
    store
        .add_evidence(g, "sam", q, by_hand(), at(10))
        .await
        .unwrap();
    assert_eq!(
        store.resolve(g, "sam").await.unwrap(),
        Resolution::Ambiguous(vec![p, q])
    );

    // Names are listed strongest first and exclude removed ones.
    let listed = store.names_of(g, &[p]).await.unwrap();
    let names: Vec<&str> = listed.iter().map(|a| a.text.as_str()).collect();
    // Both vouched names are at 1.0 and lead, ties broken alphabetically; weaker ones follow.
    assert_eq!(&names[..2], ["john smith", "johnny"], "{names:?}");
    assert!(
        listed
            .windows(2)
            .all(|w| w[0].confidence >= w[1].confidence)
    );
    assert!(store.remove_name(g, "Johnny", &[p]).await.unwrap());
    assert!(
        !store.remove_name(g, "Johnny", &[p]).await.unwrap(),
        "already removed"
    );
    assert_eq!(
        store.resolve(g, "johnny").await.unwrap(),
        Resolution::Unknown
    );
    assert!(
        !store
            .names_of(g, &[p])
            .await
            .unwrap()
            .iter()
            .any(|a| a.text == "johnny")
    );
    // New evidence brings a removed name back.
    let back = store
        .add_evidence(g, "johnny", p, by_hand(), at(11))
        .await
        .unwrap();
    assert_eq!(back.status, AliasStatus::Confirmed);
}

async fn names_of_a_person(store: &dyn IdentityStore) {
    let g = group(300);
    let (m, n, o) = (acct(31), acct(32), acct(33));
    for x in [m, n, o] {
        store.seen(x, at(0)).await.unwrap();
    }
    store.set_name(g, "robin", &[m], at(1)).await.unwrap();
    // A name given to one person's accounts is not taken from them by one of their own.
    let given = store.set_name(g, "Robin", &[m, n], at(2)).await.unwrap();
    assert_eq!(
        given.iter().map(|a| a.account).collect::<Vec<_>>(),
        vec![m, n]
    );
    assert_eq!(
        store.set_name(g, "robin", &[o], at(3)).await.unwrap_err(),
        IdentityError::NameTaken
    );
    store
        .add_evidence(g, "rob", n, extracted(1), at(6000))
        .await
        .unwrap();
    let names: Vec<(String, AliasStatus)> = store
        .names_of(g, &[m, n])
        .await
        .unwrap()
        .into_iter()
        .map(|a| (a.text, a.status))
        .collect();
    assert_eq!(
        names,
        [
            ("robin".to_owned(), AliasStatus::Confirmed),
            ("rob".to_owned(), AliasStatus::Candidate)
        ],
        "one entry per name, strongest first"
    );
    assert_eq!(
        store.resolve(g, "robin").await.unwrap(),
        Resolution::Ambiguous(vec![m, n])
    );
    assert!(store.remove_name(g, "robin", &[m, n]).await.unwrap());
    assert!(store.names_of(g, &[m]).await.unwrap().is_empty());
    assert!(store.set_name(g, "robin", &[o], at(5)).await.is_ok());
}

async fn expiry(store: &dyn IdentityStore) {
    let g = group(400);
    let r = acct(41);
    store.seen(r, at(0)).await.unwrap();
    store
        .add_evidence(g, "stale guess", r, extracted(1), at(100))
        .await
        .unwrap();
    store
        .add_evidence(g, "fresh guess", r, extracted(2), at(5000))
        .await
        .unwrap();
    store
        .add_evidence(g, "vouched", r, extracted(3), at(100))
        .await
        .unwrap();
    store.set_name(g, "vouched", &[r], at(100)).await.unwrap();
    store
        .add_evidence(g, "added by hand", r, by_hand(), at(100))
        .await
        .unwrap();

    assert_eq!(
        store.expire_candidates(at(1000)).await.unwrap(),
        1,
        "only the old, unvouched candidate"
    );
    let live: Vec<String> = store
        .names_of(g, &[r])
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.text)
        .collect();
    assert!(!live.contains(&"stale guess".to_owned()));
    for kept in ["fresh guess", "vouched", "added by hand"] {
        assert!(
            live.contains(&kept.to_owned()),
            "{kept} must survive: {live:?}"
        );
    }
    assert_eq!(
        store.expire_candidates(at(1000)).await.unwrap(),
        0,
        "idempotent"
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
