use async_trait::async_trait;
use qbot_core::{GroupId, MessageId};

use crate::item::{Kind, MediaRef};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    #[error("larger than the allowed size")]
    TooLarge,
    /// No route to the bytes worked (an expired link, a file the platform no longer serves).
    #[error("unreadable by every route")]
    Unreadable,
}

/// Gets the bytes of a picture or clip. Voice comes back as 16 kHz 16-bit mono WAV.
#[async_trait]
pub trait Fetcher: Send + Sync {
    async fn image(&self, reference: &MediaRef, max_bytes: u64) -> Result<Vec<u8>, FetchError>;
    async fn voice(&self, reference: &MediaRef, max_bytes: u64) -> Result<Vec<u8>, FetchError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DescribeError {
    /// The model looked and will not describe this picture (a content filter, an image it cannot
    /// read). Repeating the request would only be refused again.
    #[error("the model declined this picture")]
    Declined,
    #[error("{0}")]
    Failed(String),
}

#[async_trait]
pub trait Describer: Send + Sync {
    async fn describe(&self, bytes: &[u8], mime: &str) -> Result<String, DescribeError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TranscribeError(pub String);

#[async_trait]
pub trait Transcriber: Send + Sync {
    /// The words in a WAV clip; empty when nothing was recognised.
    async fn transcribe(&self, wav: &[u8]) -> Result<String, TranscribeError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CacheError(pub String);

/// What pictures were described as. Keys are opaque to the caller.
#[async_trait]
pub trait DescriptionCache: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError>;
    async fn put(&self, key: &str, description: &str) -> Result<(), CacheError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct EditError(pub String);

/// Fills in a marker of an archived line.
#[async_trait]
pub trait LineEditor: Send + Sync {
    /// Replace the `index`-th marker of `kind` in the line. Returns whether a marker was found.
    async fn fill(
        &self,
        group: GroupId,
        message: MessageId,
        kind: Kind,
        index: usize,
        replacement: &str,
    ) -> Result<bool, EditError>;
}
