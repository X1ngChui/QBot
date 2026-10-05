use qbot_core::{GroupId, MessageId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Image,
    /// A marketplace sticker: described like a picture, cached by its sticker id.
    Sticker,
    Voice,
}

impl Kind {
    /// The marker name in archived text: `[image]`, `[voice]`.
    pub fn marker(self) -> &'static str {
        match self {
            Kind::Image => "image",
            Kind::Sticker => "sticker",
            Kind::Voice => "voice",
        }
    }
}

/// How to get at one picture or clip. Any of the three may be missing; the fetcher tries what
/// it has.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaRef {
    /// A stable platform identifier (the file id), known before anything is downloaded.
    pub key: Option<String>,
    /// A link carried by the message. It may expire.
    pub url: Option<String>,
    /// A platform file name, from which a fresh link can be asked for later.
    pub file: Option<String>,
    /// Size in bytes, when the platform said.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaItem {
    pub kind: Kind,
    /// Which marker of its kind in the message this is (0 for the first `[image]`).
    pub index: usize,
    pub reference: MediaRef,
    /// Inside a forwarded record rather than posted here. Such an item gets a description only
    /// if one is already cached: one forwarded album must not set off a burst of paid calls.
    pub nested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaJob {
    pub group: GroupId,
    pub message: MessageId,
    pub items: Vec<MediaItem>,
}
