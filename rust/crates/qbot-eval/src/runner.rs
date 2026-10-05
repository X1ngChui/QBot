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
use qbot_context::{AssistantPart, ChatLine, Item, Outcome, Part, RunEnd, Speaker};
use qbot_core::{AccountId, ChainId, Clock, GroupId, MessageId, RunId, TimerId, UnixMillis};
use qbot_llm::embedding::FakeEmbedder;
use qbot_llm::search::{FakeSearch, PageRead, SearchHit, SearchResults};
use qbot_llm::{Params, Provider, ReasoningEffort, Usage};
use qbot_memory::facts::{
    DecayPolicy, FactStore, GROUP_TERM, GROUP_TOPIC, MemoryFactStore, Observation, normalize_key,
};
use qbot_memory::identity::IdentityPolicy;
use qbot_memory::predicates::{Cardinality, DecayClass, Predicates};
use qbot_memory::{
    EpisodeId, IdentityStore, MemoryEpisodeStore, MemoryIdentityStore, MemoryNoteStore, NoteStore,
    Recall, RecallParams,
};
use qbot_prompt::{
    FactKnowledge, HistorySource, Personas, PromptContext, PromptRenderer, PromptSettings,
};
use qbot_sched::{MemoryTimerStore, TaskLimits, TaskService};
use qbot_tools::{
    LookupMember, ReadUrl, SearchSettings, ToolSettings, WebSearchTool, add_memory_tools,
    standard_tools,
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
    async fn display_name(&self, _: GroupId, account: AccountId) -> Option<String> {
        self.0
            .iter()
            .find(|(a, _)| *a == account)
            .map(|(_, n)| n.clone())
    }
}

/// The simulated archive as the prompt layer reads it: every line, ordinals from 1.
struct WorldHistory(Arc<SimWorld>);

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

pub struct Runner {
    pub provider: Arc<dyn Provider>,
    pub personas: Personas,
    pub params: Params,
}

impl Runner {
    pub fn default_params() -> Params {
        // The production defaults (`[agent]`).
        Params {
            max_output_tokens: 4096,
            reasoning: ReasoningEffort::Low,
            temperature: None,
        }
    }

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

        // What the bot may look up.
        let identity = Arc::new(MemoryIdentityStore::new(IdentityPolicy::default()));
        for (_, a) in &accounts {
            identity
                .seen(*a, now.now())
                .await
                .map_err(|e| e.to_string())?;
        }
        let facts = Arc::new(MemoryFactStore::new());
        let predicates = Predicates::builtin();
        let mut episode = 0;
        for f in &scenario.facts {
            let predicate = predicates
                .get(&f.predicate)
                .ok_or_else(|| format!("unknown predicate {}", f.predicate))?;
            for _ in 0..f.episodes.max(1) {
                episode += 1;
                facts
                    .observe(&Observation {
                        group,
                        subject: Some(account_of(f.member)?),
                        predicate: f.predicate.clone(),
                        key: match predicate.cardinality {
                            Cardinality::Single => String::new(),
                            Cardinality::Multi => normalize_key(&f.object),
                        },
                        object: f.object.clone(),
                        label: None,
                        opposite: predicate.opposite.clone(),
                        decay: predicate.decay,
                        episode: EpisodeId::new(episode),
                        message: MessageId::new(episode).map_err(|e| e.to_string())?,
                        quote: f.object.clone(),
                        at: now.now(),
                    })
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        for k in &scenario.knowledge {
            episode += 1;
            let (predicate, key, label) = match &k.term {
                Some(term) => (GROUP_TERM, normalize_key(term), Some(term.clone())),
                None => (GROUP_TOPIC, String::new(), None),
            };
            facts
                .observe(&Observation {
                    group,
                    subject: None,
                    predicate: predicate.to_owned(),
                    key,
                    object: k.text.clone(),
                    label,
                    opposite: None,
                    decay: DecayClass::Stable,
                    episode: EpisodeId::new(episode),
                    message: MessageId::new(episode).map_err(|e| e.to_string())?,
                    quote: k.text.clone(),
                    at: now.now(),
                })
                .await
                .map_err(|e| e.to_string())?;
        }
        let notes = Arc::new(MemoryNoteStore::default());
        for n in &scenario.notes {
            let who = account_of(n.member)?;
            notes
                .add(group, who, &n.text, who, now.now())
                .await
                .map_err(|e| e.to_string())?;
        }
        let names = Arc::new(Names(
            scenario
                .members
                .iter()
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
            FakeSearch::new((0..8).map(|_| {
                Ok(SearchResults {
                    hits: hits.clone(),
                    credits: Some(1),
                    requests: 1,
                })
            }))
            .with_pages(scenario.pages.iter().map(|p| {
                Ok(PageRead::Read {
                    content: p.clone(),
                    credits: Some(1),
                })
            })),
        );

        // The production tool set over the simulated world.
        let clock: Arc<dyn Clock> = now.clone();
        let tasks = TaskService::new(
            Arc::new(MemoryTimerStore::new()),
            clock.clone(),
            TaskLimits::default(),
            Arc::new(Notify::new()),
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
        .and_then(|set| add_memory_tools(set, recall, episodes))
        .and_then(|set| {
            set.with(LookupMember {
                directory: names,
                identity: identity.clone(),
                facts: facts.clone(),
                notes: notes.clone(),
                decay: DecayPolicy::default(),
            })
        })
        .and_then(|set| set.with(WebSearchTool::new(search.clone())))
        .and_then(|set| set.with(ReadUrl::new(search, 8000)))
        .map_err(|e| e.to_string())?;

        // The production prompt layer.
        let prompt = PromptContext::new(
            Arc::new(WorldHistory(world.clone())),
            self.personas.clone(),
            clock.clone(),
            PromptSettings {
                window: qbot_core::HistoryWindow::default(),
                timezone: TIMEZONE.to_owned(),
            },
        )
        .map_err(|e| e.to_string())?
        .with_knowledge(Arc::new(FactKnowledge::new(
            facts.clone(),
            FactKnowledge::DEFAULT_MAX_TERMS,
        )));
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
            },
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
            params: self.params,
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
