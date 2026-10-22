//! Room for learned claims follows the conversation: salience on each group's own episode clock,
//! and capacity managed as episodes are applied.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_memory::belief::{Activity, salience};
use qbot_memory::conformance::episode;
use qbot_memory::consolidate::Consolidator;
use qbot_memory::facts::{Capacity, DecayPolicy, FactStore, GROUP_TOPIC, MemoryFactStore};
use qbot_memory::findings::{FactFinding, Findings, KnowledgeFinding};
use qbot_memory::identity::IdentityPolicy;
use qbot_memory::predicates::Predicates;
use qbot_memory::{Episode, EpisodeStore, MemoryEpisodeStore, MemoryIdentityStore};

const DAY: i64 = 86_400_000;
const T0: i64 = 1_800_000_000_000;

fn g() -> GroupId {
    GroupId::new(4200).unwrap()
}

fn who(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

fn fact(account: i64, predicate: &str, object: &str) -> FactFinding {
    FactFinding {
        account: who(account),
        predicate: predicate.into(),
        object: object.into(),
        message: MessageId::new(1).unwrap(),
        quote: object.into(),
    }
}

fn topic(text: &str) -> KnowledgeFinding {
    KnowledgeFinding::Topic {
        topic: text.into(),
        message: MessageId::new(1).unwrap(),
        quote: text.into(),
    }
}

struct Rig {
    episodes: Arc<MemoryEpisodeStore>,
    facts: Arc<MemoryFactStore>,
    consolidator: Consolidator,
    next: u64,
}

impl Rig {
    fn new(capacity: Capacity) -> Self {
        let episodes = Arc::new(MemoryEpisodeStore::new());
        let facts = Arc::new(MemoryFactStore::new());
        let identity = Arc::new(MemoryIdentityStore::new(IdentityPolicy::default()));
        let consolidator =
            Consolidator::new(facts.clone(), identity, Arc::new(Predicates::builtin()))
                .with_capacity(episodes.clone(), DecayPolicy::default(), capacity);
        Self {
            episodes,
            facts,
            consolidator,
            next: 1,
        }
    }

    /// Store and apply the group's next episode, `day` days in, with these participants and
    /// findings.
    async fn say(&mut self, day: i64, people: &[i64], findings: Findings) -> Episode {
        let n = self.next;
        self.next += 1;
        let mut e = episode(g(), (n - 1) * 10 + 1, n * 10, &format!("episode {n}"));
        e.ended = UnixMillis::new(T0 + day * DAY);
        e.started = UnixMillis::new(T0 + day * DAY - 60_000);
        e.participants = people.iter().map(|p| who(*p)).collect();
        e.findings = findings;
        let stored = self.episodes.insert(&e, &[1.0, 0.0], "m").await.unwrap();
        self.consolidator.apply(&stored).await.unwrap();
        stored
    }

    async fn chat(&mut self, day: i64, people: &[i64]) {
        self.say(day, people, Findings::default()).await;
    }

    async fn known(&self, account: Option<i64>) -> Vec<String> {
        let mut out: Vec<String> = self
            .facts
            .current(g(), account.map(who))
            .await
            .unwrap()
            .into_iter()
            .map(|f| format!("{}={}", f.predicate, f.object))
            .collect();
        out.sort();
        out
    }
}

fn facts(list: Vec<FactFinding>) -> Findings {
    Findings {
        facts: list,
        ..Findings::default()
    }
}

fn person_room(n: usize) -> Capacity {
    Capacity {
        person_facts: n,
        ..Capacity::default()
    }
}

#[test]
fn salience_runs_on_the_episodes_a_claim_was_passed_over_in() {
    assert!((salience(10.0, 0) - 1.0).abs() < 1e-12);
    assert!((salience(10.0, 10) - 0.5).abs() < 1e-12);
    let at = |d: i64| UnixMillis::new(T0 + d * DAY);
    let activity = Activity::new(vec![
        (at(3), vec![who(5)]),
        (at(1), vec![who(5), who(9)]),
        (at(2), vec![who(9)]),
    ]);
    // A claim about member 5 supported on day 1 was passed over on day 3 only; one about the
    // group, on days 2 and 3; nothing after `until` counts, nor the supporting episode itself.
    assert_eq!(activity.since(Some(who(5)), at(1), at(9)), 1);
    assert_eq!(activity.since(None, at(1), at(9)), 2);
    assert_eq!(activity.since(None, at(1), at(2)), 1);
    assert_eq!(activity.since(Some(who(7)), at(0), at(9)), 0);
}

#[tokio::test]
async fn new_memories_find_room_and_the_stalest_temporary_one_makes_way() {
    let mut r = Rig::new(person_room(3));
    r.say(
        0,
        &[5],
        facts(vec![
            fact(5, "plays", "chess"),
            fact(5, "likes", "tea"),
            fact(5, "lives_in", "Hangzhou"),
        ]),
    )
    .await;
    // Member 5 keeps talking without mentioning any of it.
    for day in 1..=20 {
        r.chat(day, &[5, 9]).await;
    }
    r.say(21, &[5], facts(vec![fact(5, "plays", "go")])).await;
    assert_eq!(
        r.known(Some(5)).await,
        ["likes=tea", "lives_in=Hangzhou", "plays=go"],
        "the new claim is recorded; the temporary activity passed over twenty times makes way, \
         the taste and the place stay"
    );
}

#[tokio::test]
async fn a_quiet_member_or_group_keeps_what_it_knows_however_old() {
    let mut r = Rig::new(person_room(3));
    r.say(
        0,
        &[5],
        facts(vec![
            fact(5, "plays", "chess"),
            fact(5, "likes", "tea"),
            fact(5, "lives_in", "Hangzhou"),
        ]),
    )
    .await;
    // Months pass; others talk and learn things, member 5 says nothing.
    for day in 1..=30 {
        r.chat(day * 10, &[9]).await;
    }
    r.say(400, &[9], facts(vec![fact(9, "likes", "coffee")]))
        .await;
    assert_eq!(
        r.known(Some(5)).await,
        ["likes=tea", "lives_in=Hangzhou", "plays=chess"],
        "room is made only where it is needed: nothing of member 5 was evicted"
    );
}

#[tokio::test]
async fn corroborated_claims_outlast_a_single_mention() {
    let mut r = Rig::new(person_room(2));
    for day in 0..4 {
        r.say(day, &[5], facts(vec![fact(5, "likes", "tea")])).await;
    }
    r.say(4, &[5], facts(vec![fact(5, "likes", "jazz")])).await;
    for day in 5..15 {
        r.chat(day, &[5]).await;
    }
    r.say(15, &[5], facts(vec![fact(5, "likes", "rain")])).await;
    assert_eq!(
        r.known(Some(5)).await,
        ["likes=rain", "likes=tea"],
        "said in four episodes, tea outlasts jazz, said once"
    );
}

#[tokio::test]
async fn a_contradicted_value_is_the_first_to_make_way() {
    let mut r = Rig::new(person_room(2));
    for day in 0..3 {
        r.say(day, &[5], facts(vec![fact(5, "lives_in", "Beijing")]))
            .await;
    }
    r.say(3, &[5], facts(vec![fact(5, "lives_in", "Shanghai")]))
        .await;
    assert_eq!(
        r.known(Some(5)).await,
        ["lives_in=Beijing", "lives_in=Shanghai"],
        "within room, the contradicted value stays as history"
    );
    r.say(4, &[5], facts(vec![fact(5, "plays", "go")])).await;
    assert_eq!(
        r.known(Some(5)).await,
        ["lives_in=Shanghai", "plays=go"],
        "three mentions of Beijing do not save it once Shanghai was said after them"
    );
}

#[tokio::test]
async fn what_an_episode_says_is_never_evicted_by_it_and_applying_it_again_changes_nothing() {
    let mut r = Rig::new(person_room(1));
    r.say(0, &[5], facts(vec![fact(5, "likes", "tea")])).await;
    let busy = r
        .say(
            1,
            &[5],
            facts(vec![fact(5, "plays", "go"), fact(5, "watches", "anime")]),
        )
        .await;
    let after = r.known(Some(5)).await;
    assert_eq!(
        after,
        ["plays=go", "watches=anime"],
        "both new claims stay, though over room; the older one makes way"
    );
    r.consolidator.apply(&busy).await.unwrap();
    assert_eq!(r.known(Some(5)).await, after);
    let current = r.facts.current(g(), Some(who(5))).await.unwrap();
    assert!(
        current.iter().all(|f| f.supports == 1),
        "no support counted twice"
    );
}

#[tokio::test]
async fn a_busy_group_cycles_its_topics_while_a_lasting_one_stays() {
    let mut r = Rig::new(Capacity {
        group_topics: 2,
        ..Capacity::default()
    });
    let topics = |list: &[&str]| Findings {
        knowledge: list.iter().map(|t| topic(t)).collect(),
        ..Findings::default()
    };
    // The group keeps coming back to games; passing topics come and go.
    for day in 0..6 {
        r.say(day, &[5], topics(&["games"])).await;
    }
    for (day, passing) in ["football", "exams", "movies", "weather"]
        .iter()
        .enumerate()
    {
        r.say(10 + day as i64, &[5, 9], topics(&[passing])).await;
    }
    let known: Vec<String> = r
        .known(None)
        .await
        .into_iter()
        .filter(|k| k.starts_with(GROUP_TOPIC))
        .collect();
    assert_eq!(
        known,
        ["group_topic=games", "group_topic=weather"],
        "room for two: the latest passing topic, and the one said again and again"
    );
}
