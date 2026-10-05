//! The picture-description cache.

use std::sync::Arc;

use qbot_core::{Clock, UnixMillis};
use sqlx::{PgPool, Row};

use crate::error::StoreError;

#[derive(Clone)]
pub struct PgMediaCache {
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for PgMediaCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgMediaCache").finish_non_exhaustive()
    }
}

impl PgMediaCache {
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>) -> Self {
        Self { pool, clock }
    }

    pub async fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let row = sqlx::query("SELECT description FROM media_cache WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("description")))
    }

    /// Remember a description. A later description for the same key replaces the earlier one.
    pub async fn put(&self, key: &str, description: &str) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO media_cache (key, kind, description, created_ms) VALUES ($1, 'image', $2, $3) \
             ON CONFLICT (key) DO UPDATE SET description = EXCLUDED.description, created_ms = EXCLUDED.created_ms",
        )
        .bind(key)
        .bind(description)
        .bind(self.clock.now().get())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forget descriptions older than `before`. Returns how many.
    pub async fn expire(&self, before: UnixMillis) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM media_cache WHERE created_ms < $1")
            .bind(before.get())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}
