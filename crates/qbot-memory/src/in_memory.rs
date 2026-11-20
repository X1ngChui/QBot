//! The reference implementation of both memory ports: it defines the semantics durable stores
//! must reproduce (`qbot_memory::conformance` runs against both).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::episode::{Episode, EpisodeId, Hit, NewEpisode};
use crate::memories::{
    About, Commit, Committed, Durability, HandError, HandMemory, Kind, Memory, MemoryId,
    MemoryState, MemoryStore, Obs, Observation, ObservationId, Op, Origin, Suppression, Target,
    memory_key, suppression_of,
};
use crate::names::name_key;
use crate::slice::SliceLine;
use crate::store::{EpisodeStore, MemoryError, WindowState, cosine_distance};
use crate::worth::Activity;

/// A memory as kept: without its support, which is the observations attached to it.
#[derive(Debug, Clone)]
struct Kept {
    memory: Memory,
}

#[derive(Default)]
struct Inner {
    lines: Vec<(GroupId, SliceLine)>,
    episodes: Vec<(Episode, String, Vec<f32>)>,
    windows: HashMap<GroupId, WindowState>,
    skipped: Vec<(GroupId, u64, u64)>,
    next_episode: i64,
    memories: BTreeMap<MemoryId, Kept>,
    next_memory: i64,
    /// Each with its group and the memory it supports (`None`: pending).
    observations: BTreeMap<ObservationId, (GroupId, Observation, Option<MemoryId>)>,
    next_observation: i64,
    suppressions: Vec<(GroupId, Suppression)>,
}

impl Inner {
    /// Whether any line of `first..=last` belongs to an episode or skipped stretch already.
    fn taken(&self, group: GroupId, first: u64, last: u64) -> bool {
        let overlaps = |(a, b): (u64, u64)| a <= last && first <= b;
        self.episodes.iter().any(|(e, _, _)| {
            e.episode.group == group && overlaps((e.episode.first_ordinal, e.episode.last_ordinal))
        }) || self
            .skipped
            .iter()
            .any(|(g, a, b)| *g == group && overlaps((*a, *b)))
    }

    fn insert_episode(&mut self, episode: &NewEpisode, model: &str, vector: &[f32]) -> Episode {
        self.next_episode += 1;
        let stored = Episode {
            id: EpisodeId::new(self.next_episode),
            created: UnixMillis::new(0),
            episode: episode.clone(),
        };
        self.episodes
            .push((stored.clone(), model.to_owned(), vector.to_vec()));
        stored
    }

    /// A memory with its support.
    fn read(&self, id: MemoryId) -> Option<Memory> {
        let kept = self.memories.get(&id)?;
        let mut memory = kept.memory.clone();
        memory.support = self
            .observations
            .values()
            .filter(|(_, _, m)| *m == Some(id))
            .map(|(_, o, _)| o.clone())
            .collect();
        memory
            .support
            .sort_by_key(|o| (o.at, o.ordinal, o.id.get()));
        Some(memory)
    }

    fn current_of(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
        kind: Kind,
        key: &str,
    ) -> Option<MemoryId> {
        self.memories
            .values()
            .map(|k| &k.memory)
            .find(|m| {
                m.group == group
                    && m.subject == subject
                    && m.kind == kind
                    && m.is_current()
                    && m.key() == key
            })
            .map(|m| m.id)
    }

    fn create(&mut self, group: GroupId, memory: Memory) -> MemoryId {
        self.next_memory += 1;
        let id = MemoryId::new(self.next_memory);
        self.memories.insert(
            id,
            Kept {
                memory: Memory {
                    id,
                    group,
                    ..memory
                },
            },
        );
        id
    }

    /// Attach an observation to a memory, which then reaches back to it.
    fn attach(&mut self, observation: ObservationId, memory: MemoryId) {
        let Some(at) = self.observations.get(&observation).map(|(_, o, _)| o.at) else {
            return;
        };
        let subject = self.memories.get(&memory).map(|k| k.memory.subject);
        if let Some((_, o, m)) = self.observations.get_mut(&observation) {
            *m = Some(memory);
            o.about = match subject {
                Some(Some(account)) => About::Member(account),
                Some(None) => About::Group,
                None => o.about.clone(),
            };
        }
        if let Some(kept) = self.memories.get_mut(&memory) {
            kept.memory.first = kept.memory.first.min(at);
            kept.memory.last = kept.memory.last.max(at);
        }
    }

    /// Delete a memory and what supports it; whatever it replaced points nowhere.
    fn delete(&mut self, id: MemoryId) -> bool {
        if self.memories.remove(&id).is_none() {
            return false;
        }
        self.observations.retain(|_, (_, _, m)| *m != Some(id));
        for kept in self.memories.values_mut() {
            if let MemoryState::Replaced { by, .. } = &mut kept.memory.state
                && *by == Some(id)
            {
                *by = None;
            }
        }
        true
    }

    fn lift(&mut self, group: GroupId, subject: Option<AccountId>, kind: Kind, key: &str) {
        self.suppressions.retain(|(g, s)| {
            !(*g == group && s.subject == subject && s.kind == kind && s.key == key)
        });
    }

    fn is_current(&self, id: MemoryId) -> bool {
        self.memories
            .get(&id)
            .is_some_and(|k| k.memory.is_current())
    }
}

/// Reference implementation of [`EpisodeStore`] and [`MemoryStore`] in one: a batch's commit
/// spans both.
#[derive(Default)]
pub struct InMemoryStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for InMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryStore").finish_non_exhaustive()
    }
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add archive lines (the real archive is a separate table).
    pub fn add_lines(&self, group: GroupId, lines: impl IntoIterator<Item = SliceLine>) {
        self.lock()
            .lines
            .extend(lines.into_iter().map(|l| (group, l)));
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl EpisodeStore for InMemoryStore {
    async fn groups(&self) -> Result<Vec<GroupId>, MemoryError> {
        let mut groups: Vec<GroupId> = self.lock().lines.iter().map(|(g, _)| *g).collect();
        groups.sort();
        groups.dedup();
        Ok(groups)
    }

    async fn window(&self, group: GroupId) -> Result<Option<WindowState>, MemoryError> {
        Ok(self.lock().windows.get(&group).copied())
    }

    async fn cut(&self, group: GroupId, ordinal: u64, at: UnixMillis) -> Result<(), MemoryError> {
        let mut inner = self.lock();
        let state = inner.windows.entry(group).or_insert(WindowState {
            batched: at,
            cut: 0,
        });
        state.cut = state.cut.max(ordinal);
        Ok(())
    }

    async fn before(
        &self,
        group: GroupId,
        ordinal: u64,
        limit: usize,
    ) -> Result<Vec<Episode>, MemoryError> {
        let mut found: Vec<Episode> = self
            .lock()
            .episodes
            .iter()
            .filter(|(e, _, _)| e.episode.group == group && e.episode.last_ordinal < ordinal)
            .map(|(e, _, _)| e.clone())
            .collect();
        found.sort_by_key(|e| e.episode.first_ordinal);
        let skip = found.len().saturating_sub(limit);
        Ok(found.split_off(skip))
    }

    async fn lines(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<SliceLine>, MemoryError> {
        let mut found: Vec<SliceLine> = self
            .lock()
            .lines
            .iter()
            .filter(|(g, l)| *g == group && (first_ordinal..=last_ordinal).contains(&l.ordinal))
            .map(|(_, l)| l.clone())
            .collect();
        found.sort_by_key(|l| l.ordinal);
        Ok(found)
    }

    async fn quoted(
        &self,
        group: GroupId,
        messages: &[MessageId],
        before: u64,
    ) -> Result<Vec<SliceLine>, MemoryError> {
        let inner = self.lock();
        let mut found: Vec<SliceLine> = messages
            .iter()
            .filter_map(|message| {
                inner
                    .lines
                    .iter()
                    .filter(|(g, l)| *g == group && l.message == *message && l.ordinal < before)
                    .max_by_key(|(_, l)| l.ordinal)
                    .map(|(_, l)| l.clone())
            })
            .collect();
        found.sort_by_key(|l| l.ordinal);
        found.dedup_by_key(|l| l.ordinal);
        Ok(found)
    }

    async fn last_ordinal(&self, group: GroupId) -> Result<u64, MemoryError> {
        Ok(self
            .lock()
            .lines
            .iter()
            .filter(|(g, _)| *g == group)
            .map(|(_, l)| l.ordinal)
            .max()
            .unwrap_or(0))
    }

    async fn covered_through(&self, group: GroupId) -> Result<u64, MemoryError> {
        let inner = self.lock();
        let episodes = inner
            .episodes
            .iter()
            .filter(|(e, _, _)| e.episode.group == group)
            .map(|(e, _, _)| e.episode.last_ordinal);
        let skipped = inner
            .skipped
            .iter()
            .filter(|(g, _, _)| *g == group)
            .map(|(_, _, last)| *last);
        Ok(episodes.chain(skipped).max().unwrap_or(0))
    }

    async fn insert(
        &self,
        episode: &NewEpisode,
        vector: &[f32],
        embed_model: &str,
    ) -> Result<Episode, MemoryError> {
        let mut inner = self.lock();
        if episode.first_ordinal > episode.last_ordinal {
            return Err(MemoryError::InvalidRange);
        }
        if inner.taken(episode.group, episode.first_ordinal, episode.last_ordinal) {
            return Err(MemoryError::Overlap);
        }
        Ok(inner.insert_episode(episode, embed_model, vector))
    }

    async fn get(&self, group: GroupId, id: EpisodeId) -> Result<Option<Episode>, MemoryError> {
        Ok(self
            .lock()
            .episodes
            .iter()
            .find(|(e, _, _)| e.id == id && e.episode.group == group)
            .map(|(e, _, _)| e.clone()))
    }

    async fn activity(&self, group: GroupId) -> Result<Activity, MemoryError> {
        Ok(Activity::new(
            self.lock()
                .episodes
                .iter()
                .filter(|(e, _, _)| e.episode.group == group)
                .map(|(e, _, _)| (e.episode.ended, e.episode.participants.clone()))
                .collect(),
        ))
    }

    async fn within(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<Episode>, MemoryError> {
        let mut found: Vec<Episode> = self
            .lock()
            .episodes
            .iter()
            .filter(|(e, _, _)| {
                e.episode.group == group
                    && e.episode.first_ordinal >= first_ordinal
                    && e.episode.last_ordinal <= last_ordinal
            })
            .map(|(e, _, _)| e.clone())
            .collect();
        found.sort_by_key(|e| e.episode.first_ordinal);
        Ok(found)
    }

    async fn ending_within(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<Episode>, MemoryError> {
        let mut found: Vec<Episode> = self
            .lock()
            .episodes
            .iter()
            .filter(|(e, _, _)| {
                e.episode.group == group
                    && (first_ordinal..=last_ordinal).contains(&e.episode.last_ordinal)
            })
            .map(|(e, _, _)| e.clone())
            .collect();
        found.sort_by_key(|e| e.episode.first_ordinal);
        Ok(found)
    }

    async fn unembedded(
        &self,
        group: GroupId,
        embed_model: &str,
        dims: usize,
        limit: usize,
    ) -> Result<Vec<Episode>, MemoryError> {
        let mut found: Vec<Episode> = self
            .lock()
            .episodes
            .iter()
            .filter(|(e, model, v)| {
                e.episode.group == group && (model != embed_model || v.len() != dims)
            })
            .map(|(e, _, _)| e.clone())
            .collect();
        found.sort_by_key(|e| e.id);
        found.truncate(limit);
        Ok(found)
    }

    async fn set_embedding(
        &self,
        id: EpisodeId,
        embed_model: &str,
        vector: &[f32],
    ) -> Result<(), MemoryError> {
        let mut inner = self.lock();
        let entry = inner
            .episodes
            .iter_mut()
            .find(|(e, _, _)| e.id == id)
            .ok_or(MemoryError::NotFound)?;
        entry.1 = embed_model.to_owned();
        entry.2 = vector.to_vec();
        Ok(())
    }

    async fn search(
        &self,
        group: GroupId,
        embed_model: &str,
        query: &[f32],
        max_distance: f32,
    ) -> Result<Vec<Hit>, MemoryError> {
        let mut hits: Vec<Hit> = self
            .lock()
            .episodes
            .iter()
            .filter(|(e, model, v)| {
                e.episode.group == group && model == embed_model && v.len() == query.len()
            })
            .map(|(e, _, v)| Hit {
                episode: e.clone(),
                distance: cosine_distance(query, v),
            })
            .filter(|h| h.distance <= max_distance)
            .collect();
        hits.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then(a.episode.id.cmp(&b.episode.id))
        });
        Ok(hits)
    }
}

/// Whether a subject's changes still apply: everything they name exists as they expect, and
/// nothing about the subject was forgotten since the batch read it.
fn applies(inner: &Inner, commit: &Commit, subject: Option<AccountId>, ops: &[Op]) -> bool {
    let overtaken = inner
        .suppressions
        .iter()
        .any(|(g, s)| *g == commit.group && s.subject == subject && s.at > commit.read);
    if overtaken {
        return false;
    }
    let current = |target: &Target| match target {
        Target::Existing(id) => inner.is_current(*id),
        Target::Created(_) => true,
    };
    ops.iter().all(|op| match op {
        Op::Support { memory, .. } => current(memory),
        Op::Close { memory, by, .. } => {
            inner.is_current(*memory) && by.as_ref().is_none_or(current)
        }
        Op::Merge { from, into } => inner.is_current(*from) && current(into),
        Op::Create(_) | Op::Drop(_) | Op::Refile { .. } | Op::Evict(_) => true,
    })
}

#[async_trait]
impl MemoryStore for InMemoryStore {
    async fn about(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
    ) -> Result<Vec<Memory>, MemoryError> {
        let inner = self.lock();
        Ok(inner
            .memories
            .values()
            .filter(|k| k.memory.group == group && k.memory.subject == subject)
            .filter_map(|k| inner.read(k.memory.id))
            .collect())
    }

    async fn current(
        &self,
        group: GroupId,
        subjects: &[Option<AccountId>],
        kind: Kind,
    ) -> Result<Vec<Memory>, MemoryError> {
        let inner = self.lock();
        Ok(inner
            .memories
            .values()
            .map(|k| &k.memory)
            .filter(|m| {
                m.group == group
                    && m.kind == kind
                    && m.is_current()
                    && subjects.contains(&m.subject)
            })
            .filter_map(|m| inner.read(m.id))
            .collect())
    }

    async fn names(&self, group: GroupId) -> Result<Vec<Memory>, MemoryError> {
        let inner = self.lock();
        Ok(inner
            .memories
            .values()
            .map(|k| &k.memory)
            .filter(|m| {
                m.group == group && m.kind == Kind::Name && m.is_current() && m.subject.is_some()
            })
            .filter_map(|m| inner.read(m.id))
            .collect())
    }

    async fn memory(&self, group: GroupId, id: MemoryId) -> Result<Option<Memory>, MemoryError> {
        let inner = self.lock();
        Ok(inner.read(id).filter(|m| m.group == group))
    }

    async fn pending(&self, group: GroupId) -> Result<Vec<Observation>, MemoryError> {
        let mut found: Vec<Observation> = self
            .lock()
            .observations
            .values()
            .filter(|(g, _, m)| *g == group && m.is_none())
            .map(|(_, o, _)| o.clone())
            .collect();
        found.sort_by_key(|o| (o.ordinal, o.id.get()));
        Ok(found)
    }

    async fn suppressions(&self, group: GroupId) -> Result<Vec<Suppression>, MemoryError> {
        Ok(self
            .lock()
            .suppressions
            .iter()
            .filter(|(g, _)| *g == group)
            .map(|(_, s)| s.clone())
            .collect())
    }

    async fn commit(&self, commit: &Commit) -> Result<Committed, MemoryError> {
        let mut inner = self.lock();
        let group = commit.group;
        // Everything is checked before anything is written.
        let mut ranges: Vec<(u64, u64)> = commit
            .episodes
            .iter()
            .map(|(e, _, _)| (e.first_ordinal, e.last_ordinal))
            .collect();
        ranges.extend(commit.skipped.iter().map(|(a, b, _)| (*a, *b)));
        for (index, (first, last)) in ranges.iter().enumerate() {
            if first > last {
                return Err(MemoryError::InvalidRange);
            }
            let among = ranges[..index].iter().any(|(a, b)| a <= last && first <= b);
            if among || inner.taken(group, *first, *last) {
                return Err(MemoryError::Overlap);
            }
        }
        let applying: Vec<bool> = commit
            .changes
            .iter()
            .map(|c| applies(&inner, commit, c.subject, &c.ops))
            .collect();

        let episodes: Vec<EpisodeId> = commit
            .episodes
            .iter()
            .map(|(e, v, model)| inner.insert_episode(e, model, v).id)
            .collect();
        if let Some((first, last, _)) = &commit.skipped {
            inner.skipped.push((group, *first, *last));
        }
        // What was forgotten since the batch read: no new observation about its subject reads
        // those lines again.
        let forgotten: BTreeSet<(Option<AccountId>, MessageId)> = inner
            .suppressions
            .iter()
            .filter(|(g, s)| *g == group && s.at > commit.read)
            .flat_map(|(_, s)| s.messages.iter().map(move |m| (s.subject, *m)))
            .collect();
        let mut stored: Vec<Option<ObservationId>> = Vec::new();
        for new in &commit.observations {
            if forgotten.contains(&(new.about.subject(), new.message))
                && !matches!(new.about, About::Name(_))
            {
                stored.push(None);
                continue;
            }
            let Some(episode) = episodes.get(new.episode).copied() else {
                stored.push(None);
                continue;
            };
            inner.next_observation += 1;
            let id = ObservationId::new(inner.next_observation);
            inner.observations.insert(
                id,
                (
                    group,
                    Observation {
                        id,
                        episode,
                        message: new.message,
                        ordinal: new.ordinal,
                        author: new.author,
                        quote: new.quote.clone(),
                        at: new.at,
                        about: new.about.clone(),
                        said: new.said.clone(),
                    },
                    None,
                ),
            );
            stored.push(Some(id));
        }
        let obs = |o: &Obs| match o {
            Obs::New(index) => stored.get(*index).copied().flatten(),
            Obs::Stored(id) => Some(*id),
        };

        let mut deferred = 0;
        for (changes, apply) in commit.changes.iter().zip(applying) {
            if !apply {
                deferred += 1;
                continue;
            }
            let mut created: Vec<Option<MemoryId>> = Vec::new();
            for op in &changes.ops {
                let target = |t: &Target, created: &[Option<MemoryId>]| match t {
                    Target::Existing(id) => Some(*id),
                    Target::Created(index) => created.get(*index).copied().flatten(),
                };
                match op {
                    Op::Create(new) => {
                        let key = match new.kind {
                            Kind::Name => name_key(&new.text).unwrap_or_default(),
                            Kind::Statement => memory_key(new.term.as_deref(), &new.text),
                        };
                        let id = match inner.current_of(group, new.subject, new.kind, &key) {
                            Some(id) => id,
                            None => inner.create(
                                group,
                                Memory {
                                    id: MemoryId::new(0),
                                    group,
                                    subject: new.subject,
                                    kind: new.kind,
                                    with: new.with,
                                    term: new.term.clone(),
                                    text: new.text.clone(),
                                    durability: new.durability,
                                    origin: new.origin,
                                    state: MemoryState::Current,
                                    first: new.at,
                                    last: new.at,
                                    support: Vec::new(),
                                },
                            ),
                        };
                        created.push(Some(id));
                    }
                    Op::Support { obs: o, memory } => {
                        if let (Some(o), Some(m)) = (obs(o), target(memory, &created)) {
                            inner.attach(o, m);
                        }
                    }
                    Op::Close { memory, by, at } => {
                        let by = by.as_ref().and_then(|t| target(t, &created));
                        if let Some(kept) = inner.memories.get_mut(memory) {
                            kept.memory.state = match by {
                                Some(by) => MemoryState::Replaced {
                                    by: Some(by),
                                    at: *at,
                                },
                                None => MemoryState::Ended { at: *at },
                            };
                        }
                    }
                    Op::Merge { from, into } => {
                        let Some(into) = target(into, &created) else {
                            continue;
                        };
                        let moving: Vec<ObservationId> = inner
                            .observations
                            .iter()
                            .filter(|(_, (_, _, m))| *m == Some(*from))
                            .map(|(id, _)| *id)
                            .collect();
                        for id in moving {
                            inner.attach(id, into);
                        }
                        for kept in inner.memories.values_mut() {
                            if let MemoryState::Replaced { by, .. } = &mut kept.memory.state
                                && *by == Some(*from)
                            {
                                *by = Some(into);
                            }
                        }
                        inner.memories.remove(from);
                    }
                    Op::Drop(id) => {
                        if inner
                            .observations
                            .get(id)
                            .is_some_and(|(g, _, m)| *g == group && m.is_none())
                        {
                            inner.observations.remove(id);
                        }
                    }
                    Op::Refile { obs: o, account } => {
                        if let Some(id) = obs(o)
                            && let Some((_, observation, _)) = inner.observations.get_mut(&id)
                        {
                            observation.about = About::Member(*account);
                        }
                    }
                    Op::Evict(id) => {
                        inner.delete(*id);
                    }
                }
            }
        }
        inner.windows.insert(
            group,
            WindowState {
                batched: commit.at,
                cut: 0,
            },
        );
        Ok(Committed {
            episodes: episodes.len(),
            deferred,
        })
    }

    async fn add(&self, group: GroupId, memory: &HandMemory) -> Result<MemoryId, HandError> {
        let mut inner = self.lock();
        let key = match memory.kind {
            Kind::Name => name_key(&memory.text).map_err(|_| HandError::Refused)?,
            Kind::Statement => memory_key(memory.term.as_deref(), &memory.text),
        };
        if let Some(id) = inner.current_of(group, memory.subject, memory.kind, &key) {
            return Err(HandError::Known(id));
        }
        inner.lift(group, memory.subject, memory.kind, &key);
        Ok(inner.create(
            group,
            Memory {
                id: MemoryId::new(0),
                group,
                subject: memory.subject,
                kind: memory.kind,
                with: None,
                term: memory.term.clone(),
                text: memory.text.clone(),
                durability: Durability::Lasting,
                origin: Origin::Hand { by: memory.by },
                state: MemoryState::Current,
                first: memory.at,
                last: memory.at,
                support: Vec::new(),
            },
        ))
    }

    async fn fix(
        &self,
        group: GroupId,
        id: MemoryId,
        text: &str,
        by: AccountId,
        at: UnixMillis,
    ) -> Result<MemoryId, HandError> {
        let mut inner = self.lock();
        let old = inner
            .memories
            .get(&id)
            .map(|k| k.memory.clone())
            .filter(|m| m.group == group && m.is_current())
            .ok_or(HandError::NotFound)?;
        let key = match old.kind {
            Kind::Name => name_key(text).map_err(|_| HandError::Refused)?,
            Kind::Statement => memory_key(old.term.as_deref(), text),
        };
        if let Some(existing) = inner.current_of(group, old.subject, old.kind, &key)
            && existing != id
        {
            return Err(HandError::Known(existing));
        }
        inner.lift(group, old.subject, old.kind, &key);
        let new = inner.create(
            group,
            Memory {
                text: text.to_owned(),
                origin: Origin::Hand { by },
                state: MemoryState::Current,
                first: at,
                last: at,
                support: Vec::new(),
                ..old
            },
        );
        if let Some(kept) = inner.memories.get_mut(&id) {
            kept.memory.state = MemoryState::Replaced { by: Some(new), at };
        }
        Ok(new)
    }

    async fn end(&self, group: GroupId, id: MemoryId, at: UnixMillis) -> Result<(), HandError> {
        let mut inner = self.lock();
        let kept = inner
            .memories
            .get_mut(&id)
            .filter(|k| k.memory.group == group && k.memory.is_current())
            .ok_or(HandError::NotFound)?;
        kept.memory.state = MemoryState::Ended { at };
        Ok(())
    }

    async fn forget(
        &self,
        group: GroupId,
        id: MemoryId,
        by: AccountId,
        at: UnixMillis,
    ) -> Result<Option<Memory>, MemoryError> {
        let mut inner = self.lock();
        let Some(memory) = inner.read(id).filter(|m| m.group == group) else {
            return Ok(None);
        };
        let suppression = suppression_of(&memory, by, at);
        inner.delete(id);
        inner.lift(group, memory.subject, memory.kind, &suppression.key);
        inner.suppressions.push((group, suppression));
        Ok(Some(memory))
    }
}
