mod admin;
mod identity;
mod notes;
mod ops;
mod profile;
mod tasks;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use qbot_core::{AccountId, UnixMillis};
use qbot_i18n::Msg;

use crate::args;
use crate::fail::Fail;
use crate::router::{Cx, Standing};

pub(crate) use admin::{admin, block, members, mute, stats, top};
pub(crate) use identity::{link, unlink};
pub(crate) use notes::note;
pub(crate) use ops::{logs, runs};
pub(crate) use profile::{forget, group, help, name, who};
pub(crate) use tasks::tasks;

pub(crate) type Reply = Result<String, Fail>;

/// Where a person of this group stands, judged by every account linked to theirs here: one bot
/// owner's or delegated admin's account makes the whole person one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rank {
    BotOwner,
    DelegatedAdmin,
    Member,
}

impl Cx<'_> {
    /// Keep a reply to one message.
    pub fn fit(&self, text: &str) -> String {
        let limit = self.deps.settings.max_message_chars.saturating_sub(2);
        args::fit(text, limit, &self.t(&Msg::Truncated {}))
    }

    /// The accounts of one person in this group: `account` and those linked to it here.
    /// Links made in other groups are not this group's to see or act on.
    pub async fn linked_accounts(&self, account: AccountId) -> Result<Vec<AccountId>, Fail> {
        Ok(self.deps.identity.linked(self.group(), account).await?)
    }

    /// Where the person `account` belongs to stands in this group.
    pub async fn rank_of(&self, account: AccountId) -> Result<Rank, Fail> {
        let person = self.linked_accounts(account).await?;
        if person.iter().any(|a| self.is_owner(*a)) {
            return Ok(Rank::BotOwner);
        }
        for a in &person {
            if self.deps.admin.is_admin(self.group(), *a).await? {
                return Ok(Rank::DelegatedAdmin);
            }
        }
        Ok(Rank::Member)
    }

    /// Who a command acts on: the one mentioned account, or the sender. Everyone may act on
    /// themselves and the accounts linked to them here; a delegated admin also on the group's
    /// ordinary members (never a bot owner or another delegated admin); a bot owner on anyone.
    pub async fn target(&self) -> Result<AccountId, Fail> {
        let mentions = &self.req.mentions;
        if mentions.len() > 1 {
            return Err(Msg::MentionsMax { max: 1 }.into());
        }
        let account = mentions.first().copied().unwrap_or(self.req.sender);
        if self.owner
            || account == self.req.sender
            || self
                .linked_accounts(self.req.sender)
                .await?
                .contains(&account)
        {
            return Ok(account);
        }
        match self.standing {
            Standing::DelegatedAdmin => match self.rank_of(account).await? {
                Rank::Member => Ok(account),
                Rank::BotOwner | Rank::DelegatedAdmin => {
                    Err(Msg::DelegatedTargetNotOrdinary {}.into())
                }
            },
            Standing::Unknown => Err(Msg::AuthorityUnknown {}.into()),
            Standing::BotOwner | Standing::Member => Err(Msg::OwnAccountsOnly {}.into()),
        }
    }

    /// Whether the command acts on someone other than the sender's own person: a delegated
    /// admin's management of another member, or a bot owner's.
    pub async fn on_someone_else(&self, account: AccountId) -> Result<bool, Fail> {
        Ok(account != self.req.sender
            && !self
                .linked_accounts(self.req.sender)
                .await?
                .contains(&account))
    }

    pub fn join_names(&self, parts: &[String]) -> String {
        parts.join(&self.t(&Msg::ListSeparator {}))
    }

    pub fn scope_label(&self, all: bool) -> String {
        self.t(&if all {
            Msg::ScopeLinked {}
        } else {
            Msg::ScopeExact {}
        })
    }

    pub fn zoned(&self, at: UnixMillis) -> Option<jiff::Zoned> {
        Timestamp::from_millisecond(at.get())
            .ok()
            .map(|t| t.to_zoned(self.deps.settings.zone.clone()))
    }

    /// `MM-DD HH:MM` in the configured zone.
    pub fn short_time(&self, at: UnixMillis) -> String {
        self.zoned(at)
            .map_or_else(|| "??".into(), |z| z.strftime("%m-%d %H:%M").to_string())
    }

    /// ISO 8601 with seconds and offset, in the configured zone.
    pub fn iso_time(&self, at: UnixMillis) -> String {
        self.zoned(at).map_or_else(
            || "??".into(),
            |z| z.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        )
    }
}

/// Midnight starting the local day (or month) that contains `at`.
pub(crate) fn period_start(
    zone: &TimeZone,
    at: UnixMillis,
    month: bool,
) -> Result<UnixMillis, Fail> {
    let zoned = Timestamp::from_millisecond(at.get())
        .map_err(|e| Fail::Storage(e.to_string()))?
        .to_zoned(zone.clone());
    let zoned = if month {
        zoned
            .with()
            .day(1)
            .build()
            .map_err(|e| Fail::Storage(e.to_string()))?
    } else {
        zoned
    };
    let start = zoned
        .start_of_day()
        .map_err(|e| Fail::Storage(e.to_string()))?;
    Ok(UnixMillis::new(start.timestamp().as_millisecond()))
}
