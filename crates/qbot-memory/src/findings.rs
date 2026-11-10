//! What an episode's extraction found besides the summary: names people use for one another and
//! memories about members and the group. Each is grounded in a verbatim quote from one line of
//! the target part and checked here against the line's structure; the model proposes, code
//! decides what is kept.

use std::collections::{HashMap, HashSet};

use qbot_core::{AccountId, MessageId};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::memories::{Durability, MAX_MEMORY_CHARS, MAX_TERM_CHARS, MemoryId, memory_key};
use crate::names::{NameBasis, NameTextError, name_key};
use crate::slice::SliceLine;

/// A name used for a member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameFinding {
    pub account: AccountId,
    pub name: String,
    /// Never [`NameBasis::Set`]: only a person sets a name.
    pub basis: NameBasis,
    /// The member answered the line (a later line of theirs replies to it).
    #[serde(default)]
    pub anchored: bool,
    pub message: MessageId,
    pub quote: String,
}

/// What a statement does to what is remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAction {
    /// Not remembered yet.
    New,
    /// Says again what memory `id` says.
    Same,
    /// Memory `id` holds no longer; this replaces it.
    Changed,
    /// Memory `id` holds no longer, and nothing replaces it.
    Ended,
}

/// A statement about a member or the group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFinding {
    /// `None`: the group.
    pub subject: Option<AccountId>,
    pub action: MemoryAction,
    /// The memory a `same`, `changed` or `ended` statement is about.
    pub id: Option<MemoryId>,
    /// For a group term.
    pub term: Option<String>,
    /// For `new` and `changed`.
    pub text: Option<String>,
    pub durability: Option<Durability>,
    pub message: MessageId,
    /// Who wrote the line.
    pub author: AccountId,
    pub quote: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Findings {
    #[serde(default)]
    pub names: Vec<NameFinding>,
    #[serde(default)]
    pub memories: Vec<MemoryFinding>,
}

impl Findings {
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.memories.is_empty()
    }
}

/// How a line used a name, as the model may say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UseArg {
    Own,
    Addressed,
    About,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct NameArg {
    pub member: u32,
    pub name: String,
    pub basis: UseArg,
    pub line: u32,
    pub quote: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MemoryArg {
    pub about: String,
    pub action: MemoryAction,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub term: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub durability: Option<Durability>,
    pub line: u32,
    pub quote: String,
}

/// What validation reads besides the arguments.
#[derive(Debug, Clone, Copy)]
pub struct FindingContext<'a> {
    /// The context before, the target and the context after.
    pub parts: [&'a [SliceLine]; 3],
    /// Older lines the shown ones quote.
    pub quoted: &'a [SliceLine],
    /// Accounts of members the lines mention without writing any of them.
    pub accounts: &'a [(u32, AccountId)],
    /// The current memories the extractor was shown, with their subjects.
    pub known: &'a [(MemoryId, Option<AccountId>)],
    /// Normalized names of the bot.
    pub bot_names: &'a [String],
}

impl FindingContext<'_> {
    fn target(&self) -> &[SliceLine] {
        self.parts[1]
    }

    /// Member numbers of the lines and their accounts.
    fn members(&self) -> HashMap<u32, AccountId> {
        let mut out: HashMap<u32, AccountId> = self.accounts.iter().copied().collect();
        for line in self.parts.iter().flat_map(|p| p.iter()) {
            if let (Some(n), Some(a)) = (line.member_no, line.speaker) {
                out.insert(n, a);
            }
        }
        out
    }

    /// Who wrote the message `id`, among the lines shown and quoted.
    fn author_of(&self, id: MessageId) -> Option<AccountId> {
        self.parts
            .iter()
            .flat_map(|p| p.iter())
            .chain(self.quoted)
            .rfind(|l| l.message == id)
            .and_then(|l| l.speaker)
    }

    /// The names each member is shown under in the lines, normalized.
    fn shown(&self) -> HashMap<String, AccountId> {
        let mut out = HashMap::new();
        for line in self.parts.iter().flat_map(|p| p.iter()) {
            if let (Some(account), Some(name)) = (line.speaker, &line.shown)
                && let Ok(key) = name_key(name)
            {
                out.insert(key, account);
            }
        }
        out
    }
}

/// `[at:N]` member numbers in a text.
fn mentioned(text: &str) -> HashSet<u32> {
    numbers_after(text, "[at:")
        .filter_map(|n| u32::try_from(n).ok())
        .collect()
}

/// Message ids a text quotes (`[reply:ID]`).
fn replies(text: &str) -> Vec<MessageId> {
    numbers_after(text, "[reply:")
        .filter_map(|n| MessageId::new(n).ok())
        .collect()
}

fn numbers_after<'a>(text: &'a str, prefix: &'a str) -> impl Iterator<Item = i64> + 'a {
    text.match_indices(prefix).filter_map(move |(at, _)| {
        let digits: String = text[at + prefix.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
}

/// The target line a finding cites, if it is a member's message that contains the quote.
fn source<'a>(
    target: &'a [SliceLine],
    line: u32,
    quote: &str,
) -> Result<(&'a SliceLine, String), String> {
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
    Ok((found, quote.to_owned()))
}

/// Whether the line is directed at `account`: it mentions them as `[at:N]` or replies to one of
/// their messages.
fn directed_at(cx: &FindingContext<'_>, line: &SliceLine, number: u32, account: AccountId) -> bool {
    mentioned(&line.text).contains(&number)
        || replies(&line.text)
            .into_iter()
            .any(|id| cx.author_of(id) == Some(account))
}

/// A reason a finding was not kept, naming the list and the item (counted from 1).
fn problem(list: &str, index: usize, reason: String) -> String {
    say(Text::ExtractFinding {
        list: list.to_owned(),
        item: (index + 1).to_string(),
        reason,
    })
}

fn account_of(members: &HashMap<u32, AccountId>, member: u32) -> Result<AccountId, String> {
    members.get(&member).copied().ok_or_else(|| {
        say(Text::ExtractFindingNoMember {
            member: member.to_string(),
        })
    })
}

fn name(
    cx: &FindingContext<'_>,
    members: &HashMap<u32, AccountId>,
    shown: &HashMap<String, AccountId>,
    arg: &NameArg,
) -> Result<NameFinding, String> {
    let (line, quote) = source(cx.target(), arg.line, &arg.quote)?;
    let account = account_of(members, arg.member)?;
    let member = arg.member.to_string();
    let author = line.speaker;
    let basis = match arg.basis {
        UseArg::Own if author == Some(account) => NameBasis::Own,
        UseArg::Own => return Err(say(Text::ExtractFindingNameOwnNotAuthor { member })),
        _ if author == Some(account) => {
            return Err(say(Text::ExtractFindingNameByThemselves { member }));
        }
        UseArg::Addressed if directed_at(cx, line, arg.member, account) => NameBasis::Addressed,
        UseArg::Addressed => return Err(say(Text::ExtractFindingNameNotAddressed { member })),
        UseArg::About => NameBasis::About,
    };
    let text = arg.name.trim();
    if text.is_empty() || !quote.contains(text) {
        return Err(say(Text::ExtractFindingNameNotQuoted {
            name: text.to_owned(),
        }));
    }
    let key = name_key(text).map_err(|e| match e {
        NameTextError::Empty => say(Text::ExtractFindingNameEmpty {}),
        NameTextError::TooLong => say(Text::ExtractFindingNameTooLong {}),
    })?;
    // Members call the bot by its names; those never become a member's.
    if cx.bot_names.contains(&key) {
        return Err(say(Text::ExtractFindingNameBot {
            name: text.to_owned(),
        }));
    }
    if let Some(other) = shown.get(&key).filter(|other| **other != account) {
        let other = members
            .iter()
            .find(|(_, a)| *a == other)
            .map_or_else(String::new, |(n, _)| n.to_string());
        return Err(say(Text::ExtractFindingNameShownAs {
            name: text.to_owned(),
            other,
        }));
    }
    // A name someone else used is anchored when the member answers the line that used it.
    let anchored = basis != NameBasis::Own
        && cx.parts.iter().flat_map(|p| p.iter()).any(|later| {
            later.ordinal > line.ordinal
                && later.speaker == Some(account)
                && replies(&later.text).contains(&line.message)
        });
    Ok(NameFinding {
        account,
        name: text.to_owned(),
        basis,
        anchored,
        message: line.message,
        quote,
    })
}

fn memory(
    cx: &FindingContext<'_>,
    members: &HashMap<u32, AccountId>,
    arg: &MemoryArg,
) -> Result<MemoryFinding, String> {
    let (line, quote) = source(cx.target(), arg.line, &arg.quote)?;
    let Some(author) = line.speaker else {
        return Err(say(Text::ExtractFindingBotLine {
            line: arg.line.to_string(),
        }));
    };
    let about = arg.about.trim();
    let subject = if about == "group" {
        None
    } else {
        let number = about
            .strip_prefix("member:")
            .and_then(|n| n.trim().parse::<u32>().ok())
            .ok_or_else(|| {
                say(Text::ExtractFindingAbout {
                    about: about.to_owned(),
                })
            })?;
        let account = account_of(members, number)?;
        // What one person says is filed under someone else only when the line is directed at
        // them; talk about an absent member by name is no evidence about any account.
        if author != account && !directed_at(cx, line, number, account) {
            return Err(say(Text::ExtractFindingNotSubject {
                member: number.to_string(),
            }));
        }
        Some(account)
    };
    let id = match (arg.action, arg.id.as_deref().map(str::trim)) {
        (MemoryAction::New, _) => None,
        (action, None | Some("")) => {
            return Err(say(Text::ExtractFindingNoId {
                action: format!("{action:?}").to_lowercase(),
            }));
        }
        (_, Some(raw)) => {
            let id = MemoryId::parse(raw)
                .filter(|id| cx.known.contains(&(*id, subject)))
                .ok_or_else(|| {
                    say(Text::ExtractFindingUnknownId {
                        id: raw.to_owned(),
                        about: about.to_owned(),
                    })
                })?;
            Some(id)
        }
    };
    let term = match arg.term.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => None,
        Some(_) if subject.is_some() => return Err(say(Text::ExtractFindingTermForMember {})),
        Some(term) if term.chars().count() > MAX_TERM_CHARS => {
            return Err(say(Text::ExtractFindingTermTooLong {}));
        }
        Some(term) => {
            // The quote shows what the term means; the term itself only has to be written
            // somewhere in the target, by a member.
            let written = cx
                .target()
                .iter()
                .any(|l| l.speaker.is_some() && l.text.contains(term));
            if !written {
                return Err(say(Text::ExtractFindingTermNotWritten {
                    term: term.to_owned(),
                }));
            }
            Some(term.to_owned())
        }
    };
    let (text, durability) = match arg.action {
        MemoryAction::New | MemoryAction::Changed => {
            let text = arg.text.as_deref().map(str::trim).unwrap_or_default();
            if text.is_empty() {
                return Err(say(Text::ExtractFindingEmptyText {}));
            }
            if text.chars().count() > MAX_MEMORY_CHARS {
                return Err(say(Text::ExtractFindingTextTooLong {
                    max: MAX_MEMORY_CHARS.to_string(),
                }));
            }
            let Some(durability) = arg.durability else {
                return Err(say(Text::ExtractFindingNoDurability {}));
            };
            (Some(text.to_owned()), Some(durability))
        }
        MemoryAction::Same | MemoryAction::Ended => (None, None),
    };
    Ok(MemoryFinding {
        subject,
        action: arg.action,
        id,
        term,
        text,
        durability,
        message: line.message,
        author,
        quote,
    })
}

/// Keep the findings that hold up; return the reasons for the rest. Findings never fail an
/// episode: a summary is worth keeping even when an item proposed beside it is not.
pub fn validate_findings(
    names: &[NameArg],
    memories: &[MemoryArg],
    cx: &FindingContext<'_>,
) -> (Findings, Vec<String>) {
    let members = cx.members();
    let shown = cx.shown();
    let mut kept = Findings::default();
    let mut dropped = Vec::new();

    let mut name_items = Vec::new();
    for (index, arg) in names.iter().enumerate() {
        match name(cx, &members, &shown, arg) {
            Ok(finding) => {
                name_items.push(index);
                kept.names.push(finding);
            }
            Err(reason) => dropped.push(problem("names", index, reason)),
        }
    }
    // One name for two people in one stretch of chat is a guess, not a finding: drop every such
    // name.
    let mut owners: HashMap<String, HashSet<AccountId>> = HashMap::new();
    for finding in &kept.names {
        if let Ok(key) = name_key(&finding.name) {
            owners.entry(key).or_default().insert(finding.account);
        }
    }
    let mut items = name_items.into_iter();
    kept.names.retain(|finding| {
        let index = items.next().unwrap_or_default();
        let shared = name_key(&finding.name)
            .ok()
            .and_then(|key| owners.get(&key))
            .is_some_and(|accounts| accounts.len() > 1);
        if shared {
            let reason = say(Text::ExtractFindingNameShared {
                name: finding.name.clone(),
            });
            dropped.push(problem("names", index, reason));
        }
        !shared
    });

    let mut lines = Vec::new();
    for (index, arg) in memories.iter().enumerate() {
        match memory(cx, &members, arg) {
            Ok(finding) => {
                lines.push(arg.line);
                kept.memories.push(finding);
            }
            Err(reason) => dropped.push(problem("memories", index, reason)),
        }
    }
    // One stretch of chat says one thing about each memory and each new statement: its last
    // line ("I quit it" ... "back on it again"), so applying the episode never both supports and
    // ends one memory.
    let what = |f: &MemoryFinding| match (f.id, &f.text) {
        (Some(id), _) => (f.subject, format!("#{}", id.get())),
        (None, text) => (
            f.subject,
            memory_key(f.term.as_deref(), text.as_deref().unwrap_or_default()),
        ),
    };
    let mut last: HashMap<(Option<AccountId>, String), u32> = HashMap::new();
    for (finding, line) in kept.memories.iter().zip(&lines) {
        let entry = last.entry(what(finding)).or_insert(*line);
        *entry = (*entry).max(*line);
    }
    let mut seen = HashSet::new();
    let mut at = lines.into_iter();
    kept.memories.retain(|finding| {
        let line = at.next().unwrap_or_default();
        let key = what(finding);
        last.get(&key) == Some(&line) && seen.insert(key)
    });
    (kept, dropped)
}
