//! The episode: one slice of a group's chat with an immutable summary.
//!
//! An episode owns exactly the lines of its slice (whole batches). Neighboring chat shown to the
//! extractor for context is not part of the source range, and the archive stays authoritative.

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use serde::{Deserialize, Serialize};

use crate::findings::Findings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EpisodeId(i64);

impl EpisodeId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

/// A verbatim quote from the slice that grounds the summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub message: MessageId,
    pub quote: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewEpisode {
    pub group: GroupId,
    /// The batches the episode owns (0-based, inclusive).
    pub first_batch: u64,
    pub last_batch: u64,
    /// The same range as archive ordinals (1-based, inclusive).
    pub first_ordinal: u64,
    pub last_ordinal: u64,
    /// Lines per batch of the grid the range was cut on, for attribution.
    pub batch_lines: u32,
    pub first_message: MessageId,
    pub last_message: MessageId,
    pub started: UnixMillis,
    pub ended: UnixMillis,
    pub line_count: u32,
    pub participants: Vec<AccountId>,
    pub title: String,
    pub summary: String,
    pub evidence: Vec<Evidence>,
    /// Which prompt and rules produced it.
    pub method: String,
    pub model: String,
    /// Names, facts and group knowledge found in the slice, validated, waiting to be (or already)
    /// applied to identity and facts. Stored with the episode so nothing found is lost between
    /// the two.
    #[serde(default)]
    pub findings: Findings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    pub id: EpisodeId,
    pub created: UnixMillis,
    #[serde(flatten)]
    pub episode: NewEpisode,
}

/// One retrieval result: how far the episode is from the query (cosine distance).
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub episode: Episode,
    pub distance: f32,
}
