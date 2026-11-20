//! Ruling on a name nobody could place.
//!
//! The reading of one stretch of chat often cannot tell whom an expression names: someone says
//! a name in an ordinary line and nothing in that conversation shows whose it is. Several such
//! uses from separate conversations can: the same member answers each time, or what is said of
//! the person fits what is remembered about one member. The batch puts the uses side by side to
//! a model (see [`crate::batch`]); code keeps a ruling for a member only when it names one the
//! lines involve and cites lines that were shown.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use jiff::tz::TimeZone;
use qbot_core::{AccountId, MessageId, UnixMillis};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::names::{OpenName, Ruling, Verdict};
use crate::slice::SliceLine;

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
    /// Accounts of the members the group's state lists, by number.
    pub accounts: &'a [(u32, AccountId)],
    /// The zone the lines' times are shown in.
    pub zone: &'a TimeZone,
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
    let mut members: BTreeMap<u32, AccountId> = case.accounts.iter().copied().collect();
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

/// The ruling nobody can give yet: the name waits for more uses.
pub fn undecided(name: &OpenName, at: UnixMillis) -> Ruling {
    Ruling {
        text: name.text.clone(),
        verdict: Verdict::Unknown,
        lines: Vec::new(),
        reason: String::new(),
        seen: name.unknown,
        at,
    }
}

/// The case as the model reads it: the expression, and each use with its chat.
pub fn render(case: &RulingCase<'_>) -> String {
    let mut out = String::new();
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
            let _ = writeln!(out, "{mark}{}", line.render(case.zone));
        }
    }
    out
}
