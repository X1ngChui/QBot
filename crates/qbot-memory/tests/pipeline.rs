#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_llm::{ConvItem, Embedder, FakeEmbedder, LlmError, Provider};
use qbot_memory::decide::Candidate;
use qbot_memory::memories::Obs;
use qbot_memory::{
    About, BatchReader, EpisodeJobs, EpisodeStore, InMemoryStore, Kind, METHOD, Memory, MemoryId,
    MemoryStore, Observation, ObservationId, Origin, Said, SliceLine, check_episodes, known_names,
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
async fn the_in_memory_store_satisfies_the_store_contracts() {
    let store = InMemoryStore::new();
    qbot_memory::conformance::run(&store, &store).await;
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
    let store = Arc::new(InMemoryStore::new());
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
    let store = Arc::new(InMemoryStore::new());
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

#[test]
fn whom_a_line_names_is_checked_against_the_lines() {
    use qbot_memory::findings::{
        BasisArg, Checked, FindingContext, ReferenceArg, RoleArg, StatementArg, validate_findings,
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
    let stated = |about: &str, text: &str, line: u32, quote: &str| StatementArg {
        about: about.into(),
        text: text.into(),
        durability: qbot_memory::Durability::Ongoing,
        term: None,
        with: None,
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
        .statements
        .iter()
        .map(|m| (m.about.clone(), m.text.as_str()))
        .collect();
    assert_eq!(
        stated,
        [
            (About::Name("Old Ray".into()), "keeps bees"),
            (About::Member(AccountId::new(11).unwrap()), "is often late"),
        ]
    );
    let reasons = dropped.join("\n");
    for expected in [
        "statements item 2: about name:Kit needs a reference that records \"Kit\" as unknown",
        "statements item 4: member:4 neither wrote that line",
    ] {
        assert!(reasons.contains(expected), "{expected}\n{reasons}");
    }
}

#[test]
fn rank_follows_support_and_the_subjects_own_conversation() {
    use qbot_memory::{Activity, Durability, HalfLives, MemoryState};
    let kit = AccountId::new(11).unwrap();
    let other = AccountId::new(12).unwrap();
    let at = |n: i64| UnixMillis::new(n * 1000);
    let memory = |id: i64, durability, episodes: i64, last: i64| Memory {
        id: MemoryId::new(id),
        group: group(),
        subject: Some(kit),
        kind: Kind::Statement,
        with: None,
        term: None,
        text: format!("m{id}"),
        durability,
        origin: Origin::Chat,
        state: MemoryState::Current,
        first: at(0),
        last: at(last),
        support: (0..episodes)
            .map(|e| Observation {
                id: ObservationId::new(e),
                episode: qbot_memory::EpisodeId::new(e),
                message: MessageId::new(1).unwrap(),
                ordinal: 1,
                author: kit,
                quote: String::new(),
                at: at(last),
                about: About::Member(kit),
                said: Said::Statement {
                    text: String::new(),
                    term: None,
                    durability,
                    with: None,
                },
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
    let mut by_hand = memory(5, Durability::Current, 0, 0);
    by_hand.origin = Origin::Hand { by: kit };
    assert_eq!(
        h.ranked(&[passed.clone(), by_hand], &activity, until)[0]
            .id
            .get(),
        5,
        "what a person entered by hand comes first"
    );
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

fn jobs_over(fake: &Arc<FakeProvider>, store: &Arc<InMemoryStore>) -> EpisodeJobs {
    EpisodeJobs::new(
        store.clone(),
        store.clone(),
        BatchReader::new(fake.clone(), "English"),
        Arc::new(FakeEmbedder::new(64)),
        Arc::new(Fixed(NOW)),
    )
}

fn batch(steps: Vec<Step>) -> (Arc<FakeProvider>, Arc<InMemoryStore>, EpisodeJobs) {
    let fake = Arc::new(FakeProvider::new(steps));
    let store = Arc::new(InMemoryStore::new());
    let job = jobs_over(&fake, &store);
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

fn decide(decisions: serde_json::Value) -> Step {
    Step::Reply(FakeReply::new().call("submit_decisions", json!({ "decisions": decisions })))
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

fn task_of(fake: &FakeProvider, request: usize) -> String {
    format!(
        "{:?}",
        fake.recorded()[request]
            .conversation
            .items()
            .last()
            .unwrap()
    )
}

fn statement(about: &str, text: &str, line: u32, quote: &str) -> serde_json::Value {
    json!({ "about": about, "text": text, "durability": "lasting", "line": line, "quote": quote })
}

fn texts(memories: &[Memory]) -> Vec<(String, bool)> {
    memories
        .iter()
        .map(|m| (m.display(), m.is_current()))
        .collect()
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
        [
            "submit_episodes",
            "submit_findings",
            "submit_decisions",
            "submit_ruling"
        ]
    );
    let lines_message = format!("{:?}", prefix_of(&fake, 0)[2]);
    assert!(
        lines_message.contains("#1 [msg:1001]") && lines_message.contains("#12 [msg:1012]"),
        "{lines_message}"
    );
    assert!(task_of(&fake, 0).contains("Task: episodes") && task_of(&fake, 0).contains("#12"));
    assert!(task_of(&fake, 1).contains("episode 1") && task_of(&fake, 1).contains("#1 to #5"));
    assert!(task_of(&fake, 2).contains("episode 2") && task_of(&fake, 2).contains("#6 to #12"));

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
    let outside = problems(json!([
        { "first_line": 1, "last_line": 4, "title": "t", "summary": "s",
          "evidence": [{ "line": 7, "quote": "about the deploy" }] },
        ep(5, 10)
    ]));
    assert!(
        outside.contains("evidence line #7 is not one of its lines #1 to #4"),
        "{outside}"
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
    let sent_back = task_of(&fake, 1);
    assert!(
        sent_back.contains("Rejected") && sent_back.contains("starts at #6"),
        "{sent_back}"
    );
    assert_eq!(
        prefix_of(&fake, 1),
        prefix_of(&fake, 0),
        "the correction keeps the prefix"
    );

    // Never a valid division: the lines are skipped for good, and later lines are still read.
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
async fn a_batch_that_fails_midway_writes_nothing_and_is_read_again_from_the_same_point() {
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 2, "I moved to Hangzhou"), (3, 3, "and I like tea")]),
        findings(
            json!({ "statements": [statement("member:2", "lives in Hangzhou", 1, "I moved to Hangzhou")] }),
        ),
        Step::Fail(LlmError::Unavailable),
    ]);
    store.add_lines(
        group(),
        [
            said(1, 11, 2, "I moved to Hangzhou"),
            said(2, 12, 3, "nice"),
            said(3, 11, 2, "and I like tea"),
        ],
    );
    assert!(job.batch_group(group()).await.is_err());
    assert_eq!(store.covered_through(group()).await.unwrap(), 0);
    assert!(store.within(group(), 1, 4).await.unwrap().is_empty());
    assert!(
        store
            .about(group(), AccountId::new(11).ok())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.pending(group()).await.unwrap().is_empty());
    assert_eq!(store.window(group()).await.unwrap(), None);

    fake.push(episodes(&[(1, 3, "I moved to Hangzhou")]));
    fake.push(findings(json!({ "statements": [statement("member:2", "lives in Hangzhou", 1, "I moved to Hangzhou")] })));
    fake.push(decide(json!([{ "statement": "o1", "action": "new" }])));
    assert_eq!(job.batch_group(group()).await.unwrap(), 1);
    assert_eq!(store.covered_through(group()).await.unwrap(), 3);
    assert_eq!(
        texts(&store.about(group(), AccountId::new(11).ok()).await.unwrap()),
        [("lives in Hangzhou".to_owned(), true)]
    );
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
        "each group's own lines"
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
        episodes(&[(1, 10, "about the deploy"), (11, 20, "about the deploy")]),
        nothing_found(),
        nothing_found(),
    ]);
    store.add_lines(group(), lines(1..=20));
    assert_eq!(job.batch_group(group()).await.unwrap(), 2);

    let fake = Arc::new(FakeProvider::new([]));
    let embedder = Arc::new(FakeEmbedder::new(64).named("other-embed"));
    let job = EpisodeJobs::new(
        store.clone(),
        store.clone(),
        BatchReader::new(fake.clone(), "English"),
        embedder.clone(),
        Arc::new(Fixed(NOW)),
    );
    assert_eq!(job.batch_group(group()).await.unwrap(), 0, "no new episode");
    assert!(fake.recorded().is_empty(), "no model call");
    assert_eq!(
        embedder.calls().concat().len(),
        2,
        "each episode embedded once"
    );
    assert!(
        embedder.calls().concat()[0].starts_with("Episode 1\n"),
        "from the stored title and summary"
    );
}

#[tokio::test]
async fn the_lines_are_shown_with_the_conversations_before_them_and_older_lines_they_quote() {
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 60, "about the deploy")]),
        nothing_found(),
        episodes(&[(1, 20, "agreed")]),
        nothing_found(),
    ]);
    store.add_lines(group(), lines(1..=60));
    job.batch_group(group()).await.unwrap();
    let first = format!("{:?}", prefix_of(&fake, 0)[2]);
    assert!(!first.contains("## Earlier in this chat"));
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
        "the summaries of the conversations before: {next}"
    );
    assert!(
        next.contains("[msg:1002]") && next.contains("[msg:1031]") && !next.contains("[msg:1030]"),
        "the quoted line, and the 30 lines before: {next}"
    );
    assert!(next.contains("#1 [msg:1061]"), "{next}");
}

#[tokio::test]
async fn statements_are_decided_per_subject_with_the_whole_night_in_view() {
    // Kit says where they live twice in one day, in other words; the group learns a term.
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 2, "I moved to Hangzhou"), (3, 4, "GG means good game")]),
        findings(json!({
            "references": [
                { "line": 1, "text": "Kit", "refers": "member:2", "role": "self", "basis": "self", "shown_by": [] },
                { "line": 2, "text": "Kit", "refers": "member:2", "role": "address", "basis": "marker", "shown_by": [] }
            ],
            "statements": [
                statement("member:2", "lives in Hangzhou", 1, "I moved to Hangzhou"),
                statement("member:2", "has lived in Hangzhou since last year", 2, "since last year")
            ]
        })),
        findings(json!({
            "statements": [
                { "about": "group", "term": "GG", "text": "good game", "durability": "ongoing", "line": 3, "quote": "GG means good game" },
                statement("member:2", "lives in Hangzhou", 4, "still in Hangzhou")
            ]
        })),
        // The group first, then each member, by account.
        decide(json!([{ "statement": "o1", "action": "new" }])),
        decide(json!([
            { "statement": "o1", "action": "new" },
            { "statement": "o2", "action": "support", "memory": "o1" },
            { "statement": "o3", "action": "support", "memory": "o1" }
        ])),
    ]);
    store.add_lines(
        group(),
        [
            said(1, 11, 2, "I moved to Hangzhou last year, call me Kit"),
            said(
                2,
                12,
                3,
                "[at:2] Kit likes cats, since last year in Hangzhou?",
            ),
            said(3, 10, 1, "here GG means good game"),
            said(4, 11, 2, "yes, still in Hangzhou"),
        ],
    );
    job.batch_group(group()).await.unwrap();

    // One call for the group and one for Kit, each with every statement about them, in order.
    assert_eq!(fake.recorded().len(), 5);
    let kit_case = task_of(&fake, 4);
    assert!(
        kit_case.contains("Task: decide")
            && kit_case.contains("Subject: member:2")
            && kit_case.contains("o1 #1 member:2, about themselves")
            && kit_case.contains("o3 #4"),
        "{kit_case}"
    );
    assert_eq!(prefix_of(&fake, 4), prefix_of(&fake, 0), "the same prefix");
    let kit = AccountId::new(11).unwrap();
    let about_kit = store.about(group(), Some(kit)).await.unwrap();
    let statements: Vec<&Memory> = about_kit
        .iter()
        .filter(|m| m.kind == Kind::Statement)
        .collect();
    assert_eq!(statements.len(), 1, "said three times, remembered once");
    assert_eq!(statements[0].text, "lives in Hangzhou");
    assert_eq!(statements[0].support.len(), 3);
    assert_eq!((statements[0].episodes(), statements[0].own_word()), (2, 2));
    // The name read from Kit's own word and from a line said to them.
    let names = known_names(&store.names(group()).await.unwrap());
    assert_eq!(names.len(), 1);
    assert_eq!(
        (
            names[0].key.as_str(),
            names[0].own,
            names[0].addressed,
            names[0].anchored
        ),
        ("kit", 1, 0, 0),
        "one episode, its strongest use: their own"
    );
    assert_eq!(
        texts(&store.about(group(), None).await.unwrap()),
        [("GG: good game".to_owned(), true)]
    );
    assert!(store.pending(group()).await.unwrap().is_empty());
}

/// A memory about Kit (account 11), said by `author` in one conversation.
fn known(id: i64, text: &str, author: i64, origin: Origin, kind: Kind) -> Memory {
    let kit = AccountId::new(11).unwrap();
    Memory {
        id: MemoryId::new(id),
        group: group(),
        subject: Some(kit),
        kind,
        with: None,
        term: None,
        text: text.into(),
        durability: qbot_memory::Durability::Lasting,
        origin,
        state: qbot_memory::MemoryState::Current,
        first: UnixMillis::new(0),
        last: UnixMillis::new(0),
        support: if origin.is_hand() {
            Vec::new()
        } else {
            vec![Observation {
                id: ObservationId::new(id),
                episode: qbot_memory::EpisodeId::new(id),
                message: MessageId::new(id).unwrap(),
                ordinal: id as u64,
                author: AccountId::new(author).unwrap(),
                quote: text.into(),
                at: UnixMillis::new(0),
                about: About::Member(kit),
                said: Said::Statement {
                    text: text.into(),
                    term: None,
                    durability: qbot_memory::Durability::Lasting,
                    with: None,
                },
            }]
        },
    }
}

fn candidate(index: usize, author: i64, text: &str) -> Candidate {
    Candidate {
        obs: Obs::New(index),
        author: AccountId::new(author).unwrap(),
        at: UnixMillis::new(1000),
        line: format!("#{}", index + 1),
        quote: text.into(),
        text: text.into(),
        term: None,
        durability: qbot_memory::Durability::Lasting,
        with: None,
    }
}

#[test]
fn decisions_are_checked_against_what_is_known_and_who_may_change_it() {
    use qbot_memory::decide::{DecideArgs, DecideCase, Decision, Reason, Ref, check};
    use qbot_memory::memories::Suppression;

    let memories = [
        known(1, "lives in Hangzhou", 11, Origin::Chat, Kind::Statement),
        known(2, "is a teacher", 12, Origin::Chat, Kind::Statement),
        known(
            3,
            "is vegetarian",
            11,
            Origin::Hand {
                by: AccountId::new(11).unwrap(),
            },
            Kind::Statement,
        ),
        known(4, "Kit", 11, Origin::Chat, Kind::Name),
    ];
    let forgotten = [Suppression {
        subject: AccountId::new(11).ok(),
        kind: Kind::Statement,
        text: "plays chess".into(),
        key: "plays chess".into(),
        messages: Vec::new(),
        by: AccountId::new(11).unwrap(),
        at: UnixMillis::new(0),
    }];
    let forgotten: Vec<&Suppression> = forgotten.iter().collect();
    let candidates = [
        candidate(0, 12, "lives in Ningbo"),
        candidate(1, 12, "is no longer a teacher"),
        candidate(2, 12, "eats meat"),
        candidate(3, 12, "do not call them Kit"),
        candidate(4, 11, "plays chess"),
        candidate(5, 11, "lives in Ningbo now"),
    ];
    let numbers = std::collections::BTreeMap::new();
    let case = DecideCase {
        subject: AccountId::new(11).ok(),
        memories: &memories,
        forgotten: &forgotten,
        candidates: &candidates,
        numbers: &numbers,
        zone: &jiff::tz::TimeZone::UTC,
    };
    let args = |decisions: serde_json::Value| -> DecideArgs {
        serde_json::from_value(json!({ "decisions": decisions })).unwrap()
    };
    // Someone else cannot overrule what the member said of themselves, change what was entered
    // by hand, or end the member's name; every statement is decided once.
    let refused = check(
        &case,
        &args(json!([
            { "statement": "o1", "action": "replace", "memory": "m1", "text": "lives in Ningbo" },
            { "statement": "o2", "action": "end", "memory": "m2" },
            { "statement": "o3", "action": "replace", "memory": "m3", "text": "eats meat" },
            { "statement": "o4", "action": "end", "memory": "m4" },
            { "statement": "o5", "action": "new" },
            { "statement": "o5", "action": "new" },
            { "statement": "o9", "action": "new" }
        ])),
    )
    .unwrap_err()
    .join("\n");
    for expected in [
        "o1: m1 rests on what the member said themselves",
        "o3: m3 was entered by hand",
        "o4: m4 is a name; only its member can end it",
        "o5: is decided more than once",
        "\"o9\" is not a statement listed here",
        "o6: has no decision",
    ] {
        assert!(refused.contains(expected), "{expected}\n{refused}");
    }
    assert!(!refused.contains("o2:"), "others may end what others said");

    // The member themselves may; what restates something forgotten is never remembered.
    let decided = check(
        &case,
        &args(json!([
            { "statement": "o1", "action": "reject", "reason": "contradicts" },
            { "statement": "o2", "action": "end", "memory": "m2" },
            { "statement": "o3", "action": "reject", "reason": "contradicts" },
            { "statement": "o4", "action": "reject", "reason": "not_about" },
            { "statement": "o5", "action": "new" },
            { "statement": "o6", "action": "replace", "memory": "m1", "text": "lives in Ningbo" }
        ])),
    )
    .unwrap();
    assert_eq!(decided.decisions[1], Decision::End(MemoryId::new(2)));
    assert_eq!(decided.decisions[4], Decision::Reject(Reason::Forgotten));
    assert!(matches!(
        &decided.decisions[5],
        Decision::Replace { memory, text, .. } if *memory == MemoryId::new(1) && text == "lives in Ningbo"
    ));
    // A statement may support what another of the same case creates, not one that creates
    // nothing.
    let support = check(
        &case,
        &args(json!([
            { "statement": "o1", "action": "reject", "reason": "joke" },
            { "statement": "o2", "action": "support", "memory": "o1" },
            { "statement": "o3", "action": "reject", "reason": "joke" },
            { "statement": "o4", "action": "reject", "reason": "joke" },
            { "statement": "o5", "action": "reject", "reason": "joke" },
            { "statement": "o6", "action": "reject", "reason": "joke" }
        ])),
    )
    .unwrap_err()
    .join("\n");
    assert!(
        support.contains("o2: support needs a memory id"),
        "{support}"
    );
    let fine = check(
        &case,
        &args(json!([
            { "statement": "o1", "action": "reject", "reason": "joke" },
            { "statement": "o2", "action": "reject", "reason": "joke" },
            { "statement": "o3", "action": "reject", "reason": "joke" },
            { "statement": "o4", "action": "reject", "reason": "joke" },
            { "statement": "o5", "action": "reject", "reason": "forgotten" },
            { "statement": "o6", "action": "support", "memory": "m1" }
        ])),
    )
    .unwrap();
    assert_eq!(
        fine.decisions[5],
        Decision::Support(Ref::Memory(MemoryId::new(1)))
    );
}

#[tokio::test]
async fn changes_and_ends_close_memories_and_history_is_kept() {
    let kit = AccountId::new(11).unwrap();
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 2, "I play Genshin")]),
        findings(json!({ "statements": [
            statement("member:2", "plays Genshin", 1, "I play Genshin"),
            statement("member:2", "lives in Hangzhou", 2, "I live in Hangzhou")
        ] })),
        decide(json!([
            { "statement": "o1", "action": "new", "durability": "current" },
            { "statement": "o2", "action": "new" }
        ])),
        episodes(&[(1, 2, "I quit Genshin")]),
        findings(json!({ "statements": [
            statement("member:2", "no longer plays Genshin", 1, "I quit Genshin"),
            statement("member:2", "lives in Ningbo", 2, "moved to Ningbo")
        ] })),
    ]);
    store.add_lines(
        group(),
        [
            said(1, 11, 2, "I play Genshin every night"),
            said(2, 11, 2, "I live in Hangzhou"),
        ],
    );
    job.batch_group(group()).await.unwrap();
    let ids: Vec<MemoryId> = store
        .about(group(), Some(kit))
        .await
        .unwrap()
        .iter()
        .map(|m| m.id)
        .collect();
    fake.push(decide(json!([
        { "statement": "o1", "action": "end", "memory": ids[0].label() },
        { "statement": "o2", "action": "replace", "memory": ids[1].label(), "text": "lives in Ningbo" }
    ])));
    store.add_lines(
        group(),
        [
            said(3, 11, 2, "I quit Genshin last week"),
            said(4, 11, 2, "and I moved to Ningbo"),
        ],
    );
    job.batch_group(group()).await.unwrap();
    let case = task_of(&fake, 5);
    assert!(
        case.contains(&format!("{} (current) plays Genshin", ids[0].label())),
        "the case lists what is known with ids: {case}"
    );
    assert_eq!(
        texts(&store.about(group(), Some(kit)).await.unwrap()),
        [
            ("plays Genshin".to_owned(), false),
            ("lives in Hangzhou".to_owned(), false),
            ("lives in Ningbo".to_owned(), true),
        ],
        "an ended or replaced memory is kept as history"
    );
}

#[tokio::test]
async fn what_was_forgotten_is_not_learned_again_in_any_wording() {
    let kit = AccountId::new(11).unwrap();
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 1, "I live in Hangzhou")]),
        findings(
            json!({ "statements": [statement("member:2", "lives in Hangzhou", 1, "I live in Hangzhou")] }),
        ),
        decide(json!([{ "statement": "o1", "action": "new" }])),
    ]);
    store.add_lines(group(), [said(1, 11, 2, "I live in Hangzhou")]);
    job.batch_group(group()).await.unwrap();
    let memory = store.about(group(), Some(kit)).await.unwrap()[0].clone();
    store
        .forget(group(), memory.id, kit, UnixMillis::new(NOW))
        .await
        .unwrap();

    // Said again, the same and in other words: the decision sees what was forgotten, and what
    // restates it is not remembered whatever is decided.
    fake.push(episodes(&[(1, 2, "Hangzhou is home")]));
    fake.push(findings(json!({ "statements": [
        statement("member:2", "lives in Hangzhou", 1, "Hangzhou is home"),
        statement("member:2", "is based in Hangzhou", 2, "based in Hangzhou")
    ] })));
    fake.push(decide(json!([
        { "statement": "o1", "action": "new" },
        { "statement": "o2", "action": "reject", "reason": "forgotten" }
    ])));
    store.add_lines(
        group(),
        [
            said(2, 11, 2, "Hangzhou is home"),
            said(3, 11, 2, "I'm based in Hangzhou"),
        ],
    );
    job.batch_group(group()).await.unwrap();
    let case = task_of(&fake, 5);
    assert!(
        case.contains("Forgotten at a member's request") && case.contains("- lives in Hangzhou"),
        "{case}"
    );
    assert!(store.about(group(), Some(kit)).await.unwrap().is_empty());
    assert!(store.pending(group()).await.unwrap().is_empty());
    // Entered again by hand, it is known: a person's word lifts what forgot it.
    store
        .add(
            group(),
            &qbot_memory::HandMemory {
                subject: Some(kit),
                kind: Kind::Statement,
                term: None,
                text: "lives in Hangzhou".into(),
                by: kit,
                at: UnixMillis::new(NOW),
            },
        )
        .await
        .unwrap();
    assert!(store.suppressions(group()).await.unwrap().is_empty());
}

#[tokio::test]
async fn command_traffic_is_not_read_but_its_lines_are_covered() {
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 2, "the deploy failed")]),
        nothing_found(),
    ]);
    store.add_lines(
        group(),
        [
            line(1, "the deploy failed"),
            said(2, 11, 2, "/memory forget m3"),
            SliceLine {
                speaker: None,
                member_no: None,
                ..said(3, 0, 0, "[reply:1002][at:2] Forgot m3: lives in Hangzhou")
            },
            line(4, "rolled back now"),
        ],
    );
    let job = job.with_commands(Arc::new(|word: &str| word == "/memory"));
    assert_eq!(job.batch_group(group()).await.unwrap(), 1);
    let lines_message = format!("{:?}", prefix_of(&fake, 0)[2]);
    assert!(
        !lines_message.contains("/memory") && !lines_message.contains("Forgot m3"),
        "{lines_message}"
    );
    assert!(lines_message.contains("#2 [msg:1004]"), "{lines_message}");
    let episodes = store.within(group(), 1, 10).await.unwrap();
    assert_eq!(
        (
            episodes[0].episode.first_ordinal,
            episodes[0].episode.last_ordinal
        ),
        (1, 4)
    );
}

#[tokio::test]
async fn a_name_no_conversation_could_place_is_ruled_on_and_what_was_held_is_filed() {
    let ray = AccountId::new(11).unwrap();
    // Three conversations name "Old Ray"; in none of them does one line show who that is.
    let unplaced = |line: u32| {
        json!({ "line": line, "text": "Old Ray", "refers": "unknown", "role": "about",
                "basis": "none", "shown_by": [] })
    };
    let (fake, store, job) = batch(vec![
        episodes(&[
            (1, 2, "never sleeps"),
            (3, 4, "about the bees"),
            (5, 6, "keeps bees"),
        ]),
        findings(json!({ "references": [unplaced(1)] })),
        findings(json!({ "references": [unplaced(3)] })),
        findings(json!({
            "references": [unplaced(5)],
            "statements": [statement("name:Old Ray", "keeps bees", 5, "keeps bees on the roof")]
        })),
        Step::Reply(FakeReply::new().call(
            "submit_ruling",
            json!({ "verdict": "member:2", "lines": [1002, 1004],
                    "reason": "member:2 answers each time it is said" }),
        )),
        decide(json!([{ "statement": "o1", "action": "new" }])),
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
    assert_eq!(job.batch_group(group()).await.unwrap(), 3);

    // The ruling read the uses side by side, after the same prefix, before any decision.
    assert_eq!(prefix_of(&fake, 4), prefix_of(&fake, 0));
    let case = task_of(&fake, 4);
    assert!(
        case.contains("Task: ruling")
            && case.contains("\\\"Old Ray\\\" was used 3 time(s)")
            && case.contains(">> [msg:1005]"),
        "{case}"
    );
    let names = known_names(&store.names(group()).await.unwrap());
    assert_eq!(names.len(), 1);
    assert_eq!(
        (
            names[0].account,
            names[0].key.as_str(),
            names[0].ruled,
            names[0].about
        ),
        (ray, "old ray", true, 3),
        "every use nobody could place is theirs now"
    );
    // What was said of "Old Ray" waited for the name, and is now known about the member.
    assert!(task_of(&fake, 5).contains("Subject: member:2"));
    assert_eq!(
        texts(
            &store
                .current(group(), &[Some(ray)], Kind::Statement)
                .await
                .unwrap()
        ),
        [("keeps bees".to_owned(), true)]
    );
    assert!(store.pending(group()).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_name_ruled_to_be_nobodys_is_kept_as_what_it_means_and_its_uses_are_not() {
    let unplaced = |line: u32| {
        json!({ "line": line, "text": "Boss", "refers": "unknown", "role": "about",
                "basis": "none", "shown_by": [] })
    };
    let (_, store, job) = batch(vec![
        episodes(&[(1, 1, "Boss"), (2, 2, "Boss"), (3, 3, "Boss")]),
        findings(json!({ "references": [unplaced(1)] })),
        findings(json!({ "references": [unplaced(2)] })),
        findings(json!({ "references": [unplaced(3)] })),
        Step::Reply(FakeReply::new().call(
            "submit_ruling",
            json!({ "verdict": "general", "lines": [], "reason": "said to anyone",
                    "meaning": "what members call whoever is in charge of a game night" }),
        )),
    ]);
    store.add_lines(
        group(),
        [
            said(1, 12, 3, "Boss, start the game"),
            said(2, 13, 4, "yes Boss"),
            said(3, 11, 2, "Boss is late"),
        ],
    );
    job.batch_group(group()).await.unwrap();
    assert_eq!(
        texts(&store.about(group(), None).await.unwrap()),
        [(
            "Boss: what members call whoever is in charge of a game night".to_owned(),
            true
        )]
    );
    assert!(store.pending(group()).await.unwrap().is_empty());
    assert!(store.names(group()).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_statement_about_two_members_is_theirs_and_either_one_may_end_it() {
    let (kit, sam) = (AccountId::new(11).unwrap(), AccountId::new(12).unwrap());
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 2, "we are roommates")]),
        findings(json!({ "statements": [
            { "about": "member:2", "with": "member:3", "text": "shares a flat", "durability": "lasting",
              "line": 1, "quote": "we are roommates" }
        ] })),
        decide(json!([{ "statement": "o1", "action": "new" }])),
        episodes(&[(1, 1, "I moved out")]),
        findings(
            json!({ "statements": [statement("member:2", "no longer shares a flat", 1, "I moved out")] }),
        ),
    ]);
    store.add_lines(
        group(),
        [
            said(1, 12, 3, "[at:2] we are roommates"),
            said(2, 11, 2, "yep"),
        ],
    );
    job.batch_group(group()).await.unwrap();
    let flat = store.about(group(), Some(kit)).await.unwrap()[0].clone();
    assert_eq!(
        (flat.with, flat.own_word()),
        (Some(sam), 1),
        "Sam is a party to it"
    );
    // Kit says it has ended: either party's own word closes it.
    fake.push(decide(
        json!([{ "statement": "o1", "action": "end", "memory": flat.id.label() }]),
    ));
    store.add_lines(group(), [said(3, 11, 2, "I moved out")]);
    job.batch_group(group()).await.unwrap();
    assert!(
        !store
            .memory(group(), flat.id)
            .await
            .unwrap()
            .unwrap()
            .is_current()
    );
}

#[tokio::test]
async fn a_full_subject_gives_up_what_is_worth_least_never_what_this_batch_touched() {
    let kit = AccountId::new(11).unwrap();
    let fake = Arc::new(FakeProvider::new(vec![
        episodes(&[(1, 2, "I play Genshin")]),
        findings(json!({ "statements": [
            statement("member:2", "plays Genshin", 1, "I play Genshin"),
            statement("member:2", "lives in Hangzhou", 2, "I live in Hangzhou")
        ] })),
        decide(json!([
            { "statement": "o1", "action": "new", "durability": "current" },
            { "statement": "o2", "action": "new" }
        ])),
        episodes(&[(1, 1, "I study law")]),
        findings(json!({ "statements": [statement("member:2", "studies law", 1, "I study law")] })),
        decide(json!([{ "statement": "o1", "action": "new" }])),
    ]));
    let store = Arc::new(InMemoryStore::new());
    let job = jobs_over(&fake, &store).with_room(
        qbot_memory::HalfLives::default(),
        qbot_memory::Capacity {
            per_member: 2,
            group: 10,
        },
    );
    store.add_lines(
        group(),
        [
            said(1, 11, 2, "I play Genshin every night"),
            said(2, 11, 2, "I live in Hangzhou"),
        ],
    );
    job.batch_group(group()).await.unwrap();
    store.add_lines(group(), [said(3, 11, 2, "I study law now")]);
    job.batch_group(group()).await.unwrap();
    let left = texts(&store.about(group(), Some(kit)).await.unwrap());
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(
        left.iter().any(|(t, _)| t == "studies law"),
        "what was just said stays"
    );
}

#[tokio::test]
async fn a_backlog_is_read_a_day_at_a_time() {
    const DAY: i64 = 86_400_000;
    let (fake, store, job) = batch(vec![
        episodes(&[(1, 2, "about the deploy")]),
        nothing_found(),
        episodes(&[(1, 2, "about the deploy")]),
        nothing_found(),
    ]);
    let mut all = lines(1..=4);
    for (i, line) in all.iter_mut().enumerate().skip(2) {
        line.at = UnixMillis::new(line.at.get() + 2 * DAY + i as i64);
    }
    store.add_lines(group(), all);
    assert_eq!(job.batch_group(group()).await.unwrap(), 2);
    let ranges: Vec<_> = store
        .within(group(), 1, 10)
        .await
        .unwrap()
        .iter()
        .map(|e| (e.episode.first_ordinal, e.episode.last_ordinal))
        .collect();
    assert_eq!(ranges, [(1, 2), (3, 4)]);
    assert!(
        format!("{:?}", prefix_of(&fake, 2)[2]).contains("Episode 1: They talked"),
        "the second day is read after the first"
    );
}

#[tokio::test]
async fn a_day_too_long_for_the_model_is_read_in_parts_cut_at_its_longest_silence() {
    let (fake, store, job) = batch(vec![
        Step::Fail(LlmError::ContextTooLong {
            limit: None,
            requested: None,
        }),
        episodes(&[(1, 2, "about the deploy")]),
        nothing_found(),
        episodes(&[(1, 2, "about the deploy")]),
        nothing_found(),
    ]);
    let mut all = lines(1..=4);
    // An hour of silence after the second line.
    for line in all.iter_mut().skip(2) {
        line.at = UnixMillis::new(line.at.get() + 3_600_000);
    }
    store.add_lines(group(), all);
    assert_eq!(job.batch_group(group()).await.unwrap(), 2);
    assert_eq!(fake.recorded().len(), 5);
    let ranges: Vec<_> = store
        .within(group(), 1, 10)
        .await
        .unwrap()
        .iter()
        .map(|e| (e.episode.first_ordinal, e.episode.last_ordinal))
        .collect();
    assert_eq!(ranges, [(1, 2), (3, 4)]);
}
