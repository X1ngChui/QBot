//! Facts: what is believed about a person or the group, with time attached.
//!
//! A fact is a claim `(subject, predicate, value)` plus the distinct episodes that supported it,
//! when it was first and last supported, and when it was last said to hold no longer. Claims of
//! one subject *compete* when they share a slot and differ in predicate or value: another value
//! of a single-valued predicate, the opposite predicate for the same thing, another meaning of the
//! same term. Competing claims stand side by side; which one is believed is their confidence
//! ([`crate::belief`]), computed on read, never a deletion. Only an owner removes a fact
//! (*forget*).

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::belief::{contradicted, credibility, persistence};
use crate::episode::EpisodeId;
use crate::predicates::{Cardinality, DecayClass, Predicate};
use crate::store::MemoryError;

/// Group-level predicates (not in the person table).
pub const GROUP_TERM: &str = "group_term";
pub const GROUP_TOPIC: &str = "group_topic";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FactId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactStatus {
    Active,
    /// An owner struck it out: never shown and no competitor of anything.
    Forgotten,
}

impl FactStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FactStatus::Active => "active",
            FactStatus::Forgotten => "forgotten",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "active" => FactStatus::Active,
            "forgotten" => FactStatus::Forgotten,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub id: FactId,
    pub group: GroupId,
    /// `None`: a fact about the group itself.
    pub subject: Option<AccountId>,
    pub predicate: String,
    /// What the claim is about: claims of a subject sharing a slot compete ([`slot`]).
    pub slot: String,
    /// The value in comparison form ([`normalize_key`]).
    pub key: String,
    pub object: String,
    /// For a group term: the term as written.
    pub label: Option<String>,
    pub status: FactStatus,
    /// Distinct episodes that supported it.
    pub supports: u32,
    pub first_seen: UnixMillis,
    pub last_confirmed: UnixMillis,
    /// The latest time it was said to hold no longer.
    pub ended: Option<UnixMillis>,
    pub decay: DecayClass,
}

impl Fact {
    /// Whether the two are different claims about the same thing.
    pub fn competes_with(&self, other: &Fact) -> bool {
        self.id != other.id
            && self.group == other.group
            && self.subject == other.subject
            && self.slot == other.slot
            && (self.predicate != other.predicate || self.key != other.key)
    }
}

/// One supported observation, from one episode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub group: GroupId,
    pub subject: Option<AccountId>,
    pub predicate: String,
    pub slot: String,
    pub object: String,
    pub label: Option<String>,
    pub decay: DecayClass,
    pub episode: EpisodeId,
    pub message: MessageId,
    pub quote: String,
    pub at: UnixMillis,
}

/// A statement that a fact no longer holds ("I quit that game", "I don't live there any more"):
/// it counts against the claim of the slot with the value it names, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retraction {
    pub group: GroupId,
    pub subject: Option<AccountId>,
    pub predicate: String,
    pub slot: String,
    pub object: String,
    pub at: UnixMillis,
}

/// Comparison form of a value: compatibility-normalized, lower-cased, whitespace collapsed.
pub fn normalize_key(text: &str) -> String {
    let folded: String = text.nfkc().collect::<String>().to_lowercase();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The slot of a person fact. A single-valued predicate has one (its values compete); a
/// multi-valued one has one per value, shared with its opposite (liking and disliking the same
/// thing compete; liking two things does not).
pub fn slot(predicate: &Predicate, object: &str) -> String {
    let family = match &predicate.opposite {
        Some(opposite) if *opposite < predicate.name => format!("{opposite}/{}", predicate.name),
        Some(opposite) => format!("{}/{opposite}", predicate.name),
        None => predicate.name.clone(),
    };
    match predicate.cardinality {
        Cardinality::Single => family,
        Cardinality::Multi => format!("{family}:{}", normalize_key(object)),
    }
}

/// The slot of a group term: one per term, its meanings compete.
pub fn term_slot(term: &str) -> String {
    format!("{GROUP_TERM}:{}", normalize_key(term))
}

/// The slot of the group's topic: one, the topics compete.
pub const TOPIC_SLOT: &str = GROUP_TOPIC;

/// How long claims typically keep holding unsaid, by decay class. Deployment policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecayPolicy {
    pub stable: Duration,
    pub default: Duration,
    pub fast: Duration,
}

impl Default for DecayPolicy {
    fn default() -> Self {
        let days = |n: u64| Duration::from_secs(n * 86_400);
        Self {
            stable: days(730),
            default: days(180),
            fast: days(45),
        }
    }
}

impl DecayPolicy {
    pub fn half_life(&self, class: DecayClass) -> Duration {
        match class {
            DecayClass::Stable => self.stable,
            DecayClass::Default => self.default,
            DecayClass::Fast => self.fast,
        }
    }

    /// The probability that `fact` holds at `now`, given the other claims of its subject
    /// (`others` may include the fact itself and claims that do not compete with it): its
    /// credibility, the chance it has persisted since its last support, and for every later
    /// incompatible statement (a competing claim supported after it, or a statement that it
    /// ended), the chance that statement was wrong.
    pub fn confidence<'a>(
        &self,
        fact: &Fact,
        others: impl IntoIterator<Item = &'a Fact>,
        now: UnixMillis,
    ) -> f64 {
        let mut p = credibility(fact.supports)
            * persistence(self.half_life(fact.decay), fact.last_confirmed, now);
        for rival in others {
            if rival.status == FactStatus::Active
                && fact.competes_with(rival)
                && rival.last_confirmed > fact.last_confirmed
            {
                p = contradicted(p, credibility(rival.supports));
            }
        }
        if fact.ended.is_some_and(|ended| ended > fact.last_confirmed) {
            p = contradicted(p, credibility(1));
        }
        p
    }

    /// The confidence of each of `facts` at `now`, in order, each weighed against the others.
    pub fn confidences(&self, facts: &[Fact], now: UnixMillis) -> Vec<f64> {
        facts
            .iter()
            .map(|fact| self.confidence(fact, facts, now))
            .collect()
    }
}

#[async_trait]
pub trait FactStore: Send + Sync {
    /// Apply one observation: support the subject's active claim with the same slot, predicate
    /// and value (compared normalized), or record a new claim beside its competitors. The same
    /// episode never supports a claim twice.
    async fn observe(&self, observation: &Observation) -> Result<FactId, MemoryError>;

    /// Active facts about a subject (`None`: about the group), by predicate, and within a
    /// predicate the most recently confirmed first (then the newest fact).
    async fn current(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
    ) -> Result<Vec<Fact>, MemoryError>;

    /// Record that the active claim the retraction names (same group, subject, predicate and
    /// slot, the same value once normalized) was said to hold no longer at its time. Returns
    /// whether there was one; a claim that was never recorded is left alone.
    async fn retract(&self, retraction: &Retraction) -> Result<bool, MemoryError>;

    /// Strike out an active fact of the group. Returns whether one was.
    async fn forget(&self, group: GroupId, id: FactId) -> Result<bool, MemoryError>;
}

#[derive(Default)]
struct Inner {
    facts: Vec<Fact>,
    /// (fact, episode) pairs already counted.
    evidence: Vec<(FactId, EpisodeId, MessageId, String)>,
    next_id: i64,
}

/// The reference implementation.
#[derive(Default)]
pub struct MemoryFactStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for MemoryFactStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryFactStore").finish_non_exhaustive()
    }
}

impl MemoryFactStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl FactStore for MemoryFactStore {
    async fn observe(&self, obs: &Observation) -> Result<FactId, MemoryError> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let key = normalize_key(&obs.object);
        let same = inner.facts.iter().position(|f| {
            f.status == FactStatus::Active
                && f.group == obs.group
                && f.subject == obs.subject
                && f.slot == obs.slot
                && f.predicate == obs.predicate
                && normalize_key(&f.object) == key
        });
        let id = match same {
            Some(index) => {
                let id = inner.facts[index].id;
                if !inner
                    .evidence
                    .iter()
                    .any(|(f, e, _, _)| *f == id && *e == obs.episode)
                {
                    let fact = &mut inner.facts[index];
                    fact.supports += 1;
                    fact.last_confirmed = fact.last_confirmed.max(obs.at);
                    inner
                        .evidence
                        .push((id, obs.episode, obs.message, obs.quote.clone()));
                }
                id
            }
            None => {
                inner.next_id += 1;
                let id = FactId(inner.next_id);
                inner.facts.push(Fact {
                    id,
                    group: obs.group,
                    subject: obs.subject,
                    predicate: obs.predicate.clone(),
                    slot: obs.slot.clone(),
                    key,
                    object: obs.object.clone(),
                    label: obs.label.clone(),
                    status: FactStatus::Active,
                    supports: 1,
                    first_seen: obs.at,
                    last_confirmed: obs.at,
                    ended: None,
                    decay: obs.decay,
                });
                inner
                    .evidence
                    .push((id, obs.episode, obs.message, obs.quote.clone()));
                id
            }
        };
        Ok(id)
    }

    async fn current(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
    ) -> Result<Vec<Fact>, MemoryError> {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut found: Vec<Fact> = inner
            .facts
            .iter()
            .filter(|f| f.status == FactStatus::Active && f.group == group && f.subject == subject)
            .cloned()
            .collect();
        found.sort_by(|a, b| {
            a.predicate
                .cmp(&b.predicate)
                .then(b.last_confirmed.cmp(&a.last_confirmed))
                .then(b.id.cmp(&a.id))
        });
        Ok(found)
    }

    async fn retract(&self, r: &Retraction) -> Result<bool, MemoryError> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let wanted = normalize_key(&r.object);
        match inner.facts.iter_mut().find(|f| {
            f.status == FactStatus::Active
                && f.group == r.group
                && f.subject == r.subject
                && f.predicate == r.predicate
                && f.slot == r.slot
                && normalize_key(&f.object) == wanted
        }) {
            Some(fact) => {
                fact.ended = Some(fact.ended.map_or(r.at, |ended| ended.max(r.at)));
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn forget(&self, group: GroupId, id: FactId) -> Result<bool, MemoryError> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match inner
            .facts
            .iter_mut()
            .find(|f| f.id == id && f.group == group && f.status == FactStatus::Active)
        {
            Some(fact) => {
                fact.status = FactStatus::Forgotten;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// Facts grouped by subject, for callers that show several people at once.
pub fn by_subject(facts: Vec<Fact>) -> HashMap<Option<AccountId>, Vec<Fact>> {
    let mut out: HashMap<Option<AccountId>, Vec<Fact>> = HashMap::new();
    for fact in facts {
        out.entry(fact.subject).or_default().push(fact);
    }
    out
}
