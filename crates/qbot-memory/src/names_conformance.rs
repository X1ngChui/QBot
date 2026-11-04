//! The behavior every `NameStore` must have. Accounts 1..=40 must exist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::episode::EpisodeId;
use crate::names::{KnownName, NameBasis, NameError, NameStore, NameUse, UseSource};

fn acct(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn at(seconds: i64) -> UnixMillis {
    UnixMillis::new(1_800_000_000_000 + seconds * 1000)
}

fn used(account: AccountId, text: &str, basis: NameBasis, episode: i64) -> NameUse {
    NameUse {
        account,
        text: text.to_owned(),
        basis,
        source: Some(UseSource {
            episode: EpisodeId::new(episode),
            message: MessageId::new(episode * 10).unwrap(),
            quote: format!("... {text} ..."),
        }),
        at: at(episode),
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
        "the same episode counts once per member and name"
    );
    let names = store.names(g, None).await.unwrap();
    let ming = find(&names, p, "ming").expect("one entry for every spelling");
    assert_eq!((ming.own, ming.addressed, ming.about), (1, 1, 1));
    assert_eq!(ming.uses(), 3);
    assert!(!ming.set);
    assert_eq!(ming.last, at(3));
    assert_eq!(
        ming.text, "MING",
        "as most recently written, whitespace collapsed"
    );
    assert_eq!(store.names(g, Some(&[q])).await.unwrap().len(), 1);
}

async fn a_shared_name_is_visible_as_shared(store: &dyn NameStore) {
    let g = group(301);
    let (a, b) = (acct(3), acct(4));
    let mut uses: Vec<NameUse> = (1..=8)
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
