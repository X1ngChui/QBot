//! Ruling on a name nobody could place.
//!
//! The reading of one stretch of chat often cannot tell whom an expression names: someone says
//! a name in an ordinary line and nothing in that conversation shows whose it is. Several such
//! uses from separate conversations can: the same member answers each time, or what is said of
//! the person fits what is remembered about one member. A model reads the uses side by side and
//! rules; code keeps a ruling for a member only when it names one the lines involve and cites
//! lines that were shown.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use qbot_core::{AccountId, MessageId, UnixMillis};
use qbot_llm::{
    Content, ConvItem, Conversation, LlmError, Message, Provider, ReasoningEffort, Request, Role,
    ToolChoice, ToolSpec,
};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::names::{OpenName, Ruling, Verdict};
use crate::slice::{Background, SliceLine};

pub const TOOL_NAME: &str = "submit_ruling";

const SYSTEM: &str = include_str!("../../../prompts/rule_name.md");

/// Model calls for one ruling: the answer and one more try when it holds no ruling. A ruling can
/// wait for the next run; a slice of chat cannot.
const ATTEMPTS: u32 = 2;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulingArgs {
    pub verdict: String,
    #[serde(default)]
    pub lines: Vec<i64>,
    #[serde(default)]
    pub reason: String,
}

/// One use of the expression with the chat around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseWindow {
    pub lines: Vec<SliceLine>,
    /// The line with the use.
    pub message: MessageId,
}

#[derive(Debug, Clone, Copy)]
pub struct RulingCase<'a> {
    pub name: &'a OpenName,
    pub uses: &'a [UseWindow],
    /// The members other conversations read the expression as meaning, by number.
    pub placed: &'a [u32],
    /// Who the people in the lines are.
    pub background: &'a Background,
}

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("model error: {0}")]
    Model(#[from] LlmError),
    #[error("the model gave no ruling that can be read")]
    NoAnswer,
}

pub struct NameRuler {
    provider: Arc<dyn Provider>,
    reasoning: ReasoningEffort,
}

impl std::fmt::Debug for NameRuler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NameRuler").finish_non_exhaustive()
    }
}

impl NameRuler {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            reasoning: ReasoningEffort::Low,
        }
    }

    /// How much the model reasons before answering (default low).
    pub fn with_reasoning(mut self, reasoning: ReasoningEffort) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Rule on the case; the ruling is dated `at`.
    pub async fn rule(&self, case: &RulingCase<'_>, at: UnixMillis) -> Result<Ruling, RuleError> {
        let tools = [ToolSpec {
            name: TOOL_NAME.to_owned(),
            description: say(Text::RuleToolDescription {}),
            schema: qbot_llm::schema::tool_schema::<RulingArgs>(vec![
                ("verdict", say(Text::RuleParamVerdict {})),
                ("lines", say(Text::RuleParamLines {})),
                ("reason", say(Text::RuleParamReason {})),
            ]),
        }];
        let text = |role, text: String| {
            ConvItem::Message(Message {
                role,
                content: vec![Content::Text(text)],
            })
        };
        let conversation = Conversation::new(vec![
            text(Role::System, SYSTEM.trim_end().to_owned()),
            text(Role::User, render(case)),
        ]);
        for _ in 0..ATTEMPTS {
            let response = self
                .provider
                .respond(Request {
                    conversation: &conversation,
                    tools: &tools,
                    tool_choice: ToolChoice::Auto,
                    parallel_tool_calls: false,
                    reasoning: self.reasoning,
                    media: None,
                })
                .await;
            let args = response?
                .calls()
                .find(|call| call.name == TOOL_NAME)
                .and_then(|call| serde_json::from_value::<RulingArgs>(call.arguments.clone()).ok());
            if let Some(args) = args {
                return Ok(ruling(case, &args, at));
            }
        }
        Err(RuleError::NoAnswer)
    }
}

/// What the model ruled, kept as far as the lines bear it: a member must be one the case
/// involves and rest on lines that were shown, or the ruling is that nobody can tell yet.
pub fn ruling(case: &RulingCase<'_>, args: &RulingArgs, at: UnixMillis) -> Ruling {
    let shown = |id: &MessageId| {
        case.uses
            .iter()
            .any(|u| u.lines.iter().any(|l| l.message == *id))
    };
    let mut lines: Vec<MessageId> = args
        .lines
        .iter()
        .filter_map(|id| MessageId::new(*id).ok())
        .filter(shown)
        .collect();
    lines.sort();
    lines.dedup();
    let mut members: BTreeMap<u32, AccountId> = case.background.accounts.iter().copied().collect();
    for line in case.uses.iter().flat_map(|u| u.lines.iter()) {
        if let (Some(number), Some(account)) = (line.member_no, line.speaker) {
            members.insert(number, account);
        }
    }
    let verdict = match args.verdict.trim() {
        "outside" => Verdict::Outside,
        "general" => Verdict::General,
        other => other
            .strip_prefix("member:")
            .and_then(|n| n.trim().parse::<u32>().ok())
            .and_then(|n| members.get(&n))
            .filter(|_| !lines.is_empty())
            .map_or(Verdict::Unknown, |account| Verdict::Member(*account)),
    };
    Ruling {
        text: case.name.text.clone(),
        verdict,
        lines,
        reason: args.reason.split_whitespace().collect::<Vec<_>>().join(" "),
        seen: case.name.unknown,
        at,
    }
}

/// The case as the model reads it: who is who, the expression, and each use with its chat.
pub fn render(case: &RulingCase<'_>) -> String {
    let mut out = String::new();
    if !case.background.text.is_empty() {
        let _ = writeln!(out, "{}\n", case.background.text.trim_end());
    }
    let placed = if case.placed.is_empty() {
        String::new()
    } else {
        say(Text::RulePlaced {
            members: case
                .placed
                .iter()
                .map(|n| format!("member:{n}"))
                .collect::<Vec<_>>()
                .join(", "),
        })
    };
    let _ = writeln!(
        out,
        "{}",
        say(Text::RuleCase {
            name: qbot_core::marker::escape_markers(&case.name.text),
            count: case.name.unknown.to_string(),
            placed,
        })
    );
    for (index, one) in case.uses.iter().enumerate() {
        let _ = writeln!(
            out,
            "\n{}",
            say(Text::RuleUse {
                index: (index + 1).to_string(),
            })
        );
        for line in &one.lines {
            let mark = if line.message == one.message {
                ">> "
            } else {
                "   "
            };
            let _ = writeln!(out, "{mark}{}", line.render(&case.background.zone));
        }
    }
    out
}
