//! Deciding, per subject, how the night's statements relate to what is known.
//!
//! The batch reads statements per episode without deciding anything about memories. Then, for
//! each member (and the group) the night's statements are about, one call sees everything known
//! about them, what was forgotten at a member's request, and every statement about them in chat
//! order, and decides each statement once: it supports a memory, is new, replaces a memory, ends
//! one, or is rejected with a reason. Duplicates among current memories can be merged. Code checks
//! what it owns: every statement decided once, every id a current memory of this subject, and the
//! authority rule: what a person entered by hand only a person changes, and what a member said
//! about themselves only they can say has changed or ended.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use jiff::tz::TimeZone;
use qbot_core::{AccountId, UnixMillis};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::memories::{
    Durability, Kind, MAX_MEMORY_CHARS, Memory, MemoryId, MemoryState, Obs, Origin, Suppression,
    memory_key,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Support,
    New,
    Replace,
    End,
    Reject,
}

/// Why a statement is not remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// A joke, sarcasm, role-play or exaggeration.
    Joke,
    /// A hypothetical, a question or a wish.
    Hypothetical,
    /// Not about this subject after all.
    NotAbout,
    /// True now, but it will not matter in a few weeks.
    Passing,
    /// Restates something forgotten at a member's request.
    Forgotten,
    /// Contradicts what a person entered by hand, or what the member says about themselves.
    Contradicts,
    /// The line does not bear it out.
    Unsupported,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Joke => "joke",
            Reason::Hypothetical => "hypothetical",
            Reason::NotAbout => "not_about",
            Reason::Passing => "passing",
            Reason::Forgotten => "forgotten",
            Reason::Contradicts => "contradicts",
            Reason::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DecisionArg {
    pub statement: String,
    pub action: Action,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub durability: Option<Durability>,
    #[serde(default)]
    pub reason: Option<Reason>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MergeArg {
    pub memory: String,
    pub into: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DecideArgs {
    pub decisions: Vec<DecisionArg>,
    #[serde(default)]
    pub merges: Vec<MergeArg>,
}

/// A statement waiting for a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub obs: Obs,
    pub author: AccountId,
    pub at: UnixMillis,
    /// Where its line is: `#12` among the lines read, `msg:ID` for an earlier line.
    pub line: String,
    pub quote: String,
    pub text: String,
    pub term: Option<String>,
    pub durability: Durability,
    pub with: Option<AccountId>,
}

/// One subject's case.
#[derive(Debug, Clone, Copy)]
pub struct DecideCase<'a> {
    /// `None`: the group.
    pub subject: Option<AccountId>,
    /// Everything known about the subject, in every state.
    pub memories: &'a [Memory],
    /// What was forgotten about the subject.
    pub forgotten: &'a [&'a Suppression],
    pub candidates: &'a [Candidate],
    /// Member numbers, for showing people as `member:N`.
    pub numbers: &'a BTreeMap<AccountId, u32>,
    pub zone: &'a TimeZone,
}

/// What a decision refers to as the memory a statement supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ref {
    Memory(MemoryId),
    /// The memory another statement of the case creates (by its index).
    Statement(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Support(Ref),
    New {
        text: String,
        durability: Durability,
    },
    Replace {
        memory: MemoryId,
        text: String,
        durability: Durability,
    },
    End(MemoryId),
    Reject(Reason),
}

/// The decisions, one per candidate in order, and the merges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    pub decisions: Vec<Decision>,
    /// `(from, into)`.
    pub merges: Vec<(MemoryId, MemoryId)>,
}

fn label(index: usize) -> String {
    format!("o{}", index + 1)
}

fn who(numbers: &BTreeMap<AccountId, u32>, account: AccountId) -> String {
    numbers
        .get(&account)
        .map_or_else(|| say(Text::DecideSomeone {}), |n| format!("member:{n}"))
}

fn date(zone: &TimeZone, at: UnixMillis) -> String {
    jiff::Timestamp::from_millisecond(at.get())
        .map(|t| t.to_zoned(zone.clone()).strftime("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn one_line(text: &str) -> String {
    qbot_core::marker::escape_markers(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Where a memory comes from, as the model reads it.
fn source(case: &DecideCase<'_>, memory: &Memory) -> String {
    match memory.origin {
        Origin::Hand { by } => say(Text::DecideSourceHand {
            member: who(case.numbers, by),
            date: date(case.zone, memory.first),
        }),
        Origin::Ruling => say(Text::DecideSourceRuling {}),
        Origin::Chat => {
            let own = memory.own_word();
            let said = say(if memory.subject.is_none() {
                Text::DecideSourceMembers {}
            } else if own == 0 {
                Text::DecideSourceOthers {}
            } else if own == memory.episodes() {
                Text::DecideSourceOwn {}
            } else {
                Text::DecideSourceBoth {}
            });
            say(Text::DecideSourceChat {
                said,
                count: memory.episodes().to_string(),
                date: date(case.zone, memory.last),
            })
        }
    }
}

/// The case as the model reads it.
pub fn render(case: &DecideCase<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        match case.subject {
            Some(account) => say(Text::DecideSubjectMember {
                member: who(case.numbers, account),
            }),
            None => say(Text::DecideSubjectGroup {}),
        }
    );
    let current: Vec<&Memory> = case.memories.iter().filter(|m| m.is_current()).collect();
    if current.is_empty() {
        let _ = writeln!(out, "{}", say(Text::DecideKnownNone {}));
    } else {
        let _ = writeln!(out, "{}", say(Text::DecideKnown {}));
        for memory in &current {
            let with = memory
                .with
                .map(|w| {
                    say(Text::DecideWith {
                        member: who(case.numbers, w),
                    })
                })
                .unwrap_or_default();
            let line = match memory.kind {
                Kind::Name => say(Text::DecideName {
                    id: memory.id.label(),
                    name: one_line(&memory.text),
                    source: source(case, memory),
                }),
                Kind::Statement => say(Text::DecideMemory {
                    id: memory.id.label(),
                    durability: memory.durability.as_str().to_owned(),
                    text: one_line(&memory.display()),
                    with,
                    source: source(case, memory),
                }),
            };
            let _ = writeln!(out, "{line}");
        }
    }
    let closed: Vec<(&Memory, UnixMillis)> = case
        .memories
        .iter()
        .filter_map(|m| match m.state {
            MemoryState::Current => None,
            MemoryState::Replaced { at, .. } | MemoryState::Ended { at } => Some((m, at)),
        })
        .collect();
    if !closed.is_empty() {
        let _ = writeln!(out, "{}", say(Text::DecideClosed {}));
        for (memory, at) in closed {
            let _ = writeln!(
                out,
                "{}",
                say(Text::DecideClosedItem {
                    id: memory.id.label(),
                    text: one_line(&memory.display()),
                    date: date(case.zone, at),
                })
            );
        }
    }
    if !case.forgotten.is_empty() {
        let _ = writeln!(out, "{}", say(Text::DecideForgotten {}));
        for one in case.forgotten {
            let _ = writeln!(
                out,
                "{}",
                say(Text::DecideForgottenItem {
                    text: one_line(&one.text),
                })
            );
        }
    }
    let _ = writeln!(out, "{}", say(Text::DecideStatements {}));
    for (index, one) in case.candidates.iter().enumerate() {
        let how = say(match case.subject {
            None => Text::DecideHowGroup {},
            Some(subject) if subject == one.author => Text::DecideHowOwn {},
            Some(_) => Text::DecideHowOthers {},
        });
        let text = match &one.term {
            Some(term) => format!("{term}: {}", one.text),
            None => one.text.clone(),
        };
        let _ = writeln!(
            out,
            "{}",
            say(Text::DecideStatement {
                label: label(index),
                line: one.line.clone(),
                author: who(case.numbers, one.author),
                how,
                durability: one.durability.as_str().to_owned(),
                text: one_line(&text),
                with: one
                    .with
                    .map(|w| say(Text::DecideWith {
                        member: who(case.numbers, w)
                    }))
                    .unwrap_or_default(),
                quote: one_line(&one.quote),
            })
        );
    }
    out
}

fn problem(index: usize, reason: String) -> String {
    say(Text::DecideProblem {
        label: label(index),
        problem: reason,
    })
}

/// Check the model's decisions against the case. Returns every problem, so one correction can
/// fix all.
pub fn check(case: &DecideCase<'_>, args: &DecideArgs) -> Result<Decided, Vec<String>> {
    let mut problems = Vec::new();
    let current = |raw: &str| -> Option<&Memory> {
        MemoryId::parse(raw)
            .and_then(|id| case.memories.iter().find(|m| m.id == id && m.is_current()))
    };
    let forgotten: BTreeSet<&str> = case.forgotten.iter().map(|s| s.key.as_str()).collect();

    // Duplicates folded together first: what is merged away is not decided on.
    let mut merges = Vec::new();
    let mut away: BTreeMap<MemoryId, MemoryId> = BTreeMap::new();
    for merge in &args.merges {
        let ok = |m: Option<&Memory>| {
            m.filter(|m| m.kind == Kind::Statement && !m.origin.is_hand())
                .map(|m| m.id)
        };
        match (ok(current(&merge.memory)), ok(current(&merge.into))) {
            (Some(from), Some(into)) if from != into && !away.contains_key(&into) => {
                away.insert(from, into);
                merges.push((from, into));
            }
            _ => problems.push(say(Text::DecideMerge {
                from: merge.memory.trim().to_owned(),
                into: merge.into.trim().to_owned(),
            })),
        }
    }
    let usable = |raw: &str| -> Result<&Memory, String> {
        let memory = current(raw).ok_or_else(|| {
            say(Text::DecideUnknownMemory {
                id: raw.trim().to_owned(),
            })
        })?;
        match away.get(&memory.id) {
            Some(into) => Err(say(Text::DecideMergedAway {
                id: memory.id.label(),
                into: into.label(),
            })),
            None => Ok(memory),
        }
    };
    // Who may close a memory: nobody what a person entered by hand, only its member a name, and
    // only a party to it what a party said themselves.
    let authority = |memory: &Memory, author: AccountId| -> Result<(), String> {
        if memory.origin.is_hand() {
            return Err(say(Text::DecideHand {
                id: memory.id.label(),
            }));
        }
        if memory.kind == Kind::Name && !memory.is_party(author) {
            return Err(say(Text::DecideNameOwnWord {
                id: memory.id.label(),
            }));
        }
        if memory.own_word() > 0 && !memory.is_party(author) {
            return Err(say(Text::DecideOwnWord {
                id: memory.id.label(),
            }));
        }
        Ok(())
    };
    let text_of = |arg: &DecisionArg, one: &Candidate| -> Result<String, String> {
        let text = arg.text.as_deref().map_or(one.text.as_str(), str::trim);
        if text.is_empty() {
            return Err(say(Text::ExtractFindingEmptyText {}));
        }
        if text.chars().count() > MAX_MEMORY_CHARS {
            return Err(say(Text::ExtractFindingTextTooLong {
                max: MAX_MEMORY_CHARS.to_string(),
            }));
        }
        Ok(text.to_owned())
    };

    let mut decided: Vec<Option<Decision>> = vec![None; case.candidates.len()];
    let mut failed: BTreeSet<usize> = BTreeSet::new();
    for arg in &args.decisions {
        let index = arg
            .statement
            .trim()
            .strip_prefix('o')
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| n.checked_sub(1))
            .filter(|i| *i < case.candidates.len());
        let Some(index) = index else {
            problems.push(say(Text::DecideUnknownStatement {
                label: arg.statement.trim().to_owned(),
            }));
            continue;
        };
        if decided[index].is_some() || failed.contains(&index) {
            problems.push(problem(index, say(Text::DecideTwice {})));
            failed.insert(index);
            continue;
        }
        let one = &case.candidates[index];
        let memory_arg = arg.memory.as_deref().map(str::trim).unwrap_or_default();
        let decision = match arg.action {
            Action::Support => {
                if let Some(other) = memory_arg
                    .strip_prefix('o')
                    .and_then(|n| n.parse::<usize>().ok())
                    .and_then(|n| n.checked_sub(1))
                    .filter(|i| *i < case.candidates.len() && *i != index)
                {
                    Ok(Decision::Support(Ref::Statement(other)))
                } else {
                    usable(memory_arg).and_then(|m| {
                        if m.kind == Kind::Statement {
                            Ok(Decision::Support(Ref::Memory(m.id)))
                        } else {
                            Err(say(Text::DecideNotStatement { id: m.id.label() }))
                        }
                    })
                }
            }
            Action::New => text_of(arg, one).map(|text| Decision::New {
                text,
                durability: arg.durability.unwrap_or(one.durability),
            }),
            Action::Replace => usable(memory_arg).and_then(|m| {
                if m.kind != Kind::Statement {
                    return Err(say(Text::DecideNotStatement { id: m.id.label() }));
                }
                authority(m, one.author)?;
                Ok(Decision::Replace {
                    memory: m.id,
                    text: text_of(arg, one)?,
                    durability: arg.durability.unwrap_or(one.durability),
                })
            }),
            Action::End => usable(memory_arg).and_then(|m| {
                authority(m, one.author)?;
                Ok(Decision::End(m.id))
            }),
            Action::Reject => arg
                .reason
                .map(Decision::Reject)
                .ok_or_else(|| say(Text::DecideNoReason {})),
        };
        match decision {
            // What restates something forgotten is never remembered, whatever was decided.
            Ok(Decision::New { ref text, .. } | Decision::Replace { ref text, .. })
                if forgotten.contains(memory_key(one.term.as_deref(), text).as_str()) =>
            {
                decided[index] = Some(Decision::Reject(Reason::Forgotten));
            }
            Ok(decision) => decided[index] = Some(decision),
            Err(reason) => {
                problems.push(problem(index, reason));
                failed.insert(index);
            }
        }
    }
    // A statement supporting another's memory needs that one to create a memory.
    for (index, decision) in decided.iter().enumerate() {
        if let Some(Decision::Support(Ref::Statement(other))) = decision {
            let creates = matches!(
                decided.get(*other),
                Some(Some(Decision::New { .. } | Decision::Replace { .. }))
            );
            if !creates {
                problems.push(problem(index, say(Text::DecideSupportTarget {})));
            }
        }
    }
    for (index, decision) in decided.iter().enumerate() {
        if decision.is_none() && !failed.contains(&index) {
            problems.push(problem(index, say(Text::DecideUndecided {})));
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(Decided {
        decisions: decided.into_iter().flatten().collect(),
        merges,
    })
}
