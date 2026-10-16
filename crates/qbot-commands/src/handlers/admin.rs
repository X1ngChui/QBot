use std::collections::BTreeMap;
use std::time::Duration;

use qbot_core::AccountId;
use qbot_i18n::Msg;
use qbot_memory::AliasStatus;
use qbot_store::BlockedBy;

use super::{Rank, Reply, period_start};
use crate::args;
use crate::fail::Fail;
use crate::router::Cx;

pub(crate) async fn mute(cx: &Cx<'_>) -> Reply {
    let tokens = cx.tokens();
    if !cx.req.mentions.is_empty() || tokens.len() > 1 {
        return Err(Msg::UsageMute {}.into());
    }
    let wanted = match tokens.first().copied() {
        None | Some("status") => None,
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some(_) => return Err(Msg::UsageMute {}.into()),
    };
    let muted = cx.deps.admin.is_muted(cx.group()).await?;
    let Some(wanted) = wanted else {
        return Ok(cx.t(&if muted {
            Msg::MuteStatusOn {}
        } else {
            Msg::MuteStatusOff {}
        }));
    };
    if wanted == muted {
        return Ok(cx.t(&if muted {
            Msg::MuteAlreadyOn {}
        } else {
            Msg::MuteAlreadyOff {}
        }));
    }
    cx.deps.admin.set_muted(cx.group(), wanted).await?;
    Ok(cx.t(&if wanted {
        Msg::MuteSetOn {}
    } else {
        Msg::MuteSetOff {}
    }))
}

/// `/admin`, `/admin add @m`, `/admin remove @m`: the group's delegated admins, who may use
/// `/mute` and `/block` here. Only bot owners reach this (a delegated admin is refused like any
/// member), so delegated authority never passes itself on.
pub(crate) async fn admin(cx: &Cx<'_>) -> Reply {
    if !cx.owner {
        return Err(Msg::OwnerOnly {}.into());
    }
    let tokens = cx.tokens();
    match (tokens.as_slice(), cx.req.mentions.as_slice()) {
        ([], []) => {
            let admins = cx.deps.admin.admins(cx.group()).await?;
            if admins.is_empty() {
                return Ok(cx.t(&Msg::AdminListEmpty {}));
            }
            let mut lines = vec![cx.t(&Msg::AdminListHeader {})];
            for account in admins {
                lines.push(cx.t(&Msg::AdminListRow {
                    label: cx.name_of(account).await,
                }));
            }
            Ok(cx.fit(&lines.join("\n")))
        }
        (["add"], [target]) => {
            if *target == cx.deps.settings.bot {
                return Err(Msg::AdminBot {}.into());
            }
            if cx.is_owner(*target) {
                return Err(Msg::AdminBotOwner {}.into());
            }
            if cx
                .deps
                .admin
                .member_number(cx.group(), *target)
                .await?
                .is_none()
            {
                return Err(Msg::WhoNoRecord {}.into());
            }
            let label = cx.name_of(*target).await;
            let added = cx
                .deps
                .admin
                .grant_admin(cx.group(), *target, cx.req.sender)
                .await?;
            Ok(cx.t(&if added {
                Msg::AdminAdded { label }
            } else {
                Msg::AdminAlready { label }
            }))
        }
        (["remove"], [target]) => {
            let label = cx.name_of(*target).await;
            let removed = cx.deps.admin.revoke_admin(cx.group(), *target).await?;
            Ok(cx.t(&if removed {
                Msg::AdminRemoved { label }
            } else {
                Msg::AdminRemoveNone { label }
            }))
        }
        _ => Err(Msg::UsageAdmin {}.into()),
    }
}

/// The accounts a block applies to: the one account, or everyone linked to it in this group.
async fn block_accounts(cx: &Cx<'_>, target: AccountId, all: bool) -> Result<Vec<AccountId>, Fail> {
    if !cx.deps.identity.is_known(target).await? {
        return Err(Fail::Say(Msg::UnknownAccount {}));
    }
    if all {
        cx.linked_accounts(target).await
    } else {
        Ok(vec![target])
    }
}

/// What a delegated admin may not do: block or unblock anyone but an ordinary member (the
/// person `target` belongs to here holds no bot owner's or delegated admin's account), or change
/// or lift a bot owner's block (lower authority never overrides higher). Bot owners may do
/// anything. Checked for every account before any is touched.
async fn delegated_may_act_on(
    cx: &Cx<'_>,
    target: AccountId,
    accounts: &[AccountId],
) -> Result<(), Fail> {
    if cx.owner {
        return Ok(());
    }
    match cx.rank_of(target).await? {
        Rank::Member => {}
        Rank::BotOwner => return Err(Msg::BlockCannot {}.into()),
        Rank::DelegatedAdmin => return Err(Msg::BlockDelegatedTarget {}.into()),
    }
    let rules = cx.deps.admin.blocks(cx.group()).await?;
    for account in accounts {
        if rules
            .iter()
            .any(|r| r.account == *account && r.by == BlockedBy::Owner)
        {
            return Err(Msg::BlockOwnerImposed {
                label: cx.name_of(*account).await,
            }
            .into());
        }
    }
    Ok(())
}

fn acting(cx: &Cx<'_>) -> BlockedBy {
    if cx.owner {
        BlockedBy::Owner
    } else {
        BlockedBy::DelegatedAdmin
    }
}

pub(crate) async fn block(cx: &Cx<'_>) -> Reply {
    let mentions = &cx.req.mentions;
    if mentions.len() > 1 {
        return Err(Msg::MentionsMax { max: 1 }.into());
    }
    let tokens = cx.tokens();
    if tokens.is_empty() && mentions.is_empty() {
        return block_list(cx).await;
    }
    let Some((action, rest)) = tokens
        .split_first()
        .filter(|(a, _)| matches!(**a, "add" | "remove"))
    else {
        return Err(Msg::UsageBlock {}.into());
    };
    let (all, rest) = args::scope(rest)?;
    let [target] = mentions.as_slice() else {
        return Err(Msg::MentionsExact { count: 1 }.into());
    };
    let target = *target;
    let label = cx.name_of(target).await;
    let scope = cx.t(&if all {
        Msg::BlockScopeLinked {}
    } else {
        Msg::BlockScopeExact {}
    });

    if *action == "remove" {
        if !rest.is_empty() {
            return Err(Msg::UsageBlockRemove {}.into());
        }
        let accounts = block_accounts(cx, target, all).await?;
        delegated_may_act_on(cx, target, &accounts).await?;
        let mut removed = false;
        for account in accounts {
            removed |= cx
                .deps
                .admin
                .unblock(cx.group(), account, acting(cx))
                .await?;
        }
        return Ok(cx.t(&if removed {
            Msg::BlockRemoved { label, scope }
        } else {
            Msg::BlockRemoveNone { label, scope }
        }));
    }

    if rest.len() > 1 {
        return Err(Msg::UsageBlockAdd {}.into());
    }
    let accounts = block_accounts(cx, target, all).await?;
    if target == cx.deps.settings.bot || cx.rank_of(target).await? == Rank::BotOwner {
        return Err(Msg::BlockCannot {}.into());
    }
    delegated_may_act_on(cx, target, &accounts).await?;
    let until = match rest.first() {
        None => None,
        Some(text) => {
            let span = args::duration(text).ok_or(Msg::DurationInvalid {})?;
            Some(cx.deps.clock.now().plus(span))
        }
    };
    for account in accounts {
        // Refused only if a bot owner blocked the account since the check above.
        if !cx
            .deps
            .admin
            .block(cx.group(), account, until, acting(cx))
            .await?
        {
            return Err(Msg::BlockOwnerImposed {
                label: cx.name_of(account).await,
            }
            .into());
        }
    }
    let lapse = cx.t(&match until {
        Some(at) => Msg::BlockLapseUntil {
            time: cx.short_time(at),
        },
        None => Msg::BlockLapseForever {},
    });
    Ok(cx.t(&Msg::BlockAdded {
        label,
        scope,
        lapse,
    }))
}

async fn block_list(cx: &Cx<'_>) -> Reply {
    let rules = cx.deps.admin.blocks(cx.group()).await?;
    if rules.is_empty() {
        return Ok(cx.t(&Msg::BlockListEmpty {}));
    }
    let mut lines = vec![cx.t(&Msg::BlockListHeader {})];
    for rule in rules {
        let until = rule
            .until
            .map(|at| {
                cx.t(&Msg::BlockUntilSuffix {
                    time: cx.short_time(at),
                })
            })
            .unwrap_or_default();
        lines.push(cx.t(&Msg::BlockListRow {
            label: cx.name_of(rule.account).await,
            until,
        }));
    }
    Ok(cx.fit(&lines.join("\n")))
}

pub(crate) async fn members(cx: &Cx<'_>) -> Reply {
    if !cx.tokens().is_empty() || !cx.req.mentions.is_empty() {
        return Err(Msg::UsageMembers {}.into());
    }
    let limit = i64::try_from(cx.deps.settings.members_max_rows).unwrap_or(i64::MAX);
    let (total, rows) = cx.deps.admin.roster(cx.group(), limit).await?;
    if rows.is_empty() {
        return Ok(cx.t(&Msg::MembersEmpty {}));
    }
    let mut lines = vec![cx.t(&Msg::MembersHeader {
        total: total.to_string(),
    })];
    for row in rows {
        let names = cx
            .deps
            .identity
            .names_of(cx.group(), &[row.account])
            .await?;
        let names: Vec<String> = names
            .into_iter()
            .filter(|a| a.status == AliasStatus::Confirmed)
            .take(3)
            .map(|a| a.text)
            .collect();
        let (display, messages) = (cx.name_of(row.account).await, row.messages);
        lines.push(cx.t(&if names.is_empty() {
            Msg::MembersRow { display, messages }
        } else {
            Msg::MembersRowNames {
                display,
                messages,
                names: cx.join_names(&names),
            }
        }));
    }
    Ok(cx.fit(&lines.join("\n")))
}

pub(crate) async fn stats(cx: &Cx<'_>) -> Reply {
    let tokens = cx.tokens();
    if !cx.req.mentions.is_empty() {
        return Err(Msg::UsageStats {}.into());
    }
    let global = match tokens.as_slice() {
        [] => false,
        ["global"] => true,
        _ => return Err(Msg::UsageStats {}.into()),
    };
    if global && !cx.owner {
        return Err(Msg::OwnerOnly {}.into());
    }
    let now = cx.deps.clock.now();
    let since = period_start(&cx.deps.settings.zone, now, false)?;
    let group = (!global).then(|| cx.group());
    let usage = cx
        .deps
        .admin
        .usage(group, since, now.plus(Duration::from_millis(1)))
        .await?;
    let mut lines = vec![cx.t(&if global {
        Msg::StatsGlobalTitle {}
    } else {
        Msg::StatsGroupTitle {
            group: cx.group().get().to_string(),
        }
    })];
    lines.push(cx.t(&Msg::StatsRuns {
        runs: usage.runs,
        tool_calls: usage.tool_calls,
    }));
    lines.extend(qbot_ops::usage_lines(&cx.deps.locales, &usage));
    if !global {
        let muted = cx.deps.admin.is_muted(cx.group()).await?;
        lines.push(cx.t(&if muted {
            Msg::StatsMuted {}
        } else {
            Msg::StatsEnabled {}
        }));
        let blocks = cx.deps.admin.blocks(cx.group()).await?.len();
        if blocks > 0 {
            lines.push(cx.t(&Msg::StatsBlocks {
                count: u64::try_from(blocks).unwrap_or(u64::MAX),
            }));
        }
    }
    Ok(cx.fit(&lines.join("\n")))
}

pub(crate) async fn top(cx: &Cx<'_>) -> Reply {
    if !cx.req.mentions.is_empty() {
        return Err(Msg::UsageTop {}.into());
    }
    let tokens = cx.tokens();
    let (all, rest) = args::scope(&tokens)?;
    let max = cx.deps.settings.top_max_rows;
    let show = match rest.as_slice() {
        [] => 5.min(max),
        [n] => args::positive(n)
            .map_err(|_| Msg::UsageTop {})?
            .try_into()
            .map_or(max, |n: usize| n.min(max)),
        _ => return Err(Msg::UsageTop {}.into()),
    };
    let since = period_start(&cx.deps.settings.zone, cx.deps.clock.now(), true)?;

    // Linked accounts are combined by person, which needs every requester, not just the top few.
    let fetch = if all {
        i64::MAX
    } else {
        i64::try_from(show).unwrap_or(i64::MAX)
    };
    let rows = cx
        .deps
        .admin
        .top_requesters(cx.group(), since, fetch)
        .await?;
    if rows.is_empty() {
        return Ok(cx.t(&Msg::TopEmpty {}));
    }

    struct Entry {
        first: AccountId,
        accounts: u64,
        runs: u64,
        tokens: u64,
    }
    let mut entries: Vec<Entry> = Vec::new();
    if all {
        let mut by_person: BTreeMap<i64, usize> = BTreeMap::new();
        for row in rows {
            // A person is named by their lowest account linked in this group.
            let person = cx.linked_accounts(row.account).await?[0].get();
            let tokens = row.input_tokens + row.output_tokens;
            match by_person.get(&person) {
                Some(&at) => {
                    entries[at].accounts += 1;
                    entries[at].runs += row.runs;
                    entries[at].tokens += tokens;
                }
                None => {
                    by_person.insert(person, entries.len());
                    entries.push(Entry {
                        first: row.account,
                        accounts: 1,
                        runs: row.runs,
                        tokens,
                    });
                }
            }
        }
        entries.sort_by(|a, b| {
            b.runs
                .cmp(&a.runs)
                .then(b.tokens.cmp(&a.tokens))
                .then(a.first.cmp(&b.first))
        });
        entries.truncate(show);
    } else {
        entries.extend(rows.into_iter().map(|r| Entry {
            first: r.account,
            accounts: 1,
            runs: r.runs,
            tokens: r.input_tokens + r.output_tokens,
        }));
    }

    let mut lines = vec![cx.t(&Msg::TopHeader {
        scope: cx.scope_label(all),
    })];
    for (rank, entry) in (1u64..).zip(&entries) {
        let tag = if all && entry.accounts > 1 {
            cx.t(&Msg::TopLinkedTag {
                count: entry.accounts,
            })
        } else {
            String::new()
        };
        lines.push(cx.t(&Msg::TopRow {
            rank,
            name: cx.name_of(entry.first).await,
            tag,
            runs: entry.runs,
            tokens: entry.tokens,
        }));
    }
    Ok(cx.fit(&lines.join("\n")))
}
