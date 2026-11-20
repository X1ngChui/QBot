//! What the batch's reading of one episode found besides its summary: the names its lines use
//! for people (references) and statements worth remembering about members and the group. The
//! model reads who is meant; code checks what can be checked against the lines themselves (the
//! expression is in the member's words, a marker is where it is said to be, an answer is a line
//! the person wrote) and decides what is kept. How a statement relates to what is already known
//! is decided later, per subject, with all of the night's statements about it in view.

use std::collections::{HashMap, HashSet};

use qbot_core::{AccountId, MessageId, UnixMillis};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::memories::{About, Durability, MAX_MEMORY_CHARS, MAX_TERM_CHARS, memory_key};
use crate::names::{Grounds, NameBasis, NameTextError, Referent, name_key, tidy_name};
use crate::slice::SliceLine;

/// An expression a line uses for a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceFinding {
    pub message: MessageId,
    pub ordinal: u64,
    pub at: UnixMillis,
    pub speaker: AccountId,
    /// The expression as written.
    pub text: String,
    pub referent: Referent,
    /// How the line uses it.
    pub basis: NameBasis,
    pub grounds: Grounds,
    /// The line is said to the member (it replies to them or mentions them), or they reply to it.
    #[serde(default)]
    pub anchored: bool,
    #[serde(default)]
    pub shown_by: Vec<MessageId>,
}

/// A statement about a member, the group, or a person known only by a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementFinding {
    pub about: About,
    pub text: String,
    /// For a group term.
    pub term: Option<String>,
    pub durability: Durability,
    /// A second member the statement involves.
    pub with: Option<AccountId>,
    pub message: MessageId,
    pub ordinal: u64,
    pub at: UnixMillis,
    /// Who wrote the line.
    pub author: AccountId,
    pub quote: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Findings {
    pub references: Vec<ReferenceFinding>,
    pub statements: Vec<StatementFinding>,
}

impl Findings {
    pub fn is_empty(&self) -> bool {
        self.references.is_empty() && self.statements.is_empty()
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
pub struct StatementArg {
    pub about: String,
    pub text: String,
    pub durability: Durability,
    #[serde(default)]
    pub term: Option<String>,
    #[serde(default)]
    pub with: Option<String>,
    pub line: u32,
    pub quote: String,
}

/// What validation reads besides the arguments.
#[derive(Debug, Clone, Copy)]
pub struct FindingContext<'a> {
    /// The context before, the target and the context after.
    pub parts: [&'a [SliceLine]; 3],
    /// The number the first target line is shown with (`#first`); the target's lines are
    /// numbered on from it.
    pub first: u32,
    /// Older lines the shown ones quote.
    pub quoted: &'a [SliceLine],
    /// Accounts of members the background lists who wrote none of the lines.
    pub accounts: &'a [(u32, AccountId)],
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

/// The target line a finding cites (numbered from `first`), if it is something a member said.
fn said(target: &[SliceLine], first: u32, line: u32) -> Result<(&SliceLine, AccountId), String> {
    let Some(found) = line
        .checked_sub(first)
        .and_then(|index| target.get(index as usize))
    else {
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
    first: u32,
    line: u32,
    quote: &str,
) -> Result<(&'a SliceLine, AccountId, String), String> {
    let (found, speaker) = said(target, first, line)?;
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
    let (line, speaker) = said(cx.target(), cx.first, arg.line)?;
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
        at: line.at,
        speaker,
        text: text.to_owned(),
        referent,
        basis,
        grounds,
        anchored,
        shown_by,
    })
}

fn statement(
    cx: &FindingContext<'_>,
    members: &HashMap<u32, AccountId>,
    references: &[ReferenceFinding],
    arg: &StatementArg,
) -> Result<StatementFinding, String> {
    let (line, author, quote) = source(cx.target(), cx.first, arg.line, &arg.quote)?;
    let about = arg.about.trim();
    let subject = if about == "group" {
        About::Group
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
        About::Name(tidy_name(name))
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
        About::Member(account)
    };
    let with = match arg.with.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
        None => None,
        Some(raw) => {
            let About::Member(account) = subject else {
                return Err(say(Text::ExtractFindingWithNotMember {}));
            };
            let number = member_number(raw).ok_or_else(|| {
                say(Text::ExtractFindingAbout {
                    about: raw.to_owned(),
                })
            })?;
            let other = account_of(members, number)?;
            if other == account {
                return Err(say(Text::ExtractFindingWithSelf {}));
            }
            Some(other)
        }
    };
    let text = arg.text.trim();
    if text.is_empty() {
        return Err(say(Text::ExtractFindingEmptyText {}));
    }
    if text.chars().count() > MAX_MEMORY_CHARS {
        return Err(say(Text::ExtractFindingTextTooLong {
            max: MAX_MEMORY_CHARS.to_string(),
        }));
    }
    let term = match arg.term.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => None,
        Some(_) if subject != About::Group => {
            return Err(say(Text::ExtractFindingTermForMember {}));
        }
        Some(term) if term.chars().count() > MAX_TERM_CHARS => {
            return Err(say(Text::ExtractFindingTermTooLong {}));
        }
        // A name a member is called by is a reference to them, never the group's vocabulary.
        Some(_) if text.contains("member:") => {
            return Err(say(Text::ExtractFindingTermNamesMember {}));
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
    Ok(StatementFinding {
        about: subject,
        text: text.to_owned(),
        term,
        durability: arg.durability,
        with,
        message: line.message,
        ordinal: line.ordinal,
        at: line.at,
        author,
        quote,
    })
}

/// What validation kept, and why it left out the rest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Checked {
    pub findings: Findings,
    /// Statements not kept: worth one correction, since each is something learned lost.
    pub dropped: Vec<String>,
    /// References not kept. A reference is one record among many, so these are logged and sent
    /// back only alongside a statement's correction.
    pub unread: Vec<String>,
}

/// Keep the findings that hold up; return the reasons for the rest. Findings never fail an
/// episode: a summary is worth keeping even when an item proposed beside it is not.
pub fn validate_findings(
    references: &[ReferenceArg],
    statements: &[StatementArg],
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

    for (index, arg) in statements.iter().enumerate() {
        match statement(cx, &members, &checked.findings.references, arg) {
            Ok(finding) => checked.findings.statements.push(finding),
            Err(reason) => checked.dropped.push(problem("statements", index, reason)),
        }
    }
    // One stretch of chat says one thing once: the same statement about the same subject is
    // kept from its last line.
    let key = |f: &StatementFinding| (f.about.clone(), memory_key(f.term.as_deref(), &f.text));
    let mut last: HashMap<(About, String), u64> = HashMap::new();
    for f in &checked.findings.statements {
        let entry = last.entry(key(f)).or_insert(f.ordinal);
        *entry = (*entry).max(f.ordinal);
    }
    let mut seen = HashSet::new();
    checked
        .findings
        .statements
        .retain(|f| last.get(&key(f)) == Some(&f.ordinal) && seen.insert(key(f)));
    checked
}
