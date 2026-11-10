//! The name index on Postgres: `name_use` (one row per member, name and episode, or a name a
//! person set) and `name_removed` (names a person removed from a member).

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_memory::names::{
    KnownName, NameBasis, NameError, NameStore, NameUse, UseSource, name_key, summarize, tidy_name,
};
use qbot_memory::{EpisodeId, MemoryError};
use sqlx::{PgExecutor, PgPool, Row};

use crate::identity::ensure_account;

fn backend(error: impl std::fmt::Display) -> MemoryError {
    MemoryError::Backend(error.to_string())
}

#[derive(Debug, Clone)]
pub struct PgNameStore {
    pool: PgPool,
}

impl PgNameStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// The group's names, per member and name, for the given accounts or all of them.
pub(crate) async fn known_names<'e>(
    executor: impl PgExecutor<'e>,
    group: GroupId,
    accounts: Option<&[AccountId]>,
) -> Result<Vec<KnownName>, MemoryError> {
    let wanted: Option<Vec<i64>> = accounts.map(|a| a.iter().map(|x| x.get()).collect());
    let rows = sqlx::query(
        "SELECT account_id, key, text, basis, episode_id, message_id, quote, anchored, at_ms \
         FROM name_use \
         WHERE group_id = $1 AND ($2::bigint[] IS NULL OR account_id = ANY($2)) \
         ORDER BY account_id, key, at_ms, coalesce(episode_id, -1)",
    )
    .bind(group.get())
    .bind(wanted)
    .fetch_all(executor)
    .await
    .map_err(backend)?;
    let mut uses = Vec::with_capacity(rows.len());
    for row in rows {
        let basis = NameBasis::parse(row.get::<&str, _>("basis"))
            .ok_or_else(|| backend("unknown name basis"))?;
        let source = match row.get::<Option<i64>, _>("episode_id") {
            Some(episode) => Some(UseSource {
                episode: EpisodeId::new(episode),
                message: MessageId::new(row.get::<Option<i64>, _>("message_id").unwrap_or(1))
                    .map_err(backend)?,
                quote: row.get::<Option<String>, _>("quote").unwrap_or_default(),
            }),
            None => None,
        };
        uses.push((
            row.get::<String, _>("key"),
            NameUse {
                account: AccountId::new(row.get("account_id")).map_err(backend)?,
                text: row.get("text"),
                basis,
                source,
                anchored: row.get("anchored"),
                at: UnixMillis::new(row.get("at_ms")),
            },
        ));
    }
    Ok(summarize(uses.iter().map(|(k, u)| (k.as_str(), u))))
}

#[async_trait]
impl NameStore for PgNameStore {
    async fn record(&self, group: GroupId, uses: &[NameUse]) -> Result<u64, MemoryError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let mut recorded = 0;
        for one in uses {
            let (Ok(key), Some(source)) = (name_key(&one.text), &one.source) else {
                continue;
            };
            if one.basis == NameBasis::Set {
                continue;
            }
            let done = sqlx::query(
                "INSERT INTO name_use (group_id, account_id, key, text, basis, episode_id, \
                 message_id, quote, at_ms, anchored) \
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10 \
                 WHERE EXISTS (SELECT 1 FROM account WHERE account_id = $2) \
                   AND NOT EXISTS (SELECT 1 FROM name_removed \
                                   WHERE group_id = $1 AND account_id = $2 AND key = $3) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(group.get())
            .bind(one.account.get())
            .bind(&key)
            .bind(tidy_name(&one.text))
            .bind(one.basis.as_str())
            .bind(source.episode.get())
            .bind(source.message.get())
            .bind(&source.quote)
            .bind(one.at.get())
            .bind(one.anchored)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
            recorded += done.rows_affected();
        }
        tx.commit().await.map_err(backend)?;
        Ok(recorded)
    }

    async fn set(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<(), NameError> {
        let key = name_key(text)?;
        let ids: Vec<i64> = accounts.iter().map(|a| a.get()).collect();
        let mut tx = self.pool.begin().await.map_err(backend)?;
        // One setter at a time per group and name, so two people cannot set it for two members.
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('name:' || $1::text || ':' || $2, 0))",
        )
        .bind(group.get())
        .bind(&key)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM name_use WHERE group_id = $1 AND key = $2 \
             AND basis = 'set' AND NOT (account_id = ANY($3)))",
        )
        .bind(group.get())
        .bind(&key)
        .bind(&ids)
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?;
        if taken {
            return Err(NameError::Taken);
        }
        for account in accounts {
            ensure_account(&mut tx, account.get(), at.get())
                .await
                .map_err(backend)?;
            sqlx::query(
                "DELETE FROM name_removed WHERE group_id = $1 AND account_id = $2 AND key = $3",
            )
            .bind(group.get())
            .bind(account.get())
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
            sqlx::query(
                "INSERT INTO name_use (group_id, account_id, key, text, basis, at_ms) \
                 VALUES ($1, $2, $3, $4, 'set', $5) \
                 ON CONFLICT (group_id, account_id, key, (coalesce(episode_id, -1))) \
                 DO UPDATE SET text = EXCLUDED.text, at_ms = EXCLUDED.at_ms",
            )
            .bind(group.get())
            .bind(account.get())
            .bind(&key)
            .bind(tidy_name(text))
            .bind(at.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    async fn remove(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<bool, NameError> {
        let key = name_key(text)?;
        let ids: Vec<i64> = accounts.iter().map(|a| a.get()).collect();
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let gone = sqlx::query(
            "DELETE FROM name_use WHERE group_id = $1 AND key = $2 AND account_id = ANY($3)",
        )
        .bind(group.get())
        .bind(&key)
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .map_err(backend)?
        .rows_affected();
        sqlx::query(
            "INSERT INTO name_removed (group_id, account_id, key, removed_ms) \
             SELECT $1, a, $2, $4 FROM unnest($3::bigint[]) a \
             WHERE EXISTS (SELECT 1 FROM account WHERE account_id = a) \
             ON CONFLICT (group_id, account_id, key) DO UPDATE SET removed_ms = EXCLUDED.removed_ms",
        )
        .bind(group.get())
        .bind(&key)
        .bind(&ids)
        .bind(at.get())
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(gone > 0)
    }

    async fn names(
        &self,
        group: GroupId,
        accounts: Option<&[AccountId]>,
    ) -> Result<Vec<KnownName>, MemoryError> {
        known_names(&self.pool, group, accounts).await
    }
}
