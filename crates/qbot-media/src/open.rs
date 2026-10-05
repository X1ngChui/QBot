//! Letting the model look at a picture itself.
//!
//! A description is a summary written once; sometimes the question is about a detail it left out.
//! `open_images` puts the original in front of the model. Pictures are found by the message they
//! arrived in and their position in it, the same way the chat shows them, so no other ids exist.
//! Bytes are fetched on demand from the platform and kept only in a bounded in-memory cache, so
//! every later turn of the run that resends the conversation does not fetch them again.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::{Effect, Tool, ToolCx, ToolError, ToolOutput};
use qbot_context::Part;
use qbot_core::{GroupId, MessageId};
use qbot_llm::{LlmError, LoadedMedia, MediaStore};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::clean::sniff_mime;
use crate::item::MediaRef;
use crate::ports::{FetchError, Fetcher};
use qbot_core::MediaKind;

/// How much fetched picture data is kept in memory, and for how long after its last use. Internal
/// tuning: enough for the pictures of the runs in flight, not a deployment choice.
const CACHE_BYTES: u64 = 64 * 1024 * 1024;
const CACHE_IDLE: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RefsError(pub String);

/// Where the platform references of archived media are kept.
#[async_trait]
pub trait MediaRefs: Send + Sync {
    async fn media_ref(
        &self,
        group: GroupId,
        message: MessageId,
        kind: MediaKind,
        index: u32,
    ) -> Result<Option<MediaRef>, RefsError>;
}

/// One archived picture or sticker, as a stable key in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Address {
    group: GroupId,
    message: MessageId,
    kind: MediaKind,
    index: u32,
}

impl Address {
    fn key(self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.group.get(),
            self.message.get(),
            self.kind.marker(),
            self.index
        )
    }

    fn parse(key: &str) -> Option<Self> {
        let mut parts = key.split('/');
        let group = GroupId::new(parts.next()?.parse().ok()?).ok()?;
        let message = MessageId::new(parts.next()?.parse().ok()?).ok()?;
        // Only pictures can be opened.
        let kind = MediaKind::from_marker(parts.next()?).filter(|k| *k != MediaKind::Voice)?;
        let index = parts.next()?.parse().ok()?;
        parts.next().is_none().then_some(Self {
            group,
            message,
            kind,
            index,
        })
    }
}

/// Archived pictures, fetched on demand. It is also the [`MediaStore`] the provider reads image
/// bytes from when a conversation contains opened pictures.
pub struct ArchivedMedia {
    refs: Arc<dyn MediaRefs>,
    fetcher: Arc<dyn Fetcher>,
    max_bytes: u64,
    cache: moka::future::Cache<String, Arc<LoadedMedia>>,
}

impl std::fmt::Debug for ArchivedMedia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchivedMedia")
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    #[error("no such picture")]
    NoSuchPicture,
    #[error("picture too large")]
    TooLarge,
    #[error("picture unreadable")]
    Unreadable,
    #[error("{0}")]
    Storage(String),
}

impl ArchivedMedia {
    pub fn new(refs: Arc<dyn MediaRefs>, fetcher: Arc<dyn Fetcher>, max_bytes: u64) -> Self {
        let cache = moka::future::Cache::builder()
            .max_capacity(CACHE_BYTES)
            .weigher(|_key: &String, value: &Arc<LoadedMedia>| {
                u32::try_from(value.bytes.len()).unwrap_or(u32::MAX)
            })
            .time_to_idle(CACHE_IDLE)
            .build();
        Self {
            refs,
            fetcher,
            max_bytes,
            cache,
        }
    }

    async fn load_address(&self, address: Address) -> Result<Arc<LoadedMedia>, OpenError> {
        let key = address.key();
        if let Some(found) = self.cache.get(&key).await {
            return Ok(found);
        }
        let reference = self
            .refs
            .media_ref(address.group, address.message, address.kind, address.index)
            .await
            .map_err(|e| OpenError::Storage(e.0))?
            .ok_or(OpenError::NoSuchPicture)?;
        let bytes = self
            .fetcher
            .image(&reference, self.max_bytes)
            .await
            .map_err(|e| match e {
                FetchError::TooLarge => OpenError::TooLarge,
                FetchError::Unreadable => OpenError::Unreadable,
            })?;
        let loaded = Arc::new(LoadedMedia {
            mime: sniff_mime(&bytes).to_owned(),
            bytes,
        });
        self.cache.insert(key, Arc::clone(&loaded)).await;
        Ok(loaded)
    }
}

#[async_trait]
impl MediaStore for ArchivedMedia {
    async fn load(&self, key: &str) -> Result<LoadedMedia, LlmError> {
        let address = Address::parse(key)
            .ok_or_else(|| LlmError::InvalidRequest(format!("unknown picture {key:?}")))?;
        self.load_address(address)
            .await
            .map(|loaded| (*loaded).clone())
            .map_err(|e| LlmError::InvalidRequest(format!("picture {key:?}: {e}")))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PictureArg {
    pub message: i64,
    pub position: u32,
    #[serde(default)]
    pub sticker: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OpenImagesArgs {
    pub pictures: Vec<PictureArg>,
}

/// The `open_images` tool.
#[derive(Debug, Clone)]
pub struct OpenImages(pub Arc<ArchivedMedia>);

#[async_trait]
impl Tool for OpenImages {
    type Args = OpenImagesArgs;
    const NAME: &'static str = "open_images";

    fn description(&self) -> String {
        say(Text::OpenImagesDescription {})
    }

    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![
            ("pictures", say(Text::OpenImagesParamPictures {})),
            ("pictures.message", say(Text::OpenImagesParamMessage {})),
            ("pictures.position", say(Text::OpenImagesParamPosition {})),
            ("pictures.sticker", say(Text::OpenImagesParamSticker {})),
        ]
    }

    fn effect(&self) -> Effect {
        Effect::Read
    }

    async fn call(&self, cx: &ToolCx<'_>, args: OpenImagesArgs) -> Result<ToolOutput, ToolError> {
        if args.pictures.is_empty() {
            return Err(ToolError::InvalidArguments(say(Text::OpenImagesNone {})));
        }
        let mut content = Vec::new();
        for picture in &args.pictures {
            let message = MessageId::new(picture.message).map_err(|_| {
                ToolError::InvalidArguments(say(Text::OpenImagesNotMessageId {
                    id: picture.message.to_string(),
                }))
            })?;
            if !cx.view.has_message(message) {
                return Err(ToolError::InvalidArguments(say(
                    Text::OpenImagesNotInChat {
                        id: picture.message.to_string(),
                    },
                )));
            }
            let Some(index) = picture.position.checked_sub(1) else {
                return Err(ToolError::InvalidArguments(say(
                    Text::OpenImagesPosition {},
                )));
            };
            let kind = if picture.sticker {
                MediaKind::Sticker
            } else {
                MediaKind::Image
            };
            let address = Address {
                group: cx.group,
                message,
                kind,
                index,
            };
            let label = format!(
                "[msg:{}] {} {}",
                picture.message,
                kind.marker(),
                picture.position
            );
            match self.0.load_address(address).await {
                Ok(_) => {
                    content.push(Part::Text(format!("{label}:")));
                    content.push(Part::Image { key: address.key() });
                }
                Err(OpenError::Storage(error)) => return Err(ToolError::Unavailable(error)),
                // One picture that cannot be shown does not hide the others.
                Err(error) => {
                    let why = say(match error {
                        OpenError::TooLarge => Text::OpenImagesTooLarge {},
                        OpenError::Unreadable => Text::OpenImagesGone {},
                        OpenError::NoSuchPicture | OpenError::Storage(_) => {
                            Text::OpenImagesNoSuch {}
                        }
                    });
                    content.push(Part::Text(format!("{label}: {why}")));
                }
            }
        }
        Ok(ToolOutput {
            content,
            ends_run: false,
        })
    }
}
