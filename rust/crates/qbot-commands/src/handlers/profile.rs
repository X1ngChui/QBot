use qbot_core::AccountId;
use qbot_i18n::Msg;
use qbot_memory::facts::{Fact, GROUP_TERM, GROUP_TOPIC};
use qbot_memory::{Alias, AliasStatus, AliasTarget, IdentityError};

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
                    line.push_str(&cx.t(&Msg::HelpOwnerTag {}));
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
            let access = cx.t(&if command.owner_only() {
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

/// The alias target for an account: the account itself, or the person it belongs to.
async fn alias_target(cx: &Cx<'_>, account: AccountId, all: bool) -> Result<AliasTarget, Fail> {
    if !all {
        return Ok(AliasTarget::Account(account));
    }
    match cx.deps.identity.holder_of(account).await? {
        Some(holder) => Ok(AliasTarget::Holder(holder.id)),
        None => Err(IdentityError::UnknownAccount.into()),
    }
}

fn entry(cx: &Cx<'_>, alias: &Alias) -> String {
    cx.t(&Msg::NameEntry {
        text: alias.text.clone(),
        confidence: format!("{:.2}", alias.confidence),
    })
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
    let holder = cx.deps.identity.holder_of(account).await?;
    let accounts = match (&holder, all) {
        (Some(h), true) => h.accounts.clone(),
        _ => vec![account],
    };
    let target = alias_target(cx, account, all).await?;
    let names = cx.deps.identity.names_of(cx.group(), target).await?;
    let confirmed: Vec<String> = names
        .iter()
        .filter(|a| a.status == AliasStatus::Confirmed)
        .map(|a| entry(cx, a))
        .collect();
    let leads: Vec<String> = names
        .iter()
        .filter(|a| a.status == AliasStatus::Candidate)
        .map(|a| entry(cx, a))
        .collect();

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
    if let Some(holder) = holder.filter(|h| h.accounts.len() > 1) {
        lines.push(cx.t(&Msg::WhoLinked {
            count: holder.accounts.len().to_string(),
        }));
    }
    if !confirmed.is_empty() {
        lines.push(cx.t(&Msg::WhoNames {
            names: cx.join_names(&confirmed),
        }));
    }
    if !leads.is_empty() {
        lines.push(cx.t(&Msg::WhoLeads {
            names: cx.join_names(&leads),
        }));
    }
    let notes = cx
        .deps
        .notes
        .notes(cx.group(), &accounts)
        .await
        .map_err(|e| Fail::Storage(e.to_string()))?;
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
    let facts = learned(cx, &accounts).await?;
    if facts.is_empty() {
        lines.push(cx.t(&Msg::WhoNoFacts {}));
    } else {
        let now = cx.deps.clock.now();
        lines.push(cx.t(&Msg::WhoFacts {
            count: facts.len() as u64,
        }));
        for (index, fact) in (1u64..).zip(&facts) {
            lines.push(cx.t(&Msg::WhoFactLine {
                index,
                predicate: fact.predicate.clone(),
                object: fact.object.clone(),
                confidence: format!("{:.2}", cx.deps.settings.fact_decay.confidence(fact, now)),
            }));
        }
    }
    if !notes.is_empty() || !facts.is_empty() {
        lines.push(cx.t(&Msg::WhoHint {}));
    }
    Ok(cx.fit(&lines.join("\n")))
}

/// The learned facts about these accounts, in the order /who numbers them.
async fn learned(cx: &Cx<'_>, accounts: &[qbot_core::AccountId]) -> Result<Vec<Fact>, Fail> {
    let mut facts = Vec::new();
    for account in accounts {
        facts.extend(
            cx.deps
                .facts
                .current(cx.group(), Some(*account))
                .await
                .map_err(|e| Fail::Storage(e.to_string()))?,
        );
    }
    facts.sort_by(|a, b| (&a.predicate, &a.key, a.subject).cmp(&(&b.predicate, &b.key, b.subject)));
    Ok(facts)
}

/// What the bot learned about the group itself, in the order /group numbers it: the topic,
/// then terms by key.
async fn group_knowledge(cx: &Cx<'_>) -> Result<Vec<Fact>, Fail> {
    let facts = cx
        .deps
        .facts
        .current(cx.group(), None)
        .await
        .map_err(|e| Fail::Storage(e.to_string()))?;
    let mut ordered: Vec<Fact> = facts
        .iter()
        .filter(|f| f.predicate == GROUP_TOPIC)
        .cloned()
        .collect();
    let mut terms: Vec<Fact> = facts
        .into_iter()
        .filter(|f| f.predicate == GROUP_TERM)
        .collect();
    terms.sort_by(|a, b| a.key.cmp(&b.key));
    ordered.extend(terms);
    Ok(ordered)
}

fn knowledge_text(fact: &Fact) -> String {
    match &fact.label {
        Some(term) if fact.predicate == GROUP_TERM => format!("{term}: {}", fact.object),
        _ => fact.object.clone(),
    }
}

pub(crate) async fn group(cx: &Cx<'_>) -> Reply {
    if !cx.tokens().is_empty() || !cx.req.mentions.is_empty() {
        return Err(Msg::UsageGroup {}.into());
    }
    let knowledge = group_knowledge(cx).await?;
    if knowledge.is_empty() {
        return Ok(cx.t(&Msg::GroupEmpty {}));
    }
    let mut lines = vec![cx.t(&Msg::GroupHeader {})];
    for (index, fact) in (1u64..).zip(&knowledge) {
        lines.push(cx.t(&if fact.predicate == GROUP_TERM {
            Msg::GroupTermLine {
                index,
                term: fact.label.clone().unwrap_or_else(|| fact.key.clone()),
                text: fact.object.clone(),
            }
        } else {
            Msg::GroupTopicLine {
                index,
                text: fact.object.clone(),
            }
        }));
    }
    lines.push(cx.t(&Msg::GroupHint {}));
    Ok(cx.fit(&lines.join("\n")))
}

/// `/forget group N`: remove learned group knowledge. Owners only.
async fn forget_group(cx: &Cx<'_>, rest: &[&str]) -> Reply {
    let [number] = rest else {
        return Err(Msg::UsageForget {}.into());
    };
    if !cx.owner {
        return Err(Msg::OwnerOnly {}.into());
    }
    if !cx.req.mentions.is_empty() {
        return Err(Msg::UsageForget {}.into());
    }
    let index = args::positive(number)?;
    let knowledge = group_knowledge(cx).await?;
    let no_such = || Msg::ForgetGroupNoSuch {
        index: u64::from(index),
    };
    let fact = knowledge.get(index as usize - 1).ok_or_else(no_such)?;
    let gone = cx
        .deps
        .facts
        .forget(cx.group(), fact.id)
        .await
        .map_err(|e| Fail::Storage(e.to_string()))?;
    if !gone {
        return Err(no_such().into());
    }
    Ok(cx.t(&Msg::ForgetGroupDone {
        index: u64::from(index),
        text: knowledge_text(fact),
    }))
}

pub(crate) async fn forget(cx: &Cx<'_>) -> Reply {
    let tokens = cx.tokens();
    if let Some((&"group", rest)) = tokens.split_first() {
        return forget_group(cx, rest).await;
    }
    let (all, rest) = args::scope(&tokens)?;
    let [number] = rest.as_slice() else {
        return Err(Msg::UsageForget {}.into());
    };
    let index = args::positive(number)?;
    let account = cx.target().await?;
    let accounts = if all {
        cx.linked_accounts(account).await?
    } else {
        vec![account]
    };
    let facts = learned(cx, &accounts).await?;
    let Some(fact) = facts.get(index as usize - 1) else {
        return Err(Msg::ForgetNoSuch {
            index: u64::from(index),
        }
        .into());
    };
    let gone = cx
        .deps
        .facts
        .forget(cx.group(), fact.id)
        .await
        .map_err(|e| Fail::Storage(e.to_string()))?;
    if !gone {
        return Err(Msg::ForgetNoSuch {
            index: u64::from(index),
        }
        .into());
    }
    Ok(cx.t(&Msg::ForgetDone {
        index: u64::from(index),
        predicate: fact.predicate.clone(),
        object: fact.object.clone(),
    }))
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
    let target = alias_target(cx, account, all).await?;

    let Some(action) = action else {
        if !rest.is_empty() {
            return Err(Msg::UsageNameList {}.into());
        }
        let names = cx.deps.identity.names_of(cx.group(), target).await?;
        if names.is_empty() {
            return Ok(cx.t(&Msg::NameNone { display }));
        }
        let mut lines = vec![cx.t(&Msg::NameHeader { display })];
        for status in [AliasStatus::Confirmed, AliasStatus::Candidate] {
            for alias in names.iter().filter(|a| a.status == status) {
                let (text, confidence) = (alias.text.clone(), format!("{:.2}", alias.confidence));
                lines.push(cx.t(&if status == AliasStatus::Confirmed {
                    Msg::NameConfirmedLine { text, confidence }
                } else {
                    Msg::NameCandidateLine { text, confidence }
                }));
            }
        }
        return Ok(cx.fit(&lines.join("\n")));
    };

    let name = rest.join(" ");
    if name.is_empty() {
        return Err(Msg::UsageNameEdit {
            action: action.to_owned(),
        }
        .into());
    }
    if action == "remove" {
        let removed = cx
            .deps
            .identity
            .remove_name(cx.group(), &name, target)
            .await?;
        return Ok(cx.t(&if removed {
            Msg::NameRemoved { display, name }
        } else {
            Msg::NameRemoveNone { display, name }
        }));
    }
    match cx
        .deps
        .identity
        .set_name(cx.group(), &name, target, cx.deps.clock.now())
        .await
    {
        Ok(_) => Ok(cx.t(&Msg::NameAdded { display, name })),
        Err(IdentityError::NameTaken) => Err(Msg::NameTaken { name }.into()),
        Err(other) => Err(other.into()),
    }
}
