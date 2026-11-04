//! Live extraction against the real text model. Ignored by default; run with
//! `cargo test -p qbot-memory --test live -- --ignored --nocapture`. The key is read from
//! `deploy/secrets/text_api_key` (or `QBOT_LIVE_SECRETS_DIR`) into memory only.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use qbot_core::{AccountId, MessageId, UnixMillis};
use qbot_llm::responses::{KeySource, ReqwestTransport, ResponsesConfig, ResponsesProvider};
use qbot_memory::findings::MemoryAction;
use qbot_memory::{EpisodeExtractor, MemoryId, NameBasis, SliceContext, SliceLine};

fn secret(file: &str) -> String {
    let dir = std::env::var_os("QBOT_LIVE_SECRETS_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/secrets"),
        PathBuf::from,
    );
    let path = dir.join(file);
    let value = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read secret file {}: {}", path.display(), e.kind()));
    value.trim().to_owned()
}

fn line(ordinal: u64, account: i64, member: u32, text: &str) -> SliceLine {
    SliceLine {
        ordinal,
        message: MessageId::new(500 + ordinal as i64).unwrap(),
        speaker: Some(AccountId::new(account).unwrap()),
        member_no: Some(member),
        at: UnixMillis::new(1_800_000_000_000 + ordinal as i64 * 60_000),
        text: text.into(),
        shown: None,
    }
}

fn live_extractor() -> EpisodeExtractor {
    let endpoint = std::env::var("QBOT_LIVE_TEXT_ENDPOINT")
        .unwrap_or_else(|_| "https://api.deepseek.com".into());
    let model = std::env::var("QBOT_LIVE_TEXT_MODEL").unwrap_or_else(|_| "deepseek-flash".into());
    let transport = ReqwestTransport::new(
        endpoint,
        KeySource::Static(secret("text_api_key")),
        &qbot_llm::net::Route::Direct,
    )
    .unwrap();
    let mut cfg = ResponsesConfig::deepseek(model);
    cfg.timeout = Some(Duration::from_secs(180));
    let provider = Arc::new(ResponsesProvider::new(cfg, Arc::new(transport)).unwrap());
    EpisodeExtractor::new(provider)
}

fn report(out: &qbot_memory::Extracted) {
    eprintln!(
        "[extract] model={} attempts={} usage={:?}",
        out.model, out.attempts, out.usage
    );
    eprintln!("[extract] title={:?}", out.title);
    eprintln!("[extract] summary={:?}", out.summary);
    eprintln!("[extract] findings={:#?}", out.findings);
    eprintln!("[extract] dropped={:?}", out.dropped);
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_real_model_writes_a_valid_episode_with_findings() {
    let extractor = live_extractor();
    let target = vec![
        line(
            1,
            1001,
            1,
            "\u{6211}\u{4e0b}\u{5468}\u{516d}\u{53bb}\u{676d}\u{5dde}\u{51fa}\u{5dee}\u{ff0c}\u{987a}\u{4fbf}\u{53bb}\u{897f}\u{6e56}\u{8d70}\u{8d70}",
        ),
        line(
            2,
            1002,
            2,
            "\u{897f}\u{6e56}\u{73b0}\u{5728}\u{4eba}\u{8d85}\u{591a}\u{ff0c}\u{5efa}\u{8bae}\u{65e9}\u{4e0a}\u{4e03}\u{70b9}\u{524d}\u{53bb}",
        ),
        line(
            3,
            1001,
            1,
            "\u{597d}\u{7684}\u{3002}\u{6211}\u{6700}\u{559c}\u{6b22}\u{732b}\u{4e86}\u{ff0c}\u{542c}\u{8bf4}\u{90a3}\u{8fb9}\u{6709}\u{5bb6}\u{732b}\u{5496}",
        ),
        line(
            4,
            1002,
            2,
            "\u{5bf9}\u{ff0c}\u{90a3}\u{5bb6}\u{6211}\u{4eec}\u{7fa4}\u{91cc}\u{90fd}\u{53eb}\u{5b83}\u{55b5}\u{7ad9}",
        ),
        line(
            5,
            1003,
            3,
            "\u{55b5}\u{7ad9}\u{662f}\u{4ec0}\u{4e48}\u{ff1f}",
        ),
        line(
            6,
            1002,
            2,
            "\u{5c31}\u{662f}\u{7fa4}\u{91cc}\u{5e38}\u{8bf4}\u{7684}\u{90a3}\u{5bb6}\u{732b}\u{5496}\u{ff0c}\u{5728}\u{6e56}\u{8fb9}",
        ),
    ];
    // As the prompt layer writes it: the members as the group knows them, and the group.
    let background = qbot_memory::Background {
        text: "## Who is who\n\nThe members these lines involve, as the group knows them.\n\n\
               - member:1: lines sent as \"\u{5c0f}\u{6797}\"\n\
               - member:2: lines sent as \"\u{963f}\u{6770}\"\n  remembered:\n  \
               - m7 (lasting) \u{4f4f}\u{5728}\u{676d}\u{5dde}: they said so, 2 conversation(s), last 2026-12-01\n\
               - member:3: lines sent as \"Mia\"\n\n\
               ## About the group\n\nFriends from the same university who chat about food and trips."
            .into(),
        zone: jiff::tz::TimeZone::get("Asia/Shanghai").unwrap(),
        known: vec![(MemoryId::new(7), AccountId::new(1002).ok())],
        ..Default::default()
    };
    let ctx = SliceContext {
        group: qbot_core::GroupId::new(900).unwrap(),
        previous: &[],
        target: &target,
        next: &[],
        background: &background,
    };
    let out = extractor.extract(&ctx, "English").await.unwrap();
    report(&out);

    assert!(!out.title.trim().is_empty() && !out.summary.trim().is_empty());
    assert!(!out.evidence.is_empty());
    for e in &out.evidence {
        let source = target
            .iter()
            .find(|l| l.message == e.message)
            .expect("evidence names a target line");
        assert!(source.text.contains(&e.quote), "{e:?}");
    }
    let member_one = AccountId::new(1001).ok();
    assert!(
        out.findings.memories.iter().any(|m| {
            let text = m.text.as_deref().unwrap_or_default().to_lowercase();
            m.subject == member_one
                && m.action == MemoryAction::New
                && (text.contains("cat") || text.contains("\u{732b}"))
        }),
        "member 1 said they like cats"
    );
    assert!(
        out.findings.memories.iter().any(|m| m.subject.is_none()
            && m.term
                .as_deref()
                .is_some_and(|t| t.contains("\u{55b5}\u{7ad9}"))),
        "the group's name for the cafe is a group term"
    );
    assert!(
        !out.findings
            .memories
            .iter()
            .any(|m| m.subject == member_one && m.message == target[0].message),
        "a business trip next week is a plan, not a memory"
    );
}

/// The judgement the extractor is asked for: something that ended, something said again in other
/// words, a plan said relative to the slice's day, a reply to a line from further back, a passing
/// mood, and a member addressing the bot by its name.
#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_real_model_ends_confirms_dates_plans_and_skips_what_passes() {
    let extractor = live_extractor();
    let target = vec![
        // "Bobo, I don't play Genshin any more, sold the account last week."
        line(
            1,
            1001,
            1,
            "Bobo\u{ff0c}\u{6211}\u{4e0d}\u{73a9}\u{539f}\u{795e}\u{4e86}\u{ff0c}\u{4e0a}\u{5468}\u{628a}\u{53f7}\u{5356}\u{4e86}",
        ),
        // A reply to an older line: "I agree too."
        line(2, 1002, 2, "[reply:420] \u{6211}\u{4e5f}\u{540c}\u{610f}"),
        // "So tired today, eating instant noodles."
        line(
            3,
            1002,
            2,
            "\u{4eca}\u{5929}\u{597d}\u{7d2f}\u{ff0c}\u{5728}\u{5403}\u{6ce1}\u{9762}",
        ),
        // "Then meet at the metro station tomorrow at 3 pm."
        line(
            4,
            1003,
            3,
            "\u{90a3}\u{660e}\u{5929}\u{4e0b}\u{5348}\u{4e09}\u{70b9}\u{5728}\u{5730}\u{94c1}\u{7ad9}\u{96c6}\u{5408}",
        ),
        // "OK, I'll definitely be there tomorrow."
        line(
            5,
            1001,
            1,
            "\u{597d}\u{ff0c}\u{6211}\u{660e}\u{5929}\u{4e00}\u{5b9a}\u{5230}",
        ),
        // "I still live in Hangzhou."
        line(
            6,
            1002,
            2,
            "\u{6211}\u{8fd8}\u{662f}\u{4f4f}\u{5728}\u{676d}\u{5dde}",
        ),
    ];
    // As the prompt layer and the extraction job write it.
    let background = qbot_memory::Background {
        text: "## Who is who\n\nThe members these lines involve, as the group knows them.\n\n\
               - member:1: lines sent as \"\u{5c0f}\u{6797}\"\n  remembered:\n  \
               - m3 (current) \u{5728}\u{73a9}\u{539f}\u{795e}: they said so, 3 conversation(s), last 2026-12-20\n\
               - member:2: lines sent as \"\u{963f}\u{6770}\"\n  remembered:\n  \
               - m5 (lasting) \u{4f4f}\u{5728}\u{676d}\u{5dde}\u{5e02}: they said so, 1 conversation(s), last 2026-09-01\n\
               - member:3: lines sent as \"Mia\"\n\n\
               ## About the group\n\nThe group's bot (its lines are `bot:`) is called Bobo. These are the bot's \
               names, never names of members.\n\
               Friends from the same university who chat about games and trips."
            .into(),
        zone: jiff::tz::TimeZone::get("Asia/Shanghai").unwrap(),
        earlier: Some((
            "Weekend plans".into(),
            "member:3 suggested a hike at the weekend; member:1 asked where to meet.".into(),
        )),
        quoted: vec![SliceLine {
            ordinal: 0,
            message: MessageId::new(420).unwrap(),
            ..line(
                0,
                1003,
                3,
                "\u{5468}\u{672b}\u{4e00}\u{8d77}\u{53bb}\u{722c}\u{5c71}\u{5427}",
            )
        }],
        bot_names: vec!["Bobo".into()],
        known: vec![
            (MemoryId::new(3), AccountId::new(1001).ok()),
            (MemoryId::new(5), AccountId::new(1002).ok()),
        ],
        ..Default::default()
    };
    let ctx = SliceContext {
        group: qbot_core::GroupId::new(900).unwrap(),
        previous: &[],
        target: &target,
        next: &[],
        background: &background,
    };
    let out = extractor.extract(&ctx, "English").await.unwrap();
    report(&out);

    let (one, two) = (AccountId::new(1001).ok(), AccountId::new(1002).ok());
    let about = |who, id: i64| {
        out.findings
            .memories
            .iter()
            .filter(move |m| m.subject == who && m.id == Some(MemoryId::new(id)))
    };
    assert!(
        about(one, 3).any(|m| m.action == MemoryAction::Ended),
        "member 1 stopped playing the game"
    );
    assert!(
        about(two, 5).any(|m| m.action == MemoryAction::Same),
        "living in Hangzhou, said again in other words, supports the memory"
    );
    assert!(
        !out.findings
            .memories
            .iter()
            .any(|m| m.subject == two && m.action == MemoryAction::New),
        "neither the mood nor the city is a new memory"
    );
    assert!(
        !out.findings
            .names
            .iter()
            .any(|n| n.name.eq_ignore_ascii_case("bobo")),
        "the bot's name is no member's name"
    );
    // The slice is on 2027-01-15 in Shanghai, so tomorrow is the 16th.
    assert!(
        out.summary.contains("16") && !out.summary.to_lowercase().contains("tomorrow"),
        "the plan has its date: {}",
        out.summary
    );
}

/// Names: a member gives their own name, someone addresses another by a nickname with an
/// @-mention, and a third name is used for someone the line does not identify.
#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_real_model_records_names_with_how_they_were_used() {
    let extractor = live_extractor();
    let target = vec![
        // member 3: "[at:1] Xiaochuan, are you coming to the dinner on Saturday?"
        line(
            1,
            1003,
            3,
            "[at:1] \u{5c0f}\u{5ddd}\u{5468}\u{516d}\u{805a}\u{9910}\u{4f60}\u{6765}\u{4e0d}\u{6765}",
        ),
        // member 1: "Coming. And don't call me Xiaochuan, call me Brother Chuan."
        line(
            2,
            1001,
            1,
            "\u{6765}\u{3002}\u{522b}\u{53eb}\u{6211}\u{5c0f}\u{5ddd}\u{4e86}\u{ff0c}\u{53eb}\u{6211}\u{5ddd}\u{54e5}",
        ),
        // member 2: "Old Wang is late again today."
        line(
            3,
            1002,
            2,
            "\u{8001}\u{738b}\u{4eca}\u{5929}\u{53c8}\u{8fdf}\u{5230}\u{4e86}",
        ),
        // member 3: "haha"
        line(4, 1003, 3, "\u{54c8}\u{54c8}"),
    ];
    let background = qbot_memory::Background {
        text: "## Who is who\n\nThe members these lines involve, as the group knows them.\n\n\
               - member:1: lines sent as \"\u{738b}\u{5c0f}\u{5ddd}\"\n\
               - member:2: lines sent as \"Echo\"\n\
               - member:3: lines sent as \"\u{5927}\u{5218}\"\n\n\
               ## About the group\n\nColleagues who chat about work and food."
            .into(),
        zone: jiff::tz::TimeZone::get("Asia/Shanghai").unwrap(),
        ..Default::default()
    };
    let ctx = SliceContext {
        group: qbot_core::GroupId::new(900).unwrap(),
        previous: &[],
        target: &target,
        next: &[],
        background: &background,
    };
    let out = extractor.extract(&ctx, "English").await.unwrap();
    report(&out);
    let one = AccountId::new(1001).unwrap();
    assert!(
        out.findings.names.iter().any(|n| n.account == one
            && n.basis == NameBasis::Own
            && n.name.contains("\u{5ddd}\u{54e5}")),
        "member 1 gave their own name"
    );
    assert!(
        !out.findings
            .names
            .iter()
            .any(|n| n.name.contains("\u{8001}\u{738b}")),
        "nothing in the lines says whom Old Wang means"
    );
}
