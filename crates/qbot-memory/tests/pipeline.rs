#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_llm::{ConvItem, Embedder, FakeEmbedder, LlmError, Provider};
use qbot_memory::{
    Background, BatchReader, EpisodeJobs, EpisodeStore, METHOD, MemoryEpisodeStore, SliceLine,
    Stretch, check_episodes,
};
use serde_json::json;

fn group() -> GroupId {
    GroupId::new(1).unwrap()
}

fn line(ordinal: u64, text: &str) -> SliceLine {
    SliceLine {
        ordinal,
        message: MessageId::new(1000 + ordinal as i64).unwrap(),
        speaker: AccountId::new(10 + (ordinal % 3) as i64).ok(),
        member_no: Some((ordinal % 3) as u32 + 1),
        at: UnixMillis::new(1_800_000_000_000 + ordinal as i64 * 1000),
        text: text.into(),
        shown: None,
    }
}

fn lines(range: std::ops::RangeInclusive<u64>) -> Vec<SliceLine> {
    range
        .map(|n| line(n, &format!("line {n} about the deploy pipeline")))
        .collect()
}

#[tokio::test]
async fn the_in_memory_store_satisfies_the_episode_store_contract() {
    qbot_memory::conformance::run(&MemoryEpisodeStore::new()).await;
}

// ----- recall and history composition -----

#[derive(Debug)]
struct FixedClock(UnixMillis);
impl qbot_core::Clock for FixedClock {
    fn now(&self) -> UnixMillis {
        self.0
    }
}

#[tokio::test]
async fn recall_weighs_the_same_memory_by_its_age_using_the_clock() {
    use qbot_memory::conformance::{episode, group as g};
    use qbot_memory::{Recall, RecallParams};
    const DAY: i64 = 86_400_000;
    let now = 1_900_000_000_000_i64;
    let store = Arc::new(MemoryEpisodeStore::new());
    let embedder = Arc::new(FakeEmbedder::new(256));
    let model = embedder.info().model.clone();
    // The same discussion three times: a year ago, last month and last week.
    for (n, (title, days)) in [("a year ago", 365), ("last week", 7), ("last month", 30)]
        .into_iter()
        .enumerate()
    {
        let first = n as u64 * 10 + 1;
        let mut e = episode(g(1), first, first + 9, title);
        e.summary = "the deploy pipeline failed and was rolled back".into();
        e.ended = UnixMillis::new(now - days * DAY);
        let v = embedder
            .embed(&["deploy\nthe deploy pipeline failed and was rolled back".to_owned()])
            .await
            .unwrap()
            .vectors
            .remove(0);
        store.insert(&e, &v, &model).await.unwrap();
    }
    let recall = Recall::new(
        store,
        embedder,
        Arc::new(FixedClock(UnixMillis::new(now))),
        RecallParams {
            limit: 2,
            max_distance: 0.9,
            half_life: std::time::Duration::from_secs(180 * 86_400),
        },
    );
    let titles: Vec<String> = recall
        .recall(g(1), "why did the deploy pipeline fail")
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.episode.episode.title)
        .collect();
    assert_eq!(titles, ["last week", "last month"]);
}

#[tokio::test]
async fn recall_finds_the_episode_about_the_question_and_respects_scope_and_distance() {
    use qbot_memory::conformance::{episode, group as g};
    use qbot_memory::{Recall, RecallParams};
    let store = Arc::new(MemoryEpisodeStore::new());
    let embedder = Arc::new(FakeEmbedder::new(256));
    let model = embedder.info().model.clone();
    let topics = [
        ("deploy", "the deploy pipeline failed and was rolled back"),
        ("dinner", "what to cook for dinner tonight pasta or rice"),
        ("holiday", "planning the summer holiday trip to the coast"),
    ];
    for (i, (title, summary)) in topics.iter().enumerate() {
        let mut e = episode(g(1), i as u64 * 10 + 1, i as u64 * 10 + 10, title);
        e.summary = (*summary).to_owned();
        let v = embedder
            .embed(&[format!("{}\n{}", e.title, e.summary)])
            .await
            .unwrap()
            .vectors
            .remove(0);
        store.insert(&e, &v, &model).await.unwrap();
    }
    let recall = Recall::new(
        store.clone(),
        embedder.clone(),
        Arc::new(qbot_core::SystemClock),
        RecallParams {
            limit: 2,
            max_distance: 0.9,
            // Similarity alone: this test is about relevance.
            half_life: std::time::Duration::ZERO,
        },
    );
    let hits = recall
        .recall(g(1), "why did the deploy pipeline fail")
        .await
        .unwrap();
    assert_eq!(hits[0].episode.episode.title, "deploy");
    assert!(hits.len() <= 2);
    assert!(
        recall
            .recall(g(2), "why did the deploy pipeline fail")
            .await
            .unwrap()
            .is_empty(),
        "another group sees nothing"
    );
    let strict = Recall::new(
        store,
        embedder,
        Arc::new(qbot_core::SystemClock),
        RecallParams {
            limit: 5,
            max_distance: 0.05,
            half_life: std::time::Duration::ZERO,
        },
    );
    assert!(
        strict
            .recall(g(1), "completely unrelated gardening question")
            .await
            .unwrap()
            .is_empty()
    );
}

mod compose {
    use super::*;
    use qbot_memory::conformance::{episode, group as g};
    use qbot_memory::{Episode, EpisodeId, HistoryPart, compose};

    fn ep(id: i64, first: u64, last: u64) -> Episode {
        Episode {
            id: EpisodeId::new(id),
            created: UnixMillis::new(0),
            episode: episode(g(1), first, last, &format!("e{id}")),
        }
    }

    fn window(range: std::ops::RangeInclusive<u64>) -> Vec<(u64, u64)> {
        range.map(|n| (n, n)).collect()
    }

    fn flatten(parts: &[HistoryPart<u64>]) -> Vec<String> {
        parts
            .iter()
            .map(|p| match p {
                HistoryPart::Lines(l) => format!("lines {}..{}", l[0], l[l.len() - 1]),
                HistoryPart::Recap(e) => format!("recap {}", e.id.get()),
            })
            .collect()
    }

    #[test]
    fn older_whole_episodes_become_recaps_and_the_tail_stays_raw() {
        let lines = window(1..=100);
        let parts = compose(&lines, &[ep(1, 1, 30), ep(2, 31, 60), ep(3, 61, 90)], 10);
        assert_eq!(
            flatten(&parts),
            ["recap 1", "recap 2", "recap 3", "lines 91..100"]
        );
    }

    #[test]
    fn the_tail_is_never_replaced_even_when_an_episode_covers_it() {
        let lines = window(1..=60);
        let parts = compose(&lines, &[ep(1, 1, 30), ep(2, 31, 60)], 20);
        assert_eq!(
            flatten(&parts),
            ["recap 1", "lines 31..60"],
            "episode 2 reaches into the tail, so it stays raw"
        );
    }

    #[test]
    fn an_episode_only_partly_in_the_window_is_not_used() {
        // The window starts in the middle of episode 1 (an evicted batch boundary).
        let lines = window(11..=70);
        let parts = compose(&lines, &[ep(1, 1, 30), ep(2, 31, 60)], 5);
        assert_eq!(flatten(&parts), ["lines 11..30", "recap 2", "lines 61..70"]);
    }

    #[test]
    fn every_line_appears_exactly_once_whatever_the_episodes() {
        let lines = window(1..=120);
        for tail in [0, 1, 7, 30, 120, 500] {
            let episodes = [ep(1, 1, 30), ep(2, 31, 60), ep(3, 91, 120)];
            let parts = compose(&lines, &episodes, tail);
            let mut seen = 0u64;
            for part in &parts {
                match part {
                    HistoryPart::Lines(l) => {
                        for n in l {
                            assert!(*n > seen, "ordered, no repeats");
                            seen = *n;
                        }
                    }
                    HistoryPart::Recap(e) => {
                        assert!(e.episode.first_ordinal > seen);
                        seen = e.episode.last_ordinal;
                    }
                }
            }
            assert_eq!(seen, 120, "tail {tail}: nothing dropped");
        }
    }

    #[test]
    fn no_episodes_or_no_lines_is_handled() {
        assert!(compose::<u64>(&[], &[ep(1, 1, 30)], 5).is_empty());
        let parts = compose(&window(1..=10), &[], 5);
        assert_eq!(flatten(&parts), ["lines 1..10"]);
    }
}

#[allow(dead_code)]
fn unused(_: &dyn Provider) {}

#[tokio::test]
async fn the_in_memory_name_and_memory_stores_define_their_contracts() {
    qbot_memory::names_conformance::run(&qbot_memory::MemoryNameStore::default()).await;
    qbot_memory::memories_conformance::run(&qbot_memory::InMemoryMemoryStore::default()).await;
}

#[test]
fn names_are_normalized_compatibly_and_bounded() {
    use qbot_memory::name_key;
    assert_eq!(
        name_key("  \u{ff21}\u{ff4c}\u{ff45}\u{ff58} \t Kim ").unwrap(),
        "alex kim"
    );
    assert!(name_key("").is_err() && name_key(" \n ").is_err());
    assert!(name_key(&"a".repeat(65)).is_err());
    assert!(name_key(&"a".repeat(64)).is_ok());
}

fn said(ordinal: u64, account: i64, member: u32, text: &str) -> SliceLine {
    SliceLine {
        ordinal,
        message: MessageId::new(1000 + ordinal as i64).unwrap(),
        speaker: AccountId::new(account).ok(),
        member_no: Some(member),
        at: UnixMillis::new(1_800_000_000_000 + ordinal as i64 * 1000),
        text: text.into(),
        shown: None,
    }
}

/// The background replies' prompt layer would give: every current memory with its id.
struct Remembered(Arc<qbot_memory::InMemoryMemoryStore>);

#[async_trait::async_trait]
impl qbot_memory::SliceBackground for Remembered {
    async fn background(
        &self,
        group: GroupId,
        members: &[qbot_memory::SliceMember],
    ) -> Result<Background, qbot_memory::MemoryError> {
        use qbot_memory::MemoryStore;
        let mut subjects: Vec<Option<AccountId>> = members.iter().map(|m| m.account).collect();
        subjects.push(None);
        let current = self.0.current(group, &subjects).await?;
        Ok(Background {
            text: current
                .iter()
                .map(|m| format!("{} {}", m.id.label(), m.display()))
                .collect::<Vec<_>>()
                .join("\n"),
            known: current.iter().map(|m| (m.id, m.subject)).collect(),
            ..Background::default()
        })
    }
}

#[tokio::test]
async fn the_in_memory_note_store_meets_the_note_contract() {
    let accounts = [1, 2, 3].map(|n| qbot_core::AccountId::new(n).unwrap());
    qbot_memory::notes_conformance::run(&qbot_memory::MemoryNoteStore::default(), accounts).await;
}

#[test]
fn one_episode_says_one_thing_about_each_memory_its_last() {
    use qbot_memory::MemoryId;
    use qbot_memory::findings::{
        Checked, FindingContext, MemoryAction, MemoryArg, validate_findings,
    };

    let target = [
        said(1, 11, 2, "I quit Genshin"),
        said(2, 11, 2, "ok I'm back on Genshin again"),
        said(3, 12, 3, "I love tea"),
        said(4, 12, 3, "no, I stopped drinking tea"),
        said(5, 12, 3, "I love tea"),
    ];
    let (kit, other) = (AccountId::new(11).ok(), AccountId::new(12).ok());
    let known = [(MemoryId::new(1), kit), (MemoryId::new(2), other)];
    let item =
        |about: &str, action, id: Option<&str>, text: Option<&str>, line: u32, quote: &str| {
            MemoryArg {
                about: about.into(),
                action,
                id: id.map(Into::into),
                term: None,
                text: text.map(Into::into),
                durability: text.map(|_| qbot_memory::Durability::Ongoing),
                line,
                quote: quote.into(),
            }
        };
    let Checked {
        findings, dropped, ..
    } = validate_findings(
        &[],
        &[
            item(
                "member:2",
                MemoryAction::Ended,
                Some("m1"),
                None,
                1,
                "I quit Genshin",
            ),
            item(
                "member:2",
                MemoryAction::Same,
                Some("m1"),
                None,
                2,
                "back on Genshin",
            ),
            item(
                "member:3",
                MemoryAction::New,
                None,
                Some("loves tea"),
                3,
                "I love tea",
            ),
            item(
                "member:3",
                MemoryAction::Ended,
                Some("2"),
                None,
                4,
                "I stopped drinking tea",
            ),
            item(
                "member:3",
                MemoryAction::New,
                None,
                Some("Loves  tea"),
                5,
                "I love tea",
            ),
        ],
        &FindingContext {
            parts: [&[], &target, &[]],
            first: 1,
            quoted: &[],
            accounts: &[],
            known: &known,
            bot_names: &[],
        },
    );
    assert!(dropped.is_empty(), "{dropped:?}");
    let kept: Vec<(MemoryAction, Option<i64>)> = findings
        .memories
        .iter()
        .map(|f| (f.action, f.id.map(MemoryId::get)))
        .collect();
    assert_eq!(
        kept,
        [
            (MemoryAction::Same, Some(1)),
            (MemoryAction::Ended, Some(2)),
            (MemoryAction::New, None),
        ],
        "back on it after quitting holds; one new statement said twice is said once"
    );
}

#[test]
fn whom_a_line_names_is_checked_against_the_lines() {
    use qbot_memory::findings::{
        BasisArg, Checked, FindingContext, MemoryAction, MemoryArg, ReferenceArg, RoleArg,
        validate_findings,
    };
    use qbot_memory::{Grounds, NameBasis, Referent};

    let mut shown = said(6, 13, 4, "hi all");
    shown.shown = Some("Wren".into());
    let target = [
        said(1, 11, 2, "Bobo, call me Kit"),
        said(
            2,
            12,
            3,
            "[at:2] Kit, are you coming? [image:Sam at the door]",
        ),
        said(3, 12, 3, "[reply:1001] sure, Kitty"),
        said(4, 12, 3, "Kit is late again"),
        said(5, 12, 3, "Wren said so"),
        shown,
        // The member answers the line that called them Kitty; nobody answers the others.
        said(7, 11, 2, "[reply:1003] on my way"),
        said(8, 12, 3, "Old Ray keeps bees"),
    ];
    let named = |line: u32, text: &str, refers: &str, role, basis, shown_by: &[i64]| ReferenceArg {
        line,
        text: text.into(),
        refers: refers.into(),
        role,
        basis,
        shown_by: shown_by.to_vec(),
    };
    let stated = |about: &str, text: &str, line: u32, quote: &str| MemoryArg {
        about: about.into(),
        action: MemoryAction::New,
        id: None,
        term: None,
        text: Some(text.into()),
        durability: Some(qbot_memory::Durability::Ongoing),
        line,
        quote: quote.into(),
    };
    let (m2, kit) = ("member:2", Referent::Member(AccountId::new(11).unwrap()));
    let Checked {
        findings,
        dropped,
        unread,
    } = validate_findings(
        &[
            named(1, "Bobo", m2, RoleArg::Own, BasisArg::Own, &[]),
            named(1, "Kit", m2, RoleArg::Own, BasisArg::Own, &[]),
            named(2, "Kit", m2, RoleArg::Address, BasisArg::Marker, &[]),
            named(3, "Kitty", m2, RoleArg::Address, BasisArg::Marker, &[]),
            named(4, "Kit", m2, RoleArg::Address, BasisArg::Marker, &[]),
            named(4, "Kit", m2, RoleArg::About, BasisArg::Answer, &[1002]),
            named(
                4,
                "Kit",
                m2,
                RoleArg::About,
                BasisArg::Answer,
                &[1001, 9999],
            ),
            named(4, "Kit", m2, RoleArg::About, BasisArg::Content, &[]),
            named(5, "Wren", m2, RoleArg::About, BasisArg::Content, &[]),
            named(4, "late", m2, RoleArg::Own, BasisArg::Own, &[]),
            named(
                8,
                "Old Ray",
                "unknown",
                RoleArg::About,
                BasisArg::Content,
                &[1004],
            ),
            named(2, "Sam", m2, RoleArg::Address, BasisArg::Marker, &[]),
            named(4, "Kit", "member:9", RoleArg::About, BasisArg::Content, &[]),
            named(4, "Kit", "everyone", RoleArg::About, BasisArg::Content, &[]),
            named(4, "again", m2, RoleArg::About, BasisArg::None, &[]),
        ],
        &[
            stated("name:Old Ray", "keeps bees", 8, "Old Ray keeps bees"),
            stated("name:Kit", "is late", 4, "Kit is late"),
            stated("member:2", "is often late", 4, "Kit is late"),
            stated("member:4", "said so", 5, "Wren said so"),
        ],
        &FindingContext {
            parts: [&[], &target, &[]],
            first: 1,
            quoted: &[],
            accounts: &[],
            known: &[],
            bot_names: &["bobo".to_owned()],
        },
    );
    let kept: Vec<_> = findings
        .references
        .iter()
        .map(|r| (r.text.as_str(), r.referent, r.basis, r.grounds, r.anchored))
        .collect();
    assert_eq!(
        kept,
        [
            (
                "Bobo",
                Referent::Bot,
                NameBasis::About,
                Grounds::None,
                false
            ),
            ("Kit", kit, NameBasis::Own, Grounds::Own, false),
            ("Kit", kit, NameBasis::Addressed, Grounds::Marker, true),
            ("Kitty", kit, NameBasis::Addressed, Grounds::Marker, true),
            ("Kit", kit, NameBasis::About, Grounds::Answer, false),
            (
                "Old Ray",
                Referent::Unknown,
                NameBasis::About,
                Grounds::None,
                false
            ),
        ],
        "the bot's name is the bot's; a line said to a member is anchored; one reading per line"
    );
    assert_eq!(
        findings.references[4].shown_by,
        [MessageId::new(1001).unwrap()],
        "only lines that were shown"
    );
    let reasons = unread.join("\n");
    for expected in [
        "references item 5: basis marker: the line neither mentions [at:2] nor replies to a message of member:2",
        "references item 6: basis answer: shown_by names no line written by member:2",
        "references item 9: \"Wren\" is how member:4 is shown in these lines",
        "references item 10: self: member:2 did not write that line",
        "references item 12: \"Sam\" is not in what the member wrote on line #2",
        "references item 13: member:9 does not appear in the chat",
        "references item 14: refers must be member:N, bot, outside or unknown, not \"everyone\"",
        "references item 15: member:2 is named without a basis",
    ] {
        assert!(reasons.contains(expected), "{expected}\n{reasons}");
    }
    assert_eq!(unread.len(), 8, "{reasons}");

    // What is said of a member someone names is theirs; what is said of a name nobody could
    // place waits under the name.
    let stated: Vec<_> = findings
        .memories
        .iter()
        .map(|m| (m.subject, m.name.as_deref(), m.text.as_deref()))
        .collect();
    assert_eq!(
        stated,
        [
            (None, Some("Old Ray"), Some("keeps bees")),
            (AccountId::new(11).ok(), None, Some("is often late")),
        ]
    );
    let reasons = dropped.join("\n");
    for expected in [
        "memories item 2: about name:Kit needs a reference that records \"Kit\" as unknown",
        "memories item 4: member:4 neither wrote that line",
    ] {
        assert!(reasons.contains(expected), "{expected}\n{reasons}");
    }
}

#[tokio::test]
async fn a_full_member_gives_up_history_first_then_what_is_worth_least_never_what_was_just_said() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::findings::{Findings, MemoryAction, MemoryFinding};
    use qbot_memory::{
        Capacity, Durability, Episode, EpisodeId, HalfLives, InMemoryMemoryStore, MemoryNameStore,
        MemoryStore,
    };

    let kit = AccountId::new(11).unwrap();
    let episodes = Arc::new(MemoryEpisodeStore::new());
    let memories = Arc::new(InMemoryMemoryStore::default());
    let consolidator = Consolidator::new(Arc::new(MemoryNameStore::default()), memories.clone())
        .with_capacity(
            episodes.clone(),
            HalfLives::default(),
            Capacity {
                per_member: 2,
                group: 10,
            },
        );
    let finding = |action, id: Option<i64>, text: &str, durability| MemoryFinding {
        subject: Some(kit),
        name: None,
        action,
        id: id.map(qbot_memory::MemoryId::new),
        term: None,
        text: (!text.is_empty()).then(|| text.to_owned()),
        durability: Some(durability),
        message: MessageId::new(1).unwrap(),
        author: kit,
        quote: "q".into(),
    };
    let apply = |n: i64, findings: Vec<MemoryFinding>| {
        let mut new =
            qbot_memory::conformance::episode(group(), 1 + 30 * (n as u64 - 1), 30 * n as u64, "e");
        new.findings = Findings {
            memories: findings,
            ..Findings::default()
        };
        new.participants = vec![kit];
        Episode {
            id: EpisodeId::new(n),
            created: UnixMillis::new(0),
            episode: new,
        }
    };
    let texts = || async {
        memories
            .about(group(), Some(kit))
            .await
            .unwrap()
            .into_iter()
            .map(|m| {
                let current = m.is_current();
                (m.text, current)
            })
            .collect::<Vec<_>>()
    };
    let one = apply(
        1,
        vec![
            finding(
                MemoryAction::New,
                None,
                "lives in Hangzhou",
                Durability::Lasting,
            ),
            finding(
                MemoryAction::New,
                None,
                "plays Genshin",
                Durability::Current,
            ),
        ],
    );
    consolidator.apply(&one).await.unwrap();
    // Moving replaces Hangzhou: three memories in a room of two, the replaced one goes.
    let two = apply(
        2,
        vec![finding(
            MemoryAction::Changed,
            Some(1),
            "lives in Ningbo",
            Durability::Lasting,
        )],
    );
    consolidator.apply(&two).await.unwrap();
    assert_eq!(
        texts().await,
        [
            ("plays Genshin".to_owned(), true),
            ("lives in Ningbo".to_owned(), true)
        ]
    );
    // Something new: the memory worth least goes, never what the episode said.
    let three = apply(
        3,
        vec![finding(
            MemoryAction::New,
            None,
            "studies law",
            Durability::Lasting,
        )],
    );
    consolidator.apply(&three).await.unwrap();
    let left = texts().await;
    assert_eq!(left.len(), 2);
    assert!(left.iter().any(|(t, _)| t == "studies law"));
    // Applying an episode again evicts nothing more.
    consolidator.apply(&three).await.unwrap();
    assert_eq!(texts().await, left);
}

#[test]
fn rank_follows_support_and_the_subjects_own_conversation() {
    use qbot_memory::{Activity, Durability, HalfLives, Memory, MemoryId, MemoryState, Support};
    let kit = AccountId::new(11).unwrap();
    let other = AccountId::new(12).unwrap();
    let at = |n: i64| UnixMillis::new(n * 1000);
    let memory = |id: i64, durability, episodes: i64, last: i64| Memory {
        id: MemoryId::new(id),
        group: group(),
        subject: Some(kit),
        term: None,
        text: format!("m{id}"),
        durability,
        state: MemoryState::Current,
        first: at(0),
        last: at(last),
        support: (0..episodes)
            .map(|e| Support {
                episode: qbot_memory::EpisodeId::new(e),
                message: MessageId::new(1).unwrap(),
                author: Some(kit),
                quote: String::new(),
                at: at(last),
            })
            .collect(),
    };
    let h = HalfLives::default();
    // Ten episodes of the group: five with kit, five without.
    let activity = Activity::new(
        (1..=10)
            .map(|n| {
                (
                    at(100 + n),
                    if n % 2 == 0 { vec![kit] } else { vec![other] },
                )
            })
            .collect(),
    );
    let fresh = memory(1, Durability::Current, 1, 200);
    let passed = memory(2, Durability::Current, 1, 0);
    let lasting = memory(3, Durability::Lasting, 1, 0);
    let repeated = memory(4, Durability::Current, 4, 0);
    let until = at(1000);
    let w = |m: &Memory| h.worth(m, &activity, until);
    assert!((w(&fresh) - 0.5).abs() < 1e-9, "said once, nothing since");
    // Five of kit's episodes went by: a current memory loses half its rank to 10 such.
    assert!((w(&passed) - 0.5 * 0.5_f64.powf(0.5)).abs() < 1e-9);
    assert!(w(&lasting) > w(&passed), "a lasting memory keeps its place");
    assert!(w(&repeated) > w(&passed), "said in more conversations");
    let quiet = Activity::new((1..=10).map(|n| (at(100 + n), vec![other])).collect());
    assert!(
        (h.worth(&passed, &quiet, until) - 0.5).abs() < 1e-9,
        "a member who said nothing since has not passed it over"
    );
    let mut ended = fresh.clone();
    ended.state = MemoryState::Ended { at: at(300) };
    assert_eq!(w(&ended), 0.0);
    let all = [passed.clone(), fresh.clone(), lasting.clone()];
    let order: Vec<i64> = h
        .ranked(&all, &activity, until)
        .iter()
        .map(|m| m.id.get())
        .collect();
    assert_eq!(order, [1, 3, 2]);
}

// ----- the batch -----

/// A clock standing still.
#[derive(Debug)]
struct Fixed(i64);

impl qbot_core::Clock for Fixed {
    fn now(&self) -> UnixMillis {
        UnixMillis::new(self.0)
    }
}

const NOW: i64 = 1_800_100_000_000;

fn batch(steps: Vec<Step>) -> (Arc<FakeProvider>, Arc<MemoryEpisodeStore>, EpisodeJobs) {
    let fake = Arc::new(FakeProvider::new(steps));
    let store = Arc::new(MemoryEpisodeStore::new());
    let job = EpisodeJobs::new(
        store.clone(),
        BatchReader::new(fake.clone(), "English"),
        Arc::new(FakeEmbedder::new(64)),
        Arc::new(Fixed(NOW)),
    );
    (fake, store, job)
}

/// The model's division into episodes: `(first, last, quote)` per episode, the quote taken from
/// the episode's first line.
fn episodes(spans: &[(u32, u32, &str)]) -> Step {
    let list: Vec<serde_json::Value> = spans
        .iter()
        .enumerate()
        .map(|(i, (first, last, quote))| {
            json!({ "first_line": first, "last_line": last, "title": format!("Episode {}", i + 1),
                    "summary": format!("They talked, part {}.", i + 1),
                    "evidence": [{ "line": first, "quote": quote }] })
        })
        .collect();
    Step::Reply(FakeReply::new().call("submit_episodes", json!({ "episodes": list })))
}

fn findings(args: serde_json::Value) -> Step {
    Step::Reply(FakeReply::new().call("submit_findings", args))
}

fn nothing_found() -> Step {
    findings(json!({}))
}

fn cut_off() -> Step {
    Step::Reply(
        FakeReply::new()
            .reasoning("thinking at length")
            .finish(qbot_llm::FinishReason::Length),
    )
}

/// The messages before the task, as the provider saw them.
fn prefix_of(fake: &FakeProvider, request: usize) -> Vec<ConvItem> {
    fake.recorded()[request].conversation.items()[..3].to_vec()
}

#[tokio::test]
async fn the_batch_divides_the_new_lines_into_episodes_that_hold_every_line_once() {
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 5, "about the deploy"), (6, 12, "about the deploy")]),
        nothing_found(),
        nothing_found(),
    ]);
    store.add_lines(group(), lines(1..=12));
    assert_eq!(job.batch_group(group()).await.unwrap(), 2);

    let ranges: Vec<_> = store
        .within(group(), 1, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|e| {
            (
                e.episode.first_ordinal,
                e.episode.last_ordinal,
                e.episode.method,
            )
        })
        .collect();
    assert_eq!(
        ranges,
        [(1, 5, METHOD.to_owned()), (6, 12, METHOD.to_owned())]
    );
    assert_eq!(store.covered_through(group()).await.unwrap(), 12);
    assert_eq!(
        store.window(group()).await.unwrap(),
        Some(qbot_memory::WindowState {
            batched: UnixMillis::new(NOW),
            cut: 0
        }),
        "the batch time is what replies are shown as of"
    );

    // Every call of the group's batch begins with the same messages and declares the same tools;
    // only the task at the end differs.
    let recorded = fake.recorded();
    assert_eq!(
        recorded.len(),
        3,
        "the episodes, then each episode's findings"
    );
    for request in 1..3 {
        assert_eq!(prefix_of(&fake, request), prefix_of(&fake, 0));
        assert_eq!(recorded[request].tool_names, recorded[0].tool_names);
    }
    assert_eq!(
        recorded[0].tool_names,
        ["submit_episodes", "submit_findings", "submit_ruling"]
    );
    let lines_message = format!("{:?}", prefix_of(&fake, 0)[2]);
    assert!(
        lines_message.contains("#1 [msg:1001]") && lines_message.contains("#12 [msg:1012]"),
        "{lines_message}"
    );
    let tasks: Vec<String> = (0..3)
        .map(|i| format!("{:?}", recorded[i].conversation.items().last().unwrap()))
        .collect();
    assert!(tasks[0].contains("Task: episodes") && tasks[0].contains("#12"));
    assert!(tasks[1].contains("episode 1") && tasks[1].contains("#1 to #5"));
    assert!(tasks[2].contains("episode 2") && tasks[2].contains("#6 to #12"));

    // Nothing new: no call, and the window stays as the batch left it.
    assert_eq!(job.batch_group(group()).await.unwrap(), 0);
    assert_eq!(fake.recorded().len(), 3);
}

#[test]
fn episode_boundaries_must_hold_every_line_once_in_order() {
    use qbot_memory::batch::EpisodesArgs;

    let target = lines(1..=10);
    let check = |spans: serde_json::Value| -> Result<usize, Vec<String>> {
        let args: EpisodesArgs = serde_json::from_value(json!({ "episodes": spans })).unwrap();
        check_episodes(&args, &target).map(|planned| planned.len())
    };
    let ep = |first: u32, last: u32| {
        json!({ "first_line": first, "last_line": last, "title": "t", "summary": "s",
                "evidence": [{ "line": first, "quote": "about the deploy" }] })
    };
    assert_eq!(check(json!([ep(1, 4), ep(5, 10)])), Ok(2));
    assert_eq!(
        check(json!([ep(1, 10)])),
        Ok(1),
        "one episode may hold everything"
    );

    let problems = |spans| check(spans).unwrap_err().join("\n");
    let gap = problems(json!([ep(1, 4), ep(6, 10)]));
    assert!(
        gap.contains("episode 2: it starts at #6, but the first line no episode holds yet is #5"),
        "{gap}"
    );
    let overlap = problems(json!([ep(1, 5), ep(5, 10)]));
    assert!(overlap.contains("episode 2: it starts at #5"), "{overlap}");
    let short = problems(json!([ep(1, 4), ep(5, 8)]));
    assert!(
        short.contains("lines #9 to #10 are in no episode"),
        "{short}"
    );
    let beyond = problems(json!([ep(1, 12)]));
    assert!(
        beyond.contains("it ends at #12, after the last line #10"),
        "{beyond}"
    );
    let reversed = problems(json!([ep(1, 4), ep(5, 3), ep(4, 10)]));
    assert!(
        reversed.contains("episode 2: it ends before it starts"),
        "{reversed}"
    );
    assert!(problems(json!([])).contains("give at least one episode"));

    // Evidence comes from the episode's own lines, copied exactly.
    let outside = problems(json!([
        { "first_line": 1, "last_line": 4, "title": "t", "summary": "s",
          "evidence": [{ "line": 7, "quote": "about the deploy" }] },
        ep(5, 10)
    ]));
    assert!(
        outside.contains("evidence line #7 is not one of its lines #1 to #4"),
        "{outside}"
    );
    let invented = problems(json!([
        { "first_line": 1, "last_line": 10, "title": " ", "summary": "s",
          "evidence": [{ "line": 2, "quote": "about the weather" }] }
    ]));
    assert!(
        invented.contains("the title is empty") && invented.contains("the quote for #2"),
        "{invented}"
    );
}

#[tokio::test]
async fn rejected_boundaries_go_back_in_the_same_exchange_and_the_attempts_are_bounded() {
    let gap = || episodes(&[(1, 4, "about the deploy"), (6, 10, "about the deploy")]);
    let (fake, store, job) = batch(vec![
        gap(),
        episodes(&[(1, 10, "about the deploy")]),
        nothing_found(),
    ]);
    store.add_lines(group(), lines(1..=10));
    assert_eq!(job.batch_group(group()).await.unwrap(), 1);
    let sent_back = format!("{:?}", fake.recorded()[1].conversation.items().last());
    assert!(
        sent_back.contains("Rejected") && sent_back.contains("starts at #6"),
        "{sent_back}"
    );
    assert_eq!(
        fake.recorded()[1].conversation.items()[..3],
        fake.recorded()[0].conversation.items()[..3],
        "the correction keeps the shared prefix"
    );

    // Never a valid division: the lines are skipped for good and stay verbatim, and the group's
    // later lines are still read.
    let (fake, store, job) = batch(vec![gap(), gap(), gap()]);
    store.add_lines(group(), lines(1..=10));
    assert_eq!(job.batch_group(group()).await.unwrap(), 0);
    assert_eq!(fake.recorded().len(), 3, "every attempt was used");
    assert_eq!(store.covered_through(group()).await.unwrap(), 10);
    assert!(store.within(group(), 1, 10).await.unwrap().is_empty());
    fake.push(episodes(&[(1, 5, "about the deploy")]));
    fake.push(nothing_found());
    store.add_lines(group(), lines(11..=15));
    assert_eq!(job.batch_group(group()).await.unwrap(), 1);
    assert_eq!(
        store.within(group(), 1, 100).await.unwrap()[0]
            .episode
            .first_ordinal,
        11
    );
}

#[tokio::test]
async fn a_lost_answer_is_asked_for_again_and_one_lost_every_time_skips_the_lines() {
    let (fake, store, job) = batch(vec![
        cut_off(),
        cut_off(),
        episodes(&[(1, 4, "about the deploy")]),
        nothing_found(),
    ]);
    store.add_lines(group(), lines(1..=4));
    assert_eq!(job.batch_group(group()).await.unwrap(), 1);
    let requests = fake.recorded();
    assert_eq!(
        requests[2].conversation.items().len(),
        requests[0].conversation.items().len(),
        "a lost answer adds nothing to the exchange: the same request again"
    );

    let (_, store, job) = batch(vec![cut_off(), cut_off(), cut_off()]);
    store.add_lines(group(), lines(1..=4));
    assert_eq!(job.batch_group(group()).await.unwrap(), 0);
    assert_eq!(store.covered_through(group()).await.unwrap(), 4, "skipped");
}

#[tokio::test]
async fn a_batch_that_fails_midway_stores_nothing_and_is_read_again_from_the_same_point() {
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 2, "about the deploy"), (3, 4, "about the deploy")]),
        nothing_found(),
        Step::Fail(LlmError::Unavailable),
    ]);
    store.add_lines(group(), lines(1..=4));
    assert!(job.batch_group(group()).await.is_err());
    assert_eq!(store.covered_through(group()).await.unwrap(), 0);
    assert!(store.within(group(), 1, 4).await.unwrap().is_empty());
    assert_eq!(store.window(group()).await.unwrap(), None);

    fake.push(episodes(&[(1, 4, "about the deploy")]));
    fake.push(nothing_found());
    assert_eq!(job.batch_group(group()).await.unwrap(), 1);
    assert_eq!(store.covered_through(group()).await.unwrap(), 4);
}

#[tokio::test]
async fn an_archive_with_a_gap_is_refused_not_papered_over() {
    let (_, store, job) = batch(vec![]);
    let mut all = lines(1..=6);
    all.remove(3);
    store.add_lines(group(), all);
    assert!(job.batch_group(group()).await.is_err());
    assert_eq!(store.covered_through(group()).await.unwrap(), 0);
}

#[tokio::test]
async fn every_group_has_its_batch_and_one_failing_does_not_stop_the_others() {
    let other = GroupId::new(2).unwrap();
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 3, "about the deploy")]),
        nothing_found(),
        Step::Fail(LlmError::Unavailable),
    ]);
    store.add_lines(group(), lines(1..=3));
    store.add_lines(other, (1..=3).map(|n| line(n, "about the other group")));
    assert!(job.run().await.is_err(), "one group failed");
    assert_eq!(store.covered_through(group()).await.unwrap(), 3);
    assert_eq!(store.covered_through(other).await.unwrap(), 0);
    assert_ne!(
        prefix_of(&fake, 2)[1..],
        prefix_of(&fake, 0)[1..],
        "each group's own state and lines"
    );
    assert_eq!(
        prefix_of(&fake, 2)[0],
        prefix_of(&fake, 0)[0],
        "the instructions are shared by every group"
    );
}

#[tokio::test]
async fn a_new_embedding_model_re_embeds_episodes_without_reading_them_again() {
    let (_, store, job) = batch(vec![
        episodes(&[
            (1, 10, "about the deploy"),
            (11, 20, "about the deploy"),
            (21, 30, "about the deploy"),
        ]),
        nothing_found(),
        nothing_found(),
        nothing_found(),
    ]);
    store.add_lines(group(), lines(1..=30));
    assert_eq!(job.batch_group(group()).await.unwrap(), 3);

    let fake = Arc::new(FakeProvider::new([]));
    let embedder = Arc::new(FakeEmbedder::new(64).named("other-embed"));
    let job = EpisodeJobs::new(
        store.clone(),
        BatchReader::new(fake.clone(), "English"),
        embedder.clone(),
        Arc::new(Fixed(NOW)),
    );
    assert_eq!(job.batch_group(group()).await.unwrap(), 0, "no new episode");
    assert!(fake.recorded().is_empty(), "no model call");
    assert_eq!(
        embedder.calls().concat().len(),
        3,
        "each episode embedded once"
    );
    assert!(
        embedder.calls().concat()[0].starts_with("Episode 1\n"),
        "from the stored title and summary"
    );
}

#[tokio::test]
async fn the_lines_are_shown_with_the_chat_they_continue_their_dates_and_older_lines_they_quote() {
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 60, "about the deploy")]),
        nothing_found(),
        episodes(&[(1, 20, "agreed")]),
        nothing_found(),
    ]);
    store.add_lines(group(), lines(1..=60));
    job.batch_group(group()).await.unwrap();
    let first = format!("{:?}", prefix_of(&fake, 0)[2]);
    assert!(
        !first.contains("## Earlier in this chat")
            && !first.contains("Quoted from earlier in the chat (")
    );
    // The lines name their dates in full: 1_800_000_000_000 ms is 2027-01-15, a Friday.
    assert!(first.contains("from 2027-01-15 Fri 08:00"), "{first}");

    // The next batch: line 61 answers line 2, long before what is shown with it.
    let mut more = lines(61..=80);
    more[0].text = "[reply:1002] agreed, line 61".into();
    store.add_lines(group(), more);
    job.batch_group(group()).await.unwrap();
    let next = format!("{:?}", prefix_of(&fake, 2)[2]);
    assert!(
        next.contains("## Earlier in this chat") && next.contains("Episode 1: They talked"),
        "the previous episode's summary: {next}"
    );
    assert!(
        next.contains("[msg:1002]") && next.contains("[msg:1031]") && !next.contains("[msg:1030]"),
        "the quoted line, and the 30 lines before: {next}"
    );
    assert!(next.contains("#1 [msg:1061]"), "{next}");
}

fn findings_of_three_lines() -> serde_json::Value {
    json!({
        "references": [
            { "line": 1, "text": "Kit", "refers": "member:2", "role": "self", "basis": "self", "shown_by": [] },
            { "line": 2, "text": "Kit", "refers": "member:2", "role": "address", "basis": "marker", "shown_by": [] },
            { "line": 1, "text": "Kit", "refers": "member:3", "role": "self", "basis": "self", "shown_by": [] }
        ],
        "memories": [
            { "about": "member:2", "action": "new", "text": "lives in Hangzhou", "durability": "lasting", "line": 1, "quote": "I moved to Hangzhou" },
            { "about": "member:2", "action": "new", "text": "likes cats", "durability": "ongoing", "line": 2, "quote": "[at:2] Kit likes cats" },
            { "about": "member:1", "action": "new", "text": "likes dogs", "durability": "ongoing", "line": 2, "quote": "Kit likes cats" },
            { "about": "member:2", "action": "same", "id": "m99", "line": 1, "quote": "I moved" },
            { "about": "group", "action": "new", "term": "GG", "text": "good game", "durability": "ongoing", "line": 3, "quote": "GG means good game" },
            { "about": "group", "action": "new", "term": "AFK", "text": "away", "durability": "ongoing", "line": 3, "quote": "GG means" }
        ]
    })
}

fn three_lines() -> [SliceLine; 3] {
    [
        said(1, 11, 2, "I moved to Hangzhou last year, call me Kit"),
        said(2, 12, 3, "[at:2] Kit likes cats a lot"),
        said(3, 10, 1, "here GG means good game"),
    ]
}

#[tokio::test]
async fn an_episodes_findings_become_name_uses_and_memories_once() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::{InMemoryMemoryStore, MemoryNameStore, MemoryStore, NameStore};

    // The rejected findings go back once; the same answer again is kept as it stands.
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 3, "I moved to Hangzhou")]),
        findings(findings_of_three_lines()),
        findings(findings_of_three_lines()),
    ]);
    store.add_lines(group(), three_lines());
    let names = Arc::new(MemoryNameStore::default());
    let memories = Arc::new(InMemoryMemoryStore::default());
    let job = job.with_consolidator(Arc::new(Consolidator::new(names.clone(), memories.clone())));
    assert_eq!(job.batch_group(group()).await.unwrap(), 1);

    let instructions = format!("{:?}", prefix_of(&fake, 0)[0]);
    assert!(
        instructions.contains("### References") && instructions.contains("`answer`"),
        "{instructions}"
    );
    let sent_back = format!("{:?}", fake.recorded()[2].conversation.items().last());
    assert!(
        sent_back.contains("references item 3: self: member:3 did not write that line")
            && sent_back.contains("memories item 3: member:1 neither wrote that line")
            && sent_back.contains("memories item 4: \\\"m99\\\" is not a current memory")
            && sent_back.contains("memories item 6: the term \\\"AFK\\\" is not written"),
        "{sent_back}"
    );

    let kit = AccountId::new(11).unwrap();
    let about_kit = memories.current(group(), &[Some(kit)]).await.unwrap();
    let texts: Vec<(&str, u32)> = about_kit
        .iter()
        .map(|m| (m.text.as_str(), m.own_word()))
        .collect();
    assert_eq!(
        texts,
        [("lives in Hangzhou", 1), ("likes cats", 0)],
        "what they said themselves, and what someone said to them"
    );
    let ours = memories.current(group(), &[None]).await.unwrap();
    assert_eq!(
        ours.iter().map(|m| m.display()).collect::<Vec<_>>(),
        ["GG: good game"]
    );
    let known = names.names(group(), None).await.unwrap();
    assert_eq!(known.len(), 1);
    assert_eq!((known[0].key.as_str(), known[0].own), ("kit", 1));

    let stored = store.within(group(), 1, 3).await.unwrap();
    assert_eq!(
        (
            stored[0].episode.findings.references.len(),
            stored[0].episode.findings.memories.len()
        ),
        (2, 3)
    );
    assert!(store.unconsolidated(group()).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_episode_stored_but_not_yet_applied_is_applied_by_the_next_batch() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::{InMemoryMemoryStore, MemoryNameStore, MemoryStore};

    // First a batch with nothing to apply findings to, as if the process died before applying.
    let (_, store, job) = batch(vec![
        episodes(&[(1, 3, "I moved to Hangzhou")]),
        findings(findings_of_three_lines()),
        findings(findings_of_three_lines()),
    ]);
    store.add_lines(group(), three_lines());
    job.batch_group(group()).await.unwrap();
    assert_eq!(store.unconsolidated(group()).await.unwrap().len(), 1);

    let memories = Arc::new(InMemoryMemoryStore::default());
    let job = EpisodeJobs::new(
        store.clone(),
        BatchReader::new(Arc::new(FakeProvider::new([])), "English"),
        Arc::new(FakeEmbedder::new(64)),
        Arc::new(Fixed(NOW)),
    )
    .with_consolidator(Arc::new(Consolidator::new(
        Arc::new(MemoryNameStore::default()),
        memories.clone(),
    )));
    assert_eq!(job.batch_group(group()).await.unwrap(), 0);
    let kit = [AccountId::new(11).ok()];
    assert_eq!(memories.current(group(), &kit).await.unwrap().len(), 2);
    assert!(store.unconsolidated(group()).await.unwrap().is_empty());
}

/// The reader over a stretch of `lines`, with an empty background.
async fn read_findings(
    steps: Vec<Step>,
    lines: &[SliceLine],
    span: (u32, u32),
) -> (
    Arc<FakeProvider>,
    qbot_memory::Answer<qbot_memory::findings::Checked>,
) {
    let fake = Arc::new(FakeProvider::new(steps));
    let reader = BatchReader::new(fake.clone(), "English");
    let background = Background::default();
    let stretch = Stretch {
        previous: &[],
        lines,
        background: &background,
    };
    let prefix = reader.prefix(&stretch);
    let plan = qbot_memory::Planned {
        first: span.0,
        last: span.1,
        title: "t".into(),
        summary: "s".into(),
        evidence: Vec::new(),
    };
    let answer = reader.findings(&prefix, &stretch, &plan, 1).await.unwrap();
    (fake, answer)
}

fn term_findings(line: u32, quote: &str) -> serde_json::Value {
    json!({ "memories": [
        { "about": "group", "action": "new", "term": "the Den", "text": "the cat cafe by the lake",
          "durability": "ongoing", "line": line, "quote": quote }
    ]})
}

#[tokio::test]
async fn rejected_findings_go_back_once_and_the_corrected_answer_is_kept() {
    let lines = [
        said(1, 11, 2, "we all call that cafe the Den"),
        said(2, 12, 3, "the Den is the cat cafe by the lake"),
    ];
    let (fake, out) = read_findings(
        vec![
            findings(term_findings(2, "the cafe by the lake")),
            findings(term_findings(2, "the cat cafe by the lake")),
        ],
        &lines,
        (1, 2),
    )
    .await;
    assert_eq!(out.attempts, 2);
    assert!(out.value.dropped.is_empty(), "{:?}", out.value.dropped);
    assert_eq!(
        out.value.findings.memories[0].term.as_deref(),
        Some("the Den")
    );
    let sent_back = format!("{:?}", fake.recorded()[1].conversation.items().last());
    assert!(
        sent_back.contains("some items were not accepted")
            && sent_back.contains("memories item 1: the quote for line #2"),
        "{sent_back}"
    );
}

#[tokio::test]
async fn findings_cite_only_the_episode_s_own_lines_numbered_as_the_stretch_numbers_them() {
    let lines = [
        said(1, 11, 2, "we all call that cafe the Den"),
        said(2, 12, 3, "the Den is the cat cafe by the lake"),
        said(3, 12, 3, "anyway, lunch?"),
    ];
    // The episode is lines #2 to #3: line 2 counts, line 1 is context.
    let (_, out) = read_findings(
        vec![findings(json!({ "memories": [
            { "about": "group", "action": "new", "term": "the Den", "text": "the cat cafe by the lake",
              "durability": "ongoing", "line": 2, "quote": "the cat cafe by the lake" },
            { "about": "member:2", "action": "new", "text": "calls it the Den",
              "durability": "ongoing", "line": 1, "quote": "call that cafe the Den" }
        ]}))],
        &lines,
        (2, 3),
    )
    .await;
    assert_eq!(out.value.findings.memories.len(), 1);
    assert_eq!(
        out.value.findings.memories[0].message,
        MessageId::new(1002).unwrap()
    );
    assert_eq!(
        out.value.dropped,
        ["memories item 2: line #1 is not a line of the target part"]
    );
}

#[tokio::test]
async fn a_term_may_be_explained_by_a_line_that_does_not_repeat_it() {
    let lines = [
        said(1, 11, 2, "we all call that place the Den"),
        said(2, 12, 3, "the cat cafe by the lake, right?"),
    ];
    let (_, out) = read_findings(
        vec![
            findings(json!({ "memories": [
                { "about": "group", "action": "new", "term": "the Den", "text": "the cat cafe by the lake", "durability": "ongoing", "line": 2, "quote": "the cat cafe by the lake" },
                { "about": "group", "action": "new", "term": "the Burrow", "text": "invented", "durability": "ongoing", "line": 2, "quote": "the cat cafe" },
                { "about": "member:2", "action": "new", "term": "Den", "text": "a term about a member", "durability": "ongoing", "line": 1, "quote": "the Den" }
            ]})),
            findings(json!({ "memories": [
                { "about": "group", "action": "new", "term": "the Den", "text": "the cat cafe by the lake", "durability": "ongoing", "line": 2, "quote": "the cat cafe by the lake" }
            ]})),
        ],
        &lines,
        (1, 2),
    )
    .await;
    assert_eq!(out.value.findings.memories.len(), 1);
    assert_eq!(out.attempts, 2, "the dropped items were sent back once");
}

#[tokio::test]
async fn later_statements_confirm_replace_and_end_memories_and_history_is_kept() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::{InMemoryMemoryStore, MemoryNameStore, MemoryState, MemoryStore};

    let first = json!({ "memories": [
        { "about": "member:2", "action": "new", "text": "plays Genshin", "durability": "current", "line": 1, "quote": "I play Genshin" },
        { "about": "member:2", "action": "new", "text": "plays chess on weekends", "durability": "ongoing", "line": 2, "quote": "and Chess" },
        { "about": "member:2", "action": "new", "text": "lives in Hangzhou", "durability": "lasting", "line": 3, "quote": "in Hangzhou" }
    ]});
    // The ids are those the background listed: m1 Genshin, m2 chess, m3 Hangzhou.
    let second = json!({ "memories": [
        { "about": "member:2", "action": "ended", "id": "m1", "line": 1, "quote": "quit Genshin" },
        { "about": "member:2", "action": "ended", "id": "m42", "line": 1, "quote": "quit Genshin" },
        { "about": "member:3", "action": "ended", "id": "m2", "line": 2, "quote": "no more" },
        { "about": "member:2", "action": "changed", "id": "m3", "text": "lives in Ningbo", "durability": "lasting", "line": 3, "quote": "moved to Ningbo" },
        { "about": "member:2", "action": "same", "id": "m2", "line": 4, "quote": "still play chess" }
    ]});
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 4, "I play Genshin")]),
        findings(first),
        episodes(&[(1, 4, "quit Genshin")]),
        findings(second.clone()),
        findings(second),
    ]);
    store.add_lines(
        group(),
        [
            said(1, 11, 2, "I play Genshin every night"),
            said(2, 11, 2, "and Chess on weekends"),
            said(3, 11, 2, "I live in Hangzhou"),
            said(4, 12, 3, "nice"),
        ],
    );
    let memories = Arc::new(InMemoryMemoryStore::default());
    let job = job
        .with_consolidator(Arc::new(Consolidator::new(
            Arc::new(MemoryNameStore::default()),
            memories.clone(),
        )))
        .with_background(Arc::new(Remembered(memories.clone())));
    job.batch_group(group()).await.unwrap();
    store.add_lines(
        group(),
        [
            said(5, 11, 2, "I quit Genshin last week"),
            said(6, 12, 3, "no more for me either"),
            said(7, 11, 2, "and I moved to Ningbo"),
            said(8, 11, 2, "I still play chess though"),
        ],
    );
    job.batch_group(group()).await.unwrap();
    assert!(
        format!("{:?}", prefix_of(&fake, 2)[1]).contains("m3 lives in Hangzhou"),
        "the batch saw the memories with their ids"
    );

    let kit = AccountId::new(11).ok();
    let all = memories.about(group(), kit).await.unwrap();
    let states: Vec<(&str, &str)> = all
        .iter()
        .map(|m| {
            let state = match m.state {
                MemoryState::Current => "current",
                MemoryState::Replaced { .. } => "replaced",
                MemoryState::Ended { .. } => "ended",
            };
            (m.text.as_str(), state)
        })
        .collect();
    assert_eq!(
        states,
        [
            ("plays Genshin", "ended"),
            ("plays chess on weekends", "current"),
            ("lives in Hangzhou", "replaced"),
            ("lives in Ningbo", "current"),
        ],
        "an ended or replaced memory is kept as history"
    );
    assert_eq!(all[1].episodes(), 2, "said again in the second episode");
}

#[tokio::test]
async fn a_name_no_conversation_could_place_is_ruled_on_from_its_uses_together() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::{InMemoryMemoryStore, MemoryNameStore, MemoryStore, NameStore};

    // Three conversations name "Old Ray"; in none of them does one line show who that is.
    let unplaced = |line: u32| {
        findings(
            json!({ "references": [{ "line": line, "text": "Old Ray", "refers": "unknown",
                         "role": "about", "basis": "none", "shown_by": [] }] }),
        )
    };
    let held = findings(json!({
        "references": [{ "line": 5, "text": "Old Ray", "refers": "unknown", "role": "about",
                         "basis": "none", "shown_by": [] }],
        "memories": [{ "about": "name:Old Ray", "action": "new", "text": "keeps bees",
                       "durability": "lasting", "line": 5, "quote": "keeps bees on the roof" }]
    }));
    let (fake, store, job) = batch(vec![
        episodes(&[
            (1, 2, "never sleeps"),
            (3, 4, "about the bees"),
            (5, 6, "keeps bees"),
        ]),
        unplaced(1),
        unplaced(3),
        held,
        Step::Reply(FakeReply::new().call(
            "submit_ruling",
            json!({ "verdict": "member:2", "lines": [1002, 1004, 4242],
                    "reason": "member:2 answers each time it is said" }),
        )),
    ]);
    store.add_lines(
        group(),
        [
            said(1, 12, 3, "Old Ray never sleeps"),
            said(2, 11, 2, "who, me?"),
            said(3, 13, 4, "ask Old Ray about the bees"),
            said(4, 11, 2, "sure, ask away"),
            said(5, 12, 3, "Old Ray keeps bees on the roof"),
            said(6, 13, 4, "nice"),
        ],
    );
    let names = Arc::new(MemoryNameStore::default());
    let memories = Arc::new(InMemoryMemoryStore::default());
    let job = job
        .with_consolidator(Arc::new(Consolidator::new(names.clone(), memories.clone())))
        .with_rulings(names.clone());
    assert_eq!(job.batch_group(group()).await.unwrap(), 3);

    // The ruling read the uses side by side, each with its chat, after the same prefix.
    assert_eq!(prefix_of(&fake, 4), prefix_of(&fake, 0));
    let case = format!("{:?}", fake.recorded()[4].conversation.items().last());
    assert!(
        case.contains("Task: ruling")
            && case.contains("\\\"Old Ray\\\" was used 3 time(s)")
            && case.contains("Use 3:")
            && case.contains(">> [msg:1005]")
            && case.contains("   [msg:1004]"),
        "{case}"
    );
    let ray = AccountId::new(11).unwrap();
    let known = names.names(group(), None).await.unwrap();
    assert_eq!(known.len(), 1);
    assert_eq!(
        (known[0].account, known[0].key.as_str(), known[0].ruled),
        (ray, "old ray", true)
    );
    // What was said of "Old Ray" waited for the name, and is now remembered about the member.
    assert!(names.held(group()).await.unwrap().is_empty());
    let about = memories.current(group(), &[Some(ray)]).await.unwrap();
    assert_eq!(about.len(), 1);
    assert_eq!(about[0].text, "keeps bees");
    // Nothing is left to ask: the next batch calls no model.
    assert_eq!(job.batch_group(group()).await.unwrap(), 0);
    assert_eq!(fake.recorded().len(), 5);
}

#[tokio::test]
async fn an_episode_an_older_method_read_is_read_again_for_its_findings_alone() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::{
        Change, Durability, EpisodeId, InMemoryMemoryStore, MemoryNameStore, MemoryStore,
        NameStore, Statement, Support,
    };

    let kit = AccountId::new(11).unwrap();
    let (fake, store, job) = batch(vec![findings(json!({
        "references": [{ "line": 1, "text": "Kit", "refers": "member:2", "role": "self",
                         "basis": "self", "shown_by": [] }],
        "memories": [
            { "about": "member:2", "action": "changed", "id": "m1", "text": "lives in Hangzhou",
              "durability": "lasting", "line": 1, "quote": "I moved to Hangzhou" },
            { "about": "member:2", "action": "ended", "id": "m2", "line": 1, "quote": "gave up chess" }
        ]
    }))]);
    store.add_lines(
        group(),
        [
            said(
                1,
                11,
                2,
                "call me Kit, I moved to Hangzhou and gave up chess",
            ),
            said(2, 12, 3, "ok"),
        ],
    );
    // The episode as an earlier version left it, already applied, and what chat since then has
    // taught: two memories about its speaker.
    let old = store
        .insert(
            &qbot_memory::conformance::episode(group(), 1, 2, "old"),
            &[1.0; 64],
            "fake",
        )
        .await
        .unwrap();
    store.mark_consolidated(old.id).await.unwrap();
    let memories = Arc::new(InMemoryMemoryStore::default());
    let later = |text: &str| {
        Change::New(Statement {
            subject: Some(kit),
            term: None,
            text: text.into(),
            durability: Durability::Lasting,
            support: Support {
                episode: EpisodeId::new(90),
                message: MessageId::new(9000).unwrap(),
                author: Some(kit),
                quote: "q".into(),
                at: UnixMillis::new(1_900_000_000_000),
            },
        })
    };
    memories
        .apply(group(), &[later("lives in Beijing"), later("plays chess")])
        .await
        .unwrap();
    let names = Arc::new(MemoryNameStore::default());
    let job = job
        .with_consolidator(Arc::new(Consolidator::new(names.clone(), memories.clone())))
        .with_background(Arc::new(Remembered(memories.clone())))
        .with_reread(1);
    assert_eq!(job.batch_group(group()).await.unwrap(), 0, "nothing new");
    let task = format!("{:?}", fake.recorded()[0].conversation.items().last());
    assert!(
        task.contains("earlier stretch") && task.contains("#1 [msg:1001]"),
        "{task}"
    );

    let again = store.get(group(), old.id).await.unwrap().unwrap();
    assert_eq!(again.episode.method, METHOD);
    assert_eq!(
        (
            again.episode.title.as_str(),
            again.episode.findings.references.len()
        ),
        ("old", 1),
        "the summary stays; the findings are the new reading's"
    );
    let known = names.names(group(), None).await.unwrap();
    assert_eq!((known[0].key.as_str(), known[0].own), ("kit", 1));
    // Older words close nothing learned since: what changed is remembered as said then, and
    // what ended is left alone.
    let mut texts: Vec<String> = memories
        .current(group(), &[Some(kit)])
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect();
    texts.sort();
    assert_eq!(
        texts,
        ["lives in Beijing", "lives in Hangzhou", "plays chess"]
    );
    // Read the current way, it is not read again.
    assert_eq!(job.batch_group(group()).await.unwrap(), 0);
    assert_eq!(fake.recorded().len(), 1);
}
