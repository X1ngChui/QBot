//! The behavior every `NameStore` must have. Accounts 1..=40 must exist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::episode::EpisodeId;
use crate::memories::{Durability, Support};
use crate::names::{
    Grounds, HeldClaim, KnownName, NameBasis, NameError, NameStore, RULE_AGAIN, Reference,
    Referent, Ruling, Verdict,
};

fn acct(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn at(seconds: i64) -> UnixMillis {
    UnixMillis::new(1_800_000_000_000 + seconds * 1000)
}

/// Whoever says what the member does not say themselves.
fn someone() -> AccountId {
    acct(40)
}

fn used(account: AccountId, text: &str, basis: NameBasis, episode: i64) -> Reference {
    let own = basis == NameBasis::Own;
    Reference {
        episode: EpisodeId::new(episode),
        message: MessageId::new(episode * 10).unwrap(),
        ordinal: u64::try_from(episode * 10).unwrap(),
        speaker: if own { account } else { someone() },
        text: text.to_owned(),
        referent: Referent::Member(account),
        basis,
        grounds: if own { Grounds::Own } else { Grounds::Content },
        anchored: false,
        shown_by: Vec::new(),
        at: at(episode),
    }
}

/// A use nobody could place, on its own line of the episode.
fn unplaced(text: &str, episode: i64, line: i64) -> Reference {
    Reference {
        message: MessageId::new(episode * 10 + line).unwrap(),
        ordinal: u64::try_from(episode * 10 + line).unwrap(),
        referent: Referent::Unknown,
        basis: NameBasis::About,
        grounds: Grounds::None,
        ..used(someone(), text, NameBasis::About, episode)
    }
}

fn ruling(text: &str, verdict: Verdict, seen: u32) -> Ruling {
    Ruling {
        text: text.to_owned(),
        verdict,
        lines: vec![MessageId::new(11).unwrap()],
        reason: "they answer to it".into(),
        seen,
        at: at(50),
    }
}

fn find<'a>(names: &'a [KnownName], account: AccountId, key: &str) -> Option<&'a KnownName> {
    names.iter().find(|n| n.account == account && n.key == key)
}

pub async fn run(store: &dyn NameStore) {
    uses_add_up_per_member_and_name(store).await;
    a_shared_name_is_visible_as_shared(store).await;
    setting_and_removing(store).await;
    groups_are_separate(store).await;
    a_name_nobody_could_place_waits_for_a_ruling(store).await;
    a_ruling_places_what_was_unplaced(store).await;
    statements_are_held_until_filed(store).await;
}

async fn uses_add_up_per_member_and_name(store: &dyn NameStore) {
    let g = group(300);
    let (p, q) = (acct(1), acct(2));
    let uses = [
        used(p, "Ming", NameBasis::Own, 1),
        used(p, "ming", NameBasis::Addressed, 2),
        used(p, "  MING ", NameBasis::About, 3),
        used(q, "Q", NameBasis::Addressed, 3),
    ];
    assert_eq!(store.record(g, &uses).await.unwrap(), 4);
    assert_eq!(
        store.record(g, &uses).await.unwrap(),
        0,
        "one reading of an expression per episode and line"
    );
    let names = store.names(g, None).await.unwrap();
    let ming = find(&names, p, "ming").expect("one entry for every spelling");
    assert_eq!((ming.own, ming.addressed, ming.about), (1, 1, 1));
    assert_eq!((ming.anchored, ming.grounded), (0, 0));
    assert!(
        ming.vouched() && ming.corroborated(),
        "the member used it themselves, and three conversations agree"
    );
    // The member answered to "Q" in one of two conversations.
    let answered = Reference {
        anchored: true,
        ..used(q, "q", NameBasis::About, 4)
    };
    store.record(g, &[answered]).await.unwrap();
    let names = store.names(g, None).await.unwrap();
    let called = find(&names, q, "q").unwrap();
    assert_eq!((called.uses(), called.anchored, called.grounded), (2, 1, 1));
    assert!(called.vouched() && !find(&names, p, "ming").unwrap().set);
    assert_eq!(ming.uses(), 3);
    assert!(!ming.set && !ming.ruled);
    assert_eq!(ming.last, at(3));
    assert_eq!(
        ming.text, "MING",
        "as most recently written, whitespace collapsed"
    );
    assert_eq!(store.names(g, Some(&[q])).await.unwrap().len(), 1);

    // Several uses in one episode count once: the strongest, with what any of them rests on.
    let r = acct(10);
    let boss = [
        used(r, "Boss", NameBasis::About, 5),
        Reference {
            message: MessageId::new(51).unwrap(),
            grounds: Grounds::Answer,
            ..used(r, "boss", NameBasis::Addressed, 5)
        },
        used(r, "boss", NameBasis::About, 6),
    ];
    assert_eq!(store.record(g, &boss).await.unwrap(), 3);
    let names = store.names(g, Some(&[r])).await.unwrap();
    let boss = find(&names, r, "boss").unwrap();
    assert_eq!((boss.addressed, boss.about, boss.grounded), (1, 1, 1));
    assert!(
        !boss.vouched() && boss.corroborated(),
        "two conversations agree, one of them by an answer"
    );
    // What rests only on what is believed about the member does not confirm itself.
    let s = acct(11);
    store
        .record(
            g,
            &[
                used(s, "Doc", NameBasis::About, 7),
                used(s, "Doc", NameBasis::About, 8),
            ],
        )
        .await
        .unwrap();
    let names = store.names(g, Some(&[s])).await.unwrap();
    let doc = find(&names, s, "doc").unwrap();
    assert!(doc.uses() == 2 && !doc.corroborated() && !doc.vouched());
}

async fn a_shared_name_is_visible_as_shared(store: &dyn NameStore) {
    let g = group(301);
    let (a, b) = (acct(3), acct(4));
    let mut uses: Vec<Reference> = (1..=8)
        .map(|e| used(a, "Boss", NameBasis::Addressed, e))
        .collect();
    uses.push(used(b, "boss", NameBasis::About, 9));
    store.record(g, &uses).await.unwrap();
    let names = store.names(g, None).await.unwrap();
    let both: Vec<&KnownName> = names.iter().filter(|n| n.key == "boss").collect();
    assert_eq!(
        both.len(),
        2,
        "one entry per member: whom the name means is read from both"
    );
    assert_eq!(find(&names, a, "boss").unwrap().uses(), 8);
    assert_eq!(find(&names, b, "boss").unwrap().uses(), 1);
    // Every use can be read back: one per episode, the latest episodes first.
    let latest = store.uses(g, "boss", 3).await.unwrap();
    assert_eq!(
        latest.iter().map(|r| r.episode.get()).collect::<Vec<_>>(),
        vec![9, 8, 7]
    );
    assert_eq!(latest[0].referent, Referent::Member(b));
}

async fn setting_and_removing(store: &dyn NameStore) {
    let g = group(302);
    let (p, q, r) = (acct(5), acct(6), acct(7));
    store
        .set(g, "Captain", &[p, q], at(1))
        .await
        .expect("a name for one person's accounts");
    assert_eq!(
        store.set(g, "captain", &[r], at(2)).await.unwrap_err(),
        NameError::Taken,
        "a set name is not set for someone else"
    );
    assert!(matches!(
        store.set(g, "   ", &[r], at(2)).await.unwrap_err(),
        NameError::Text(_)
    ));
    assert!(matches!(
        store
            .set(g, &"x".repeat(65), &[r], at(2))
            .await
            .unwrap_err(),
        NameError::Text(_)
    ));
    let names = store.names(g, Some(&[p])).await.unwrap();
    assert!(find(&names, p, "captain").unwrap().set);

    // Learned uses of a set name add to it.
    store
        .record(g, &[used(p, "captain", NameBasis::Addressed, 4)])
        .await
        .unwrap();
    let names = store.names(g, Some(&[p])).await.unwrap();
    let captain = find(&names, p, "captain").unwrap();
    assert!(captain.set);
    assert_eq!(captain.addressed, 1);

    // Removed: its uses go, and chat using it again is not recorded.
    assert!(store.remove(g, "CAPTAIN", &[p], at(5)).await.unwrap());
    assert!(!store.remove(g, "captain", &[p], at(5)).await.unwrap());
    assert!(store.names(g, Some(&[p])).await.unwrap().is_empty());
    assert_eq!(
        store
            .record(g, &[used(p, "Captain", NameBasis::Own, 6)])
            .await
            .unwrap(),
        0
    );
    assert!(store.names(g, Some(&[p])).await.unwrap().is_empty());
    assert!(
        find(&store.names(g, Some(&[q])).await.unwrap(), q, "captain").is_some(),
        "the other account keeps it"
    );
    // A person setting it again brings it back.
    store.set(g, "Captain", &[p, q], at(7)).await.unwrap();
    store
        .record(g, &[used(p, "captain", NameBasis::Own, 8)])
        .await
        .unwrap();
    let names = store.names(g, Some(&[p])).await.unwrap();
    assert_eq!(find(&names, p, "captain").unwrap().own, 1);
}

async fn groups_are_separate(store: &dyn NameStore) {
    let (g, h) = (group(303), group(304));
    let p = acct(8);
    store.set(g, "Solo", &[p], at(1)).await.unwrap();
    store
        .record(g, &[used(p, "Lone", NameBasis::Own, 2)])
        .await
        .unwrap();
    assert!(store.names(h, None).await.unwrap().is_empty());
    assert!(store.set(h, "Solo", &[acct(9)], at(3)).await.is_ok());
    store.remove(h, "Lone", &[p], at(4)).await.unwrap();
    assert!(
        find(&store.names(g, Some(&[p])).await.unwrap(), p, "lone").is_some(),
        "a removal in another group changes nothing here"
    );
}

async fn a_name_nobody_could_place_waits_for_a_ruling(store: &dyn NameStore) {
    let g = group(305);
    // Three uses in one conversation offer nothing to compare; two in each of two do.
    store
        .record(
            g,
            &[
                unplaced("Pip", 1, 1),
                unplaced("pip", 1, 2),
                unplaced("PIP", 1, 3),
                unplaced("Rex", 1, 4),
                unplaced("Rex", 2, 1),
                unplaced("Old Ray", 1, 5),
                unplaced("old ray", 2, 2),
                unplaced("Old Ray", 3, 1),
                unplaced("old  ray", 3, 2),
            ],
        )
        .await
        .unwrap();
    assert!(
        store.names(g, None).await.unwrap().is_empty(),
        "an unplaced use is nobody's name"
    );
    let open = store.open(g, 10).await.unwrap();
    assert_eq!(
        open.len(),
        1,
        "enough uses, from more than one conversation"
    );
    assert_eq!(
        (open[0].key.as_str(), open[0].unknown, open[0].episodes),
        ("old ray", 4, 3)
    );
    assert_eq!(open[0].text, "old ray", "as most recently written");
    store.record(g, &[unplaced("pip", 2, 3)]).await.unwrap();
    let open = store.open(g, 10).await.unwrap();
    assert_eq!(
        open.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(),
        vec!["old ray", "pip"],
        "the most used first"
    );
    assert_eq!(store.open(g, 1).await.unwrap().len(), 1);

    // A ruling that could not tell leaves it waiting for more uses.
    store
        .rule(g, &ruling("Old Ray", Verdict::Unknown, 4))
        .await
        .unwrap();
    let keys = |open: Vec<crate::names::OpenName>| -> Vec<String> {
        open.into_iter().map(|o| o.key).collect()
    };
    assert_eq!(keys(store.open(g, 10).await.unwrap()), vec!["pip"]);
    let more: Vec<Reference> = (0..i64::from(RULE_AGAIN))
        .map(|n| unplaced("old ray", 4, n + 1))
        .collect();
    store.record(g, &more).await.unwrap();
    assert_eq!(
        keys(store.open(g, 10).await.unwrap()),
        vec!["old ray", "pip"]
    );
    // An ordinary word is nobody's name and is not asked about again; a name a person set
    // needs no ruling.
    store
        .rule(g, &ruling("old ray", Verdict::General, 9))
        .await
        .unwrap();
    store.set(g, "Pip", &[acct(12)], at(9)).await.unwrap();
    assert!(store.open(g, 10).await.unwrap().is_empty());
}

async fn a_ruling_places_what_was_unplaced(store: &dyn NameStore) {
    let g = group(306);
    let (p, q) = (acct(13), acct(14));
    store
        .record(
            g,
            &[
                unplaced("Bean", 1, 1),
                unplaced("bean", 2, 1),
                unplaced("Bean", 3, 1),
                used(q, "Bean", NameBasis::About, 3),
            ],
        )
        .await
        .unwrap();
    store
        .rule(g, &ruling("Bean", Verdict::Member(p), 3))
        .await
        .unwrap();
    let names = store.names(g, None).await.unwrap();
    let bean = find(&names, p, "bean").unwrap();
    assert!(bean.ruled && bean.vouched());
    assert_eq!(
        (bean.about, bean.grounded),
        (3, 0),
        "its unplaced uses are theirs now"
    );
    assert!(
        !find(&names, q, "bean").unwrap().ruled,
        "another member's own uses stay theirs"
    );
    assert!(store.open(g, 10).await.unwrap().is_empty());
    // A later use nobody could place is theirs too.
    assert_eq!(store.record(g, &[unplaced("bean", 4, 1)]).await.unwrap(), 1);
    let placed = store.uses(g, "bean", 1).await.unwrap();
    assert_eq!(
        (placed[0].referent, placed[0].grounds),
        (Referent::Member(p), Grounds::Ruling)
    );
    assert_eq!(
        find(&store.names(g, Some(&[p])).await.unwrap(), p, "bean")
            .unwrap()
            .about,
        4
    );
    // A person removing the name undoes the ruling, and no later ruling gives it back.
    assert!(store.remove(g, "Bean", &[p], at(60)).await.unwrap());
    assert!(store.names(g, Some(&[p])).await.unwrap().is_empty());
    store
        .rule(g, &ruling("Bean", Verdict::Member(p), 0))
        .await
        .unwrap();
    assert!(store.names(g, Some(&[p])).await.unwrap().is_empty());

    // An outsider's name is no member's, whatever single conversations read it as.
    let r = acct(15);
    store
        .record(
            g,
            &[
                used(r, "Zed", NameBasis::About, 5),
                unplaced("Zed", 5, 2),
                unplaced("Zed", 6, 1),
            ],
        )
        .await
        .unwrap();
    assert!(find(&store.names(g, None).await.unwrap(), r, "zed").is_some());
    store
        .rule(g, &ruling("Zed", Verdict::Outside, 2))
        .await
        .unwrap();
    assert!(find(&store.names(g, None).await.unwrap(), r, "zed").is_none());
    let zed = store.uses(g, "zed", 1).await.unwrap();
    assert_eq!(zed[0].referent, Referent::Outside);
    store.set(g, "Zed", &[r], at(70)).await.unwrap();
    assert!(
        find(&store.names(g, None).await.unwrap(), r, "zed")
            .unwrap()
            .set,
        "a person's word outranks a ruling"
    );
}

async fn statements_are_held_until_filed(store: &dyn NameStore) {
    let g = group(307);
    let claim = |text: &str, episode: i64| HeldClaim {
        name: " Old  Ray ".into(),
        text: text.to_owned(),
        durability: Durability::Lasting,
        support: Support {
            episode: EpisodeId::new(episode),
            message: MessageId::new(episode * 10).unwrap(),
            author: Some(someone()),
            quote: "Old Ray keeps bees".into(),
            at: at(episode),
        },
    };
    let claims = [claim("keeps bees", 1), claim("lives by the river", 2)];
    store.hold(g, &claims).await.unwrap();
    store.hold(g, &claims).await.unwrap();
    let held = store.held(g).await.unwrap();
    assert_eq!(held.len(), 2, "one per episode, line and statement");
    assert_eq!(held[0].1.name, "Old Ray");
    assert_eq!(
        (held[0].1.text.as_str(), held[0].1.durability),
        ("keeps bees", Durability::Lasting)
    );
    assert_eq!(held[0].1.support, claims[0].support);
    assert!(store.held(group(308)).await.unwrap().is_empty());
    store.file(g, held[0].0, acct(16)).await.unwrap();
    let held = store.held(g).await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].1.text, "lives by the river");
    // Filed is filed: holding the same statement again does not bring it back.
    store.hold(g, &claims).await.unwrap();
    assert_eq!(store.held(g).await.unwrap().len(), 1);
}
