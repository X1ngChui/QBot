use qbot_core::{GroupId, MediaKind, MessageId};

/// How to get at one picture or clip. Any of the three may be missing; the fetcher tries what
/// it has.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaRef {
    /// A platform identifier (a file id, a sticker id). It names where the bytes come from, for
    /// holding an unreadable source; it never stands for what they show.
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
    pub kind: MediaKind,
    /// Which marker of its kind in the message this is (0 for the first `[image]`).
    pub index: usize,
    pub reference: MediaRef,
    /// Inside a forwarded record rather than posted here. Such an item gets a description only
    /// if its picture was described before: one forwarded album must not set off a burst of
    /// paid calls.
    pub nested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaJob {
    pub group: GroupId,
    pub message: MessageId,
    pub items: Vec<MediaItem>,
}
