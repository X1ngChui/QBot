use std::sync::Arc;

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qbot_agent::{
    ArchiveCursor, ContextSource, EnvError, OpenedContext, Recap, RecapWhen, Trigger,
};
use qbot_context::{ChatLine, Instruction, InstructionRole, Speaker};
use qbot_core::{Clock, GroupId, HistoryWindow};
use qbot_memory::facts::{FactStore, GROUP_TERM, GROUP_TOPIC};
use qbot_memory::{EpisodeStore, HistoryPart, compose};
use qbot_store::PgArchive;
use qbot_wording::{Text, say};

use crate::persona::Personas;
use crate::render::clock_label;
use crate::templates::{PromptError, Template, render_template};

/// One piece of learned group knowledge: a term and its meaning, or (no term) what the group
/// is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Knowledge {
    pub term: Option<String>,
    pub text: String,
}

/// Where learned group knowledge comes from.
#[async_trait]
pub trait KnowledgeSource: Send + Sync {
    /// The group's current knowledge, in a stable order.
    async fn knowledge(&self, group: GroupId) -> Result<Vec<Knowledge>, EnvError>;
}

/// Group knowledge held as group facts.
pub struct FactKnowledge {
    facts: Arc<dyn FactStore>,
    /// Terms shown at most: the block enters every reply's instructions, and a long-lived group
    /// accumulates far more terms than a prompt should carry.
    max_terms: usize,
}

impl FactKnowledge {
    /// Terms a reply is shown. The block enters every reply's instructions, and a long-lived
    /// group learns far more terms than a prompt should carry; 40 covers a group's working
    /// vocabulary.
    pub const MAX_TERMS: usize = 40;

    pub fn new(facts: Arc<dyn FactStore>, max_terms: usize) -> Self {
        Self { facts, max_terms }
    }
}

impl std::fmt::Debug for FactKnowledge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FactKnowledge")
            .field("max_terms", &self.max_terms)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl KnowledgeSource for FactKnowledge {
    /// The topic, then the most recently confirmed terms (up to `max_terms`), listed in key
    /// order: which terms are shown follows recency, but confirming a shown term again does not
    /// reorder the block, so the cached prompt prefix survives it.
    async fn knowledge(&self, group: GroupId) -> Result<Vec<Knowledge>, EnvError> {
        let facts = self
            .facts
            .current(group, None)
            .await
            .map_err(|e| EnvError(e.to_string()))?;
        // Within a predicate the store lists the most recently confirmed first.
        let mut out: Vec<Knowledge> = facts
            .iter()
            .find(|f| f.predicate == GROUP_TOPIC)
            .map(|f| Knowledge {
                term: None,
                text: f.object.clone(),
            })
            .into_iter()
            .collect();
        let mut terms: Vec<_> = facts
            .iter()
            .filter(|f| f.predicate == GROUP_TERM)
            .take(self.max_terms)
            .collect();
        terms.sort_by(|a, b| a.key.cmp(&b.key).then(a.id.cmp(&b.id)));
        out.extend(terms.into_iter().map(|f| Knowledge {
            term: Some(f.label.clone().unwrap_or_else(|| f.key.clone())),
            text: f.object.clone(),
        }));
        Ok(out)
    }
}

/// Where a run's chat comes from.
#[async_trait]
pub trait HistorySource: Send + Sync {
    /// The ordinal of the group's newest line, 0 when it has none.
    async fn last_ordinal(&self, group: GroupId) -> Result<u64, EnvError>;

    /// Lines from ordinal `first` on, as `(ordinal, line)` in order, and the cursor after the
    /// newest.
    async fn lines_from(
        &self,
        group: GroupId,
        first: u64,
    ) -> Result<(Vec<(u64, ChatLine)>, ArchiveCursor), EnvError>;
}

#[async_trait]
impl HistorySource for PgArchive {
    async fn last_ordinal(&self, group: GroupId) -> Result<u64, EnvError> {
        PgArchive::last_ordinal(self, group)
            .await
            .map_err(|e| EnvError(e.to_string()))
    }

    async fn lines_from(
        &self,
        group: GroupId,
        first: u64,
    ) -> Result<(Vec<(u64, ChatLine)>, ArchiveCursor), EnvError> {
        PgArchive::lines_from(self, group, first)
            .await
            .map_err(|e| EnvError(e.to_string()))
    }
}

/// A run's chat: every line it loads, and the episode recaps over some of them.
struct History {
    lines: Vec<(u64, ChatLine)>,
    cursor: ArchiveCursor,
    recaps: Vec<Recap>,
}

#[derive(Debug, Clone)]
pub struct PromptSettings {
    pub window: HistoryWindow,
    /// IANA name, shown to the model next to the current time.
    pub timezone: String,
}

pub struct PromptContext {
    knowledge: Option<Arc<dyn KnowledgeSource>>,
    episodes: Option<Arc<dyn EpisodeStore>>,
    history: Arc<dyn HistorySource>,
    personas: Personas,
    clock: Arc<dyn Clock>,
    window: HistoryWindow,
    zone: TimeZone,
    timezone: String,
}

impl std::fmt::Debug for PromptContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptContext")
            .field("timezone", &self.timezone)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown time zone `{0}`")]
pub struct UnknownZone(pub String);

impl PromptContext {
    pub fn new(
        history: Arc<dyn HistorySource>,
        personas: Personas,
        clock: Arc<dyn Clock>,
        settings: PromptSettings,
    ) -> Result<Self, UnknownZone> {
        let zone = TimeZone::get(&settings.timezone)
            .map_err(|_| UnknownZone(settings.timezone.clone()))?;
        Ok(Self {
            knowledge: None,
            episodes: None,
            history,
            personas,
            clock,
            window: settings.window,
            zone,
            timezone: settings.timezone,
        })
    }

    /// Add what the group has taught the bot to every run's instructions.
    pub fn with_knowledge(mut self, source: Arc<dyn KnowledgeSource>) -> Self {
        self.knowledge = Some(source);
        self
    }

    /// Show older batches through their episodes' summaries (see [`HistoryWindow`]). Without an
    /// episode store every loaded line is shown verbatim.
    pub fn with_episodes(mut self, episodes: Arc<dyn EpisodeStore>) -> Self {
        self.episodes = Some(episodes);
        self
    }

    /// Load the summary and raw tiers and decide which lines episodes stand in for.
    ///
    /// Episodes that end in the summary tier replace their lines from the start of the run. The
    /// lines are still loaded (reaching back to where such an episode begins), so the transcript
    /// holds the originals; the model sees the summary. Episodes in the raw tier are offered only
    /// as the fallback for a provider that finds the context too long. No recap ever covers the
    /// trigger message or what follows it, and lines no episode covers stay verbatim.
    async fn history(&self, group: GroupId, trigger: &Trigger) -> Result<History, EnvError> {
        let env = |e: qbot_memory::MemoryError| EnvError(e.to_string());
        let last = self.history.last_ordinal(group).await?;
        if last == 0 {
            return Ok(History {
                lines: Vec::new(),
                cursor: ArchiveCursor(0),
                recaps: Vec::new(),
            });
        }
        let grid = self.window.grid();
        let tiers = self.window.tiers(last);
        let raw_start = *grid.batch_range(tiers.raw_from).start();
        let summary_start = *grid.batch_range(tiers.summary_from).start();
        let summarized = match &self.episodes {
            Some(store) if summary_start < raw_start => store
                .ending_within(group, summary_start, raw_start - 1)
                .await
                .map_err(env)?,
            _ => Vec::new(),
        };
        let load_from = summarized
            .iter()
            .map(|e| e.episode.first_ordinal)
            .fold(summary_start, u64::min);
        let (lines, cursor) = self.history.lines_from(group, load_from).await?;
        let (Some(first), Some(newest)) = (lines.first(), lines.last()) else {
            return Ok(History {
                lines,
                cursor,
                recaps: Vec::new(),
            });
        };
        let mut episodes = summarized;
        if let Some(store) = &self.episodes {
            episodes.extend(
                store
                    .ending_within(group, raw_start.max(first.0), newest.0)
                    .await
                    .map_err(env)?,
            );
        }
        let keep_tail = match trigger {
            Trigger::Addressed { message, .. } => lines
                .iter()
                .position(|(_, l)| l.message == *message)
                .map_or(0, |i| lines.len() - i),
            Trigger::Wake { .. } => 0,
        };
        let mut recaps = Vec::new();
        let mut at = 0;
        for part in compose(&lines, &episodes, keep_tail) {
            match part {
                HistoryPart::Lines(raw) => at += raw.len(),
                HistoryPart::Recap(episode) => {
                    let e = &episode.episode;
                    let count = lines[at..]
                        .iter()
                        .take_while(|(o, _)| *o <= e.last_ordinal)
                        .count();
                    let text = say(Text::PromptRecap {
                        start: clock_label(&self.zone, e.started),
                        end: clock_label(&self.zone, e.ended),
                        title: e.title.trim().to_owned(),
                        summary: e.summary.trim().to_owned(),
                    });
                    recaps.push(Recap {
                        lines: at..at + count,
                        text,
                        when: if e.last_ordinal < raw_start {
                            RecapWhen::Open
                        } else {
                            RecapWhen::Overflow
                        },
                    });
                    at += count;
                }
            }
        }
        Ok(History {
            lines,
            cursor,
            recaps,
        })
    }

    /// The learned-knowledge instruction, if the group has any knowledge.
    async fn knowledge_instruction(&self, group: GroupId) -> Result<Option<Instruction>, EnvError> {
        let Some(source) = &self.knowledge else {
            return Ok(None);
        };
        let items = source.knowledge(group).await?;
        if items.is_empty() {
            return Ok(None);
        }
        let entries: Vec<String> = items
            .iter()
            .map(|k| match &k.term {
                Some(term) => say(Text::PromptGroupTerm {
                    term: term.clone(),
                    text: k.text.clone(),
                }),
                None => say(Text::PromptGroupTopic {
                    text: k.text.clone(),
                }),
            })
            .collect();
        let text = render_template(
            Template::LearnedKnowledge,
            &[("entries", &entries.join("\n"))],
        )
        .map_err(|e| EnvError(e.to_string()))?;
        Ok(Some(Instruction {
            role: InstructionRole::System,
            text,
            template_hash: Template::LearnedKnowledge.hash(),
        }))
    }

    pub fn zone(&self) -> &TimeZone {
        &self.zone
    }

    /// The instructions every run of `group` starts with. They depend only on the group's
    /// persona, so runs of one group share a prefix the provider can cache.
    pub fn instructions(&self, group: GroupId) -> Result<Vec<Instruction>, PromptError> {
        let persona = self.personas.for_group(group);
        let instruction = |template: Template, text: String| Instruction {
            role: InstructionRole::System,
            text,
            template_hash: template.hash(),
        };
        let mut out = vec![
            instruction(
                Template::ReplySystem,
                render_template(Template::ReplySystem, &[("bot_name", &persona.name)])?,
            ),
            instruction(Template::Legend, render_template(Template::Legend, &[])?),
            instruction(
                Template::PersonaBlock,
                render_template(
                    Template::PersonaBlock,
                    &[("persona", persona.system_prompt.trim())],
                )?,
            ),
        ];
        if !persona.group_knowledge.trim().is_empty() {
            out.push(instruction(
                Template::KnowledgeBlock,
                render_template(
                    Template::KnowledgeBlock,
                    &[("knowledge", persona.group_knowledge.trim())],
                )?,
            ));
        }
        Ok(out)
    }

    fn now_label(&self) -> String {
        let now = self.clock.now();
        // Same shape as chat lines, with the year, since the model has no other clock.
        Timestamp::from_millisecond(now.get())
            .map(|t| {
                t.to_zoned(self.zone.clone())
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|_| clock_label(&self.zone, now))
    }

    fn trigger_note(
        &self,
        trigger: &Trigger,
        window: &[ChatLine],
    ) -> Result<Option<Instruction>, PromptError> {
        let now = self.now_label();
        let (template, text) = match trigger {
            Trigger::Addressed { message, .. } => {
                let sender =
                    window
                        .iter()
                        .find(|l| l.message == *message)
                        .and_then(|l| match l.speaker {
                            Speaker::Member { number, .. } => Some(number.get().to_string()),
                            Speaker::Bot => None,
                        });
                let Some(sender) = sender else {
                    return Ok(None);
                };
                let message = message.get().to_string();
                (
                    Template::TriggerAddressed,
                    render_template(
                        Template::TriggerAddressed,
                        &[
                            ("now", &now),
                            ("timezone", &self.timezone),
                            ("sender", &sender),
                            ("message", &message),
                        ],
                    )?,
                )
            }
            Trigger::Wake {
                timer,
                intent,
                chain,
            } => {
                let (task, depth) = (timer.get().to_string(), chain.depth.to_string());
                (
                    Template::TriggerWake,
                    render_template(
                        Template::TriggerWake,
                        &[
                            ("now", &now),
                            ("timezone", &self.timezone),
                            ("task", &task),
                            ("depth", &depth),
                            ("intent", intent),
                        ],
                    )?,
                )
            }
        };
        Ok(Some(Instruction {
            role: InstructionRole::Trigger,
            text,
            template_hash: template.hash(),
        }))
    }
}

#[async_trait]
impl ContextSource for PromptContext {
    async fn open(&self, group: GroupId, trigger: &Trigger) -> Result<OpenedContext, EnvError> {
        let History {
            lines,
            cursor,
            recaps,
        } = self.history(group, trigger).await?;
        let window: Vec<ChatLine> = lines.into_iter().map(|(_, line)| line).collect();
        let env = |e: PromptError| EnvError(e.to_string());
        let mut instructions = self.instructions(group).map_err(env)?;
        instructions.extend(self.knowledge_instruction(group).await?);
        let undelivered_note = Instruction {
            role: InstructionRole::Developer,
            text: render_template(Template::UndeliveredNote, &[]).map_err(env)?,
            template_hash: Template::UndeliveredNote.hash(),
        };
        Ok(OpenedContext {
            instructions,
            trigger_note: self.trigger_note(trigger, &window).map_err(env)?,
            window,
            cursor,
            recaps,
            undelivered_note: Some(undelivered_note),
        })
    }
}
