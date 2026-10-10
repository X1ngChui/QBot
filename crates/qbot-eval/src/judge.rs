//! The LLM judge: grades a run against the scenario's English rubric.

use qbot_context::{ChatLine, Speaker};
use qbot_llm::{
    Content, ConvItem, Conversation, Message, Provider, ReasoningEffort, Request, Role, ToolChoice,
    ToolSpec,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::runner::RunResult;
use crate::scenario::Scenario;

const INSTRUCTIONS: &str = include_str!("../../../eval/judge.md");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub criterion: usize,
    pub passed: bool,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
struct Submitted {
    verdicts: Vec<Verdict>,
}

fn time(at: qbot_core::UnixMillis) -> String {
    jiff::Timestamp::from_millisecond(at.get())
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::get("Asia/Shanghai").unwrap_or(jiff::tz::TimeZone::UTC))
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// The evidence, as one English text.
pub fn evidence(scenario: &Scenario, run: &RunResult, lines: &[ChatLine]) -> String {
    let name = |number: u32| {
        scenario
            .member(number)
            .map_or_else(String::new, |m| format!(" ({})", m.name))
    };
    let before = scenario.chat.len();
    let mut out = format!("Situation: {}\n\nGroup chat:\n", scenario.description);
    for (i, line) in lines.iter().enumerate().take(before) {
        let who = match line.speaker {
            Speaker::Member { number, .. } => {
                format!("member:{}{}", number.get(), name(number.get()))
            }
            Speaker::Bot => "the bot".to_owned(),
        };
        out.push_str(&format!(
            "{}. [{}] {who}: {}\n",
            i + 1,
            time(line.at),
            line.text
        ));
    }
    let blocked: Vec<String> = scenario
        .members
        .iter()
        .filter(|m| m.blocked)
        .map(|m| format!("member:{}", m.number))
        .collect();
    if !blocked.is_empty() {
        out.push_str(&format!(
            "\nBlocked (cannot start a conversation with the bot): {}\n",
            blocked.join(", ")
        ));
    }
    for set in &scenario.same_person {
        let set: Vec<String> = set.iter().map(|n| format!("member:{n}")).collect();
        out.push_str(&format!(
            "Linked accounts of one person: {}\n",
            set.join(", ")
        ));
    }
    out.push_str("\nWhat started the run: ");
    match (&scenario.trigger.line, &scenario.trigger.wake) {
        (Some(n), _) => out.push_str(&format!("line {n} addressed the bot.\n")),
        (None, Some(intent)) => out.push_str(&format!(
            "a scheduled task came due. Its stored intent: {intent}\n"
        )),
        (None, None) if scenario.trigger.spontaneous => out.push_str(
            "nobody addressed the bot; it looked at the conversation on its own and may join in or stay silent.\n",
        ),
        (None, None) => out.push('\n'),
    }
    out.push_str("\nWhat the bot did:\n");
    if run.calls.is_empty() {
        out.push_str("(no tool calls)\n");
    }
    for (i, call) in run.calls.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}({}) -> {}: {}\n",
            i + 1,
            call.name,
            call.arguments,
            call.outcome,
            call.result
        ));
    }
    if !run.undelivered.is_empty() {
        out.push_str("\nText the bot wrote outside tool calls (never shown to the group):\n");
        for text in &run.undelivered {
            out.push_str(&format!("- {text}\n"));
        }
    }
    out.push_str("\nMessages the group saw from the bot:\n");
    if run.sent.is_empty() {
        out.push_str("(none: the bot stayed silent)\n");
    }
    for message in &run.sent {
        out.push_str(&format!("- {message}\n"));
    }
    out.push_str(&format!("\nHow the run ended: {}\n", run.end));
    out
}

pub async fn judge(
    provider: &dyn Provider,
    scenario: &Scenario,
    evidence: &str,
) -> Result<Vec<Verdict>, String> {
    if scenario.expect.rubric.is_empty() {
        return Ok(Vec::new());
    }
    let criteria: String = scenario
        .expect
        .rubric
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {c}\n", i + 1))
        .collect();
    let tools = [ToolSpec {
        name: "submit_verdict".into(),
        description: "Submit one verdict per criterion, in order.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "verdicts": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "criterion": { "type": "integer" },
                            "passed": { "type": "boolean" },
                            "reason": { "type": "string" }
                        },
                        "required": ["criterion", "passed", "reason"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["verdicts"],
            "additionalProperties": false
        }),
    }];
    let message = |role, text: String| {
        ConvItem::Message(Message {
            role,
            content: vec![Content::Text(text)],
        })
    };
    let conversation = Conversation::new(vec![
        message(Role::System, INSTRUCTIONS.to_owned()),
        message(Role::User, format!("{evidence}\nCriteria:\n{criteria}")),
    ]);
    for _ in 0..2 {
        let response = provider
            .respond(Request {
                conversation: &conversation,
                tools: &tools,
                tool_choice: ToolChoice::Auto,
                parallel_tool_calls: false,
                reasoning: ReasoningEffort::Medium,
                continuation: None,
                media: None,
            })
            .await
            .map_err(|e| e.to_string())?;
        if let Some(call) = response.calls().find(|c| c.name == "submit_verdict")
            && let Ok(submitted) = serde_json::from_value::<Submitted>(call.arguments.clone())
        {
            // Every criterion gets a verdict; one the judge skipped counts as failed.
            return Ok((1..=scenario.expect.rubric.len())
                .map(|n| {
                    submitted
                        .verdicts
                        .iter()
                        .find(|v| v.criterion == n)
                        .cloned()
                        .unwrap_or(Verdict {
                            criterion: n,
                            passed: false,
                            reason: "the judge gave no verdict".into(),
                        })
                })
                .collect());
        }
    }
    Err("the judge did not submit a verdict".into())
}
