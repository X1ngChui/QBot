#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::*;
use qbot_agent::sim::MemorySink;
use qbot_agent::{ChatView, RunState, ToolCx, ToolSet, Trigger};
use qbot_core::{
    AccountId, BatchGrid, GroupId, MessageId, RunId, SliceGrid, SystemClock, UnixMillis,
};
use qbot_llm::FakeEmbedder;
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_memory::identity::IdentityPolicy;
use qbot_memory::{
    BuilderConfig, EpisodeBuilder, EpisodeExtractor, EpisodeJobs, EpisodeStore, Recall,
    RecallParams,
};
use qbot_store::{Appended, NewLine, NewSpeaker, PgArchive, PgEpisodeStore, PgIdentityStore};
use serde_json::json;
use sqlx::Row;

fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn member(group: GroupId, message: i64, account: i64, text: &str) -> NewLine {
    NewLine {
        group,
        message: MessageId::new(message).unwrap(),
        speaker: NewSpeaker::Member(AccountId::new(account).unwrap()),
        at: UnixMillis::new(T0 + message * 1000),
        text: text.into(),
    }
}

async fn ordinal_of(db: &TestDb, group: GroupId, message: i64) -> i64 {
    sqlx::query_scalar("SELECT ordinal FROM chat_line WHERE group_id = $1 AND message_id = $2")
        .bind(group.get())
        .bind(message)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

// ----- archive ordinals and accounts -----

#[tokio::test]
async fn ordinals_are_dense_per_group_even_under_concurrency_and_duplicates_use_none() {
    let db = db!();
    let archive = Arc::new(PgArchive::new(db.pool().clone()));
    let g = group(1);
    let tasks: Vec<_> = (0..30)
        .map(|i| {
            let archive = archive.clone();
            tokio::spawn(async move {
                archive
                    .append(member(g, 100 + i, 500 + i % 5, "hello"))
                    .await
                    .unwrap()
            })
        })
        .collect();
    for task in tasks {
        assert!(matches!(task.await.unwrap(), Appended::Stored { .. }));
    }
    let ordinals: BTreeSet<i64> =
        sqlx::query_scalar("SELECT ordinal FROM chat_line WHERE group_id = $1")
            .bind(g.get())
            .fetch_all(db.pool())
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert_eq!(ordinals, (1..=30).collect::<BTreeSet<i64>>(), "gapless");

    assert_eq!(
        archive
            .append(member(g, 100, 999, "replayed"))
            .await
            .unwrap(),
        Appended::Duplicate
    );
    archive.append(member(g, 200, 501, "next")).await.unwrap();
    assert_eq!(
        ordinal_of(&db, g, 200).await,
        31,
        "a duplicate consumed no ordinal"
    );

    archive
        .append(member(group(2), 1, 501, "other group"))
        .await
        .unwrap();
    assert_eq!(
        ordinal_of(&db, group(2), 1).await,
        1,
        "each group counts from 1"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn a_line_from_a_new_account_creates_its_account_and_holder_atomically() {
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let identity = PgIdentityStore::new(db.pool().clone(), IdentityPolicy::default());
    let g = group(1);
    archive
        .append(member(g, 1, 777, "first words"))
        .await
        .unwrap();
    use qbot_memory::IdentityStore;
    let holder = identity
        .holder_of(AccountId::new(777).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (holder.revision, holder.accounts),
        (0, vec![AccountId::new(777).unwrap()])
    );
    // A duplicate stores nothing, so it creates no account either.
    assert_eq!(
        archive.append(member(g, 1, 888, "replay")).await.unwrap(),
        Appended::Duplicate
    );
    assert!(
        identity
            .holder_of(AccountId::new(888).unwrap())
            .await
            .unwrap()
            .is_none()
    );
    db.drop_db().await;
}

// ----- identity on Postgres -----

#[tokio::test]
async fn the_postgres_identity_store_satisfies_the_identity_contract() {
    let db = db!();
    qbot_memory::identity_conformance::run(&PgIdentityStore::new(
        db.pool().clone(),
        IdentityPolicy::default(),
    ))
    .await;
    db.drop_db().await;
}

#[tokio::test]
async fn identity_stays_consistent_under_concurrency() {
    use qbot_memory::{IdentityError, IdentityStore, LinkError};
    let db = db!();
    let store = Arc::new(PgIdentityStore::new(
        db.pool().clone(),
        IdentityPolicy::default(),
    ));

    // The same account seen by many tasks at once is still one account with one holder.
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .seen(AccountId::new(1).unwrap(), UnixMillis::new(T0))
                    .await
                    .unwrap()
                    .id
            })
        })
        .collect();
    let mut holders = BTreeSet::new();
    for t in tasks {
        holders.insert(t.await.unwrap().0);
    }
    assert_eq!(holders.len(), 1);
    let (holder_count, account_count): (i64, i64) = (
        sqlx::query_scalar("SELECT count(*) FROM holder")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT count(*) FROM account")
            .fetch_one(db.pool())
            .await
            .unwrap(),
    );
    assert_eq!(
        (holder_count, account_count),
        (1, 1),
        "no orphan holders from the race"
    );

    // Of many concurrent invitations touching the same account, exactly one is accepted.
    for n in 2..=9 {
        store
            .seen(AccountId::new(n).unwrap(), UnixMillis::new(T0))
            .await
            .unwrap();
    }
    let g = group(1);
    let tasks: Vec<_> = (2..=9)
        .map(|n| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .invite(
                        g,
                        AccountId::new(1).unwrap(),
                        AccountId::new(n).unwrap(),
                        MessageId::new(n).unwrap(),
                        UnixMillis::new(T0),
                    )
                    .await
            })
        })
        .collect();
    let mut accepted = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => accepted += 1,
            Err(error) => assert_eq!(error, IdentityError::Link(LinkError::Busy)),
        }
    }
    assert_eq!(accepted, 1);

    // Concurrent merges into one person end with every account in the same holder.
    let tasks: Vec<_> = (2..=9)
        .map(|n| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .merge(AccountId::new(1).unwrap(), AccountId::new(n).unwrap())
                    .await
                    .unwrap()
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let holder = store
        .holder_of(AccountId::new(5).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(holder.accounts.len(), 9);
    db.drop_db().await;
}

// ----- episodes on Postgres -----

#[tokio::test]
async fn the_postgres_episode_store_satisfies_the_episode_store_contract() {
    let db = db!();
    qbot_memory::conformance::run(&PgEpisodeStore::new(
        db.pool().clone(),
        ManualClock::new(T0),
    ))
    .await;
    db.drop_db().await;
}

#[tokio::test]
async fn two_workers_extracting_the_same_slice_cannot_both_store_it() {
    use qbot_memory::conformance::episode;
    let db = db!();
    let store = Arc::new(PgEpisodeStore::new(db.pool().clone(), ManualClock::new(T0)));
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .insert(&episode(group(1), 1, 30, "same slice"), &[1.0, 0.0], "m")
                    .await
            })
        })
        .collect();
    let mut stored = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => stored += 1,
            Err(error) => assert_eq!(error, qbot_memory::MemoryError::Overlap),
        }
    }
    assert_eq!(
        stored, 1,
        "the database, not the callers, guarantees exclusivity"
    );
    let embeddings: i64 = sqlx::query_scalar("SELECT count(*) FROM episode_embedding")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        embeddings, 1,
        "a rejected episode leaves no embedding behind"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn pgvector_ranks_by_cosine_distance_in_a_real_exact_scan() {
    use qbot_memory::conformance::episode;
    let db = db!();
    let store = PgEpisodeStore::new(db.pool().clone(), ManualClock::new(T0));
    let g = group(1);
    for (i, v) in [
        [1.0f32, 0.0, 0.0],
        [0.7, 0.7, 0.0],
        [0.0, 1.0, 0.0],
        [-1.0, 0.0, 0.0],
    ]
    .iter()
    .enumerate()
    {
        let (a, b) = (i as u64 * 10 + 1, i as u64 * 10 + 10);
        store
            .insert(&episode(g, a, b, &format!("e{i}")), v, "m")
            .await
            .unwrap();
    }
    let hits = store.search(g, "m", &[1.0, 0.0, 0.0], 2.0).await.unwrap();
    let distances: Vec<f32> = hits.iter().map(|h| h.distance).collect();
    let expected = [0.0, 1.0 - std::f32::consts::FRAC_1_SQRT_2, 1.0, 2.0];
    for (got, want) in distances.iter().zip(expected) {
        assert!((got - want).abs() < 1e-3, "{distances:?}");
    }
    let near: Vec<_> = store
        .search(g, "m", &[1.0, 0.0, 0.0], 0.5)
        .await
        .unwrap()
        .iter()
        .map(|h| h.episode.episode.title.clone())
        .collect();
    assert_eq!(near, ["e0", "e1"]);
    db.drop_db().await;
}

#[tokio::test]
async fn deleting_an_episode_removes_its_embedding_and_lines_carry_member_numbers() {
    use qbot_memory::conformance::episode;
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let store = PgEpisodeStore::new(db.pool().clone(), ManualClock::new(T0));
    let g = group(1);
    archive.append(member(g, 1, 10, "from ten")).await.unwrap();
    archive
        .append(member(g, 2, 20, "from twenty"))
        .await
        .unwrap();
    archive
        .append(NewLine {
            group: g,
            message: MessageId::new(3).unwrap(),
            speaker: NewSpeaker::Bot,
            at: UnixMillis::new(T0),
            text: "bot line".into(),
        })
        .await
        .unwrap();
    archive.append(member(g, 4, 10, "ten again")).await.unwrap();

    let lines = store.lines(g, 2, 4).await.unwrap();
    let shown: Vec<(u64, Option<u32>, bool, &str)> = lines
        .iter()
        .map(|l| (l.ordinal, l.member_no, l.speaker.is_none(), l.text.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            (2, Some(2), false, "from twenty"),
            (3, None, true, "bot line"),
            (4, Some(1), false, "ten again")
        ]
    );
    assert_eq!(store.last_ordinal(g).await.unwrap(), 4);
    assert_eq!(store.last_ordinal(group(9)).await.unwrap(), 0);

    let e = store
        .insert(&episode(g, 1, 4, "x"), &[1.0], "m")
        .await
        .unwrap();
    sqlx::query("DELETE FROM episode WHERE episode_id = $1")
        .bind(e.id.get())
        .execute(db.pool())
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM episode_embedding")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
    db.drop_db().await;
}

// ----- the whole memory pipeline on Postgres -----

fn topic_line(ordinal: u64) -> String {
    let slice = (ordinal - 1) / 20; // 10 lines per batch, 2 batches per slice
    let topic = [
        "deploy pipeline rollback staging",
        "dinner pasta recipe cooking kitchen",
        "holiday coast trip planning beach",
    ][slice as usize % 3];
    format!("line {ordinal} {topic}")
}

fn answer(slice: usize) -> Step {
    let topic = [
        "deploy pipeline rollback staging",
        "dinner pasta recipe cooking kitchen",
        "holiday coast trip planning beach",
    ][slice % 3];
    let first = slice as u64 * 20 + 1;
    Step::Reply(FakeReply::new().call(
        "submit_episode",
        json!({ "title": format!("Talk about {topic}"), "summary": format!("The group discussed {topic}."), "evidence": [{ "line": 2, "quote": format!("line {} {topic}", first + 1) }] }),
    ))
}

#[tokio::test]
async fn lines_become_episodes_which_recall_finds_and_read_episode_opens() {
    let db = db!();
    let clock = ManualClock::new(T0);
    let archive = PgArchive::new(db.pool().clone());
    let g = group(1);
    for n in 1..=100u64 {
        archive
            .append(member(g, n as i64, 10 + (n % 3) as i64, &topic_line(n)))
            .await
            .unwrap();
    }

    let store = Arc::new(PgEpisodeStore::new(db.pool().clone(), clock.clone()));
    let fake = Arc::new(FakeProvider::new((0..6).map(answer).collect::<Vec<_>>()));
    let embedder = Arc::new(FakeEmbedder::new(64));
    let builder = EpisodeBuilder::new(
        EpisodeExtractor::new(fake.clone()),
        embedder.clone(),
        BuilderConfig::default(),
    );
    let grid = SliceGrid {
        grid: BatchGrid {
            lines_per_batch: 10,
        },
        slice_batches: 2,
        previous_context_batches: 1,
        next_context_batches: 1,
        retained_raw_batches: 10,
    };
    let job = EpisodeJobs::new(store.clone(), builder, grid);

    assert_eq!(
        job.extract_group(g).await.unwrap(),
        5,
        "100 lines = 5 slices of 20"
    );
    assert_eq!(job.extract_group(g).await.unwrap(), 0, "idempotent");
    let episodes = store.within(g, 1, 100).await.unwrap();
    let ranges: Vec<_> = episodes
        .iter()
        .map(|e| (e.episode.first_ordinal, e.episode.last_ordinal))
        .collect();
    assert_eq!(ranges, [(1, 20), (21, 40), (41, 60), (61, 80), (81, 100)]);
    assert!(
        episodes
            .iter()
            .all(|e| e.episode.batch_lines == 10 && e.episode.line_count == 20)
    );

    // Recall finds the right episode by meaning.
    let recall = Arc::new(Recall::new(
        store.clone(),
        embedder.clone(),
        Arc::new(qbot_core::SystemClock),
        RecallParams {
            limit: 3,
            max_distance: 0.9,
            // Similarity alone: this checks relevance.
            half_life: std::time::Duration::ZERO,
        },
    ));
    let hits = recall
        .recall(g, "why did the deploy pipeline need a rollback")
        .await
        .unwrap();
    assert!(
        hits[0].episode.episode.title.contains("deploy"),
        "{:?}",
        hits.iter()
            .map(|h| &h.episode.episode.title)
            .collect::<Vec<_>>()
    );
    assert!(
        recall
            .recall(group(2), "deploy pipeline")
            .await
            .unwrap()
            .is_empty(),
        "another group sees nothing"
    );

    // The tools, run the way the agent runtime runs them.
    let set = qbot_tools::add_memory_tools(ToolSet::new(), recall, store.clone()).unwrap();
    let trigger = Trigger::Addressed {
        message: MessageId::new(1).unwrap(),
        sender: AccountId::new(10).unwrap(),
    };
    let (view, state, clock2) = (ChatView::default(), RunState::default(), SystemClock);
    let cx = ToolCx {
        group: g,
        run: RunId::new(1),
        trigger: &trigger,
        view: &view,
        state: &state,
        clock: &clock2,
    };
    let found = set
        .get("recall_episodes")
        .unwrap()
        .call(&cx, json!({ "question": "holiday trip to the coast" }))
        .await
        .unwrap();
    let text = found
        .content
        .iter()
        .filter_map(|p| {
            if let qbot_context::Part::Text(t) = p {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect::<String>();
    assert!(
        text.starts_with("episode ") && text.contains("holiday"),
        "{text}"
    );
    let id: i64 = text.split_whitespace().nth(1).unwrap().parse().unwrap();

    let opened = set
        .get("read_episode")
        .unwrap()
        .call(&cx, json!({ "id": id }))
        .await
        .unwrap();
    let body = opened
        .content
        .iter()
        .filter_map(|p| {
            if let qbot_context::Part::Text(t) = p {
                Some(t.as_str())
            } else {
                None
            }
        })
        .collect::<String>();
    assert_eq!(
        body.lines().count(),
        21,
        "the summary line plus the 20 lines of exactly that slice"
    );
    assert!(body.contains("holiday coast trip") && body.contains("member:"));
    assert!(
        !body.contains("deploy pipeline"),
        "an episode's lines are only its own range"
    );

    let missing = set
        .get("read_episode")
        .unwrap()
        .call(&cx, json!({ "id": 999_999 }))
        .await
        .unwrap_err();
    assert!(matches!(missing, qbot_agent::ToolError::Refused { .. }));
    let other = ToolCx {
        group: group(2),
        ..cx
    };
    assert!(
        set.get("read_episode")
            .unwrap()
            .call(&other, json!({ "id": id }))
            .await
            .is_err(),
        "episodes are group-scoped"
    );
    let none = set
        .get("recall_episodes")
        .unwrap()
        .call(&other, json!({ "question": "anything" }))
        .await
        .unwrap();
    assert!(matches!(&none.content[0], qbot_context::Part::Text(t) if t == "no matching episodes"));

    let _ = (
        MemorySink::new(),
        sqlx::query("SELECT 1")
            .fetch_one(db.pool())
            .await
            .unwrap()
            .get::<i32, _>(0),
    );
    db.drop_db().await;
}

#[tokio::test]
async fn the_postgres_fact_store_satisfies_the_fact_contract() {
    use qbot_memory::IdentityStore;
    let db = db!();
    let identity = PgIdentityStore::new(db.pool().clone(), IdentityPolicy::default());
    for account in [501, 502] {
        identity
            .seen(AccountId::new(account).unwrap(), UnixMillis::new(T0))
            .await
            .unwrap();
    }
    qbot_memory::facts_conformance::run(
        &qbot_store::PgFactStore::new(db.pool().clone()),
        &[501, 502],
    )
    .await;
    db.drop_db().await;
}

#[tokio::test]
async fn the_postgres_note_store_satisfies_the_note_contract() {
    use qbot_memory::IdentityStore;
    let db = db!();
    let identity = PgIdentityStore::new(db.pool().clone(), IdentityPolicy::default());
    let accounts = [601, 602, 603].map(|n| AccountId::new(n).unwrap());
    for account in accounts {
        identity.seen(account, UnixMillis::new(T0)).await.unwrap();
    }
    qbot_memory::notes_conformance::run(&qbot_store::PgNoteStore::new(db.pool().clone()), accounts)
        .await;
    db.drop_db().await;
}

// ----- member numbers and the people of a group -----

/// Each account's number in `g`, read straight from the table.
async fn numbers(db: &TestDb, g: GroupId) -> Vec<(i64, i32)> {
    sqlx::query(
        "SELECT account_id, number FROM member_number WHERE group_id = $1 ORDER BY account_id",
    )
    .bind(g.get())
    .fetch_all(db.pool())
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get("account_id"), r.get("number")))
    .collect()
}

#[tokio::test]
async fn a_member_number_survives_everything_that_happens_to_its_account() {
    use qbot_memory::{AliasTarget, IdentityStore};
    let db = db!();
    let clock = ManualClock::new(T0);
    let archive = PgArchive::new(db.pool().clone());
    let identity = PgIdentityStore::new(db.pool().clone(), IdentityPolicy::default());
    let policy = qbot_store::PgGroupPolicy::new(db.pool().clone(), clock.clone());
    let (g, other) = (group(50), group(51));
    let account = |n: i64| AccountId::new(n).unwrap();

    // 601 speaks first, 603 is mentioned before it ever speaks, 602 speaks after that.
    archive.append(member(g, 1, 601, "hi")).await.unwrap();
    archive
        .append_with_mentions(member(g, 2, 601, "hey [at:603]"), &[account(603)])
        .await
        .unwrap();
    archive.append(member(g, 3, 602, "hello")).await.unwrap();
    let assigned = numbers(&db, g).await;
    assert_eq!(assigned, [(601, 1), (602, 3), (603, 2)]);

    // None of these may move a number.
    archive
        .append(member(g, 3, 604, "replayed id"))
        .await
        .unwrap();
    archive
        .append(member(other, 1, 603, "elsewhere"))
        .await
        .unwrap();
    identity.merge(account(601), account(602)).await.unwrap();
    identity
        .split(account(602), UnixMillis::new(T0))
        .await
        .unwrap();
    identity.merge(account(601), account(603)).await.unwrap();
    policy.block(g, account(602), None).await.unwrap();
    policy.unblock(g, account(602)).await.unwrap();
    identity
        .set_name(
            g,
            "Tester",
            AliasTarget::Account(account(601)),
            UnixMillis::new(T0),
        )
        .await
        .unwrap();
    for i in 0..5 {
        archive
            .append(member(g, 10 + i, 601, "more"))
            .await
            .unwrap();
    }
    assert_eq!(numbers(&db, g).await, assigned);
    assert_eq!(
        numbers(&db, other).await,
        [(603, 1)],
        "numbers are per group"
    );

    // A fresh archive (as after a restart) reads the same numbers.
    let restarted = PgArchive::new(db.pool().clone());
    let (lines, _) = restarted.recent(g, 50).await.unwrap();
    assert!(lines.iter().all(|l| match l.speaker {
        qbot_context::Speaker::Member { account, number } =>
            assigned.contains(&(account.get(), number.get() as i32)),
        qbot_context::Speaker::Bot => true,
    }));
    db.drop_db().await;
}

#[tokio::test]
async fn people_shows_current_blocks_and_linked_accounts_by_member_number() {
    use qbot_core::MemberNo;
    use qbot_memory::IdentityStore;
    let db = db!();
    let clock = ManualClock::new(T0);
    let archive = PgArchive::new(db.pool().clone());
    let identity = PgIdentityStore::new(db.pool().clone(), IdentityPolicy::default());
    let policy = qbot_store::PgGroupPolicy::new(db.pool().clone(), clock.clone());
    let g = group(60);
    let account = |n: i64| AccountId::new(n).unwrap();
    let no = |n: u32| MemberNo::new(n);
    for (i, a) in [701, 702, 703, 704, 705].into_iter().enumerate() {
        archive
            .append(member(g, i as i64 + 1, a, "hi"))
            .await
            .unwrap();
    }
    // 706 speaks only in another group: no number here.
    archive
        .append(member(group(61), 1, 706, "hi"))
        .await
        .unwrap();

    let now = UnixMillis::new(T0);
    assert_eq!(archive.people(g, now).await.unwrap(), Default::default());

    policy.block(g, account(704), None).await.unwrap();
    policy
        .block(g, account(702), Some(UnixMillis::new(T0 + 60_000)))
        .await
        .unwrap();
    policy.block(g, account(706), None).await.unwrap();
    policy.block(group(61), account(701), None).await.unwrap();
    identity.merge(account(705), account(703)).await.unwrap();
    identity.merge(account(701), account(706)).await.unwrap();

    let people = archive.people(g, now).await.unwrap();
    assert_eq!(people.blocked, [no(2), no(4)], "ascending, this group only");
    assert_eq!(
        people.same_person,
        [vec![no(3), no(5)]],
        "a linked account without a number here is not shown"
    );

    let later = UnixMillis::new(T0 + 60_001);
    identity.merge(account(701), account(702)).await.unwrap();
    let people = archive.people(g, later).await.unwrap();
    assert_eq!(people.blocked, [no(4)], "an expired block is gone");
    assert_eq!(people.same_person, [vec![no(1), no(2)], vec![no(3), no(5)]]);

    identity.split(account(703), later).await.unwrap();
    assert_eq!(
        archive.people(g, later).await.unwrap().same_person,
        [vec![no(1), no(2)]],
        "unlinking ends the set"
    );
    db.drop_db().await;
}
