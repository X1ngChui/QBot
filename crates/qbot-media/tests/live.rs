//! Live check of picture description with the production describer: the real vision provider,
//! the real instructions, the media service's cache and single-flight, over a simulated platform.
//! Ignored by default, it costs money and needs `deploy/secrets/vision_api_key` (or the directory
//! in `QBOT_LIVE_SECRETS_DIR`), read into memory only:
//!
//! ```text
//! cargo test -p qbot-media --test live -- --ignored
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use qbot_core::marker::fill;
use qbot_core::{GroupId, MediaKind, MessageId};
use qbot_llm::responses::{KeySource, ReqwestTransport, ResponsesConfig, ResponsesProvider};
use qbot_media::{
    CacheError, DescribeError, Describer, DescriptionCache, EditError, FetchError, Fetcher,
    LineEditor, LlmDescriber, MediaConfig, MediaDeps, MediaItem, MediaJob, MediaRef, MediaService,
};

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

/// A 96x64 picture whose left half is `left` and right half `right`.
fn halves_png(left: [u8; 3], right: [u8; 3]) -> Vec<u8> {
    let (w, h) = (96u32, 64u32);
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for _ in 0..h {
        for x in 0..w {
            data.extend_from_slice(if x < w / 2 { &left } else { &right });
        }
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, w, h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&data)
        .unwrap();
    out
}

/// The platform: pictures by file id.
struct Platform(HashMap<String, Vec<u8>>);

#[async_trait]
impl Fetcher for Platform {
    async fn image(&self, reference: &MediaRef, _: u64) -> Result<Vec<u8>, FetchError> {
        let key = reference.key.as_deref().unwrap_or_default();
        self.0.get(key).cloned().ok_or(FetchError::Unreadable)
    }
    async fn voice(&self, _: &MediaRef, _: u64) -> Result<Vec<u8>, FetchError> {
        Err(FetchError::Unreadable)
    }
}

/// The production describer, counting its calls.
struct Counted {
    inner: LlmDescriber,
    calls: AtomicUsize,
}

#[async_trait]
impl Describer for Counted {
    async fn describe(&self, bytes: &[u8], mime: &str) -> Result<String, DescribeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.describe(bytes, mime).await
    }
    fn fingerprint(&self) -> String {
        self.inner.fingerprint()
    }
}

#[derive(Default)]
struct Cache(Mutex<HashMap<String, String>>);

#[async_trait]
impl DescriptionCache for Cache {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    async fn put(&self, key: &str, description: &str) -> Result<(), CacheError> {
        self.0
            .lock()
            .unwrap()
            .insert(key.to_owned(), description.to_owned());
        Ok(())
    }
}

#[derive(Default)]
struct Lines(Mutex<HashMap<i64, String>>);

#[async_trait]
impl LineEditor for Lines {
    async fn fill(
        &self,
        _: GroupId,
        message: MessageId,
        kind: MediaKind,
        index: usize,
        replacement: &str,
    ) -> Result<bool, EditError> {
        let mut lines = self.0.lock().unwrap();
        let Some(line) = lines.get_mut(&message.get()) else {
            return Ok(false);
        };
        match fill(line, kind.marker(), index, replacement) {
            Some(next) => {
                *line = next;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

fn job(message: i64, file: &str) -> MediaJob {
    MediaJob {
        group: GroupId::new(900).unwrap(),
        message: MessageId::new(message).unwrap(),
        items: vec![MediaItem {
            kind: MediaKind::Image,
            index: 0,
            reference: MediaRef {
                key: Some(file.into()),
                ..MediaRef::default()
            },
            nested: false,
        }],
    }
}

#[tokio::test]
#[ignore = "calls paid provider APIs"]
async fn pictures_are_described_as_themselves_and_a_repost_is_not_described_again() {
    let transport = ReqwestTransport::new(
        "https://api.deepseek.com",
        KeySource::Static(secret("vision_api_key")),
        &qbot_llm::net::Route::Direct,
    )
    .unwrap();
    let mut cfg = ResponsesConfig::deepseek("deepseek-flash");
    cfg.timeout = Some(Duration::from_secs(120));
    let vision = Arc::new(ResponsesProvider::new(cfg, Arc::new(transport)).unwrap());
    // As production writes them: the instructions in the deployment's writing language.
    let instructions = qbot_prompt::describe_image_instructions("Simplified Chinese").unwrap();
    let describer = Arc::new(Counted {
        inner: LlmDescriber::new(vision, instructions),
        calls: AtomicUsize::new(0),
    });
    let platform = Platform(HashMap::from([
        (
            "red-blue".to_owned(),
            halves_png([220, 20, 20], [20, 40, 220]),
        ),
        (
            "green-yellow".to_owned(),
            halves_png([20, 170, 40], [240, 220, 20]),
        ),
        (
            "red-blue-again".to_owned(),
            halves_png([220, 20, 20], [20, 40, 220]),
        ),
    ]));
    let lines = Arc::new(Lines::default());
    for id in 1..=3 {
        lines.0.lock().unwrap().insert(id, "[image]".into());
    }
    let service = MediaService::new(
        MediaConfig::default(),
        MediaDeps {
            fetcher: Arc::new(platform),
            describer: Some(describer.clone()),
            transcriber: None,
            cache: Arc::new(Cache::default()),
            editor: lines.clone(),
        },
    );
    let group = GroupId::new(900).unwrap();
    service.admit(job(1, "red-blue"));
    service.admit(job(2, "green-yellow"));
    assert!(service.settle(group, Duration::from_secs(120)).await);
    service.admit(job(3, "red-blue-again"));
    assert!(service.settle(group, Duration::from_secs(120)).await);
    service.shutdown().await;

    let text = |id: i64| lines.0.lock().unwrap().get(&id).cloned().unwrap();
    for id in 1..=3 {
        eprintln!("[describe] line {id}: {}", text(id));
    }
    let (red, blue, green, yellow) = ('\u{7EA2}', '\u{84DD}', '\u{7EFF}', '\u{9EC4}');
    let first = text(1);
    assert!(
        first.starts_with("[image:") && first.ends_with(']'),
        "{first}"
    );
    assert!(first.contains(red) && first.contains(blue), "{first}");
    let second = text(2);
    assert!(
        second.contains(green) && second.contains(yellow) && !second.contains(red),
        "the second picture is described as itself, not as the first: {second}"
    );
    assert_eq!(
        text(3),
        first,
        "the same bytes under another id reuse the description"
    );
    assert_eq!(describer.calls.load(Ordering::SeqCst), 2);
}
