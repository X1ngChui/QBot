//! Applying what an episode found: names become extracted identity evidence, facts and group
//! knowledge become observations. Idempotent: an episode supports a name or a fact at most once,
//! and one episode says one thing per slot (its last statement), so applying the same episode
//! again changes nothing, and a crash between storing an episode and applying it loses nothing
//! (the episode stays unconsolidated until applied).
//!
//! Room follows the conversation: after an episode is applied, each scope it touched (a member's
//! facts, the group's terms, its topics) that is over its capacity lets its claims of least value
//! go, weighed as of the episode's end. Nothing else ages memory; nothing runs on a schedule.
//! Weighing as of the episode makes it deterministic, so applying the same episode again evicts
//! nothing more, and what the episode itself supported is never evicted by it.

use std::collections::BTreeSet;
use std::sync::Arc;

use qbot_core::AccountId;

use crate::episode::Episode;
use crate::facts::{
    Capacity, DecayPolicy, FactStore, GROUP_TERM, GROUP_TOPIC, Observation, Retraction, slot,
    term_slot, topic_slot,
};
use crate::findings::KnowledgeFinding;
use crate::identity::{EvidenceKind, EvidenceRecord, IdentityError};
use crate::identity_store::IdentityStore;
use crate::predicates::{DecayClass, Predicates};
use crate::store::{EpisodeStore, MemoryError};

pub struct Consolidator {
    facts: Arc<dyn FactStore>,
    identity: Arc<dyn IdentityStore>,
    predicates: Arc<Predicates>,
    room: Option<Room>,
}

/// What capacity management needs: the group's activity, how claims age, and the room.
struct Room {
    episodes: Arc<dyn EpisodeStore>,
    decay: DecayPolicy,
    capacity: Capacity,
}

impl std::fmt::Debug for Consolidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Consolidator").finish_non_exhaustive()
    }
}

impl Consolidator {
    pub fn new(
        facts: Arc<dyn FactStore>,
        identity: Arc<dyn IdentityStore>,
        predicates: Arc<Predicates>,
    ) -> Self {
        Self {
            facts,
            identity,
            predicates,
            room: None,
        }
    }

    /// Keep each scope within `capacity` as episodes are applied, weighing claims by `decay` on
    /// the activity of `episodes`. Without it, claims accumulate.
    pub fn with_capacity(
        mut self,
        episodes: Arc<dyn EpisodeStore>,
        decay: DecayPolicy,
        capacity: Capacity,
    ) -> Self {
        self.room = Some(Room {
            episodes,
            decay,
            capacity,
        });
        self
    }

    pub async fn apply(&self, episode: &Episode) -> Result<(), MemoryError> {
        let source = &episode.episode;
        let (group, at) = (source.group, source.ended);
        for name in &source.findings.names {
            let evidence = EvidenceRecord {
                kind: EvidenceKind::Extracted,
                support: Some(episode.id.get()),
            };
            match self
                .identity
                .add_evidence(group, &name.name, name.account, evidence, at)
                .await
            {
                Ok(_) => {}
                // A name the identity rules refuse (too long, empty after normalizing) is not
                // evidence of anything; it was already checked, so this is rare and harmless.
                Err(IdentityError::Name(error)) => {
                    tracing::info!(%error, "an extracted name was not recorded")
                }
                Err(error) => return Err(MemoryError::Backend(error.to_string())),
            }
        }
        let mut observations = Vec::new();
        for fact in &source.findings.facts {
            // The table may have changed since extraction; a predicate it no longer has is skipped.
            let Some(predicate) = self.predicates.get(&fact.predicate) else {
                tracing::info!(predicate = %fact.predicate, "a fact for an unknown predicate was skipped");
                continue;
            };
            observations.push(Observation {
                group,
                subject: Some(fact.account),
                predicate: fact.predicate.clone(),
                slot: slot(predicate, &fact.object),
                object: fact.object.clone(),
                label: None,
                decay: predicate.decay,
                episode: episode.id,
                message: fact.message,
                quote: fact.quote.clone(),
                at,
            });
        }
        for item in &source.findings.knowledge {
            let observation = match item {
                // A term's meanings compete.
                KnowledgeFinding::Term {
                    term,
                    meaning,
                    message,
                    quote,
                } => Observation {
                    group,
                    subject: None,
                    predicate: GROUP_TERM.into(),
                    slot: term_slot(term),
                    object: meaning.clone(),
                    label: Some(term.clone()),
                    decay: DecayClass::Default,
                    episode: episode.id,
                    message: *message,
                    quote: quote.clone(),
                    at,
                },
                // A topic stands beside the others; they compete only for room.
                KnowledgeFinding::Topic {
                    topic,
                    message,
                    quote,
                } => Observation {
                    group,
                    subject: None,
                    predicate: GROUP_TOPIC.into(),
                    slot: topic_slot(topic),
                    object: topic.clone(),
                    label: None,
                    decay: DecayClass::Stable,
                    episode: episode.id,
                    message: *message,
                    quote: quote.clone(),
                    at,
                },
            };
            observations.push(observation);
        }
        // The scopes the episode touched, and the claims it supported there.
        let mut touched: BTreeSet<Option<AccountId>> = BTreeSet::new();
        let mut said = Vec::new();
        for observation in last_per_slot(observations) {
            touched.insert(observation.subject);
            said.push(self.facts.observe(&observation).await?);
        }
        // After what the episode affirms: within one stretch of chat, "I don't play it any more"
        // comes after the playing it ends. Retracting what is not recorded changes nothing, so
        // applying the episode again changes nothing either.
        for fact in &source.findings.ended {
            let Some(predicate) = self.predicates.get(&fact.predicate) else {
                continue;
            };
            let retraction = Retraction {
                group,
                subject: Some(fact.account),
                predicate: fact.predicate.clone(),
                slot: slot(predicate, &fact.object),
                object: fact.object.clone(),
                at,
            };
            if self.facts.retract(&retraction).await? {
                tracing::info!(group = group.get(), predicate = %fact.predicate, "a fact was said to hold no longer");
            }
        }
        if let Some(room) = &self.room {
            let activity = room.episodes.activity(group).await?;
            for subject in touched {
                let claims = self.facts.current(group, subject).await?;
                let values: Vec<f64> = room
                    .decay
                    .assess(&claims, &activity, at)
                    .into_iter()
                    .map(|(_, value)| value)
                    .collect();
                let evict = room.capacity.evictions(&claims, &values, &said);
                if !evict.is_empty() {
                    let evicted = self.facts.evict(group, &evict).await?;
                    tracing::info!(
                        group = group.get(),
                        evicted,
                        "claims of least value made room"
                    );
                }
            }
        }
        Ok(())
    }
}

/// One observation per slot of a subject, the last one made, in the order they were made: within
/// one slice, a later statement replaces an earlier one about the same thing, and applying both
/// would leave a claim the slice itself took back standing beside its replacement, as recent as it.
fn last_per_slot(observations: Vec<Observation>) -> Vec<Observation> {
    let slot = |o: &Observation| (o.subject, o.slot.clone());
    let mut kept: Vec<Observation> = Vec::new();
    for observation in observations {
        kept.retain(|earlier| slot(earlier) != slot(&observation));
        kept.push(observation);
    }
    kept
}
