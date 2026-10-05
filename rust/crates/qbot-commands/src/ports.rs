//! What commands need beyond the identity store and the task service.

use async_trait::async_trait;
use qbot_context::Item;
use qbot_core::{AccountId, GroupId, RunId, UnixMillis};
use qbot_store::{
    BlockRule, PgAdmin, PgGroupPolicy, PgRunLog, RosterRow, RunRecord, TopRow, UsageTotals,
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
    async fn block(
        &self,
        group: GroupId,
        account: AccountId,
        until: Option<UnixMillis>,
    ) -> Result<(), AdminError>;
    /// Whether a block was removed.
    async fn unblock(&self, group: GroupId, account: AccountId) -> Result<bool, AdminError>;
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
    async fn usage(
        &self,
        group: Option<GroupId>,
        since: UnixMillis,
    ) -> Result<UsageTotals, AdminError>;
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
    ) -> Result<(), AdminError> {
        self.policy
            .block(group, account, until)
            .await
            .map_err(admin)
    }

    async fn unblock(&self, group: GroupId, account: AccountId) -> Result<bool, AdminError> {
        self.policy.unblock(group, account).await.map_err(admin)
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
        since: UnixMillis,
    ) -> Result<UsageTotals, AdminError> {
        self.admin.usage(group, since).await.map_err(admin)
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
    /// `WARN` or `ERROR`.
    pub level: String,
    /// The message and its fields, on one line.
    pub text: String,
}

/// The process's recent warnings and errors, for `/logs`.
pub trait RecentLogs: Send + Sync {
    /// The newest `limit` lines, oldest first.
    fn recent(&self, limit: usize) -> Vec<LogLine>;
}
