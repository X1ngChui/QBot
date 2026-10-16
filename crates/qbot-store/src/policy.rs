//! Mute and block state, the admission gates, and who besides the bot owners may change them.

use std::sync::Arc;

use async_trait::async_trait;
use qbot_agent::{EnvError, GroupPolicy};
use qbot_core::{AccountId, Clock, GroupId, UnixMillis};
use sqlx::{PgPool, Row};

use crate::admin::BlockedBy;
use crate::error::StoreError;

#[derive(Clone)]
pub struct PgGroupPolicy {
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for PgGroupPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgGroupPolicy").finish_non_exhaustive()
    }
}

impl PgGroupPolicy {
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>) -> Self {
        Self { pool, clock }
    }

    pub async fn set_muted(&self, group: GroupId, muted: bool) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO group_state (group_id, muted, first_seen_ms) VALUES ($1, $2, $3) \
             ON CONFLICT (group_id) DO UPDATE SET muted = EXCLUDED.muted",
        )
        .bind(group.get())
        .bind(muted)
        .bind(self.clock.now().get())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A bot owner blocks an account until `until`, or until removed when `None`.
    pub async fn block(
        &self,
        group: GroupId,
        account: AccountId,
        until: Option<UnixMillis>,
    ) -> Result<(), StoreError> {
        self.block_as(group, account, until, BlockedBy::Owner)
            .await
            .map(|_| ())
    }

    /// Block an account until `until`, or until removed when `None`. Re-blocking replaces the
    /// previous expiry and who set it, except that a delegated admin never replaces a bot
    /// owner's block in force. Returns whether the block was set.
    pub async fn block_as(
        &self,
        group: GroupId,
        account: AccountId,
        until: Option<UnixMillis>,
        by: BlockedBy,
    ) -> Result<bool, StoreError> {
        let now = self.clock.now().get();
        let result = sqlx::query(
            "INSERT INTO group_block (group_id, account_id, until_ms, created_ms, by_owner) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (group_id, account_id) DO UPDATE \
             SET until_ms = EXCLUDED.until_ms, by_owner = EXCLUDED.by_owner \
             WHERE EXCLUDED.by_owner OR NOT group_block.by_owner \
                OR group_block.until_ms <= EXCLUDED.created_ms",
        )
        .bind(group.get())
        .bind(account.get())
        .bind(until.map(UnixMillis::get))
        .bind(now)
        .bind(by == BlockedBy::Owner)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// A bot owner lifts a block. Returns whether a block existed.
    pub async fn unblock(&self, group: GroupId, account: AccountId) -> Result<bool, StoreError> {
        self.unblock_as(group, account, BlockedBy::Owner).await
    }

    /// Lift a block; a delegated admin lifts only a delegated admin's. Returns whether a block
    /// was lifted.
    pub async fn unblock_as(
        &self,
        group: GroupId,
        account: AccountId,
        by: BlockedBy,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "DELETE FROM group_block WHERE group_id = $1 AND account_id = $2 \
             AND ($3 OR NOT by_owner)",
        )
        .bind(group.get())
        .bind(account.get())
        .bind(by == BlockedBy::Owner)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Make the account a delegated admin of the group, as a bot owner (`by`) asked. Returns
    /// whether it was not one already.
    pub async fn grant_admin(
        &self,
        group: GroupId,
        account: AccountId,
        by: AccountId,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "INSERT INTO group_admin (group_id, account_id, granted_by, granted_ms) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(group.get())
        .bind(account.get())
        .bind(by.get())
        .bind(self.clock.now().get())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Returns whether the account was a delegated admin of the group.
    pub async fn revoke_admin(
        &self,
        group: GroupId,
        account: AccountId,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM group_admin WHERE group_id = $1 AND account_id = $2")
            .bind(group.get())
            .bind(account.get())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

fn env(error: impl std::fmt::Display) -> EnvError {
    EnvError(error.to_string())
}

#[async_trait]
impl GroupPolicy for PgGroupPolicy {
    async fn is_muted(&self, group: GroupId) -> Result<bool, EnvError> {
        let row = sqlx::query("SELECT muted FROM group_state WHERE group_id = $1")
            .bind(group.get())
            .fetch_optional(&self.pool)
            .await
            .map_err(env)?;
        Ok(row.is_some_and(|r| r.get::<bool, _>("muted")))
    }

    async fn is_blocked(&self, group: GroupId, account: AccountId) -> Result<bool, EnvError> {
        let row = sqlx::query(
            "SELECT 1 AS blocked FROM group_block \
             WHERE group_id = $1 AND account_id = $2 AND (until_ms IS NULL OR until_ms > $3)",
        )
        .bind(group.get())
        .bind(account.get())
        .bind(self.clock.now().get())
        .fetch_optional(&self.pool)
        .await
        .map_err(env)?;
        Ok(row.is_some())
    }
}
