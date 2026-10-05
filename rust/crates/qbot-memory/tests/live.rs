//! Live extraction against the real text model. Ignored by default; run with
//! `cargo test -p qbot-memory --test live -- --ignored --nocapture`. The key is read from
//! `deploy/secrets/text_api_key` (or `QBOT_LIVE_SECRETS_DIR`) into memory only.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use qbot_core::{AccountId, MessageId, UnixMillis};
use qbot_llm::responses::{KeySource, ReqwestTransport, ResponsesConfig, ResponsesProvider};
use qbot_memory::findings::KnowledgeFinding;
use qbot_memory::{EpisodeExtractor, ExtractorConfig, SliceContext, SliceLine};

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
    }
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn a_real_model_writes_a_valid_episode_with_findings() {
    let endpoint = std::env::var("QBOT_LIVE_TEXT_ENDPOINT")
        .unwrap_or_else(|_| "https://api.deepseek.com".into());
    let model = std::env::var("QBOT_LIVE_TEXT_MODEL").unwrap_or_else(|_| "deepseek-flash".into());
    let transport = ReqwestTransport::new(
        endpoint,
        KeySource::Static(secret("text_api_key")),
        Duration::from_secs(10),
    )
    .unwrap();
    let mut cfg = ResponsesConfig::deepseek(model);
    cfg.timeout = Some(Duration::from_secs(180));
    let provider = Arc::new(ResponsesProvider::new(cfg, Arc::new(transport)).unwrap());
    let extractor = EpisodeExtractor::new(
        provider,
        ExtractorConfig {
            max_output_tokens: 4_000,
            max_attempts: 3,
        },
    );
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
    let ctx = SliceContext {
        previous: &[],
        target: &target,
        next: &[],
    };
    let out = extractor.extract(&ctx, "English").await.unwrap();
    eprintln!(
        "[extract] model={} attempts={} usage={:?}",
        out.model, out.attempts, out.usage
    );
    eprintln!("[extract] title={:?}", out.title);
    eprintln!("[extract] summary={:?}", out.summary);
    eprintln!("[extract] evidence={:?}", out.evidence);
    eprintln!("[extract] findings={:#?}", out.findings);
    eprintln!("[extract] dropped={:?}", out.dropped);

    assert!(!out.title.trim().is_empty() && !out.summary.trim().is_empty());
    assert!(!out.evidence.is_empty());
    for e in &out.evidence {
        let source = target
            .iter()
            .find(|l| l.message == e.message)
            .expect("evidence names a target line");
        assert!(source.text.contains(&e.quote), "{e:?}");
    }
    let member_one = AccountId::new(1001).unwrap();
    assert!(
        out.findings.facts.iter().any(|f| f.account == member_one
            && f.predicate == "likes"
            && f.object.contains("\u{732b}")),
        "member 1 said they like cats"
    );
    assert!(
        out.findings.knowledge.iter().any(
            |k| matches!(k, KnowledgeFinding::Term { term, .. } if term.contains("\u{55b5}\u{7ad9}"))
        ),
        "the group's name for the cafe is group knowledge"
    );
}
