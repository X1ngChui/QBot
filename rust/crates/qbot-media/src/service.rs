use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::join_all;
use qbot_core::{GroupId, MessageId};
use sha2::{Digest, Sha256};
use tokio::sync::{Semaphore, watch};
use tokio_util::task::TaskTracker;

use crate::clean::{clean_description, sniff_mime};
use crate::flight::Flights;
use crate::item::{Kind, MediaItem, MediaJob, MediaRef};
use crate::ports::{
    DescribeError, Describer, DescriptionCache, FetchError, Fetcher, LineEditor, Transcriber,
};

/// Bytes per second of 16 kHz 16-bit mono WAV, the only audio the service handles. The clip
/// length limit is stated in seconds and enforced as a byte cap through this.
pub const WAV_BYTES_PER_SEC: u64 = 32_000;

/// What is written in place of a clip nothing could be recognised in.
const UNCLEAR: &str = "unclear";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaConfig {
    /// Pictures described per group per minute.
    pub images_per_minute: u32,
    /// Clips transcribed per group per minute.
    pub clips_per_minute: u32,
    pub max_image_bytes: u64,
    pub max_audio: Duration,
    /// Fetches, model calls and recognitions running at once.
    pub concurrency: usize,
    /// Messages with media in flight; beyond it new media stays a bare marker.
    pub capacity: usize,
    /// How long a picture that could not be read (or was declined) is not tried again.
    pub unreadable_hold: Duration,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            images_per_minute: 6,
            clips_per_minute: 20,
            // A picture is untrusted input of any size; this is above what the vision providers
            // accept, so only absurd files are refused before fetching.
            max_image_bytes: 8 * 1024 * 1024,
            // Recognition runs on the CPU in the process; a longer clip would hold a worker for
            // minutes.
            max_audio: Duration::from_secs(300),
            concurrency: 4,
            capacity: 32,
            unreadable_hold: Duration::from_secs(600),
        }
    }
}

pub struct MediaDeps {
    pub fetcher: Arc<dyn Fetcher>,
    /// `None` leaves pictures as bare markers: description is off by choice.
    pub describer: Option<Arc<dyn Describer>>,
    /// `None` leaves voice as bare markers.
    pub transcriber: Option<Arc<dyn Transcriber>>,
    pub cache: Arc<dyn DescriptionCache>,
    pub editor: Arc<dyn LineEditor>,
}

impl std::fmt::Debug for MediaDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaDeps")
            .field("describer", &self.describer.is_some())
            .field("transcriber", &self.transcriber.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Nothing in the message needs (or can get) work.
    Nothing,
    Started,
    /// Too much media in flight; the markers stay bare.
    Overloaded,
    ShuttingDown,
}

struct Inner {
    cfg: MediaConfig,
    deps: MediaDeps,
    permits: Semaphore,
    flights: Arc<Flights>,
    /// Per-group rates: one busy group cannot use up the capacity of all.
    image_rate: GroupRate,
    clip_rate: GroupRate,
    /// Pictures not worth another attempt for a while, by flight key.
    unreadable: moka::sync::Cache<String, ()>,
    pending: Mutex<HashMap<(GroupId, MessageId), watch::Receiver<bool>>>,
    in_flight: AtomicUsize,
    closed: AtomicBool,
}

pub struct MediaService {
    inner: Arc<Inner>,
    tasks: TaskTracker,
}

impl std::fmt::Debug for MediaService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaService")
            .field("in_flight", &self.inner.in_flight.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

/// The cache key of a platform id. Sticker ids and picture file ids are different namespaces.
fn platform_key(kind: Kind, key: &str) -> String {
    match kind {
        Kind::Sticker => format!("p:sticker:{key}"),
        _ => format!("p:{key}"),
    }
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

type GroupRate = governor::DefaultKeyedRateLimiter<GroupId>;

/// At most `per_minute` a minute for each group, as a burst that refills evenly.
fn group_rate(per_minute: u32) -> GroupRate {
    let per_minute = NonZeroU32::new(per_minute).unwrap_or(NonZeroU32::MIN);
    governor::RateLimiter::keyed(governor::Quota::per_minute(per_minute))
}

/// Take one unit of `group`'s rate. The table forgets groups whose rate has refilled when it
/// grows, so a long-running process does not keep an entry for every group it has seen.
fn take(rate: &GroupRate, group: GroupId) -> bool {
    if rate.len() >= 1024 {
        rate.retain_recent();
    }
    rate.check_key(&group).is_ok()
}

impl Inner {
    fn flight_key(kind: Kind, reference: &MediaRef) -> Option<String> {
        let source = reference
            .key
            .as_deref()
            .or(reference.file.as_deref())
            .or(reference.url.as_deref())?;
        Some(format!("{}:{}", kind.marker(), digest(source.as_bytes())))
    }

    fn held(&self, key: &str) -> bool {
        self.unreadable.contains_key(key)
    }

    fn hold(&self, key: &str) {
        self.unreadable.insert(key.to_owned(), ());
    }

    async fn cached(&self, key: &str) -> Option<String> {
        match self.deps.cache.get(key).await {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(%error, "description cache unavailable");
                None
            }
        }
    }

    async fn remember(&self, key: &str, description: &str) {
        if let Err(error) = self.deps.cache.put(key, description).await {
            tracing::warn!(%error, "description could not be cached");
        }
    }

    /// The description of a picture, or `None` to leave its marker bare.
    async fn describe(
        self: &Arc<Self>,
        group: GroupId,
        kind: Kind,
        reference: &MediaRef,
    ) -> Option<String> {
        let describer = self.deps.describer.clone()?;
        let platform_key = reference.key.as_ref().map(|k| platform_key(kind, k));
        if let Some(key) = &platform_key
            && let Some(found) = self.cached(key).await
        {
            return Some(found);
        }
        if reference
            .size
            .is_some_and(|size| size > self.cfg.max_image_bytes)
        {
            return None;
        }
        let flight_key = Self::flight_key(kind, reference)?;
        if self.held(&flight_key) {
            tracing::debug!("picture recently unreadable or declined; not retried");
            return None;
        }
        if !take(&self.image_rate, group) {
            tracing::info!(
                group = group.get(),
                "picture description rate limited; left as it is"
            );
            return None;
        }
        let this = Arc::clone(self);
        let reference = reference.clone();
        let flights = Arc::clone(&self.flights);
        flights
            .run(flight_key.clone(), async move {
                let _permit = this.permits.acquire().await.ok()?;
                let bytes = match this
                    .deps
                    .fetcher
                    .image(&reference, this.cfg.max_image_bytes)
                    .await
                {
                    Ok(bytes) => bytes,
                    Err(FetchError::TooLarge) => return None,
                    Err(FetchError::Unreadable) => {
                        tracing::warn!(
                            "picture unreadable by every route; not retried for a while"
                        );
                        this.hold(&flight_key);
                        return None;
                    }
                };
                // The same picture may arrive under another id.
                let content_key = format!("h:{}", digest(&bytes));
                if let Some(found) = this.cached(&content_key).await {
                    if let Some(key) = &platform_key {
                        this.remember(key, &found).await;
                    }
                    return Some(found);
                }
                let described = describer.describe(&bytes, sniff_mime(&bytes)).await;
                let text = match described {
                    Ok(text) => clean_description(&text),
                    Err(DescribeError::Declined) => {
                        tracing::info!("the vision model declined a picture; left unseen");
                        this.hold(&flight_key);
                        return None;
                    }
                    Err(DescribeError::Failed(error)) => {
                        tracing::warn!(%error, "picture description failed");
                        return None;
                    }
                };
                if text.is_empty() {
                    return None;
                }
                this.remember(&content_key, &text).await;
                if let Some(key) = &platform_key {
                    this.remember(key, &text).await;
                }
                Some(text)
            })
            .await
    }

    /// The words of a clip, `unclear` when none were recognised, or `None` to leave it bare.
    async fn transcribe(self: &Arc<Self>, group: GroupId, reference: &MediaRef) -> Option<String> {
        let transcriber = self.deps.transcriber.clone()?;
        let max_bytes = self
            .cfg
            .max_audio
            .as_secs()
            .saturating_mul(WAV_BYTES_PER_SEC);
        if reference
            .size
            .is_some_and(|size| size > max_bytes.saturating_mul(8))
        {
            // Stored clips are far smaller than their WAV form, so only an absurd size is
            // refused before fetching; the exact cap is enforced on the converted audio.
            return None;
        }
        let flight_key = Self::flight_key(Kind::Voice, reference)?;
        if self.held(&flight_key) {
            return None;
        }
        if !take(&self.clip_rate, group) {
            tracing::info!(
                group = group.get(),
                "voice transcription rate limited; left as it is"
            );
            return None;
        }
        let this = Arc::clone(self);
        let reference = reference.clone();
        let flights = Arc::clone(&self.flights);
        flights
            .run(flight_key.clone(), async move {
                let _permit = this.permits.acquire().await.ok()?;
                let wav = match this.deps.fetcher.voice(&reference, max_bytes).await {
                    Ok(wav) => wav,
                    Err(FetchError::TooLarge) => return None,
                    Err(FetchError::Unreadable) => {
                        tracing::warn!(
                            "voice clip unreadable by every route; not retried for a while"
                        );
                        this.hold(&flight_key);
                        return None;
                    }
                };
                match transcriber.transcribe(&wav).await {
                    Ok(text) => {
                        let text = clean_description(&text);
                        Some(if text.is_empty() {
                            UNCLEAR.to_owned()
                        } else {
                            text
                        })
                    }
                    Err(error) => {
                        tracing::warn!(%error, "voice transcription failed");
                        None
                    }
                }
            })
            .await
    }

    /// A description already paid for, by platform id. Costs nothing.
    async fn cached_description(&self, kind: Kind, reference: &MediaRef) -> Option<String> {
        let key = reference.key.as_ref()?;
        self.cached(&platform_key(kind, key)).await
    }

    async fn process(self: &Arc<Self>, job: &MediaJob, item: &MediaItem) {
        let marker = item.kind.marker();
        let replacement = match item.kind {
            Kind::Image | Kind::Sticker if item.nested => self
                .cached_description(item.kind, &item.reference)
                .await
                .map(|text| format!("[{marker}:{text}]")),
            Kind::Image | Kind::Sticker => self
                .describe(job.group, item.kind, &item.reference)
                .await
                .map(|text| format!("[{marker}:{text}]")),
            Kind::Voice => self
                .transcribe(job.group, &item.reference)
                .await
                .map(|text| format!("[{marker}:{text}]")),
        };
        let Some(replacement) = replacement else {
            return;
        };
        match self
            .deps
            .editor
            .fill(job.group, job.message, item.kind, item.index, &replacement)
            .await
        {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                message = job.message.get(),
                "no marker to fill in the archived line"
            ),
            Err(error) => tracing::error!(%error, "could not fill a media marker"),
        }
    }
}

impl MediaService {
    pub fn new(cfg: MediaConfig, deps: MediaDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                permits: Semaphore::new(cfg.concurrency.max(1)),
                image_rate: group_rate(cfg.images_per_minute),
                clip_rate: group_rate(cfg.clips_per_minute),
                unreadable: moka::sync::Cache::builder()
                    .time_to_live(cfg.unreadable_hold)
                    .build(),
                cfg,
                deps,
                flights: Arc::default(),
                pending: Mutex::default(),
                in_flight: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
            }),
            tasks: TaskTracker::new(),
        }
    }

    /// Start work on a message's media. Never blocks: the work runs on its own task.
    pub fn admit(&self, job: MediaJob) -> Admission {
        let inner = &self.inner;
        if inner.closed.load(Ordering::SeqCst) {
            return Admission::ShuttingDown;
        }
        let items: Vec<MediaItem> = job
            .items
            .iter()
            .filter(|item| match item.kind {
                // A nested picture only reads the cache, which needs no describer.
                Kind::Image | Kind::Sticker => item.nested || inner.deps.describer.is_some(),
                Kind::Voice => inner.deps.transcriber.is_some(),
            })
            .cloned()
            .collect();
        if items.is_empty() {
            return Admission::Nothing;
        }
        let reserved = inner
            .in_flight
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < inner.cfg.capacity).then_some(n + 1)
            });
        if reserved.is_err() {
            tracing::info!(
                message = job.message.get(),
                "too much media in flight; left as it is"
            );
            return Admission::Overloaded;
        }
        let (done_tx, done_rx) = watch::channel(false);
        let key = (job.group, job.message);
        inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, done_rx);
        let inner = Arc::clone(inner);
        self.tasks.spawn(async move {
            let job = MediaJob { items, ..job };
            join_all(job.items.iter().map(|item| inner.process(&job, item))).await;
            inner
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&key);
            inner.in_flight.fetch_sub(1, Ordering::SeqCst);
            let _ = done_tx.send(true);
        });
        Admission::Started
    }

    /// Wait until a message's media work has finished, up to `timeout`. True when there is
    /// nothing (left) to wait for.
    pub async fn wait(&self, group: GroupId, message: MessageId, timeout: Duration) -> bool {
        let receiver = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(group, message))
            .cloned();
        let Some(mut receiver) = receiver else {
            return true;
        };
        tokio::time::timeout(timeout, receiver.wait_for(|done| *done))
            .await
            .is_ok()
    }

    /// Stop accepting work and let what is in flight finish.
    pub async fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.tasks.close();
        self.tasks.wait().await;
    }
}
