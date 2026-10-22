//! Facts: what is believed about a person or the group, with time attached.
//!
//! A fact is a claim `(subject, predicate, value)` plus the distinct episodes that supported it,
//! when it was first and last supported, and when it was last said to hold no longer. Claims of
//! one subject *compete* when they share a slot and differ in predicate or value: another value
//! of a single-valued predicate, the opposite predicate for the same thing, another meaning of the
//! same term. Competing claims stand side by side; which one is believed is their confidence
//! ([`crate::belief`]), computed on read, never a deletion. An owner removes a fact (*forget*);
//! otherwise a claim is deleted only to make room, when an episode leaves its scope over its
//! [`Capacity`] and it is worth least there ([`DecayPolicy::value`]).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::belief::{Activity, contradicted, credibility, persistence, salience};
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

/// The slot of one of the group's topics: one per topic, so topics stand side by side (a group
/// talking about exams has not stopped being about games) and compete only for room.
pub fn topic_slot(topic: &str) -> String {
    format!("{GROUP_TOPIC}:{}", normalize_key(topic))
}

/// How claims age, by decay class: how long they typically keep holding unsaid (wall-clock
/// half-lives, for confidence) and how many episodes their subject can take part in without
/// restating them before they are half as current (for salience). Deployment policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecayPolicy {
    pub stable: Duration,
    pub default: Duration,
    pub fast: Duration,
    pub activity: EpisodeHalfLives,
}

/// Half-lives counted in episodes, by decay class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EpisodeHalfLives {
    pub stable: f64,
    pub default: f64,
    pub fast: f64,
}

impl Default for DecayPolicy {
    fn default() -> Self {
        let days = |n: u64| Duration::from_secs(n * 86_400);
        Self {
            stable: days(730),
            default: days(180),
            fast: days(45),
            activity: EpisodeHalfLives {
                stable: 200.0,
                default: 40.0,
                fast: 10.0,
            },
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

    pub fn activity_half_life(&self, class: DecayClass) -> f64 {
        match class {
            DecayClass::Stable => self.activity.stable,
            DecayClass::Default => self.activity.default,
            DecayClass::Fast => self.activity.fast,
        }
    }

    /// What a claim is worth where room is limited (a prompt budget, a capacity): the chance it
    /// holds, times how current it still is in the conversation. Not shown as confidence.
    pub fn value(&self, fact: &Fact, confidence: f64, activity: &Activity, now: UnixMillis) -> f64 {
        let passed = activity.since(fact.subject, fact.last_confirmed, now);
        confidence * salience(self.activity_half_life(fact.decay), passed)
    }

    /// The confidence and value of each of `facts` at `now`, in order.
    pub fn assess(&self, facts: &[Fact], activity: &Activity, now: UnixMillis) -> Vec<(f64, f64)> {
        facts
            .iter()
            .map(|fact| {
                let confidence = self.confidence(fact, facts, now);
                (confidence, self.value(fact, confidence, activity, now))
            })
            .collect()
    }
}

/// The room a group gives learned claims: how many active claims each scope keeps. Past it, the
/// claims of least value make way (`Capacity::evictions`). Deployment policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    /// Per member of a group.
    pub person_facts: usize,
    pub group_terms: usize,
    pub group_topics: usize,
}

impl Default for Capacity {
    fn default() -> Self {
        Self {
            person_facts: 40,
            group_terms: 120,
            group_topics: 8,
        }
    }
}

/// The room a claim takes: its subject's, or the group's terms or topics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    Person(AccountId),
    Terms,
    Topics,
}

impl Capacity {
    fn room(&self, scope: Scope) -> usize {
        match scope {
            Scope::Person(_) => self.person_facts,
            Scope::Terms => self.group_terms,
            Scope::Topics => self.group_topics,
        }
    }

    /// The claims to evict so that every scope among `facts` (active claims of one group, with
    /// their values in order) fits its room: in each scope over it, those of least value (then
    /// the longer unsupported, then the older), never one of `keep` (what was just said, so new
    /// information always finds room).
    pub fn evictions(&self, facts: &[Fact], values: &[f64], keep: &[FactId]) -> Vec<FactId> {
        let mut scopes: BTreeMap<Scope, Vec<(&Fact, f64)>> = BTreeMap::new();
        for (fact, value) in facts.iter().zip(values) {
            let scope = match (fact.subject, fact.predicate.as_str()) {
                (Some(account), _) => Scope::Person(account),
                (None, GROUP_TERM) => Scope::Terms,
                (None, GROUP_TOPIC) => Scope::Topics,
                (None, _) => continue,
            };
            if fact.status == FactStatus::Active {
                scopes.entry(scope).or_default().push((fact, *value));
            }
        }
        let mut out = Vec::new();
        for (scope, mut claims) in scopes {
            let over = claims.len().saturating_sub(self.room(scope));
            claims.sort_by(|(a, p), (b, q)| {
                p.total_cmp(q)
                    .then(a.last_confirmed.cmp(&b.last_confirmed))
                    .then(a.id.cmp(&b.id))
            });
            out.extend(
                claims
                    .iter()
                    .filter(|(f, _)| !keep.contains(&f.id))
                    .take(over)
                    .map(|(f, _)| f.id),
            );
        }
        out.sort();
        out
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

    /// Delete active claims of the group to make room, with their evidence (the episodes that
    /// supported them keep what was said). Returns how many.
    async fn evict(&self, group: GroupId, ids: &[FactId]) -> Result<usize, MemoryError>;
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

    async fn evict(&self, group: GroupId, ids: &[FactId]) -> Result<usize, MemoryError> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let before = inner.facts.len();
        inner.facts.retain(|f| {
            !(f.group == group && f.status == FactStatus::Active && ids.contains(&f.id))
        });
        let gone = before - inner.facts.len();
        let alive: std::collections::HashSet<FactId> = inner.facts.iter().map(|f| f.id).collect();
        inner.evidence.retain(|(f, _, _, _)| alive.contains(f));
        Ok(gone)
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
