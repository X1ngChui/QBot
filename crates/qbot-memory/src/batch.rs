//! The model calls of a group's batch.
//!
//! Once a day each group's chat since its previous batch is read in one sitting. Every call of a
//! group's batch begins with the same messages: the instructions (one set for every task, with
//! every task's tool declared), the group as it was before the batch, and the lines read. Only
//! the last message, the task, differs, so the provider's prefix cache serves all of a group's
//! calls after the first, and every group's calls share the instructions.
//!
//! The tasks: divide the lines into episodes, find the references and memories of each episode,
//! rule on names nobody could place, and read an earlier stretch again. Code checks each answer
//! against what it owns (episode boundaries that hold every line once and in order, quotes copied
//! from the lines, the rules of [`crate::findings`]) and sends a rejected answer back with the
//! reasons, a bounded number of times.

use std::fmt::Write as _;
use std::sync::Arc;

use qbot_context::{AssistantPart, AssistantTurn, ToolCall};
use qbot_core::{CallId, UnixMillis};
use qbot_llm::{
    Content, ConvItem, Conversation, FinishReason, LlmError, Message, Provider, ReasoningEffort,
    Request, Role, ToolChoice, ToolOutput, ToolSpec, ToolStatus, Usage,
};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::episode::Evidence;
use crate::findings::{Checked, FindingContext, MemoryArg, ReferenceArg, validate_findings};
use crate::names::{Ruling, name_key};
use crate::rule::{RulingArgs, RulingCase, render as render_case, ruling};
use crate::slice::{Background, SliceLine};

/// Version of the prompt and validation rules, recorded on every episode.
pub const METHOD: &str = "batch-v1";

/// Model calls for one task: the first answer and up to two corrections or retries. Bounds a
/// model that never produces a valid answer, so one bad stretch cannot stall a group.
pub const MAX_ATTEMPTS: u32 = 3;

pub const EPISODES_TOOL: &str = "submit_episodes";
pub const FINDINGS_TOOL: &str = "submit_findings";
pub const RULING_TOOL: &str = "submit_ruling";

/// The instructions; `{language}` is filled per reader.
const SYSTEM: &str = include_str!("../../../prompts/batch.md");

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EpisodesArgs {
    pub episodes: Vec<EpisodeArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EpisodeArg {
    pub first_line: u32,
    pub last_line: u32,
    pub title: String,
    pub summary: String,
    pub evidence: Vec<EvidenceArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvidenceArg {
    pub line: u32,
    pub quote: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindingsArgs {
    #[serde(default)]
    pub references: Vec<ReferenceArg>,
    #[serde(default)]
    pub memories: Vec<MemoryArg>,
}

/// What a group's batch reads: the lines since its previous batch, numbered from 1.
#[derive(Debug, Clone, Copy)]
pub struct Stretch<'a> {
    /// The lines just before, unnumbered: what the chat continues.
    pub previous: &'a [SliceLine],
    /// The lines read.
    pub lines: &'a [SliceLine],
    /// Who the people are and what the group is, as of before the batch.
    pub background: &'a Background,
}

/// An earlier episode's lines, read again: numbered from 1, with context around them.
#[derive(Debug, Clone, Copy)]
pub struct Earlier<'a> {
    pub previous: &'a [SliceLine],
    pub target: &'a [SliceLine],
    pub next: &'a [SliceLine],
    /// Older lines these quote.
    pub quoted: &'a [SliceLine],
}

/// An episode as the model divided the stretch: lines `first..=last`, numbered from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub first: u32,
    pub last: u32,
    pub title: String,
    pub summary: String,
    pub evidence: Vec<Evidence>,
}

/// An accepted answer and what getting it cost.
#[derive(Debug, Clone)]
pub struct Answer<T> {
    pub value: T,
    pub usage: Usage,
    pub model: String,
    pub attempts: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("model error: {0}")]
    Model(#[from] LlmError),
    #[error("the answer was cut off at the provider's output limit on every attempt")]
    CutOff,
    #[error("the model gave no valid answer after {attempts} attempts: {problems:?}")]
    Invalid {
        attempts: u32,
        problems: Vec<String>,
    },
}

impl BatchError {
    /// Whether asking again would only spend calls: the provider refuses the content or finds
    /// it too long, or the model gave no valid answer in any attempt. Account, network and
    /// provider-side failures pass with time and are not.
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            BatchError::CutOff
                | BatchError::Invalid { .. }
                | BatchError::Model(LlmError::ContentFiltered | LlmError::ContextTooLong { .. })
        )
    }
}

/// How an answer's arguments fared.
enum Check<T> {
    Accept(T),
    /// Usable, but worth one correction: kept if the correction does not come back valid.
    Correct(T, Vec<String>),
    Reject(Vec<String>),
}

pub struct BatchReader {
    provider: Arc<dyn Provider>,
    reasoning: ReasoningEffort,
    language: String,
    tools: Vec<ToolSpec>,
}

impl std::fmt::Debug for BatchReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchReader").finish_non_exhaustive()
    }
}

/// Every task's tool, the same for every call, so the declared tools are part of the shared
/// prefix.
fn tools() -> Vec<ToolSpec> {
    let spec = |name: &str, description: String, schema: Value| ToolSpec {
        name: name.to_owned(),
        description,
        schema,
    };
    vec![
        spec(
            EPISODES_TOOL,
            say(Text::BatchEpisodesTool {}),
            qbot_llm::schema::tool_schema::<EpisodesArgs>(episode_parameters()),
        ),
        spec(
            FINDINGS_TOOL,
            say(Text::BatchFindingsTool {}),
            qbot_llm::schema::tool_schema::<FindingsArgs>(finding_parameters()),
        ),
        spec(
            RULING_TOOL,
            say(Text::RuleToolDescription {}),
            qbot_llm::schema::tool_schema::<RulingArgs>(vec![
                ("verdict", say(Text::RuleParamVerdict {})),
                ("lines", say(Text::RuleParamLines {})),
                ("reason", say(Text::RuleParamReason {})),
            ]),
        ),
    ]
}

fn episode_parameters() -> Vec<(&'static str, String)> {
    vec![
        ("episodes", say(Text::BatchParamEpisodes {})),
        ("episodes.first_line", say(Text::BatchParamFirstLine {})),
        ("episodes.last_line", say(Text::BatchParamLastLine {})),
        ("episodes.title", say(Text::BatchParamTitle {})),
        ("episodes.summary", say(Text::BatchParamSummary {})),
        ("episodes.evidence", say(Text::BatchParamEvidence {})),
        (
            "episodes.evidence.line",
            say(Text::BatchParamEvidenceLine {}),
        ),
        (
            "episodes.evidence.quote",
            say(Text::BatchParamEvidenceQuote {}),
        ),
    ]
}

fn finding_parameters() -> Vec<(&'static str, String)> {
    vec![
        ("references", say(Text::ExtractParamReferences {})),
        ("references.line", say(Text::ExtractParamReferenceLine {})),
        ("references.text", say(Text::ExtractParamReferenceText {})),
        (
            "references.refers",
            say(Text::ExtractParamReferenceRefers {}),
        ),
        ("references.role", say(Text::ExtractParamReferenceRole {})),
        ("references.basis", say(Text::ExtractParamReferenceBasis {})),
        (
            "references.shown_by",
            say(Text::ExtractParamReferenceShownBy {}),
        ),
        ("memories", say(Text::ExtractParamMemories {})),
        ("memories.about", say(Text::ExtractParamMemoryAbout {})),
        ("memories.action", say(Text::ExtractParamMemoryAction {})),
        ("memories.id", say(Text::ExtractParamMemoryId {})),
        ("memories.term", say(Text::ExtractParamMemoryTerm {})),
        ("memories.text", say(Text::ExtractParamMemoryText {})),
        (
            "memories.durability",
            say(Text::ExtractParamMemoryDurability {}),
        ),
        ("memories.line", say(Text::ExtractParamLine {})),
        ("memories.quote", say(Text::ExtractParamQuote {})),
    ]
}

fn user(text: String) -> ConvItem {
    ConvItem::Message(Message {
        role: Role::User,
        content: vec![Content::Text(text)],
    })
}

/// Text written from chat, as it may stand in the prompt: marker-escaped, on one line.
fn one_line(text: &str) -> String {
    qbot_core::marker::escape_markers(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// A full local date and time with the weekday, so the model can turn "tomorrow" into a date.
fn full_time(zone: &jiff::tz::TimeZone, at: UnixMillis) -> String {
    jiff::Timestamp::from_millisecond(at.get())
        .map(|t| {
            t.to_zoned(zone.clone())
                .strftime("%Y-%m-%d %a %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// The zone's name where it has one, and its offset at `at` in any case.
fn zone_label(zone: &jiff::tz::TimeZone, at: UnixMillis) -> String {
    let offset = jiff::Timestamp::from_millisecond(at.get())
        .map(|t| t.to_zoned(zone.clone()).strftime("UTC%:z").to_string())
        .unwrap_or_default();
    match zone.iana_name() {
        Some(name) => format!("{name}, {offset}"),
        None => offset,
    }
}

/// `lines` numbered from 1 under `heading` (which is given the span and zone), with unnumbered
/// context before and after.
fn numbered(
    out: &mut String,
    zone: &jiff::tz::TimeZone,
    previous: &[SliceLine],
    lines: &[SliceLine],
    next: &[SliceLine],
    heading: impl Fn(String, String, String) -> String,
) {
    if !previous.is_empty() {
        let _ = writeln!(out, "{}", say(Text::ExtractContextBefore {}));
        for line in previous {
            let _ = writeln!(out, "{}", line.render(zone));
        }
        out.push('\n');
    }
    if let (Some(first), Some(last)) = (lines.first(), lines.last()) {
        let _ = writeln!(
            out,
            "{}",
            heading(
                full_time(zone, first.at),
                full_time(zone, last.at),
                zone_label(zone, first.at)
            )
        );
    }
    for (i, line) in lines.iter().enumerate() {
        let _ = writeln!(out, "#{} {}", i + 1, line.render(zone));
    }
    if !next.is_empty() {
        let _ = writeln!(out, "\n{}", say(Text::ExtractContextAfter {}));
        for line in next {
            let _ = writeln!(out, "{}", line.render(zone));
        }
    }
}

/// Today's lines as the model reads them: what the chat was about before, lines quoted from
/// further back, the lines just before, then the lines read, numbered.
pub fn render_stretch(stretch: &Stretch<'_>) -> String {
    let background = stretch.background;
    let zone = &background.zone;
    let mut out = String::new();
    if let Some((title, summary)) = &background.earlier {
        let _ = writeln!(
            out,
            "{}\n",
            // Written from chat: escaped like it, and on one line so it opens no section.
            say(Text::ExtractEarlier {
                title: one_line(title),
                summary: one_line(summary),
            })
        );
    }
    if !background.quoted.is_empty() {
        let _ = writeln!(out, "{}", say(Text::ExtractQuoted {}));
        for line in &background.quoted {
            let _ = writeln!(out, "{}", line.render(zone));
        }
        out.push('\n');
    }
    if stretch.lines.is_empty() {
        let _ = writeln!(out, "{}", say(Text::BatchNoLines {}));
        return out;
    }
    numbered(
        &mut out,
        zone,
        stretch.previous,
        stretch.lines,
        &[],
        |from, to, zone| say(Text::BatchStretch { from, to, zone }),
    );
    out
}

/// Check the episodes against the lines: every line in exactly one episode, in order, and each
/// episode titled, summarized and grounded in quotes from its own lines. Returns every problem,
/// so one correction can fix all.
pub fn check_episodes(
    args: &EpisodesArgs,
    lines: &[SliceLine],
) -> Result<Vec<Planned>, Vec<String>> {
    let total = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    let mut problems = Vec::new();
    if args.episodes.is_empty() {
        return Err(vec![say(Text::BatchEpisodesNone {})]);
    }
    let mut planned = Vec::new();
    // The first line no episode holds yet.
    let mut next = 1u32;
    for (index, episode) in args.episodes.iter().enumerate() {
        let mut found = Vec::new();
        let (first, last) = (episode.first_line, episode.last_line);
        if first != next {
            found.push(say(Text::BatchEpisodeStart {
                first: first.to_string(),
                expected: next.to_string(),
            }));
        }
        if last < first {
            found.push(say(Text::BatchEpisodeReversed {}));
        } else if last > total {
            found.push(say(Text::BatchEpisodeBeyond {
                last: last.to_string(),
                max: total.to_string(),
            }));
        }
        if episode.title.trim().is_empty() {
            found.push(say(Text::ExtractEmptyTitle {}));
        }
        if episode.summary.trim().is_empty() {
            found.push(say(Text::ExtractEmptySummary {}));
        }
        if !(1..=3).contains(&episode.evidence.len()) {
            found.push(say(Text::ExtractEvidenceCount {
                count: episode.evidence.len().to_string(),
            }));
        }
        let mut evidence = Vec::new();
        for quote in &episode.evidence {
            if quote.line < first || quote.line > last {
                found.push(say(Text::BatchEpisodeEvidenceOutside {
                    line: quote.line.to_string(),
                    first: first.to_string(),
                    last: last.to_string(),
                }));
                continue;
            }
            match lines.get((quote.line as usize).wrapping_sub(1)) {
                Some(line)
                    if !quote.quote.trim().is_empty() && line.text.contains(quote.quote.trim()) =>
                {
                    evidence.push(Evidence {
                        message: line.message,
                        quote: quote.quote.trim().to_owned(),
                    });
                }
                _ => found.push(say(Text::ExtractEvidenceQuote {
                    line: quote.line.to_string(),
                })),
            }
        }
        if found.is_empty() {
            planned.push(Planned {
                first,
                last,
                title: episode.title.trim().to_owned(),
                summary: episode.summary.trim().to_owned(),
                evidence,
            });
        }
        problems.extend(found.into_iter().map(|problem| {
            say(Text::BatchEpisodeProblem {
                index: (index + 1).to_string(),
                problem,
            })
        }));
        next = next.max(last.saturating_add(1));
    }
    if next <= total {
        problems.push(say(Text::BatchEpisodesUncovered {
            from: next.to_string(),
            to: total.to_string(),
        }));
    }
    if problems.is_empty() {
        Ok(planned)
    } else {
        Err(problems)
    }
}

/// Validate findings, asking for one correction when statements were dropped: they are
/// optional, but worth fixing. A reference left out is one record among many and is not worth
/// another call by itself.
fn findings_check(arguments: Value, cx: &FindingContext<'_>) -> Check<Checked> {
    match serde_json::from_value::<FindingsArgs>(arguments) {
        Err(error) => Check::Reject(vec![say(Text::ExtractBadArguments {
            error: error.to_string(),
        })]),
        Ok(args) => {
            let checked = validate_findings(&args.references, &args.memories, cx);
            if checked.dropped.is_empty() {
                Check::Accept(checked)
            } else {
                let mut problems = checked.dropped.clone();
                problems.extend(checked.unread.iter().cloned());
                Check::Correct(checked, problems)
            }
        }
    }
}

impl BatchReader {
    /// A reader writing in `language` (the bot's writing language).
    pub fn new(provider: Arc<dyn Provider>, language: impl Into<String>) -> Self {
        Self {
            provider,
            reasoning: ReasoningEffort::Low,
            language: language.into(),
            tools: tools(),
        }
    }

    /// How much the model reasons before answering (default low).
    pub fn with_reasoning(mut self, reasoning: ReasoningEffort) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// The instructions, in the writing language.
    pub fn instructions(&self) -> String {
        SYSTEM.trim_end().replace("{language}", &self.language)
    }

    /// The messages every call of a group's batch begins with.
    pub fn prefix(&self, stretch: &Stretch<'_>) -> Vec<ConvItem> {
        let state = stretch.background.text.trim();
        vec![
            ConvItem::Message(Message {
                role: Role::System,
                content: vec![Content::Text(self.instructions())],
            }),
            user(if state.is_empty() {
                say(Text::BatchNoState {})
            } else {
                state.to_owned()
            }),
            user(render_stretch(stretch)),
        ]
    }

    /// Divide the stretch into episodes.
    pub async fn episodes(
        &self,
        prefix: &[ConvItem],
        stretch: &Stretch<'_>,
    ) -> Result<Answer<Vec<Planned>>, BatchError> {
        let task = say(Text::BatchTaskEpisodes {
            last: stretch.lines.len().to_string(),
        });
        self.ask(
            prefix,
            task,
            EPISODES_TOOL,
            |arguments| match serde_json::from_value::<EpisodesArgs>(arguments) {
                Err(error) => Check::Reject(vec![say(Text::ExtractBadArguments {
                    error: error.to_string(),
                })]),
                Ok(args) => match check_episodes(&args, stretch.lines) {
                    Ok(planned) => Check::Accept(planned),
                    Err(problems) => Check::Reject(problems),
                },
            },
        )
        .await
    }

    /// The references and memories of one planned episode (the `index`th, from 1).
    pub async fn findings(
        &self,
        prefix: &[ConvItem],
        stretch: &Stretch<'_>,
        planned: &Planned,
        index: usize,
    ) -> Result<Answer<Checked>, BatchError> {
        let lines = stretch.lines;
        let (from, to) = (planned.first as usize - 1, planned.last as usize);
        let before: Vec<SliceLine> = stretch
            .previous
            .iter()
            .chain(&lines[..from])
            .cloned()
            .collect();
        let bot_names = bot_keys(stretch.background);
        let background = stretch.background;
        let cx = FindingContext {
            parts: [&before, &lines[from..to], &lines[to..]],
            first: planned.first,
            quoted: &background.quoted,
            accounts: &background.accounts,
            known: &background.known,
            bot_names: &bot_names,
        };
        let task = say(Text::BatchTaskFindings {
            index: index.to_string(),
            title: one_line(&planned.title),
            first: planned.first.to_string(),
            last: planned.last.to_string(),
        });
        self.ask(prefix, task, FINDINGS_TOOL, |arguments| {
            findings_check(arguments, &cx)
        })
        .await
    }

    /// The references and memories of an earlier episode's lines, read again.
    pub async fn reread(
        &self,
        prefix: &[ConvItem],
        background: &Background,
        earlier: &Earlier<'_>,
    ) -> Result<Answer<Checked>, BatchError> {
        let zone = &background.zone;
        let mut lines = String::new();
        if !earlier.quoted.is_empty() {
            let _ = writeln!(lines, "{}", say(Text::ExtractQuoted {}));
            for line in earlier.quoted {
                let _ = writeln!(lines, "{}", line.render(zone));
            }
            lines.push('\n');
        }
        numbered(
            &mut lines,
            zone,
            earlier.previous,
            earlier.target,
            earlier.next,
            |from, to, zone| say(Text::BatchEarlierStretch { from, to, zone }),
        );
        let bot_names = bot_keys(background);
        let cx = FindingContext {
            parts: [earlier.previous, earlier.target, earlier.next],
            first: 1,
            quoted: earlier.quoted,
            accounts: &background.accounts,
            known: &background.known,
            bot_names: &bot_names,
        };
        let task = say(Text::BatchTaskReread {
            last: earlier.target.len().to_string(),
            lines: lines.trim_end().to_owned(),
        });
        self.ask(prefix, task, FINDINGS_TOOL, |arguments| {
            findings_check(arguments, &cx)
        })
        .await
    }

    /// Rule on a name nobody could place; the ruling is dated `at`.
    pub async fn ruling(
        &self,
        prefix: &[ConvItem],
        case: &RulingCase<'_>,
        at: UnixMillis,
    ) -> Result<Answer<Ruling>, BatchError> {
        let task = say(Text::BatchTaskRuling {
            case: render_case(case).trim_end().to_owned(),
        });
        self.ask(
            prefix,
            task,
            RULING_TOOL,
            |arguments| match serde_json::from_value::<RulingArgs>(arguments) {
                Ok(args) => Check::Accept(ruling(case, &args, at)),
                Err(error) => Check::Reject(vec![say(Text::ExtractBadArguments {
                    error: error.to_string(),
                })]),
            },
        )
        .await
    }

    /// Ask for `task` after `prefix`, to be answered with one call of `tool`, and check the
    /// answer. A rejected answer goes back in the same exchange with the reasons, so the prefix
    /// stays shared; an answer cut off at the output limit is lost, not wrong, and the same
    /// request is sent again.
    async fn ask<T>(
        &self,
        prefix: &[ConvItem],
        task: String,
        tool: &str,
        mut check: impl FnMut(Value) -> Check<T>,
    ) -> Result<Answer<T>, BatchError> {
        let mut items = prefix.to_vec();
        items.push(user(task));
        let mut usage = Usage::ZERO;
        let mut model = String::new();
        let mut problems: Vec<String> = Vec::new();
        // An answer usable as it is, sent back once for a correction.
        let mut kept: Option<T> = None;
        let done = |value, usage, model, attempts| Answer {
            value,
            usage,
            model,
            attempts,
        };
        for attempt in 1..=MAX_ATTEMPTS {
            let conversation = Conversation::new(items.clone());
            let response = match self
                .provider
                .respond(Request {
                    conversation: &conversation,
                    tools: &self.tools,
                    tool_choice: ToolChoice::Auto,
                    parallel_tool_calls: false,
                    reasoning: self.reasoning,
                    media: None,
                })
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    return match kept {
                        Some(value) => {
                            tracing::warn!(%error, "the correction failed; keeping the first answer");
                            Ok(done(value, usage, model, attempt - 1))
                        }
                        None => Err(error.into()),
                    };
                }
            };
            usage += response.usage;
            model = response.meta.model.clone();
            let calls: Vec<ToolCall> = response.calls().cloned().collect();
            if calls.is_empty() && response.finish == FinishReason::Length {
                if attempt < MAX_ATTEMPTS {
                    tracing::warn!(attempt, tool, "batch answer cut off; asking again");
                    continue;
                }
                return match kept {
                    Some(value) => Ok(done(value, usage, model, attempt)),
                    None => Err(BatchError::CutOff),
                };
            }
            let mut correcting = false;
            match calls.as_slice() {
                [] => {
                    problems = vec![say(Text::ExtractNoCall {
                        tool: tool.to_owned(),
                    })];
                }
                [call] if call.name == tool => match check(call.arguments.clone()) {
                    Check::Accept(value) => return Ok(done(value, usage, model, attempt)),
                    Check::Correct(value, found) if kept.is_none() && attempt < MAX_ATTEMPTS => {
                        kept = Some(value);
                        problems = found;
                        correcting = true;
                    }
                    Check::Correct(value, _) => return Ok(done(value, usage, model, attempt)),
                    Check::Reject(found) => problems = found,
                },
                _ => {
                    problems = vec![say(Text::ExtractCallOnce {
                        tool: tool.to_owned(),
                    })];
                }
            }
            tracing::info!(
                attempt,
                tool,
                ?problems,
                "batch answer sent back for correction"
            );
            items.push(ConvItem::Assistant(AssistantTurn::new(
                response
                    .turn
                    .parts()
                    .iter()
                    .filter(|p| !matches!(p, AssistantPart::Reasoning(_)))
                    .cloned()
                    .collect::<Vec<_>>(),
            )));
            let listed = problems
                .iter()
                .map(|problem| {
                    say(Text::ExtractProblem {
                        problem: problem.clone(),
                    })
                })
                .collect::<Vec<_>>()
                .join("\n");
            let reasons = say(if correcting {
                Text::ExtractFindingsRejected {
                    problems: listed,
                    tool: tool.to_owned(),
                }
            } else {
                Text::ExtractRejected {
                    problems: listed,
                    tool: tool.to_owned(),
                }
            });
            if calls.is_empty() {
                items.push(user(reasons));
            } else {
                let ids: Vec<CallId> = calls.iter().map(|c| c.id.clone()).collect();
                for call_id in ids {
                    items.push(ConvItem::ToolResult(ToolOutput {
                        call_id,
                        status: ToolStatus::Refused,
                        content: vec![Content::Text(reasons.clone())],
                    }));
                }
            }
        }
        match kept {
            Some(value) => Ok(done(value, usage, model, MAX_ATTEMPTS)),
            None => Err(BatchError::Invalid {
                attempts: MAX_ATTEMPTS,
                problems,
            }),
        }
    }
}

/// The bot's names, normalized: never a member's name.
fn bot_keys(background: &Background) -> Vec<String> {
    background
        .bot_names
        .iter()
        .filter_map(|n| name_key(n).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instructions_have_only_the_language_slot_and_every_tool_is_described() {
        let slots: Vec<&str> = SYSTEM
            .match_indices('{')
            .map(|(at, _)| {
                let rest = &SYSTEM[at + 1..];
                &rest[..rest.find('}').unwrap_or(0)]
            })
            .collect();
        assert!(slots.iter().all(|s| *s == "language"), "{slots:?}");
        let reader = BatchReader::new(Arc::new(qbot_llm::fake::FakeProvider::new([])), "English");
        let text = reader.instructions();
        assert!(!text.contains("{language}"), "every slot is filled");
        for spec in tools() {
            assert_eq!(
                qbot_llm::schema::undescribed(&spec.schema),
                Vec::<String>::new(),
                "{}",
                spec.name
            );
        }
    }
}
