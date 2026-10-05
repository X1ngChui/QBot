//! Facts: what is known about a person or the group, with time attached.
//!
//! A fact is `(subject, predicate, value)` plus how often independent episodes supported it and
//! when it was last confirmed. Nothing is deleted when the world changes: a new value for a
//! single-valued predicate *supersedes* the old row (somebody moving is itself information), an
//! opposite predicate supersedes its counterpart, a fact nobody confirms for long enough
//! *expires*, and an owner can *forget* one. Only active facts are shown.
//!
//! Confidence is earned, not asserted: the Wilson lower bound on the supporting evidence (one
//! mention scores about 0.27, three about 0.53, eight about 0.75), multiplied by an exponential
//! decay since the last confirmation with the predicate's half-life.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::episode::EpisodeId;
use crate::predicates::DecayClass;
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
    /// Replaced by a newer value or by its opposite.
    Superseded,
    /// Nothing confirmed it for long enough.
    Expired,
    /// An owner struck it out.
    Forgotten,
}

impl FactStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FactStatus::Active => "active",
            FactStatus::Superseded => "superseded",
            FactStatus::Expired => "expired",
            FactStatus::Forgotten => "forgotten",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "active" => FactStatus::Active,
            "superseded" => FactStatus::Superseded,
            "expired" => FactStatus::Expired,
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
    /// What distinguishes values of the predicate: empty for single-valued ones.
    pub key: String,
    pub object: String,
    /// For a group term: the term as written.
    pub label: Option<String>,
    pub status: FactStatus,
    /// Distinct episodes that supported it.
    pub supports: u32,
    pub first_seen: UnixMillis,
    pub last_confirmed: UnixMillis,
    pub ended: Option<UnixMillis>,
    pub decay: DecayClass,
}

/// One supported observation, from one episode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub group: GroupId,
    pub subject: Option<AccountId>,
    pub predicate: String,
    pub key: String,
    pub object: String,
    pub label: Option<String>,
    /// The predicate this one retires for the same key.
    pub opposite: Option<String>,
    pub decay: DecayClass,
    pub episode: EpisodeId,
    pub message: MessageId,
    pub quote: String,
    pub at: UnixMillis,
}

/// Comparison form of a value: compatibility-normalized, lower-cased, whitespace collapsed.
pub fn normalize_key(text: &str) -> String {
    let folded: String = text.nfkc().collect::<String>().to_lowercase();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The one-sided 95% Wilson lower bound on a proportion of `supports` out of `supports` trials.
pub fn wilson(supports: u32) -> f64 {
    if supports == 0 {
        return 0.0;
    }
    let n = f64::from(supports);
    let z = 1.6449_f64;
    let z2 = z * z;
    let centre = 1.0 + z2 / (2.0 * n);
    let margin = z * (z2 / (4.0 * n * n)).sqrt();
    ((centre - margin) / (1.0 + z2 / n)).max(0.0)
}

/// How facts age. The half-lives and the floor are deployment policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecayPolicy {
    pub stable: Duration,
    pub default: Duration,
    pub fast: Duration,
    /// A fact whose current confidence falls below this is expired.
    pub forget_below: f64,
}

impl Default for DecayPolicy {
    fn default() -> Self {
        let days = |n: u64| Duration::from_secs(n * 86_400);
        Self {
            stable: days(90),
            default: days(30),
            fast: days(14),
            forget_below: 0.05,
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

    /// The fact's confidence now: earned confidence, halved every half-life since it was last
    /// confirmed.
    pub fn confidence(&self, fact: &Fact, now: UnixMillis) -> f64 {
        let age = now.since(fact.last_confirmed).as_secs_f64();
        let half = self.half_life(fact.decay).as_secs_f64().max(1.0);
        wilson(fact.supports) * 0.5_f64.powf(age / half)
    }
}

#[async_trait]
pub trait FactStore: Send + Sync {
    /// Apply one observation: support the matching active fact, or record a new one (superseding
    /// a different active value of the same predicate and key, and the opposite predicate's fact
    /// for the same key). The same episode never supports a fact twice.
    async fn observe(&self, observation: &Observation) -> Result<FactId, MemoryError>;

    /// Active facts about a subject (`None`: about the group), ordered by predicate then key.
    async fn current(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
    ) -> Result<Vec<Fact>, MemoryError>;

    /// Strike out an active fact of the group. Returns whether one was.
    async fn forget(&self, group: GroupId, id: FactId) -> Result<bool, MemoryError>;

    /// Every active fact, of every group (for decay).
    async fn all_active(&self) -> Result<Vec<Fact>, MemoryError>;

    /// Mark active facts expired as of `at`. Returns how many.
    async fn expire(&self, ids: &[FactId], at: UnixMillis) -> Result<usize, MemoryError>;
}

/// Expire every active fact whose confidence has fallen below the policy's floor.
pub async fn decay(
    store: &dyn FactStore,
    policy: &DecayPolicy,
    now: UnixMillis,
) -> Result<usize, MemoryError> {
    let stale: Vec<FactId> = store
        .all_active()
        .await?
        .into_iter()
        .filter(|fact| policy.confidence(fact, now) < policy.forget_below)
        .map(|fact| fact.id)
        .collect();
    if stale.is_empty() {
        Ok(0)
    } else {
        store.expire(&stale, now).await
    }
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
        let same_slot = |f: &Fact, predicate: &str| {
            f.status == FactStatus::Active
                && f.group == obs.group
                && f.subject == obs.subject
                && f.predicate == predicate
                && f.key == obs.key
        };
        if let Some(opposite) = &obs.opposite {
            for fact in inner.facts.iter_mut().filter(|f| same_slot(f, opposite)) {
                fact.status = FactStatus::Superseded;
                fact.ended = Some(obs.at);
            }
        }
        let wanted = normalize_key(&obs.object);
        let current = inner
            .facts
            .iter()
            .position(|f| same_slot(f, &obs.predicate));
        let id = match current {
            Some(index) if normalize_key(&inner.facts[index].object) == wanted => {
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
            other => {
                if let Some(index) = other {
                    inner.facts[index].status = FactStatus::Superseded;
                    inner.facts[index].ended = Some(obs.at);
                }
                inner.next_id += 1;
                let id = FactId(inner.next_id);
                inner.facts.push(Fact {
                    id,
                    group: obs.group,
                    subject: obs.subject,
                    predicate: obs.predicate.clone(),
                    key: obs.key.clone(),
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
        found.sort_by(|a, b| (&a.predicate, &a.key).cmp(&(&b.predicate, &b.key)));
        Ok(found)
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

    async fn all_active(&self) -> Result<Vec<Fact>, MemoryError> {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(inner
            .facts
            .iter()
            .filter(|f| f.status == FactStatus::Active)
            .cloned()
            .collect())
    }

    async fn expire(&self, ids: &[FactId], at: UnixMillis) -> Result<usize, MemoryError> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut count = 0;
        for fact in inner
            .facts
            .iter_mut()
            .filter(|f| f.status == FactStatus::Active && ids.contains(&f.id))
        {
            fact.status = FactStatus::Expired;
            fact.ended = Some(at);
            count += 1;
        }
        Ok(count)
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
