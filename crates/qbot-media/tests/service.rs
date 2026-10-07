#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use qbot_core::MediaKind;
use qbot_core::marker::fill;
use qbot_core::{GroupId, MessageId};
use qbot_media::{
    Admission, CacheError, DescribeError, Describer, DescriptionCache, EditError, FetchError,
    Fetcher, LineEditor, MediaConfig, MediaDeps, MediaItem, MediaJob, MediaRef, MediaService,
    TranscribeError, Transcriber,
};

fn group() -> GroupId {
    GroupId::new(900).unwrap()
}

fn message(n: i64) -> MessageId {
    MessageId::new(n).unwrap()
}

#[derive(Default)]
struct Fetcher0 {
    /// key -> bytes or failure
    files: Mutex<HashMap<String, Result<Vec<u8>, FetchError>>>,
    calls: AtomicUsize,
    /// Keys whose download takes this long.
    slow: Mutex<HashMap<String, Duration>>,
}

impl Fetcher0 {
    fn with(files: &[(&str, Result<Vec<u8>, FetchError>)]) -> Arc<Self> {
        let f = Fetcher0::default();
        for (k, v) in files {
            f.files.lock().unwrap().insert((*k).to_owned(), v.clone());
        }
        Arc::new(f)
    }

    fn get(&self, reference: &MediaRef) -> Result<Vec<u8>, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let key = reference.key.clone().unwrap_or_default();
        self.files
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or(Err(FetchError::Unreadable))
    }
}

#[async_trait]
impl Fetcher for Fetcher0 {
    async fn image(&self, reference: &MediaRef, _: u64) -> Result<Vec<u8>, FetchError> {
        let delay = reference
            .key
            .as_ref()
            .and_then(|key| self.slow.lock().unwrap().get(key).copied());
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        self.get(reference)
    }
    async fn voice(&self, reference: &MediaRef, _: u64) -> Result<Vec<u8>, FetchError> {
        self.get(reference)
    }
}

struct Describer0 {
    answer: Mutex<Result<String, DescribeError>>,
    delay: Duration,
    calls: AtomicUsize,
    fingerprint: &'static str,
}

impl Describer0 {
    fn new(answer: &str) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(Ok(answer.to_owned())),
            delay: Duration::ZERO,
            calls: AtomicUsize::new(0),
            fingerprint: "v1",
        })
    }
    fn slow(answer: &str, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(Ok(answer.to_owned())),
            delay,
            calls: AtomicUsize::new(0),
            fingerprint: "v1",
        })
    }
}

#[async_trait]
impl Describer for Describer0 {
    async fn describe(&self, _: &[u8], _: &str) -> Result<String, DescribeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.answer.lock().unwrap().clone()
    }

    fn fingerprint(&self) -> String {
        self.fingerprint.to_owned()
    }
}

struct Transcriber0(Mutex<Result<String, TranscribeError>>);

#[async_trait]
impl Transcriber for Transcriber0 {
    async fn transcribe(&self, _: &[u8]) -> Result<String, TranscribeError> {
        self.0.lock().unwrap().clone()
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

/// Lines by message id, edited with the real marker logic.
#[derive(Default)]
struct Lines(Mutex<HashMap<i64, String>>);

impl Lines {
    fn with(entries: &[(i64, &str)]) -> Arc<Self> {
        let l = Lines::default();
        for (id, text) in entries {
            l.0.lock().unwrap().insert(*id, (*text).to_owned());
        }
        Arc::new(l)
    }
    fn text(&self, id: i64) -> String {
        self.0.lock().unwrap().get(&id).cloned().unwrap()
    }
}

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

fn image(index: usize, key: &str) -> MediaItem {
    MediaItem {
        kind: MediaKind::Image,
        index,
        reference: MediaRef {
            key: Some(key.into()),
            ..MediaRef::default()
        },
        nested: false,
    }
}

fn voice(index: usize, key: &str) -> MediaItem {
    MediaItem {
        kind: MediaKind::Voice,
        index,
        reference: MediaRef {
            key: Some(key.into()),
            ..MediaRef::default()
        },
        nested: false,
    }
}

fn job(message_id: i64, items: Vec<MediaItem>) -> MediaJob {
    MediaJob {
        group: group(),
        message: message(message_id),
        items,
    }
}

fn config() -> MediaConfig {
    MediaConfig {
        images_per_minute: 100,
        clips_per_minute: 100,
        max_image_bytes: 1000,
        max_audio: Duration::from_secs(60),
        concurrency: 4,
        capacity: 8,
        unreadable_hold: Duration::from_secs(600),
    }
}

struct Rig {
    service: MediaService,
    fetcher: Arc<Fetcher0>,
    describer: Arc<Describer0>,
    cache: Arc<Cache>,
    lines: Arc<Lines>,
}

fn rig(
    cfg: MediaConfig,
    fetcher: Arc<Fetcher0>,
    describer: Arc<Describer0>,
    lines: &[(i64, &str)],
) -> Rig {
    let cache = Arc::new(Cache::default());
    let lines = Lines::with(lines);
    let service = MediaService::new(
        cfg,
        MediaDeps {
            fetcher: fetcher.clone(),
            describer: Some(describer.clone()),
            transcriber: Some(Arc::new(Transcriber0(Mutex::new(Ok(
                "see you at eight".into()
            ))))),
            cache: cache.clone(),
            editor: lines.clone(),
        },
    );
    Rig {
        service,
        fetcher,
        describer,
        cache,
        lines,
    }
}

/// Wait for the group's media work; `id` names the message the test is waiting for.
async fn finish(rig: &Rig, id: i64) {
    assert!(
        rig.service.settle(group(), Duration::from_secs(5)).await,
        "the media work of message {id} should finish"
    );
}

#[tokio::test]
async fn a_picture_is_described_cached_and_filled_into_its_line() {
    let r = rig(
        config(),
        Fetcher0::with(&[("a", Ok(b"\x89PNG-bytes".to_vec()))]),
        Describer0::new("**A cat** on a\nkeyboard [sic]"),
        &[(1, "look [image]")],
    );
    assert_eq!(
        r.service.admit(job(1, vec![image(0, "a")])),
        Admission::Started
    );
    finish(&r, 1).await;
    assert_eq!(
        r.lines.text(1),
        "look [image:A cat on a keyboard \u{FF3B}sic\u{FF3D}]",
        "markdown stripped, one line, brackets neutralized"
    );
    assert_eq!(
        r.cache.0.lock().unwrap().keys().collect::<Vec<_>>(),
        [&format!("v1:{}", sha256_hex(b"\x89PNG-bytes"))],
        "by describer and content"
    );

    // The same picture again costs a download, no model call.
    let (calls_f, calls_d) = (
        r.fetcher.calls.load(Ordering::SeqCst),
        r.describer.calls.load(Ordering::SeqCst),
    );
    r.lines.0.lock().unwrap().insert(2, "again [image]".into());
    r.service.admit(job(2, vec![image(0, "a")]));
    finish(&r, 2).await;
    assert!(
        r.lines
            .text(2)
            .starts_with("again [image:A cat on a keyboard")
    );
    assert_eq!(
        (
            r.fetcher.calls.load(Ordering::SeqCst),
            r.describer.calls.load(Ordering::SeqCst)
        ),
        (calls_f + 1, calls_d)
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

#[tokio::test]
async fn a_platform_id_reused_for_other_pixels_gets_its_own_description() {
    let fetcher = Fetcher0::with(&[("same-name.jpg", Ok(b"first".to_vec()))]);
    let r = rig(
        config(),
        fetcher.clone(),
        Describer0::new("a cat"),
        &[(1, "[image]"), (2, "[image]")],
    );
    r.service.admit(job(1, vec![image(0, "same-name.jpg")]));
    finish(&r, 1).await;
    fetcher
        .files
        .lock()
        .unwrap()
        .insert("same-name.jpg".into(), Ok(b"second".to_vec()));
    *r.describer.answer.lock().unwrap() = Ok("a bus".into());
    r.service.admit(job(2, vec![image(0, "same-name.jpg")]));
    finish(&r, 2).await;
    assert_eq!(
        (r.lines.text(1), r.lines.text(2)),
        ("[image:a cat]".into(), "[image:a bus]".into())
    );
}

#[tokio::test]
async fn another_describer_does_not_reuse_the_old_descriptions() {
    let cache = Arc::new(Cache::default());
    let lines = Lines::with(&[(1, "[image]"), (2, "[image]")]);
    let service = |describer: Arc<Describer0>| {
        MediaService::new(
            config(),
            MediaDeps {
                fetcher: Fetcher0::with(&[("a", Ok(b"1".to_vec()))]),
                describer: Some(describer),
                transcriber: None,
                cache: cache.clone(),
                editor: lines.clone(),
            },
        )
    };
    let old = service(Describer0::new("a cat"));
    old.admit(job(1, vec![image(0, "a")]));
    assert!(old.settle(group(), Duration::from_secs(5)).await);
    let other = Arc::new(Describer0 {
        fingerprint: "v2",
        ..Arc::into_inner(Describer0::new("eine Katze")).unwrap()
    });
    let new = service(other.clone());
    new.admit(job(2, vec![image(0, "a")]));
    assert!(new.settle(group(), Duration::from_secs(5)).await);
    assert_eq!(lines.text(2), "[image:eine Katze]");
    assert_eq!(other.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_same_pixels_under_another_id_are_described_once() {
    let bytes = b"\xff\xd8same".to_vec();
    let r = rig(
        config(),
        Fetcher0::with(&[("a", Ok(bytes.clone())), ("b", Ok(bytes))]),
        Describer0::new("a dog"),
        &[(1, "[image]"), (2, "[image]")],
    );
    r.service.admit(job(1, vec![image(0, "a")]));
    finish(&r, 1).await;
    r.service.admit(job(2, vec![image(0, "b")]));
    finish(&r, 2).await;
    assert_eq!(r.lines.text(2), "[image:a dog]");
    assert_eq!(r.describer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        r.fetcher.calls.load(Ordering::SeqCst),
        2,
        "the bytes had to be fetched to recognise them"
    );
}

#[tokio::test]
async fn concurrent_requests_for_one_picture_share_one_model_call() {
    let r = rig(
        config(),
        Fetcher0::with(&[("a", Ok(b"x".to_vec()))]),
        Describer0::slow("a tree", Duration::from_millis(150)),
        &[(1, "[image]"), (2, "[image]")],
    );
    r.service.admit(job(1, vec![image(0, "a")]));
    r.service.admit(job(2, vec![image(0, "a")]));
    finish(&r, 1).await;
    finish(&r, 2).await;
    assert_eq!(
        (r.lines.text(1), r.lines.text(2)),
        ("[image:a tree]".into(), "[image:a tree]".into())
    );
    assert_eq!(r.describer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn markers_keep_their_slots_when_filled_out_of_order() {
    let r = rig(
        config(),
        Fetcher0::with(&[
            ("slow", Ok(b"1".to_vec())),
            ("fast", Ok(b"2".to_vec())),
            ("clip", Ok(vec![0; 4])),
        ]),
        Describer0::new("pic"),
        &[(1, "[image] and [image] and [voice]")],
    );
    r.service.admit(job(
        1,
        vec![image(0, "slow"), image(1, "fast"), voice(0, "clip")],
    ));
    finish(&r, 1).await;
    assert_eq!(
        r.lines.text(1),
        "[image:pic] and [image:pic] and [voice:see you at eight]"
    );
}

#[tokio::test]
async fn limits_and_failures_leave_the_marker_bare_without_hammering_the_backend() {
    let mut cfg = config();
    cfg.images_per_minute = 1;
    let r = rig(
        cfg,
        Fetcher0::with(&[
            ("ok", Ok(b"1".to_vec())),
            ("big", Err(FetchError::TooLarge)),
            ("gone", Err(FetchError::Unreadable)),
            ("p2", Ok(b"2".to_vec())),
        ]),
        Describer0::new("pic"),
        &[(1, "[image]"), (2, "[image]"), (3, "[image]")],
    );
    // The first picture uses the group's one slot for this minute.
    r.service.admit(job(1, vec![image(0, "ok")]));
    finish(&r, 1).await;
    assert_eq!(r.lines.text(1), "[image:pic]");
    r.service.admit(job(2, vec![image(0, "p2")]));
    finish(&r, 2).await;
    assert_eq!(r.lines.text(2), "[image]", "rate limited");

    // An unreadable picture is remembered and not fetched again within the hold.
    let r = rig(
        config(),
        Fetcher0::with(&[("gone", Err(FetchError::Unreadable))]),
        Describer0::new("pic"),
        &[(1, "[image]"), (2, "[image]")],
    );
    r.service.admit(job(1, vec![image(0, "gone")]));
    finish(&r, 1).await;
    r.service.admit(job(2, vec![image(0, "gone")]));
    finish(&r, 2).await;
    assert_eq!(r.fetcher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(r.lines.text(1), "[image]");

    // Too large, by declared size and by the fetcher's verdict.
    let r = rig(
        config(),
        Fetcher0::with(&[("big", Err(FetchError::TooLarge))]),
        Describer0::new("pic"),
        &[(1, "[image]"), (2, "[image]")],
    );
    let mut declared = image(0, "x");
    declared.reference.size = Some(5000);
    r.service.admit(job(1, vec![declared]));
    r.service.admit(job(2, vec![image(0, "big")]));
    finish(&r, 1).await;
    finish(&r, 2).await;
    assert_eq!(
        (r.lines.text(1), r.lines.text(2)),
        ("[image]".into(), "[image]".into())
    );
    assert_eq!(r.describer.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_declined_picture_is_not_asked_again_and_a_failed_call_may_be() {
    let r = rig(
        config(),
        Fetcher0::with(&[("a", Ok(b"1".to_vec()))]),
        Describer0::new("x"),
        &[(1, "[image]"), (2, "[image]"), (3, "[image]")],
    );
    *r.describer.answer.lock().unwrap() = Err(DescribeError::Declined);
    r.service.admit(job(1, vec![image(0, "a")]));
    finish(&r, 1).await;
    r.service.admit(job(2, vec![image(0, "a")]));
    finish(&r, 2).await;
    assert_eq!(
        r.describer.calls.load(Ordering::SeqCst),
        1,
        "declined once, held"
    );
    assert!(
        r.cache.0.lock().unwrap().is_empty(),
        "a refusal is not a description"
    );

    // A transient failure is not held: the next attempt goes through.
    let r = rig(
        config(),
        Fetcher0::with(&[("a", Ok(b"1".to_vec()))]),
        Describer0::new("x"),
        &[(1, "[image]"), (2, "[image]")],
    );
    *r.describer.answer.lock().unwrap() = Err(DescribeError::Failed("timeout".into()));
    r.service.admit(job(1, vec![image(0, "a")]));
    finish(&r, 1).await;
    *r.describer.answer.lock().unwrap() = Ok("a fox".into());
    r.service.admit(job(2, vec![image(0, "a")]));
    finish(&r, 2).await;
    assert_eq!(
        (r.lines.text(1), r.lines.text(2)),
        ("[image]".into(), "[image:a fox]".into())
    );
}

#[tokio::test]
async fn voice_is_transcribed_or_marked_unclear() {
    let describer = Describer0::new("unused");
    let cache = Arc::new(Cache::default());
    let lines = Lines::with(&[(1, "[voice]"), (2, "[voice]")]);
    let transcriber = Arc::new(Transcriber0(Mutex::new(Ok("  hello\nthere ".into()))));
    let service = MediaService::new(
        config(),
        MediaDeps {
            fetcher: Fetcher0::with(&[("c1", Ok(vec![0; 10])), ("c2", Ok(vec![0; 10]))]),
            describer: Some(describer),
            transcriber: Some(transcriber.clone()),
            cache,
            editor: lines.clone(),
        },
    );
    service.admit(job(1, vec![voice(0, "c1")]));
    assert!(service.settle(group(), Duration::from_secs(5)).await);
    assert_eq!(lines.text(1), "[voice:hello there]");
    *transcriber.0.lock().unwrap() = Ok("   ".into());
    service.admit(job(2, vec![voice(0, "c2")]));
    assert!(service.settle(group(), Duration::from_secs(5)).await);
    assert_eq!(lines.text(2), "[voice:unclear]");
}

#[tokio::test]
async fn disabled_backends_leave_their_media_alone() {
    let lines = Lines::with(&[(1, "[image]")]);
    let service = MediaService::new(
        config(),
        MediaDeps {
            fetcher: Fetcher0::with(&[]),
            describer: None,
            transcriber: None,
            cache: Arc::new(Cache::default()),
            editor: lines.clone(),
        },
    );
    assert_eq!(
        service.admit(job(1, vec![image(0, "a")])),
        Admission::Nothing
    );
    assert_eq!(lines.text(1), "[image]");
    assert!(
        service.settle(group(), Duration::from_millis(10)).await,
        "nothing to wait for"
    );
}

#[tokio::test]
async fn too_much_media_in_flight_is_declined_and_waiting_is_bounded() {
    let mut cfg = config();
    cfg.capacity = 1;
    let r = rig(
        cfg,
        Fetcher0::with(&[("a", Ok(b"1".to_vec())), ("b", Ok(b"2".to_vec()))]),
        Describer0::slow("pic", Duration::from_millis(300)),
        &[(1, "[image]"), (2, "[image]")],
    );
    assert_eq!(
        r.service.admit(job(1, vec![image(0, "a")])),
        Admission::Started
    );
    assert_eq!(
        r.service.admit(job(2, vec![image(0, "b")])),
        Admission::Overloaded
    );
    assert!(
        !r.service.settle(group(), Duration::from_millis(20)).await,
        "a short wait gives up"
    );
    finish(&r, 1).await;
    assert_eq!(r.lines.text(1), "[image:pic]");
    assert_eq!(r.lines.text(2), "[image]");
    assert_eq!(
        r.service.admit(job(2, vec![image(0, "b")])),
        Admission::Started,
        "capacity is freed when work ends"
    );
    finish(&r, 2).await;
}

#[tokio::test]
async fn shutdown_lets_started_work_finish_and_refuses_new_work() {
    let r = rig(
        config(),
        Fetcher0::with(&[("a", Ok(b"1".to_vec()))]),
        Describer0::slow("pic", Duration::from_millis(100)),
        &[(1, "[image]"), (2, "[image]")],
    );
    r.service.admit(job(1, vec![image(0, "a")]));
    r.service.shutdown().await;
    assert_eq!(r.lines.text(1), "[image:pic]");
    assert_eq!(
        r.service.admit(job(2, vec![image(0, "a")])),
        Admission::ShuttingDown
    );
}

#[tokio::test]
async fn a_picture_inside_a_forwarded_record_only_reads_the_cache() {
    let r = rig(
        config(),
        Fetcher0::with(&[
            ("posted", Ok(b"1".to_vec())),
            ("same", Ok(b"1".to_vec())),
            ("new", Ok(b"2".to_vec())),
        ]),
        Describer0::new("a red door"),
        &[(1, "[image]"), (2, "[image] [image]")],
    );
    r.service.admit(job(1, vec![image(0, "posted")]));
    finish(&r, 1).await;
    let mut same = image(0, "same");
    same.nested = true;
    let mut new = image(1, "new");
    new.nested = true;
    r.service.admit(job(2, vec![same, new]));
    r.service.shutdown().await;
    assert_eq!(
        r.lines.text(2),
        "[image:a red door] [image]",
        "the picture posted before is recognised by its content; the new one is not paid for"
    );
    assert_eq!(r.describer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stickers_are_described_once_per_picture() {
    let r = rig(
        config(),
        Fetcher0::with(&[("e9", Ok(b"\x89PNG".to_vec()))]),
        Describer0::new("a waving cat"),
        &[(1, "[sticker:shy]"), (2, "[sticker]")],
    );
    let sticker = |index| MediaItem {
        kind: MediaKind::Sticker,
        index,
        reference: MediaRef {
            key: Some("e9".into()),
            ..MediaRef::default()
        },
        nested: false,
    };
    r.service.admit(job(1, vec![sticker(0)]));
    finish(&r, 1).await;
    assert_eq!(r.lines.text(1), "[sticker:a waving cat]");
    r.service.admit(job(2, vec![sticker(0)]));
    finish(&r, 2).await;
    assert_eq!(r.lines.text(2), "[sticker:a waving cat]");
    assert_eq!(r.describer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn settling_waits_for_every_message_of_the_group_and_no_other_group() {
    let r = rig(
        config(),
        Fetcher0::with(&[("a", Ok(b"1".to_vec())), ("b", Ok(b"2".to_vec()))]),
        Describer0::slow("pic", Duration::from_millis(100)),
        &[(1, "[image]"), (2, "[image]")],
    );
    r.service.admit(job(1, vec![image(0, "a")]));
    let elsewhere = MediaJob {
        group: GroupId::new(901).unwrap(),
        ..job(2, vec![image(0, "b")])
    };
    r.service.admit(elsewhere);
    assert!(r.service.busy(group()));
    finish(&r, 1).await;
    assert_eq!(r.lines.text(1), "[image:pic]");
    assert!(!r.service.busy(group()));
}

#[tokio::test]
async fn pictures_inside_a_forwarded_record_never_hold_a_reply_up() {
    let fetcher = Fetcher0::with(&[("posted", Ok(b"1".to_vec())), ("album", Ok(b"2".to_vec()))]);
    fetcher
        .slow
        .lock()
        .unwrap()
        .insert("album".into(), Duration::from_secs(2));
    let r = rig(
        config(),
        fetcher,
        Describer0::new("a red door"),
        &[(1, "[image]"), (2, "[image] [image]")],
    );
    let mut album = image(1, "album");
    album.nested = true;
    r.service.admit(job(1, vec![album.clone()]));
    assert!(
        !r.service.busy(group()),
        "a record of forwarded pictures alone is nothing to wait for"
    );
    album.index = 1;
    r.service.admit(job(2, vec![image(0, "posted"), album]));
    assert!(
        r.service.settle(group(), Duration::from_millis(500)).await,
        "the posted picture is waited for, the slow forwarded one is not"
    );
    assert_eq!(r.lines.text(2), "[image:a red door] [image]");
    r.service.shutdown().await;
}
