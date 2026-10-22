//! The behavior every `FactStore` must have; the in-memory store defines it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::episode::EpisodeId;
use crate::facts::{DecayPolicy, FactStore, Observation, Retraction, slot, term_slot};
use crate::predicates::{DecayClass, Predicates};

fn at(seconds: i64) -> UnixMillis {
    UnixMillis::new(1_800_000_000_000 + seconds * 1000)
}

fn obs(
    group: GroupId,
    subject: Option<i64>,
    predicate: &str,
    object: &str,
    episode: i64,
    when: i64,
) -> Observation {
    let slot = match Predicates::builtin().get(predicate) {
        Some(p) => slot(p, object),
        None => term_slot(object),
    };
    Observation {
        group,
        subject: subject.map(|s| AccountId::new(s).unwrap()),
        predicate: predicate.into(),
        slot,
        object: object.into(),
        label: None,
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
    let policy = DecayPolicy::default();

    // A new value is recorded; the same episode again changes nothing; another episode supports
    // it (values compare normalized) and moves its confirmation forward.
    let first = store
        .observe(&obs(g, Some(a), "likes", "Cats", 1, 0))
        .await
        .unwrap();
    assert_eq!(
        store
            .observe(&obs(g, Some(a), "likes", "Cats", 1, 0))
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        store
            .observe(&obs(g, Some(a), "likes", "  CATS ", 2, 50))
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
            current[0].first_seen,
            current[0].key.as_str(),
        ),
        (2, at(50), at(0), "cats")
    );
    assert_eq!(current[0].object, "Cats", "the first wording is kept");

    // A multi-valued predicate keeps values side by side, neither weighing on the other; a
    // single-valued one keeps a new value beside the old, which the later one contradicts.
    store
        .observe(&obs(g, Some(a), "likes", "dogs", 3, 60))
        .await
        .unwrap();
    let old_home = store
        .observe(&obs(g, Some(a), "lives_in", "Hangzhou", 4, 70))
        .await
        .unwrap();
    let new_home = store
        .observe(&obs(g, Some(a), "lives_in", "Shanghai", 5, 80))
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
            ("lives_in", "Shanghai"),
            ("lives_in", "Hangzhou")
        ],
        "by predicate; within one, the most recently confirmed first"
    );
    let confidence = |id| {
        let fact = current.iter().find(|f| f.id == id).unwrap();
        policy.confidence(fact, &current, at(80))
    };
    assert!((confidence(new_home) - 2.0 / 3.0).abs() < 1e-9);
    assert!(
        (confidence(old_home) - 2.0 / 3.0 * (1.0 - 2.0 / 3.0)).abs() < 1e-4,
        "the later value counts against the earlier one"
    );
    assert!(
        (confidence(first) - 0.75).abs() < 1e-4,
        "dogs do not count against cats"
    );

    // Confirming the older value again brings it to the front, and the other is now the one
    // contradicted.
    store
        .observe(&obs(g, Some(a), "lives_in", "hangzhou", 20, 90))
        .await
        .unwrap();
    let current = store.current(g, subject(a)).await.unwrap();
    let homes: Vec<&str> = current
        .iter()
        .filter(|f| f.predicate == "lives_in")
        .map(|f| f.object.as_str())
        .collect();
    assert_eq!(homes, ["Hangzhou", "Shanghai"]);
    let old = current.iter().find(|f| f.id == old_home).unwrap();
    let new = current.iter().find(|f| f.id == new_home).unwrap();
    assert!(policy.confidence(old, &current, at(90)) > policy.confidence(new, &current, at(90)));

    // An opposite predicate competes with its counterpart for the same thing, and only that.
    store
        .observe(&obs(g, Some(a), "dislikes", "dogs", 6, 100))
        .await
        .unwrap();
    let current = store.current(g, subject(a)).await.unwrap();
    let dogs = current
        .iter()
        .find(|f| f.predicate == "likes" && f.key == "dogs")
        .unwrap();
    let cats = current.iter().find(|f| f.id == first).unwrap();
    let hate = current.iter().find(|f| f.predicate == "dislikes").unwrap();
    assert!(dogs.competes_with(hate) && hate.competes_with(dogs));
    assert!(!cats.competes_with(hate) && !cats.competes_with(dogs));

    // Subjects and groups are separate; group facts have no subject.
    store
        .observe(&obs(g, Some(b), "likes", "cats", 7, 0))
        .await
        .unwrap();
    let mut term = obs(g, None, "group_term", "good game", 8, 0);
    term.slot = term_slot("GG");
    term.label = Some("GG".into());
    store.observe(&term).await.unwrap();
    let mut meaning = term.clone();
    meaning.object = "get good".into();
    meaning.episode = EpisodeId::new(11);
    store.observe(&meaning).await.unwrap();
    store
        .observe(&obs(other, Some(a), "likes", "tea", 9, 0))
        .await
        .unwrap();
    assert_eq!(store.current(g, subject(b)).await.unwrap().len(), 1);
    let group_facts = store.current(g, None).await.unwrap();
    assert_eq!(group_facts.len(), 2, "a term's meanings stand side by side");
    assert!(group_facts[0].competes_with(&group_facts[1]));
    assert_eq!(
        (
            group_facts[0].label.as_deref(),
            group_facts[0].slot.as_str()
        ),
        (Some("GG"), term_slot("gg").as_str())
    );
    assert_eq!(store.current(other, subject(a)).await.unwrap().len(), 1);

    // Forgetting is per group and only for active facts; the same value said again afterwards is
    // a new claim.
    let tea = store.current(other, subject(a)).await.unwrap()[0].id;
    assert!(
        !store.forget(g, tea).await.unwrap(),
        "not a fact of this group"
    );
    assert!(store.forget(other, tea).await.unwrap());
    assert!(!store.forget(other, tea).await.unwrap());
    assert!(store.current(other, subject(a)).await.unwrap().is_empty());
    let again = store
        .observe(&obs(other, Some(a), "likes", "tea", 10, 110))
        .await
        .unwrap();
    assert_ne!(again, tea);
    let renewed = &store.current(other, subject(a)).await.unwrap()[0];
    assert_eq!((renewed.supports, renewed.first_seen), (1, at(110)));

    // A retraction records when the claim with the value it names was said to have ended, and
    // touches nothing else; it counts until the claim is supported again.
    let retraction = |group: GroupId, object: &str, when: i64| Retraction {
        group,
        subject: subject(a),
        predicate: "lives_in".into(),
        slot: "lives_in".into(),
        object: object.into(),
        at: at(when),
    };
    assert!(
        !store.retract(&retraction(g, "Beijing", 120)).await.unwrap(),
        "never recorded"
    );
    assert!(
        !store
            .retract(&retraction(other, "Shanghai", 120))
            .await
            .unwrap()
    );
    assert!(
        store
            .retract(&retraction(g, " shanghai", 120))
            .await
            .unwrap()
    );
    assert!(
        store
            .retract(&retraction(g, "Shanghai", 115))
            .await
            .unwrap(),
        "an earlier retraction keeps the later time"
    );
    let current = store.current(g, subject(a)).await.unwrap();
    let shanghai = current.iter().find(|f| f.id == new_home).unwrap();
    assert_eq!(shanghai.ended, Some(at(120)));
    assert!(
        current
            .iter()
            .filter(|f| f.id != new_home)
            .all(|f| f.ended.is_none()),
        "other facts are untouched"
    );
    let before = policy.confidence(shanghai, &current, at(120));
    store
        .observe(&obs(g, Some(a), "lives_in", "Shanghai", 12, 130))
        .await
        .unwrap();
    let current = store.current(g, subject(a)).await.unwrap();
    let shanghai = current.iter().find(|f| f.id == new_home).unwrap();
    assert!(policy.confidence(shanghai, &current, at(130)) > before * 3.0);

    // Eviction deletes active claims of the group with their evidence, nothing else; the same
    // value said again afterwards is a new claim.
    let cats = current.iter().find(|f| f.key == "cats").unwrap().id;
    assert_eq!(
        store.evict(other, &[cats]).await.unwrap(),
        0,
        "not this group's"
    );
    assert_eq!(store.evict(g, &[cats, new_home]).await.unwrap(), 2);
    assert_eq!(store.evict(g, &[cats]).await.unwrap(), 0, "already gone");
    assert_eq!(store.evict(g, &[]).await.unwrap(), 0);
    let left = store.current(g, subject(a)).await.unwrap();
    assert!(left.iter().all(|f| f.id != cats && f.id != new_home));
    assert!(left.iter().any(|f| f.key == "dogs"), "other claims stay");
    let again = store
        .observe(&obs(g, Some(a), "likes", "cats", 2, 140))
        .await
        .unwrap();
    assert_ne!(again, cats);
    let renewed = store.current(g, subject(a)).await.unwrap();
    let renewed = renewed.iter().find(|f| f.id == again).unwrap();
    assert_eq!(
        (renewed.supports, renewed.first_seen),
        (1, at(140)),
        "its old evidence went with it, so an episode that supported it before counts afresh"
    );
}
