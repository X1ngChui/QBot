//! The behavior every `FactStore` must have; the in-memory store defines it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::episode::EpisodeId;
use crate::facts::{FactStatus, FactStore, Observation, normalize_key};
use crate::predicates::DecayClass;

fn at(seconds: i64) -> UnixMillis {
    UnixMillis::new(1_800_000_000_000 + seconds * 1000)
}

fn obs(
    group: GroupId,
    subject: Option<i64>,
    predicate: &str,
    key: &str,
    object: &str,
    episode: i64,
    when: i64,
) -> Observation {
    Observation {
        group,
        subject: subject.map(|s| AccountId::new(s).unwrap()),
        predicate: predicate.into(),
        key: key.into(),
        object: object.into(),
        label: None,
        opposite: None,
        decay: DecayClass::Default,
        episode: EpisodeId::new(episode),
        message: MessageId::new(episode * 10).unwrap(),
        quote: format!("quote {episode}"),
        at: at(when),
    }
}

pub async fn run(store: &dyn FactStore, accounts: &[i64]) {
    let g = GroupId::new(7001).unwrap();
    let other = GroupId::new(7002).unwrap();
    let (a, b) = (accounts[0], accounts[1]);
    let subject = |n: i64| Some(AccountId::new(n).unwrap());

    // A new value is recorded; the same episode again changes nothing; another episode supports
    // it (values compare normalized) and moves its confirmation forward.
    let first = store
        .observe(&obs(g, Some(a), "likes", "cats", "Cats", 1, 0))
        .await
        .unwrap();
    assert_eq!(
        store
            .observe(&obs(g, Some(a), "likes", "cats", "Cats", 1, 0))
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        store
            .observe(&obs(g, Some(a), "likes", "cats", "  CATS ", 2, 50))
            .await
            .unwrap(),
        first
    );
    let current = store.current(g, subject(a)).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(
        (
            current[0].supports,
            current[0].last_confirmed,
            current[0].first_seen
        ),
        (2, at(50), at(0))
    );
    assert_eq!(current[0].object, "Cats", "the first wording is kept");

    // A multi-valued predicate keeps values side by side.
    store
        .observe(&obs(g, Some(a), "likes", "dogs", "dogs", 3, 60))
        .await
        .unwrap();
    // A single-valued predicate: a new value supersedes the old one.
    let old_home = store
        .observe(&obs(g, Some(a), "lives_in", "", "Hangzhou", 4, 70))
        .await
        .unwrap();
    let new_home = store
        .observe(&obs(g, Some(a), "lives_in", "", "Shanghai", 5, 80))
        .await
        .unwrap();
    assert_ne!(old_home, new_home);
    let current = store.current(g, subject(a)).await.unwrap();
    let shown: Vec<(&str, &str)> = current
        .iter()
        .map(|f| (f.predicate.as_str(), f.object.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            ("likes", "dogs"),
            ("likes", "Cats"),
            ("lives_in", "Shanghai")
        ],
        "by predicate; within one, the most recently confirmed first"
    );
    // Confirming an older value again brings it to the front.
    store
        .observe(&obs(g, Some(a), "likes", "cats", "cats", 20, 65))
        .await
        .unwrap();
    let likes: Vec<String> = store
        .current(g, subject(a))
        .await
        .unwrap()
        .into_iter()
        .filter(|f| f.predicate == "likes")
        .map(|f| f.object)
        .collect();
    assert_eq!(likes, ["Cats", "dogs"]);

    // An opposite predicate retires its counterpart for the same key, and only that one.
    let mut dislike = obs(g, Some(a), "dislikes", "dogs", "dogs", 6, 90);
    dislike.opposite = Some("likes".into());
    store.observe(&dislike).await.unwrap();
    let current = store.current(g, subject(a)).await.unwrap();
    let shown: Vec<(&str, &str)> = current
        .iter()
        .map(|f| (f.predicate.as_str(), f.key.as_str()))
        .collect();
    assert_eq!(
        shown,
        [("dislikes", "dogs"), ("likes", "cats"), ("lives_in", "")]
    );

    // Subjects and groups are separate; group facts have no subject.
    store
        .observe(&obs(g, Some(b), "likes", "cats", "cats", 7, 0))
        .await
        .unwrap();
    let mut term = obs(
        g,
        None,
        "group_term",
        &normalize_key("GG"),
        "good game",
        8,
        0,
    );
    term.label = Some("GG".into());
    store.observe(&term).await.unwrap();
    store
        .observe(&obs(other, Some(a), "likes", "tea", "tea", 9, 0))
        .await
        .unwrap();
    assert_eq!(store.current(g, subject(b)).await.unwrap().len(), 1);
    let group_facts = store.current(g, None).await.unwrap();
    assert_eq!(group_facts.len(), 1);
    assert_eq!(
        (
            group_facts[0].label.as_deref(),
            group_facts[0].object.as_str()
        ),
        (Some("GG"), "good game")
    );
    assert_eq!(store.current(other, subject(a)).await.unwrap().len(), 1);

    // Forgetting is per group and only for active facts.
    let tea = store.current(other, subject(a)).await.unwrap()[0].id;
    assert!(
        !store.forget(g, tea).await.unwrap(),
        "not a fact of this group"
    );
    assert!(store.forget(other, tea).await.unwrap());
    assert!(!store.forget(other, tea).await.unwrap());
    assert!(store.current(other, subject(a)).await.unwrap().is_empty());

    // Expiry applies to active facts only; a later observation of the same value starts afresh.
    let active = store.all_active().await.unwrap();
    assert!(active.iter().all(|f| f.status == FactStatus::Active));
    let cats = store
        .current(g, subject(a))
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.key == "cats")
        .unwrap()
        .id;
    assert_eq!(
        store.expire(&[cats, old_home], at(100)).await.unwrap(),
        1,
        "the superseded home is not active"
    );
    let again = store
        .observe(&obs(g, Some(a), "likes", "cats", "cats", 10, 110))
        .await
        .unwrap();
    assert_ne!(again, cats);
    let renewed = store
        .current(g, subject(a))
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.key == "cats")
        .unwrap();
    assert_eq!((renewed.supports, renewed.first_seen), (1, at(110)));
}
