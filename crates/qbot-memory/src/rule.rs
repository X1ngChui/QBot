//! Ruling on a name nobody could place.
//!
//! The reading of one stretch of chat often cannot tell whom an expression names: someone says
//! a name in an ordinary line and nothing in that conversation shows whose it is. Several such
//! uses from separate conversations can: the same member answers each time, or what is said of
//! the person fits what is remembered about one member. The batch puts the uses side by side to
//! a model (see [`crate::batch`]); code keeps a ruling for a member only when it names one the
//! lines involve and cites lines that were shown. A name ruled to be no member's is kept as what
//! it means in the group, so later uses are read as that.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use jiff::tz::TimeZone;
use qbot_core::{AccountId, MessageId};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::memories::MAX_MEMORY_CHARS;
use crate::names::{OpenName, Verdict};
use crate::slice::SliceLine;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RulingArgs {
    pub verdict: String,
    #[serde(default)]
    pub lines: Vec<i64>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub meaning: String,
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
    /// Accounts of the members the group's state lists, by number.
    pub accounts: &'a [(u32, AccountId)],
    /// The zone the lines' times are shown in.
    pub zone: &'a TimeZone,
}

/// What the model ruled, kept as far as the lines bear it: a member must be one the case
/// involves and rest on lines that were shown, and a name that is nobody's needs what it means,
/// or the ruling is that nobody can tell yet.
pub fn verdict(case: &RulingCase<'_>, args: &RulingArgs) -> Verdict {
    let shown = |id: &MessageId| {
        case.uses
            .iter()
            .any(|u| u.lines.iter().any(|l| l.message == *id))
    };
    let cited = args
        .lines
        .iter()
        .filter_map(|id| MessageId::new(*id).ok())
        .any(|id| shown(&id));
    let mut members: BTreeMap<u32, AccountId> = case.accounts.iter().copied().collect();
    for line in case.uses.iter().flat_map(|u| u.lines.iter()) {
        if let (Some(number), Some(account)) = (line.member_no, line.speaker) {
            members.insert(number, account);
        }
    }
    let meaning = args
        .meaning
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let meant =
        (!meaning.is_empty() && meaning.chars().count() <= MAX_MEMORY_CHARS).then_some(meaning);
    match args.verdict.trim() {
        "outside" => meant.map_or(Verdict::Unknown, Verdict::Outside),
        "general" => meant.map_or(Verdict::Unknown, Verdict::General),
        other => other
            .strip_prefix("member:")
            .and_then(|n| n.trim().parse::<u32>().ok())
            .and_then(|n| members.get(&n))
            .filter(|_| cited)
            .map_or(Verdict::Unknown, |account| Verdict::Member(*account)),
    }
}

/// The case as the model reads it: the expression, and each use with its chat.
pub fn render(case: &RulingCase<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        say(Text::RuleCase {
            name: qbot_core::marker::escape_markers(&case.name.text),
            count: case.name.unknown.to_string(),
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
