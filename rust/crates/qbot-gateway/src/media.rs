//! Media on the NapCat side: which pictures and clips a message carries, and how to get their
//! bytes.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt;
use qbot_media::{FetchError, Fetcher, Kind, MediaItem, MediaJob, MediaRef};
use serde_json::{Value, json};

use crate::bridge::Bridge;
use crate::render::RenderContext;
use crate::wire::{GroupMessage, Segment};

/// The pictures, stickers and clips of a message, each with its position among the markers of
/// its kind in the rendered line. The walk mirrors [`render`](crate::render::render) exactly,
/// including the messages of a forwarded record that are shown, so positions always match.
pub fn job_for(message: &GroupMessage, ctx: &RenderContext) -> MediaJob {
    let mut walk = Walk::default();
    walk.collect(&message.segments, ctx, false);
    MediaJob {
        group: message.group,
        message: message.message,
        items: walk.items,
    }
}

#[derive(Default)]
struct Walk {
    images: usize,
    stickers: usize,
    clips: usize,
    items: Vec<MediaItem>,
}

impl Walk {
    fn collect(&mut self, segments: &[Segment], ctx: &RenderContext, nested: bool) {
        for segment in segments {
            match segment {
                Segment::Image { file, url, size } => {
                    self.items.push(MediaItem {
                        kind: Kind::Image,
                        index: self.images,
                        reference: MediaRef {
                            key: file.clone().or_else(|| url.clone()),
                            url: url.clone(),
                            file: file.clone(),
                            size: *size,
                        },
                        nested,
                    });
                    self.images += 1;
                }
                Segment::Sticker { url, key, .. } => {
                    self.items.push(MediaItem {
                        kind: Kind::Sticker,
                        index: self.stickers,
                        reference: MediaRef {
                            key: key.clone().or_else(|| url.clone()),
                            url: url.clone(),
                            file: None,
                            size: None,
                        },
                        nested,
                    });
                    self.stickers += 1;
                }
                Segment::Voice { file, url } => {
                    // A clip inside a forwarded record is never transcribed: it was not posted
                    // here, and recognition has no cache to reuse.
                    if !nested {
                        self.items.push(MediaItem {
                            kind: Kind::Voice,
                            index: self.clips,
                            reference: MediaRef {
                                key: file.clone().or_else(|| url.clone()),
                                url: url.clone(),
                                file: file.clone(),
                                size: None,
                            },
                            nested,
                        });
                    }
                    self.clips += 1;
                }
                Segment::Forward { nodes: Some(nodes) } if !nested => {
                    for node in nodes.iter().take(ctx.forward_max_lines) {
                        self.collect(&node.segments, ctx, true);
                    }
                }
                _ => {}
            }
        }
    }
}

/// The links worth trying for a picture, best first. A marketplace sticker arrives as a link
/// into its directory on the sticker CDN, usually to an animated file that often is not there;
/// the same directory serves a still 300x300 PNG for every sticker, so that is asked for first.
fn links(url: &str) -> Vec<String> {
    const STICKER_CDN: &str = "gxh.vip.qq.com/club/item/parcel/item/";
    if !url.contains(STICKER_CDN) {
        return vec![url.to_owned()];
    }
    let mut base = url.trim_end_matches('/');
    if base
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'))
    {
        base = base.rsplit_once('/').map_or(base, |(dir, _)| dir);
    }
    let png = format!("{base}/300x300.png");
    if png == url {
        vec![png]
    } else {
        vec![png, url.to_owned()]
    }
}

#[derive(Debug, Clone)]
pub struct FetchSettings {
    /// One HTTP download.
    pub http_timeout: Duration,
    /// One platform action (`get_image`, `get_record`). A file the platform can no longer serve
    /// does not fail, it hangs, so this must be short.
    pub protocol_timeout: Duration,
}

/// Fetches pictures and clips by whichever route answers.
pub struct OneBotFetcher {
    bridge: Arc<Bridge>,
    http: reqwest::Client,
    settings: FetchSettings,
}

impl std::fmt::Debug for OneBotFetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneBotFetcher").finish_non_exhaustive()
    }
}

impl OneBotFetcher {
    pub fn new(bridge: Arc<Bridge>, settings: FetchSettings) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(settings.http_timeout)
            .build()?;
        Ok(Self {
            bridge,
            http,
            settings,
        })
    }

    async fn action(&self, name: &str, params: Value) -> Option<Value> {
        let response = self
            .bridge
            .call(name, params, self.settings.protocol_timeout)
            .await
            .ok()?;
        response.ok.then_some(response.data)
    }

    /// `Ok(None)`: this route did not answer, try the next. `Err`: the verdict is final.
    async fn download(&self, url: &str, max_bytes: u64) -> Result<Option<Vec<u8>>, FetchError> {
        let response = match self.http.get(url).send().await {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        if !response.status().is_success() {
            return Ok(None);
        }
        if response.content_length().is_some_and(|n| n > max_bytes) {
            return Err(FetchError::TooLarge);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else { return Ok(None) };
            if (body.len() + chunk.len()) as u64 > max_bytes {
                return Err(FetchError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((!body.is_empty()).then_some(body))
    }

    /// The bytes an action's answer carries: inline (NapCat sends them when its
    /// `enableLocalFile2Url` is on), else an HTTP link. A local path in the answer is NapCat's
    /// own file and is never read.
    async fn bytes_of_answer(
        &self,
        answer: &Value,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, FetchError> {
        if let Some(encoded) = answer.get("base64").and_then(Value::as_str) {
            let encoded = encoded
                .split_once(',')
                .filter(|_| encoded.starts_with("data:"))
                .map_or(encoded, |(_, rest)| rest);
            // Reject before decoding: base64 is 4 bytes per 3.
            if encoded.len() as u64 > max_bytes.saturating_mul(4) / 3 + 8 {
                return Err(FetchError::TooLarge);
            }
            if let Ok(data) = STANDARD.decode(encoded) {
                if data.len() as u64 > max_bytes {
                    return Err(FetchError::TooLarge);
                }
                if !data.is_empty() {
                    return Ok(Some(data));
                }
            }
        }
        if let Some(url) = answer
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        {
            return self.download(url, max_bytes).await;
        }
        Ok(None)
    }
}

#[async_trait]
impl Fetcher for OneBotFetcher {
    /// The link the message carried (it may have expired), then a fresh copy from the platform.
    async fn image(&self, reference: &MediaRef, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
        for link in reference.url.as_deref().map(links).unwrap_or_default() {
            if let Some(data) = self.download(&link, max_bytes).await? {
                return Ok(data);
            }
        }
        if let Some(file) = &reference.file
            && let Some(answer) = self.action("get_image", json!({ "file": file })).await
            && let Some(data) = self.bytes_of_answer(&answer, max_bytes).await?
        {
            return Ok(data);
        }
        Err(FetchError::Unreadable)
    }

    /// Always through `get_record` with a WAV conversion. The stored file and the CDN link both
    /// hold the platform's native SILK audio whatever their suffix says, and a recogniser fed
    /// SILK answers with an empty transcript for a perfectly clear clip.
    async fn voice(&self, reference: &MediaRef, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
        let Some(file) = reference.file.as_deref().or(reference.url.as_deref()) else {
            return Err(FetchError::Unreadable);
        };
        let Some(answer) = self
            .action("get_record", json!({ "file": file, "out_format": "wav" }))
            .await
        else {
            return Err(FetchError::Unreadable);
        };
        self.bytes_of_answer(&answer, max_bytes)
            .await?
            .ok_or(FetchError::Unreadable)
    }
}
