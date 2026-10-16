//! What commands need beyond the identity store and the task service.

use async_trait::async_trait;
use qbot_context::Item;
use qbot_core::{AccountId, GroupId, RunId, UnixMillis};
use qbot_store::{
    BlockRule, BlockedBy, PgAdmin, PgGroupPolicy, PgRunLog, RosterRow, RunRecord, TopRow,
    UsageSummary,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct AdminError(pub String);

fn admin(error: impl std::fmt::Display) -> AdminError {
    AdminError(error.to_string())
}

#[async_trait]
pub trait GroupAdmin: Send + Sync {
    async fn is_muted(&self, group: GroupId) -> Result<bool, AdminError>;
    async fn set_muted(&self, group: GroupId, muted: bool) -> Result<(), AdminError>;
    /// Blocks in force now.
    async fn blocks(&self, group: GroupId) -> Result<Vec<BlockRule>, AdminError>;
    /// Whether the block was set: a delegated admin never replaces a bot owner's block.
    async fn block(
        &self,
        group: GroupId,
        account: AccountId,
        until: Option<UnixMillis>,
        by: BlockedBy,
    ) -> Result<bool, AdminError>;
    /// Whether a block was removed: a delegated admin removes only a delegated admin's.
    async fn unblock(
        &self,
        group: GroupId,
        account: AccountId,
        by: BlockedBy,
    ) -> Result<bool, AdminError>;
    /// The group's delegated admins, oldest grant first.
    async fn admins(&self, group: GroupId) -> Result<Vec<AccountId>, AdminError>;
    /// Whether this very account is a delegated admin of the group.
    async fn is_admin(&self, group: GroupId, account: AccountId) -> Result<bool, AdminError>;
    /// Whether the account was not an admin already.
    async fn grant_admin(
        &self,
        group: GroupId,
        account: AccountId,
        by: AccountId,
    ) -> Result<bool, AdminError>;
    /// Whether the account was an admin.
    async fn revoke_admin(&self, group: GroupId, account: AccountId) -> Result<bool, AdminError>;
    async fn member_number(
        &self,
        group: GroupId,
        account: AccountId,
    ) -> Result<Option<u32>, AdminError>;
    async fn message_count(
        &self,
        group: GroupId,
        accounts: &[AccountId],
    ) -> Result<u64, AdminError>;
    /// Total members on record, and the most active `limit`.
    async fn roster(&self, group: GroupId, limit: i64)
    -> Result<(u64, Vec<RosterRow>), AdminError>;
    /// What was measured in `[from, to)`, for one group or (`None`) all of them.
    async fn usage(
        &self,
        group: Option<GroupId>,
        from: UnixMillis,
        to: UnixMillis,
    ) -> Result<UsageSummary, AdminError>;
    async fn top_requesters(
        &self,
        group: GroupId,
        since: UnixMillis,
        limit: i64,
    ) -> Result<Vec<TopRow>, AdminError>;
}

/// Postgres-backed [`GroupAdmin`].
#[derive(Debug, Clone)]
pub struct PgGroupAdmin {
    admin: PgAdmin,
    policy: PgGroupPolicy,
}

impl PgGroupAdmin {
    pub fn new(admin: PgAdmin, policy: PgGroupPolicy) -> Self {
        Self { admin, policy }
    }
}

#[async_trait]
impl GroupAdmin for PgGroupAdmin {
    async fn is_muted(&self, group: GroupId) -> Result<bool, AdminError> {
        self.admin.is_muted(group).await.map_err(admin)
    }

    async fn set_muted(&self, group: GroupId, muted: bool) -> Result<(), AdminError> {
        self.policy.set_muted(group, muted).await.map_err(admin)
    }

    async fn blocks(&self, group: GroupId) -> Result<Vec<BlockRule>, AdminError> {
        self.admin.blocks(group).await.map_err(admin)
    }

    async fn block(
        &self,
        group: GroupId,
        account: AccountId,
        until: Option<UnixMillis>,
        by: BlockedBy,
    ) -> Result<bool, AdminError> {
        self.policy
            .block_as(group, account, until, by)
            .await
            .map_err(admin)
    }

    async fn unblock(
        &self,
        group: GroupId,
        account: AccountId,
        by: BlockedBy,
    ) -> Result<bool, AdminError> {
        self.policy
            .unblock_as(group, account, by)
            .await
            .map_err(admin)
    }

    async fn admins(&self, group: GroupId) -> Result<Vec<AccountId>, AdminError> {
        self.admin.admins(group).await.map_err(admin)
    }

    async fn is_admin(&self, group: GroupId, account: AccountId) -> Result<bool, AdminError> {
        self.admin.is_admin(group, account).await.map_err(admin)
    }

    async fn grant_admin(
        &self,
        group: GroupId,
        account: AccountId,
        by: AccountId,
    ) -> Result<bool, AdminError> {
        self.policy
            .grant_admin(group, account, by)
            .await
            .map_err(admin)
    }

    async fn revoke_admin(&self, group: GroupId, account: AccountId) -> Result<bool, AdminError> {
        self.policy
            .revoke_admin(group, account)
            .await
            .map_err(admin)
    }

    async fn member_number(
        &self,
        group: GroupId,
        account: AccountId,
    ) -> Result<Option<u32>, AdminError> {
        self.admin
            .member_number(group, account)
            .await
            .map_err(admin)
    }

    async fn message_count(
        &self,
        group: GroupId,
        accounts: &[AccountId],
    ) -> Result<u64, AdminError> {
        self.admin
            .message_count(group, accounts)
            .await
            .map_err(admin)
    }

    async fn roster(
        &self,
        group: GroupId,
        limit: i64,
    ) -> Result<(u64, Vec<RosterRow>), AdminError> {
        self.admin.roster(group, limit).await.map_err(admin)
    }

    async fn usage(
        &self,
        group: Option<GroupId>,
        from: UnixMillis,
        to: UnixMillis,
    ) -> Result<UsageSummary, AdminError> {
        self.admin.usage(group, from, to).await.map_err(admin)
    }

    async fn top_requesters(
        &self,
        group: GroupId,
        since: UnixMillis,
        limit: i64,
    ) -> Result<Vec<TopRow>, AdminError> {
        self.admin
            .top_requesters(group, since, limit)
            .await
            .map_err(admin)
    }
}

/// Runs of a group, for `/runs`.
#[async_trait]
pub trait RunHistory: Send + Sync {
    /// The group's latest runs, newest first.
    async fn recent(&self, group: GroupId, limit: i64) -> Result<Vec<RunRecord>, AdminError>;
    /// One run of the group with its items; `None` for an unknown run or one of another group.
    async fn run(
        &self,
        group: GroupId,
        run: RunId,
    ) -> Result<Option<(RunRecord, Vec<Item>)>, AdminError>;
}

#[async_trait]
impl RunHistory for PgRunLog {
    async fn recent(&self, group: GroupId, limit: i64) -> Result<Vec<RunRecord>, AdminError> {
        PgRunLog::recent(self, group, limit).await.map_err(admin)
    }

    async fn run(
        &self,
        group: GroupId,
        run: RunId,
    ) -> Result<Option<(RunRecord, Vec<Item>)>, AdminError> {
        let Some(record) = self.record(run).await.map_err(admin)? else {
            return Ok(None);
        };
        if record.group != group {
            return Ok(None);
        }
        let items = self.load_items(run).await.map_err(admin)?;
        Ok(Some((record, items)))
    }
}

/// One captured log event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub at: UnixMillis,
    /// The group the event was about (its `group` field), if any.
    pub group: Option<GroupId>,
    /// `WARN` or `ERROR`.
    pub level: String,
    /// The message and its fields, on one line.
    pub text: String,
}

/// The process's recent warnings and errors, for `/logs`.
pub trait RecentLogs: Send + Sync {
    /// The newest `limit` lines about `group` or about no group, oldest first. Another group's
    /// lines are never shown: what went wrong there may quote its chat.
    fn recent(&self, group: GroupId, limit: usize) -> Vec<LogLine>;
}
