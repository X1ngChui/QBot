//! What an episode's extraction found besides the summary: the expressions its lines use for
//! people (references) and statements worth remembering about members and the group. The model
//! reads who is meant; code checks what can be checked against the lines themselves (the
//! expression is in the member's words, a marker is where it is said to be, an answer is a line
//! the person wrote) and decides what is kept.

use std::collections::{HashMap, HashSet};

use qbot_core::{AccountId, MessageId};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::memories::{Durability, MAX_MEMORY_CHARS, MAX_TERM_CHARS, MemoryId, memory_key};
use crate::names::{Grounds, NameBasis, NameTextError, Referent, name_key};
use crate::slice::SliceLine;

/// An expression a line uses for a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceFinding {
    pub message: MessageId,
    pub ordinal: u64,
    pub speaker: AccountId,
    /// The expression as written.
    pub text: String,
    pub referent: Referent,
    /// How the line uses it; never [`NameBasis::Set`].
    pub basis: NameBasis,
    pub grounds: Grounds,
    /// The line is said to the member (it replies to them or mentions them), or they reply to it.
    #[serde(default)]
    pub anchored: bool,
    #[serde(default)]
    pub shown_by: Vec<MessageId>,
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

/// A statement about a member, the group, or a person known only by a name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFinding {
    /// `None`: the group, or (with `name`) a person nobody has placed yet.
    pub subject: Option<AccountId>,
    /// The name of a person the lines did not let the reader place: the statement is held until
    /// the name is some member's.
    #[serde(default)]
    pub name: Option<String>,
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
    pub references: Vec<ReferenceFinding>,
    #[serde(default)]
    pub memories: Vec<MemoryFinding>,
}

impl Findings {
    pub fn is_empty(&self) -> bool {
        self.references.is_empty() && self.memories.is_empty()
    }
}

/// How a line uses an expression, as the model may say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoleArg {
    Address,
    About,
    #[serde(rename = "self")]
    Own,
}

/// What shows who is meant, as the model may say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BasisArg {
    Marker,
    Answer,
    Content,
    #[serde(rename = "self")]
    Own,
    Listed,
    None,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReferenceArg {
    pub line: u32,
    pub text: String,
    pub refers: String,
    pub role: RoleArg,
    pub basis: BasisArg,
    #[serde(default)]
    pub shown_by: Vec<i64>,
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
    /// Accounts of members the background lists who wrote none of the lines.
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

    fn shown_lines(&self) -> impl Iterator<Item = &SliceLine> {
        self.parts.iter().flat_map(|p| p.iter())
    }

    /// Member numbers of the lines and their accounts.
    fn members(&self) -> HashMap<u32, AccountId> {
        let mut out: HashMap<u32, AccountId> = self.accounts.iter().copied().collect();
        for line in self.shown_lines() {
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

    /// Whether the message is among the lines shown and quoted.
    fn shows(&self, id: MessageId) -> bool {
        self.shown_lines()
            .chain(self.quoted)
            .any(|l| l.message == id)
    }

    /// The names each member is shown under in the lines, normalized.
    fn shown(&self) -> HashMap<String, AccountId> {
        let mut out = HashMap::new();
        for line in self.shown_lines() {
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

/// What the member typed on a line: the text before any forwarded record, without what stands
/// in brackets (what the message carried, whom it mentions, what it replies to).
fn own_words(text: &str) -> String {
    let typed = text.find("[forward:").map_or(text, |at| &text[..at]);
    let mut out = String::with_capacity(typed.len());
    let mut rest = typed;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        match rest[open..].find(']') {
            Some(close) => {
                out.push(' ');
                rest = &rest[open + close + 1..];
            }
            None => {
                rest = &rest[open..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The target line a finding cites, if it is something a member said.
fn said(target: &[SliceLine], line: u32) -> Result<(&SliceLine, AccountId), String> {
    let Some(found) = target.get((line as usize).wrapping_sub(1)) else {
        return Err(say(Text::ExtractFindingLine {
            line: line.to_string(),
        }));
    };
    let Some(speaker) = found.speaker else {
        return Err(say(Text::ExtractFindingBotLine {
            line: line.to_string(),
        }));
    };
    if found.text.starts_with("[notice:") {
        return Err(say(Text::ExtractFindingEvent {
            line: line.to_string(),
        }));
    }
    Ok((found, speaker))
}

/// The target line a statement cites, if it is a member's message that contains the quote.
fn source<'a>(
    target: &'a [SliceLine],
    line: u32,
    quote: &str,
) -> Result<(&'a SliceLine, AccountId, String), String> {
    let (found, speaker) = said(target, line)?;
    let quote = quote.trim();
    if quote.is_empty() || !found.text.contains(quote) {
        return Err(say(Text::ExtractFindingQuote {
            line: line.to_string(),
        }));
    }
    Ok((found, speaker, quote.to_owned()))
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

fn member_number(text: &str) -> Option<u32> {
    text.strip_prefix("member:")
        .and_then(|n| n.trim().parse().ok())
}

fn reference(
    cx: &FindingContext<'_>,
    members: &HashMap<u32, AccountId>,
    shown: &HashMap<String, AccountId>,
    arg: &ReferenceArg,
) -> Result<ReferenceFinding, String> {
    let (line, speaker) = said(cx.target(), arg.line)?;
    let text = arg.text.trim();
    let key = name_key(text).map_err(|e| match e {
        NameTextError::Empty => say(Text::ExtractFindingNameEmpty {}),
        NameTextError::TooLong => say(Text::ExtractFindingNameTooLong {}),
    })?;
    if !own_words(&line.text).contains(text) {
        return Err(say(Text::ExtractFindingNameNotWritten {
            name: text.to_owned(),
            line: arg.line.to_string(),
        }));
    }
    let refers = arg.refers.trim();
    let named = match refers {
        "bot" => Referent::Bot,
        "outside" => Referent::Outside,
        "unknown" => Referent::Unknown,
        other => {
            let number = member_number(other).ok_or_else(|| {
                say(Text::ExtractFindingRefers {
                    refers: other.to_owned(),
                })
            })?;
            Referent::Member(account_of(members, number)?)
        }
    };
    // Members call the bot by its names; those never become a member's.
    let referent = if cx.bot_names.contains(&key) {
        Referent::Bot
    } else {
        named
    };
    let spoken = match arg.role {
        RoleArg::Address => NameBasis::Addressed,
        RoleArg::About | RoleArg::Own => NameBasis::About,
    };
    let (basis, grounds, anchored) = match referent {
        Referent::Member(account) if account == speaker => (NameBasis::Own, Grounds::Own, false),
        Referent::Member(account) => {
            let number = members
                .iter()
                .find(|(_, a)| **a == account)
                .map_or(0, |(n, _)| *n);
            let member = number.to_string();
            if arg.role == RoleArg::Own {
                return Err(say(Text::ExtractFindingNameOwnNotAuthor { member }));
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
            let directed = directed_at(cx, line, number, account);
            let answered = cx.shown_lines().any(|later| {
                later.ordinal > line.ordinal
                    && later.speaker == Some(account)
                    && replies(&later.text).contains(&line.message)
            });
            let grounds = match arg.basis {
                BasisArg::Marker if directed => Grounds::Marker,
                BasisArg::Marker => {
                    return Err(say(Text::ExtractFindingNameNotAddressed { member }));
                }
                // An answer is a line the person wrote; which one, the model says.
                BasisArg::Answer => {
                    let theirs = arg
                        .shown_by
                        .iter()
                        .filter_map(|id| MessageId::new(*id).ok())
                        .any(|id| cx.author_of(id) == Some(account));
                    if !theirs {
                        return Err(say(Text::ExtractFindingNameNoAnswer { member }));
                    }
                    Grounds::Answer
                }
                BasisArg::Content => Grounds::Content,
                BasisArg::Listed => Grounds::Listed,
                BasisArg::Own => {
                    return Err(say(Text::ExtractFindingNameOwnNotAuthor { member }));
                }
                BasisArg::None => return Err(say(Text::ExtractFindingNameNoBasis { member })),
            };
            (spoken, grounds, directed || answered)
        }
        Referent::Unknown => (spoken, Grounds::None, false),
        Referent::Bot | Referent::Outside => {
            let grounds = match arg.basis {
                BasisArg::Answer => Grounds::Answer,
                BasisArg::Content => Grounds::Content,
                BasisArg::Listed => Grounds::Listed,
                BasisArg::Marker | BasisArg::Own | BasisArg::None => Grounds::None,
            };
            (spoken, grounds, false)
        }
    };
    let mut shown_by: Vec<MessageId> = arg
        .shown_by
        .iter()
        .filter_map(|id| MessageId::new(*id).ok())
        .filter(|id| cx.shows(*id))
        .collect();
    shown_by.dedup();
    Ok(ReferenceFinding {
        message: line.message,
        ordinal: line.ordinal,
        speaker,
        text: text.to_owned(),
        referent,
        basis,
        grounds,
        anchored,
        shown_by,
    })
}

fn memory(
    cx: &FindingContext<'_>,
    members: &HashMap<u32, AccountId>,
    references: &[ReferenceFinding],
    arg: &MemoryArg,
) -> Result<MemoryFinding, String> {
    let (line, author, quote) = source(cx.target(), arg.line, &arg.quote)?;
    let about = arg.about.trim();
    let (subject, name) = if about == "group" {
        (None, None)
    } else if let Some(name) = about.strip_prefix("name:") {
        // A person the lines name but do not place: the statement waits for the name.
        let name = name.trim();
        let unplaced = name_key(name).ok().is_some_and(|key| {
            references.iter().any(|r| {
                r.referent == Referent::Unknown && name_key(&r.text).ok().as_ref() == Some(&key)
            })
        });
        if !unplaced {
            return Err(say(Text::ExtractFindingNameNotRecorded {
                name: name.to_owned(),
            }));
        }
        if arg.action != MemoryAction::New {
            return Err(say(Text::ExtractFindingNameAction {}));
        }
        (None, Some(name.to_owned()))
    } else {
        let number = member_number(about).ok_or_else(|| {
            say(Text::ExtractFindingAbout {
                about: about.to_owned(),
            })
        })?;
        let account = account_of(members, number)?;
        // What one person says is filed under another when the line is said to them, or when
        // the reading of these lines names them: an expression recorded as meaning them.
        let named = references
            .iter()
            .any(|r| r.referent == Referent::Member(account));
        if author != account && !named && !directed_at(cx, line, number, account) {
            return Err(say(Text::ExtractFindingNotSubject {
                member: number.to_string(),
            }));
        }
        (Some(account), None)
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
        Some(_) if subject.is_some() || name.is_some() => {
            return Err(say(Text::ExtractFindingTermForMember {}));
        }
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
        name,
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

/// What validation kept, and why it left out the rest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Checked {
    pub findings: Findings,
    /// Statements not kept: worth one correction, since each is a memory lost.
    pub dropped: Vec<String>,
    /// References not kept. A reference is one record among many, so these are logged and sent
    /// back only alongside a statement's correction.
    pub unread: Vec<String>,
}

/// Keep the findings that hold up; return the reasons for the rest. Findings never fail an
/// episode: a summary is worth keeping even when an item proposed beside it is not.
pub fn validate_findings(
    references: &[ReferenceArg],
    memories: &[MemoryArg],
    cx: &FindingContext<'_>,
) -> Checked {
    let members = cx.members();
    let shown = cx.shown();
    let mut checked = Checked::default();

    // One reading of an expression per line.
    let mut read: HashSet<(MessageId, String)> = HashSet::new();
    for (index, arg) in references.iter().enumerate() {
        match reference(cx, &members, &shown, arg) {
            Ok(finding) => {
                let key = name_key(&finding.text).unwrap_or_default();
                if read.insert((finding.message, key)) {
                    checked.findings.references.push(finding);
                }
            }
            Err(reason) => checked.unread.push(problem("references", index, reason)),
        }
    }

    let mut lines = Vec::new();
    for (index, arg) in memories.iter().enumerate() {
        match memory(cx, &members, &checked.findings.references, arg) {
            Ok(finding) => {
                lines.push(arg.line);
                checked.findings.memories.push(finding);
            }
            Err(reason) => checked.dropped.push(problem("memories", index, reason)),
        }
    }
    // One stretch of chat says one thing about each memory and each new statement: its last
    // line ("I quit it" ... "back on it again"), so applying the episode never both supports and
    // ends one memory.
    type Said = (Option<AccountId>, Option<String>, String);
    let what = |f: &MemoryFinding| -> Said {
        let name = f.name.as_deref().and_then(|n| name_key(n).ok());
        match (f.id, &f.text) {
            (Some(id), _) => (f.subject, name, format!("#{}", id.get())),
            (None, text) => (
                f.subject,
                name,
                memory_key(f.term.as_deref(), text.as_deref().unwrap_or_default()),
            ),
        }
    };
    let mut last: HashMap<Said, u32> = HashMap::new();
    for (finding, line) in checked.findings.memories.iter().zip(&lines) {
        let entry = last.entry(what(finding)).or_insert(*line);
        *entry = (*entry).max(*line);
    }
    let mut seen = HashSet::new();
    let mut at = lines.into_iter();
    checked.findings.memories.retain(|finding| {
        let line = at.next().unwrap_or_default();
        let key = what(finding);
        last.get(&key) == Some(&line) && seen.insert(key)
    });
    checked
}
