//! Pictures and voice clips: what they contain, in words, filled into the archived chat line.
//!
//! A message with media is archived at once with bare markers (`[image]`, `[voice]`). This crate
//! then fetches each item, describes the picture or transcribes the clip, and rewrites its
//! marker (`[image:a cat on a keyboard]`, `[voice:see you at eight]`). Raw bytes live only in
//! memory for the duration of the work. A description is cached by the picture's content and the
//! describer that wrote it, so a repost of the same picture under any id costs a download, not a
//! model call; once filled into a line it is part of the archive and never revisited.
//!
//! Everything vendor- or platform-specific is behind a port ([`Fetcher`], [`Describer`],
//! [`Transcriber`]), so the service is tested without a network or a model.

mod clean;
mod describe;
mod flight;
mod item;
mod open;
mod pg;
mod ports;
mod service;

pub use clean::{clean_description, sniff_mime};
pub use describe::LlmDescriber;
pub use item::{MediaItem, MediaJob, MediaRef};
pub use open::{
    ArchivedMedia, MediaRefs, OpenError, OpenImages, OpenImagesArgs, PictureArg, RefsError,
};
pub use pg::PgLineEditor;
pub use ports::{
    CacheError, DescribeError, Describer, DescriptionCache, EditError, FetchError, Fetcher,
    LineEditor, TranscribeError, Transcriber,
};
pub use service::{Admission, MediaConfig, MediaDeps, MediaService, WAV_BYTES_PER_SEC};
