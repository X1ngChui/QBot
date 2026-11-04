use qbot_core::{AccountId, UnixMillis};
use qbot_i18n::Msg;
use qbot_memory::memories::{Memory, MemoryId};
use qbot_memory::names::{KnownName, MAX_NAME_CHARS, NameError, NameTextError};

use super::Reply;
use crate::args;
use crate::catalog::Command;
use crate::fail::Fail;
use crate::router::Cx;

pub(crate) async fn help(cx: &Cx<'_>) -> Reply {
    if !cx.req.mentions.is_empty() {
        return Err(Msg::UsageHelp {}.into());
    }
    let tokens = cx.tokens();
    match tokens.as_slice() {
        [] => {
            let mut lines = vec![cx.t(&Msg::HelpHeader {})];
            let mut last = None;
            for command in Command::ALL {
                if last != Some(command.category()) {
                    lines.push(format!("\n[{}]", cx.t(&command.category().title())));
                    last = Some(command.category());
                }
                let mut line = cx.t(&Msg::HelpEntry {
                    command: format!("/{}", command.name()),
                    what: cx.t(&command.what()),
                });
                if command.owner_only() {
                    line.push(' ');
                    line.push_str(&cx.t(&if command.delegated() {
                        Msg::HelpDelegatedTag {}
                    } else {
                        Msg::HelpOwnerTag {}
                    }));
                }
                lines.push(line);
            }
            lines.push(format!("\n{}", cx.t(&Msg::HelpFooter {})));
            Ok(lines.join("\n"))
        }
        [name] => {
            let command = Command::find(name).ok_or_else(|| Msg::HelpUnknown {
                name: (*name).to_owned(),
            })?;
            let access = cx.t(&if command.delegated() {
                Msg::HelpAccessDelegated {}
            } else if command.owner_only() {
                Msg::HelpAccessOwner {}
            } else {
                Msg::HelpAccessMember {}
            });
            Ok(cx.t(&Msg::HelpDetail {
                command: format!("/{}", command.name()),
                what: cx.t(&command.what()),
                access,
                detail: cx.t(&command.detail()),
            }))
        }
        _ => Err(Msg::UsageHelp {}.into()),
    }
}

/// The accounts a scoped command covers: the account, or the person's accounts in this group.
async fn scope_accounts(
    cx: &Cx<'_>,
    account: AccountId,
    all: bool,
) -> Result<Vec<AccountId>, Fail> {
    if all {
        cx.linked_accounts(account).await
    } else {
        Ok(vec![account])
    }
}

fn storage(error: impl std::fmt::Display) -> Fail {
    Fail::Storage(error.to_string())
}

fn date(cx: &Cx<'_>, at: UnixMillis) -> String {
    jiff::Timestamp::from_millisecond(at.get())
        .map(|t| {
            t.to_zoned(cx.deps.settings.zone.clone())
                .strftime("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

/// How a name is used, as members read it: set by hand, given by the member, used by others,
/// and whom else it is used for.
async fn name_line(
    cx: &Cx<'_>,
    name: &KnownName,
    all: &[KnownName],
    own: &[AccountId],
) -> Result<String, Fail> {
    let mut how = Vec::new();
    if name.set {
        how.push(cx.t(&Msg::NameHowSet {}));
    }
    if name.own > 0 {
        how.push(cx.t(&Msg::NameHowOwn {
            count: u64::from(name.own),
        }));
    }
    let others = name.addressed + name.about;
    if others > 0 {
        how.push(cx.t(&Msg::NameHowOthers {
            count: u64::from(others),
            date: date(cx, name.last),
        }));
    }
    let mut shared = Vec::new();
    for other in all
        .iter()
        .filter(|n| n.key == name.key && !own.contains(&n.account))
    {
        shared.push(cx.name_of(other.account).await);
    }
    if !shared.is_empty() {
        how.push(cx.t(&Msg::NameHowShared {
            members: cx.join_names(&shared),
        }));
    }
    Ok(cx.t(&Msg::NameLine {
        text: name.text.trim().to_owned(),
        how: how.join("; "),
    }))
}

/// The names of these accounts, one line each, set names first, then the most used.
async fn name_lines(cx: &Cx<'_>, accounts: &[AccountId]) -> Result<Vec<String>, Fail> {
    let all = cx
        .deps
        .names
        .names(cx.group(), None)
        .await
        .map_err(storage)?;
    let mut mine: Vec<&KnownName> = all
        .iter()
        .filter(|n| accounts.contains(&n.account))
        .collect();
    mine.sort_by(|a, b| {
        b.set
            .cmp(&a.set)
            .then(b.uses().cmp(&a.uses()))
            .then_with(|| a.key.cmp(&b.key))
    });
    let mut lines = Vec::new();
    for name in mine {
        lines.push(name_line(cx, name, &all, accounts).await?);
    }
    Ok(lines)
}

/// Who said a memory, as members read it.
fn source(cx: &Cx<'_>, memory: &Memory) -> String {
    if memory.subject.is_none() {
        return cx.t(&Msg::MemorySourceMembers {});
    }
    let own = memory.own_word();
    cx.t(&if own == 0 {
        Msg::MemorySourceOthers {}
    } else if own == memory.episodes() {
        Msg::MemorySourceOwn {}
    } else {
        Msg::MemorySourceBoth {}
    })
}

fn memory_line(cx: &Cx<'_>, memory: &Memory) -> String {
    cx.t(&Msg::MemoryLine {
        id: memory.id.label(),
        text: memory.display(),
        source: source(cx, memory),
        count: u64::from(memory.episodes()),
        date: date(cx, memory.last),
    })
}

/// The current memories about these accounts, by id: the order /who lists them.
async fn remembered(cx: &Cx<'_>, accounts: &[AccountId]) -> Result<Vec<Memory>, Fail> {
    let subjects: Vec<Option<AccountId>> = accounts.iter().copied().map(Some).collect();
    cx.deps
        .memories
        .current(cx.group(), &subjects)
        .await
        .map_err(storage)
}

pub(crate) async fn who(cx: &Cx<'_>) -> Reply {
    let tokens = cx.tokens();
    let (all, rest) = args::scope(&tokens)?;
    if !rest.is_empty() {
        return Err(Msg::UsageWho {}.into());
    }
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
    let linked = cx.linked_accounts(account).await?;
    let accounts = if all { linked.clone() } else { vec![account] };

    let mut lines = vec![
        cx.t(&Msg::WhoTitle {
            display: cx.name_of(account).await,
            scope: cx.scope_label(all),
        }),
        cx.t(&Msg::WhoMessages {
            count: cx
                .deps
                .admin
                .message_count(cx.group(), &accounts)
                .await?
                .to_string(),
        }),
    ];
    if linked.len() > 1 {
        lines.push(cx.t(&Msg::WhoLinked {
            count: linked.len().to_string(),
        }));
    }
    let names = name_lines(cx, &accounts).await?;
    if !names.is_empty() {
        lines.push(cx.t(&Msg::WhoNames {
            count: names.len() as u64,
        }));
        lines.extend(names);
    }
    let notes = cx
        .deps
        .notes
        .notes(cx.group(), &accounts)
        .await
        .map_err(storage)?;
    if notes.is_empty() {
        lines.push(cx.t(&Msg::WhoNoNotes {}));
    } else {
        lines.push(cx.t(&Msg::WhoNotes {
            count: notes.len() as u64,
        }));
        for (index, note) in (1u64..).zip(&notes) {
            lines.push(cx.t(&Msg::WhoNoteLine {
                index,
                text: args::preview(&note.text, 80),
            }));
        }
    }
    let memories = remembered(cx, &accounts).await?;
    if memories.is_empty() {
        lines.push(cx.t(&Msg::WhoNoMemories {}));
    } else {
        lines.push(cx.t(&Msg::WhoMemories {
            count: memories.len() as u64,
        }));
        lines.extend(memories.iter().map(|m| memory_line(cx, m)));
    }
    if !notes.is_empty() || !memories.is_empty() {
        lines.push(cx.t(&Msg::WhoHint {}));
    }
    Ok(cx.fit(&lines.join("\n")))
}

pub(crate) async fn group(cx: &Cx<'_>) -> Reply {
    if !cx.tokens().is_empty() || !cx.req.mentions.is_empty() {
        return Err(Msg::UsageGroup {}.into());
    }
    let mut memories = cx
        .deps
        .memories
        .current(cx.group(), &[None])
        .await
        .map_err(storage)?;
    if memories.is_empty() {
        return Ok(cx.t(&Msg::GroupEmpty {}));
    }
    memories.sort_by(|a, b| (&a.term, &a.text, a.id).cmp(&(&b.term, &b.text, b.id)));
    let mut lines = vec![cx.t(&Msg::GroupHeader {})];
    lines.extend(memories.iter().map(|m| memory_line(cx, m)));
    lines.push(cx.t(&Msg::GroupHint {}));
    Ok(cx.fit(&lines.join("\n")))
}

/// `/forget group ID`: remove a group memory. Owners only.
async fn forget_group(cx: &Cx<'_>, rest: &[&str]) -> Reply {
    let [raw] = rest else {
        return Err(Msg::UsageForget {}.into());
    };
    if !cx.owner {
        return Err(Msg::OwnerOnly {}.into());
    }
    if !cx.req.mentions.is_empty() {
        return Err(Msg::UsageForget {}.into());
    }
    let no_such = || Msg::ForgetGroupNoSuch {
        id: (*raw).to_owned(),
    };
    let id = MemoryId::parse(raw).ok_or_else(no_such)?;
    let memory = cx
        .deps
        .memories
        .get(cx.group(), id)
        .await
        .map_err(storage)?
        .filter(|m| m.subject.is_none())
        .ok_or_else(no_such)?;
    if !cx
        .deps
        .memories
        .delete(cx.group(), id)
        .await
        .map_err(storage)?
    {
        return Err(no_such().into());
    }
    Ok(cx.t(&Msg::ForgetGroupDone {
        id: id.label(),
        text: memory.display(),
    }))
}

pub(crate) async fn forget(cx: &Cx<'_>) -> Reply {
    let tokens = cx.tokens();
    if let Some((&"group", rest)) = tokens.split_first() {
        return forget_group(cx, rest).await;
    }
    let (all, rest) = args::scope(&tokens)?;
    let account = cx.target().await?;
    let accounts = scope_accounts(cx, account, all).await?;
    match rest.as_slice() {
        ["all"] => {
            let mut gone = 0;
            for one in &accounts {
                gone += cx
                    .deps
                    .memories
                    .delete_about(cx.group(), *one)
                    .await
                    .map_err(storage)?;
            }
            Ok(cx.t(&Msg::ForgetAllDone {
                display: cx.name_of(account).await,
                count: gone,
            }))
        }
        [raw] => {
            let no_such = || Msg::ForgetNoSuch {
                id: (*raw).to_owned(),
            };
            let id = MemoryId::parse(raw).ok_or_else(no_such)?;
            // Only a memory about the accounts in scope, in any state.
            let memory = cx
                .deps
                .memories
                .get(cx.group(), id)
                .await
                .map_err(storage)?
                .filter(|m| m.subject.is_some_and(|s| accounts.contains(&s)))
                .ok_or_else(no_such)?;
            if !cx
                .deps
                .memories
                .delete(cx.group(), id)
                .await
                .map_err(storage)?
            {
                return Err(no_such().into());
            }
            Ok(cx.t(&Msg::ForgetDone {
                id: id.label(),
                text: memory.display(),
            }))
        }
        _ => Err(Msg::UsageForget {}.into()),
    }
}

pub(crate) async fn name(cx: &Cx<'_>) -> Reply {
    let tokens = cx.tokens();
    let (action, rest) = match tokens.split_first() {
        Some((first, rest)) if matches!(*first, "add" | "remove") => (Some(*first), rest.to_vec()),
        _ => (None, tokens.clone()),
    };
    let (all, rest) = args::scope(&rest)?;
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
    let display = cx.name_of(account).await;
    let accounts = scope_accounts(cx, account, all).await?;

    let Some(action) = action else {
        if !rest.is_empty() {
            return Err(Msg::UsageNameList {}.into());
        }
        let names = name_lines(cx, &accounts).await?;
        if names.is_empty() {
            return Ok(cx.t(&Msg::NameNone { display }));
        }
        let mut lines = vec![cx.t(&Msg::NameHeader { display })];
        lines.extend(names);
        return Ok(cx.fit(&lines.join("\n")));
    };

    let name = rest.join(" ");
    if name.is_empty() {
        return Err(Msg::UsageNameEdit {
            action: action.to_owned(),
        }
        .into());
    }
    let now = cx.deps.clock.now();
    let result = if action == "remove" {
        cx.deps
            .names
            .remove(cx.group(), &name, &accounts, now)
            .await
            .map(|removed| {
                cx.t(&if removed {
                    Msg::NameRemoved {
                        display,
                        name: name.clone(),
                    }
                } else {
                    Msg::NameRemoveNone {
                        display,
                        name: name.clone(),
                    }
                })
            })
    } else {
        cx.deps
            .names
            .set(cx.group(), &name, &accounts, now)
            .await
            .map(|()| {
                cx.t(&Msg::NameAdded {
                    display,
                    name: name.clone(),
                })
            })
    };
    match result {
        Ok(reply) => Ok(reply),
        Err(NameError::Taken) => Err(Msg::NameTaken { name }.into()),
        Err(NameError::Text(NameTextError::Empty)) => Err(Msg::NameEmptyName {}.into()),
        Err(NameError::Text(NameTextError::TooLong)) => Err(Msg::NameTooLong {
            max: MAX_NAME_CHARS as u32,
        }
        .into()),
        Err(NameError::Backend(error)) => Err(storage(error)),
    }
}
