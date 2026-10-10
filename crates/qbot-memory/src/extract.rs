//! Summarizing one slice with a model.
//!
//! The model summarizes the target slice and may read the neighboring batches only to interpret
//! references that cross the boundary. Code validates the answer: it needs a title, a summary,
//! and verbatim quotes taken from the target slice itself. A rejected answer goes back to the
//! model with the reasons, a bounded number of times.

use std::fmt::Write as _;
use std::sync::Arc;

use qbot_context::{AssistantPart, AssistantTurn};
use qbot_core::CallId;
use qbot_llm::{
    Content, ConvItem, Conversation, FinishReason, LlmError, Message, Provider, ReasoningEffort,
    Request, Role, ToolChoice, ToolOutput, ToolSpec, ToolStatus, Usage,
};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::episode::Evidence;
use crate::findings::{FactArg, Findings, KnowledgeArg, NameArg, validate_findings};
use crate::predicates::Predicates;
use crate::slice::{Background, SliceLine};

pub const TOOL_NAME: &str = "submit_episode";
/// Version of the prompt and validation rules, recorded on every episode.
pub const METHOD: &str = "slice-v3";

/// The extraction instructions; `{language}` and `{predicates}` are filled per call.
const SYSTEM: &str = include_str!("../../../prompts/extract.md");

/// Model calls for one slice: the first answer and up to two corrections or retries. Bounds a
/// model that never produces a valid answer, so one bad slice cannot stall a group.
pub const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmitArgs {
    pub title: String,
    pub summary: String,
    pub evidence: Vec<EvidenceArg>,
    #[serde(default)]
    pub names: Vec<NameArg>,
    #[serde(default)]
    pub facts: Vec<FactArg>,
    #[serde(default)]
    pub knowledge: Vec<KnowledgeArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvidenceArg {
    pub line: u32,
    pub quote: String,
}

/// The slice to summarize and its neighbors.
#[derive(Debug, Clone, Copy)]
pub struct SliceContext<'a> {
    /// The nearest batch before the slice. Context only.
    pub previous: &'a [SliceLine],
    /// The slice. The episode's source range.
    pub target: &'a [SliceLine],
    /// The nearest batch after the slice. Context only.
    pub next: &'a [SliceLine],
    /// Who the people in the lines are and what the group is.
    pub background: &'a Background,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Extracted {
    pub title: String,
    pub summary: String,
    pub evidence: Vec<Evidence>,
    pub findings: Findings,
    /// Why proposed findings were not kept, for the log.
    pub dropped: Vec<String>,
    pub usage: Usage,
    pub attempts: u32,
    pub model: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
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

pub struct EpisodeExtractor {
    provider: Arc<dyn Provider>,
    predicates: Arc<Predicates>,
    reasoning: ReasoningEffort,
}

impl std::fmt::Debug for EpisodeExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpisodeExtractor").finish_non_exhaustive()
    }
}

impl EpisodeExtractor {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            predicates: Arc::new(Predicates::builtin()),
            reasoning: ReasoningEffort::Low,
        }
    }

    /// How much the model reasons before answering (default low).
    pub fn with_reasoning(mut self, reasoning: ReasoningEffort) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// The instructions, with the predicate table spelled out.
    pub fn instructions(&self, language: &str) -> String {
        let predicates: Vec<String> = self
            .predicates
            .iter()
            .map(|p| {
                say(Text::ExtractPredicate {
                    name: p.name.to_string(),
                    cardinality: say(if p.cardinality == crate::predicates::Cardinality::Single {
                        Text::ExtractOneValue {}
                    } else {
                        Text::ExtractSeveralValues {}
                    }),
                    rule: p.rule.to_string(),
                })
            })
            .collect();
        SYSTEM
            .trim_end()
            .replace("{language}", language)
            .replace("{predicates}", &predicates.join("\n"))
    }

    pub async fn extract(
        &self,
        ctx: &SliceContext<'_>,
        language: &str,
    ) -> Result<Extracted, ExtractError> {
        let tools = [ToolSpec {
            name: TOOL_NAME.to_owned(),
            description: say(Text::ExtractToolDescription {}),
            schema: qbot_llm::schema::tool_schema::<SubmitArgs>(parameters()),
        }];
        let text = |role, text: String| {
            ConvItem::Message(Message {
                role,
                content: vec![Content::Text(text)],
            })
        };
        let mut items = vec![
            text(Role::System, self.instructions(language)),
            text(Role::User, render(ctx)),
        ];
        let mut usage = Usage::ZERO;
        let mut problems: Vec<String> = Vec::new();
        // A valid episode whose findings were partly rejected: sent back once for the findings,
        // and kept if the correction does not come back valid.
        let mut accepted: Option<Extracted> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let conversation = Conversation::new(items.clone());
            let response = self
                .provider
                .respond(Request {
                    conversation: &conversation,
                    tools: &tools,
                    tool_choice: ToolChoice::Auto,
                    parallel_tool_calls: false,
                    reasoning: self.reasoning,
                    continuation: None,
                    media: None,
                })
                .await;
            let response = match (response, accepted.take()) {
                (Ok(response), kept) => {
                    accepted = kept;
                    response
                }
                // An answer that came back unreadable (arguments that are not JSON) is lost,
                // not wrong: the model can do it right on another try.
                (Err(LlmError::Protocol(detail)), kept) if attempt < MAX_ATTEMPTS => {
                    accepted = kept;
                    tracing::warn!(attempt, %detail, "extraction answer unreadable; asking again");
                    continue;
                }
                (Err(error), None) => return Err(error.into()),
                (Err(error), Some(kept)) => {
                    tracing::warn!(%error, "the findings correction failed; keeping the episode");
                    return Ok(Extracted { usage, ..kept });
                }
            };
            usage += response.usage;
            let model = response.meta.model.clone();
            let calls: Vec<_> = response.calls().cloned().collect();
            let response_finish = response.finish.clone();
            let turn = response.turn;

            // An answer cut off at the provider's output limit is lost, not wrong. How much the
            // model reasons varies from try to try, so the same request is sent again.
            if calls.is_empty() && response_finish == FinishReason::Length {
                if attempt < MAX_ATTEMPTS {
                    tracing::warn!(attempt, "extraction answer cut off; asking again");
                    continue;
                }
                return match accepted {
                    Some(kept) => Ok(Extracted { usage, ..kept }),
                    None => Err(ExtractError::CutOff),
                };
            }
            let mut feedback: Vec<(CallId, String)> = Vec::new();
            let mut only_findings = false;
            match calls.as_slice() {
                [] => {
                    problems = vec![say(Text::ExtractNoCall {
                        tool: TOOL_NAME.to_owned(),
                    })]
                }
                [call] if call.name == TOOL_NAME => {
                    match serde_json::from_value::<SubmitArgs>(call.arguments.clone()) {
                        Err(error) => {
                            problems = vec![say(Text::ExtractBadArguments {
                                error: error.to_string(),
                            })]
                        }
                        Ok(args) => match validate(&args, ctx.target) {
                            Ok(v) => {
                                let (findings, dropped) = validate_findings(
                                    &args.names,
                                    &args.facts,
                                    &args.knowledge,
                                    ctx,
                                    &self.predicates,
                                );
                                let extracted = Extracted {
                                    title: v.0,
                                    summary: v.1,
                                    evidence: v.2,
                                    findings,
                                    dropped,
                                    usage,
                                    attempts: attempt,
                                    model,
                                };
                                // One correction round for findings: they are optional, the episode is not.
                                if extracted.dropped.is_empty()
                                    || accepted.is_some()
                                    || attempt == MAX_ATTEMPTS
                                {
                                    return Ok(extracted);
                                }
                                problems = extracted.dropped.clone();
                                accepted = Some(extracted);
                                only_findings = true;
                            }
                            Err(found) => problems = found,
                        },
                    }
                }
                many => {
                    problems = vec![say(Text::ExtractCallOnce {
                        tool: TOOL_NAME.to_owned(),
                    })];
                    feedback = many
                        .iter()
                        .map(|c| (c.id.clone(), problems.join("; ")))
                        .collect();
                }
            }
            // Send the answer back with the reasons, as the next turn of the same exchange.
            items.push(ConvItem::Assistant(AssistantTurn::new(
                turn.parts()
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
            let tool = TOOL_NAME.to_owned();
            let reasons = say(if only_findings {
                Text::ExtractFindingsRejected {
                    problems: listed,
                    tool,
                }
            } else {
                Text::ExtractRejected {
                    problems: listed,
                    tool,
                }
            });
            if calls.is_empty() {
                items.push(text(Role::User, reasons));
            } else {
                if feedback.is_empty() {
                    feedback = calls
                        .iter()
                        .map(|c| (c.id.clone(), reasons.clone()))
                        .collect();
                }
                for (call_id, content) in feedback {
                    items.push(ConvItem::ToolResult(ToolOutput {
                        call_id,
                        status: ToolStatus::Refused,
                        content: vec![Content::Text(content)],
                    }));
                }
            }
        }
        match accepted {
            Some(kept) => Ok(Extracted { usage, ..kept }),
            None => Err(ExtractError::Invalid {
                attempts: MAX_ATTEMPTS,
                problems,
            }),
        }
    }
}

/// Descriptions of the `submit_episode` parameters.
fn parameters() -> Vec<(&'static str, String)> {
    vec![
        ("title", say(Text::ExtractParamTitle {})),
        ("summary", say(Text::ExtractParamSummary {})),
        ("evidence", say(Text::ExtractParamEvidence {})),
        ("evidence.line", say(Text::ExtractParamEvidenceLine {})),
        ("evidence.quote", say(Text::ExtractParamEvidenceQuote {})),
        ("names", say(Text::ExtractParamNames {})),
        ("names.member", say(Text::ExtractParamNameMember {})),
        ("names.name", say(Text::ExtractParamNameName {})),
        ("names.line", say(Text::ExtractParamLine {})),
        ("names.quote", say(Text::ExtractParamQuote {})),
        ("facts", say(Text::ExtractParamFacts {})),
        ("facts.member", say(Text::ExtractParamFactMember {})),
        ("facts.predicate", say(Text::ExtractParamFactPredicate {})),
        ("facts.object", say(Text::ExtractParamFactObject {})),
        ("facts.line", say(Text::ExtractParamLine {})),
        ("facts.quote", say(Text::ExtractParamQuote {})),
        ("knowledge", say(Text::ExtractParamKnowledge {})),
        ("knowledge.kind", say(Text::ExtractParamKnowledgeKind {})),
        ("knowledge.term", say(Text::ExtractParamKnowledgeTerm {})),
        ("knowledge.text", say(Text::ExtractParamKnowledgeText {})),
        ("knowledge.line", say(Text::ExtractParamLine {})),
        ("knowledge.quote", say(Text::ExtractParamQuote {})),
    ]
}

/// The target and its context, as the model sees them. Only target lines are numbered.
pub fn render(ctx: &SliceContext<'_>) -> String {
    let zone = &ctx.background.zone;
    let mut out = String::new();
    if !ctx.background.text.is_empty() {
        let _ = writeln!(out, "{}\n", ctx.background.text.trim_end());
    }
    if !ctx.previous.is_empty() {
        let _ = writeln!(out, "{}", say(Text::ExtractContextBefore {}));
        for line in ctx.previous {
            let _ = writeln!(out, "{}", line.render(zone));
        }
        out.push('\n');
    }
    let _ = writeln!(out, "{}", say(Text::ExtractTarget {}));
    for (i, line) in ctx.target.iter().enumerate() {
        let _ = writeln!(out, "#{} {}", i + 1, line.render(zone));
    }
    if !ctx.next.is_empty() {
        let _ = writeln!(out, "\n{}", say(Text::ExtractContextAfter {}));
        for line in ctx.next {
            let _ = writeln!(out, "{}", line.render(zone));
        }
    }
    out
}

/// Check an answer against what code owns. Returns every problem, so one correction can fix all.
pub fn validate(
    args: &SubmitArgs,
    target: &[SliceLine],
) -> Result<(String, String, Vec<Evidence>), Vec<String>> {
    let mut problems = Vec::new();
    if args.title.trim().is_empty() {
        problems.push(say(Text::ExtractEmptyTitle {}));
    }
    if args.summary.trim().is_empty() {
        problems.push(say(Text::ExtractEmptySummary {}));
    }
    if !(1..=3).contains(&args.evidence.len()) {
        problems.push(say(Text::ExtractEvidenceCount {
            count: args.evidence.len().to_string(),
        }));
    }
    let mut evidence = Vec::new();
    for q in &args.evidence {
        let line = q.line as usize;
        match target.get(line.wrapping_sub(1)) {
            None => problems.push(say(Text::ExtractEvidenceLine {
                line: line.to_string(),
            })),
            Some(l) if q.quote.trim().is_empty() || !l.text.contains(q.quote.trim()) => {
                problems.push(say(Text::ExtractEvidenceQuote {
                    line: line.to_string(),
                }));
            }
            Some(l) => evidence.push(Evidence {
                message: l.message,
                quote: q.quote.trim().to_owned(),
            }),
        }
    }
    if problems.is_empty() {
        Ok((
            args.title.trim().to_owned(),
            args.summary.trim().to_owned(),
            evidence,
        ))
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instructions_have_exactly_the_language_and_predicate_slots() {
        let slots: Vec<&str> = SYSTEM
            .match_indices('{')
            .filter_map(|(at, _)| SYSTEM[at + 1..].split_once('}').map(|(slot, _)| slot))
            .collect();
        assert_eq!(slots, ["language", "predicates"]);

        let extractor = EpisodeExtractor::new(Arc::new(qbot_llm::fake::FakeProvider::new([])));
        let text = extractor.instructions("English");
        assert!(!text.contains('{'), "every slot is filled");
        assert!(text.contains("in this language: English."));
        assert!(text.contains("- lives_in (one value): "));
    }
}

#[cfg(test)]
mod schema_tests {
    #[test]
    fn every_field_of_the_episode_submission_is_described() {
        let schema = qbot_llm::schema::tool_schema::<super::SubmitArgs>(super::parameters());
        assert_eq!(qbot_llm::schema::undescribed(&schema), Vec::<String>::new());
    }
}
