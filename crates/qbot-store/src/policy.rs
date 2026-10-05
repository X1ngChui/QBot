//! Mute and block state: the admission gates.

use std::sync::Arc;

use async_trait::async_trait;
use qbot_agent::{EnvError, GroupPolicy};
use qbot_core::{AccountId, Clock, GroupId, UnixMillis};
use sqlx::{PgPool, Row};

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

    /// Block an account until `until`, or until removed when `None`. Re-blocking replaces the
    /// previous expiry.
    pub async fn block(
        &self,
        group: GroupId,
        account: AccountId,
        until: Option<UnixMillis>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO group_block (group_id, account_id, until_ms, created_ms) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (group_id, account_id) DO UPDATE SET until_ms = EXCLUDED.until_ms",
        )
        .bind(group.get())
        .bind(account.get())
        .bind(until.map(UnixMillis::get))
        .bind(self.clock.now().get())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Returns whether a block existed.
    pub async fn unblock(&self, group: GroupId, account: AccountId) -> Result<bool, StoreError> {
        let result = sqlx::query("DELETE FROM group_block WHERE group_id = $1 AND account_id = $2")
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
