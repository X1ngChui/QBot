//! Names people use for one another.
//!
//! A name is a memory of its member ([`crate::memories`]); what supports it are its *uses*: in
//! this episode, on this line, this name was used for the member, with how the line used it and
//! what showed who was meant. Nothing is decided from one use. Whom a name belongs to is read
//! from all its uses together ([`KnownName`]), so a name used for one member in eight
//! conversations reads differently from one used once, and a name used for several members is
//! visibly shared.
//!
//! A use nobody could place waits as a pending observation about the name. A name used again
//! and again that way is put to a ruling with its uses from several conversations, which places
//! it with a member or says what it means in the group. A person can enter a name by hand and
//! forget one; both outrank everything read from chat.
//!
//! What a member is *shown* as (their group display name) is not a use: the archive records it
//! with every line they send.

use std::collections::{BTreeMap, BTreeSet};

use qbot_core::{AccountId, UnixMillis};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::episode::EpisodeId;
use crate::memories::{About, Kind, Memory, MemoryId, Observation, Origin, Said};

/// The longest name stored. A name is a few words; this keeps absurd input out of the index.
pub const MAX_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameTextError {
    #[error("a name must not be empty")]
    Empty,
    #[error("a name is at most {MAX_NAME_CHARS} characters")]
    TooLong,
}

/// The form names are compared in: compatibility-normalized (full-width and half-width forms
/// agree), whitespace collapsed, lower-cased.
pub fn name_key(text: &str) -> Result<String, NameTextError> {
    let folded: String = text.nfkc().collect::<String>().to_lowercase();
    let collapsed = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err(NameTextError::Empty);
    }
    if collapsed.chars().count() > MAX_NAME_CHARS {
        return Err(NameTextError::TooLong);
    }
    Ok(collapsed)
}

/// A name as it is kept and shown: as written, with its whitespace collapsed.
pub fn tidy_name(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How a name is used for a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameBasis {
    /// The member used it for themselves.
    Own,
    /// Someone spoke to the member by it.
    Addressed,
    /// Someone spoke of the member by it.
    About,
}

impl NameBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            NameBasis::Own => "own",
            NameBasis::Addressed => "addressed",
            NameBasis::About => "about",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "own" => NameBasis::Own,
            "addressed" => NameBasis::Addressed,
            "about" => NameBasis::About,
            _ => return None,
        })
    }

    /// The stronger of two uses in one episode: the member's own word, then addressing them,
    /// then talk about them.
    pub(crate) fn strength(self) -> u8 {
        match self {
            NameBasis::Own => 2,
            NameBasis::Addressed => 1,
            NameBasis::About => 0,
        }
    }
}

/// What showed the reader who an expression named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grounds {
    /// The line replies to the person or mentions them.
    Marker,
    /// The person answers the line, or the line answers them, and what is said shows it.
    Answer,
    /// What the line says of the person fits them and no one else.
    Content,
    /// The speaker names themselves.
    Own,
    /// The expression is a name the group already has for the member, or a form of one.
    Listed,
    /// A ruling over the expression's uses across conversations.
    Ruling,
    /// Nothing did: who is meant is not known.
    None,
}

impl Grounds {
    pub fn as_str(self) -> &'static str {
        match self {
            Grounds::Marker => "marker",
            Grounds::Answer => "answer",
            Grounds::Content => "content",
            Grounds::Own => "self",
            Grounds::Listed => "listed",
            Grounds::Ruling => "ruling",
            Grounds::None => "none",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "marker" => Grounds::Marker,
            "answer" => Grounds::Answer,
            "content" => Grounds::Content,
            "self" => Grounds::Own,
            "listed" => Grounds::Listed,
            "ruling" => Grounds::Ruling,
            "none" => Grounds::None,
            _ => return None,
        })
    }

    /// Whether the conversation itself showed who was meant, rather than what is already
    /// believed about them: a name that rests only on what is believed would confirm itself.
    pub fn in_the_chat(self) -> bool {
        matches!(self, Grounds::Marker | Grounds::Answer | Grounds::Own)
    }
}

/// Who an expression names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Referent {
    Member(AccountId),
    Bot,
    /// A person who is not a member of the group.
    Outside,
    /// The lines did not show who.
    Unknown,
}

impl Referent {
    pub fn as_str(self) -> &'static str {
        match self {
            Referent::Member(_) => "member",
            Referent::Bot => "bot",
            Referent::Outside => "outside",
            Referent::Unknown => "unknown",
        }
    }

    pub fn member(self) -> Option<AccountId> {
        match self {
            Referent::Member(account) => Some(account),
            _ => None,
        }
    }
}

/// What the uses of one name for one member add up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownName {
    /// The name memory.
    pub id: MemoryId,
    pub account: AccountId,
    /// As most recently written.
    pub text: String,
    pub key: String,
    /// A person entered it by hand.
    pub set: bool,
    /// A ruling over the name's uses across conversations placed it with this member.
    pub ruled: bool,
    /// Distinct episodes in which the member used it for themselves, someone spoke to them by
    /// it, someone spoke of them by it.
    pub own: u32,
    pub addressed: u32,
    pub about: u32,
    /// Of the episodes in which others used it, those in which the chat's structure tied it to
    /// the member.
    pub anchored: u32,
    /// Of the episodes in which others used it, those in which the conversation itself showed
    /// who was meant (a marker or an answer), not only what is believed about the member.
    pub grounded: u32,
    /// The latest use.
    pub last: UnixMillis,
}

/// Conversations in which others must have used a name for a member before it counts without
/// any anchor: one line a model read as naming someone is a guess, two conversations agree.
pub const CORROBORATED_USES: u32 = 2;

/// Uses nobody could place, and the conversations they must come from, before a name is put to
/// a ruling: fewer are a passing word, and one conversation offers nothing to compare.
pub const OPEN_USES: u32 = 3;
pub const OPEN_EPISODES: u32 = 2;

impl KnownName {
    /// Episodes in which the name was used for the member.
    pub fn uses(&self) -> u32 {
        self.own + self.addressed + self.about
    }

    /// Whether the records alone tie the name to the member: a person set it, a ruling placed
    /// it, the member used it for themselves, or the chat's structure tied it to them. (A name
    /// contained in the member's display name is anchored too; that is for whoever knows the
    /// display names to say.)
    pub fn vouched(&self) -> bool {
        self.set || self.ruled || self.own > 0 || self.anchored > 0
    }

    /// Whether separate conversations agree on the name, at least one of them by what the chat
    /// showed rather than by what is believed about the member.
    pub fn corroborated(&self) -> bool {
        self.uses() >= CORROBORATED_USES && self.own + self.grounded > 0
    }
}

/// A current name memory read as its uses add up; `None` for anything else.
pub fn known_name(memory: &Memory) -> Option<KnownName> {
    let account = memory.subject?;
    if memory.kind != Kind::Name || !memory.is_current() {
        return None;
    }
    struct Episode {
        basis: NameBasis,
        anchored: bool,
        grounded: bool,
    }
    let mut episodes: BTreeMap<EpisodeId, Episode> = BTreeMap::new();
    for one in &memory.support {
        let Said::Name {
            basis,
            grounds,
            anchored,
            ..
        } = &one.said
        else {
            continue;
        };
        // The strongest use stands for the episode; what any of its uses rests on is kept.
        let episode = episodes.entry(one.episode).or_insert(Episode {
            basis: *basis,
            anchored: false,
            grounded: false,
        });
        if basis.strength() > episode.basis.strength() {
            episode.basis = *basis;
        }
        episode.anchored |= *anchored;
        episode.grounded |= *anchored || grounds.in_the_chat();
    }
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let of = |basis: NameBasis| episodes.values().filter(move |e| e.basis == basis).count();
    let others = || episodes.values().filter(|e| e.basis != NameBasis::Own);
    Some(KnownName {
        id: memory.id,
        account,
        text: memory.text.clone(),
        key: memory.key(),
        set: memory.origin.is_hand(),
        ruled: memory.origin == Origin::Ruling,
        own: count(of(NameBasis::Own)),
        addressed: count(of(NameBasis::Addressed)),
        about: count(of(NameBasis::About)),
        anchored: count(others().filter(|e| e.anchored).count()),
        grounded: count(others().filter(|e| e.grounded).count()),
        last: memory.last,
    })
}

/// The group's names from its name memories, ordered by account, then key.
pub fn known_names(memories: &[Memory]) -> Vec<KnownName> {
    let mut out: Vec<KnownName> = memories.iter().filter_map(known_name).collect();
    out.sort_by(|a, b| (a.account, &a.key).cmp(&(b.account, &b.key)));
    out
}

/// What a ruling says a name is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A name of this member.
    Member(AccountId),
    /// A name of someone who is not a member; what the group means by it.
    Outside(String),
    /// A general form of address or an ordinary word, nobody's name; what it means.
    General(String),
    /// The uses do not show whose it is.
    Unknown,
}

/// A name waiting for a ruling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenName {
    pub key: String,
    /// As most recently written.
    pub text: String,
    /// Its uses nobody could place, and the episodes they are from.
    pub unknown: u32,
    pub episodes: u32,
}

/// The names to put to a ruling: uses nobody could place (`uses`: observations about a name),
/// at least [`OPEN_USES`] of them from at least [`OPEN_EPISODES`] episodes, one of them among
/// `fresh` (today's: the same uses would be ruled the same), and none `settled` (a member's name
/// already, or the group's word). The most used first, at most `limit`.
pub fn open_names(
    uses: &[&Observation],
    fresh: &BTreeSet<String>,
    settled: &BTreeSet<String>,
    limit: usize,
) -> Vec<OpenName> {
    struct Seen {
        text: String,
        last: (UnixMillis, u64),
        unknown: u32,
        episodes: BTreeSet<EpisodeId>,
    }
    let mut seen: BTreeMap<String, Seen> = BTreeMap::new();
    for one in uses {
        let (About::Name(_), Said::Name { text, .. }) = (&one.about, &one.said) else {
            continue;
        };
        let key = one.key();
        let entry = seen.entry(key).or_insert_with(|| Seen {
            text: text.clone(),
            last: (one.at, one.ordinal),
            unknown: 0,
            episodes: BTreeSet::new(),
        });
        entry.unknown += 1;
        entry.episodes.insert(one.episode);
        if (one.at, one.ordinal) >= entry.last {
            entry.last = (one.at, one.ordinal);
            entry.text = text.clone();
        }
    }
    let mut open: Vec<OpenName> = seen
        .into_iter()
        .filter(|(key, s)| {
            fresh.contains(key)
                && !settled.contains(key)
                && s.unknown >= OPEN_USES
                && s.episodes.len() >= OPEN_EPISODES as usize
        })
        .map(|(key, s)| OpenName {
            key,
            text: s.text,
            unknown: s.unknown,
            episodes: u32::try_from(s.episodes.len()).unwrap_or(u32::MAX),
        })
        .collect();
    open.sort_by(|a, b| b.unknown.cmp(&a.unknown).then_with(|| a.key.cmp(&b.key)));
    open.truncate(limit);
    open
}
