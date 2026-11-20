//! The behavior every store of episodes and memories must have, run against each
//! implementation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::episode::{Episode, EpisodeId, Evidence, NewEpisode};
use crate::memories::{
    About, Changes, Commit, Committed, Durability, HandError, HandMemory, Kind, Memory,
    MemoryState, MemoryStore, NewMemory, NewObservation, Obs, Op, Origin, Said, Target,
};
use crate::names::{Grounds, NameBasis};
use crate::store::{EpisodeStore, MemoryError, WindowState};

pub fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

/// An episode over ordinals `first..=last` whose summary embedding is `vector`'s `[x, y]`.
pub fn episode(group: GroupId, first: u64, last: u64, title: &str) -> NewEpisode {
    NewEpisode {
        group,
        first_ordinal: first,
        last_ordinal: last,
        first_message: MessageId::new(first as i64).unwrap(),
        last_message: MessageId::new(last as i64).unwrap(),
        started: UnixMillis::new(1_800_000_000_000 + first as i64 * 1000),
        ended: UnixMillis::new(1_800_000_000_000 + last as i64 * 1000),
        line_count: (last.saturating_sub(first) + 1) as u32,
        participants: vec![AccountId::new(5).unwrap(), AccountId::new(9).unwrap()],
        title: title.into(),
        summary: format!("summary of {title}"),
        evidence: vec![Evidence {
            message: MessageId::new(first as i64).unwrap(),
            quote: "quote".into(),
        }],
        method: "test".into(),
        model: "m".into(),
    }
}

/// A batch that recorded only that it ran at `at`.
fn batched(group: GroupId, at: UnixMillis) -> Commit {
    Commit {
        group,
        read: at,
        at,
        episodes: Vec::new(),
        skipped: None,
        observations: Vec::new(),
        changes: Vec::new(),
    }
}

/// Both contracts. `store` and `memories` are the same store.
pub async fn run(store: &dyn EpisodeStore, memories: &dyn MemoryStore) {
    episodes(store, memories).await;
    self::memories(store, memories).await;
}

async fn episodes(store: &dyn EpisodeStore, memories: &dyn MemoryStore) {
    let t0 = UnixMillis::new(1_800_000_000_000);
    let (g, h) = (group(8001), group(8002));
    assert_eq!(store.covered_through(g).await.unwrap(), 0);

    // Insert, read back exactly, and scope by group.
    let first = store
        .insert(&episode(g, 1, 30, "deploy"), &[1.0, 0.0], "m1")
        .await
        .unwrap();
    let second = store
        .insert(&episode(g, 31, 60, "dinner"), &[0.0, 1.0], "m1")
        .await
        .unwrap();
    let third = store
        .insert(&episode(g, 61, 90, "holiday"), &[0.6, 0.8], "m1")
        .await
        .unwrap();
    assert!(first.id < second.id && second.id < third.id, "ids increase");
    assert_eq!(first.episode, episode(g, 1, 30, "deploy"));
    assert_eq!(
        store.get(g, first.id).await.unwrap().unwrap().episode,
        first.episode
    );
    assert!(
        store.get(h, first.id).await.unwrap().is_none(),
        "another group cannot read it"
    );
    assert!(
        store
            .get(g, EpisodeId::new(9_999_999))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.covered_through(g).await.unwrap(), 90);
    assert_eq!(store.covered_through(h).await.unwrap(), 0);

    // Ranges of one group never overlap, however they overlap; another group is independent.
    for (a, b) in [(1, 30), (20, 40), (30, 31), (50, 70), (1, 200)] {
        let result = store
            .insert(&episode(g, a, b, "x"), &[1.0, 0.0], "m1")
            .await;
        assert_eq!(result.unwrap_err(), MemoryError::Overlap, "{a}..={b}");
    }
    assert_eq!(
        store
            .insert(&episode(g, 91, 90, "reversed"), &[1.0, 0.0], "m1")
            .await
            .unwrap_err(),
        MemoryError::InvalidRange,
        "a reversed range is its own error, not an overlap"
    );
    store
        .insert(&episode(h, 1, 30, "other group"), &[1.0, 0.0], "m1")
        .await
        .unwrap();
    store
        .insert(&episode(g, 91, 120, "adjacent is fine"), &[0.0, 1.0], "m1")
        .await
        .unwrap();
    assert_eq!(store.covered_through(g).await.unwrap(), 120);

    // A skipped stretch counts as covered, and shares the no-overlap rule with episodes.
    memories.commit(&skip(g, 121, 150, t0)).await.unwrap();
    assert_eq!(store.covered_through(g).await.unwrap(), 150);
    for (a, b) in [(100, 130), (121, 150), (140, 160)] {
        assert_eq!(
            memories.commit(&skip(g, a, b, t0)).await.unwrap_err(),
            MemoryError::Overlap,
            "skip {a}..={b}"
        );
    }
    assert_eq!(
        store
            .insert(&episode(g, 141, 170, "over a skip"), &[1.0, 0.0], "m1")
            .await
            .unwrap_err(),
        MemoryError::Overlap
    );
    assert_eq!(
        memories.commit(&skip(g, 160, 151, t0)).await.unwrap_err(),
        MemoryError::InvalidRange
    );
    memories.commit(&skip(h, 121, 150, t0)).await.unwrap();
    assert!(
        store.within(g, 1, 1000).await.unwrap().len() == 4,
        "a skipped stretch is no episode"
    );

    // `within` returns episodes wholly inside the range, in order.
    let within: Vec<_> = store
        .within(g, 20, 95)
        .await
        .unwrap()
        .iter()
        .map(|e| e.episode.title.clone())
        .collect();
    assert_eq!(
        within,
        ["dinner", "holiday"],
        "a partly covered episode is not wholly inside"
    );
    assert_eq!(store.within(g, 1, 120).await.unwrap().len(), 4);
    assert!(store.within(g, 200, 300).await.unwrap().is_empty());
    assert!(store.within(group(8999), 1, 1000).await.unwrap().is_empty());

    // `ending_within` takes episodes by where they end, however early they begin.
    let ending: Vec<_> = store
        .ending_within(g, 45, 95)
        .await
        .unwrap()
        .iter()
        .map(|e| e.episode.title.clone())
        .collect();
    assert_eq!(ending, ["dinner", "holiday"]);
    assert!(store.ending_within(g, 1, 29).await.unwrap().is_empty());
    assert!(
        store
            .ending_within(group(8999), 1, 1000)
            .await
            .unwrap()
            .is_empty()
    );

    // Search: every candidate within the distance, closest first, scoped by group and model.
    let hits = store.search(g, "m1", &[1.0, 0.0], 2.0).await.unwrap();
    let order: Vec<_> = hits
        .iter()
        .map(|h| h.episode.episode.title.as_str())
        .collect();
    assert_eq!(order[0], "deploy");
    assert!(hits[0].distance < 1e-5 && hits.windows(2).all(|w| w[0].distance <= w[1].distance));
    assert_eq!(hits.len(), 4);
    let near = store.search(g, "m1", &[1.0, 0.0], 0.5).await.unwrap();
    assert_eq!(
        near.iter()
            .map(|h| h.episode.episode.title.as_str())
            .collect::<Vec<_>>(),
        ["deploy", "holiday"],
        "0.6*1 -> distance 0.4"
    );
    assert!(
        store
            .search(g, "other-model", &[1.0, 0.0], 2.0)
            .await
            .unwrap()
            .is_empty(),
        "vectors are scoped to their model"
    );
    assert!(
        store
            .search(h, "m1", &[0.0, 1.0], 0.1)
            .await
            .unwrap()
            .is_empty(),
        "no leakage across groups"
    );
    assert!(
        store
            .search(g, "m1", &[1.0, 0.0, 0.0], 2.0)
            .await
            .unwrap()
            .is_empty(),
        "a different width never matches"
    );

    // The embedding index: what lacks a vector from a model at a width, and replacing one.
    assert!(
        store.unembedded(g, "m1", 2, 10).await.unwrap().is_empty(),
        "everything is embedded with the model it was stored with"
    );
    let ids = |found: Vec<Episode>| found.into_iter().map(|e| e.id).collect::<Vec<_>>();
    let mut every = ids(store.within(g, 0, i64::MAX as u64).await.unwrap());
    every.sort();
    assert_eq!(
        ids(store.unembedded(g, "m2", 2, 100).await.unwrap()),
        every,
        "another model: every episode, oldest first"
    );
    assert_eq!(
        ids(store.unembedded(g, "m1", 3, 100).await.unwrap()),
        every,
        "another width is another index"
    );
    assert_eq!(
        ids(store.unembedded(g, "m2", 2, 2).await.unwrap()),
        [first.id, second.id]
    );
    assert!(
        store
            .unembedded(h, "m2", 2, 100)
            .await
            .unwrap()
            .iter()
            .all(|e| e.episode.group == h),
        "scoped by group"
    );
    store
        .set_embedding(first.id, "m2", &[0.0, 1.0])
        .await
        .unwrap();
    assert_eq!(
        ids(store.unembedded(g, "m2", 2, 100).await.unwrap()),
        every[1..]
    );
    let m2 = store.search(g, "m2", &[0.0, 1.0], 2.0).await.unwrap();
    assert_eq!(m2.len(), 1);
    assert_eq!(m2[0].episode.id, first.id);
    assert!(
        store
            .search(g, "m1", &[1.0, 0.0], 2.0)
            .await
            .unwrap()
            .iter()
            .all(|h| h.episode.id != first.id),
        "an episode has one embedding: the old model's is gone"
    );
    assert!(matches!(
        store
            .set_embedding(EpisodeId::new(9_999_999), "m2", &[1.0, 0.0])
            .await,
        Err(MemoryError::NotFound)
    ));

    // The group's activity: every episode of the group, with who took part.
    let (dawn, end) = (UnixMillis::new(0), UnixMillis::new(i64::MAX));
    let activity = store.activity(g).await.unwrap();
    assert_eq!(activity.since(None, dawn, end) as usize, every.len());
    assert_eq!(
        activity.since(Some(AccountId::new(5).unwrap()), dawn, end) as usize,
        every.len()
    );
    assert_eq!(
        activity.since(Some(AccountId::new(77).unwrap()), dawn, end),
        0
    );
    assert_eq!(
        store.activity(h).await.unwrap().since(None, dawn, end),
        1,
        "scoped by group"
    );

    // The newest episodes before a line, in chat order, at most as many as asked for.
    let titles = |found: Vec<Episode>| -> Vec<String> {
        found.into_iter().map(|e| e.episode.title).collect()
    };
    assert_eq!(
        titles(store.before(g, 61, 5).await.unwrap()),
        ["deploy", "dinner"],
        "an episode ending at or after the line is not before it"
    );
    assert_eq!(titles(store.before(g, 91, 1).await.unwrap()), ["holiday"]);
    assert!(store.before(g, 1, 5).await.unwrap().is_empty());

    // A group's window: none before its first batch; a cut moves the raw part forward only, and
    // the next batch starts it over.
    let w = group(8003);
    assert_eq!(store.window(w).await.unwrap(), None);
    let (t1, t2) = (
        UnixMillis::new(1_800_000_100_000),
        UnixMillis::new(1_800_000_200_000),
    );
    memories.commit(&batched(w, t1)).await.unwrap();
    assert_eq!(
        store.window(w).await.unwrap(),
        Some(WindowState {
            batched: t1,
            cut: 0
        })
    );
    store.cut(w, 40, t2).await.unwrap();
    store.cut(w, 30, t2).await.unwrap();
    assert_eq!(
        store.window(w).await.unwrap(),
        Some(WindowState {
            batched: t1,
            cut: 40
        }),
        "never back, and the batch time stays"
    );
    memories.commit(&batched(w, t2)).await.unwrap();
    assert_eq!(
        store.window(w).await.unwrap(),
        Some(WindowState {
            batched: t2,
            cut: 0
        })
    );
    // A cut before any batch starts the group's window at that moment.
    let v = group(8004);
    store.cut(v, 7, t2).await.unwrap();
    assert_eq!(
        store.window(v).await.unwrap(),
        Some(WindowState {
            batched: t2,
            cut: 7
        })
    );
    assert_eq!(store.window(group(8999)).await.unwrap(), None, "per group");
}

fn skip(group: GroupId, first: u64, last: u64, at: UnixMillis) -> Commit {
    Commit {
        group,
        read: at,
        at,
        episodes: Vec::new(),
        skipped: Some((first, last, "test".into())),
        observations: Vec::new(),
        changes: Vec::new(),
    }
}

fn account(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

fn statement(text: &str) -> Said {
    Said::Statement {
        text: text.into(),
        term: None,
        durability: Durability::Ongoing,
        with: None,
    }
}

fn name_use(text: &str) -> Said {
    Said::Name {
        text: text.into(),
        basis: NameBasis::Own,
        grounds: Grounds::Own,
        anchored: false,
        shown_by: Vec::new(),
    }
}

fn seen(episode: usize, message: i64, author: i64, about: About, said: Said) -> NewObservation {
    NewObservation {
        episode,
        message: MessageId::new(message).unwrap(),
        ordinal: message as u64,
        author: account(author),
        quote: format!("quote {message}"),
        at: UnixMillis::new(1_800_000_000_000 + message * 1000),
        about,
        said,
    }
}

fn new_statement(subject: Option<AccountId>, text: &str) -> NewMemory {
    NewMemory {
        subject,
        kind: Kind::Statement,
        with: None,
        term: None,
        text: text.into(),
        durability: Durability::Ongoing,
        origin: Origin::Chat,
        at: UnixMillis::new(1_800_000_000_000),
    }
}

fn texts(memories: &[Memory]) -> Vec<(String, bool)> {
    memories
        .iter()
        .map(|m| (m.text.clone(), m.is_current()))
        .collect()
}

/// The behavior every store of what is known must have. `store` is also the group's episode
/// store: a commit spans both.
pub async fn memories(episodes: &dyn EpisodeStore, store: &dyn MemoryStore) {
    let (g, h) = (group(8101), group(8102));
    let (kit, sam, ray) = (account(11), account(12), account(13));
    let t = |n: i64| UnixMillis::new(1_800_000_000_000 + n * 1000);

    // By hand: entered, refused when known, a name set for one member is not set for another.
    let pizza = store
        .add(
            g,
            &HandMemory {
                subject: Some(kit),
                kind: Kind::Statement,
                term: None,
                text: "likes pizza".into(),
                by: sam,
                at: t(1),
            },
        )
        .await
        .unwrap();
    let again = HandMemory {
        subject: Some(kit),
        kind: Kind::Statement,
        term: None,
        text: "Likes  Pizza".into(),
        by: kit,
        at: t(2),
    };
    assert_eq!(
        store.add(g, &again).await.unwrap_err(),
        HandError::Known(pizza),
        "the same statement in another case and spacing"
    );
    let named = HandMemory {
        subject: Some(kit),
        kind: Kind::Name,
        term: None,
        text: "Kit".into(),
        by: kit,
        at: t(3),
    };
    let kit_name = store.add(g, &named).await.unwrap();
    // Two members may go by the same name.
    let lee = AccountId::new(39).unwrap();
    let lees = store
        .add(
            g,
            &HandMemory {
                subject: Some(lee),
                text: "KIT".into(),
                ..named.clone()
            },
        )
        .await
        .unwrap();
    let stored = store.memory(g, pizza).await.unwrap().unwrap();
    assert_eq!(stored.origin, Origin::Hand { by: sam });
    assert!(store.memory(h, pizza).await.unwrap().is_none(), "per group");
    let names: Vec<(crate::MemoryId, Option<AccountId>, String)> = store
        .names(g)
        .await
        .unwrap()
        .into_iter()
        .map(|n| (n.id, n.subject, n.text))
        .collect();
    assert_eq!(
        names,
        [
            (kit_name, Some(kit), "Kit".to_owned()),
            (lees, Some(lee), "KIT".to_owned())
        ]
    );

    // Corrected: a new memory replaces it, the old one kept. Ended: kept as no longer holding.
    let fixed = store.fix(g, pizza, "likes pasta", kit, t(4)).await.unwrap();
    let about_kit = store.about(g, Some(kit)).await.unwrap();
    assert!(about_kit.iter().any(|m| m.id == pizza
        && m.state
            == MemoryState::Replaced {
                by: Some(fixed),
                at: t(4)
            }));
    assert_eq!(
        store.fix(g, pizza, "again", kit, t(5)).await.unwrap_err(),
        HandError::NotFound,
        "only a current memory"
    );
    store.end(g, fixed, t(5)).await.unwrap();
    assert_eq!(
        store.memory(g, fixed).await.unwrap().unwrap().state,
        MemoryState::Ended { at: t(5) }
    );
    assert!(
        store
            .current(g, &[Some(kit)], Kind::Statement)
            .await
            .unwrap()
            .is_empty()
    );

    // A batch: its episode, its observations, and the changes, all at once.
    let first = Commit {
        group: g,
        read: t(10),
        at: t(11),
        episodes: vec![(episode(g, 1, 10, "first"), vec![1.0, 0.0], "m1".into())],
        skipped: None,
        observations: vec![
            seen(0, 2, 11, About::Member(kit), name_use("Kitty")),
            seen(0, 3, 11, About::Member(kit), statement("lives in Hangzhou")),
            seen(
                0,
                4,
                12,
                About::Name("Old Ray".into()),
                statement("keeps bees"),
            ),
            seen(0, 5, 12, About::Group, statement("meets on Fridays")),
        ],
        changes: vec![
            Changes {
                subject: Some(kit),
                ops: vec![
                    Op::Create(NewMemory {
                        kind: Kind::Name,
                        text: "Kitty".into(),
                        durability: Durability::Lasting,
                        ..new_statement(Some(kit), "")
                    }),
                    Op::Support {
                        obs: Obs::New(0),
                        memory: Target::Created(0),
                    },
                    Op::Create(new_statement(Some(kit), "lives in Hangzhou")),
                    Op::Support {
                        obs: Obs::New(1),
                        memory: Target::Created(1),
                    },
                ],
            },
            Changes {
                subject: None,
                ops: vec![
                    Op::Create(new_statement(None, "meets on Fridays")),
                    Op::Support {
                        obs: Obs::New(3),
                        memory: Target::Created(0),
                    },
                ],
            },
        ],
    };
    let committed = store.commit(&first).await.unwrap();
    assert_eq!(
        committed,
        Committed {
            episodes: 1,
            deferred: 0
        }
    );
    assert_eq!(episodes.covered_through(g).await.unwrap(), 10);
    assert_eq!(
        episodes.window(g).await.unwrap(),
        Some(WindowState {
            batched: t(11),
            cut: 0
        })
    );
    let hangzhou = store
        .current(g, &[Some(kit)], Kind::Statement)
        .await
        .unwrap();
    assert_eq!(texts(&hangzhou), [("lives in Hangzhou".to_owned(), true)]);
    let support = &hangzhou[0].support;
    assert_eq!(support.len(), 1);
    assert_eq!(
        (
            support[0].message,
            support[0].author,
            support[0].quote.as_str()
        ),
        (MessageId::new(3).unwrap(), kit, "quote 3")
    );
    assert_eq!(hangzhou[0].episodes(), 1);
    assert_eq!(hangzhou[0].own_word(), 1);
    let names = store.names(g).await.unwrap();
    assert_eq!(
        names.len(),
        3,
        "the names set by hand and the one read from chat"
    );
    let kitty = names.iter().find(|n| n.text == "Kitty").unwrap();
    assert_eq!((kitty.origin, kitty.support.len()), (Origin::Chat, 1));
    let pending = store.pending(g).await.unwrap();
    assert_eq!(pending.len(), 1, "what nobody has placed waits");
    assert_eq!(pending[0].about, About::Name("Old Ray".into()));
    assert!(store.pending(h).await.unwrap().is_empty());
    let group_facts = store.current(g, &[None], Kind::Statement).await.unwrap();
    assert_eq!(texts(&group_facts), [("meets on Fridays".to_owned(), true)]);

    // The same range again is refused, and nothing of it is written.
    assert_eq!(
        store.commit(&first).await.unwrap_err(),
        MemoryError::Overlap
    );
    assert_eq!(store.pending(g).await.unwrap().len(), 1);
    assert_eq!(store.about(g, Some(kit)).await.unwrap().len(), 5);
    // A skipped stretch counts as covered and shares the rule.
    store.commit(&skip(g, 11, 15, t(12))).await.unwrap();
    assert_eq!(episodes.covered_through(g).await.unwrap(), 15);
    assert_eq!(
        store.commit(&skip(g, 14, 20, t(12))).await.unwrap_err(),
        MemoryError::Overlap
    );

    // A held statement filed under its member; a replacement; a merge; a drop; an eviction.
    let held = pending[0].id;
    let moved = Commit {
        group: g,
        read: t(20),
        at: t(21),
        episodes: vec![(episode(g, 16, 20, "second"), vec![0.0, 1.0], "m1".into())],
        skipped: None,
        observations: vec![
            seen(0, 17, 11, About::Member(kit), statement("moved to Ningbo")),
            seen(0, 18, 11, About::Member(kit), statement("plays chess")),
            seen(
                0,
                19,
                11,
                About::Member(kit),
                statement("plays Chess often"),
            ),
        ],
        changes: vec![
            Changes {
                subject: Some(ray),
                ops: vec![
                    Op::Refile {
                        obs: Obs::Stored(held),
                        account: ray,
                    },
                    Op::Create(new_statement(Some(ray), "keeps bees")),
                    Op::Support {
                        obs: Obs::Stored(held),
                        memory: Target::Created(0),
                    },
                ],
            },
            Changes {
                subject: Some(kit),
                ops: vec![
                    Op::Create(new_statement(Some(kit), "lives in Ningbo")),
                    Op::Support {
                        obs: Obs::New(0),
                        memory: Target::Created(0),
                    },
                    Op::Close {
                        memory: hangzhou[0].id,
                        by: Some(Target::Created(0)),
                        at: t(17),
                    },
                    Op::Create(new_statement(Some(kit), "plays chess")),
                    Op::Support {
                        obs: Obs::New(1),
                        memory: Target::Created(1),
                    },
                    Op::Create(new_statement(Some(kit), "plays chess often")),
                    Op::Support {
                        obs: Obs::New(2),
                        memory: Target::Created(2),
                    },
                ],
            },
        ],
    };
    store.commit(&moved).await.unwrap();
    assert!(store.pending(g).await.unwrap().is_empty());
    let bees = store.about(g, Some(ray)).await.unwrap();
    assert_eq!(texts(&bees), [("keeps bees".to_owned(), true)]);
    assert_eq!(bees[0].support[0].about, About::Member(ray));
    let kits = store.about(g, Some(kit)).await.unwrap();
    let ningbo = kits.iter().find(|m| m.text == "lives in Ningbo").unwrap();
    assert_eq!(
        kits.iter().find(|m| m.id == hangzhou[0].id).unwrap().state,
        MemoryState::Replaced {
            by: Some(ningbo.id),
            at: t(17)
        },
        "the old one is kept, as replaced"
    );
    let chess = kits.iter().find(|m| m.text == "plays chess").unwrap().id;
    let often = kits
        .iter()
        .find(|m| m.text == "plays chess often")
        .unwrap()
        .id;
    store
        .commit(&Commit {
            group: g,
            read: t(30),
            at: t(31),
            episodes: Vec::new(),
            skipped: None,
            observations: Vec::new(),
            changes: vec![Changes {
                subject: Some(kit),
                ops: vec![
                    Op::Merge {
                        from: often,
                        into: Target::Existing(chess),
                    },
                    Op::Evict(ningbo.id),
                ],
            }],
        })
        .await
        .unwrap();
    let kits = store.about(g, Some(kit)).await.unwrap();
    assert!(kits.iter().all(|m| m.id != often && m.id != ningbo.id));
    let merged = kits.iter().find(|m| m.id == chess).unwrap();
    assert_eq!(
        merged.support.len(),
        2,
        "what supported the duplicate now supports it"
    );
    assert_eq!(
        kits.iter().find(|m| m.id == hangzhou[0].id).unwrap().state,
        MemoryState::Replaced {
            by: None,
            at: t(17)
        },
        "what replaced it is gone"
    );

    // Forgotten: deleted with its support, and remembered as forgotten.
    let gone = store.forget(g, chess, kit, t(40)).await.unwrap().unwrap();
    assert_eq!(gone.text, "plays chess");
    assert!(store.memory(g, chess).await.unwrap().is_none());
    assert!(store.forget(g, chess, kit, t(40)).await.unwrap().is_none());
    let forgotten = store.suppressions(g).await.unwrap();
    assert_eq!(forgotten.len(), 1);
    assert_eq!(
        (
            forgotten[0].subject,
            forgotten[0].kind,
            forgotten[0].text.as_str(),
            forgotten[0].key.as_str(),
            forgotten[0].by,
            forgotten[0].at
        ),
        (
            Some(kit),
            Kind::Statement,
            "plays chess",
            "plays chess",
            kit,
            t(40)
        )
    );
    assert_eq!(
        forgotten[0].messages,
        [MessageId::new(18).unwrap(), MessageId::new(19).unwrap()],
        "the lines it rested on"
    );
    assert!(store.suppressions(h).await.unwrap().is_empty());
    // Entered again by hand, it is known again: a person's word lifts what was forgotten.
    store
        .add(
            g,
            &HandMemory {
                subject: Some(kit),
                kind: Kind::Statement,
                term: None,
                text: "plays chess".into(),
                by: kit,
                at: t(41),
            },
        )
        .await
        .unwrap();
    assert!(store.suppressions(g).await.unwrap().is_empty());

    // A command overtakes a batch: what was forgotten after the batch read wins. The subject's
    // changes are left out, its new observations stay pending, except those reading the lines
    // of what was forgotten.
    let tea = store
        .add(
            g,
            &HandMemory {
                subject: Some(sam),
                kind: Kind::Statement,
                term: None,
                text: "drinks tea".into(),
                by: sam,
                at: t(42),
            },
        )
        .await
        .unwrap();
    let overtaken = Commit {
        group: g,
        read: t(50),
        at: t(52),
        episodes: vec![(episode(g, 21, 30, "third"), vec![1.0, 0.0], "m1".into())],
        skipped: None,
        observations: vec![
            seen(0, 22, 12, About::Member(sam), statement("drinks green tea")),
            seen(0, 23, 11, About::Member(kit), statement("likes cats")),
        ],
        changes: vec![
            Changes {
                subject: Some(sam),
                ops: vec![Op::Support {
                    obs: Obs::New(0),
                    memory: Target::Existing(tea),
                }],
            },
            Changes {
                subject: Some(kit),
                ops: vec![
                    Op::Create(new_statement(Some(kit), "likes cats")),
                    Op::Support {
                        obs: Obs::New(1),
                        memory: Target::Created(0),
                    },
                ],
            },
        ],
    };
    store.forget(g, tea, sam, t(51)).await.unwrap();
    let committed = store.commit(&overtaken).await.unwrap();
    assert_eq!(
        committed,
        Committed {
            episodes: 1,
            deferred: 1
        }
    );
    assert!(store.about(g, Some(sam)).await.unwrap().is_empty());
    let pending = store.pending(g).await.unwrap();
    assert_eq!(
        pending.iter().map(|o| o.message.get()).collect::<Vec<_>>(),
        [22],
        "waiting for the next batch"
    );
    assert_eq!(
        texts(
            &store
                .current(g, &[Some(kit)], Kind::Statement)
                .await
                .unwrap()
        ),
        [
            ("plays chess".to_owned(), true),
            ("likes cats".to_owned(), true)
        ],
        "another subject's changes apply"
    );
    // A pending observation rejected is dropped.
    store
        .commit(&Commit {
            group: g,
            read: t(60),
            at: t(60),
            episodes: Vec::new(),
            skipped: None,
            observations: Vec::new(),
            changes: vec![Changes {
                subject: Some(sam),
                ops: vec![Op::Drop(pending[0].id)],
            }],
        })
        .await
        .unwrap();
    assert!(store.pending(g).await.unwrap().is_empty());
    assert!(
        store.about(h, Some(kit)).await.unwrap().is_empty(),
        "per group"
    );
}
