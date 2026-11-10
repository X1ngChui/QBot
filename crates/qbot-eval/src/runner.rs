//! One scenario through the real system: the production prompt layer, tool set and run loop,
//! against the configured text model. Only the world around the bot is simulated (the group's
//! archive and delivery, stores, web results); nothing reaches QQ.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::sim::{MemorySink, SimWorld};
use qbot_agent::{
    Archive, ArchiveCursor, Chain, ContextSource, Delivery, Directory, EnvError, OutSegment,
    RunDeps, RunInput, RunLimits, RunLog, Trigger, execute,
};
use qbot_context::{
    AssistantPart, ChatLine, InstructionRole, Item, Outcome, Part, RunEnd, Speaker,
};
use qbot_core::{AccountId, ChainId, Clock, GroupId, MessageId, RunId, TimerId, UnixMillis};
use qbot_llm::embedding::FakeEmbedder;
use qbot_llm::search::{FakeSearch, PageRead, SearchHit, SearchResults};
use qbot_llm::{Provider, ReasoningEffort, Usage};
use qbot_memory::{
    Change, Durability, EpisodeId, Grounds, InMemoryMemoryStore, MemoryEpisodeStore, MemoryId,
    MemoryNameStore, MemoryNoteStore, MemoryStore, NameBasis, NameStore, NoteStore, Recall,
    RecallParams, Reference, Referent, Statement, Support,
};
use qbot_prompt::{
    GroupPeople, HistorySource, MemoryView, PeopleSource, Person, Personas, PromptContext,
    PromptRenderer, PromptSettings, ShownName,
};
use qbot_sched::{MemoryTimerStore, TaskLimits, TaskService};
use qbot_tools::{
    ReadUrl, SearchSettings, ToolSettings, WebSearchTool, add_memory_tools, standard_tools,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::scenario::{Scenario, SendRule};

pub const GROUP: i64 = 900_001;
const TIMEZONE: &str = "Asia/Shanghai";

/// A clock that always says the same time: just after the last chat line.
#[derive(Debug)]
struct Fixed(UnixMillis);
impl Clock for Fixed {
    fn now(&self) -> UnixMillis {
        self.0
    }
}

/// Group display names as the scenario defines them.
struct Names(Vec<(AccountId, String)>);

#[async_trait]
impl Directory for Names {
    /// Marker-safe, as the platform directory gives a name.
    async fn display_name(&self, _: GroupId, account: AccountId) -> Option<String> {
        self.0
            .iter()
            .find(|(a, _)| *a == account)
            .map(|(_, n)| qbot_core::marker::escape_markers(n))
    }
}

/// The simulated archive as the prompt layer reads it: every line, ordinals from 1, and the
/// scenario's members by number.
struct WorldHistory(Arc<SimWorld>, Vec<(u32, AccountId)>);

#[async_trait]
impl HistorySource for WorldHistory {
    async fn last_ordinal(&self, _: GroupId) -> Result<u64, EnvError> {
        Ok(self.0.lines().len() as u64)
    }

    async fn lines_from(
        &self,
        _: GroupId,
        first: u64,
    ) -> Result<(Vec<(u64, ChatLine)>, ArchiveCursor), EnvError> {
        let lines = self.0.lines();
        let cursor = ArchiveCursor(lines.len() as u64);
        Ok((
            (1u64..).zip(lines).filter(|(o, _)| *o >= first).collect(),
            cursor,
        ))
    }

    async fn members(
        &self,
        _: GroupId,
        numbers: &[qbot_core::MemberNo],
    ) -> Result<Vec<(qbot_core::MemberNo, AccountId)>, EnvError> {
        Ok(self
            .1
            .iter()
            .map(|(n, a)| (qbot_core::MemberNo::new(*n), *a))
            .filter(|(n, _)| numbers.contains(n))
            .collect())
    }
}

/// The scenario's roster, as the store would report it.
struct ScenarioPeople(GroupPeople);

#[async_trait]
impl PeopleSource for ScenarioPeople {
    async fn people(
        &self,
        _: GroupId,
        _: UnixMillis,
        _: &[AccountId],
    ) -> Result<GroupPeople, EnvError> {
        Ok(self.0.clone())
    }
}

/// Whether `text` refers to someone by a member handle: `member:12`, `member 12`, `Member12`.
/// The markers the model can only read: every system marker except the ones `send_messages`
/// turns into message segments.
const SENDABLE: [&str; 6] = ["at", "reply", "face", "dice", "rps", "contact"];

/// Receive-only markers written into a message, as `[name:` or `[name]`.
fn read_only_markers(text: &str) -> Vec<String> {
    qbot_core::marker::MARKERS
        .iter()
        .filter(|name| !SENDABLE.contains(name))
        .filter(|name| text.contains(&format!("[{name}:")) || text.contains(&format!("[{name}]")))
        .map(|name| (*name).to_owned())
        .collect()
}

fn has_member_handle(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.match_indices("member").any(|(at, word)| {
        let starts_word = !lower[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        starts_word
            && lower[at + word.len()..]
                .trim_start_matches([':', ' ', '#'])
                .starts_with(|c: char| c.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn receive_only_markers_are_found_and_send_markers_are_not() {
        assert_eq!(
            super::read_only_markers("see [link:title=x] and [image] and [at:3] [face:14]"),
            ["image", "link"]
        );
        assert!(super::read_only_markers("arr[0] [sic] https://x.example/[1]").is_empty());
    }

    use super::has_member_handle;

    #[test]
    fn member_handles_are_found_in_prose_and_nothing_else_is() {
        for text in ["ask member:5", "member 12 said", "Member#3", "MEMBER:7"] {
            assert!(has_member_handle(text), "{text}");
        }
        for text in [
            "members 5 of us",
            "remember 2 things",
            "a member said 3 things",
            "member:",
        ] {
            assert!(!has_member_handle(text), "{text}");
        }
    }
}

/// One tool call as it happened.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CallRecord {
    pub name: String,
    pub arguments: String,
    pub outcome: String,
    pub result: String,
}

/// A deterministic check and how it came out.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    pub what: String,
    pub passed: bool,
    pub detail: String,
}

/// Everything the bot did in one scenario run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunResult {
    pub end: String,
    pub error: Option<String>,
    /// The messages sent, rendered for reading.
    pub sent: Vec<String>,
    pub calls: Vec<CallRecord>,
    /// Text the model wrote outside tool calls. Never delivered; kept to show what it meant.
    pub undelivered: Vec<String>,
    /// A turn ended in text alone, so the run reminded the model and forced a tool call: the
    /// fallback, not the normal path.
    pub reminded: bool,
    pub checks: Vec<Check>,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

fn timestamp(text: &str) -> Result<UnixMillis, String> {
    let at = jiff::Timestamp::strptime("%Y-%m-%dT%H:%M:%S%:z", text).map_err(|e| e.to_string())?;
    Ok(UnixMillis::new(at.as_millisecond()))
}

fn account(n: i64) -> Result<AccountId, String> {
    AccountId::new(n).map_err(|e| e.to_string())
}

/// How a sent message reads, with members named as the group shows them.
fn render(segments: &[OutSegment], scenario: &Scenario, accounts: &[(u32, AccountId)]) -> String {
    let name = |a: &AccountId| {
        accounts
            .iter()
            .find(|(_, x)| x == a)
            .and_then(|(n, _)| scenario.member(*n))
            .map_or_else(|| format!("account {}", a.get()), |m| m.name.clone())
    };
    segments
        .iter()
        .map(|s| match s {
            OutSegment::Text(t) => t.clone(),
            OutSegment::At(a) => format!("@{}", name(a)),
            OutSegment::Reply(m) => format!("[quoting message {}]", m.get()),
            OutSegment::Face(id) => format!("[face {id}]"),
            OutSegment::Dice => "[dice]".to_owned(),
            OutSegment::Rps => "[rock-paper-scissors]".to_owned(),
            OutSegment::Contact(a) => format!("[contact card of {}]", name(a)),
        })
        .collect()
}

/// The roster as the store would give it: every member (a scenario's members all spoke
/// recently), one entry per person, with what they are and were shown as and the names people
/// use for them.
async fn scenario_people(
    scenario: &Scenario,
    accounts: &[(u32, AccountId)],
    names: &dyn NameStore,
    group: GroupId,
    now: UnixMillis,
) -> Result<GroupPeople, String> {
    use qbot_core::MemberNo;
    let all = names.names(group, None).await.map_err(|e| e.to_string())?;
    let number_of = |account: AccountId| {
        accounts
            .iter()
            .find(|(_, a)| *a == account)
            .map(|(n, _)| *n)
    };
    let mut sets: Vec<Vec<u32>> = scenario.same_person.clone();
    for (n, _) in accounts {
        if !sets.iter().any(|set| set.contains(n)) {
            sets.push(vec![*n]);
        }
    }
    let mut people = Vec::new();
    for mut set in sets {
        set.sort();
        let mut person = Person {
            active: Some(now),
            ..Person::default()
        };
        for n in &set {
            let Some((_, account)) = accounts.iter().find(|(x, _)| x == n) else {
                continue;
            };
            let member = MemberNo::new(*n);
            person.members.push((member, *account));
            let Some(spec) = scenario.member(*n) else {
                continue;
            };
            if spec.blocked {
                person.blocked.push(member);
            }
            // Earlier display names were last used a day apart, before the current one.
            let shown = std::iter::once(&spec.name).chain(&spec.earlier_names);
            for (days, text) in (0i64..).zip(shown) {
                person.shown.push(ShownName {
                    member,
                    text: text.clone(),
                    last: UnixMillis::new(now.get() - days * 86_400_000),
                });
            }
            person
                .names
                .extend(all.iter().filter(|k| k.account == *account).cloned());
        }
        person
            .names
            .sort_by(|a, b| b.set.cmp(&a.set).then(b.uses().cmp(&a.uses())));
        people.push(person);
    }
    people.sort_by_key(|p| p.members.first().map(|(n, _)| *n));
    let names = all
        .into_iter()
        .filter_map(|k| number_of(k.account).map(|n| (MemberNo::new(n), k)))
        .collect();
    Ok(GroupPeople { people, names })
}

pub struct Runner {
    pub provider: Arc<dyn Provider>,
    pub personas: Personas,
    /// The replies' reasoning effort (`providers.text.reasoning`).
    pub reasoning: ReasoningEffort,
}

impl Runner {
    pub async fn run(&self, scenario: &Scenario) -> Result<(RunResult, Vec<ChatLine>), String> {
        let group = GroupId::new(GROUP).map_err(|e| e.to_string())?;
        let world = SimWorld::new(group);
        let accounts: Vec<(u32, AccountId)> = scenario
            .members
            .iter()
            .map(|m| account(m.account).map(|a| (m.number, a)))
            .collect::<Result<_, _>>()?;
        let account_of = |n: u32| {
            accounts
                .iter()
                .find(|(x, _)| *x == n)
                .map(|(_, a)| *a)
                .ok_or_else(|| format!("unknown member {n}"))
        };
        for m in scenario.members.iter().filter(|m| m.blocked) {
            world.block(m.account);
        }

        // The chat, with times.
        let mut at = timestamp(&scenario.start)?;
        let mut lines = Vec::new();
        for (i, line) in scenario.chat.iter().enumerate() {
            if i > 0 {
                at = at.plus(Duration::from_secs(
                    60 * line.minutes.unwrap_or(1).max(0) as u64,
                ));
            }
            world.set_time(at);
            lines.push(match line.member {
                Some(n) => world.say(account_of(n)?.get(), n, &line.text),
                None => world.bot_says(&line.text),
            });
        }
        let now = Arc::new(Fixed(at.plus(Duration::from_secs(30))));
        world.set_time(now.now());

        // What the bot keeps about the group and its people.
        let memories = Arc::new(InMemoryMemoryStore::default());
        let mut episode = 0;
        let mut ids: Vec<MemoryId> = Vec::new();
        for (i, m) in scenario.memories.iter().enumerate() {
            let subject = m.member.map(account_of).transpose()?;
            let author = match (m.by, m.member) {
                (Some(by), _) | (None, Some(by)) => account_of(by)?,
                (None, None) => accounts.first().map(|(_, a)| *a).ok_or("no members")?,
            };
            let durability = m
                .durability
                .as_deref()
                .and_then(Durability::parse)
                .unwrap_or(Durability::Ongoing);
            let last = UnixMillis::new(now.now().get() - i64::from(m.days_ago) * 86_400_000);
            let mut id = None;
            // Each supporting conversation a week before the next, the last one `days_ago`.
            for k in (0..m.episodes.max(1)).rev() {
                episode += 1;
                let statement = Statement {
                    subject,
                    term: m.term.clone(),
                    text: m.text.clone(),
                    durability,
                    support: Support {
                        episode: EpisodeId::new(episode),
                        message: MessageId::new(episode).map_err(|e| e.to_string())?,
                        author: Some(author),
                        quote: m.text.clone(),
                        at: UnixMillis::new(last.get() - i64::from(k) * 7 * 86_400_000),
                    },
                };
                let change = match (m.replaces, id) {
                    (Some(r), None) => Change::Replace {
                        id: ids[r - 1],
                        with: statement,
                    },
                    _ => Change::New(statement),
                };
                id = memories
                    .apply(group, &[change])
                    .await
                    .map_err(|e| e.to_string())?
                    .first()
                    .copied();
            }
            ids.push(id.ok_or_else(|| format!("memory {} was not stored", i + 1))?);
        }
        let notes = Arc::new(MemoryNoteStore::default());
        for note in &scenario.notes {
            let about = account_of(note.member)?;
            notes
                .add(group, about, &note.text, about, now.now())
                .await
                .map_err(|e| e.to_string())?;
        }
        let name_store = MemoryNameStore::default();
        for n in &scenario.names {
            let about = account_of(n.member)?;
            match n.basis.as_deref().and_then(NameBasis::parse) {
                Some(NameBasis::Set) => name_store
                    .set(group, &n.name, &[about], now.now())
                    .await
                    .map_err(|e| e.to_string())?,
                basis => {
                    let basis = basis.unwrap_or(NameBasis::Addressed);
                    let uses: Vec<Reference> = (0..n.episodes.max(1))
                        .map(|k| {
                            episode += 1;
                            Ok(Reference {
                                episode: EpisodeId::new(episode),
                                message: MessageId::new(episode).map_err(|e| e.to_string())?,
                                ordinal: u64::try_from(episode).unwrap_or(0),
                                // Who said it plays no part in what a scenario is shown.
                                speaker: about,
                                text: n.name.clone(),
                                referent: Referent::Member(about),
                                basis,
                                grounds: match basis {
                                    NameBasis::Own => Grounds::Own,
                                    NameBasis::About => Grounds::Content,
                                    _ => Grounds::Marker,
                                },
                                anchored: false,
                                shown_by: Vec::new(),
                                at: UnixMillis::new(
                                    now.now().get() - i64::from(k) * 3 * 86_400_000,
                                ),
                            })
                        })
                        .collect::<Result<_, String>>()?;
                    name_store
                        .record(group, &uses)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        let people = scenario_people(scenario, &accounts, &name_store, group, now.now()).await?;
        let names = Arc::new(Names(
            scenario
                .members
                .iter()
                .filter(|m| !m.name_unavailable)
                .map(|m| account(m.account).map(|a| (a, m.name.clone())))
                .collect::<Result<_, _>>()?,
        ));
        let hits: Vec<SearchHit> = scenario
            .web
            .iter()
            .map(|h| SearchHit {
                title: h.title.clone(),
                url: h.url.clone(),
                content: h.content.clone(),
                published: None,
            })
            .collect();
        let search = Arc::new(
            FakeSearch::new((0..8).map(|_| Ok(SearchResults { hits: hits.clone() }))).with_pages(
                scenario
                    .pages
                    .iter()
                    .map(|p| Ok(PageRead::Read { content: p.clone() })),
            ),
        );

        // The production tool set over the simulated world.
        let clock: Arc<dyn Clock> = now.clone();
        let tasks = TaskService::new(
            Arc::new(MemoryTimerStore::new()),
            clock.clone(),
            TaskLimits::default(),
            Arc::new(Notify::new()),
            qbot_sched::Zone::get(TIMEZONE).map_err(|e| e.to_string())?,
        );
        let episodes = Arc::new(MemoryEpisodeStore::default());
        let recall = Arc::new(Recall::new(
            episodes.clone(),
            Arc::new(FakeEmbedder::new(64)),
            clock.clone(),
            RecallParams {
                limit: 5,
                max_distance: 0.8,
                ..RecallParams::default()
            },
        ));
        // The production prompt layer.
        let prompt = Arc::new(
            PromptContext::new(
                Arc::new(WorldHistory(world.clone(), accounts.clone())),
                self.personas.clone(),
                clock.clone(),
                PromptSettings {
                    window: qbot_core::HistoryWindow::default(),
                    timezone: TIMEZONE.to_owned(),
                },
            )
            .map_err(|e| e.to_string())?
            .with_people(Arc::new(ScenarioPeople(people)))
            // memory.group_memories and memory.person_memories by default
            .with_memories(
                memories.clone(),
                notes.clone(),
                MemoryView {
                    group_memories: 40,
                    person_memories: 8,
                    half_lives: Default::default(),
                },
            )
            .with_directory(names.clone()),
        );
        let tools = standard_tools(
            world.clone() as Arc<dyn Delivery>,
            world.clone() as Arc<dyn Archive>,
            tasks,
            ToolSettings {
                max_sends_per_run: 4,
                search: SearchSettings {
                    default_limit: 8,
                    max_limit: 50,
                },
            },
        )
        .and_then(|set| {
            add_memory_tools(
                set,
                recall,
                episodes,
                Some(prompt.clone() as Arc<dyn qbot_tools::MemberRecords>),
            )
        })
        .and_then(|set| set.with(WebSearchTool::new(search.clone())))
        .and_then(|set| set.with(ReadUrl::new(search, 8000)))
        .map_err(|e| e.to_string())?;

        let trigger = match (&scenario.trigger.line, &scenario.trigger.wake) {
            (Some(n), _) => {
                let line = &lines[n - 1];
                let Speaker::Member { account, .. } = line.speaker else {
                    return Err("the trigger line is not a member's".into());
                };
                Trigger::Addressed {
                    message: line.message,
                    sender: account,
                }
            }
            (None, Some(intent)) => Trigger::Wake {
                timer: TimerId::new(1),
                intent: intent.clone(),
                chain: Chain {
                    id: ChainId::new(1),
                    depth: 0,
                },
                repeats: false,
            },
            (None, None) if scenario.trigger.spontaneous => Trigger::Spontaneous,
            (None, None) => return Err("no trigger".into()),
        };
        let context = prompt
            .open(group, &trigger)
            .await
            .map_err(|e| e.to_string())?;
        let deps = RunDeps {
            provider: self.provider.clone(),
            tools,
            archive: world.clone(),
            log: world.clone() as Arc<dyn RunLog>,
            sink: MemorySink::new(),
            renderer: Arc::new(PromptRenderer::new(prompt.zone().clone())),
            clock,
            limits: RunLimits::default(),
            reasoning: self.reasoning,
            media: None,
        };
        let input = RunInput {
            run: RunId::new(1),
            group,
            trigger,
            context,
            deadline: tokio::time::Instant::now() + Duration::from_secs(300),
        };
        let report = execute(&deps, &input, &CancellationToken::new()).await;

        let calls = calls(report.transcript.items());
        let undelivered: Vec<String> = report
            .transcript
            .items()
            .iter()
            .filter_map(|item| match item {
                Item::Assistant(turn) => Some(turn.text()),
                _ => None,
            })
            .filter(|t| !t.trim().is_empty())
            .collect();
        let reminded = report.transcript.items().iter().any(
            |item| matches!(item, Item::Instruction(i) if i.role == InstructionRole::Developer),
        );
        let sent: Vec<String> = world
            .sent()
            .iter()
            .map(|s| render(s, scenario, &accounts))
            .collect();
        let checks = checks(scenario, &world.sent(), &calls, &accounts, report.end);
        let Usage {
            input_tokens,
            output_tokens,
            ..
        } = report.usage;
        Ok((
            RunResult {
                end: report.end.as_str().to_owned(),
                error: report.error.map(|e| e.to_string()),
                sent,
                calls,
                undelivered,
                reminded,
                checks,
                input_tokens,
                output_tokens,
            },
            world.lines(),
        ))
    }
}

fn calls(items: &[Item]) -> Vec<CallRecord> {
    let mut out: Vec<CallRecord> = Vec::new();
    for item in items {
        match item {
            Item::Assistant(turn) => {
                for part in turn.parts() {
                    if let AssistantPart::Call(call) = part {
                        out.push(CallRecord {
                            name: call.name.clone(),
                            arguments: call.arguments.to_string(),
                            outcome: String::new(),
                            result: call.id.as_str().to_owned(),
                        });
                    }
                }
            }
            Item::ToolResult(result) => {
                if let Some(record) = out
                    .iter_mut()
                    .find(|c| c.outcome.is_empty() && c.result == result.call_id.as_str())
                {
                    record.outcome = match &result.outcome {
                        Outcome::Ok => "ok".into(),
                        other => format!("{other:?}").to_lowercase(),
                    };
                    record.result = result
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            Part::Text(t) => Some(t.as_str()),
                            Part::Image { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                }
            }
            _ => {}
        }
    }
    out
}

fn checks(
    scenario: &Scenario,
    sent: &[Vec<OutSegment>],
    calls: &[CallRecord],
    accounts: &[(u32, AccountId)],
    end: RunEnd,
) -> Vec<Check> {
    let mut out = Vec::new();
    let errors = matches!(
        end,
        RunEnd::ModelError | RunEnd::Environment | RunEnd::Deadline | RunEnd::StepLimit
    );
    out.push(Check {
        what: "the run ended normally".into(),
        passed: !errors,
        detail: end.as_str().to_owned(),
    });
    let rule = scenario.expect.send;
    if rule != SendRule::Optional {
        let sent_any = !sent.is_empty();
        out.push(Check {
            what: match rule {
                SendRule::Required => "sends a message".into(),
                _ => "stays silent".into(),
            },
            passed: sent_any == (rule == SendRule::Required),
            detail: format!("{} message(s) sent", sent.len()),
        });
    }
    if let Some((from, to)) = scenario.expect.messages {
        let n = u32::try_from(sent.len()).unwrap_or(u32::MAX);
        out.push(Check {
            what: if from == to {
                format!("sends {from} message(s)")
            } else {
                format!("sends {from} to {to} messages")
            },
            passed: (from..=to).contains(&n),
            detail: format!("{n} sent"),
        });
    }
    if let Some(max) = scenario.expect.send_calls_at_most {
        let n = calls.iter().filter(|c| c.name == "send_messages").count();
        out.push(Check {
            what: format!("sends in at most {max} send_messages call(s)"),
            passed: u32::try_from(n).is_ok_and(|n| n <= max),
            detail: format!("{n} call(s)"),
        });
    }
    let called = |name: &str| calls.iter().any(|c| c.name == name);
    for tool in &scenario.expect.tools_required {
        out.push(Check {
            what: format!("calls {tool}"),
            passed: called(tool),
            detail: String::new(),
        });
    }
    for tool in &scenario.expect.tools_forbidden {
        out.push(Check {
            what: format!("does not call {tool}"),
            passed: !called(tool),
            detail: String::new(),
        });
    }
    if !scenario.expect.member_handles_allowed {
        let handles: Vec<&str> = sent
            .iter()
            .flatten()
            .filter_map(|s| match s {
                OutSegment::Text(text) if has_member_handle(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        out.push(Check {
            what: "names people instead of writing member:N".into(),
            passed: handles.is_empty(),
            detail: handles.join(" | "),
        });
    }
    let copied: Vec<String> = sent
        .iter()
        .flatten()
        .flat_map(|s| match s {
            OutSegment::Text(text) => read_only_markers(text),
            _ => Vec::new(),
        })
        .collect();
    out.push(Check {
        what: "writes no receive-only marker".into(),
        passed: copied.is_empty(),
        detail: copied.join(", "),
    });
    for n in &scenario.expect.mentions_forbidden {
        let Some((_, who)) = accounts.iter().find(|(x, _)| x == n) else {
            continue;
        };
        let named = sent
            .iter()
            .flatten()
            .any(|s| matches!(s, OutSegment::At(a) | OutSegment::Contact(a) if a == who));
        out.push(Check {
            what: format!("does not mention member {n}"),
            passed: !named,
            detail: String::new(),
        });
    }
    out
}
