//! `/note`: notes people write by hand. Only the note store is touched here; learned memory is
//! `/forget`'s.

use qbot_core::AccountId;
use qbot_i18n::Msg;
use qbot_memory::Note;

use super::Reply;
use crate::args;
use crate::fail::Fail;
use crate::router::Cx;

fn storage(error: impl std::fmt::Display) -> Fail {
    Fail::Storage(error.to_string())
}

/// The account a note command is about, after checking it has a record in the group.
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

/// The notes of the scope, in the order they are numbered.
async fn scoped(cx: &Cx<'_>, account: AccountId, all: bool) -> Result<Vec<Note>, Fail> {
    let accounts = if all {
        cx.linked_accounts(account).await?
    } else {
        vec![account]
    };
    cx.deps
        .notes
        .notes(cx.group(), &accounts)
        .await
        .map_err(storage)
}

/// An optional `--linked`, then the words that follow it.
fn all_flag<'a>(words: &'a [&'a str]) -> (bool, &'a [&'a str]) {
    match words.split_first() {
        Some((&"--linked", rest)) => (true, rest),
        _ => (false, words),
    }
}

fn pick<'n>(notes: &'n [Note], number: &str) -> Result<(u64, &'n Note), Fail> {
    let index = args::positive(number)?;
    notes
        .get(index as usize - 1)
        .map(|note| (u64::from(index), note))
        .ok_or_else(|| {
            Msg::NoteNoSuch {
                index: u64::from(index),
            }
            .into()
        })
}

pub(crate) async fn note(cx: &Cx<'_>) -> Reply {
    let tokens = cx.tokens();
    let usage = || Fail::Say(Msg::UsageNote {});
    let Some((&action, words)) = tokens.split_first() else {
        return list(cx, false).await;
    };
    match action {
        "add" => {
            let text = args::after_words(&cx.req.args, 1);
            if text.starts_with("--") {
                return Err(usage());
            }
            if text.is_empty() {
                return Err(Msg::NoteEmpty {}.into());
            }
            let account = subject(cx).await?;
            let existing = scoped(cx, account, false).await?;
            let max = cx.deps.settings.notes_per_account;
            if existing.len() >= max {
                return Err(Msg::NoteTooMany { max: max as u64 }.into());
            }
            cx.deps
                .notes
                .add(
                    cx.group(),
                    account,
                    text,
                    cx.req.sender,
                    cx.deps.clock.now(),
                )
                .await
                .map_err(storage)?;
            Ok(cx.t(&Msg::NoteAdded {
                display: cx.name_of(account).await,
                index: existing.len() as u64 + 1,
            }))
        }
        "edit" => {
            let (all, rest) = all_flag(words);
            let Some(number) = rest.first() else {
                return Err(usage());
            };
            let text = args::after_words(&cx.req.args, if all { 3 } else { 2 });
            if text.is_empty() {
                return Err(Msg::NoteEmpty {}.into());
            }
            let account = subject(cx).await?;
            let notes = scoped(cx, account, all).await?;
            let (index, note) = pick(&notes, number)?;
            let changed = cx
                .deps
                .notes
                .edit(
                    cx.group(),
                    note.id,
                    text,
                    cx.req.sender,
                    cx.deps.clock.now(),
                )
                .await
                .map_err(storage)?;
            if !changed {
                return Err(Msg::NoteNoSuch { index }.into());
            }
            Ok(cx.t(&Msg::NoteEdited { index }))
        }
        "remove" => {
            let (all, rest) = all_flag(words);
            let [number] = rest else {
                return Err(usage());
            };
            let account = subject(cx).await?;
            let notes = scoped(cx, account, all).await?;
            let (index, note) = pick(&notes, number)?;
            let removed = cx
                .deps
                .notes
                .remove(cx.group(), note.id)
                .await
                .map_err(storage)?;
            if !removed {
                return Err(Msg::NoteNoSuch { index }.into());
            }
            Ok(cx.t(&Msg::NoteRemoved {
                index,
                text: args::preview(&note.text, 80),
            }))
        }
        "clear" => {
            let (all, rest) = all_flag(words);
            if !rest.is_empty() {
                return Err(usage());
            }
            let account = subject(cx).await?;
            let accounts = if all {
                cx.linked_accounts(account).await?
            } else {
                vec![account]
            };
            let count = cx
                .deps
                .notes
                .clear(cx.group(), &accounts)
                .await
                .map_err(storage)?;
            Ok(cx.t(&Msg::NoteCleared {
                display: cx.name_of(account).await,
                count,
            }))
        }
        "--linked" if words.is_empty() => list(cx, true).await,
        _ => Err(usage()),
    }
}

async fn list(cx: &Cx<'_>, all: bool) -> Reply {
    let account = subject(cx).await?;
    let display = cx.name_of(account).await;
    let notes = scoped(cx, account, all).await?;
    if notes.is_empty() {
        return Ok(cx.t(&Msg::NoteNone { display }));
    }
    let mut lines = vec![cx.t(&Msg::NoteHeader {
        display,
        scope: cx.scope_label(all),
    })];
    for (index, note) in (1u64..).zip(&notes) {
        lines.push(cx.t(&Msg::NoteLine {
            index,
            text: note.text.clone(),
            author: cx.name_of(note.author).await,
            time: cx.short_time(note.updated),
        }));
    }
    Ok(cx.fit(&lines.join("\n")))
}
