//! Memory: who people are (identity), what happened (episodes) and what the bot knows about
//! members and the group (memories: names and statements).
//!
//! Once a day each group's *batch* reads everything archived since its previous batch and divides
//! it into *episodes*, one per conversation. An episode owns exactly its lines and its summary
//! never changes: episodes are the unit of long-term recall and the stand-in for older chat in
//! what replies are shown. The batch also reads the names the lines use for people and the
//! statements worth remembering, decides per subject how they relate to what is known, and
//! commits everything at once. Nothing else writes memories but members' own commands, so what
//! replies are shown of a group stays the same from one batch to the next.

pub mod batch;
pub mod conformance;
pub mod decide;
pub mod episode;
pub mod findings;
pub mod history;
pub mod identity;
pub mod identity_conformance;
pub mod identity_store;
pub mod in_memory;
pub mod jobs;
pub mod memories;
pub mod names;
pub mod recall;
pub mod rule;
pub mod slice;
pub mod store;
pub mod worth;

pub use batch::{
    Answer, BatchError, BatchReader, MAX_ATTEMPTS, METHOD, Planned, Stretch, check_episodes,
};
pub use episode::{Episode, EpisodeId, Evidence, Hit, NewEpisode};
pub use history::{HistoryPart, compose};
pub use identity::{
    IdentityError, IdentityPolicy, Invitation, InvitationState, LinkError, LinkOutcome,
};
pub use identity_store::{IdentityStore, MemoryIdentityStore};
pub use in_memory::InMemoryStore;
pub use jobs::{Commands, EpisodeJobs, embedding_text};
pub use memories::{
    About, Commit, Committed, Durability, HandError, HandMemory, Kind, Memory, MemoryId,
    MemoryState, MemoryStore, Observation, ObservationId, Origin, Said, Suppression,
};
pub use names::{
    CORROBORATED_USES, Grounds, KnownName, NameBasis, NameTextError, OpenName, Referent, Verdict,
    known_names, name_key,
};
pub use recall::{Recall, RecallError, RecallParams, rank};
pub use slice::{Background, SliceBackground, SliceLine, SliceMember};
pub use store::{EpisodeStore, MemoryError, WindowState};
pub use worth::{Activity, Capacity, HalfLives};
