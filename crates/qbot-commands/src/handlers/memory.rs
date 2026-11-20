//! `/memory`: everything the bot knows about a member or the group, in one place. What it read
//! from chat and what people entered by hand are one store; this command views it and lets a
//! person add, correct, close or forget an entry. Forgetting is final: what was forgotten is
//! never learned again from chat, unless someone enters it by hand.

use qbot_core::{AccountId, UnixMillis};
use qbot_i18n::Msg;
use qbot_memory::memories::{HandError, HandMemory, MAX_MEMORY_CHARS, forget_all};
use qbot_memory::names::{MAX_NAME_CHARS, NameTextError, name_key};
use qbot_memory::{Kind, KnownName, Memory, MemoryId, MemoryState, Origin, known_names};

use super::Reply;
use crate::args;
use crate::fail::Fail;
use crate::router::{Cx, Standing};

/// What one person may enter by hand at most: what is entered is in every run they are part of.
const HAND_ENTRIES: usize = 20;

fn storage(error: impl std::fmt::Display) -> Fail {
    Fail::Storage(error.to_string())
}

fn date(cx: &Cx<'_>, at: UnixMillis) -> String {
    cx.zoned(at)
        .map(|z| z.strftime("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

pub(crate) async fn memory(cx: &Cx<'_>) -> Reply {
    let tokens = cx.tokens();
    match tokens.first().copied() {
        None => view(cx).await,
        Some("group") if tokens.len() == 1 && cx.req.mentions.is_empty() => view_group(cx).await,
        Some("add") => add(cx, tokens.get(1) == Some(&"group")).await,
        Some("name") => name(cx).await,
        Some("fix") if tokens.len() >= 3 => fix(cx, tokens[1]).await,
        Some("end") if tokens.len() == 2 => end(cx, tokens[1]).await,
        Some("forget") if tokens.get(1) == Some(&"all") && tokens.len() == 2 => {
            forget_everything(cx).await
        }
        Some("forget") if tokens.len() == 2 => forget(cx, tokens[1]).await,
        _ => Err(Msg::UsageMemory {}.into()),
    }
}

/// The member a person command is about, after checking they have a record in the group.
async fn subject(cx: &Cx<'_>) -> Result<AccountId, Fail> {
    let account = cx.target().await?;
    if cx
        .deps
        .admin
        .member_number(cx.group(), account)
        .await?
        .is_none()
    {
        return Err(Msg::WhoNoRecord {}.into());
    }
    Ok(account)
}

/// Whether the sender may change what is known about the group: bot owners and the group's
/// delegated admins.
fn manages_group(cx: &Cx<'_>) -> bool {
    cx.owner || cx.standing == Standing::DelegatedAdmin
}

/// Whether the sender may change this memory; refuses as not found what they may not even see.
async fn may_change(cx: &Cx<'_>, memory: &Memory) -> Result<(), Fail> {
    let Some(subject) = memory.subject else {
        return if manages_group(cx) {
            Ok(())
        } else {
            Err(Msg::MemoryGroupAdmins {}.into())
        };
    };
    let mut reachable = cx.may_act_on(subject).await?;
    if !reachable && let Some(with) = memory.with {
        reachable = cx.may_act_on(with).await?;
    }
    if !reachable {
        return Err(Msg::MemoryNoSuch {
            id: memory.id.label(),
        }
        .into());
    }
    // What a bot owner entered about someone stays theirs against a delegated admin: lower
    // authority never undoes higher. The member it is about may.
    if let Origin::Hand { by } = memory.origin
        && cx.is_owner(by)
        && !cx.owner
        && cx.on_someone_else(subject).await?
    {
        return Err(Msg::MemoryOwnerEntered {}.into());
    }
    Ok(())
}

/// A memory by the id a person typed, in this group.
async fn by_id(cx: &Cx<'_>, raw: &str) -> Result<Memory, Fail> {
    let no_such = || Fail::Say(Msg::MemoryNoSuch { id: raw.to_owned() });
    let id = MemoryId::parse(raw).ok_or_else(no_such)?;
    cx.deps
        .memories
        .memory(cx.group(), id)
        .await
        .map_err(storage)?
        .ok_or_else(no_such)
}

fn check_text(kind: Kind, text: &str) -> Result<(), Fail> {
    match kind {
        Kind::Name => match name_key(text) {
            Ok(_) => Ok(()),
            Err(NameTextError::Empty) => Err(Msg::MemoryEmpty {}.into()),
            Err(NameTextError::TooLong) => Err(Msg::MemoryNameTooLong {
                max: MAX_NAME_CHARS as u32,
            }
            .into()),
        },
        Kind::Statement if text.is_empty() => Err(Msg::MemoryEmpty {}.into()),
        Kind::Statement if text.chars().count() > MAX_MEMORY_CHARS => Err(Msg::MemoryTooLong {
            max: MAX_MEMORY_CHARS as u32,
        }
        .into()),
        Kind::Statement => Ok(()),
    }
}

fn hand_error(error: HandError) -> Fail {
    match error {
        HandError::Known(id) => Msg::MemoryKnownAlready { id: id.label() }.into(),
        HandError::NotFound | HandError::Refused => Msg::UsageMemory {}.into(),
        HandError::Backend(error) => Fail::Storage(error),
    }
}

/// Everything about one person's accounts in this group, by id.
async fn about(cx: &Cx<'_>, accounts: &[AccountId]) -> Result<Vec<Memory>, Fail> {
    let mut all = Vec::new();
    for account in accounts {
        all.extend(
            cx.deps
                .memories
                .about(cx.group(), Some(*account))
                .await
                .map_err(storage)?,
        );
    }
    all.sort_by_key(|m| m.id);
    Ok(all)
}

/// How a name is used, as members read it.
fn name_line(cx: &Cx<'_>, name: &KnownName) -> String {
    let mut how = Vec::new();
    if name.set {
        how.push(cx.t(&Msg::MemoryNameHand {}));
    }
    if name.own > 0 {
        how.push(cx.t(&Msg::MemoryNameOwn {
            count: u64::from(name.own),
        }));
    }
    let others = name.addressed + name.about;
    if others > 0 {
        how.push(cx.t(&Msg::MemoryNameOthers {
            count: u64::from(others),
            date: date(cx, name.last),
        }));
    }
    cx.t(&Msg::MemoryNameLine {
        id: name.id.label(),
        text: name.text.trim().to_owned(),
        how: how.join(&cx.t(&Msg::MemoryHowSeparator {})),
    })
}

/// A current statement as members read it, with where it comes from.
async fn statement_line(cx: &Cx<'_>, memory: &Memory) -> String {
    if let Origin::Hand { by } = memory.origin {
        return cx.t(&Msg::MemoryHandLine {
            id: memory.id.label(),
            text: memory.display(),
            by: cx.name_of(by).await,
            date: date(cx, memory.first),
        });
    }
    let source = if memory.subject.is_none() {
        Msg::MemorySourceMembers {}
    } else {
        let own = memory.own_word();
        if own == 0 {
            Msg::MemorySourceOthers {}
        } else if own == memory.episodes() {
            Msg::MemorySourceOwn {}
        } else {
            Msg::MemorySourceBoth {}
        }
    };
    cx.t(&Msg::MemoryLine {
        id: memory.id.label(),
        text: memory.display(),
        source: cx.t(&source),
        count: u64::from(memory.episodes().max(1)),
        date: date(cx, memory.last),
    })
}

/// What no longer holds, as members read it.
fn closed_line(cx: &Cx<'_>, memory: &Memory) -> Option<String> {
    let (id, text) = (memory.id.label(), memory.display());
    Some(cx.t(&match memory.state {
        MemoryState::Replaced { at, .. } => Msg::MemoryReplacedLine {
            id,
            text,
            date: date(cx, at),
        },
        MemoryState::Ended { at } => Msg::MemoryEndedLine {
            id,
            text,
            date: date(cx, at),
        },
        MemoryState::Current => return None,
    }))
}

/// The statements: current ones, then what no longer holds.
async fn statement_lines(cx: &Cx<'_>, memories: &[Memory], lines: &mut Vec<String>) {
    let current: Vec<&Memory> = memories
        .iter()
        .filter(|m| m.kind == Kind::Statement && m.is_current())
        .collect();
    if current.is_empty() {
        lines.push(cx.t(&Msg::MemoryNothing {}));
    } else {
        lines.push(cx.t(&Msg::MemoryKnown {
            count: current.len() as u64,
        }));
        for memory in current {
            lines.push(statement_line(cx, memory).await);
        }
    }
    let closed: Vec<String> = memories
        .iter()
        .filter(|m| !m.is_current())
        .filter_map(|m| closed_line(cx, m))
        .collect();
    if !closed.is_empty() {
        lines.push(cx.t(&Msg::MemoryHistory {
            count: closed.len() as u64,
        }));
        lines.extend(closed);
    }
}

async fn view(cx: &Cx<'_>) -> Reply {
    let account = subject(cx).await?;
    let accounts = cx.linked_accounts(account).await?;
    let mut lines = vec![
        cx.t(&Msg::MemoryTitle {
            display: cx.name_of(account).await,
        }),
        cx.t(&Msg::MemoryMessages {
            count: cx
                .deps
                .admin
                .message_count(cx.group(), &accounts)
                .await?
                .to_string(),
        }),
    ];
    if accounts.len() > 1 {
        lines.push(cx.t(&Msg::MemoryLinked {
            count: accounts.len().to_string(),
        }));
    }
    let names_all = known_names(&cx.deps.memories.names(cx.group()).await.map_err(storage)?);
    let mut mine: Vec<&KnownName> = names_all
        .iter()
        .filter(|n| accounts.contains(&n.account))
        .collect();
    mine.sort_by(|a, b| {
        b.set
            .cmp(&a.set)
            .then(b.uses().cmp(&a.uses()))
            .then_with(|| a.key.cmp(&b.key))
    });
    if !mine.is_empty() {
        lines.push(cx.t(&Msg::MemoryNames {
            count: mine.len() as u64,
        }));
        for name in mine {
            lines.push(name_line(cx, name));
        }
    }
    let memories = about(cx, &accounts).await?;
    statement_lines(cx, &memories, &mut lines).await;
    lines.push(cx.t(&Msg::MemoryHint {}));
    Ok(cx.fit(&lines.join("\n")))
}

async fn view_group(cx: &Cx<'_>) -> Reply {
    let memories = cx
        .deps
        .memories
        .about(cx.group(), None)
        .await
        .map_err(storage)?;
    let mut lines = vec![cx.t(&Msg::MemoryGroupTitle {})];
    statement_lines(cx, &memories, &mut lines).await;
    lines.push(cx.t(&Msg::MemoryHint {}));
    Ok(cx.fit(&lines.join("\n")))
}

/// Refuse an entry by hand when the person has as many as they may.
async fn room_for_hand(cx: &Cx<'_>, account: AccountId) -> Result<(), Fail> {
    let accounts = cx.linked_accounts(account).await?;
    let entered = about(cx, &accounts)
        .await?
        .iter()
        .filter(|m| m.is_current() && m.origin.is_hand())
        .count();
    if entered >= HAND_ENTRIES {
        return Err(Msg::MemoryTooMany {
            max: HAND_ENTRIES as u32,
        }
        .into());
    }
    Ok(())
}

async fn add(cx: &Cx<'_>, group: bool) -> Reply {
    let text = args::after_words(&cx.req.args, if group { 2 } else { 1 });
    check_text(Kind::Statement, text)?;
    let (subject, display) = if group {
        if !cx.req.mentions.is_empty() {
            return Err(Msg::UsageMemory {}.into());
        }
        if !manages_group(cx) {
            return Err(Msg::MemoryGroupAdmins {}.into());
        }
        (None, None)
    } else {
        let account = subject(cx).await?;
        room_for_hand(cx, account).await?;
        (Some(account), Some(cx.name_of(account).await))
    };
    let id = cx
        .deps
        .memories
        .add(
            cx.group(),
            &HandMemory {
                subject,
                kind: Kind::Statement,
                term: None,
                text: text.to_owned(),
                by: cx.req.sender,
                at: cx.deps.clock.now(),
            },
        )
        .await
        .map_err(hand_error)?;
    Ok(cx.t(&match display {
        Some(display) => Msg::MemoryAdded {
            id: id.label(),
            display,
        },
        None => Msg::MemoryAddedGroup { id: id.label() },
    }))
}

async fn name(cx: &Cx<'_>) -> Reply {
    let text = qbot_memory::names::tidy_name(args::after_words(&cx.req.args, 1));
    check_text(Kind::Name, &text)?;
    let account = subject(cx).await?;
    room_for_hand(cx, account).await?;
    let id = cx
        .deps
        .memories
        .add(
            cx.group(),
            &HandMemory {
                subject: Some(account),
                kind: Kind::Name,
                term: None,
                text: text.clone(),
                by: cx.req.sender,
                at: cx.deps.clock.now(),
            },
        )
        .await
        .map_err(hand_error)?;
    Ok(cx.t(&Msg::MemoryNameAdded {
        id: id.label(),
        display: cx.name_of(account).await,
        name: text,
    }))
}

async fn fix(cx: &Cx<'_>, raw: &str) -> Reply {
    let memory = by_id(cx, raw).await?;
    may_change(cx, &memory).await?;
    if !memory.is_current() {
        return Err(Msg::MemoryNotCurrent {
            id: memory.id.label(),
        }
        .into());
    }
    let text = match memory.kind {
        Kind::Name => qbot_memory::names::tidy_name(args::after_words(&cx.req.args, 2)),
        Kind::Statement => args::after_words(&cx.req.args, 2).to_owned(),
    };
    check_text(memory.kind, &text)?;
    let new = cx
        .deps
        .memories
        .fix(
            cx.group(),
            memory.id,
            &text,
            cx.req.sender,
            cx.deps.clock.now(),
        )
        .await
        .map_err(hand_error)?;
    Ok(cx.t(&Msg::MemoryFixed {
        old: memory.id.label(),
        new: new.label(),
        text,
    }))
}

async fn end(cx: &Cx<'_>, raw: &str) -> Reply {
    let memory = by_id(cx, raw).await?;
    may_change(cx, &memory).await?;
    if !memory.is_current() {
        return Err(Msg::MemoryNotCurrent {
            id: memory.id.label(),
        }
        .into());
    }
    cx.deps
        .memories
        .end(cx.group(), memory.id, cx.deps.clock.now())
        .await
        .map_err(hand_error)?;
    Ok(cx.t(&Msg::MemoryEnded {
        id: memory.id.label(),
        text: memory.display(),
    }))
}

async fn forget(cx: &Cx<'_>, raw: &str) -> Reply {
    let memory = by_id(cx, raw).await?;
    may_change(cx, &memory).await?;
    cx.deps
        .memories
        .forget(cx.group(), memory.id, cx.req.sender, cx.deps.clock.now())
        .await
        .map_err(storage)?;
    Ok(cx.t(&Msg::MemoryForgot {
        id: memory.id.label(),
        text: memory.display(),
    }))
}

async fn forget_everything(cx: &Cx<'_>) -> Reply {
    let account = subject(cx).await?;
    let accounts = cx.linked_accounts(account).await?;
    let mut ids = Vec::new();
    for memory in about(cx, &accounts).await? {
        if may_change(cx, &memory).await.is_ok() {
            ids.push(memory.id);
        }
    }
    let count = forget_all(
        cx.deps.memories.as_ref(),
        cx.group(),
        &ids,
        cx.req.sender,
        cx.deps.clock.now(),
    )
    .await
    .map_err(storage)?;
    Ok(cx.t(&Msg::MemoryForgotAll {
        display: cx.name_of(account).await,
        count,
    }))
}
