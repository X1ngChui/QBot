use qbot_i18n::Msg;
use qbot_memory::{IdentityError, MergeOutcome};

use super::Reply;
use crate::fail::Fail;
use crate::router::Cx;

pub(crate) async fn link(cx: &Cx<'_>) -> Reply {
    let tokens = cx.tokens();
    let (group, sender, now) = (cx.group(), cx.req.sender, cx.deps.clock.now());
    match tokens.as_slice() {
        ["confirm"] => {
            if !cx.req.mentions.is_empty() {
                return Err(Msg::UsageLinkConfirm {}.into());
            }
            cx.deps
                .identity
                .confirm(group, sender, cx.req.message, now)
                .await?;
            Ok(cx.t(&Msg::LinkConfirmed {}))
        }
        ["cancel"] => {
            if !cx.req.mentions.is_empty() {
                return Err(Msg::UsageLinkCancel {}.into());
            }
            let cancelled = cx.deps.identity.cancel_invitation(group, sender).await?;
            Ok(cx.t(&if cancelled {
                Msg::LinkCancelled {}
            } else {
                Msg::LinkNothingPending {}
            }))
        }
        [] if cx.req.mentions.len() == 2 => link_directly(cx).await,
        _ => {
            let [target] = cx.req.mentions.as_slice() else {
                return Err(Msg::UsageLinkIssue {}.into());
            };
            if !tokens.is_empty() {
                return Err(Msg::UsageLinkIssue {}.into());
            }
            cx.deps
                .identity
                .invite(group, sender, *target, cx.req.message, now)
                .await?;
            Ok(cx.t(&Msg::LinkInvited {
                target: cx.name_of(*target).await,
                seconds: cx.deps.settings.link_ttl.as_secs(),
            }))
        }
    }
}

pub(crate) async fn unlink(cx: &Cx<'_>) -> Reply {
    if !cx.tokens().is_empty() {
        return Err(Msg::UsageUnlink {}.into());
    }
    match cx.req.mentions.as_slice() {
        [] => {
            cx.deps
                .identity
                .split(cx.req.sender, cx.deps.clock.now())
                .await?;
            Ok(cx.t(&Msg::UnlinkDone {}))
        }
        // An owner detaching someone else's account.
        [account] if *account != cx.req.sender => {
            if !cx.owner {
                return Err(Msg::UnlinkOwnerOnly {}.into());
            }
            if cx.deps.identity.holder_of(*account).await?.is_none() {
                return Err(Msg::UnlinkUnknown {
                    account: account.get().to_string(),
                }
                .into());
            }
            match cx.deps.identity.split(*account, cx.deps.clock.now()).await {
                Ok(_) => Ok(cx.t(&Msg::UnlinkForced {})),
                Err(IdentityError::NotLinked) => Err(Fail::Say(Msg::NotLinked {})),
                Err(other) => Err(other.into()),
            }
        }
        _ => Err(Msg::UsageUnlink {}.into()),
    }
}

/// `/link @a @b`: an owner links two accounts directly, without an invitation.
async fn link_directly(cx: &Cx<'_>) -> Reply {
    let [a, b] = cx.req.mentions.as_slice() else {
        return Err(Msg::MentionsExact { count: 2 }.into());
    };
    if !cx.owner {
        return Err(Msg::LinkOwnerOnly {}.into());
    }
    for account in [a, b] {
        if cx.deps.identity.holder_of(*account).await?.is_none() {
            return Err(Msg::LinkUnknown {
                account: account.get().to_string(),
            }
            .into());
        }
    }
    match cx.deps.identity.merge(*a, *b).await? {
        MergeOutcome::Merged { .. } => Ok(cx.t(&Msg::LinkForced {})),
        MergeOutcome::AlreadyLinked(_) => Ok(cx.t(&Msg::LinkAlready {})),
    }
}
