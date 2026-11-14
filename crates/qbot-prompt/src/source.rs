use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qbot_agent::{
    ArchiveCursor, ContextSource, Directory, EnvError, OpenedContext, Recap, RecapWhen, Trigger,
};
use qbot_context::{ChatLine, Instruction, InstructionRole, Speaker};
use qbot_core::marker::{RECALLED, escape_markers};
use qbot_core::{AccountId, Clock, GroupId, HistoryWindow, MemberNo, UnixMillis};
use qbot_memory::memories::{Memory, MemoryState, MemoryStore};
use qbot_memory::worth::{Activity, as_of_day};
use qbot_memory::{
    Background, EpisodeStore, HistoryPart, MemoryError, NameStore, NoteStore, SliceBackground,
    SliceMember, compose,
};
use qbot_store::{GroupPeople, PgArchive, RosterSettings};
use qbot_tools::{MemberRecord, MemberRecords};
use qbot_wording::{Text, say};

use crate::people::{
    MemoryView, NameIndex, background_item, date, group_item, line, memory_item, name_entry,
    roster_entry,
};
use crate::persona::Personas;
use crate::render::clock_label;
use crate::templates::{PromptError, Template, render_template};

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

    /// The accounts behind those of `numbers` that are member numbers of the group.
    async fn members(
        &self,
        group: GroupId,
        numbers: &[MemberNo],
    ) -> Result<Vec<(MemberNo, AccountId)>, EnvError>;
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

    async fn members(
        &self,
        group: GroupId,
        numbers: &[MemberNo],
    ) -> Result<Vec<(MemberNo, AccountId)>, EnvError> {
        PgArchive::members(self, group, numbers)
            .await
            .map_err(|e| EnvError(e.to_string()))
    }
}

/// Who is who in a group, from the store of record.
#[async_trait]
pub trait PeopleSource: Send + Sync {
    /// The group's roster, with the people of `include` listed whatever their activity.
    async fn people(
        &self,
        group: GroupId,
        now: UnixMillis,
        include: &[AccountId],
    ) -> Result<GroupPeople, EnvError>;
}

/// The roster read from the archive.
#[derive(Debug, Clone)]
pub struct ArchivePeople {
    archive: PgArchive,
    settings: RosterSettings,
}

impl ArchivePeople {
    pub fn new(archive: PgArchive, settings: RosterSettings) -> Self {
        Self { archive, settings }
    }
}

#[async_trait]
impl PeopleSource for ArchivePeople {
    async fn people(
        &self,
        group: GroupId,
        now: UnixMillis,
        include: &[AccountId],
    ) -> Result<GroupPeople, EnvError> {
        self.archive
            .people(group, now, self.settings, include)
            .await
            .map_err(|e| EnvError(e.to_string()))
    }
}

/// What the bot remembers and what members wrote, and how much of it a run is shown.
struct Records {
    memories: Arc<dyn MemoryStore>,
    notes: Arc<dyn NoteStore>,
    view: MemoryView,
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

/// The newest lines of the window whose people a run is shown memories of: the conversation the
/// run answers is at its end, and older lines are mostly summaries.
const PEOPLE_HERE_LINES: usize = 50;

pub struct PromptContext {
    people: Option<Arc<dyn PeopleSource>>,
    records: Option<Records>,
    names: Option<Arc<dyn NameStore>>,
    directory: Option<Arc<dyn Directory>>,
    episodes: Option<Arc<dyn EpisodeStore>>,
    history: Arc<dyn HistorySource>,
    personas: Personas,
    clock: Arc<dyn Clock>,
    window: HistoryWindow,
    zone: TimeZone,
    timezone: String,
    /// What members call the bot besides its persona's name.
    bot_nicknames: Vec<String>,
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
            people: None,
            records: None,
            names: None,
            directory: None,
            episodes: None,
            history,
            personas,
            clock,
            window: settings.window,
            zone,
            timezone: settings.timezone,
            bot_nicknames: Vec::new(),
        })
    }

    /// The names besides its persona's that members call the bot (ot.nicknames), so memory
    /// extraction never takes one for a member's name.
    pub fn with_bot_nicknames(mut self, nicknames: &[String]) -> Self {
        self.bot_nicknames = nicknames
            .iter()
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .collect();
        self
    }

    /// Show every run who is who in the group: the roster in the cached prefix.
    pub fn with_people(mut self, source: Arc<dyn PeopleSource>) -> Self {
        self.people = Some(source);
        self
    }

    /// Show every run what is remembered: the group's memories in the cached prefix, and the
    /// memories and notes of the people in its chat in its opening note.
    pub fn with_memories(
        mut self,
        memories: Arc<dyn MemoryStore>,
        notes: Arc<dyn NoteStore>,
        view: MemoryView,
    ) -> Self {
        self.records = Some(Records {
            memories,
            notes,
            view,
        });
        self
    }

    /// Tell the extractor which names the group uses for someone nobody could place yet, so
    /// every further use of them is recorded.
    pub fn with_names(mut self, names: Arc<dyn NameStore>) -> Self {
        self.names = Some(names);
        self
    }

    /// Tell every run what the members in its chat are called whom the roster does not name,
    /// so it can name people instead of writing their member numbers.
    pub fn with_directory(mut self, directory: Arc<dyn Directory>) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Show older batches through their episodes' summaries (see [`HistoryWindow`]). Without an
    /// episode store every loaded line is shown verbatim. The episodes are also the clock
    /// memories are ranked on.
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
                .rposition(|(_, l)| l.message == *message)
                .map_or(0, |i| lines.len() - i),
            // Nothing to keep raw for: no message started the run.
            Trigger::Wake { .. } | Trigger::Spontaneous => 0,
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
                        // So the model can read the episode's messages without guessing its id.
                        id: episode.id.get().to_string(),
                        start: clock_label(&self.zone, e.started),
                        end: clock_label(&self.zone, e.ended),
                        // Written from chat, and shown among the chat lines.
                        title: escape_markers(e.title.trim()),
                        summary: escape_markers(e.summary.trim()),
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

    async fn roster(&self, group: GroupId, include: &[AccountId]) -> Result<GroupPeople, EnvError> {
        match &self.people {
            Some(source) => source.people(group, self.clock.now(), include).await,
            None => Ok(GroupPeople::default()),
        }
    }

    /// The group's conversation as the clock memories are ranked on; empty without episodes.
    async fn activity(&self, group: GroupId) -> Result<Activity, MemoryError> {
        match &self.episodes {
            Some(store) => store.activity(group).await,
            None => Ok(Activity::default()),
        }
    }

    /// The roster block: one entry per listed person, in member order. It changes only when a
    /// record does or with the day, so it sits in the cached prefix. What members wrote or were
    /// called is escaped like chat and kept on its line.
    fn people_instruction(&self, people: &GroupPeople) -> Result<Option<Instruction>, EnvError> {
        let owners = NameIndex::of(people);
        let entries: Vec<String> = people
            .people
            .iter()
            .filter_map(|p| roster_entry(&self.zone, p, &owners))
            .collect();
        if entries.is_empty() {
            return Ok(None);
        }
        let text = render_template(Template::PeopleBlock, &[("entries", &entries.join("\n"))])
            .map_err(|e| EnvError(e.to_string()))?;
        Ok(Some(Instruction {
            role: InstructionRole::Reference,
            text,
            template_hash: Template::PeopleBlock.hash(),
        }))
    }

    /// The learned-knowledge block: the group's memories most worth showing, as of the start of
    /// the day, listed by term and text so the prefix changes only when what is shown does. It is
    /// reference material learned from what members said, not a system instruction.
    async fn knowledge_instruction(&self, group: GroupId) -> Result<Option<Instruction>, EnvError> {
        let Some(records) = &self.records else {
            return Ok(None);
        };
        let env = |e: MemoryError| EnvError(e.to_string());
        let memories = records
            .memories
            .current(group, &[None])
            .await
            .map_err(env)?;
        if memories.is_empty() {
            return Ok(None);
        }
        let activity = self.activity(group).await.map_err(env)?;
        let mut shown: Vec<&Memory> = records
            .view
            .half_lives
            .ranked(&memories, &activity, as_of_day(self.clock.now()))
            .into_iter()
            .take(records.view.group_memories)
            .collect();
        shown.sort_by(|a, b| (&a.term, &a.text, a.id).cmp(&(&b.term, &b.text, b.id)));
        let entries: Vec<String> = shown.iter().map(|m| group_item(&self.zone, m)).collect();
        let text = render_template(
            Template::LearnedKnowledge,
            &[("entries", &entries.join("\n"))],
        )
        .map_err(|e| EnvError(e.to_string()))?;
        Ok(Some(Instruction {
            role: InstructionRole::Reference,
            text,
            template_hash: Template::LearnedKnowledge.hash(),
        }))
    }

    pub fn zone(&self) -> &TimeZone {
        &self.zone
    }

    /// The instructions every run of `group` starts with. The rules and the legend are the same
    /// for every group and come first; the persona follows. Runs of one group share the whole
    /// prefix, and runs of different groups share the rules, for the provider's cache.
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
                render_template(Template::ReplySystem, &[])?,
            ),
            instruction(Template::Legend, render_template(Template::Legend, &[])?),
            instruction(
                Template::PersonaBlock,
                render_template(
                    Template::PersonaBlock,
                    &[
                        ("bot_name", &persona.name),
                        ("persona", persona.system_prompt.trim()),
                    ],
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

    /// The people the run's conversation involves: who wrote the newest lines of the window and
    /// whom they mention, and anyone a task's intent names; by member number.
    async fn people_here(
        &self,
        group: GroupId,
        window: &[ChatLine],
        trigger: &Trigger,
    ) -> Result<BTreeMap<MemberNo, AccountId>, EnvError> {
        let recent = &window[window.len().saturating_sub(PEOPLE_HERE_LINES)..];
        let mut here: BTreeMap<MemberNo, AccountId> = recent
            .iter()
            .filter_map(|line| match line.speaker {
                Speaker::Member { account, number } => Some((number, account)),
                Speaker::Bot => None,
            })
            .collect();
        let mut wanted = BTreeSet::new();
        for line in recent {
            wanted.extend(numbers_after(&line.text, "[at:"));
        }
        if let Trigger::Wake { intent, .. } = trigger {
            wanted.extend(numbers_after(intent, "member:"));
            wanted.extend(numbers_after(intent, "[at:"));
        }
        wanted.retain(|n| !here.contains_key(n));
        if !wanted.is_empty() {
            let wanted: Vec<MemberNo> = wanted.into_iter().collect();
            here.extend(self.history.members(group, &wanted).await?);
        }
        Ok(here)
    }

    /// The current group display name of every member in `here`, one per line in member order,
    /// asked of the platform now (in parallel) and kept nowhere. For the members the roster
    /// cannot name; the others are named there, from the archive. Empty without a directory. A
    /// member the platform cannot name is listed as unavailable.
    async fn names(&self, group: GroupId, here: &BTreeMap<MemberNo, AccountId>) -> String {
        let Some(directory) = &self.directory else {
            return String::new();
        };
        let names = futures_util::future::join_all(
            here.values()
                .map(|account| directory.display_name(group, *account)),
        )
        .await;
        here.keys()
            .zip(names)
            .map(|(number, name)| {
                let member = number.get().to_string();
                match name {
                    Some(name) => say(Text::PromptMemberName { member, name }),
                    None => say(Text::PromptMemberNameUnavailable { member }),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What is remembered about the people `here` and what members wrote about them: for each,
    /// the notes and the memories most worth showing, as of the start of the day. Empty when
    /// nothing is kept about any of them.
    async fn memories_here(
        &self,
        group: GroupId,
        here: &BTreeMap<MemberNo, AccountId>,
    ) -> Result<String, EnvError> {
        let Some(records) = &self.records else {
            return Ok(String::new());
        };
        if here.is_empty() {
            return Ok(String::new());
        }
        let env = |e: MemoryError| EnvError(e.to_string());
        let accounts: Vec<AccountId> = here.values().copied().collect();
        let subjects: Vec<Option<AccountId>> = accounts.iter().copied().map(Some).collect();
        let memories = records
            .memories
            .current(group, &subjects)
            .await
            .map_err(env)?;
        let notes = records.notes.notes(group, &accounts).await.map_err(env)?;
        if memories.is_empty() && notes.is_empty() {
            return Ok(String::new());
        }
        let activity = self.activity(group).await.map_err(env)?;
        let as_of = as_of_day(self.clock.now());
        let mut entries = Vec::new();
        for (number, account) in here {
            let mine: Vec<Memory> = memories
                .iter()
                .filter(|m| m.subject == Some(*account))
                .cloned()
                .collect();
            let theirs: Vec<String> = notes
                .iter()
                .filter(|n| n.account == *account)
                .map(|n| {
                    say(Text::PromptPersonNote {
                        text: line(&n.text),
                        date: date(&self.zone, n.updated),
                    })
                })
                .collect();
            if mine.is_empty() && theirs.is_empty() {
                continue;
            }
            entries.push(say(Text::PromptMemoryPerson {
                member: number.get().to_string(),
            }));
            if !theirs.is_empty() {
                entries.push(say(Text::PromptPersonNotes {
                    notes: theirs.join("; "),
                }));
            }
            entries.extend(
                records
                    .view
                    .half_lives
                    .ranked(&mine, &activity, as_of)
                    .into_iter()
                    .take(records.view.person_memories)
                    .map(|m| memory_item(&self.zone, m)),
            );
        }
        Ok(say(Text::PromptPeopleHere {
            entries: entries.join("\n"),
        }))
    }

    /// Members the run's context names besides its speakers, so tools can address them: the
    /// people of the run's conversation, everyone the visible chat mentions (`[at:N]`, wherever
    /// in the window) and everyone the roster lists.
    async fn named_members(
        &self,
        group: GroupId,
        window: &[ChatLine],
        here: &BTreeMap<MemberNo, AccountId>,
        people: &GroupPeople,
    ) -> Result<Vec<(MemberNo, AccountId)>, EnvError> {
        let mut known: BTreeMap<MemberNo, AccountId> = people
            .people
            .iter()
            .flat_map(|p| p.members.iter().copied())
            .collect();
        known.extend(here.iter().map(|(n, a)| (*n, *a)));
        let mut mentioned: BTreeSet<MemberNo> = window
            .iter()
            .flat_map(|line| numbers_after(&line.text, "[at:"))
            .collect();
        mentioned.retain(|n| !known.contains_key(n));
        if !mentioned.is_empty() {
            let mentioned: Vec<MemberNo> = mentioned.into_iter().collect();
            known.extend(self.history.members(group, &mentioned).await?);
        }
        for line in window {
            if let Speaker::Member { number, .. } = line.speaker {
                known.remove(&number);
            }
        }
        Ok(known.into_iter().collect())
    }

    fn trigger_note(
        &self,
        trigger: &Trigger,
        window: &[ChatLine],
        names: &str,
        people: &str,
    ) -> Result<Option<Instruction>, PromptError> {
        let now = self.now_label();
        let (template, text) = match trigger {
            Trigger::Addressed { message, .. } => {
                let Some(at) = window.iter().rposition(|l| l.message == *message) else {
                    return Ok(None);
                };
                let Speaker::Member { number, .. } = window[at].speaker else {
                    return Ok(None);
                };
                let sender = number.get().to_string();
                // Another run answered after the message (an earlier run of the same member, or
                // one of someone else's): a flag, any non-empty value turns the paragraph on.
                let replied = if window[at + 1..]
                    .iter()
                    .any(|l| matches!(l.speaker, Speaker::Bot))
                {
                    "yes"
                } else {
                    ""
                };
                // Taken back before the run could open: nothing is left to answer.
                let recalled = if window[at].text == RECALLED {
                    "yes"
                } else {
                    ""
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
                            ("replied", replied),
                            ("recalled", recalled),
                            ("names", names),
                            ("people", people),
                        ],
                    )?,
                )
            }
            Trigger::Spontaneous => (
                Template::TriggerSpontaneous,
                render_template(
                    Template::TriggerSpontaneous,
                    &[
                        ("now", &now),
                        ("timezone", &self.timezone),
                        ("names", names),
                        ("people", people),
                    ],
                )?,
            ),
            Trigger::Wake {
                timer,
                intent,
                chain,
                repeats,
            } => {
                let (task, depth) = (timer.get().to_string(), chain.depth.to_string());
                // A flag: any non-empty value turns the template's paragraph on.
                let repeats = if *repeats { "yes" } else { "" };
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
                            ("repeats", repeats),
                            ("names", names),
                            ("people", people),
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

/// The numbers after each `prefix` in `text` (`[at:3]`, `member:3`).
fn numbers_after(text: &str, prefix: &str) -> Vec<MemberNo> {
    text.match_indices(prefix)
        .filter_map(|(at, _)| {
            let digits: String = text[at + prefix.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<u32>().ok().map(MemberNo::new)
        })
        .collect()
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
        let people = self.roster(group, &[]).await?;
        // Most stable first, so runs share the longest prefix the provider can cache: the
        // persona's fixed instructions, then the roster and the group's memories (both change
        // only when a record does, or with the day), then the chat.
        let mut instructions = self.instructions(group).map_err(env)?;
        instructions.extend(self.people_instruction(&people)?);
        instructions.extend(self.knowledge_instruction(group).await?);
        let here = self.people_here(group, &window, trigger).await?;
        let members = self.named_members(group, &window, &here, &people).await?;
        // Everyone who speaks in the visible chat and everyone else the conversation involves
        // has a name the model can use. The roster says what its people are shown as, from
        // their own lines; the platform is asked only about those it does not name (someone
        // mentioned who never wrote, a line from before display names were kept).
        let summarized = |index: usize| {
            recaps
                .iter()
                .any(|r| r.when == RecapWhen::Open && r.lines.contains(&index))
        };
        let mut unnamed = here.clone();
        unnamed.extend(
            window
                .iter()
                .enumerate()
                .filter(|(index, _)| !summarized(*index))
                .filter_map(|(_, line)| match line.speaker {
                    Speaker::Member { account, number } => Some((number, account)),
                    Speaker::Bot => None,
                }),
        );
        let listed: BTreeSet<MemberNo> = people
            .people
            .iter()
            .flat_map(|p| p.shown.iter().map(|s| s.member))
            .collect();
        unnamed.retain(|number, _| !listed.contains(number));
        let names = self.names(group, &unnamed).await;
        let remembered = self.memories_here(group, &here).await?;
        let undelivered_note = Instruction {
            role: InstructionRole::Developer,
            text: render_template(Template::UndeliveredNote, &[]).map_err(env)?,
            template_hash: Template::UndeliveredNote.hash(),
        };
        Ok(OpenedContext {
            instructions,
            trigger_note: self
                .trigger_note(trigger, &window, &names, &remembered)
                .map_err(env)?,
            window,
            members,
            cursor,
            recaps,
            undelivered_note: Some(undelivered_note),
        })
    }
}

/// Names nobody could place that the extractor is told about: the ones a ruling waits on, the
/// most used first; more would crowd the people the lines involve.
const OPEN_NAMES_SHOWN: usize = 12;

/// What the extractor is told about a slice: the involved members as the group knows them (the
/// display names their lines were sent under, the names people use for them, linked accounts, a
/// block, what is remembered about them with ids) and what the group is (the bot's names, the
/// operator's background, what is remembered about the group with ids). Notes are left out:
/// they are what members wrote about each other, not needed to read a conversation.
#[async_trait]
impl SliceBackground for PromptContext {
    async fn background(
        &self,
        group: GroupId,
        members: &[SliceMember],
    ) -> Result<Background, MemoryError> {
        let backend = |e: EnvError| MemoryError::Backend(e.0);
        // Accounts of members the lines only mention.
        let unknown: Vec<MemberNo> = members
            .iter()
            .filter(|m| m.account.is_none())
            .map(|m| MemberNo::new(m.number))
            .collect();
        let resolved: BTreeMap<MemberNo, AccountId> = if unknown.is_empty() {
            BTreeMap::new()
        } else {
            self.history
                .members(group, &unknown)
                .await
                .map_err(backend)?
                .into_iter()
                .collect()
        };
        let involved: Vec<(u32, Option<AccountId>, &[String])> = members
            .iter()
            .map(|m| {
                let account = m
                    .account
                    .or_else(|| resolved.get(&MemberNo::new(m.number)).copied());
                (m.number, account, m.shown.as_slice())
            })
            .collect();
        let accounts: Vec<AccountId> = involved.iter().filter_map(|(_, a, _)| *a).collect();
        let people = self.roster(group, &accounts).await.map_err(backend)?;
        let owners = NameIndex::of(&people);
        let mut subjects: Vec<Option<AccountId>> = accounts.iter().copied().map(Some).collect();
        subjects.push(None);
        let memories = match &self.records {
            Some(records) => records.memories.current(group, &subjects).await?,
            None => Vec::new(),
        };
        let activity = self.activity(group).await?;
        let half_lives = self
            .records
            .as_ref()
            .map(|r| r.view.half_lives)
            .unwrap_or_default();
        let until = self.clock.now();
        let mut known = Vec::new();

        let mut entries = Vec::new();
        for (number, account, shown) in &involved {
            let person = account.and_then(|a| {
                people
                    .people
                    .iter()
                    .find(|p| p.members.iter().any(|(_, x)| *x == a))
            });
            let mut details = Vec::new();
            if !shown.is_empty() {
                details.push(say(Text::ExtractBackgroundShown {
                    names: shown
                        .iter()
                        .map(|s| format!("\"{}\"", line(s)))
                        .collect::<Vec<_>>()
                        .join(", "),
                }));
            }
            if let Some(person) = person {
                let own: Vec<MemberNo> = person.members.iter().map(|(n, _)| *n).collect();
                let names: Vec<String> = person
                    .names
                    .iter()
                    .filter(|n| Some(n.account) == *account && owners.shows(n))
                    .map(|n| name_entry(&self.zone, n, &owners, &own))
                    .collect();
                if !names.is_empty() {
                    details.push(say(Text::ExtractBackgroundCalled {
                        names: names.join(", "),
                    }));
                }
                let others: Vec<String> = own
                    .iter()
                    .filter(|n| n.get() != *number)
                    .map(|n| format!("member:{}", n.get()))
                    .collect();
                if !others.is_empty() {
                    details.push(say(Text::ExtractBackgroundLinked {
                        members: others.join(", "),
                    }));
                }
                if person.blocked.iter().any(|n| n.get() == *number) {
                    details.push(say(Text::ExtractBackgroundBlocked {}));
                }
            }
            let member = number.to_string();
            entries.push(if details.is_empty() {
                say(Text::ExtractBackgroundMemberPlain { member })
            } else {
                say(Text::ExtractBackgroundMember {
                    member,
                    details: details.join("; "),
                })
            });
            // Every current memory, the most worth first: any of them can be confirmed, changed
            // or ended.
            if let Some(account) = account {
                let mine: Vec<Memory> = memories
                    .iter()
                    .filter(|m| m.subject == Some(*account))
                    .cloned()
                    .collect();
                if !mine.is_empty() {
                    entries.push(say(Text::ExtractBackgroundRemembered {}));
                    for memory in half_lives.ranked(&mine, &activity, until) {
                        known.push((memory.id, memory.subject));
                        entries.push(background_item(&self.zone, memory));
                    }
                }
            }
        }
        // The rest of the roster: a line may name a member who writes none of these lines, and
        // only a listed member can be recorded as who is meant.
        let involved_numbers: BTreeSet<u32> = involved.iter().map(|(n, _, _)| *n).collect();
        let mut others = Vec::new();
        let mut other_accounts = Vec::new();
        for person in &people.people {
            let own: Vec<MemberNo> = person.members.iter().map(|(n, _)| *n).collect();
            for (number, account) in &person.members {
                if involved_numbers.contains(&number.get()) {
                    continue;
                }
                let mut details = Vec::new();
                let shown: Vec<String> = person
                    .shown
                    .iter()
                    .filter(|s| s.member == *number)
                    .map(|s| format!("\"{}\"", line(&s.text)))
                    .collect();
                if !shown.is_empty() {
                    details.push(say(Text::ExtractBackgroundShown {
                        names: shown.join(", "),
                    }));
                }
                let names: Vec<String> = person
                    .names
                    .iter()
                    .filter(|n| n.account == *account && owners.shows(n))
                    .map(|n| name_entry(&self.zone, n, &owners, &own))
                    .collect();
                if !names.is_empty() {
                    details.push(say(Text::ExtractBackgroundCalled {
                        names: names.join(", "),
                    }));
                }
                if details.is_empty() {
                    continue;
                }
                others.push(say(Text::ExtractBackgroundMember {
                    member: number.get().to_string(),
                    details: details.join("; "),
                }));
                other_accounts.push((number.get(), *account));
            }
        }
        let mut sections = Vec::new();
        if !entries.is_empty() {
            sections.push(say(Text::ExtractBackgroundPeople {
                entries: entries.join("\n"),
            }));
        }
        if !others.is_empty() {
            sections.push(say(Text::ExtractBackgroundOthers {
                entries: others.join("\n"),
            }));
        }
        if let Some(names) = &self.names {
            let open = names.open(group, OPEN_NAMES_SHOWN).await?;
            if !open.is_empty() {
                sections.push(say(Text::ExtractBackgroundOpen {
                    names: open
                        .iter()
                        .map(|o| format!("\"{}\"", line(&o.text)))
                        .collect::<Vec<_>>()
                        .join(", "),
                }));
            }
        }
        let persona = self.personas.for_group(group);
        // Members call the bot by its names; none may become one of their own.
        let mut bot_names = vec![persona.name.trim().to_owned()];
        for nickname in &self.bot_nicknames {
            if !bot_names.iter().any(|n| n.eq_ignore_ascii_case(nickname)) {
                bot_names.push(nickname.clone());
            }
        }
        bot_names.retain(|n| !n.is_empty());
        let mut about = vec![say(Text::ExtractBackgroundBot {
            names: escape_markers(&bot_names.join(", ")),
        })];
        let operator = persona.group_knowledge.trim().to_owned();
        if !operator.is_empty() {
            about.push(operator);
        }
        let ours: Vec<Memory> = memories
            .iter()
            .filter(|m| m.subject.is_none())
            .cloned()
            .collect();
        if !ours.is_empty() {
            about.push(say(Text::ExtractBackgroundGroupRemembered {}));
            for memory in half_lives.ranked(&ours, &activity, until) {
                known.push((memory.id, None));
                about.push(background_item(&self.zone, memory));
            }
        }
        sections.push(say(Text::ExtractBackgroundGroup {
            text: about.join("\n"),
        }));
        // The chat before the slice is the extraction job's to add: it reads the episodes.
        Ok(Background {
            text: sections.join("\n\n"),
            zone: self.zone.clone(),
            bot_names,
            accounts: involved
                .iter()
                .filter_map(|(n, a, _)| a.map(|a| (*n, a)))
                .chain(other_accounts)
                .collect(),
            known,
            ..Background::default()
        })
    }
}

/// `about_member`: everything kept about one member, from the same records replies use.
#[async_trait]
impl MemberRecords for PromptContext {
    async fn describe(
        &self,
        group: GroupId,
        member: MemberNo,
    ) -> Result<Option<MemberRecord>, String> {
        let Some((_, account)) = self
            .history
            .members(group, &[member])
            .await
            .map_err(|e| e.0)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let people = self.roster(group, &[account]).await.map_err(|e| e.0)?;
        let owners = NameIndex::of(&people);
        let mut out = vec![format!("member:{}", member.get())];
        if let Some(person) = people
            .people
            .iter()
            .find(|p| p.members.iter().any(|(_, a)| *a == account))
        {
            let shown: Vec<String> = person
                .shown
                .iter()
                .filter(|s| s.member == member)
                .map(|s| format!("\"{}\"", line(&s.text)))
                .collect();
            if !shown.is_empty() {
                out.push(say(Text::AboutMemberShown {
                    names: shown.join(", "),
                }));
            }
            let own: Vec<MemberNo> = person.members.iter().map(|(n, _)| *n).collect();
            let names: Vec<String> = person
                .names
                .iter()
                .filter(|n| n.account == account && owners.shows(n))
                .map(|n| name_entry(&self.zone, n, &owners, &own))
                .collect();
            if !names.is_empty() {
                out.push(say(Text::AboutMemberCalled {
                    names: names.join(", "),
                }));
            }
            if own.len() > 1 {
                out.push(say(Text::AboutMemberLinked {
                    members: own
                        .iter()
                        .map(|n| format!("member:{}", n.get()))
                        .collect::<Vec<_>>()
                        .join(", "),
                }));
            }
            if person.blocked.contains(&member) {
                out.push(say(Text::AboutMemberBlocked {}));
            }
        }
        let mut anything = out.len() > 1;
        if let Some(records) = &self.records {
            let notes = records
                .notes
                .notes(group, &[account])
                .await
                .map_err(|e| e.to_string())?;
            if !notes.is_empty() {
                out.push(say(Text::AboutMemberNotes {}));
                out.extend(notes.iter().map(|n| {
                    format!(
                        "  - {}",
                        say(Text::PromptPersonNote {
                            text: line(&n.text),
                            date: date(&self.zone, n.updated),
                        })
                    )
                }));
                anything = true;
            }
            let all = records
                .memories
                .about(group, Some(account))
                .await
                .map_err(|e| e.to_string())?;
            let current: Vec<Memory> = all.iter().filter(|m| m.is_current()).cloned().collect();
            if !current.is_empty() {
                let activity = self.activity(group).await.map_err(|e| e.to_string())?;
                out.push(say(Text::AboutMemberRemembered {}));
                out.extend(
                    records
                        .view
                        .half_lives
                        .ranked(&current, &activity, self.clock.now())
                        .into_iter()
                        .map(|m| memory_item(&self.zone, m)),
                );
                anything = true;
            }
            let mut closed: Vec<&Memory> = all.iter().filter(|m| !m.is_current()).collect();
            closed.sort_by_key(|m| std::cmp::Reverse(closed_at(m)));
            if !closed.is_empty() {
                out.push(say(Text::AboutMemberHistory {}));
                out.extend(closed.into_iter().map(|m| {
                    let (text, date) = (line(&m.display()), date(&self.zone, closed_at(m)));
                    say(match m.state {
                        MemoryState::Replaced { .. } => Text::AboutMemberReplaced { text, date },
                        _ => Text::AboutMemberEnded { text, date },
                    })
                }));
                anything = true;
            }
        }
        if !anything {
            out.push(say(Text::AboutMemberNothing {}));
        }
        Ok(Some(MemberRecord {
            account,
            text: out.join("\n"),
        }))
    }
}

fn closed_at(memory: &Memory) -> UnixMillis {
    match memory.state {
        MemoryState::Replaced { at, .. } | MemoryState::Ended { at } => at,
        MemoryState::Current => memory.last,
    }
}
