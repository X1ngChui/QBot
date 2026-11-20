//! The batch: the nightly job that turns each group's archived chat into episodes and what is
//! known about its people and itself.
//!
//! Each group is read in one sitting, at most a day of chat at a time, in an order chosen for
//! the provider's prefix cache (see [`crate::batch`]): the episodes of the lines, then each
//! episode's findings, then rulings on names nobody could place, then one decision per subject
//! on how its statements relate to what is known. Nothing is written until every call is done;
//! then one commit stores the episodes and every change at once ([`MemoryStore::commit`]), so a
//! batch that fails leaves nothing half-applied and the next run reads the same lines again.
//! What replies are shown of the group changes only here and through members' own commands, so
//! between two batches every reply shares the prompt prefix of the one before it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use qbot_core::{AccountId, Clock, GroupId, MessageId, UnixMillis};
use qbot_llm::{Attribution, ConvItem, Embedder, LlmError, Purpose, attributed};
use qbot_sched::JobError;

use crate::batch::{BatchError, BatchReader, METHOD, Planned, Stretch};
use crate::decide::{Candidate, DecideCase, Decision, Ref};
use crate::episode::NewEpisode;
use crate::findings::Findings;
use crate::memories::{
    About, Changes, Commit, Durability, Kind, Memory, MemoryState, MemoryStore, NewMemory,
    NewObservation, Obs, Observation, Op, Origin, Said, Suppression, Target,
};
use crate::names::{OpenName, Verdict, name_key, open_names};
use crate::rule::{RulingCase, UseWindow};
use crate::slice::{Background, SliceBackground, SliceLine, SliceMember};
use crate::store::EpisodeStore;
use crate::worth::{Capacity, HalfLives};

/// Names put to a ruling in a group's batch: each is a model call over the group's shared
/// prefix.
const RULINGS_PER_BATCH: usize = 6;
/// Uses of a name a ruling is shown, one per conversation: enough to compare, few enough to
/// read with their chat.
const RULING_USES: usize = 8;
/// Lines shown before and after a use: what the line was said into, and who took it up.
const AROUND_USE: (u64, u64) = (6, 4);
/// Lines shown before the lines read, unnumbered: what the chat continues.
const CONTEXT_LINES: usize = 30;
/// Episode summaries shown before the lines read: the conversations they continue.
const RECENT_EPISODES: usize = 8;
/// The longest stretch read in one sitting: a day's chat. A longer backlog (after the bot was
/// down) is read a day at a time, each day's results the state the next is read against.
const DAY_MS: i64 = 86_400_000;

/// What an episode is embedded as: its title and summary. Recall compares questions with this.
pub fn embedding_text(title: &str, summary: &str) -> String {
    format!("{title}\n{summary}")
}

/// Whether a member's line is a bot command, given its first word.
pub type Commands = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Runs the batch: for every group, reads everything archived since its previous batch.
/// Idempotent: lines covered by an episode are never read again, and a stretch that can never
/// be read is recorded as skipped so it does not hold back the group's later chat.
pub struct EpisodeJobs {
    episodes: Arc<dyn EpisodeStore>,
    memories: Arc<dyn MemoryStore>,
    reader: BatchReader,
    embedder: Arc<dyn Embedder>,
    clock: Arc<dyn Clock>,
    background: Option<Arc<dyn SliceBackground>>,
    commands: Option<Commands>,
    half_lives: HalfLives,
    capacity: Capacity,
    /// One batch at a time: a retried job must find the earlier attempt's episodes rather than
    /// write them again.
    running: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for EpisodeJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpisodeJobs").finish_non_exhaustive()
    }
}

fn failed(error: impl std::fmt::Display) -> JobError {
    JobError(error.to_string())
}

/// Why a stretch was not read.
enum Unread {
    /// The provider found it too long: read less of it.
    TooLong,
    Failed(JobError),
}

impl From<JobError> for Unread {
    fn from(error: JobError) -> Self {
        Unread::Failed(error)
    }
}

impl EpisodeJobs {
    /// `episodes` and `memories` are one store: the batch's commit spans both.
    pub fn new(
        episodes: Arc<dyn EpisodeStore>,
        memories: Arc<dyn MemoryStore>,
        reader: BatchReader,
        embedder: Arc<dyn Embedder>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            episodes,
            memories,
            reader,
            embedder,
            clock,
            background: None,
            commands: None,
            half_lives: HalfLives::default(),
            capacity: Capacity::default(),
            running: tokio::sync::Mutex::new(()),
        }
    }

    /// Tell the reader who the people are and what the group is.
    pub fn with_background(mut self, background: Arc<dyn SliceBackground>) -> Self {
        self.background = Some(background);
        self
    }

    /// Leave command traffic out of what is read: a member's command and the bot's reply to it
    /// (which quotes it) are not conversation, and the replies print what is known, which must
    /// not be read back into it.
    pub fn with_commands(mut self, commands: Commands) -> Self {
        self.commands = Some(commands);
        self
    }

    /// How statements lose rank, and how many each subject keeps.
    pub fn with_room(mut self, half_lives: HalfLives, capacity: Capacity) -> Self {
        self.half_lives = half_lives;
        self.capacity = capacity;
        self
    }

    /// The batch of every group, one after another (so a group's calls follow each other while
    /// their prefix is cached). A group that fails does not stop the others; the job fails, and
    /// is retried, if any did. Returns how many episodes were stored.
    pub async fn run(&self) -> Result<usize, JobError> {
        let _one_at_a_time = self.running.lock().await;
        let mut stored = 0;
        let mut failures = Vec::new();
        for group in self.episodes.groups().await.map_err(failed)? {
            match self.batch_group_locked(group).await {
                Ok(n) => stored += n,
                Err(error) => {
                    tracing::error!(group = group.get(), error = %error.0, "a group's batch failed");
                    failures.push(format!("{}: {}", group.get(), error.0));
                }
            }
        }
        if failures.is_empty() {
            Ok(stored)
        } else {
            Err(JobError(failures.join("; ")))
        }
    }

    /// One group's batch; returns how many episodes were stored. Every provider call is recorded
    /// as the group's extraction.
    pub async fn batch_group(&self, group: GroupId) -> Result<usize, JobError> {
        let _one_at_a_time = self.running.lock().await;
        self.batch_group_locked(group).await
    }

    async fn batch_group_locked(&self, group: GroupId) -> Result<usize, JobError> {
        attributed(
            Attribution::group(Purpose::Extraction, group),
            self.batch(group),
        )
        .await
    }

    async fn batch(&self, group: GroupId) -> Result<usize, JobError> {
        self.index_group(group).await?;
        let mut stored = 0;
        loop {
            let covered = self.episodes.covered_through(group).await.map_err(failed)?;
            let last = self.episodes.last_ordinal(group).await.map_err(failed)?;
            if covered >= last {
                return Ok(stored);
            }
            let lines = self.lines(group, covered + 1, last).await?;
            if lines.len() as u64 != last - covered {
                return Err(JobError(format!(
                    "lines {}..={last} have {} of {} lines",
                    covered + 1,
                    lines.len(),
                    last - covered
                )));
            }
            let end = lines[0].at.get().saturating_add(DAY_MS);
            let mut take = lines.iter().take_while(|l| l.at.get() < end).count();
            loop {
                match self.read(group, &lines[..take]).await {
                    Ok(n) => {
                        stored += n;
                        break;
                    }
                    Err(Unread::TooLong) if take > 1 => {
                        take = longest_silence(&lines[..take]);
                        tracing::warn!(
                            group = group.get(),
                            lines = take,
                            "the stretch is too long for the model; reading it in parts"
                        );
                    }
                    Err(Unread::TooLong) => {
                        self.skip(group, &lines[..take], "too long for the model")
                            .await?;
                        break;
                    }
                    Err(Unread::Failed(error)) => return Err(error),
                }
            }
        }
    }

    /// Embed the group's episodes that the current embedding index lacks: after the embedding
    /// model or width changes, recall finds an episode again once this has run. Only the
    /// embedding is redone; the episode itself stays as it is.
    async fn index_group(&self, group: GroupId) -> Result<usize, JobError> {
        // Episodes embedded per round; the embedder splits them into requests it accepts.
        const ROUND: usize = 64;
        let info = self.embedder.info();
        let (model, dims) = (info.model.clone(), info.dims);
        let mut indexed = 0;
        loop {
            let missing = self
                .episodes
                .unembedded(group, &model, dims, ROUND)
                .await
                .map_err(failed)?;
            if missing.is_empty() {
                break;
            }
            let vectors = self
                .embed(&missing.iter().map(|e| &e.episode).collect::<Vec<_>>())
                .await?;
            for (episode, vector) in missing.iter().zip(&vectors) {
                self.episodes
                    .set_embedding(episode.id, &model, vector)
                    .await
                    .map_err(failed)?;
            }
            indexed += missing.len();
        }
        if indexed > 0 {
            tracing::info!(
                group = group.get(),
                indexed,
                model,
                "episodes embedded for recall"
            );
        }
        Ok(indexed)
    }

    async fn embed(&self, episodes: &[&NewEpisode]) -> Result<Vec<Vec<f32>>, JobError> {
        if episodes.is_empty() {
            return Ok(Vec::new());
        }
        let texts: Vec<String> = episodes
            .iter()
            .map(|e| embedding_text(&e.title, &e.summary))
            .collect();
        let vectors = self.embedder.embed(&texts).await.map_err(failed)?.vectors;
        if vectors.len() != episodes.len() {
            return Err(JobError(format!(
                "{} episodes embedded as {} vectors",
                episodes.len(),
                vectors.len()
            )));
        }
        Ok(vectors)
    }

    /// The lines with ordinals in `first..=last`, none when the range is empty.
    async fn lines(
        &self,
        group: GroupId,
        first: u64,
        last: u64,
    ) -> Result<Vec<SliceLine>, JobError> {
        if first > last || last == 0 {
            return Ok(Vec::new());
        }
        self.episodes
            .lines(group, first.max(1), last)
            .await
            .map_err(failed)
    }

    /// Record lines as read without an episode.
    async fn skip(
        &self,
        group: GroupId,
        lines: &[SliceLine],
        reason: &str,
    ) -> Result<(), JobError> {
        let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
            return Ok(());
        };
        tracing::error!(
            group = group.get(),
            lines = ?(first.ordinal..=last.ordinal),
            reason,
            "lines skipped: they can never be read; they stay searchable"
        );
        let now = self.clock.now();
        self.memories
            .commit(&Commit {
                group,
                read: now,
                at: now,
                episodes: Vec::new(),
                skipped: Some((first.ordinal, last.ordinal, reason.to_owned())),
                observations: Vec::new(),
                changes: Vec::new(),
            })
            .await
            .map_err(failed)?;
        Ok(())
    }

    /// Who the people in `parts` are and what the group is, the conversations before `start`,
    /// and the older lines `parts` quote.
    async fn background(
        &self,
        group: GroupId,
        start: u64,
        parts: &[&[SliceLine]],
    ) -> Result<Background, JobError> {
        let mut background = match &self.background {
            Some(source) => source
                .background(group, &members(parts))
                .await
                .map_err(failed)?,
            None => Background::default(),
        };
        background.recent = self
            .episodes
            .before(group, start, RECENT_EPISODES)
            .await
            .map_err(failed)?
            .into_iter()
            .map(|e| (e.episode.title, e.episode.summary))
            .collect();
        let first_shown = parts
            .iter()
            .find_map(|part| part.first())
            .map_or(start, |l| l.ordinal);
        background.quoted = self
            .episodes
            .quoted(group, &quoted_unshown(parts), first_shown)
            .await
            .map_err(failed)?;
        Ok(background)
    }

    /// The lines without command traffic: a member line that is a command, and a bot line
    /// replying to one.
    fn conversation(&self, lines: &[SliceLine]) -> Vec<SliceLine> {
        let Some(commands) = &self.commands else {
            return lines.to_vec();
        };
        let mut issued: BTreeSet<MessageId> = BTreeSet::new();
        let mut out = Vec::new();
        for line in lines {
            let command = match line.speaker {
                Some(_) => first_word(&line.text).is_some_and(|word| commands(word)),
                None => replies_to(&line.text).iter().any(|id| issued.contains(id)),
            };
            if command {
                if line.speaker.is_some() {
                    issued.insert(line.message);
                }
            } else {
                out.push(line.clone());
            }
        }
        out
    }

    /// Read one stretch and commit what it found; returns how many episodes were stored.
    async fn read(&self, group: GroupId, lines: &[SliceLine]) -> Result<usize, Unread> {
        let read = self.clock.now();
        let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
            return Ok(0);
        };
        let shown = self.conversation(lines);
        if shown.is_empty() {
            self.skip(group, lines, "only command traffic").await?;
            return Ok(0);
        }
        let earlier = self
            .lines(
                group,
                first.ordinal.saturating_sub(CONTEXT_LINES as u64 * 2),
                first.ordinal - 1,
            )
            .await?;
        let earlier = self.conversation(&earlier);
        let previous = &earlier[earlier.len().saturating_sub(CONTEXT_LINES)..];
        let background = self
            .background(group, first.ordinal, &[previous, &shown])
            .await?;
        let stretch = Stretch {
            previous,
            lines: &shown,
            background: &background,
        };
        let prefix = self.reader.prefix(&stretch);

        let planned = match self.reader.episodes(&prefix, &stretch).await {
            Ok(answer) => answer,
            Err(BatchError::Model(LlmError::ContextTooLong { .. })) => return Err(Unread::TooLong),
            Err(error) if error.is_permanent() => {
                self.skip(group, lines, &error.to_string()).await?;
                return Ok(0);
            }
            Err(error) => return Err(failed(error).into()),
        };
        let model = planned.model.clone();
        let mut episodes = Vec::new();
        let mut found: Vec<NewObservation> = Vec::new();
        for (index, plan) in planned.value.iter().enumerate() {
            let findings = match self
                .reader
                .findings(&prefix, &stretch, plan, index + 1)
                .await
            {
                Ok(answer) => {
                    for problem in answer.value.dropped.iter().chain(&answer.value.unread) {
                        tracing::info!(
                            group = group.get(),
                            problem,
                            "a proposed finding was not kept"
                        );
                    }
                    answer.value.findings
                }
                // The episode is worth keeping without what its reading would have found.
                Err(error) if error.is_permanent() => {
                    tracing::warn!(group = group.get(), %error, "an episode's findings could not be read");
                    Findings::default()
                }
                Err(error) => return Err(failed(error).into()),
            };
            let span = span(&planned.value, index, &shown, first.ordinal, last.ordinal);
            episodes.push(new_episode(group, plan, &shown, span, &model));
            found.extend(observations(index, &findings));
        }

        let state = State::load(self.memories.as_ref(), group).await?;
        let numbers = numbers(&[previous, &shown], &background);
        let mut plan = Plan::default();
        found.retain(|o| !state.suppressed(o.about.subject(), o.message, &o.said));
        for stored in &state.pending {
            if state.suppressed(stored.about.subject(), stored.message, &stored.said) {
                plan.op(stored.about.subject(), Op::Drop(stored.id));
            }
        }
        let pending: Vec<&Observation> = state
            .pending
            .iter()
            .filter(|o| !state.suppressed(o.about.subject(), o.message, &o.said))
            .collect();

        // Names nobody could place, ruled on with their uses side by side.
        let verdicts = self
            .rule(group, &prefix, &background, &state, &found, &pending)
            .await?;
        let mut placed: BTreeMap<String, AccountId> = BTreeMap::new();
        let mut nobodys: BTreeMap<String, (String, String, UnixMillis)> = BTreeMap::new();
        for (name, verdict, at) in &verdicts {
            match verdict {
                Verdict::Member(account) if !state.forgot_name(*account, &name.key) => {
                    placed.insert(name.key.clone(), *account);
                }
                Verdict::Outside(meaning) | Verdict::General(meaning) => {
                    nobodys.insert(name.key.clone(), (name.text.clone(), meaning.clone(), *at));
                }
                _ => {}
            }
        }
        let settled_nobody = |key: &str| nobodys.contains_key(key) || state.nobodys.contains(key);
        // Uses and held statements of a name that is nobody's are not kept.
        found.retain(|o| !matches!(&o.about, About::Name(n) if settled_nobody(&key_of(n))));
        for stored in &pending {
            if let About::Name(n) = &stored.about
                && settled_nobody(&key_of(n))
            {
                plan.op(None, Op::Drop(stored.id));
            }
        }
        for (key, (text, meaning, at)) in &nobodys {
            if !state.group_terms.contains_key(key) {
                plan.create(
                    None,
                    NewMemory {
                        subject: None,
                        kind: Kind::Statement,
                        with: None,
                        term: Some(text.clone()),
                        text: meaning.clone(),
                        durability: Durability::Lasting,
                        origin: Origin::Ruling,
                        at: *at,
                    },
                );
            }
        }

        // Every observation from here on, today's (by index) and stored, with whom it is about
        // once names placed today are applied.
        let today: Vec<(Obs, &NewObservation)> = found
            .iter()
            .enumerate()
            .map(|(i, o)| (Obs::New(i), o))
            .collect();
        let refile = |about: &About| -> About {
            match about {
                About::Name(n) => match placed.get(&key_of(n)) {
                    Some(account) => About::Member(*account),
                    None => state
                        .ruled
                        .get(&key_of(n))
                        .map_or_else(|| about.clone(), |a| About::Member(*a)),
                },
                other => other.clone(),
            }
        };

        // Names: each use supports its member's name, created on its first use.
        let mut names: BTreeMap<(AccountId, String), Target> = BTreeMap::new();
        let uses = today
            .iter()
            .map(|(obs, o)| (*obs, o.about.clone(), &o.said, o.at))
            .chain(
                pending
                    .iter()
                    .map(|o| (Obs::Stored(o.id), o.about.clone(), &o.said, o.at)),
            );
        for (obs, about, said, at) in uses {
            let Said::Name { text, .. } = said else {
                continue;
            };
            let About::Member(account) = refile(&about) else {
                continue;
            };
            let key = key_of(text);
            if state.forgot_name(account, &key) {
                if let Obs::Stored(id) = obs {
                    plan.op(Some(account), Op::Drop(id));
                }
                continue;
            }
            if matches!(about, About::Name(_)) {
                plan.op(Some(account), Op::Refile { obs, account });
            }
            let target = match names.get(&(account, key.clone())) {
                Some(target) => *target,
                None => {
                    let ruled = placed.get(&key) == Some(&account);
                    let target = match state.name(account, &key) {
                        Some(id) => Target::Existing(id),
                        None => plan.create(
                            Some(account),
                            NewMemory {
                                subject: Some(account),
                                kind: Kind::Name,
                                with: None,
                                term: None,
                                text: crate::names::tidy_name(text),
                                durability: Durability::Lasting,
                                origin: if ruled { Origin::Ruling } else { Origin::Chat },
                                at,
                            },
                        ),
                    };
                    names.insert((account, key), target);
                    target
                }
            };
            plan.op(
                Some(account),
                Op::Support {
                    obs,
                    memory: target,
                },
            );
        }
        // Statements: one decision per subject, with every statement about it.
        let mut cases: BTreeMap<Option<AccountId>, Vec<Candidate>> = BTreeMap::new();
        let line_of = |message: MessageId| {
            shown.iter().position(|l| l.message == message).map_or_else(
                || format!("msg:{}", message.get()),
                |i| format!("#{}", i + 1),
            )
        };
        let statements = today
            .iter()
            .map(|(obs, o)| {
                (
                    *obs,
                    o.about.clone(),
                    &o.said,
                    o.author,
                    o.at,
                    o.message,
                    &o.quote,
                )
            })
            .chain(pending.iter().map(|o| {
                (
                    Obs::Stored(o.id),
                    o.about.clone(),
                    &o.said,
                    o.author,
                    o.at,
                    o.message,
                    &o.quote,
                )
            }));
        for (obs, about, said, author, at, message, quote) in statements {
            let Said::Statement {
                text,
                term,
                durability,
                with,
            } = said
            else {
                continue;
            };
            let subject = match refile(&about) {
                About::Member(account) => {
                    if matches!(about, About::Name(_)) {
                        plan.op(Some(account), Op::Refile { obs, account });
                    }
                    Some(account)
                }
                About::Group => None,
                // Held until its name is placed.
                About::Name(_) => continue,
            };
            cases.entry(subject).or_default().push(Candidate {
                obs,
                author,
                at,
                line: line_of(message),
                quote: quote.clone(),
                text: text.clone(),
                term: term.clone(),
                durability: *durability,
                with: *with,
            });
        }
        let activity = self.episodes.activity(group).await.map_err(failed)?;
        for (subject, candidates) in &cases {
            let memories = self.memories.about(group, *subject).await.map_err(failed)?;
            let forgotten: Vec<&Suppression> = state
                .suppressions
                .iter()
                .filter(|s| s.subject == *subject)
                .collect();
            let case = DecideCase {
                subject: *subject,
                memories: &memories,
                forgotten: &forgotten,
                candidates,
                numbers: &numbers,
                zone: &background.zone,
            };
            let decided = match self.reader.decide(&prefix, &case).await {
                Ok(answer) => answer.value,
                // Undecided statements wait for the subject's next batch.
                Err(error) if error.is_permanent() => {
                    tracing::warn!(group = group.get(), %error, "statements could not be decided");
                    continue;
                }
                Err(error) => return Err(failed(error).into()),
            };
            plan.decided(*subject, candidates, &decided);
            let adding = decided
                .decisions
                .iter()
                .filter(|d| matches!(d, Decision::New { .. }))
                .count();
            let evict = self.capacity.evictions(
                &after(&memories, &decided),
                adding,
                &plan.touched(*subject),
                &self.half_lives,
                &activity,
                read,
            );
            for id in evict {
                plan.op(*subject, Op::Evict(id));
            }
        }

        let vectors = self.embed(&episodes.iter().collect::<Vec<_>>()).await?;
        let embed_model = self.embedder.info().model.clone();
        let stored = episodes.len();
        let (observations, changes) = plan.finish(found);
        let commit = Commit {
            group,
            read,
            at: self.clock.now(),
            episodes: episodes
                .into_iter()
                .zip(vectors)
                .map(|(e, v)| (e, v, embed_model.clone()))
                .collect(),
            skipped: None,
            observations,
            changes,
        };
        let committed = self.memories.commit(&commit).await.map_err(failed)?;
        tracing::info!(
            group = group.get(),
            lines = lines.len(),
            episodes = stored,
            observations = commit.observations.len(),
            deferred = committed.deferred,
            "batch committed"
        );
        Ok(stored)
    }

    /// Put the group's most used names nobody could place to a ruling. A name the model cannot
    /// rule on stays open and is asked about again when it is used again.
    async fn rule(
        &self,
        group: GroupId,
        prefix: &[ConvItem],
        background: &Background,
        state: &State,
        found: &[NewObservation],
        pending: &[&Observation],
    ) -> Result<Vec<(OpenName, Verdict, UnixMillis)>, JobError> {
        // Every unplaced use as a stored observation; today's, not stored yet, under placeholder
        // ids, each episode of today's under its own.
        let uses: Vec<Observation> = found
            .iter()
            .filter(|o| matches!(o.about, About::Name(_)) && matches!(o.said, Said::Name { .. }))
            .map(|o| Observation {
                id: crate::memories::ObservationId::new(0),
                episode: crate::episode::EpisodeId::new(-(o.episode as i64) - 1),
                message: o.message,
                ordinal: o.ordinal,
                author: o.author,
                quote: o.quote.clone(),
                at: o.at,
                about: o.about.clone(),
                said: o.said.clone(),
            })
            .chain(pending.iter().map(|o| (*o).clone()))
            .filter(|o| o.is_name() && matches!(o.about, About::Name(_)))
            .collect();
        let fresh: BTreeSet<String> = found
            .iter()
            .filter(|o| matches!(o.about, About::Name(_)))
            .filter_map(|o| match &o.said {
                Said::Name { text, .. } => Some(key_of(text)),
                Said::Statement { .. } => None,
            })
            .collect();
        let mut settled: BTreeSet<String> = state.nobodys.clone();
        settled.extend(state.ruled.keys().cloned());
        let refs: Vec<&Observation> = uses.iter().collect();
        let open = open_names(&refs, &fresh, &settled, RULINGS_PER_BATCH);
        let mut verdicts = Vec::new();
        for name in open {
            // One use per conversation, the latest first.
            let mut chosen: Vec<&Observation> = Vec::new();
            let mut sorted: Vec<&Observation> =
                uses.iter().filter(|o| o.key() == name.key).collect();
            sorted.sort_by_key(|o| std::cmp::Reverse((o.at, o.ordinal)));
            for one in sorted {
                if chosen.len() < RULING_USES && !chosen.iter().any(|c| c.episode == one.episode) {
                    chosen.push(one);
                }
            }
            let Some(at) = chosen.iter().map(|u| u.at).max() else {
                continue;
            };
            let mut windows = Vec::new();
            for one in &chosen {
                let lines = self
                    .lines(
                        group,
                        one.ordinal.saturating_sub(AROUND_USE.0).max(1),
                        one.ordinal + AROUND_USE.1,
                    )
                    .await?;
                if lines.iter().any(|l| l.message == one.message) {
                    windows.push(UseWindow {
                        lines,
                        message: one.message,
                    });
                }
            }
            let case = RulingCase {
                name: &name,
                uses: &windows,
                accounts: &background.accounts,
                zone: &background.zone,
            };
            let verdict = match self.reader.ruling(prefix, &case).await {
                Ok(answer) => answer.value,
                Err(error) if error.is_permanent() => Verdict::Unknown,
                // The provider failing passes with time: the name is asked about again.
                Err(error) => {
                    tracing::warn!(group = group.get(), %error, "a name could not be ruled on");
                    continue;
                }
            };
            tracing::info!(
                group = group.get(),
                verdict = ?verdict,
                uses = name.unknown,
                "a name nobody could place was ruled on"
            );
            verdicts.push((name, verdict, at));
        }
        Ok(verdicts)
    }
}

/// What the batch reads of what is known before deciding.
struct State {
    pending: Vec<Observation>,
    suppressions: Vec<Suppression>,
    /// Current names, by member and key.
    names: BTreeMap<(AccountId, String), crate::memories::MemoryId>,
    /// Names a ruling placed with exactly one member: later uses nobody could place are theirs.
    ruled: BTreeMap<String, AccountId>,
    /// Current group terms, by key of the term.
    group_terms: BTreeMap<String, crate::memories::MemoryId>,
    /// Names ruled to be nobody's (kept as group terms).
    nobodys: BTreeSet<String>,
}

impl State {
    async fn load(memories: &dyn MemoryStore, group: GroupId) -> Result<Self, JobError> {
        let pending = memories.pending(group).await.map_err(failed)?;
        let suppressions = memories.suppressions(group).await.map_err(failed)?;
        let all_names = memories.names(group).await.map_err(failed)?;
        let mut names = BTreeMap::new();
        let mut ruled: BTreeMap<String, Vec<AccountId>> = BTreeMap::new();
        for memory in &all_names {
            let Some(account) = memory.subject else {
                continue;
            };
            names.insert((account, memory.key()), memory.id);
            if memory.origin == Origin::Ruling {
                ruled.entry(memory.key()).or_default().push(account);
            }
        }
        let terms = memories
            .current(group, &[None], Kind::Statement)
            .await
            .map_err(failed)?;
        let mut group_terms = BTreeMap::new();
        let mut nobodys = BTreeSet::new();
        for memory in &terms {
            if let Some(term) = &memory.term {
                let key = key_of(term);
                group_terms.insert(key.clone(), memory.id);
                if memory.origin == Origin::Ruling {
                    nobodys.insert(key);
                }
            }
        }
        Ok(Self {
            pending,
            suppressions,
            names,
            ruled: ruled
                .into_iter()
                .filter_map(|(key, accounts)| match accounts.as_slice() {
                    [one] => Some((key, *one)),
                    _ => None,
                })
                .collect(),
            group_terms,
            nobodys,
        })
    }

    fn name(&self, account: AccountId, key: &str) -> Option<crate::memories::MemoryId> {
        self.names.get(&(account, key.to_owned())).copied()
    }

    fn forgot_name(&self, account: AccountId, key: &str) -> bool {
        self.suppressions
            .iter()
            .any(|s| s.subject == Some(account) && s.kind == Kind::Name && s.key == key)
    }

    /// Whether an observation about `subject` reads what was forgotten: a line a forgotten
    /// memory rested on, or a forgotten name.
    fn suppressed(&self, subject: Option<AccountId>, message: MessageId, said: &Said) -> bool {
        let line = self
            .suppressions
            .iter()
            .any(|s| s.subject == subject && s.messages.contains(&message));
        let name = match (subject, said) {
            (Some(account), Said::Name { text, .. }) => self.forgot_name(account, &key_of(text)),
            _ => false,
        };
        line || name
    }
}

/// Changes by subject, in the order they were planned.
#[derive(Default)]
struct Plan {
    changes: BTreeMap<Option<AccountId>, (Vec<Op>, usize)>,
    /// Today's observations decided away (rejected, or the line ending a memory): not stored.
    dropped: BTreeSet<usize>,
}

impl Plan {
    fn op(&mut self, subject: Option<AccountId>, op: Op) {
        self.changes.entry(subject).or_default().0.push(op);
    }

    fn create(&mut self, subject: Option<AccountId>, memory: NewMemory) -> Target {
        let entry = self.changes.entry(subject).or_default();
        let target = Target::Created(entry.1);
        entry.1 += 1;
        entry.0.push(Op::Create(memory));
        target
    }

    /// The existing memories a subject's changes name.
    fn touched(&self, subject: Option<AccountId>) -> BTreeSet<crate::memories::MemoryId> {
        let mut out = BTreeSet::new();
        for op in self.changes.get(&subject).map_or(&[][..], |c| &c.0) {
            match op {
                Op::Support {
                    memory: Target::Existing(id),
                    ..
                }
                | Op::Close { memory: id, .. } => {
                    out.insert(*id);
                }
                Op::Merge {
                    into: Target::Existing(id),
                    ..
                } => {
                    out.insert(*id);
                }
                _ => {}
            }
        }
        out
    }

    /// The changes one subject's decisions make.
    fn decided(
        &mut self,
        subject: Option<AccountId>,
        candidates: &[Candidate],
        decided: &crate::decide::Decided,
    ) {
        for (from, into) in &decided.merges {
            self.op(
                subject,
                Op::Merge {
                    from: *from,
                    into: Target::Existing(*into),
                },
            );
        }
        // Memories created first, so a statement can support the one another creates.
        let mut created: BTreeMap<usize, Target> = BTreeMap::new();
        for (index, (one, decision)) in candidates.iter().zip(&decided.decisions).enumerate() {
            let (text, durability) = match decision {
                Decision::New { text, durability }
                | Decision::Replace {
                    text, durability, ..
                } => (text, durability),
                _ => continue,
            };
            let target = self.create(
                subject,
                NewMemory {
                    subject,
                    kind: Kind::Statement,
                    with: one.with,
                    term: one.term.clone(),
                    text: text.clone(),
                    durability: *durability,
                    origin: Origin::Chat,
                    at: one.at,
                },
            );
            created.insert(index, target);
            if let Decision::Replace { memory, .. } = decision {
                self.op(
                    subject,
                    Op::Close {
                        memory: *memory,
                        by: Some(target),
                        at: one.at,
                    },
                );
            }
        }
        let merged: BTreeMap<_, _> = decided.merges.iter().copied().collect();
        for (index, (one, decision)) in candidates.iter().zip(&decided.decisions).enumerate() {
            let support = match decision {
                Decision::New { .. } | Decision::Replace { .. } => created.get(&index).copied(),
                Decision::Support(Ref::Statement(other)) => created.get(other).copied(),
                Decision::Support(Ref::Memory(id)) => {
                    Some(Target::Existing(*merged.get(id).unwrap_or(id)))
                }
                // A memory said to have ended keeps no support from the line ending it.
                Decision::End(id) => {
                    self.op(
                        subject,
                        Op::Close {
                            memory: *id,
                            by: None,
                            at: one.at,
                        },
                    );
                    None
                }
                Decision::Reject(reason) => {
                    tracing::info!(reason = reason.as_str(), "a statement was not remembered");
                    None
                }
            };
            match (support, one.obs) {
                (Some(memory), obs) => self.op(subject, Op::Support { obs, memory }),
                (None, Obs::Stored(id)) => self.op(subject, Op::Drop(id)),
                (None, Obs::New(index)) => {
                    self.dropped.insert(index);
                }
            }
        }
    }

    /// Today's observations to store, and the changes, which refer to them by their place among
    /// those stored.
    fn finish(self, found: Vec<NewObservation>) -> (Vec<NewObservation>, Vec<Changes>) {
        let mut place: Vec<Option<usize>> = Vec::with_capacity(found.len());
        let mut kept = Vec::new();
        for (index, observation) in found.into_iter().enumerate() {
            if self.dropped.contains(&index) {
                place.push(None);
            } else {
                place.push(Some(kept.len()));
                kept.push(observation);
            }
        }
        let moved = |obs: Obs| match obs {
            Obs::New(index) => place.get(index).copied().flatten().map(Obs::New),
            stored => Some(stored),
        };
        let changes = self
            .changes
            .into_iter()
            .map(|(subject, (ops, _))| Changes {
                subject,
                ops: ops
                    .into_iter()
                    .filter_map(|op| match op {
                        Op::Support { obs, memory } => {
                            moved(obs).map(|obs| Op::Support { obs, memory })
                        }
                        Op::Refile { obs, account } => {
                            moved(obs).map(|obs| Op::Refile { obs, account })
                        }
                        other => Some(other),
                    })
                    .collect(),
            })
            .collect();
        (kept, changes)
    }
}

/// A subject's memories as they will stand once its decisions apply, for weighing room: what is
/// closed is closed, what is merged away is gone.
fn after(memories: &[Memory], decided: &crate::decide::Decided) -> Vec<Memory> {
    let merged: BTreeSet<_> = decided.merges.iter().map(|(from, _)| *from).collect();
    let closed: BTreeSet<_> = decided
        .decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Replace { memory, .. } | Decision::End(memory) => Some(*memory),
            _ => None,
        })
        .collect();
    memories
        .iter()
        .filter(|m| !merged.contains(&m.id))
        .map(|m| {
            let mut m = m.clone();
            if closed.contains(&m.id) && m.is_current() {
                m.state = MemoryState::Ended { at: m.last };
            }
            m
        })
        .collect()
}

fn key_of(name: &str) -> String {
    name_key(name).unwrap_or_default()
}

/// What a line's typed text begins with: its first word, past any reply and mention markers.
fn first_word(text: &str) -> Option<&str> {
    let mut rest = text.trim_start();
    while rest.starts_with("[reply:") || rest.starts_with("[at:") {
        rest = rest[rest.find(']')? + 1..].trim_start();
    }
    rest.split_whitespace().next()
}

/// Message ids a text quotes (`[reply:ID]`).
fn replies_to(text: &str) -> Vec<MessageId> {
    text.match_indices("[reply:")
        .filter_map(|(at, _)| {
            let digits: String = text[at + 7..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok().and_then(|id| MessageId::new(id).ok())
        })
        .collect()
}

/// Where to cut lines too long to read at once: after the longest silence between two of them,
/// so a conversation is not cut in the middle.
fn longest_silence(lines: &[SliceLine]) -> usize {
    let cut = lines
        .windows(2)
        .enumerate()
        .max_by_key(|(_, pair)| pair[1].at.get() - pair[0].at.get())
        .map_or(1, |(i, _)| i + 1);
    cut.clamp(1, lines.len().saturating_sub(1).max(1))
}

/// The ordinals an episode covers: from its first line (the stretch's first ordinal for the
/// first episode) up to just before the next episode's first line (the stretch's last for the
/// last), so lines left out of what was read (command traffic) are covered too.
fn span(plans: &[Planned], index: usize, shown: &[SliceLine], first: u64, last: u64) -> (u64, u64) {
    let start = if index == 0 {
        first
    } else {
        shown[plans[index].first as usize - 1].ordinal
    };
    let end = plans
        .get(index + 1)
        .map_or(last, |next| shown[next.first as usize - 1].ordinal - 1);
    (start, end)
}

/// The episode a plan stands for, over the shown `lines`, covering the ordinals `span`.
fn new_episode(
    group: GroupId,
    plan: &Planned,
    lines: &[SliceLine],
    span: (u64, u64),
    model: &str,
) -> NewEpisode {
    let own = &lines[plan.first as usize - 1..plan.last as usize];
    let (first, last) = (&own[0], &own[own.len() - 1]);
    let mut participants: Vec<_> = own.iter().filter_map(|l| l.speaker).collect();
    participants.sort();
    participants.dedup();
    NewEpisode {
        group,
        first_ordinal: span.0,
        last_ordinal: span.1,
        first_message: first.message,
        last_message: last.message,
        started: first.at,
        ended: last.at,
        line_count: u32::try_from(own.len()).unwrap_or(u32::MAX),
        participants,
        title: plan.title.clone(),
        summary: plan.summary.clone(),
        evidence: plan.evidence.clone(),
        method: METHOD.to_owned(),
        model: model.to_owned(),
    }
}

/// An episode's findings as observations: uses of names for members and for people nobody
/// could place, and statements. A name used for the bot or for someone outside the group is not
/// kept.
fn observations(episode: usize, findings: &Findings) -> Vec<NewObservation> {
    let mut out = Vec::new();
    for r in &findings.references {
        let about = match r.referent {
            crate::names::Referent::Member(account) => About::Member(account),
            crate::names::Referent::Unknown => About::Name(r.text.clone()),
            crate::names::Referent::Bot | crate::names::Referent::Outside => continue,
        };
        out.push(NewObservation {
            episode,
            message: r.message,
            ordinal: r.ordinal,
            author: r.speaker,
            quote: r.text.clone(),
            at: r.at,
            about,
            said: Said::Name {
                text: r.text.clone(),
                basis: r.basis,
                grounds: r.grounds,
                anchored: r.anchored,
                shown_by: r.shown_by.clone(),
            },
        });
    }
    for s in &findings.statements {
        out.push(NewObservation {
            episode,
            message: s.message,
            ordinal: s.ordinal,
            author: s.author,
            quote: s.quote.clone(),
            at: s.at,
            about: s.about.clone(),
            said: Said::Statement {
                text: s.text.clone(),
                term: s.term.clone(),
                durability: s.durability,
                with: s.with,
            },
        });
    }
    out
}

/// Member numbers of the accounts in the lines and the background.
fn numbers(parts: &[&[SliceLine]], background: &Background) -> BTreeMap<AccountId, u32> {
    let mut out: BTreeMap<AccountId, u32> = background
        .accounts
        .iter()
        .map(|(number, account)| (*account, *number))
        .collect();
    for line in parts.iter().flat_map(|p| p.iter()) {
        if let (Some(account), Some(number)) = (line.speaker, line.member_no) {
            out.insert(account, number);
        }
    }
    out
}

/// The messages the lines quote (`[reply:ID]`) that are not among them.
fn quoted_unshown(parts: &[&[SliceLine]]) -> Vec<MessageId> {
    let lines = || parts.iter().flat_map(|part| part.iter());
    let shown: std::collections::HashSet<MessageId> = lines().map(|l| l.message).collect();
    let mut wanted: Vec<MessageId> = lines()
        .flat_map(|line| replies_to(&line.text))
        .filter(|id| !shown.contains(id))
        .collect();
    wanted.sort();
    wanted.dedup();
    wanted
}

/// The members the lines involve, by number: who wrote them (with the account and the display
/// names their lines were sent under) and who they mention (`[at:N]`; the account is not in the
/// line).
fn members(parts: &[&[SliceLine]]) -> Vec<SliceMember> {
    let mut found: BTreeMap<u32, SliceMember> = BTreeMap::new();
    for line in parts.iter().flat_map(|part| part.iter()) {
        if let (Some(account), Some(number)) = (line.speaker, line.member_no) {
            let member = found.entry(number).or_insert_with(|| SliceMember {
                number,
                account: None,
                shown: Vec::new(),
            });
            member.account = Some(account);
            if let Some(name) = &line.shown
                && !member.shown.contains(name)
            {
                member.shown.push(name.clone());
            }
        }
        for (at, _) in line.text.match_indices("[at:") {
            let digits: String = line.text[at + 4..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(number) = digits.parse() {
                found.entry(number).or_insert_with(|| SliceMember {
                    number,
                    account: None,
                    shown: Vec::new(),
                });
            }
        }
    }
    found.into_values().collect()
}
