//! The batch: the nightly job that turns each group's archived chat into episodes, names and
//! memories.
//!
//! Each group is read in one sitting, in an order chosen for the provider's prefix cache (see
//! [`crate::batch`]): the episodes of everything said since the group's previous batch, then the
//! findings of each episode, then rulings on names nobody could place, then earlier episodes an
//! older method read. Episodes are stored and applied together, in chat order, only after every
//! episode's findings are in; what replies are shown of the group (who is who, what it knows,
//! where its raw chat starts) changes only here and through members' own commands, so between
//! two batches every reply shares the prompt prefix of the one before it.

use std::collections::BTreeSet;
use std::sync::Arc;

use qbot_core::{Clock, GroupId, MessageId};
use qbot_llm::{Attribution, Embedder, Purpose, attributed};
use qbot_sched::JobError;

use crate::batch::{BatchReader, Earlier, METHOD, Planned, Stretch};
use crate::consolidate::Consolidator;
use crate::episode::{Episode, NewEpisode};
use crate::findings::{Findings, MemoryAction};
use crate::names::{NameStore, OpenName};
use crate::rule::{RulingCase, UseWindow, undecided};
use crate::slice::{Background, SliceBackground, SliceLine, SliceMember};
use crate::store::EpisodeStore;

/// Names put to a ruling in a group's batch: each is a model call over the group's shared
/// prefix.
const RULINGS_PER_BATCH: usize = 6;
/// Uses of a name a ruling is shown, one per conversation: enough to compare, few enough to
/// read with their chat.
const RULING_USES: usize = 8;
/// Lines shown before and after a use: what the line was said into, and who took it up.
const AROUND_USE: (u64, u64) = (6, 4);
/// Lines shown before a stretch (and around an earlier episode read again), unnumbered: what the
/// chat continues.
const CONTEXT_LINES: u64 = 30;

/// What an episode is embedded as: its title and summary. Recall compares questions with this.
pub fn embedding_text(title: &str, summary: &str) -> String {
    format!("{title}\n{summary}")
}

/// Runs the batch: for every group, reads everything archived since its previous batch.
/// Idempotent: lines covered by an episode are never read again, a crash between storing and
/// applying loses nothing, and a stretch that can never be read is recorded as skipped so it does
/// not hold back the group's later chat.
pub struct EpisodeJobs {
    store: Arc<dyn EpisodeStore>,
    reader: BatchReader,
    embedder: Arc<dyn Embedder>,
    clock: Arc<dyn Clock>,
    consolidator: Option<Arc<Consolidator>>,
    background: Option<Arc<dyn SliceBackground>>,
    names: Option<Arc<dyn NameStore>>,
    /// Earlier episodes read again per batch of a group.
    reread: usize,
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

impl EpisodeJobs {
    pub fn new(
        store: Arc<dyn EpisodeStore>,
        reader: BatchReader,
        embedder: Arc<dyn Embedder>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            reader,
            embedder,
            clock,
            consolidator: None,
            background: None,
            names: None,
            reread: 0,
            running: tokio::sync::Mutex::new(()),
        }
    }

    /// Apply each new episode's findings to the name ledger and the memories.
    pub fn with_consolidator(mut self, consolidator: Arc<Consolidator>) -> Self {
        self.consolidator = Some(consolidator);
        self
    }

    /// Tell the reader who the people are and what the group is.
    pub fn with_background(mut self, background: Arc<dyn SliceBackground>) -> Self {
        self.background = Some(background);
        self
    }

    /// Put names nobody could place to a ruling, recorded in `names`.
    pub fn with_rulings(mut self, names: Arc<dyn NameStore>) -> Self {
        self.names = Some(names);
        self
    }

    /// Read up to `per_batch` earlier episodes again in each group's batch, until every
    /// episode's findings come from the current method.
    pub fn with_reread(mut self, per_batch: usize) -> Self {
        self.reread = per_batch;
        self
    }

    /// The batch of every group, one after another (so a group's calls follow each other while
    /// their prefix is cached). A group that fails does not stop the others; the job fails, and
    /// is retried, if any did. Returns how many episodes were stored.
    pub async fn run(&self) -> Result<usize, JobError> {
        let _one_at_a_time = self.running.lock().await;
        let mut stored = 0;
        let mut failures = Vec::new();
        for group in self.store.groups().await.map_err(failed)? {
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

    /// Apply the findings of every stored episode not applied yet, in chat order.
    async fn consolidate_pending(&self, group: GroupId) -> Result<(), JobError> {
        let Some(consolidator) = &self.consolidator else {
            return Ok(());
        };
        for episode in self.store.unconsolidated(group).await.map_err(failed)? {
            consolidator.apply(&episode).await.map_err(failed)?;
            self.store
                .mark_consolidated(episode.id)
                .await
                .map_err(failed)?;
        }
        Ok(())
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
                .store
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
                self.store
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
        self.store
            .lines(group, first.max(1), last)
            .await
            .map_err(failed)
    }

    /// Who the people in `parts` are and what the group is, the episode before `start`, and the
    /// older lines `parts` quote.
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
        background.earlier = self
            .store
            .ending_within(group, start.saturating_sub(1), start.saturating_sub(1))
            .await
            .map_err(failed)?
            .pop()
            .map(|e| (e.episode.title, e.episode.summary));
        let first_shown = parts
            .iter()
            .find_map(|part| part.first())
            .map_or(start, |l| l.ordinal);
        background.quoted = self
            .store
            .quoted(group, &quoted_unshown(parts), first_shown)
            .await
            .map_err(failed)?;
        Ok(background)
    }

    async fn batch(&self, group: GroupId) -> Result<usize, JobError> {
        self.index_group(group).await?;
        self.consolidate_pending(group).await?;
        let covered = self.store.covered_through(group).await.map_err(failed)?;
        let last = self.store.last_ordinal(group).await.map_err(failed)?;
        let lines = self.lines(group, covered + 1, last).await?;
        if lines.len() as u64 != last.saturating_sub(covered) {
            return Err(JobError(format!(
                "lines {}..={last} have {} of {} lines",
                covered + 1,
                lines.len(),
                last.saturating_sub(covered)
            )));
        }
        let earlier = if self.reread > 0 {
            self.store
                .read_otherwise(group, METHOD, self.reread)
                .await
                .map_err(failed)?
        } else {
            Vec::new()
        };
        let open = match &self.names {
            Some(names) => names.open(group, RULINGS_PER_BATCH).await.map_err(failed)?,
            None => Vec::new(),
        };
        if lines.is_empty() && earlier.is_empty() && open.is_empty() {
            return Ok(0);
        }

        let previous = self
            .lines(group, covered.saturating_sub(CONTEXT_LINES) + 1, covered)
            .await?;
        let background = self
            .background(group, covered + 1, &[&previous, &lines])
            .await?;
        let stretch = Stretch {
            previous: &previous,
            lines: &lines,
            background: &background,
        };
        let prefix = self.reader.prefix(&stretch);

        let stored = if lines.is_empty() {
            0
        } else {
            self.read_new(group, &prefix, &stretch).await?
        };
        // Today's findings may have opened names, or given waiting ones more uses.
        let open = match &self.names {
            Some(names) => names.open(group, RULINGS_PER_BATCH).await.map_err(failed)?,
            None => Vec::new(),
        };
        self.rule_names(group, &prefix, &background, &open).await?;
        self.read_again(group, &prefix, &background, &earlier)
            .await?;
        self.store
            .batched(group, self.clock.now())
            .await
            .map_err(failed)?;
        Ok(stored)
    }

    /// Divide the new lines into episodes, find each one's references and memories, then store
    /// and apply them all in chat order. Returns how many were stored.
    async fn read_new(
        &self,
        group: GroupId,
        prefix: &[qbot_llm::ConvItem],
        stretch: &Stretch<'_>,
    ) -> Result<usize, JobError> {
        let lines = stretch.lines;
        let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
            return Ok(0);
        };
        let (first_ordinal, last_ordinal) = (first.ordinal, last.ordinal);
        let planned = match self.reader.episodes(prefix, stretch).await {
            Ok(answer) => answer,
            Err(error) if error.is_permanent() => {
                tracing::error!(
                    group = group.get(),
                    lines = ?(first_ordinal..=last_ordinal),
                    %error,
                    "lines skipped: they can never be read; they stay searchable"
                );
                self.store
                    .skip_slice(group, first_ordinal, last_ordinal, &error.to_string())
                    .await
                    .map_err(failed)?;
                return Ok(0);
            }
            Err(error) => return Err(failed(error)),
        };
        let model = planned.model.clone();
        let mut episodes = Vec::new();
        for (index, plan) in planned.value.iter().enumerate() {
            let findings = match self.reader.findings(prefix, stretch, plan, index + 1).await {
                Ok(answer) => {
                    for reason in &answer.value.dropped {
                        tracing::info!(group = group.get(), %reason, "a proposed finding was not kept");
                    }
                    answer.value.findings
                }
                Err(error) if error.is_permanent() => {
                    tracing::warn!(
                        group = group.get(),
                        episode = index + 1,
                        %error,
                        "an episode's findings cannot be read; it is kept without them"
                    );
                    Findings::default()
                }
                // Nothing is stored yet: the next attempt reads the whole stretch again.
                Err(error) => return Err(failed(error)),
            };
            episodes.push(new_episode(group, plan, lines, findings, &model));
        }
        let vectors = self.embed(&episodes.iter().collect::<Vec<_>>()).await?;
        let embed_model = self.embedder.info().model.clone();
        for (episode, vector) in episodes.iter().zip(&vectors) {
            self.store
                .insert(episode, vector, &embed_model)
                .await
                .map_err(failed)?;
        }
        tracing::info!(
            group = group.get(),
            lines = ?(first_ordinal..=last_ordinal),
            episodes = episodes.len(),
            "episodes stored"
        );
        self.consolidate_pending(group).await?;
        Ok(episodes.len())
    }

    /// Put the group's most used names nobody could place to a ruling, then file what was held
    /// for the names now placed. A ruling the model cannot give is recorded as unknown, so the
    /// name waits for more uses rather than being asked about in every batch.
    async fn rule_names(
        &self,
        group: GroupId,
        prefix: &[qbot_llm::ConvItem],
        background: &Background,
        open: &[OpenName],
    ) -> Result<(), JobError> {
        let Some(names) = &self.names else {
            return Ok(());
        };
        let mut ruled = 0;
        for name in open {
            let uses = names
                .uses(group, &name.key, RULING_USES)
                .await
                .map_err(failed)?;
            let Some(at) = uses.iter().map(|u| u.at).max() else {
                continue;
            };
            let mut windows = Vec::new();
            for one in &uses {
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
            let accounts: BTreeSet<_> = uses.iter().filter_map(|u| u.referent.member()).collect();
            let mut placed: Vec<u32> = background
                .accounts
                .iter()
                .filter(|(_, account)| accounts.contains(account))
                .map(|(number, _)| *number)
                .chain(windows.iter().flat_map(|w| {
                    w.lines
                        .iter()
                        .filter_map(|l| match (l.member_no, l.speaker) {
                            (Some(number), Some(account)) if accounts.contains(&account) => {
                                Some(number)
                            }
                            _ => None,
                        })
                }))
                .collect();
            placed.sort_unstable();
            placed.dedup();
            let case = RulingCase {
                name,
                uses: &windows,
                placed: &placed,
                accounts: &background.accounts,
                zone: &background.zone,
            };
            let ruling = match self.reader.ruling(prefix, &case, at).await {
                Ok(answer) => answer.value,
                Err(error) if error.is_permanent() => undecided(name, at),
                // The provider failing passes with time: the name is asked about next batch.
                Err(error) => {
                    tracing::warn!(group = group.get(), %error, "a name could not be ruled on");
                    continue;
                }
            };
            names.rule(group, &ruling).await.map_err(failed)?;
            tracing::info!(
                group = group.get(),
                verdict = ruling.verdict.as_str(),
                uses = name.unknown,
                "a name nobody could place was ruled on"
            );
            ruled += 1;
        }
        if ruled > 0
            && let Some(consolidator) = &self.consolidator
        {
            consolidator.file_held(group).await.map_err(failed)?;
        }
        Ok(())
    }

    /// Read again the group's earlier episodes whose findings an older method read, the oldest
    /// first, and apply what the reading finds as history: later chat has been read already, so
    /// a statement that something changed is remembered as what was said then, and one that
    /// something ended is left out, so nothing learned since is closed by older words. An episode
    /// whose lines can never be read keeps its summary and has no findings.
    async fn read_again(
        &self,
        group: GroupId,
        prefix: &[qbot_llm::ConvItem],
        background: &Background,
        earlier: &[Episode],
    ) -> Result<(), JobError> {
        for episode in earlier {
            let e = &episode.episode;
            let target = self.lines(group, e.first_ordinal, e.last_ordinal).await?;
            let previous = self
                .lines(
                    group,
                    e.first_ordinal.saturating_sub(CONTEXT_LINES),
                    e.first_ordinal.saturating_sub(1),
                )
                .await?;
            let next = self
                .lines(group, e.last_ordinal + 1, e.last_ordinal + CONTEXT_LINES)
                .await?;
            let first_shown = previous.first().map_or(e.first_ordinal, |l| l.ordinal);
            let quoted = self
                .store
                .quoted(
                    group,
                    &quoted_unshown(&[&previous, &target, &next]),
                    first_shown,
                )
                .await
                .map_err(failed)?;
            let mut findings = if target.is_empty() {
                Findings::default()
            } else {
                let lines = Earlier {
                    previous: &previous,
                    target: &target,
                    next: &next,
                    quoted: &quoted,
                };
                match self.reader.reread(prefix, background, &lines).await {
                    Ok(answer) => answer.value.findings,
                    Err(error) if error.is_permanent() => {
                        tracing::warn!(
                            group = group.get(),
                            lines = ?(e.first_ordinal..=e.last_ordinal),
                            %error,
                            "earlier episode cannot be read again; it keeps its summary"
                        );
                        Findings::default()
                    }
                    Err(error) => return Err(failed(error)),
                }
            };
            findings
                .memories
                .retain(|f| f.action != MemoryAction::Ended);
            for finding in &mut findings.memories {
                if finding.action == MemoryAction::Changed {
                    finding.action = MemoryAction::New;
                    finding.id = None;
                }
            }
            self.store
                .reread(episode.id, METHOD, &findings)
                .await
                .map_err(failed)?;
            tracing::info!(
                group = group.get(),
                lines = ?(e.first_ordinal..=e.last_ordinal),
                references = findings.references.len(),
                memories = findings.memories.len(),
                "earlier episode read again"
            );
            self.consolidate_pending(group).await?;
        }
        Ok(())
    }
}

/// The episode a plan stands for, over the stretch's `lines`.
fn new_episode(
    group: GroupId,
    plan: &Planned,
    lines: &[SliceLine],
    findings: Findings,
    model: &str,
) -> NewEpisode {
    let own = &lines[plan.first as usize - 1..plan.last as usize];
    let (first, last) = (&own[0], &own[own.len() - 1]);
    let mut participants: Vec<_> = own.iter().filter_map(|l| l.speaker).collect();
    participants.sort();
    participants.dedup();
    NewEpisode {
        group,
        first_ordinal: first.ordinal,
        last_ordinal: last.ordinal,
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
        findings,
    }
}

/// The messages the lines quote (`[reply:ID]`) that are not among them.
fn quoted_unshown(parts: &[&[SliceLine]]) -> Vec<MessageId> {
    let lines = || parts.iter().flat_map(|part| part.iter());
    let shown: std::collections::HashSet<MessageId> = lines().map(|l| l.message).collect();
    let mut wanted: Vec<MessageId> = lines()
        .flat_map(|line| {
            line.text
                .match_indices("[reply:")
                .map(move |(at, _)| &line.text[at + 7..])
        })
        .filter_map(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok().and_then(|id| MessageId::new(id).ok())
        })
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
    let mut found: std::collections::BTreeMap<u32, SliceMember> = Default::default();
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
