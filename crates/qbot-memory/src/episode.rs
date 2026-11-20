//! The episode: one conversation of a group's chat with an immutable summary.
//!
//! A group's batch reads everything archived since the previous one and divides it into
//! episodes where the conversations divide. An episode owns exactly its lines; chat shown around
//! them for context is not part of the range, and the archive stays authoritative.

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

/// A verbatim quote from the episode's lines that grounds the summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub message: MessageId,
    pub quote: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewEpisode {
    pub group: GroupId,
    /// The lines it owns, as archive ordinals (1-based, inclusive).
    pub first_ordinal: u64,
    pub last_ordinal: u64,
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
    /// Name uses and memory changes found in its lines, validated, waiting to be (or already)
    /// applied to the name index and the memories. Stored with the episode so nothing found is
    /// lost between the two.
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
