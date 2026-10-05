//! The behavior every `IdentityStore` must have. The store must be built with
//! `IdentityPolicy::default()` (confirm at 0.75, invitations last 600 s).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::identity::{
    AliasStatus, AliasTarget, EvidenceKind, EvidenceRecord, IdentityError, InvitationState,
    LinkError, MergeOutcome, Resolution,
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
    holders_and_merging(store).await;
    splitting(store).await;
    names(store).await;
    holder_names_follow_merges(store).await;
    expiry(store).await;
    invitations(store).await;
}

async fn holders_and_merging(store: &dyn IdentityStore) {
    let (a, b, c) = (acct(1), acct(2), acct(3));
    let ha = store.seen(a, at(0)).await.unwrap();
    let hb = store.seen(b, at(10)).await.unwrap();
    assert_ne!(ha.id, hb.id, "each new account is its own person");
    assert_eq!((ha.revision, ha.accounts.clone()), (0, vec![a]));
    assert_eq!(
        store.seen(a, at(99)).await.unwrap(),
        ha,
        "seeing an account again changes nothing"
    );
    assert!(store.holder_of(acct(999)).await.unwrap().is_none());
    assert_eq!(
        store.merge(a, acct(999)).await.unwrap_err(),
        IdentityError::UnknownAccount
    );

    // The older holder wins, both revisions change, and every account follows the winner.
    let outcome = store.merge(b, a).await.unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Merged {
            winner: ha.id,
            loser: hb.id
        }
    );
    let merged = store.holder_of(b).await.unwrap().unwrap();
    assert_eq!((merged.id, merged.revision), (ha.id, 1));
    assert_eq!(merged.accounts, vec![a, b]);
    assert_eq!(store.holder_of(a).await.unwrap().unwrap(), merged);
    assert_eq!(
        store.merge(a, b).await.unwrap(),
        MergeOutcome::AlreadyLinked(ha.id)
    );
    assert_eq!(
        store.holder_of(a).await.unwrap().unwrap().revision,
        1,
        "a no-op does not bump the revision"
    );

    // A third account joins the existing person.
    store.seen(c, at(20)).await.unwrap();
    let outcome = store.merge(c, b).await.unwrap();
    assert!(matches!(outcome, MergeOutcome::Merged { winner, .. } if winner == ha.id));
    assert_eq!(
        store.holder_of(c).await.unwrap().unwrap().accounts,
        vec![a, b, c]
    );
}

async fn splitting(store: &dyn IdentityStore) {
    let (x, y, z) = (acct(11), acct(12), acct(13));
    store.seen(x, at(0)).await.unwrap();
    assert_eq!(
        store.split(x, at(5)).await.unwrap_err(),
        IdentityError::NotLinked,
        "a lone account has nothing to split from"
    );
    assert_eq!(
        store.split(acct(998), at(5)).await.unwrap_err(),
        IdentityError::UnknownAccount
    );

    store.seen(y, at(1)).await.unwrap();
    store.seen(z, at(2)).await.unwrap();
    store.merge(x, y).await.unwrap();
    store.merge(x, z).await.unwrap();
    let before = store.holder_of(x).await.unwrap().unwrap();

    // A name attached to the person (not to one login) stays with the original holder.
    let g = group(100);
    store
        .set_name(g, "Xavier", AliasTarget::Holder(before.id), at(3))
        .await
        .unwrap();
    let fresh = store.split(z, at(6)).await.unwrap();
    assert_eq!(fresh.accounts, vec![z]);
    assert_ne!(fresh.id, before.id);
    assert_eq!(
        fresh.revision, 1,
        "the new holder starts a revision history of its own"
    );
    let after = store.holder_of(x).await.unwrap().unwrap();
    assert_eq!(
        (after.id, after.revision, after.accounts.clone()),
        (before.id, before.revision + 1, vec![x, y])
    );
    assert_eq!(
        store.resolve(g, "xavier").await.unwrap(),
        Resolution::One(AliasTarget::Holder(before.id)),
        "holder-scoped records stay behind"
    );
    assert!(
        store
            .names_of(g, AliasTarget::Holder(fresh.id))
            .await
            .unwrap()
            .is_empty()
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
        .add_evidence(
            g,
            "  Ｊｏｈｎ   Smith ",
            AliasTarget::Account(p),
            by_hand(),
            at(1),
        )
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
        Resolution::One(AliasTarget::Account(p))
    );
    assert_eq!(
        store.resolve(other, "john smith").await.unwrap(),
        Resolution::Unknown,
        "names are per group"
    );
    assert!(matches!(
        store
            .add_evidence(g, "   ", AliasTarget::Account(p), by_hand(), at(1))
            .await,
        Err(IdentityError::Name(_))
    ));
    assert!(matches!(
        store
            .add_evidence(
                g,
                &"x".repeat(65),
                AliasTarget::Account(p),
                by_hand(),
                at(1)
            )
            .await,
        Err(IdentityError::Name(_))
    ));
    assert!(
        store
            .add_evidence(
                g,
                &"x".repeat(64),
                AliasTarget::Account(p),
                by_hand(),
                at(1)
            )
            .await
            .is_ok()
    );
    assert_eq!(
        store
            .add_evidence(
                g,
                "ghost",
                AliasTarget::Account(acct(997)),
                by_hand(),
                at(1)
            )
            .await
            .unwrap_err(),
        IdentityError::UnknownAccount
    );

    // Extracted evidence accumulates per distinct episode, and alone never confirms.
    let one = store
        .add_evidence(g, "pj", AliasTarget::Account(q), extracted(1), at(2))
        .await
        .unwrap();
    assert_eq!(one.status, AliasStatus::Candidate);
    assert!((one.confidence - 0.25).abs() < 1e-6);
    let again = store
        .add_evidence(g, "pj", AliasTarget::Account(q), extracted(1), at(3))
        .await
        .unwrap();
    assert!(
        (again.confidence - 0.25).abs() < 1e-6,
        "the same episode twice counts once"
    );
    let two = store
        .add_evidence(g, "pj", AliasTarget::Account(q), extracted(2), at(4))
        .await
        .unwrap();
    assert!((two.confidence - 0.4375).abs() < 1e-5);
    for episode in 3..=12 {
        store
            .add_evidence(g, "pj", AliasTarget::Account(q), extracted(episode), at(5))
            .await
            .unwrap();
    }
    let capped = store
        .add_evidence(g, "pj", AliasTarget::Account(q), extracted(13), at(6))
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
        .add_evidence(g, "pj", AliasTarget::Account(q), by_hand(), at(7))
        .await
        .unwrap();
    assert_eq!(both.status, AliasStatus::Confirmed);
    assert!((both.confidence - 1.0).abs() < 1e-5);

    // A person's word is decisive, and a name refused for taken only when someone else holds it.
    assert_eq!(
        store
            .set_name(g, "john smith", AliasTarget::Account(q), at(8))
            .await
            .unwrap_err(),
        IdentityError::NameTaken
    );
    assert!(
        store
            .set_name(g, "john smith", AliasTarget::Account(p), at(8))
            .await
            .is_ok(),
        "the holder may vouch for their own name"
    );
    let manual = store
        .set_name(g, "Johnny", AliasTarget::Account(p), at(9))
        .await
        .unwrap();
    assert_eq!(
        (manual.status, manual.confidence),
        (AliasStatus::Confirmed, 1.0)
    );

    // Two confirmed targets for one name is ambiguity, never a guess.
    store
        .add_evidence(g, "sam", AliasTarget::Account(p), by_hand(), at(10))
        .await
        .unwrap();
    store
        .add_evidence(g, "sam", AliasTarget::Account(q), by_hand(), at(10))
        .await
        .unwrap();
    assert_eq!(
        store.resolve(g, "sam").await.unwrap(),
        Resolution::Ambiguous(vec![AliasTarget::Account(p), AliasTarget::Account(q)])
    );

    // Names are listed strongest first and exclude removed ones.
    let listed = store.names_of(g, AliasTarget::Account(p)).await.unwrap();
    let names: Vec<&str> = listed.iter().map(|a| a.text.as_str()).collect();
    // Both vouched names are at 1.0 and lead, ties broken alphabetically; weaker ones follow.
    assert_eq!(&names[..2], ["john smith", "johnny"], "{names:?}");
    assert!(
        listed
            .windows(2)
            .all(|w| w[0].confidence >= w[1].confidence)
    );
    assert!(
        store
            .remove_name(g, "Johnny", AliasTarget::Account(p))
            .await
            .unwrap()
    );
    assert!(
        !store
            .remove_name(g, "Johnny", AliasTarget::Account(p))
            .await
            .unwrap(),
        "already removed"
    );
    assert_eq!(
        store.resolve(g, "johnny").await.unwrap(),
        Resolution::Unknown
    );
    assert!(
        !store
            .names_of(g, AliasTarget::Account(p))
            .await
            .unwrap()
            .iter()
            .any(|a| a.text == "johnny")
    );
    // New evidence brings a removed name back.
    let back = store
        .add_evidence(g, "johnny", AliasTarget::Account(p), by_hand(), at(11))
        .await
        .unwrap();
    assert_eq!(back.status, AliasStatus::Confirmed);
}

async fn holder_names_follow_merges(store: &dyn IdentityStore) {
    let g = group(300);
    let (m, n) = (acct(31), acct(32));
    let hm = store.seen(m, at(0)).await.unwrap();
    let hn = store.seen(n, at(5)).await.unwrap();
    // The name is given to the younger holder, which then loses the merge.
    store
        .set_name(g, "robin", AliasTarget::Holder(hn.id), at(6))
        .await
        .unwrap();
    assert_eq!(
        store.resolve(g, "robin").await.unwrap(),
        Resolution::One(AliasTarget::Holder(hn.id))
    );
    store.merge(m, n).await.unwrap();
    assert_eq!(
        store.resolve(g, "robin").await.unwrap(),
        Resolution::One(AliasTarget::Holder(hm.id)),
        "a name given to a holder follows the person to the merged holder"
    );
    assert_eq!(
        store
            .names_of(g, AliasTarget::Holder(hm.id))
            .await
            .unwrap()
            .len(),
        1
    );
}

async fn expiry(store: &dyn IdentityStore) {
    let g = group(400);
    let r = acct(41);
    store.seen(r, at(0)).await.unwrap();
    store
        .add_evidence(
            g,
            "stale guess",
            AliasTarget::Account(r),
            extracted(1),
            at(100),
        )
        .await
        .unwrap();
    store
        .add_evidence(
            g,
            "fresh guess",
            AliasTarget::Account(r),
            extracted(2),
            at(5000),
        )
        .await
        .unwrap();
    store
        .add_evidence(g, "vouched", AliasTarget::Account(r), extracted(3), at(100))
        .await
        .unwrap();
    store
        .set_name(g, "vouched", AliasTarget::Account(r), at(100))
        .await
        .unwrap();
    store
        .add_evidence(
            g,
            "added by hand",
            AliasTarget::Account(r),
            by_hand(),
            at(100),
        )
        .await
        .unwrap();

    assert_eq!(
        store.expire_candidates(at(1000)).await.unwrap(),
        1,
        "only the old, unvouched candidate"
    );
    let live: Vec<String> = store
        .names_of(g, AliasTarget::Account(r))
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
    assert_eq!((inv.initiator_revision, inv.target_revision), (0, 0));
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
        store.confirm(g, b, msg(9), at(110)).await.unwrap_err(),
        IdentityError::Link(LinkError::OutOfOrder),
        "must come after the invitation"
    );
    assert_eq!(
        store.confirm(g, d, msg(20), at(110)).await.unwrap_err(),
        IdentityError::Link(LinkError::NoInvitation)
    );
    let applied = store.confirm(g, b, msg(20), at(110)).await.unwrap();
    assert!(matches!(applied, MergeOutcome::Merged { .. }));
    assert_eq!(
        store.holder_of(a).await.unwrap().unwrap().accounts,
        vec![a, b]
    );
    assert_eq!(
        store.confirm(g, b, msg(20), at(111)).await.unwrap(),
        MergeOutcome::AlreadyLinked(store.holder_of(a).await.unwrap().unwrap().id),
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
        store.holder_of(e).await.unwrap().unwrap().accounts,
        vec![e],
        "an expired invitation merges nothing"
    );
    assert!(
        store.invite(g, e, f, msg(70), at(1700)).await.is_ok(),
        "an expired invitation no longer blocks"
    );
    store.cancel_invitation(g, e).await.unwrap();

    // Staleness: if either side's holder changed since the invitation, it no longer applies.
    let (h, i, j) = (acct(57), acct(58), acct(59));
    for x in [h, i, j] {
        store.seen(x, at(0)).await.unwrap();
    }
    store.invite(g, h, i, msg(80), at(2000)).await.unwrap();
    store.merge(i, j).await.unwrap(); // i's holder changes (revision) outside the invitation
    assert_eq!(
        store.confirm(g, i, msg(90), at(2010)).await.unwrap_err(),
        IdentityError::Link(LinkError::Stale)
    );
    assert_eq!(store.holder_of(h).await.unwrap().unwrap().accounts, vec![h]);
    assert!(
        store.invite(g, h, i, msg(91), at(2020)).await.is_ok(),
        "a stale invitation is closed and can be redone"
    );
}
