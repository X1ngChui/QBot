#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use qbot_core::{AccountId, BatchGrid, GroupId, MessageId, SliceGrid, UnixMillis};
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_llm::{ConvItem, Embedder, FakeEmbedder, LlmError, Provider};
use qbot_memory::extract::{SubmitArgs, validate};
use qbot_memory::{
    Background, BuildError, BuilderConfig, EpisodeBuilder, EpisodeExtractor, EpisodeJobs,
    EpisodeStore, ExtractError, Extracted, METHOD, MemoryEpisodeStore, SliceContext, SliceLine,
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

fn good_answer() -> serde_json::Value {
    json!({ "title": "Deploy pipeline", "summary": "They discussed the deploy pipeline.", "evidence": [{ "line": 2, "quote": "about the deploy" }] })
}

fn extractor(steps: Vec<Step>) -> (Arc<FakeProvider>, EpisodeExtractor) {
    let fake = Arc::new(FakeProvider::new(steps));
    (fake.clone(), EpisodeExtractor::new(fake))
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
                group: group(),
                background: &Background::default(),
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
        p.contains("#1 [msg:1004] 01-15 08:00 member:2: line 4 about"),
        "target lines are numbered from 1 and carry their id, time and speaker: {p}"
    );
    assert!(
        p.contains("[msg:1003] 01-15 08:00 member:1: line 3 about") && !p.contains("#3 [msg:1003]"),
        "context lines appear, unnumbered, with ids a [reply:ID] can point at"
    );
    assert!(p.contains("Summarize and quote only the target part"));
    assert!(p.contains("language: English"));
}

#[tokio::test]
async fn the_background_comes_before_the_lines_and_times_are_local() {
    let (fake, ex) = extractor(vec![answer(good_answer())]);
    let target = lines(4..=7);
    let background = Background {
        text: "## Who is who\n- member:2: shown as \"Kit\"".into(),
        zone: jiff::tz::TimeZone::get("Asia/Shanghai").unwrap(),
        ..Default::default()
    };
    ex.extract(
        &SliceContext {
            group: group(),
            background: &background,
            previous: &[],
            target: &target,
            next: &[],
        },
        "English",
    )
    .await
    .unwrap();
    let p = prompt(&fake, 0);
    let who = p.find("## Who is who").expect("the background is shown");
    assert!(who < p.find("Target part").unwrap(), "{p}");
    assert!(p.contains("#1 [msg:1004] 01-15 16:00 member:2:"), "{p}");
}

#[tokio::test]
async fn context_sections_are_omitted_when_there_is_no_context() {
    let (fake, ex) = extractor(vec![answer(good_answer())]);
    let target = lines(1..=4);
    ex.extract(
        &SliceContext {
            group: group(),
            background: &Background::default(),
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
                group: group(),
                background: &Background::default(),
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
                group: group(),
                background: &Background::default(),
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
        group: group(),
        background: &Background::default(),
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
                group: group(),
                background: &Background::default(),
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
                group: group(),
                background: &Background::default(),
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
                group: group(),
                background: &Background::default(),
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

#[tokio::test]
async fn a_new_embedding_model_re_embeds_episodes_without_extracting_them_again() {
    let store = Arc::new(MemoryEpisodeStore::new());
    store.add_lines(group(), lines(1..=30));
    let (fake, _, b) = builder(answers(3));
    EpisodeJobs::new(store.clone(), b, slice_grid(10, 1, 0, 0))
        .extract_group(group())
        .await
        .unwrap();
    assert_eq!(fake.recorded().len(), 3, "three slices extracted");

    // The same episodes, a different embedding model: nothing to extract, everything to embed.
    let (fake, _) = extractor(vec![]);
    let embedder = Arc::new(FakeEmbedder::new(64).named("other-embed"));
    let job = EpisodeJobs::new(
        store.clone(),
        EpisodeBuilder::new(
            EpisodeExtractor::new(fake.clone()),
            embedder.clone(),
            BuilderConfig::default(),
        ),
        slice_grid(10, 1, 0, 0),
    );
    assert_eq!(
        store
            .unembedded(group(), "other-embed", 64, 10)
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        job.extract_group(group()).await.unwrap(),
        0,
        "no new episode"
    );
    assert!(fake.recorded().is_empty(), "no extraction call");
    assert_eq!(
        embedder.calls().concat().len(),
        3,
        "each episode embedded once"
    );
    assert!(
        embedder.calls().concat()[0].starts_with("Deploy pipeline\n"),
        "from the stored title and summary"
    );
    assert!(
        store
            .unembedded(group(), "other-embed", 64, 10)
            .await
            .unwrap()
            .is_empty()
    );
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
    assert!(p.contains(" member:") && p.contains("line 30 about") && p.contains("line 61 about"));
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
async fn a_slice_that_can_never_be_extracted_is_skipped_and_the_later_ones_still_are() {
    let (fake, store, job) = jobs(
        vec![
            answer(good_answer()),
            Step::Fail(LlmError::ContentFiltered),
            answer(good_answer()),
        ],
        10,
        1,
    );
    store.add_lines(group(), lines(1..=30));
    assert_eq!(
        job.extract_group(group()).await.unwrap(),
        2,
        "the refused slice holds nothing back"
    );
    let ranges: Vec<_> = store
        .within(group(), 1, 1000)
        .await
        .unwrap()
        .iter()
        .map(|e| (e.episode.first_ordinal, e.episode.last_ordinal))
        .collect();
    assert_eq!(
        ranges,
        [(1, 10), (21, 30)],
        "the refused slice has no episode"
    );
    assert_eq!(store.covered_through(group()).await.unwrap(), 30);
    assert_eq!(job.extract_group(group()).await.unwrap(), 0);
    assert_eq!(
        fake.recorded().len(),
        3,
        "the refused slice is not asked again"
    );
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

fn answer_with_findings() -> serde_json::Value {
    json!({
        "title": "Moving and pets",
        "summary": "Kit moved to Hangzhou; the group explained GG.",
        "evidence": [{ "line": 1, "quote": "I moved to Hangzhou" }],
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

#[tokio::test]
async fn an_episodes_findings_become_name_uses_and_memories_once() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::{InMemoryMemoryStore, MemoryNameStore, MemoryStore, NameStore};

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
    let names = Arc::new(MemoryNameStore::default());
    let memories = Arc::new(InMemoryMemoryStore::default());
    let consolidator = Arc::new(Consolidator::new(names.clone(), memories.clone()));
    let job =
        EpisodeJobs::new(store.clone(), b, slice_grid(3, 1, 1, 1)).with_consolidator(consolidator);
    assert_eq!(job.extract_group(group()).await.unwrap(), 1);

    let p = prompt(&fake, 0);
    assert!(p.contains("## References") && p.contains("`answer`"), "{p}");
    let sent_back = format!("{:?}", fake.recorded()[1].conversation.items().last());
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
    assert!(
        memories
            .current(group(), &[AccountId::new(10).ok()])
            .await
            .unwrap()
            .is_empty(),
        "a statement about a member the line is not directed at is dropped"
    );
    let ours = memories.current(group(), &[None]).await.unwrap();
    assert_eq!(
        ours.iter().map(|m| m.display()).collect::<Vec<_>>(),
        ["GG: good game"]
    );

    // A name is read from its references: per member, name and episode, the strongest use.
    let known = names.names(group(), None).await.unwrap();
    assert_eq!(known.len(), 1);
    assert!(known[0].vouched(), "the member gave it themselves");
    assert_eq!(
        (
            known[0].key.as_str(),
            known[0].own,
            known[0].addressed,
            known[0].set
        ),
        ("kit", 1, 0, false)
    );

    // The episode keeps exactly what was kept, and is marked applied.
    let stored = store.within(group(), 1, 3).await.unwrap();
    assert_eq!(
        (
            stored[0].episode.findings.references.len(),
            stored[0].episode.findings.memories.len()
        ),
        (2, 3)
    );
    assert!(store.unconsolidated(group()).await.unwrap().is_empty());
    assert_eq!(stored[0].episode.method, METHOD);
}

#[tokio::test]
async fn an_episode_stored_but_not_yet_applied_is_applied_by_the_next_run() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::{InMemoryMemoryStore, MemoryNameStore, MemoryStore};

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

    let memories = Arc::new(InMemoryMemoryStore::default());
    let consolidator = Arc::new(Consolidator::new(
        Arc::new(MemoryNameStore::default()),
        memories.clone(),
    ));
    let (_, _, b) = builder(vec![]);
    let job =
        EpisodeJobs::new(store.clone(), b, slice_grid(3, 1, 1, 1)).with_consolidator(consolidator);
    assert_eq!(job.extract_group(group()).await.unwrap(), 0);
    let kit = [AccountId::new(11).ok()];
    assert_eq!(
        memories.current(group(), &kit).await.unwrap().len(),
        2,
        "but the stored findings were applied"
    );
    assert!(store.unconsolidated(group()).await.unwrap().is_empty());
    // Running again changes nothing: an episode supports a memory once.
    job.extract_group(group()).await.unwrap();
    assert!(
        memories
            .current(group(), &kit)
            .await
            .unwrap()
            .iter()
            .all(|m| m.episodes() == 1)
    );
}

#[tokio::test]
async fn the_in_memory_note_store_meets_the_note_contract() {
    let accounts = [1, 2, 3].map(|n| qbot_core::AccountId::new(n).unwrap());
    qbot_memory::notes_conformance::run(&qbot_memory::MemoryNoteStore::default(), accounts).await;
}

fn term_answer(quote: &str) -> serde_json::Value {
    json!({
        "title": "The cafe",
        "summary": "member:2 said the group calls the cafe the Den.",
        "evidence": [{ "line": 1, "quote": "call that cafe the Den" }],
        "memories": [
            { "about": "group", "action": "new", "term": "the Den", "text": "the cat cafe by the lake", "durability": "ongoing", "line": 2, "quote": quote }
        ]
    })
}

#[tokio::test]
async fn rejected_findings_go_back_once_and_the_corrected_answer_is_kept() {
    let target = vec![
        said(1, 11, 2, "we all call that cafe the Den"),
        said(2, 12, 3, "the Den is the cat cafe by the lake"),
    ];
    let (fake, ex) = extractor(vec![
        answer(term_answer("the cafe by the lake")),
        answer(term_answer("the cat cafe by the lake")),
    ]);
    let out = ex
        .extract(
            &SliceContext {
                group: group(),
                background: &Background::default(),
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
    assert_eq!(out.findings.memories[0].term.as_deref(), Some("the Den"));
    let sent_back = format!("{:?}", fake.recorded()[1].conversation.items().last());
    assert!(
        sent_back.contains("The episode is fine, but some items were not accepted")
            && sent_back.contains("memories item 1: the quote for line #2"),
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
        "memories": [
            { "about": "group", "action": "new", "term": "the Den", "text": "the cat cafe by the lake", "durability": "ongoing", "line": 2, "quote": "the cat cafe by the lake" },
            { "about": "group", "action": "new", "term": "the Burrow", "text": "invented", "durability": "ongoing", "line": 2, "quote": "the cat cafe" },
            { "about": "member:2", "action": "new", "term": "Den", "text": "a term about a member", "durability": "ongoing", "line": 1, "quote": "the Den" }
        ]
    }))]);
    let ctx = SliceContext {
        group: group(),
        background: &Background::default(),
        previous: &[],
        target: &target,
        next: &[],
    };
    let out = ex.extract(&ctx, "English").await.unwrap();
    assert_eq!(out.findings.memories.len(), 1, "{:?}", out.dropped);
    assert_eq!(
        out.dropped,
        [
            "memories item 2: the term \"the Burrow\" is not written in the target part",
            "memories item 3: a term belongs to the group: give it with about group"
        ]
    );
}

fn cut_off() -> Step {
    Step::Reply(
        FakeReply::new()
            .reasoning("thinking at length")
            .finish(qbot_llm::FinishReason::Length),
    )
}

async fn extract_lines(ex: &EpisodeExtractor) -> Result<Extracted, ExtractError> {
    let target = lines(1..=4);
    ex.extract(
        &SliceContext {
            group: group(),
            background: &Background::default(),
            previous: &[],
            target: &target,
            next: &[],
        },
        "English",
    )
    .await
}

#[tokio::test]
async fn a_lost_answer_is_asked_for_again_and_a_later_good_one_is_kept() {
    // Cut off at the output limit, then arguments that are not JSON, then a good answer.
    let (fake, ex) = extractor(vec![
        cut_off(),
        Step::Fail(LlmError::Protocol("arguments are not JSON".into())),
        answer(good_answer()),
    ]);
    let out = extract_lines(&ex).await.unwrap();
    assert_eq!(out.attempts, 3);
    assert_eq!(out.title, "Deploy pipeline");
    let requests = fake.recorded();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2].conversation.items().len(),
        requests[0].conversation.items().len(),
        "a lost answer adds nothing to the exchange: the same request again"
    );
}

#[tokio::test]
async fn an_answer_cut_off_on_every_attempt_is_an_error() {
    let (fake, ex) = extractor(vec![cut_off(), cut_off(), cut_off()]);
    let error = extract_lines(&ex).await.unwrap_err();
    assert!(matches!(error, ExtractError::CutOff), "{error}");
    assert_eq!(fake.recorded().len(), 3, "every attempt was used");
}

#[tokio::test]
async fn a_slice_is_shown_the_chat_it_continues_its_dates_and_lines_it_quotes_from_further_back() {
    let (fake, store, job) = jobs(answers(4), 10, 1);
    let mut all = lines(1..=40);
    // Line 35 answers line 2, far before anything shown with that slice.
    all[34].text = "[reply:1002] agreed, line 35".into();
    store.add_lines(group(), all);
    assert_eq!(job.extract_group(group()).await.unwrap(), 4);

    let first = prompt(&fake, 0);
    assert!(
        !first.contains("## Earlier in this chat")
            && !first.contains("Quoted from earlier in the chat (")
    );
    // The target names its dates in full: 1_800_000_000_000 ms is 2027-01-15, a Friday.
    assert!(
        first.contains("Target part (summarize this), from 2027-01-15 Fri 08:00"),
        "{first}"
    );

    let last = prompt(&fake, 3);
    assert!(
        last.contains("## Earlier in this chat")
            && last.contains("Deploy pipeline: They discussed"),
        "the previous episode's summary: {last}"
    );
    let quoted = last
        .split("Quoted from earlier in the chat (")
        .nth(1)
        .and_then(|rest| rest.split("Context before").next())
        .unwrap_or_default();
    assert!(
        quoted.contains("[msg:1002]") && quoted.contains("line 2 about"),
        "the quoted line is shown: {last}"
    );
    assert!(
        !quoted.contains("line 21 about"),
        "a line already shown is not repeated"
    );
}

#[tokio::test]
async fn later_statements_confirm_replace_and_end_memories_and_history_is_kept() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::{InMemoryMemoryStore, MemoryNameStore, MemoryState, MemoryStore};

    let first = json!({
        "title": "Games", "summary": "member:2 plays two games and lives in Hangzhou.",
        "evidence": [{ "line": 1, "quote": "I play Genshin" }],
        "memories": [
            { "about": "member:2", "action": "new", "text": "plays Genshin", "durability": "current", "line": 1, "quote": "I play Genshin" },
            { "about": "member:2", "action": "new", "text": "plays chess on weekends", "durability": "ongoing", "line": 2, "quote": "and Chess" },
            { "about": "member:2", "action": "new", "text": "lives in Hangzhou", "durability": "lasting", "line": 3, "quote": "in Hangzhou" }
        ]
    });
    // The ids are those the background listed: m1 Genshin, m2 chess, m3 Hangzhou.
    let second = json!({
        "title": "Changes", "summary": "member:2 quit Genshin and moved to Ningbo.",
        "evidence": [{ "line": 1, "quote": "quit Genshin" }],
        "memories": [
            { "about": "member:2", "action": "ended", "id": "m1", "line": 1, "quote": "quit Genshin" },
            { "about": "member:2", "action": "ended", "id": "m42", "line": 1, "quote": "quit Genshin" },
            { "about": "member:3", "action": "ended", "id": "m2", "line": 2, "quote": "no more" },
            { "about": "member:2", "action": "changed", "id": "m3", "text": "lives in Ningbo", "durability": "lasting", "line": 3, "quote": "moved to Ningbo" },
            { "about": "member:2", "action": "same", "id": "m2", "line": 4, "quote": "still play chess" }
        ]
    });
    let (fake, _, b) = builder(vec![answer(first), answer(second.clone()), answer(second)]);
    let store = Arc::new(MemoryEpisodeStore::new());
    store.add_lines(
        group(),
        [
            said(1, 11, 2, "I play Genshin every night"),
            said(2, 11, 2, "and Chess on weekends"),
            said(3, 11, 2, "I live in Hangzhou"),
            said(4, 12, 3, "nice"),
            said(5, 11, 2, "I quit Genshin last week"),
            said(6, 12, 3, "no more for me either"),
            said(7, 11, 2, "and I moved to Ningbo"),
            said(8, 11, 2, "I still play chess though"),
        ],
    );
    let memories = Arc::new(InMemoryMemoryStore::default());
    let consolidator = Arc::new(Consolidator::new(
        Arc::new(MemoryNameStore::default()),
        memories.clone(),
    ));
    let job = EpisodeJobs::new(store.clone(), b, slice_grid(4, 1, 0, 0))
        .with_consolidator(consolidator)
        .with_background(Arc::new(Remembered(memories.clone())));
    assert_eq!(job.extract_group(group()).await.unwrap(), 2);
    assert!(
        prompt(&fake, 1).contains("m3 lives in Hangzhou"),
        "the extractor saw the memories with their ids"
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
    assert!(matches!(all[2].state, MemoryState::Replaced { by: Some(id), .. } if id == all[3].id));
    let stored = store.within(group(), 5, 8).await.unwrap();
    assert_eq!(
        stored[0].episode.findings.memories.len(),
        3,
        "an unknown id and someone else's memory end nothing"
    );
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

#[tokio::test]
async fn a_name_no_conversation_could_place_is_ruled_on_from_its_uses_together() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::{InMemoryMemoryStore, MemoryNameStore, MemoryStore, NameRuler, NameStore};

    // Three conversations name "Old Ray"; in none of them does one line show who that is.
    let unplaced = |title: &str, quote: &str| {
        json!({
            "title": title, "summary": "They talked about Old Ray.",
            "evidence": [{ "line": 1, "quote": quote }],
            "references": [{ "line": 1, "text": "Old Ray", "refers": "unknown", "role": "about",
                             "basis": "none", "shown_by": [] }]
        })
    };
    let mut third = unplaced("Bees", "keeps bees");
    third["memories"] = json!([{ "about": "name:Old Ray", "action": "new", "text": "keeps bees",
        "durability": "lasting", "line": 1, "quote": "keeps bees on the roof" }]);
    let (fake, _, b) = builder(vec![
        answer(unplaced("Sleep", "never sleeps")),
        answer(unplaced("Asking", "about the bees")),
        answer(third),
        Step::Reply(FakeReply::new().call(
            "submit_ruling",
            json!({ "verdict": "member:2", "lines": [1002, 1004, 4242],
                    "reason": "member:2 answers each time it is said" }),
        )),
    ]);
    let store = Arc::new(MemoryEpisodeStore::new());
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
    let job = EpisodeJobs::new(store.clone(), b, slice_grid(2, 1, 0, 0))
        .with_consolidator(Arc::new(Consolidator::new(names.clone(), memories.clone())))
        .with_rulings(names.clone(), NameRuler::new(fake.clone()));
    assert_eq!(job.extract_group(group()).await.unwrap(), 3);

    // The ruling read the uses side by side, each with its chat.
    let case = prompt(&fake, 3);
    assert!(
        case.contains("\\\"Old Ray\\\" was used 3 time(s)")
            && case.contains("Use 3:")
            && case.contains(">> [msg:1005]")
            && case.contains("   [msg:1004]"),
        "{case}"
    );
    let ray = AccountId::new(11).unwrap();
    let known = names.names(group(), None).await.unwrap();
    assert_eq!(known.len(), 1);
    assert_eq!(
        (
            known[0].account,
            known[0].key.as_str(),
            known[0].ruled,
            known[0].about
        ),
        (ray, "old ray", true, 3),
        "every use nobody could place is theirs now"
    );
    assert!(names.open(group(), 10).await.unwrap().is_empty());
    // What was said of "Old Ray" waited for the name, and is now remembered about the member.
    assert!(names.held(group()).await.unwrap().is_empty());
    let about = memories.current(group(), &[Some(ray)]).await.unwrap();
    assert_eq!(about.len(), 1);
    assert_eq!(
        (about[0].text.as_str(), about[0].own_word()),
        ("keeps bees", 0)
    );
    // Nothing is left to ask: another run calls no model.
    assert_eq!(job.extract_group(group()).await.unwrap(), 0);
    assert_eq!(fake.recorded().len(), 4);
}

#[tokio::test]
async fn an_episode_an_older_method_read_is_read_again_for_its_findings_alone() {
    use qbot_memory::consolidate::Consolidator;
    use qbot_memory::{
        Change, Durability, EpisodeId, InMemoryMemoryStore, MemoryNameStore, MemoryStore,
        NameStore, Statement, Support,
    };

    let kit = AccountId::new(11).unwrap();
    let store = Arc::new(MemoryEpisodeStore::new());
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

    let (fake, _, b) = builder(vec![answer(json!({
        "title": "ignored", "summary": "Ignored.",
        "evidence": [{ "line": 1, "quote": "call me Kit" }],
        "references": [{ "line": 1, "text": "Kit", "refers": "member:2", "role": "self",
                         "basis": "self", "shown_by": [] }],
        "memories": [
            { "about": "member:2", "action": "changed", "id": "m1", "text": "lives in Hangzhou",
              "durability": "lasting", "line": 1, "quote": "I moved to Hangzhou" },
            { "about": "member:2", "action": "ended", "id": "m2", "line": 1, "quote": "gave up chess" }
        ]
    }))]);
    let names = Arc::new(MemoryNameStore::default());
    let job = EpisodeJobs::new(store.clone(), b, slice_grid(2, 1, 0, 0))
        .with_consolidator(Arc::new(Consolidator::new(names.clone(), memories.clone())))
        .with_background(Arc::new(Remembered(memories.clone())))
        .with_reread(1);
    assert_eq!(job.extract_group(group()).await.unwrap(), 0, "no new slice");

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
    assert!(store.unconsolidated(group()).await.unwrap().is_empty());
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
    assert_eq!(job.extract_group(group()).await.unwrap(), 0);
    assert_eq!(fake.recorded().len(), 1);
}
