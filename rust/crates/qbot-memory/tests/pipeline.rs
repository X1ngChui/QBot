#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use qbot_core::{AccountId, BatchGrid, GroupId, MessageId, SliceGrid, UnixMillis};
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_llm::{ConvItem, Embedder, FakeEmbedder, LlmError, Provider};
use qbot_memory::extract::{SubmitArgs, validate};
use qbot_memory::{
    BuildError, BuilderConfig, EpisodeBuilder, EpisodeExtractor, EpisodeJobs, EpisodeStore,
    ExtractError, ExtractorConfig, METHOD, MemoryEpisodeStore, SliceContext, SliceLine,
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
    }
}

fn lines(range: std::ops::RangeInclusive<u64>) -> Vec<SliceLine> {
    range
        .map(|n| line(n, &format!("line {n} about the deploy pipeline")))
        .collect()
}

fn good_answer() -> serde_json::Value {
    json!({ "title": "Deploy pipeline", "summary": "They discussed the deploy pipeline.", "evidence": [{ "line": 2, "quote": "about the deploy" }] })
}

fn extractor(steps: Vec<Step>) -> (Arc<FakeProvider>, EpisodeExtractor) {
    let fake = Arc::new(FakeProvider::new(steps));
    (
        fake.clone(),
        EpisodeExtractor::new(fake, ExtractorConfig::default()),
    )
}

fn answer(args: serde_json::Value) -> Step {
    Step::Reply(FakeReply::new().call("submit_episode", args))
}

fn prompt(fake: &FakeProvider, request: usize) -> String {
    let conv = &fake.recorded()[request].conversation;
    conv.items()
        .iter()
        .filter_map(|i| match i {
            ConvItem::Message(m) => Some(format!("{m:?}")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_target_is_numbered_and_the_neighbors_are_marked_context_only() {
    let (fake, ex) = extractor(vec![answer(good_answer())]);
    let (before, target, after) = (lines(1..=3), lines(4..=7), lines(8..=9));
    let out = ex
        .extract(
            &SliceContext {
                previous: &before,
                target: &target,
                next: &after,
            },
            "English",
        )
        .await
        .unwrap();
    assert_eq!(out.attempts, 1);

    let p = prompt(&fake, 0);
    assert!(p.contains(
        "Context before (for understanding references only; do not summarize or quote it)"
    ));
    assert!(p.contains("Target part (summarize this)"));
    assert!(p.contains("Context after (for understanding references only"));
    assert!(
        p.contains("#1 [member:2] line 4 about"),
        "target lines are numbered from 1: {p}"
    );
    assert!(
        p.contains("[member:1] line 3 about"),
        "context lines appear"
    );
    assert!(
        !p.contains("#1 [member:1] line 3"),
        "but context lines are not numbered"
    );
    assert!(p.contains("Summarize and quote only the target part"));
    assert!(p.contains("language: English"));
}

#[tokio::test]
async fn context_sections_are_omitted_when_there_is_no_context() {
    let (fake, ex) = extractor(vec![answer(good_answer())]);
    let target = lines(1..=4);
    ex.extract(
        &SliceContext {
            previous: &[],
            target: &target,
            next: &[],
        },
        "English",
    )
    .await
    .unwrap();
    let p = prompt(&fake, 0);
    assert!(!p.contains("Context before") && !p.contains("Context after"));
}

#[tokio::test]
async fn evidence_must_be_quoted_from_the_target_not_from_context() {
    let target = lines(4..=7);
    let args = |value: serde_json::Value| serde_json::from_value::<SubmitArgs>(value).unwrap();
    let ok = validate(&args(good_answer()), &target).unwrap();
    assert_eq!(ok.2[0].message, target[1].message);

    // "line 3" exists only in the context before the target.
    let from_context = args(
        json!({ "title": "t", "summary": "s", "evidence": [{ "line": 1, "quote": "line 3 about" }] }),
    );
    assert!(
        validate(&from_context, &target)
            .unwrap_err()
            .iter()
            .any(|p| p.contains("not copied exactly"))
    );

    let cases = [
        (
            json!({ "title": " ", "summary": "s", "evidence": [{ "line": 1, "quote": "line 4" }] }),
            "title is empty",
        ),
        (
            json!({ "title": "t", "summary": "", "evidence": [{ "line": 1, "quote": "line 4" }] }),
            "summary is empty",
        ),
        (
            json!({ "title": "t", "summary": "s", "evidence": [] }),
            "one to three",
        ),
        (
            json!({ "title": "t", "summary": "s", "evidence": [{"line":1,"quote":"line 4"},{"line":1,"quote":"line 4"},{"line":1,"quote":"line 4"},{"line":1,"quote":"line 4"}] }),
            "one to three",
        ),
        (
            json!({ "title": "t", "summary": "s", "evidence": [{ "line": 9, "quote": "line 4" }] }),
            "not a line of the target",
        ),
        (
            json!({ "title": "t", "summary": "s", "evidence": [{ "line": 0, "quote": "line 4" }] }),
            "not a line of the target",
        ),
        (
            json!({ "title": "t", "summary": "s", "evidence": [{ "line": 1, "quote": "  " }] }),
            "not copied exactly",
        ),
    ];
    for (value, expected) in cases {
        let problems = validate(&args(value), &target).unwrap_err();
        assert!(
            problems.iter().any(|p| p.contains(expected)),
            "{expected}: {problems:?}"
        );
    }
}

#[tokio::test]
async fn a_rejected_answer_is_corrected_in_the_same_exchange_and_the_attempts_are_bounded() {
    let bad = json!({ "title": "t", "summary": "s", "evidence": [{ "line": 1, "quote": "invented words" }] });
    let (fake, ex) = extractor(vec![answer(bad.clone()), answer(good_answer())]);
    let target = lines(1..=4);
    let out = ex
        .extract(
            &SliceContext {
                previous: &[],
                target: &target,
                next: &[],
            },
            "English",
        )
        .await
        .unwrap();
    assert_eq!(out.attempts, 2);
    assert!(out.usage.input_tokens > 0);
    let second = &fake.recorded()[1].conversation;
    let rejection = second
        .items()
        .iter()
        .filter_map(|i| {
            if let ConvItem::ToolResult(o) = i {
                Some(format!("{o:?}"))
            } else {
                None
            }
        })
        .collect::<String>();
    assert!(
        rejection.contains("not copied exactly") && rejection.contains("Refused"),
        "the reasons go back to the model: {rejection}"
    );

    let (_, ex) = extractor(vec![
        answer(bad.clone()),
        answer(bad.clone()),
        answer(bad.clone()),
        answer(good_answer()),
    ]);
    match ex
        .extract(
            &SliceContext {
                previous: &[],
                target: &target,
                next: &[],
            },
            "English",
        )
        .await
    {
        Err(ExtractError::Invalid {
            attempts: 3,
            problems,
        }) => assert!(!problems.is_empty()),
        other => panic!("expected the bound to apply, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_malformed_and_multiple_calls_are_corrected_too() {
    let target = lines(1..=4);
    let ctx = SliceContext {
        previous: &[],
        target: &target,
        next: &[],
    };
    let (fake, ex) = extractor(vec![
        Step::Reply(FakeReply::new().text("here is a summary")),
        answer(good_answer()),
    ]);
    assert_eq!(ex.extract(&ctx, "English").await.unwrap().attempts, 2);
    assert!(prompt(&fake, 1).contains("you did not call submit_episode"));

    let (_, ex) = extractor(vec![answer(json!({ "title": 5 })), answer(good_answer())]);
    assert_eq!(ex.extract(&ctx, "English").await.unwrap().attempts, 2);

    let two = FakeReply::new()
        .call("submit_episode", good_answer())
        .call("submit_episode", good_answer());
    let (_, ex) = extractor(vec![Step::Reply(two), answer(good_answer())]);
    assert_eq!(ex.extract(&ctx, "English").await.unwrap().attempts, 2);

    let (_, ex) = extractor(vec![Step::Fail(LlmError::Unavailable)]);
    assert!(matches!(
        ex.extract(&ctx, "English").await,
        Err(ExtractError::Model(LlmError::Unavailable))
    ));
}

fn builder(steps: Vec<Step>) -> (Arc<FakeProvider>, Arc<FakeEmbedder>, EpisodeBuilder) {
    let (fake, ex) = extractor(steps);
    let embedder = Arc::new(FakeEmbedder::new(64));
    (
        fake,
        embedder.clone(),
        EpisodeBuilder::new(ex, embedder, BuilderConfig::default()),
    )
}

#[tokio::test]
async fn an_episode_owns_exactly_the_target_range_and_nothing_from_the_context() {
    let (_, embedder, b) = builder(vec![answer(good_answer())]);
    let grid = slice_grid(4, 2, 1, 1);
    let plan = grid.next(4, 100).unwrap(); // batches 1..=2 -> ordinals 5..=12
    assert_eq!(plan.target, 5..=12);
    let (before, target, after) = (lines(1..=4), lines(5..=12), lines(13..=16));
    let built = b
        .build(
            group(),
            &plan,
            4,
            &SliceContext {
                previous: &before,
                target: &target,
                next: &after,
            },
        )
        .await
        .unwrap();
    let e = &built.episode;
    assert_eq!(
        (e.first_batch, e.last_batch, e.first_ordinal, e.last_ordinal),
        (1, 2, 5, 12)
    );
    assert_eq!(
        (
            e.first_message.get(),
            e.last_message.get(),
            e.line_count,
            e.batch_lines
        ),
        (1005, 1012, 8, 4)
    );
    assert_eq!((e.started, e.ended), (target[0].at, target[7].at));
    assert_eq!(e.method, METHOD);
    assert_eq!(e.model, "fake-model");
    let mut expected: Vec<_> = target.iter().filter_map(|l| l.speaker).collect();
    expected.sort();
    expected.dedup();
    assert_eq!(e.participants, expected);
    assert_eq!(
        embedder.calls().last().unwrap(),
        &[format!("{}\n{}", e.title, e.summary)],
        "the title and summary are what gets embedded"
    );
    assert_eq!(built.vector.len(), 64);
    assert_eq!(built.embed_model, "fake-embed");
}

#[tokio::test]
async fn building_needs_lines_and_surfaces_extraction_failures() {
    let (_, _, b) = builder(vec![answer(good_answer())]);
    let grid = slice_grid(4, 1, 1, 1);
    let plan = grid.next(0, 100).unwrap();
    assert!(matches!(
        b.build(
            group(),
            &plan,
            4,
            &SliceContext {
                previous: &[],
                target: &[],
                next: &[]
            }
        )
        .await,
        Err(BuildError::EmptyTarget)
    ));

    let (_, embedder, b) = builder(vec![answer(good_answer())]);
    embedder.fail_next(LlmError::Unavailable);
    // The embedding of the summary fails after a successful extraction.
    let target = lines(1..=4);
    // (The fake embedder fails its next call, which is the summary embedding.)
    assert!(matches!(
        b.build(
            group(),
            &plan,
            4,
            &SliceContext {
                previous: &[],
                target: &target,
                next: &[]
            }
        )
        .await,
        Err(BuildError::Embed(LlmError::Unavailable))
    ));
}

fn slice_grid(batch_lines: u32, slice_batches: u32, previous: u32, next: u32) -> SliceGrid {
    SliceGrid {
        grid: BatchGrid {
            lines_per_batch: batch_lines,
        },
        slice_batches,
        previous_context_batches: previous,
        next_context_batches: next,
        retained_raw_batches: 100,
    }
}

fn jobs(
    steps: Vec<Step>,
    batch_lines: u32,
    slice_batches: u32,
) -> (Arc<FakeProvider>, Arc<MemoryEpisodeStore>, EpisodeJobs) {
    let (fake, _, b) = builder(steps);
    let store = Arc::new(MemoryEpisodeStore::new());
    let grid = slice_grid(batch_lines, slice_batches, 1, 1);
    (fake, store.clone(), EpisodeJobs::new(store, b, grid))
}

fn answers(n: usize) -> Vec<Step> {
    (0..n).map(|_| answer(good_answer())).collect()
}

#[tokio::test]
async fn the_job_extracts_each_complete_slice_once_with_neighbors_as_context() {
    let (fake, store, job) = jobs(answers(6), 10, 3);
    store.add_lines(group(), lines(1..=100));

    assert_eq!(
        job.extract_group(group()).await.unwrap(),
        3,
        "ordinals 1-30, 31-60, 61-90; 91-100 is only one batch"
    );
    assert_eq!(store.covered_through(group()).await.unwrap(), 90);
    let ranges: Vec<_> = store
        .within(group(), 1, 1000)
        .await
        .unwrap()
        .iter()
        .map(|e| (e.episode.first_ordinal, e.episode.last_ordinal))
        .collect();
    assert_eq!(
        ranges,
        [(1, 30), (31, 60), (61, 90)],
        "the episodes tile the archive exactly, context never extends them"
    );
    assert_eq!(
        job.extract_group(group()).await.unwrap(),
        0,
        "nothing left: idempotent"
    );
    assert_eq!(fake.recorded().len(), 3);

    // The middle slice saw the batch before it and the batch after it, as context.
    let p = prompt(&fake, 1);
    assert!(p.contains("[member:") && p.contains("line 30 about") && p.contains("line 61 about"));
    assert!(p.contains("#1 ") && p.contains("line 31 about") && p.contains("line 60 about"));
    assert!(
        !p.contains("line 20 about") && !p.contains("line 71 about"),
        "only the nearest batch of each neighbor"
    );
    // The first slice has no earlier context.
    assert!(!prompt(&fake, 0).contains("Context before"));

    // New lines complete the next slice.
    store.add_lines(group(), lines(101..=120));
    assert_eq!(job.extract_group(group()).await.unwrap(), 1);
    assert_eq!(store.covered_through(group()).await.unwrap(), 120);
}

#[tokio::test]
async fn extraction_uses_fewer_following_batches_when_fewer_exist_and_never_a_partial_one() {
    let (fake, store, job) = jobs(answers(1), 10, 1);
    store.add_lines(group(), lines(1..=14)); // batch 0 complete; batch 1 only partly there
    assert_eq!(job.extract_group(group()).await.unwrap(), 1);
    let p = prompt(&fake, 0);
    assert!(
        !p.contains("Context after") && !p.contains("line 11 about"),
        "a partial batch is not context: {p}"
    );
}

#[tokio::test]
async fn the_context_sizes_are_settings() {
    // Two batches before and two after, with a slice of one batch of 10 lines.
    let (fake, _, b) = builder(answers(6));
    let store = Arc::new(MemoryEpisodeStore::new());
    let wide = slice_grid(10, 1, 2, 2);
    let job = EpisodeJobs::new(store.clone(), b, wide);
    store.add_lines(group(), lines(1..=100));
    // Put the first episode at batch 4 by covering batches 0..=3 with a stand-in episode.
    let mut e = qbot_memory::conformance::episode(group(), 1, 40, "earlier");
    e.last_batch = 3;
    store.insert(&e, &[1.0], "m").await.unwrap();
    assert_eq!(
        job.extract_group(group()).await.unwrap(),
        6,
        "batches 4..=9 are each one slice"
    );
    let p = prompt(&fake, 0);
    assert!(
        p.contains("line 31 about") && p.contains("line 40 about"),
        "two earlier batches: 31..40 is the batch directly before"
    );
    assert!(
        p.contains("line 21 about") || !p.contains("line 20 about"),
        "bounded by the setting"
    );
}

#[tokio::test]
async fn a_failed_slice_leaves_no_episode_and_is_retried_from_the_same_point() {
    let (_, store, job) = jobs(
        vec![
            answer(good_answer()),
            Step::Fail(LlmError::Unavailable),
            answer(good_answer()),
            answer(good_answer()),
        ],
        10,
        1,
    );
    store.add_lines(group(), lines(1..=30));
    assert!(
        job.extract_group(group()).await.is_err(),
        "the second slice fails"
    );
    assert_eq!(
        store.covered_through(group()).await.unwrap(),
        10,
        "the first slice was stored, the failed one was not"
    );
    assert_eq!(
        job.extract_group(group()).await.unwrap(),
        2,
        "the retry resumes at the failed slice"
    );
    assert_eq!(store.covered_through(group()).await.unwrap(), 30);
}

#[tokio::test]
async fn an_archive_with_a_gap_is_refused_not_papered_over() {
    let (_, store, job) = jobs(answers(1), 10, 1);
    let mut all = lines(1..=20);
    all.remove(4);
    store.add_lines(group(), all);
    assert!(job.extract_group(group()).await.is_err());
    assert_eq!(store.covered_through(group()).await.unwrap(), 0);
}

#[tokio::test]
async fn groups_are_independent_and_only_extract_jobs_are_accepted() {
    use qbot_sched::{JobKind, JobRunner};
    let (_, store, job) = jobs(answers(2), 10, 1);
    store.add_lines(group(), lines(1..=10));
    store.add_lines(GroupId::new(2).unwrap(), lines(1..=10));
    job.run(JobKind::Extract, Some(group())).await.unwrap();
    assert_eq!(store.covered_through(group()).await.unwrap(), 10);
    assert_eq!(
        store
            .covered_through(GroupId::new(2).unwrap())
            .await
            .unwrap(),
        0
    );
    assert!(job.run(JobKind::Extract, None).await.is_err());
    assert!(job.run(JobKind::Backup, Some(group())).await.is_err());
}

#[tokio::test]
async fn the_in_memory_store_satisfies_the_episode_store_contract() {
    qbot_memory::conformance::run(&MemoryEpisodeStore::new()).await;
}

// ----- recall and history composition -----

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
        RecallParams {
            limit: 2,
            max_distance: 0.9,
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
        RecallParams {
            limit: 5,
            max_distance: 0.05,
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
async fn the_in_memory_fact_store_defines_the_contract() {
    qbot_memory::facts_conformance::run(&qbot_memory::facts::MemoryFactStore::new(), &[5, 9]).await;
}

#[test]
fn earned_confidence_grows_with_independent_support_and_fades_without_it() {
    use qbot_memory::facts::{DecayPolicy, Fact, FactId, FactStatus, wilson};
    assert!(
        (wilson(1) - 0.27).abs() < 0.01
            && (wilson(3) - 0.53).abs() < 0.01
            && (wilson(8) - 0.75).abs() < 0.02
    );
    let policy = DecayPolicy::default();
    let fact = |supports, decay| Fact {
        id: FactId(1),
        group: qbot_core::GroupId::new(1).unwrap(),
        subject: None,
        predicate: "likes".into(),
        key: "x".into(),
        object: "x".into(),
        label: None,
        status: FactStatus::Active,
        supports,
        first_seen: qbot_core::UnixMillis::new(0),
        last_confirmed: qbot_core::UnixMillis::new(0),
        ended: None,
        decay,
    };
    let day = 86_400_000;
    let now = |days: i64| qbot_core::UnixMillis::new(days * day);
    let once = fact(1, qbot_memory::predicates::DecayClass::Default);
    assert!(
        (policy.confidence(&once, now(30)) - wilson(1) / 2.0).abs() < 1e-9,
        "one half-life halves it"
    );
    // Forgotten below 0.05: a single mention lasts about 2.4 half-lives, eight mentions about 3.9.
    assert!(policy.confidence(&once, now(70)) > 0.05 && policy.confidence(&once, now(80)) < 0.05);
    let often = fact(8, qbot_memory::predicates::DecayClass::Default);
    assert!(policy.confidence(&often, now(110)) > 0.05);
}

fn said(ordinal: u64, account: i64, member: u32, text: &str) -> SliceLine {
    SliceLine {
        ordinal,
        message: MessageId::new(1000 + ordinal as i64).unwrap(),
        speaker: AccountId::new(account).ok(),
        member_no: Some(member),
        at: UnixMillis::new(1_800_000_000_000 + ordinal as i64 * 1000),
        text: text.into(),
    }
}

fn answer_with_findings() -> serde_json::Value {
    json!({
        "title": "Moving and pets",
        "summary": "Kit moved to Hangzhou; the group explained GG.",
        "evidence": [{ "line": 1, "quote": "I moved to Hangzhou" }],
        "names": [
            { "member": 2, "name": "Kit", "line": 1, "quote": "call me Kit" },
            { "member": 2, "name": "Kit", "line": 2, "quote": "[at:2] Kit likes" },
            { "member": 3, "name": "Boss", "line": 1, "quote": "call me Kit" }
        ],
        "facts": [
            { "member": 2, "predicate": "lives_in", "object": "Hangzhou", "line": 1, "quote": "I moved to Hangzhou" },
            { "member": 2, "predicate": "likes", "object": "cats", "line": 2, "quote": "[at:2] Kit likes cats" },
            { "member": 1, "predicate": "likes", "object": "dogs", "line": 2, "quote": "Kit likes cats" },
            { "member": 2, "predicate": "astrology", "object": "leo", "line": 1, "quote": "I moved" },
            { "member": 3, "predicate": "likes", "object": "tea", "line": 4, "quote": "tea" }
        ],
        "knowledge": [
            { "kind": "term", "term": "GG", "text": "good game", "line": 3, "quote": "GG means good game" },
            { "kind": "topic", "text": "travel and pets", "line": 1, "quote": "I moved" },
            { "kind": "term", "term": "AFK", "text": "away", "line": 3, "quote": "GG means" }
        ]
    })
}

#[tokio::test]
async fn an_episodes_findings_become_names_facts_and_group_knowledge_once() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::facts::{FactStore, MemoryFactStore};
    use qbot_memory::identity::{AliasStatus, AliasTarget, IdentityPolicy};
    use qbot_memory::predicates::Predicates;
    use qbot_memory::{IdentityStore, MemoryIdentityStore};

    // The rejected findings go back once; the same answer again is kept as it stands.
    let (fake, _, b) = builder(vec![
        answer(answer_with_findings()),
        answer(answer_with_findings()),
    ]);
    let store = Arc::new(MemoryEpisodeStore::new());
    store.add_lines(
        group(),
        [
            said(1, 11, 2, "I moved to Hangzhou last year, call me Kit"),
            said(2, 12, 3, "[at:2] Kit likes cats a lot"),
            said(3, 10, 1, "here GG means good game"),
        ],
    );
    let facts = Arc::new(MemoryFactStore::new());
    let identity = Arc::new(MemoryIdentityStore::new(IdentityPolicy::default()));
    for account in [10, 11, 12] {
        identity
            .seen(AccountId::new(account).unwrap(), UnixMillis::new(0))
            .await
            .unwrap();
    }
    let consolidator = Arc::new(Consolidator::new(
        facts.clone(),
        identity.clone(),
        Arc::new(Predicates::builtin()),
    ));
    let job =
        EpisodeJobs::new(store.clone(), b, slice_grid(3, 1, 1, 1)).with_consolidator(consolidator);
    assert_eq!(job.extract_group(group()).await.unwrap(), 1);

    // The model was told about the predicates.
    let p = prompt(&fake, 0);
    assert!(
        p.contains("- lives_in (one value): The city or region where they live now"),
        "{p}"
    );

    let kit = AccountId::new(11).unwrap();
    let about_kit: Vec<(String, String)> = facts
        .current(group(), Some(kit))
        .await
        .unwrap()
        .into_iter()
        .map(|f| (f.predicate, f.object))
        .collect();
    assert_eq!(
        about_kit,
        [
            ("likes".to_owned(), "cats".to_owned()),
            ("lives_in".to_owned(), "Hangzhou".to_owned())
        ]
    );
    assert!(
        facts
            .current(group(), AccountId::new(10).ok())
            .await
            .unwrap()
            .is_empty(),
        "a fact from someone else's line about an unmentioned member is dropped"
    );
    let group_facts: Vec<(String, Option<String>, String)> = facts
        .current(group(), None)
        .await
        .unwrap()
        .into_iter()
        .map(|f| (f.predicate, f.label, f.object))
        .collect();
    assert_eq!(
        group_facts,
        [
            (
                "group_term".to_owned(),
                Some("GG".to_owned()),
                "good game".to_owned()
            ),
            ("group_topic".to_owned(), None, "travel and pets".to_owned())
        ]
    );

    // The name is extracted evidence: a lead, never confirmed by extraction alone, counted once
    // per episode however often it is cited.
    let names = identity
        .names_of(group(), AliasTarget::Account(kit))
        .await
        .unwrap();
    assert_eq!(names.len(), 1);
    assert_eq!(
        (names[0].text.as_str(), names[0].status),
        ("kit", AliasStatus::Candidate)
    );
    assert!(
        (names[0].confidence - 0.25).abs() < 1e-6,
        "{}",
        names[0].confidence
    );
    assert!(
        identity
            .names_of(group(), AliasTarget::Account(AccountId::new(12).unwrap()))
            .await
            .unwrap()
            .is_empty()
    );

    // The episode keeps exactly what was kept, and is marked applied.
    let stored = store.within(group(), 1, 3).await.unwrap();
    assert_eq!(
        (
            stored[0].episode.findings.names.len(),
            stored[0].episode.findings.facts.len(),
            stored[0].episode.findings.knowledge.len()
        ),
        (2, 2, 2)
    );
    assert!(store.unconsolidated(group()).await.unwrap().is_empty());
    assert_eq!(stored[0].episode.method, "slice-v3");
}

#[tokio::test]
async fn an_episode_stored_but_not_yet_applied_is_applied_by_the_next_run() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::facts::{FactStore, MemoryFactStore};
    use qbot_memory::identity::IdentityPolicy;
    use qbot_memory::predicates::Predicates;
    use qbot_memory::{IdentityStore, MemoryIdentityStore};

    let lines = [
        said(1, 11, 2, "I moved to Hangzhou last year, call me Kit"),
        said(2, 12, 3, "[at:2] Kit likes cats a lot"),
        said(3, 10, 1, "here GG means good game"),
    ];
    // First a run with nothing to apply findings to, as if the process died before applying them.
    let (_, _, b) = builder(vec![answer(answer_with_findings())]);
    let store = Arc::new(MemoryEpisodeStore::new());
    store.add_lines(group(), lines.clone());
    EpisodeJobs::new(store.clone(), b, slice_grid(3, 1, 1, 1))
        .extract_group(group())
        .await
        .unwrap();
    assert_eq!(store.unconsolidated(group()).await.unwrap().len(), 1);

    let facts = Arc::new(MemoryFactStore::new());
    let identity = Arc::new(MemoryIdentityStore::new(IdentityPolicy::default()));
    for account in [10, 11, 12] {
        identity
            .seen(AccountId::new(account).unwrap(), UnixMillis::new(0))
            .await
            .unwrap();
    }
    let consolidator = Arc::new(Consolidator::new(
        facts.clone(),
        identity,
        Arc::new(Predicates::builtin()),
    ));
    let (_, _, b) = builder(vec![]);
    let job =
        EpisodeJobs::new(store.clone(), b, slice_grid(3, 1, 1, 1)).with_consolidator(consolidator);
    assert_eq!(
        job.extract_group(group()).await.unwrap(),
        0,
        "nothing new to extract"
    );
    assert_eq!(
        facts
            .current(group(), AccountId::new(11).ok())
            .await
            .unwrap()
            .len(),
        2,
        "but the stored findings were applied"
    );
    assert!(store.unconsolidated(group()).await.unwrap().is_empty());
    // Running again changes nothing: an episode supports a fact once.
    job.extract_group(group()).await.unwrap();
    assert!(
        facts
            .current(group(), AccountId::new(11).ok())
            .await
            .unwrap()
            .iter()
            .all(|f| f.supports == 1)
    );
}

#[tokio::test]
async fn the_in_memory_note_store_meets_the_note_contract() {
    let accounts = [1, 2, 3].map(|n| qbot_core::AccountId::new(n).unwrap());
    qbot_memory::notes_conformance::run(&qbot_memory::MemoryNoteStore::default(), accounts).await;
}

#[tokio::test]
async fn rejected_findings_go_back_once_and_the_corrected_answer_is_kept() {
    let target = vec![
        said(1, 11, 2, "we all call that cafe the Den"),
        said(2, 12, 3, "the Den is the cat cafe by the lake"),
    ];
    let summary = |quote: &str| {
        json!({
            "title": "The cafe",
            "summary": "member:2 said the group calls the cafe the Den.",
            "evidence": [{ "line": 1, "quote": "call that cafe the Den" }],
            "knowledge": [
                { "kind": "term", "term": "the Den", "text": "the cat cafe by the lake", "line": 2, "quote": quote }
            ]
        })
    };
    let (fake, ex) = extractor(vec![
        answer(summary("the cafe by the lake")),
        answer(summary("the cat cafe by the lake")),
    ]);
    let out = ex
        .extract(
            &SliceContext {
                previous: &[],
                target: &target,
                next: &[],
            },
            "English",
        )
        .await
        .unwrap();
    assert_eq!(out.attempts, 2);
    assert!(out.dropped.is_empty(), "{:?}", out.dropped);
    assert!(matches!(
        &out.findings.knowledge[..],
        [qbot_memory::findings::KnowledgeFinding::Term { term, .. }] if term == "the Den"
    ));
    let sent_back = format!("{:?}", fake.recorded()[1].conversation.items().last());
    assert!(
        sent_back.contains("The episode is fine, but some items were not accepted")
            && sent_back.contains("knowledge item 1: the quote for line #2"),
        "{sent_back}"
    );
}

#[tokio::test]
async fn a_term_may_be_explained_by_a_line_that_does_not_repeat_it() {
    let target = vec![
        said(1, 11, 2, "we all call that place the Den"),
        said(2, 12, 3, "the cat cafe by the lake, right?"),
    ];
    let (_, ex) = extractor(vec![answer(json!({
        "title": "The cafe",
        "summary": "member:2 said the group calls a place the Den.",
        "evidence": [{ "line": 1, "quote": "call that place the Den" }],
        "knowledge": [
            { "kind": "term", "term": "the Den", "text": "the cat cafe by the lake", "line": 2, "quote": "the cat cafe by the lake" },
            { "kind": "term", "term": "the Burrow", "text": "invented", "line": 2, "quote": "the cat cafe" }
        ]
    }))]);
    let ctx = SliceContext {
        previous: &[],
        target: &target,
        next: &[],
    };
    let out = ex.extract(&ctx, "English").await.unwrap();
    assert_eq!(out.findings.knowledge.len(), 1, "{:?}", out.dropped);
    assert_eq!(
        out.dropped,
        ["knowledge item 2: the term \"the Burrow\" is not written in the target part"]
    );
}
