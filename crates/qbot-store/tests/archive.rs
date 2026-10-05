#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use qbot_agent::{Archive, ArchiveCursor};
use qbot_context::Speaker;
use qbot_core::{AccountId, GroupId, MediaKind, MemberNo, MessageId, UnixMillis};
use qbot_store::{Appended, NewLine, NewSpeaker, PgArchive, PgGroupPolicy};

fn group(n: i64) -> GroupId {
    GroupId::new(n).unwrap()
}

fn member(group: GroupId, message: i64, account: i64, text: &str) -> NewLine {
    NewLine {
        group,
        message: MessageId::new(message).unwrap(),
        speaker: NewSpeaker::Member(AccountId::new(account).unwrap()),
        at: UnixMillis::new(T0 + message),
        text: text.into(),
    }
}

/// A parsed `search_history` query over the whole group.
fn q(text: &str) -> qbot_agent::HistoryQuery {
    qbot_agent::HistoryQuery {
        text: qbot_tools::parse_query(text).unwrap(),
        speaker: None,
    }
}

fn number_of(line: &qbot_context::ChatLine) -> u32 {
    match line.speaker {
        Speaker::Member { number, .. } => number.get(),
        Speaker::Bot => 0,
    }
}

#[tokio::test]
async fn members_get_stable_dense_numbers_per_group_on_first_appearance() {
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let g = group(100);
    let first = archive.append(member(g, 1, 501, "hi")).await.unwrap();
    let second = archive.append(member(g, 2, 502, "hello")).await.unwrap();
    let again = archive.append(member(g, 3, 501, "me again")).await.unwrap();
    let (
        Appended::Stored { line: a, .. },
        Appended::Stored { line: b, .. },
        Appended::Stored { line: c, .. },
    ) = (first, second, again)
    else {
        panic!("expected stored lines")
    };
    assert_eq!((number_of(&a), number_of(&b), number_of(&c)), (1, 2, 1));

    // Numbers are per group: the same account is member 1 of another group too.
    let Appended::Stored { line: other, .. } = archive
        .append(member(group(200), 1, 502, "x"))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(number_of(&other), 1);
    db.drop_db().await;
}

#[tokio::test]
async fn concurrent_first_appearances_get_unique_gapless_numbers() {
    let db = db!();
    let archive = Arc::new(PgArchive::new(db.pool().clone()));
    let g = group(300);
    let tasks: Vec<_> = (0..24)
        .map(|i| {
            let archive = archive.clone();
            tokio::spawn(async move {
                archive
                    .append(member(g, 1000 + i, 9000 + i, "hello"))
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut numbers = BTreeSet::new();
    for task in tasks {
        let Appended::Stored { line, .. } = task.await.unwrap() else {
            panic!()
        };
        numbers.insert(number_of(&line));
    }
    assert_eq!(
        numbers,
        (1..=24).collect::<BTreeSet<u32>>(),
        "every number used exactly once, no gaps"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn a_duplicate_message_stores_nothing_and_consumes_no_number() {
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let g = group(400);
    assert!(matches!(
        archive.append(member(g, 1, 501, "once")).await.unwrap(),
        Appended::Stored { .. }
    ));
    assert_eq!(
        archive
            .append(member(g, 1, 777, "replayed by the platform"))
            .await
            .unwrap(),
        Appended::Duplicate
    );
    let Appended::Stored { line, .. } = archive.append(member(g, 2, 502, "next")).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(
        number_of(&line),
        2,
        "the duplicate's account never got a number"
    );
    let (lines, _) = archive.recent(g, 10).await.unwrap();
    assert_eq!(lines.len(), 2);
    db.drop_db().await;
}

#[tokio::test]
async fn since_and_recent_follow_the_cursor_and_isolate_groups() {
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let (g, h) = (group(500), group(501));
    for i in 1..=5 {
        archive
            .append(member(g, i, 600, &format!("g line {i}")))
            .await
            .unwrap();
    }
    archive
        .append(member(h, 1, 600, "other group"))
        .await
        .unwrap();
    archive
        .append(NewLine {
            group: g,
            message: MessageId::new(6).unwrap(),
            speaker: NewSpeaker::Bot,
            at: UnixMillis::new(T0),
            text: "bot reply".into(),
        })
        .await
        .unwrap();

    let (recent, cursor) = archive.recent(g, 3).await.unwrap();
    let texts: Vec<_> = recent.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        ["g line 4", "g line 5", "bot reply"],
        "the last three, oldest first"
    );
    assert!(matches!(recent[2].speaker, Speaker::Bot));

    assert!(archive.since(g, cursor).await.unwrap().is_empty());
    archive.append(member(g, 7, 601, "new")).await.unwrap();
    let fresh = archive.since(g, cursor).await.unwrap();
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].line.text, "new");
    assert!(fresh[0].seq > cursor);
    assert!(
        archive
            .since(h, ArchiveCursor(0))
            .await
            .unwrap()
            .iter()
            .all(|a| a.line.text == "other group")
    );
    db.drop_db().await;
}

#[tokio::test]
async fn search_is_case_insensitive_literal_newest_first_and_group_scoped() {
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let g = group(600);
    archive
        .append(member(g, 1, 700, "The Deploy failed"))
        .await
        .unwrap();
    archive
        .append(member(g, 2, 700, "100% sure it is fixed_now"))
        .await
        .unwrap();
    archive
        .append(member(g, 3, 700, "deploy is fixed"))
        .await
        .unwrap();
    archive
        .append(member(group(601), 1, 700, "deploy elsewhere"))
        .await
        .unwrap();

    let hits: Vec<_> = archive
        .search(g, &q("DEPLOY"), 10)
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert_eq!(hits, ["deploy is fixed", "The Deploy failed"]);
    assert_eq!(
        archive.search(g, &q("deploy"), 1).await.unwrap().len(),
        1,
        "limit applies"
    );
    assert_eq!(
        archive.search(g, &q("100%"), 10).await.unwrap().len(),
        1,
        "% is a literal character"
    );
    assert_eq!(
        archive.search(g, &q("fixed_now"), 10).await.unwrap().len(),
        1,
        "_ is a literal character"
    );
    assert!(
        archive
            .search(g, &q("fixedXnow"), 10)
            .await
            .unwrap()
            .is_empty(),
        "_ is not a wildcard"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn a_block_never_removes_lines_or_changes_numbers() {
    let db = db!();
    let clock = ManualClock::new(T0);
    let archive = PgArchive::new(db.pool().clone());
    let policy = PgGroupPolicy::new(db.pool().clone(), clock.clone());
    let g = group(700);
    archive
        .append(member(g, 1, 800, "before the block"))
        .await
        .unwrap();
    policy
        .block(g, AccountId::new(800).unwrap(), None)
        .await
        .unwrap();
    archive
        .append(member(g, 2, 800, "while blocked"))
        .await
        .unwrap();
    archive
        .append(member(g, 3, 801, "someone else"))
        .await
        .unwrap();

    let (lines, _) = archive.recent(g, 10).await.unwrap();
    assert_eq!(lines.len(), 3, "a block never removes lines");
    let numbers: Vec<u32> = lines.iter().map(number_of).collect();
    assert_eq!(numbers, [1, 1, 2]);
    db.drop_db().await;
}

#[tokio::test]
async fn mentions_get_member_numbers_and_the_marker_is_rewritten() {
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let g = group(300);
    archive.append(member(g, 1, 501, "hi")).await.unwrap();
    let mentioned = [AccountId::new(777).unwrap(), AccountId::new(501).unwrap()];
    let Appended::Stored { line, .. } = archive
        .append_with_mentions(
            member(g, 2, 502, "[at:777] and [at:501] and [at:bot]"),
            &mentioned,
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    // 501 spoke first (1), 502 is the author of this line (assigned after the mentions: 2 for the
    // mentioned 777, then the author), so numbers are dense in order of first use.
    assert_eq!(line.text, "[at:2] and [at:1] and [at:bot]");
    // The mentioned-but-silent account now has a number; speaking later reuses it.
    let Appended::Stored { line: later, .. } =
        archive.append(member(g, 3, 777, "here")).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(number_of(&later), 2);
    // A duplicate does not consume numbers.
    let dup = archive
        .append_with_mentions(
            member(g, 2, 502, "[at:888]"),
            &[AccountId::new(888).unwrap()],
        )
        .await
        .unwrap();
    assert_eq!(dup, Appended::Duplicate);
    let Appended::Stored { line: next, .. } = archive.append(member(g, 4, 889, "x")).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(
        number_of(&next),
        4,
        "502 took 3; 888 was rolled back with the duplicate"
    );
}

#[tokio::test]
async fn logical_queries_run_as_parameterized_sql_on_postgres() {
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let g = group(620);
    let printer = "\u{6253}\u{5370}\u{673a}";
    let copier = "\u{590d}\u{5370}";
    let lines = [
        (1, 701, format!("{printer}\u{53c8}\u{574f}\u{4e86}")),
        (
            2,
            702,
            format!("{printer}\u{548c}{copier}\u{90fd}\u{4e0d}\u{884c}"),
        ),
        (3, 701, "the printer is jammed again".to_owned()),
        (4, 703, "see you at eight, bring the scanner".to_owned()),
        (
            5,
            702,
            "Robert'); DROP TABLE chat_line;-- said hi".to_owned(),
        ),
    ];
    for (message, account, text) in &lines {
        archive
            .append(member(g, *message, *account, text))
            .await
            .unwrap();
    }
    let texts = |found: Vec<qbot_context::ChatLine>| -> Vec<i64> {
        found.iter().map(|l| l.message.get()).collect()
    };
    let run = |query: qbot_agent::HistoryQuery| {
        let archive = archive.clone();
        async move { archive.search(g, &query, 10).await.unwrap() }
    };

    // Chinese terms are substrings: no word splitting is needed to find them.
    assert_eq!(texts(run(q(printer)).await), [2, 1]);
    assert_eq!(
        texts(run(q(&format!("({printer} OR printer) -{copier}"))).await),
        [3, 1],
        "OR, grouping and exclusion"
    );
    assert_eq!(
        texts(run(q("printer jammed")).await),
        [3],
        "spaces mean AND"
    );
    assert!(run(q("printer scanner")).await.is_empty());
    assert_eq!(texts(run(q("\"see you at\" OR jammed")).await), [4, 3]);
    assert!(
        run(q("\"see at\"")).await.is_empty(),
        "a phrase is matched whole"
    );

    // Speaker filtering by member number (701 was numbered first).
    let mut by_speaker = q(&format!("{printer} OR printer"));
    by_speaker.speaker = Some(MemberNo::new(1));
    assert_eq!(texts(run(by_speaker).await), [3, 1]);

    // Terms are bound, never spliced: SQL in a query is just text to look for.
    assert_eq!(texts(run(q("\"'); DROP TABLE chat_line;--\"")).await), [5]);
    assert_eq!(
        archive.last_ordinal(g).await.unwrap(),
        5,
        "the table is still there"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn lines_are_read_from_an_ordinal_with_their_ordinals() {
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let g = group(400);
    assert_eq!(archive.last_ordinal(g).await.unwrap(), 0);
    assert!(archive.lines_from(g, 1).await.unwrap().0.is_empty());
    for n in 1..=13 {
        let appended = archive
            .append(member(g, n, 501, &format!("line {n}")))
            .await
            .unwrap();
        assert!(matches!(appended, Appended::Stored { ordinal, .. } if ordinal == n as u64));
    }
    assert_eq!(archive.last_ordinal(g).await.unwrap(), 13);
    let (lines, cursor) = archive.lines_from(g, 5).await.unwrap();
    let got: Vec<_> = lines.iter().map(|(o, l)| (*o, l.text.as_str())).collect();
    assert_eq!(got.first(), Some(&(5, "line 5")));
    assert_eq!(got.last(), Some(&(13, "line 13")));
    assert_eq!(got.len(), 9);
    assert!(cursor.0 > 0);
}

#[tokio::test]
async fn a_stored_line_can_be_rewritten_under_a_lock() {
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let g = group(500);
    archive
        .append(member(g, 1, 501, "look [image] and [voice]"))
        .await
        .unwrap();
    let message = MessageId::new(1).unwrap();
    assert!(
        archive
            .rewrite_text(g, message, |t| Some(t.replace("[image]", "[image:a cat]")))
            .await
            .unwrap()
    );
    assert!(
        archive
            .rewrite_text(g, message, |t| Some(t.replace("[voice]", "[voice:hello]")))
            .await
            .unwrap()
    );
    assert!(
        !archive.rewrite_text(g, message, |_| None).await.unwrap(),
        "no edit, no change"
    );
    assert!(
        !archive
            .rewrite_text(g, message, |t| Some(t.to_owned()))
            .await
            .unwrap(),
        "same text is not a change"
    );
    assert!(
        !archive
            .rewrite_text(g, MessageId::new(99).unwrap(), |_| Some("x".into()))
            .await
            .unwrap(),
        "unknown line"
    );
    let (lines, _) = archive.recent(g, 5).await.unwrap();
    assert_eq!(lines[0].text, "look [image:a cat] and [voice:hello]");
}

#[tokio::test]
async fn the_media_cache_stores_replaces_and_expires_descriptions() {
    use qbot_store::PgMediaCache;
    let db = db!();
    let clock = ManualClock::new(T0);
    let cache = PgMediaCache::new(db.pool().clone(), clock.clone());
    assert_eq!(cache.get("p:a").await.unwrap(), None);
    cache.put("p:a", "a cat").await.unwrap();
    cache.put("h:z", "a cat").await.unwrap();
    assert_eq!(cache.get("p:a").await.unwrap().as_deref(), Some("a cat"));
    clock.advance(Duration::from_secs(100));
    cache.put("p:a", "a tabby cat").await.unwrap();
    assert_eq!(
        cache.get("p:a").await.unwrap().as_deref(),
        Some("a tabby cat")
    );
    assert_eq!(
        cache.expire(UnixMillis::new(T0 + 50_000)).await.unwrap(),
        1,
        "only the older entry"
    );
    assert_eq!(cache.get("h:z").await.unwrap(), None);
    assert!(cache.get("p:a").await.unwrap().is_some());
}

#[tokio::test]
async fn media_references_are_stored_with_the_line_and_found_by_position() {
    use qbot_store::MediaRefRow;
    let db = db!();
    let archive = PgArchive::new(db.pool().clone());
    let g = group(600);
    let row = |kind: MediaKind, index: u32, key: &str| MediaRefRow {
        kind,
        index,
        key: Some(key.into()),
        file: Some(format!("{key}.jpg")),
        url: Some(format!("https://example.invalid/{key}")),
        size: Some(1234),
    };
    let stored = archive
        .append_line(
            member(g, 1, 501, "[image][image][sticker]"),
            &[],
            &[
                row(MediaKind::Image, 0, "a"),
                row(MediaKind::Image, 1, "b"),
                row(MediaKind::Sticker, 0, "s"),
            ],
        )
        .await
        .unwrap();
    assert!(matches!(stored, Appended::Stored { .. }));
    let found = archive
        .media_ref(g, MessageId::new(1).unwrap(), MediaKind::Image, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((found.key.as_deref(), found.size), (Some("b"), Some(1234)));
    assert!(
        archive
            .media_ref(g, MessageId::new(1).unwrap(), MediaKind::Image, 2)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        archive
            .media_ref(g, MessageId::new(1).unwrap(), MediaKind::Sticker, 0)
            .await
            .unwrap()
            .unwrap()
            .key
            .as_deref(),
        Some("s")
    );

    // A redelivered event stores nothing twice, and does not fail on the references.
    let again = archive
        .append_line(
            member(g, 1, 501, "[image]"),
            &[],
            &[row(MediaKind::Image, 0, "a")],
        )
        .await
        .unwrap();
    assert_eq!(again, Appended::Duplicate);
}
