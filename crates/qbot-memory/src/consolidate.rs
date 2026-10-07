//! Applying what an episode found: names become extracted identity evidence, facts and group
//! knowledge become observations. Idempotent: an episode supports a name or a fact at most once,
//! and one episode says one thing per slot (its last statement), so applying the same episode
//! again changes nothing, and a crash between storing an episode and applying it loses nothing
//! (the episode stays unconsolidated until applied).

use std::sync::Arc;

use crate::episode::Episode;
use crate::facts::{FactStore, GROUP_TERM, GROUP_TOPIC, Observation, normalize_key};
use crate::findings::KnowledgeFinding;
use crate::identity::{AliasTarget, EvidenceKind, EvidenceRecord, IdentityError};
use crate::identity_store::IdentityStore;
use crate::predicates::{Cardinality, DecayClass, Predicates};
use crate::store::MemoryError;

pub struct Consolidator {
    facts: Arc<dyn FactStore>,
    identity: Arc<dyn IdentityStore>,
    predicates: Arc<Predicates>,
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
        }
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
                .add_evidence(
                    group,
                    &name.name,
                    AliasTarget::Account(name.account),
                    evidence,
                    at,
                )
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
            let key = match predicate.cardinality {
                Cardinality::Single => String::new(),
                Cardinality::Multi => normalize_key(&fact.object),
            };
            observations.push(Observation {
                group,
                subject: Some(fact.account),
                predicate: fact.predicate.clone(),
                key,
                object: fact.object.clone(),
                label: None,
                opposite: predicate.opposite.clone(),
                decay: predicate.decay,
                episode: episode.id,
                message: fact.message,
                quote: fact.quote.clone(),
                at,
            });
        }
        for item in &source.findings.knowledge {
            let observation = match item {
                // A term keeps one meaning at a time; a new meaning replaces the old one.
                KnowledgeFinding::Term {
                    term,
                    meaning,
                    message,
                    quote,
                } => Observation {
                    group,
                    subject: None,
                    predicate: GROUP_TERM.into(),
                    key: normalize_key(term),
                    object: meaning.clone(),
                    label: Some(term.clone()),
                    opposite: None,
                    decay: DecayClass::Default,
                    episode: episode.id,
                    message: *message,
                    quote: quote.clone(),
                    at,
                },
                // A group has one topic at a time.
                KnowledgeFinding::Topic {
                    topic,
                    message,
                    quote,
                } => Observation {
                    group,
                    subject: None,
                    predicate: GROUP_TOPIC.into(),
                    key: String::new(),
                    object: topic.clone(),
                    label: None,
                    opposite: None,
                    decay: DecayClass::Stable,
                    episode: episode.id,
                    message: *message,
                    quote: quote.clone(),
                    at,
                },
            };
            observations.push(observation);
        }
        for observation in last_per_slot(observations) {
            self.facts.observe(&observation).await?;
        }
        Ok(())
    }
}

/// One observation per slot, the last one made, in the order they were made. A slot is a
/// subject's predicate and key, with a predicate and its opposite sharing one: within one slice,
/// a later statement replaces an earlier one about the same thing, and applying both would leave
/// a superseded row that never held and make a second application differ from the first.
fn last_per_slot(observations: Vec<Observation>) -> Vec<Observation> {
    let slot = |o: &Observation| {
        let pair = match &o.opposite {
            Some(opposite) if *opposite < o.predicate => opposite.clone(),
            _ => o.predicate.clone(),
        };
        (o.subject, pair, o.key.clone())
    };
    let mut kept: Vec<Observation> = Vec::new();
    for observation in observations {
        kept.retain(|earlier| slot(earlier) != slot(&observation));
        kept.push(observation);
    }
    kept
}
