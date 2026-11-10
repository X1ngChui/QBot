//! The background job that turns archived chat into episodes.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use qbot_core::{GroupId, MessageId, SliceGrid};
use qbot_llm::{Attribution, Purpose, attributed};
use qbot_sched::{JobError, JobKind, JobRunner};

use crate::build::EpisodeBuilder;
use crate::consolidate::Consolidator;
use crate::episode::Episode;
use crate::extract::{METHOD, SliceContext};
use crate::findings::Findings;
use crate::names::{NameStore, Ruling, Verdict};
use crate::rule::{NameRuler, RuleError, RulingCase, UseWindow};
use crate::slice::{Background, SliceBackground, SliceLine, SliceMember};
use crate::store::EpisodeStore;

/// Names put to a ruling each time a group's extraction runs: each is a model call, and the
/// run holds the one-at-a-time lock every group's extraction waits behind.
const RULINGS_PER_RUN: usize = 2;
/// Uses of a name a ruling is shown, one per conversation: enough to compare, few enough to
/// read with their chat.
const RULING_USES: usize = 8;
/// Lines shown before and after a use: what the line was said into, and who took it up.
const AROUND_USE: (u64, u64) = (6, 4);

/// Runs `JobKind::Extract` for a group: extracts every complete slice that has no episode yet,
/// oldest first. Idempotent: an extracted slice is covered and is never extracted again, and a
/// crash between slices loses nothing. A slice that can never be extracted is recorded as
/// skipped, so it does not hold back the group's later slices. With the new slices done, a run
/// reads a few earlier episodes again whose findings an older method read, and puts names
/// nobody could place to a ruling.
pub struct EpisodeJobs {
    store: Arc<dyn EpisodeStore>,
    builder: EpisodeBuilder,
    slices: SliceGrid,
    consolidator: Option<Arc<Consolidator>>,
    background: Option<Arc<dyn SliceBackground>>,
    rulings: Option<(Arc<dyn NameStore>, NameRuler)>,
    /// Earlier episodes read again per run.
    reread: usize,
    /// One extraction at a time: several filled batches may queue jobs for one group, and a
    /// later job must find the earlier one's episodes rather than write them again.
    running: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for EpisodeJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpisodeJobs")
            .field("slices", &self.slices)
            .finish_non_exhaustive()
    }
}

fn failed(error: impl std::fmt::Display) -> JobError {
    JobError(error.to_string())
}

impl EpisodeJobs {
    pub fn new(store: Arc<dyn EpisodeStore>, builder: EpisodeBuilder, slices: SliceGrid) -> Self {
        Self {
            store,
            builder,
            slices,
            consolidator: None,
            background: None,
            rulings: None,
            reread: 0,
            running: tokio::sync::Mutex::new(()),
        }
    }

    /// Apply each new episode's findings to the name ledger and the memories as it is stored.
    pub fn with_consolidator(mut self, consolidator: Arc<Consolidator>) -> Self {
        self.consolidator = Some(consolidator);
        self
    }

    /// Tell the extractor who the people in a slice are and what the group is.
    pub fn with_background(mut self, background: Arc<dyn SliceBackground>) -> Self {
        self.background = Some(background);
        self
    }

    /// Put names nobody could place to a ruling, recorded in `names`.
    pub fn with_rulings(mut self, names: Arc<dyn NameStore>, ruler: NameRuler) -> Self {
        self.rulings = Some((names, ruler));
        self
    }

    /// Read up to `per_run` earlier episodes again each run, until every episode's findings
    /// come from the current method.
    pub fn with_reread(mut self, per_run: usize) -> Self {
        self.reread = per_run;
        self
    }

    /// Apply the findings of every stored episode not applied yet (after a crash between storing
    /// and applying, say).
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
    /// embedding is redone; the episode itself (summary, evidence, findings) stays as it is.
    async fn index_group(&self, group: GroupId) -> Result<usize, JobError> {
        // Episodes embedded per round; the embedder splits them into requests it accepts.
        const ROUND: usize = 64;
        let (model, dims) = self.builder.index();
        let (model, mut indexed) = (model.to_owned(), 0);
        loop {
            let missing = self
                .store
                .unembedded(group, &model, dims, ROUND)
                .await
                .map_err(failed)?;
            if missing.is_empty() {
                break;
            }
            let vectors = self.builder.embed(&missing).await.map_err(failed)?;
            if vectors.len() != missing.len() {
                return Err(JobError(format!(
                    "{} episodes embedded as {} vectors",
                    missing.len(),
                    vectors.len()
                )));
            }
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

    /// Extract all complete slices of `group`; returns how many episodes were stored. Episodes
    /// the current embedding index lacks are embedded first. Every provider call is recorded as
    /// the group's extraction.
    pub async fn extract_group(&self, group: GroupId) -> Result<usize, JobError> {
        attributed(
            Attribution::group(Purpose::Extraction, group),
            self.extract(group),
        )
        .await
    }

    /// The lines with ordinals in `first..=last`, none when the range is empty.
    async fn lines(
        &self,
        group: GroupId,
        first: u64,
        last: u64,
    ) -> Result<Vec<SliceLine>, JobError> {
        if first > last {
            return Ok(Vec::new());
        }
        self.store.lines(group, first, last).await.map_err(failed)
    }

    /// What the extractor is told besides the lines of a slice starting at `start`: who the
    /// people are, what the chat was about just before, and the older lines these quote.
    async fn background(
        &self,
        group: GroupId,
        start: u64,
        parts: &[&[SliceLine]; 3],
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
        let first_shown = parts[0].first().map_or(start, |l| l.ordinal);
        background.quoted = self
            .store
            .quoted(group, &quoted_unshown(parts), first_shown)
            .await
            .map_err(failed)?;
        Ok(background)
    }

    async fn extract(&self, group: GroupId) -> Result<usize, JobError> {
        let _one_at_a_time = self.running.lock().await;
        self.index_group(group).await?;
        self.consolidate_pending(group).await?;
        let mut stored = 0;
        loop {
            let covered = self.store.covered_through(group).await.map_err(failed)?;
            let last = self.store.last_ordinal(group).await.map_err(failed)?;
            let Some(plan) = self.slices.next(covered, last) else {
                break;
            };

            let target = self
                .lines(group, *plan.target.start(), *plan.target.end())
                .await?;
            let expected = (plan.target.end() - plan.target.start() + 1) as usize;
            if target.len() != expected {
                return Err(JobError(format!(
                    "slice {:?} has {} of {expected} lines",
                    plan.target,
                    target.len()
                )));
            }
            let previous = match &plan.previous {
                Some(range) => self.lines(group, *range.start(), *range.end()).await?,
                None => Vec::new(),
            };
            let next = match &plan.next {
                Some(range) => self.lines(group, *range.start(), *range.end()).await?,
                None => Vec::new(),
            };
            let background = self
                .background(
                    group,
                    *plan.target.start(),
                    &[&previous[..], &target[..], &next[..]],
                )
                .await?;
            let ctx = SliceContext {
                group,
                previous: &previous,
                target: &target,
                next: &next,
                background: &background,
            };
            let built = match self
                .builder
                .build(group, &plan, self.slices.grid.lines_per_batch, &ctx)
                .await
            {
                Ok(built) => built,
                Err(error) if error.is_permanent() => {
                    tracing::error!(
                        group = group.get(),
                        lines = ?plan.target,
                        %error,
                        "slice skipped: it can never be extracted; its lines stay verbatim"
                    );
                    self.store
                        .skip_slice(
                            group,
                            *plan.target.start(),
                            *plan.target.end(),
                            &error.to_string(),
                        )
                        .await
                        .map_err(failed)?;
                    continue;
                }
                Err(error) => return Err(failed(error)),
            };
            self.store
                .insert(&built.episode, &built.vector, &built.embed_model)
                .await
                .map_err(failed)?;
            tracing::info!(
                group = group.get(),
                lines = ?plan.target,
                attempts = built.attempts,
                input_tokens = built.usage.input_tokens,
                output_tokens = built.usage.output_tokens,
                "episode stored"
            );
            self.consolidate_pending(group).await?;
            stored += 1;
        }
        self.reread_earlier(group).await?;
        self.rule_names(group).await?;
        Ok(stored)
    }

    /// Read again a few of the group's earlier episodes whose findings an older method read,
    /// the oldest first, and apply what the reading finds. An episode whose lines can never be
    /// read keeps its summary and has no findings.
    async fn reread_earlier(&self, group: GroupId) -> Result<usize, JobError> {
        if self.reread == 0 {
            return Ok(0);
        }
        let earlier = self
            .store
            .read_otherwise(group, METHOD, self.reread)
            .await
            .map_err(failed)?;
        let mut read = 0;
        for episode in earlier {
            let findings = self.read_again(group, &episode).await?;
            self.store
                .reread(episode.id, METHOD, &findings)
                .await
                .map_err(failed)?;
            tracing::info!(
                group = group.get(),
                lines = ?(episode.episode.first_ordinal..=episode.episode.last_ordinal),
                references = findings.references.len(),
                memories = findings.memories.len(),
                "earlier episode read again"
            );
            self.consolidate_pending(group).await?;
            read += 1;
        }
        Ok(read)
    }

    async fn read_again(&self, group: GroupId, episode: &Episode) -> Result<Findings, JobError> {
        let e = &episode.episode;
        let batch = u64::from(e.batch_lines.max(1));
        let target = self.lines(group, e.first_ordinal, e.last_ordinal).await?;
        let previous = self
            .lines(
                group,
                e.first_ordinal.saturating_sub(batch).max(1),
                e.first_ordinal - 1,
            )
            .await?;
        let next = self
            .lines(group, e.last_ordinal + 1, e.last_ordinal + batch)
            .await?;
        let background = self
            .background(
                group,
                e.first_ordinal,
                &[&previous[..], &target[..], &next[..]],
            )
            .await?;
        let ctx = SliceContext {
            group,
            previous: &previous,
            target: &target,
            next: &next,
            background: &background,
        };
        match self.builder.read(&ctx).await {
            Ok(findings) => Ok(findings),
            Err(error) if error.is_permanent() => {
                tracing::warn!(
                    group = group.get(),
                    lines = ?(e.first_ordinal..=e.last_ordinal),
                    %error,
                    "earlier episode cannot be read again; it keeps its summary"
                );
                Ok(Findings::default())
            }
            Err(error) => Err(failed(error)),
        }
    }

    /// Put the group's most used names nobody could place to a ruling, then remember what was
    /// held for the names now placed. A ruling the model cannot give is recorded as unknown, so
    /// the name waits for more uses rather than being asked about on every run.
    async fn rule_names(&self, group: GroupId) -> Result<(), JobError> {
        let Some((names, ruler)) = &self.rulings else {
            return Ok(());
        };
        let open = names.open(group, RULINGS_PER_RUN).await.map_err(failed)?;
        let mut ruled = 0;
        for name in &open {
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
            let shown: Vec<&[SliceLine]> = windows.iter().map(|w| &w.lines[..]).collect();
            let background = match &self.background {
                Some(source) => source
                    .background(group, &members(&shown))
                    .await
                    .map_err(failed)?,
                None => Background::default(),
            };
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
                background: &background,
            };
            let ruling = match ruler.rule(&case, at).await {
                Ok(ruling) => ruling,
                Err(RuleError::NoAnswer) => Ruling {
                    text: name.text.clone(),
                    verdict: Verdict::Unknown,
                    lines: Vec::new(),
                    reason: String::new(),
                    seen: name.unknown,
                    at,
                },
                // The provider failing passes with time: the name is asked about next run.
                Err(RuleError::Model(error)) => {
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

#[async_trait]
impl JobRunner for EpisodeJobs {
    async fn run(&self, kind: JobKind, group: Option<GroupId>) -> Result<(), JobError> {
        match (kind, group) {
            (JobKind::Extract, Some(group)) => self.extract_group(group).await.map(|_| ()),
            (JobKind::Extract, None) => Err(JobError("an extract job needs a group".into())),
            (other, _) => Err(JobError(format!("{other:?} is not an episode job"))),
        }
    }
}
