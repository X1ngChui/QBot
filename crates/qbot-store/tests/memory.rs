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
        platform_seq: None,
        speaker: NewSpeaker::Member(AccountId::new(account).unwrap()),
        at: UnixMillis::new(T0 + message * 1000),
        text: text.into(),
        sender_name: None,
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
async fn a_line_from_a_new_account_creates_its_account_atomically() {
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let identity = PgIdentityStore::new(db.pool().clone(), IdentityPolicy::default());
    let g = group(1);
    archive
        .append(member(g, 1, 777, "first words"))
        .await
        .unwrap();
    use qbot_memory::IdentityStore;
    assert!(
        identity
            .is_known(AccountId::new(777).unwrap())
            .await
            .unwrap()
    );
    // A duplicate stores nothing, so it creates no account either.
    assert_eq!(
        archive.append(member(g, 1, 888, "replay")).await.unwrap(),
        Appended::Duplicate
    );
    assert!(
        !identity
            .is_known(AccountId::new(888).unwrap())
            .await
            .unwrap()
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

    // The same account seen by many tasks at once is still one account.
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .seen(AccountId::new(1).unwrap(), UnixMillis::new(T0))
                    .await
                    .unwrap()
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM account")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(accounts, 1);

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

    // Concurrent links into one person end with every account linked once.
    let tasks: Vec<_> = (2..=9)
        .flat_map(|n| [(1, n), (n, 1)])
        .map(|(a, b)| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .link(
                        g,
                        AccountId::new(a).unwrap(),
                        AccountId::new(b).unwrap(),
                        UnixMillis::new(T0),
                    )
                    .await
                    .unwrap()
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(
        store
            .linked(g, AccountId::new(5).unwrap())
            .await
            .unwrap()
            .len(),
        9
    );
    let links: i64 = sqlx::query_scalar("SELECT count(*) FROM account_link")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(links, 8, "a link raced from both sides is stored once");
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
            platform_seq: None,
            speaker: NewSpeaker::Bot,
            at: UnixMillis::new(T0),
            text: "bot line".into(),
            sender_name: None,
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

    // Quoted lines: the newest with each id before the given ordinal, oldest first, own group
    // only; an id with no line, or only lines from that ordinal on, gives nothing.
    archive
        .append(member(group(9), 2, 30, "another group"))
        .await
        .unwrap();
    let id = |n: i64| MessageId::new(n).unwrap();
    let quoted = store
        .quoted(g, &[id(3), id(1), id(77), id(4)], 4)
        .await
        .unwrap();
    let shown: Vec<(u64, &str)> = quoted
        .iter()
        .map(|l| (l.ordinal, l.text.as_str()))
        .collect();
    assert_eq!(shown, [(1, "from ten"), (3, "bot line")]);
    assert!(store.quoted(g, &[], 4).await.unwrap().is_empty());
    assert_eq!(
        store.quoted(group(9), &[id(2)], 10).await.unwrap()[0].text,
        "another group"
    );

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
    let set = qbot_tools::add_memory_tools(ToolSet::new(), recall, store.clone(), None).unwrap();
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
        renderer: &qbot_llm::PlainRenderer,
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
async fn the_postgres_name_and_memory_stores_satisfy_their_contracts() {
    use qbot_memory::IdentityStore;
    let db = db!();
    let identity = PgIdentityStore::new(db.pool().clone(), IdentityPolicy::default());
    for account in 1..=40 {
        identity
            .seen(AccountId::new(account).unwrap(), UnixMillis::new(T0))
            .await
            .unwrap();
    }
    qbot_memory::names_conformance::run(&qbot_store::PgNameStore::new(db.pool().clone())).await;
    qbot_memory::memories_conformance::run(&qbot_store::PgMemoryStore::new(db.pool().clone()))
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
    use qbot_memory::IdentityStore;
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
    let now = UnixMillis::new(T0);
    identity
        .link(g, account(601), account(602), now)
        .await
        .unwrap();
    identity.unlink(g, account(602)).await.unwrap();
    identity
        .link(g, account(601), account(603), now)
        .await
        .unwrap();
    policy.block(g, account(602), None).await.unwrap();
    policy.unblock(g, account(602)).await.unwrap();
    {
        use qbot_memory::NameStore;
        qbot_store::PgNameStore::new(db.pool().clone())
            .set(g, "Tester", &[account(601)], UnixMillis::new(T0))
            .await
            .unwrap();
    }
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
async fn the_roster_lists_who_is_who_in_its_group_only() {
    use qbot_memory::{EpisodeId, IdentityStore, NameStore, Reference};
    use qbot_store::{PgNameStore, RosterSettings};
    let db = db!();
    let clock = ManualClock::new(T0);
    let archive = PgArchive::new(db.pool().clone());
    let identity = PgIdentityStore::new(db.pool().clone(), IdentityPolicy::default());
    let policy = qbot_store::PgGroupPolicy::new(db.pool().clone(), clock.clone());
    let names = PgNameStore::new(db.pool().clone());
    let (g, other) = (group(60), group(61));
    let account = |n: i64| AccountId::new(n).unwrap();
    const DAY: i64 = 86_400_000;
    let named = |g: GroupId, message: i64, account: i64, at: i64, name: &str| NewLine {
        sender_name: Some(name.into()),
        at: UnixMillis::new(at),
        ..member(g, message, account, "hi")
    };
    for (i, (a, name)) in [
        (701, "Ann"),
        (702, "Bo"),
        (703, "Cy"),
        (704, "Di"),
        (705, "Ed"),
    ]
    .into_iter()
    .enumerate()
    {
        let i = i as i64;
        archive
            .append(named(g, i + 1, a, T0 + i, name))
            .await
            .unwrap();
    }
    // 705 is shown as something else from the next day; 706 speaks only in the other group, and
    // 701 is shown differently there.
    archive
        .append(named(g, 6, 705, T0 + DAY, "Eddie"))
        .await
        .unwrap();
    archive
        .append(named(other, 1, 706, T0, "Fay"))
        .await
        .unwrap();
    archive
        .append(named(other, 2, 701, T0, "Ann elsewhere"))
        .await
        .unwrap();
    let settings = RosterSettings {
        recent_ms: 30 * DAY,
        max_people: 10,
    };
    let shape = |people: &qbot_store::GroupPeople| {
        people
            .people
            .iter()
            .map(|p| {
                (
                    p.members.iter().map(|(n, _)| n.get()).collect::<Vec<_>>(),
                    p.shown.iter().map(|s| s.text.clone()).collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    let now = UnixMillis::new(T0 + 3 * DAY);
    // A line sent today counts from tomorrow: the roster changes by activity once a day.
    archive
        .append(named(g, 7, 705, now.get() - 1, "Edward"))
        .await
        .unwrap();
    let people = archive.people(g, now, settings, &[]).await.unwrap();
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        shape(&people),
        [
            (vec![1], s(&["Ann"])),
            (vec![2], s(&["Bo"])),
            (vec![3], s(&["Cy"])),
            (vec![4], s(&["Di"])),
            (vec![5], s(&["Eddie", "Ed"])),
        ],
        "what each member is shown as, the latest first, from this group's lines only"
    );

    // Names people use, each under its member, only this group's.
    names.set(g, "Boss", &[account(702)], now).await.unwrap();
    names
        .set(other, "Elsewhere", &[account(701)], now)
        .await
        .unwrap();
    let used = |episode: i64| {
        Reference::of(
            account(701),
            account(702),
            "Annie",
            EpisodeId::new(episode),
            MessageId::new(episode).unwrap(),
            now,
        )
    };
    names.record(g, &[used(1), used(2)]).await.unwrap();
    identity
        .link(g, account(701), account(702), now)
        .await
        .unwrap();
    let people = archive.people(g, now, settings, &[]).await.unwrap();
    let pair = &people.people[0];
    assert_eq!(
        pair.members
            .iter()
            .map(|(n, _)| n.get())
            .collect::<Vec<_>>(),
        [1, 2]
    );
    let keys: Vec<(&str, bool, u32)> = pair
        .names
        .iter()
        .map(|n| (n.key.as_str(), n.set, n.uses()))
        .collect();
    assert_eq!(
        keys,
        [("boss", true, 0), ("annie", false, 2)],
        "set names first"
    );
    assert_eq!(
        people.names.len(),
        2,
        "every name of this group, with its member"
    );

    // Weeks later nobody spoke recently: only linked, blocked and named people are listed, and
    // anyone asked for.
    let later = UnixMillis::new(T0 + 40 * DAY);
    policy.block(g, account(704), None).await.unwrap();
    names.set(g, "Cee", &[account(703)], later).await.unwrap();
    let members = |people: &qbot_store::GroupPeople| {
        people
            .people
            .iter()
            .map(|p| p.members.iter().map(|(n, _)| n.get()).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };
    let quiet = archive.people(g, later, settings, &[]).await.unwrap();
    assert_eq!(members(&quiet), [vec![1, 2], vec![3], vec![4]]);
    assert_eq!(quiet.people[2].blocked, [qbot_core::MemberNo::new(4)]);
    let asked = archive
        .people(g, later, settings, &[account(705)])
        .await
        .unwrap();
    assert_eq!(members(&asked), [vec![1, 2], vec![3], vec![4], vec![5]]);

    // A bound: of those who only spoke, the most recently active are kept; those with a record
    // and those asked for always are.
    let one = RosterSettings {
        max_people: 1,
        ..settings
    };
    archive
        .append(named(g, 8, 704, now.get() - 2 * DAY, "Di"))
        .await
        .unwrap();
    let busy = archive.people(g, now, one, &[account(705)]).await.unwrap();
    assert_eq!(
        members(&busy),
        [vec![1, 2], vec![3], vec![4], vec![5]],
        "linked, named and blocked people stay; 705 was asked for"
    );
    archive
        .append(named(other, 3, 707, now.get() - 2 * DAY, "Gil"))
        .await
        .unwrap();
    for (i, a) in [708, 709].into_iter().enumerate() {
        archive
            .append(named(
                g,
                20 + i as i64,
                a,
                now.get() - 2 * DAY + i as i64,
                "new",
            ))
            .await
            .unwrap();
    }
    let bounded = archive.people(g, now, one, &[]).await.unwrap();
    let only_active: Vec<Vec<u32>> = members(&bounded)
        .into_iter()
        .filter(|m| m.iter().all(|n| *n > 5))
        .collect();
    assert_eq!(
        only_active.len(),
        1,
        "one of the people who only spoke: {only_active:?}"
    );
    assert_eq!(
        archive.people(g, now, settings, &[]).await.unwrap(),
        archive.people(g, now, settings, &[]).await.unwrap(),
        "the same records give the same roster"
    );
    db.drop_db().await;
}
