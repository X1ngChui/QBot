//! What an episode's extraction found besides the summary: names people go by, facts about them,
//! and knowledge about the group. Each is grounded in a verbatim quote from one line of the target
//! part, checked here; the model may propose, code decides what is kept.

use std::collections::HashMap;

use qbot_core::{AccountId, MessageId};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::identity::{AliasTextError, normalize_alias};
use crate::predicates::Predicates;
use crate::slice::SliceLine;

/// A name someone goes by, as used in the chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameFinding {
    pub account: AccountId,
    pub name: String,
    pub message: MessageId,
    pub quote: String,
}

/// A fact about one person, by a predicate of the table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactFinding {
    pub account: AccountId,
    pub predicate: String,
    pub object: String,
    pub message: MessageId,
    pub quote: String,
}

/// Something true of the group rather than of anyone in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KnowledgeFinding {
    /// A word or phrase the group uses, and what it means here.
    Term {
        term: String,
        meaning: String,
        message: MessageId,
        quote: String,
    },
    /// What the group is about.
    Topic {
        topic: String,
        message: MessageId,
        quote: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Findings {
    #[serde(default)]
    pub names: Vec<NameFinding>,
    #[serde(default)]
    pub facts: Vec<FactFinding>,
    #[serde(default)]
    pub knowledge: Vec<KnowledgeFinding>,
}

impl Findings {
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.facts.is_empty() && self.knowledge.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct NameArg {
    pub member: u32,
    pub name: String,
    pub line: u32,
    pub quote: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FactArg {
    pub member: u32,
    pub predicate: String,
    pub object: String,
    pub line: u32,
    pub quote: String,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeKind {
    Term,
    Topic,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KnowledgeArg {
    pub kind: KnowledgeKind,
    #[serde(default)]
    pub term: Option<String>,
    pub text: String,
    pub line: u32,
    pub quote: String,
}

/// Who appears in the slice and its context, by member number.
fn members(lines: &[&[SliceLine]]) -> HashMap<u32, AccountId> {
    lines
        .iter()
        .flat_map(|part| part.iter())
        .filter_map(|l| Some((l.member_no?, l.speaker?)))
        .collect()
}

struct Source<'a> {
    line: &'a SliceLine,
    quote: String,
}

/// The target line a finding cites, if it is a member's message that contains the quote.
fn source<'a>(target: &'a [SliceLine], line: u32, quote: &str) -> Result<Source<'a>, String> {
    let Some(found) = target.get((line as usize).wrapping_sub(1)) else {
        return Err(say(Text::ExtractFindingLine {
            line: line.to_string(),
        }));
    };
    if found.speaker.is_none() {
        return Err(say(Text::ExtractFindingBotLine {
            line: line.to_string(),
        }));
    }
    if found.text.starts_with("[notice:") {
        return Err(say(Text::ExtractFindingEvent {
            line: line.to_string(),
        }));
    }
    let quote = quote.trim();
    if quote.is_empty() || !found.text.contains(quote) {
        return Err(say(Text::ExtractFindingQuote {
            line: line.to_string(),
        }));
    }
    Ok(Source {
        line: found,
        quote: quote.to_owned(),
    })
}

/// The account a finding is about. It must be the person who wrote the line, or someone the
/// quote itself mentions: what one person says is never filed under somebody else's name.
fn subject(
    members: &HashMap<u32, AccountId>,
    member: u32,
    source: &Source<'_>,
) -> Result<AccountId, String> {
    let account = *members.get(&member).ok_or_else(|| {
        say(Text::ExtractFindingNoMember {
            member: member.to_string(),
        })
    })?;
    if source.line.speaker == Some(account) || source.quote.contains(&format!("[at:{member}]")) {
        Ok(account)
    } else {
        Err(say(Text::ExtractFindingNotSubject {
            member: member.to_string(),
        }))
    }
}

/// A reason a finding was not kept, naming the list and the item (counted from 1).
fn problem(list: &str, index: usize, reason: String) -> String {
    say(Text::ExtractFinding {
        list: list.to_owned(),
        item: (index + 1).to_string(),
        reason,
    })
}

/// Keep the findings that hold up; return the reasons for the rest. Findings never fail an
/// episode: a summary is worth keeping even when a fact proposed beside it is not.
pub fn validate_findings(
    names: &[NameArg],
    facts: &[FactArg],
    knowledge: &[KnowledgeArg],
    ctx: &crate::extract::SliceContext<'_>,
    predicates: &Predicates,
) -> (Findings, Vec<String>) {
    let members = members(&[ctx.previous, ctx.target, ctx.next]);
    let mut kept = Findings::default();
    let mut dropped = Vec::new();

    let mut name_items = Vec::new();
    for (index, arg) in names.iter().enumerate() {
        let result = source(ctx.target, arg.line, &arg.quote).and_then(|src| {
            let account = subject(&members, arg.member, &src)?;
            let name = arg.name.trim();
            if name.is_empty() || !src.quote.contains(name) {
                return Err(say(Text::ExtractFindingNameNotQuoted {
                    name: name.to_owned(),
                }));
            }
            normalize_alias(name).map_err(|e| match e {
                AliasTextError::Empty => say(Text::ExtractFindingNameEmpty {}),
                AliasTextError::TooLong => say(Text::ExtractFindingNameTooLong {}),
            })?;
            Ok(NameFinding {
                account,
                name: name.to_owned(),
                message: src.line.message,
                quote: src.quote,
            })
        });
        match result {
            Ok(finding) => {
                name_items.push(index);
                kept.names.push(finding);
            }
            Err(reason) => dropped.push(problem("names", index, reason)),
        }
    }
    // One name for two people in one answer is a guess, not a finding: drop every such name.
    let mut owners: HashMap<String, Vec<AccountId>> = HashMap::new();
    for finding in &kept.names {
        if let Ok(key) = normalize_alias(&finding.name) {
            owners.entry(key).or_default().push(finding.account);
        }
    }
    let mut items = name_items.into_iter();
    kept.names.retain(|finding| {
        let index = items.next().unwrap_or_default();
        let ambiguous = normalize_alias(&finding.name)
            .ok()
            .and_then(|key| owners.get(&key))
            .is_some_and(|accounts| accounts.iter().any(|a| *a != finding.account));
        if ambiguous {
            let reason = say(Text::ExtractFindingNameShared {
                name: finding.name.clone(),
            });
            dropped.push(problem("names", index, reason));
        }
        !ambiguous
    });

    for (index, arg) in facts.iter().enumerate() {
        let result = source(ctx.target, arg.line, &arg.quote).and_then(|src| {
            let account = subject(&members, arg.member, &src)?;
            if predicates.get(&arg.predicate).is_none() {
                return Err(say(Text::ExtractFindingPredicate {
                    predicate: arg.predicate.clone(),
                }));
            }
            let object = arg.object.trim();
            if object.is_empty() {
                return Err(say(Text::ExtractFindingEmptyObject {}));
            }
            Ok(FactFinding {
                account,
                predicate: arg.predicate.clone(),
                object: object.to_owned(),
                message: src.line.message,
                quote: src.quote,
            })
        });
        match result {
            Ok(finding) => kept.facts.push(finding),
            Err(reason) => dropped.push(problem("facts", index, reason)),
        }
    }

    for (index, arg) in knowledge.iter().enumerate() {
        let result = source(ctx.target, arg.line, &arg.quote).and_then(|src| {
            let text = arg.text.trim();
            if text.is_empty() {
                return Err(say(Text::ExtractFindingEmptyText {}));
            }
            match arg.kind {
                KnowledgeKind::Term => {
                    let term = arg.term.as_deref().map(str::trim).unwrap_or_default();
                    // The quote shows what the term means; the term itself only has to be
                    // written somewhere in the target, by a member.
                    let written = ctx
                        .target
                        .iter()
                        .any(|l| l.speaker.is_some() && l.text.contains(term));
                    if term.is_empty() || !written {
                        return Err(say(Text::ExtractFindingTermNotWritten {
                            term: term.to_owned(),
                        }));
                    }
                    Ok(KnowledgeFinding::Term {
                        term: term.to_owned(),
                        meaning: text.to_owned(),
                        message: src.line.message,
                        quote: src.quote,
                    })
                }
                KnowledgeKind::Topic => Ok(KnowledgeFinding::Topic {
                    topic: text.to_owned(),
                    message: src.line.message,
                    quote: src.quote,
                }),
            }
        });
        match result {
            Ok(finding) => kept.knowledge.push(finding),
            Err(reason) => dropped.push(problem("knowledge", index, reason)),
        }
    }
    (kept, dropped)
}
