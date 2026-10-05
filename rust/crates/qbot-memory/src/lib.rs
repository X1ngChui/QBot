//! Memory: who people are (identity) and what happened (episodes).
//!
//! An *episode* is the summary of one fixed slice of a group's chat: a configured number of whole
//! chat batches, on the same batch grid the history window uses. It owns exactly that range; the
//! neighboring batches are shown to the extractor as context only. Episodes are the unit of
//! long-term recall and, because a summary never changes and covers whole batches, also the
//! natural stand-in for older chat when context has to shrink.

pub mod build;
pub mod conformance;
pub mod consolidate;
pub mod episode;
pub mod extract;
pub mod facts;
pub mod facts_conformance;
pub mod findings;
pub mod history;
pub mod identity;
pub mod identity_conformance;
pub mod identity_store;
pub mod jobs;
pub mod notes;
pub mod notes_conformance;
pub mod predicates;
pub mod recall;
pub mod slice;
pub mod store;

pub use build::{BuildError, BuilderConfig, Built, EpisodeBuilder};
pub use episode::{Episode, EpisodeId, Evidence, Hit, NewEpisode};
pub use extract::{
    EpisodeExtractor, ExtractError, Extracted, ExtractorConfig, METHOD, SliceContext,
};
pub use history::{HistoryPart, compose};
pub use identity::{
    Alias, AliasId, AliasStatus, AliasTarget, AliasTextError, EvidenceKind, EvidenceRecord, Holder,
    HolderId, IdentityError, IdentityPolicy, Invitation, InvitationState, LinkError,
    MAX_ALIAS_CHARS, MergeOutcome, Resolution, confidence, normalize_alias,
};
pub use identity_store::{IdentityStore, MemoryIdentityStore};
pub use jobs::EpisodeJobs;
pub use notes::{MemoryNoteStore, Note, NoteId, NoteStore};
pub use recall::{Recall, RecallError, RecallParams, rank};
pub use slice::SliceLine;
pub use store::{EpisodeStore, MemoryEpisodeStore, MemoryError};
