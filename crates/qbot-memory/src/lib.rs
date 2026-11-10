//! Memory: who people are (identity, names), what happened (episodes) and what the bot remembers
//! about members and the group (memories).
//!
//! An *episode* is the summary of one fixed slice of a group's chat: a configured number of whole
//! chat batches, on the same batch grid the history window uses. It owns exactly that range; the
//! neighboring batches are shown to the extractor as context only. Episodes are the unit of
//! long-term recall and, because a summary never changes and covers whole batches, also the
//! natural stand-in for older chat when context has to shrink. Each episode's extraction also
//! finds the names its lines use for members and the statements worth remembering, which
//! consolidation applies to the name index and the memories.

pub mod build;
pub mod conformance;
pub mod consolidate;
pub mod episode;
pub mod extract;
pub mod findings;
pub mod history;
pub mod identity;
pub mod identity_conformance;
pub mod identity_store;
pub mod jobs;
pub mod memories;
pub mod memories_conformance;
pub mod names;
pub mod names_conformance;
pub mod notes;
pub mod notes_conformance;
pub mod recall;
pub mod rule;
pub mod slice;
pub mod store;
pub mod worth;

pub use build::{BuildError, BuilderConfig, Built, EpisodeBuilder};
pub use episode::{Episode, EpisodeId, Evidence, Hit, NewEpisode};
pub use extract::{EpisodeExtractor, ExtractError, Extracted, MAX_ATTEMPTS, METHOD, SliceContext};
pub use history::{HistoryPart, compose};
pub use identity::{
    IdentityError, IdentityPolicy, Invitation, InvitationState, LinkError, LinkOutcome,
};
pub use identity_store::{IdentityStore, MemoryIdentityStore};
pub use jobs::EpisodeJobs;
pub use memories::{
    Change, Durability, InMemoryMemoryStore, Memory, MemoryId, MemoryState, MemoryStore, Statement,
    Support,
};
pub use names::{
    CORROBORATED_USES, Grounds, HeldClaim, HeldId, KnownName, MemoryNameStore, NameBasis,
    NameError, NameStore, NameTextError, OpenName, Reference, Referent, Ruling, Verdict, name_key,
};
pub use notes::{MemoryNoteStore, Note, NoteId, NoteStore};
pub use recall::{Recall, RecallError, RecallParams, rank};
pub use rule::NameRuler;
pub use slice::{Background, SliceBackground, SliceLine, SliceMember};
pub use store::{EpisodeStore, MemoryEpisodeStore, MemoryError};
pub use worth::{Activity, Capacity, HalfLives};
