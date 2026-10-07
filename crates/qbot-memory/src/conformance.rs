//! The behavior every `EpisodeStore` must have, run against each implementation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::episode::{Episode, EpisodeId, Evidence, NewEpisode};
use crate::store::{EpisodeStore, MemoryError};

pub fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

/// An episode over ordinals `first..=last` whose summary embedding is `vector`'s `[x, y]`.
pub fn episode(group: GroupId, first: u64, last: u64, title: &str) -> NewEpisode {
    NewEpisode {
        group,
        first_batch: (first - 1) / 10,
        last_batch: (last - 1) / 10,
        first_ordinal: first,
        last_ordinal: last,
        batch_lines: 10,
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
        findings: Default::default(),
    }
}

pub async fn run(store: &dyn EpisodeStore) {
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

    // A skipped slice counts as covered, and shares the no-overlap rule with episodes.
    store.skip_slice(g, 121, 150, "refused").await.unwrap();
    assert_eq!(store.covered_through(g).await.unwrap(), 150);
    for (a, b) in [(100, 130), (121, 150), (140, 160)] {
        assert_eq!(
            store.skip_slice(g, a, b, "again").await.unwrap_err(),
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
        store.skip_slice(g, 160, 151, "reversed").await.unwrap_err(),
        MemoryError::InvalidRange
    );
    store.skip_slice(h, 121, 150, "other group").await.unwrap();
    assert!(
        store.within(g, 1, 1000).await.unwrap().len() == 4,
        "a skipped slice is no episode"
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
}
