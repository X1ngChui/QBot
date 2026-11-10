//! Names people use for one another, kept as a ledger of references.
//!
//! A *reference* is one recorded use: in this episode, on this line, this expression named this
//! person (a member, the bot, someone outside the group) or someone the lines did not let the
//! reader place, with how the line used it and what showed who was meant. Nothing is decided
//! from one reference. Whom a name belongs to is read from all its references together
//! ([`KnownName`] per member and name), so a name used for one member eight times and for
//! another once reads exactly so, and a name used for several members is visibly shared.
//!
//! An expression used again and again for someone no single conversation could place is put
//! before a model with its uses from several conversations; what it rules ([`Ruling`]) places
//! the expression's unplaced references with a member, or says it is no member's name. A person
//! can set a name with `/name` and remove one; both outrank everything read from chat.
//!
//! What a member is *shown* as (their group display name) is not a reference: the archive
//! records it with every line they send.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::episode::EpisodeId;
use crate::memories::{Durability, Support};
use crate::store::MemoryError;

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
    /// A person set it with `/name`: vouched for, not inferred. Never a reference's.
    Set,
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
            NameBasis::Set => "set",
            NameBasis::Own => "own",
            NameBasis::Addressed => "addressed",
            NameBasis::About => "about",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "set" => NameBasis::Set,
            "own" => NameBasis::Own,
            "addressed" => NameBasis::Addressed,
            "about" => NameBasis::About,
            _ => return None,
        })
    }

    /// The stronger of two uses in one episode: the member's own word, then addressing them,
    /// then talk about them.
    fn strength(self) -> u8 {
        match self {
            NameBasis::Set => 3,
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

/// One use of an expression for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub episode: EpisodeId,
    /// The line: its message, its place in the group's chat and who wrote it.
    pub message: MessageId,
    pub ordinal: u64,
    pub speaker: AccountId,
    /// The expression as written.
    pub text: String,
    pub referent: Referent,
    /// How the line uses it; never [`NameBasis::Set`].
    pub basis: NameBasis,
    pub grounds: Grounds,
    /// The chat's structure ties the expression to the member: the line is said to them (it
    /// replies to them or mentions them), or they reply to it.
    pub anchored: bool,
    /// The lines that showed who was meant.
    pub shown_by: Vec<MessageId>,
    pub at: UnixMillis,
}

impl Reference {
    /// A use of `text` for a member on a line someone wrote, as a test or a seed states it: a
    /// member's own use rests on their word, another's on the line being said to them.
    pub fn of(
        account: AccountId,
        speaker: AccountId,
        text: &str,
        episode: EpisodeId,
        message: MessageId,
        at: UnixMillis,
    ) -> Self {
        let own = account == speaker;
        Self {
            episode,
            message,
            ordinal: u64::try_from(message.get()).unwrap_or(0),
            speaker,
            text: text.to_owned(),
            referent: Referent::Member(account),
            basis: if own {
                NameBasis::Own
            } else {
                NameBasis::Addressed
            },
            grounds: if own { Grounds::Own } else { Grounds::Marker },
            anchored: false,
            shown_by: Vec::new(),
            at,
        }
    }
}

/// What the uses of one name for one member add up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownName {
    pub account: AccountId,
    /// As most recently written.
    pub text: String,
    pub key: String,
    /// A person set it.
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

/// Uses nobody could place, and the conversations they must come from, before an expression is
/// put to a ruling: fewer are a passing word, and one conversation offers nothing to compare.
pub const OPEN_USES: u32 = 3;
pub const OPEN_EPISODES: u32 = 2;
/// Further unplaced uses after which an expression a ruling could not place is put to one again:
/// each ruling is a model call, and the same uses would be ruled the same.
pub const RULE_AGAIN: u32 = 5;

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

/// What a ruling says an expression is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// A name of this member.
    Member(AccountId),
    /// A name of someone who is not a member.
    Outside,
    /// A general form of address or an ordinary word: nobody's name.
    General,
    /// The uses do not show whose it is.
    Unknown,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Member(_) => "member",
            Verdict::Outside => "outside",
            Verdict::General => "general",
            Verdict::Unknown => "unknown",
        }
    }
}

/// What reading an expression's uses across conversations concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruling {
    /// The expression as written.
    pub text: String,
    pub verdict: Verdict,
    /// The lines the ruling rests on.
    pub lines: Vec<MessageId>,
    pub reason: String,
    /// The unplaced uses the expression had when it was ruled on.
    pub seen: u32,
    pub at: UnixMillis,
}

/// An expression waiting for a ruling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenName {
    pub key: String,
    /// As most recently written.
    pub text: String,
    /// Its uses nobody could place, and the episodes they are from.
    pub unknown: u32,
    pub episodes: u32,
}

/// A statement about a person known only by a name nobody has placed yet. It is held until the
/// name is some member's, and then remembered about them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldClaim {
    /// The name as written.
    pub name: String,
    pub text: String,
    pub durability: Durability,
    pub support: Support,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HeldId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error(transparent)]
    Text(#[from] NameTextError),
    /// A person already set the name for a member not among those given.
    #[error("the name is already set for another member")]
    Taken,
    #[error("storage failure: {0}")]
    Backend(String),
}

impl From<MemoryError> for NameError {
    fn from(error: MemoryError) -> Self {
        NameError::Backend(error.to_string())
    }
}

#[async_trait]
pub trait NameStore: Send + Sync {
    /// Record an episode's references. Not recorded: a second reference for one episode, line
    /// and expression; a use for a member a person removed the name from. A use nobody could
    /// place is recorded as the ruling on its expression places it, when there is one. Returns
    /// how many were recorded.
    async fn record(&self, group: GroupId, references: &[Reference]) -> Result<u64, MemoryError>;

    /// A person sets a name for each of `accounts` (one account, or the accounts of one person in
    /// the group), lifting a removal. Refused when a person already set it for another member.
    async fn set(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<(), NameError>;

    /// A person removes a name from `accounts`: what set it, its uses for them and a ruling that
    /// placed it with them are deleted, and nothing read from chat gives it back. Returns
    /// whether anything was deleted.
    async fn remove(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<bool, NameError>;

    /// The group's names, per member and name; for the given accounts only, or all of them.
    /// Ordered by account, then key. An expression ruled to be nobody's name, or an outsider's,
    /// is a name of no member unless a person set it.
    async fn names(
        &self,
        group: GroupId,
        accounts: Option<&[AccountId]>,
    ) -> Result<Vec<KnownName>, MemoryError>;

    /// Expressions to put to a ruling: at least [`OPEN_USES`] uses nobody could place, from at
    /// least [`OPEN_EPISODES`] episodes, that no person set as a name and no ruling settled (one
    /// that left it unknown counts again after [`RULE_AGAIN`] more such uses). The most used
    /// first, at most `limit`.
    async fn open(&self, group: GroupId, limit: usize) -> Result<Vec<OpenName>, MemoryError>;

    /// Uses of an expression, whoever they name: one per episode (its first line), the latest
    /// episodes first, at most `limit`.
    async fn uses(
        &self,
        group: GroupId,
        key: &str,
        limit: usize,
    ) -> Result<Vec<Reference>, MemoryError>;

    /// Record a ruling on an expression, replacing an earlier one. One that names a member
    /// places every unplaced use of the expression with them; one that names an outsider marks
    /// them so. A ruling for a member a person removed the name from is kept as unknown.
    async fn rule(&self, group: GroupId, ruling: &Ruling) -> Result<(), MemoryError>;

    /// Hold statements about people known only by a name. One per episode, line and statement.
    async fn hold(&self, group: GroupId, claims: &[HeldClaim]) -> Result<(), MemoryError>;

    /// The statements still held, oldest first.
    async fn held(&self, group: GroupId) -> Result<Vec<(HeldId, HeldClaim)>, MemoryError>;

    /// Record that a held statement is now remembered about `account`: it is held no longer.
    async fn file(&self, group: GroupId, id: HeldId, account: AccountId)
    -> Result<(), MemoryError>;
}

/// The references to members, as the index counts them: `(key, reference)`.
pub fn summarize<'a>(
    uses: impl IntoIterator<Item = (&'a str, &'a Reference)>,
    set: &[(AccountId, String, String, UnixMillis)],
    rulings: &[(String, Ruling)],
) -> Vec<KnownName> {
    struct Episode {
        basis: NameBasis,
        anchored: bool,
        grounded: bool,
    }
    struct Seen {
        text: String,
        /// The latest use: when, and where in the chat.
        last: (UnixMillis, u64),
        episodes: BTreeMap<EpisodeId, Episode>,
    }
    let mut seen: BTreeMap<(AccountId, String), Seen> = BTreeMap::new();
    for (key, one) in uses {
        let Referent::Member(account) = one.referent else {
            continue;
        };
        let entry = seen
            .entry((account, key.to_owned()))
            .or_insert_with(|| Seen {
                text: one.text.clone(),
                last: (one.at, one.ordinal),
                episodes: BTreeMap::new(),
            });
        if (one.at, one.ordinal) >= entry.last {
            entry.last = (one.at, one.ordinal);
            entry.text = one.text.clone();
        }
        // The strongest use stands for the episode; what any of its uses rests on is kept.
        let episode = entry.episodes.entry(one.episode).or_insert(Episode {
            basis: one.basis,
            anchored: false,
            grounded: false,
        });
        if one.basis.strength() > episode.basis.strength() {
            episode.basis = one.basis;
        }
        episode.anchored |= one.anchored;
        episode.grounded |= one.anchored || one.grounds.in_the_chat();
    }
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let mut out: BTreeMap<(AccountId, String), KnownName> = seen
        .into_iter()
        .map(|((account, key), s)| {
            let of = |basis: NameBasis| s.episodes.values().filter(move |e| e.basis == basis);
            let others = || s.episodes.values().filter(|e| e.basis != NameBasis::Own);
            let name = KnownName {
                account,
                text: s.text.clone(),
                key: key.clone(),
                set: false,
                ruled: false,
                own: count(of(NameBasis::Own).count()),
                addressed: count(of(NameBasis::Addressed).count()),
                about: count(of(NameBasis::About).count()),
                anchored: count(others().filter(|e| e.anchored).count()),
                grounded: count(others().filter(|e| e.grounded).count()),
                last: s.last.0,
            };
            ((account, key), name)
        })
        .collect();
    let blank = |account: AccountId, key: &str, text: &str, at: UnixMillis| KnownName {
        account,
        text: text.to_owned(),
        key: key.to_owned(),
        set: false,
        ruled: false,
        own: 0,
        addressed: 0,
        about: 0,
        anchored: 0,
        grounded: 0,
        last: at,
    };
    for (key, ruling) in rulings {
        if let Verdict::Member(account) = ruling.verdict {
            out.entry((account, key.clone()))
                .or_insert_with(|| blank(account, key, &ruling.text, ruling.at))
                .ruled = true;
        }
    }
    let nobodys: BTreeSet<&str> = rulings
        .iter()
        .filter(|(_, r)| matches!(r.verdict, Verdict::Outside | Verdict::General))
        .map(|(key, _)| key.as_str())
        .collect();
    out.retain(|(_, key), _| !nobodys.contains(key.as_str()));
    for (account, key, text, at) in set {
        let name = out
            .entry((*account, key.clone()))
            .or_insert_with(|| blank(*account, key, text, *at));
        name.set = true;
        if *at >= name.last {
            name.last = *at;
            name.text = text.clone();
        }
    }
    out.into_values().collect()
}

/// The expressions of `references` (`(key, reference)`) to put to a ruling, given the group's
/// set names and rulings by key.
pub fn open_names<'a>(
    references: impl IntoIterator<Item = (&'a str, &'a Reference)>,
    set: &BTreeSet<String>,
    rulings: &BTreeMap<String, Ruling>,
    limit: usize,
) -> Vec<OpenName> {
    struct Seen {
        text: String,
        last: (UnixMillis, u64),
        unknown: u32,
        episodes: BTreeSet<EpisodeId>,
    }
    let mut seen: BTreeMap<String, Seen> = BTreeMap::new();
    for (key, one) in references {
        if one.referent != Referent::Unknown {
            continue;
        }
        let entry = seen.entry(key.to_owned()).or_insert_with(|| Seen {
            text: one.text.clone(),
            last: (one.at, one.ordinal),
            unknown: 0,
            episodes: BTreeSet::new(),
        });
        entry.unknown += 1;
        entry.episodes.insert(one.episode);
        if (one.at, one.ordinal) >= entry.last {
            entry.last = (one.at, one.ordinal);
            entry.text = one.text.clone();
        }
    }
    let mut open: Vec<OpenName> = seen
        .into_iter()
        .filter(|(key, s)| {
            let waited = match rulings.get(key) {
                None => true,
                Some(ruling) => {
                    ruling.verdict == Verdict::Unknown && s.unknown >= ruling.seen + RULE_AGAIN
                }
            };
            waited
                && !set.contains(key)
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

#[derive(Default)]
struct Inner {
    /// In the order recorded, with each reference's key.
    references: Vec<(GroupId, String, Reference)>,
    set: BTreeMap<(GroupId, AccountId, String), (String, UnixMillis)>,
    removed: BTreeSet<(GroupId, AccountId, String)>,
    rulings: BTreeMap<(GroupId, String), Ruling>,
    held: Vec<(GroupId, HeldId, HeldClaim, Option<AccountId>)>,
    next_held: i64,
}

/// The reference implementation.
#[derive(Default)]
pub struct MemoryNameStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for MemoryNameStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryNameStore").finish_non_exhaustive()
    }
}

impl MemoryNameStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A reference nobody could place, as the ruling on its expression (when there is one) places
/// it; any other reference as it is.
pub fn placed(mut one: Reference, ruling: Option<Verdict>) -> Reference {
    if one.referent != Referent::Unknown {
        return one;
    }
    match ruling {
        Some(Verdict::Member(account)) => {
            one.referent = Referent::Member(account);
            one.grounds = Grounds::Ruling;
            if one.speaker == account {
                one.basis = NameBasis::Own;
            }
        }
        Some(Verdict::Outside) => {
            one.referent = Referent::Outside;
            one.grounds = Grounds::Ruling;
        }
        _ => {}
    }
    one
}

#[async_trait]
impl NameStore for MemoryNameStore {
    async fn record(&self, group: GroupId, references: &[Reference]) -> Result<u64, MemoryError> {
        let mut inner = self.lock();
        let mut recorded = 0;
        for one in references {
            let Ok(key) = name_key(&one.text) else {
                continue;
            };
            if one.basis == NameBasis::Set {
                continue;
            }
            let ruled = inner.rulings.get(&(group, key.clone())).map(|r| r.verdict);
            let one = placed(one.clone(), ruled);
            if let Referent::Member(account) = one.referent
                && inner.removed.contains(&(group, account, key.clone()))
            {
                continue;
            }
            let again = inner.references.iter().any(|(g, k, r)| {
                *g == group && *k == key && r.episode == one.episode && r.message == one.message
            });
            if again {
                continue;
            }
            inner.references.push((
                group,
                key,
                Reference {
                    text: tidy_name(&one.text),
                    ..one
                },
            ));
            recorded += 1;
        }
        Ok(recorded)
    }

    async fn set(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<(), NameError> {
        let key = name_key(text)?;
        let mut inner = self.lock();
        let taken = inner
            .set
            .keys()
            .any(|(g, account, k)| *g == group && *k == key && !accounts.contains(account));
        if taken {
            return Err(NameError::Taken);
        }
        for account in accounts {
            inner.removed.remove(&(group, *account, key.clone()));
            inner
                .set
                .insert((group, *account, key.clone()), (tidy_name(text), at));
        }
        Ok(())
    }

    async fn remove(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        _at: UnixMillis,
    ) -> Result<bool, NameError> {
        let key = name_key(text)?;
        let mut inner = self.lock();
        let mut gone = false;
        for account in accounts {
            gone |= inner.set.remove(&(group, *account, key.clone())).is_some();
            inner.removed.insert((group, *account, key.clone()));
        }
        let before = inner.references.len();
        inner.references.retain(|(g, k, r)| {
            !(*g == group
                && *k == key
                && r.referent.member().is_some_and(|a| accounts.contains(&a)))
        });
        gone |= inner.references.len() < before;
        let ruled = inner
            .rulings
            .get(&(group, key.clone()))
            .is_some_and(|r| matches!(r.verdict, Verdict::Member(a) if accounts.contains(&a)));
        if ruled {
            inner.rulings.remove(&(group, key));
            gone = true;
        }
        Ok(gone)
    }

    async fn names(
        &self,
        group: GroupId,
        accounts: Option<&[AccountId]>,
    ) -> Result<Vec<KnownName>, MemoryError> {
        let inner = self.lock();
        let wanted = |account: &AccountId| accounts.is_none_or(|a| a.contains(account));
        let set: Vec<(AccountId, String, String, UnixMillis)> = inner
            .set
            .iter()
            .filter(|((g, account, _), _)| *g == group && wanted(account))
            .map(|((_, account, key), (text, at))| (*account, key.clone(), text.clone(), *at))
            .collect();
        let rulings: Vec<(String, Ruling)> = inner
            .rulings
            .iter()
            .filter(|((g, _), _)| *g == group)
            .map(|((_, key), ruling)| (key.clone(), ruling.clone()))
            .collect();
        let mut names = summarize(
            inner
                .references
                .iter()
                .filter(|(g, _, _)| *g == group)
                .map(|(_, key, one)| (key.as_str(), one)),
            &set,
            &rulings,
        );
        names.retain(|name| wanted(&name.account));
        Ok(names)
    }

    async fn open(&self, group: GroupId, limit: usize) -> Result<Vec<OpenName>, MemoryError> {
        let inner = self.lock();
        let set: BTreeSet<String> = inner
            .set
            .keys()
            .filter(|(g, _, _)| *g == group)
            .map(|(_, _, key)| key.clone())
            .collect();
        let rulings: BTreeMap<String, Ruling> = inner
            .rulings
            .iter()
            .filter(|((g, _), _)| *g == group)
            .map(|((_, key), ruling)| (key.clone(), ruling.clone()))
            .collect();
        Ok(open_names(
            inner
                .references
                .iter()
                .filter(|(g, _, _)| *g == group)
                .map(|(_, key, one)| (key.as_str(), one)),
            &set,
            &rulings,
            limit,
        ))
    }

    async fn uses(
        &self,
        group: GroupId,
        key: &str,
        limit: usize,
    ) -> Result<Vec<Reference>, MemoryError> {
        let inner = self.lock();
        let mut first: BTreeMap<EpisodeId, &Reference> = BTreeMap::new();
        for (_, _, one) in inner
            .references
            .iter()
            .filter(|(g, k, _)| *g == group && k == key)
        {
            let kept = first.entry(one.episode).or_insert(one);
            if one.ordinal < kept.ordinal {
                *kept = one;
            }
        }
        Ok(first.into_values().rev().take(limit).cloned().collect())
    }

    async fn rule(&self, group: GroupId, ruling: &Ruling) -> Result<(), MemoryError> {
        let Ok(key) = name_key(&ruling.text) else {
            return Ok(());
        };
        let mut inner = self.lock();
        let mut ruling = Ruling {
            text: tidy_name(&ruling.text),
            ..ruling.clone()
        };
        if let Verdict::Member(account) = ruling.verdict
            && inner.removed.contains(&(group, account, key.clone()))
        {
            ruling.verdict = Verdict::Unknown;
        }
        for (g, k, one) in &mut inner.references {
            if *g == group && *k == key {
                *one = placed(one.clone(), Some(ruling.verdict));
            }
        }
        inner.rulings.insert((group, key), ruling);
        Ok(())
    }

    async fn hold(&self, group: GroupId, claims: &[HeldClaim]) -> Result<(), MemoryError> {
        let mut inner = self.lock();
        for claim in claims {
            if name_key(&claim.name).is_err() {
                continue;
            }
            let again = inner.held.iter().any(|(g, _, held, _)| {
                *g == group
                    && held.support.episode == claim.support.episode
                    && held.support.message == claim.support.message
                    && held.text == claim.text
            });
            if again {
                continue;
            }
            inner.next_held += 1;
            let id = HeldId(inner.next_held);
            inner.held.push((
                group,
                id,
                HeldClaim {
                    name: tidy_name(&claim.name),
                    ..claim.clone()
                },
                None,
            ));
        }
        Ok(())
    }

    async fn held(&self, group: GroupId) -> Result<Vec<(HeldId, HeldClaim)>, MemoryError> {
        Ok(self
            .lock()
            .held
            .iter()
            .filter(|(g, _, _, filed)| *g == group && filed.is_none())
            .map(|(_, id, claim, _)| (*id, claim.clone()))
            .collect())
    }

    async fn file(
        &self,
        group: GroupId,
        id: HeldId,
        account: AccountId,
    ) -> Result<(), MemoryError> {
        for (g, held, _, filed) in &mut self.lock().held {
            if *g == group && *held == id {
                *filed = Some(account);
            }
        }
        Ok(())
    }
}
