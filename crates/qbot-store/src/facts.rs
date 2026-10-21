//! Facts on Postgres. The rules are those of `qbot_memory::facts::MemoryFactStore`; one
//! transaction per observation, serialized per group so concurrent observations of one claim
//! cannot both insert.

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, UnixMillis};
use qbot_memory::MemoryError;
use qbot_memory::facts::{
    Fact, FactId, FactStatus, FactStore, Observation, Retraction, normalize_key,
};
use qbot_memory::predicates::DecayClass;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

fn backend(error: impl std::fmt::Display) -> MemoryError {
    MemoryError::Backend(error.to_string())
}

pub(crate) const COLS: &str = "fact_id, group_id, subject_account, predicate, slot, key, object, label, status, \
    supports, decay, first_seen_ms, last_confirmed_ms, ended_ms";

pub(crate) fn to_fact(row: &PgRow) -> Result<Fact, MemoryError> {
    let status: String = row.get("status");
    let decay: String = row.get("decay");
    Ok(Fact {
        id: FactId(row.get("fact_id")),
        group: GroupId::new(row.get("group_id")).map_err(backend)?,
        subject: row
            .get::<Option<i64>, _>("subject_account")
            .map(AccountId::new)
            .transpose()
            .map_err(backend)?,
        predicate: row.get("predicate"),
        slot: row.get("slot"),
        key: row.get("key"),
        object: row.get("object"),
        label: row.get("label"),
        status: FactStatus::parse(&status)
            .ok_or_else(|| backend(format!("unknown fact status {status:?}")))?,
        supports: u32::try_from(row.get::<i32, _>("supports")).map_err(backend)?,
        first_seen: UnixMillis::new(row.get("first_seen_ms")),
        last_confirmed: UnixMillis::new(row.get("last_confirmed_ms")),
        ended: row.get::<Option<i64>, _>("ended_ms").map(UnixMillis::new),
        decay: DecayClass::parse(&decay)
            .ok_or_else(|| backend(format!("unknown decay class {decay:?}")))?,
    })
}

#[derive(Debug, Clone)]
pub struct PgFactStore {
    pool: PgPool,
}

impl PgFactStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

async fn lock_group(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group: GroupId,
) -> Result<(), MemoryError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('fact:' || $1::text, 0))")
        .bind(group.get())
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    Ok(())
}

/// The subject's active claim of `predicate` in `slot` whose value is `object` once normalized,
/// locked for the rest of the transaction. Values are compared in Rust, the one definition of
/// the comparison form.
async fn active_claim(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group: GroupId,
    subject: Option<AccountId>,
    predicate: &str,
    slot: &str,
    object: &str,
) -> Result<Option<Fact>, MemoryError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM fact WHERE group_id = $1 AND subject_account IS NOT DISTINCT FROM $2 \
         AND predicate = $3 AND slot = $4 AND status = 'active' ORDER BY fact_id FOR UPDATE"
    ))
    .bind(group.get())
    .bind(subject.map(AccountId::get))
    .bind(predicate)
    .bind(slot)
    .fetch_all(&mut **tx)
    .await
    .map_err(backend)?;
    let wanted = normalize_key(object);
    for row in &rows {
        let fact = to_fact(row)?;
        if normalize_key(&fact.object) == wanted {
            return Ok(Some(fact));
        }
    }
    Ok(None)
}

#[async_trait]
impl FactStore for PgFactStore {
    async fn observe(&self, obs: &Observation) -> Result<FactId, MemoryError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock_group(&mut tx, obs.group).await?;
        let same = active_claim(
            &mut tx,
            obs.group,
            obs.subject,
            &obs.predicate,
            &obs.slot,
            &obs.object,
        )
        .await?;
        let id = match same {
            Some(fact) => {
                let added = sqlx::query(
                    "INSERT INTO fact_evidence (fact_id, episode_id, message_id, quote) VALUES ($1, $2, $3, $4) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(fact.id.0)
                .bind(obs.episode.get())
                .bind(obs.message.get())
                .bind(&obs.quote)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
                if added.rows_affected() > 0 {
                    sqlx::query(
                        "UPDATE fact SET supports = supports + 1, last_confirmed_ms = greatest(last_confirmed_ms, $2) \
                         WHERE fact_id = $1",
                    )
                    .bind(fact.id.0)
                    .bind(obs.at.get())
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                }
                fact.id
            }
            None => {
                let id: i64 = sqlx::query(
                    "INSERT INTO fact (group_id, subject_account, predicate, slot, key, object, label, status, \
                     supports, decay, first_seen_ms, last_confirmed_ms) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, 'active', 1, $8, $9, $9) RETURNING fact_id",
                )
                .bind(obs.group.get())
                .bind(obs.subject.map(AccountId::get))
                .bind(&obs.predicate)
                .bind(&obs.slot)
                .bind(normalize_key(&obs.object))
                .bind(&obs.object)
                .bind(&obs.label)
                .bind(obs.decay.as_str())
                .bind(obs.at.get())
                .fetch_one(&mut *tx)
                .await
                .map_err(backend)?
                .get("fact_id");
                sqlx::query("INSERT INTO fact_evidence (fact_id, episode_id, message_id, quote) VALUES ($1, $2, $3, $4)")
                    .bind(id)
                    .bind(obs.episode.get())
                    .bind(obs.message.get())
                    .bind(&obs.quote)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                FactId(id)
            }
        };
        tx.commit().await.map_err(backend)?;
        Ok(id)
    }

    async fn current(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
    ) -> Result<Vec<Fact>, MemoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM fact WHERE group_id = $1 AND subject_account IS NOT DISTINCT FROM $2 \
             AND status = 'active' ORDER BY predicate COLLATE \"C\", last_confirmed_ms DESC, fact_id DESC"
        ))
        .bind(group.get())
        .bind(subject.map(AccountId::get))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(to_fact).collect()
    }

    async fn retract(&self, r: &Retraction) -> Result<bool, MemoryError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock_group(&mut tx, r.group).await?;
        let Some(fact) = active_claim(
            &mut tx,
            r.group,
            r.subject,
            &r.predicate,
            &r.slot,
            &r.object,
        )
        .await?
        else {
            return Ok(false);
        };
        sqlx::query("UPDATE fact SET ended_ms = greatest(ended_ms, $2) WHERE fact_id = $1")
            .bind(fact.id.0)
            .bind(r.at.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(true)
    }

    async fn forget(&self, group: GroupId, id: FactId) -> Result<bool, MemoryError> {
        let result = sqlx::query("UPDATE fact SET status = 'forgotten' WHERE fact_id = $1 AND group_id = $2 AND status = 'active'")
            .bind(id.0)
            .bind(group.get())
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(result.rows_affected() > 0)
    }
}
