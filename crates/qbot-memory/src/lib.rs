//! Memory: who people are (identity, names), what happened (episodes) and what the bot remembers
//! about members and the group (memories).
//!
//! Once a day each group's *batch* reads everything archived since its previous batch and divides
//! it into *episodes*, one per conversation. An episode owns exactly its lines and its summary
//! never changes: episodes are the unit of long-term recall and the stand-in for older chat in
//! what replies are shown. The batch also finds the names the lines use for people and the
//! statements worth remembering, which consolidation applies to the name ledger and the
//! memories. Nothing else writes them, so what replies are shown of a group stays the same from
//! one batch to the next.

pub mod batch;
pub mod conformance;
pub mod consolidate;
pub mod episode;
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

pub use batch::{
    Answer, BatchError, BatchReader, Earlier, MAX_ATTEMPTS, METHOD, Planned, Stretch,
    check_episodes,
};
pub use episode::{Episode, EpisodeId, Evidence, Hit, NewEpisode};
pub use history::{HistoryPart, compose};
pub use identity::{
    IdentityError, IdentityPolicy, Invitation, InvitationState, LinkError, LinkOutcome,
};
pub use identity_store::{IdentityStore, MemoryIdentityStore};
pub use jobs::{EpisodeJobs, embedding_text};
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
pub use slice::{Background, SliceBackground, SliceLine, SliceMember};
pub use store::{EpisodeStore, MemoryEpisodeStore, MemoryError, WindowState};
pub use worth::{Activity, Capacity, HalfLives};
