//! Memories on Postgres: `memory` (one row per statement remembered, in its state) and
//! `memory_evidence` (one row per supporting episode). The in-memory store defines the
//! semantics; the shared conformance suite runs against both.

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_memory::memories::{
    Change, Durability, Memory, MemoryId, MemoryState, MemoryStore, Statement, Support, memory_key,
};
use qbot_memory::{EpisodeId, MemoryError};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row, Transaction};

type Tx<'a> = Transaction<'a, Postgres>;

fn backend(error: impl std::fmt::Display) -> MemoryError {
    MemoryError::Backend(error.to_string())
}

#[derive(Debug, Clone)]
pub struct PgMemoryStore {
    pool: PgPool,
}

impl PgMemoryStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

const COLS: &str = "memory_id, group_id, subject_account, term, text, durability, state, \
                    replaced_by, closed_ms, first_ms, last_ms";

fn to_memory(row: &PgRow) -> Result<Memory, MemoryError> {
    let closed = row.get::<Option<i64>, _>("closed_ms").map(UnixMillis::new);
    let state = match (row.get::<&str, _>("state"), closed) {
        ("current", _) => MemoryState::Current,
        ("replaced", Some(at)) => MemoryState::Replaced {
            by: row.get::<Option<i64>, _>("replaced_by").map(MemoryId::new),
            at,
        },
        ("ended", Some(at)) => MemoryState::Ended { at },
        (other, _) => return Err(backend(format!("memory state {other}"))),
    };
    Ok(Memory {
        id: MemoryId::new(row.get("memory_id")),
        group: GroupId::new(row.get("group_id")).map_err(backend)?,
        subject: row
            .get::<Option<i64>, _>("subject_account")
            .map(AccountId::new)
            .transpose()
            .map_err(backend)?,
        term: row.get("term"),
        text: row.get("text"),
        durability: Durability::parse(row.get::<&str, _>("durability"))
            .ok_or_else(|| backend("unknown durability"))?,
        state,
        first: UnixMillis::new(row.get("first_ms")),
        last: UnixMillis::new(row.get("last_ms")),
        support: Vec::new(),
    })
}

/// The memories of `rows` with their evidence.
async fn with_support(pool: &PgPool, rows: Vec<PgRow>) -> Result<Vec<Memory>, MemoryError> {
    let mut memories = rows.iter().map(to_memory).collect::<Result<Vec<_>, _>>()?;
    let ids: Vec<i64> = memories.iter().map(|m| m.id.get()).collect();
    let evidence = sqlx::query(
        "SELECT memory_id, episode_id, message_id, author_account, quote, at_ms \
         FROM memory_evidence WHERE memory_id = ANY($1) ORDER BY at_ms, episode_id",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await
    .map_err(backend)?;
    for row in evidence {
        let id = MemoryId::new(row.get("memory_id"));
        if let Some(memory) = memories.iter_mut().find(|m| m.id == id) {
            memory.support.push(Support {
                episode: EpisodeId::new(row.get("episode_id")),
                message: MessageId::new(row.get("message_id")).map_err(backend)?,
                author: row
                    .get::<Option<i64>, _>("author_account")
                    .map(AccountId::new)
                    .transpose()
                    .map_err(backend)?,
                quote: row.get("quote"),
                at: UnixMillis::new(row.get("at_ms")),
            });
        }
    }
    Ok(memories)
}

/// Whether `id` is a current memory of `subject` in the group, locked for the change.
async fn current_of(
    tx: &mut Tx<'_>,
    group: GroupId,
    id: MemoryId,
    subject: Option<AccountId>,
) -> Result<bool, MemoryError> {
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT memory_id FROM memory WHERE memory_id = $1 AND group_id = $2 \
         AND subject_account IS NOT DISTINCT FROM $3 AND state = 'current' FOR UPDATE",
    )
    .bind(id.get())
    .bind(group.get())
    .bind(subject.map(|s| s.get()))
    .fetch_optional(&mut **tx)
    .await
    .map_err(backend)?;
    Ok(found.is_some())
}

async fn support(tx: &mut Tx<'_>, id: MemoryId, support: &Support) -> Result<(), MemoryError> {
    let added = sqlx::query(
        "INSERT INTO memory_evidence (memory_id, episode_id, message_id, author_account, quote, \
         at_ms) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
    )
    .bind(id.get())
    .bind(support.episode.get())
    .bind(support.message.get())
    .bind(support.author.map(|a| a.get()))
    .bind(&support.quote)
    .bind(support.at.get())
    .execute(&mut **tx)
    .await
    .map_err(backend)?
    .rows_affected();
    if added > 0 {
        sqlx::query(
            "UPDATE memory SET first_ms = least(first_ms, $2), last_ms = greatest(last_ms, $2) \
             WHERE memory_id = $1",
        )
        .bind(id.get())
        .bind(support.at.get())
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    }
    Ok(())
}

/// The memory a statement is: the current one with its key, supported, or a new one.
async fn remember(
    tx: &mut Tx<'_>,
    group: GroupId,
    statement: &Statement,
) -> Result<MemoryId, MemoryError> {
    let key = memory_key(statement.term.as_deref(), &statement.text);
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT memory_id FROM memory WHERE group_id = $1 \
         AND subject_account IS NOT DISTINCT FROM $2 AND key = $3 AND state = 'current'",
    )
    .bind(group.get())
    .bind(statement.subject.map(|s| s.get()))
    .bind(&key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(backend)?;
    let id = match existing {
        Some(id) => MemoryId::new(id),
        None => MemoryId::new(
            sqlx::query_scalar(
                "INSERT INTO memory (group_id, subject_account, term, text, key, durability, \
                 state, first_ms, last_ms) \
                 VALUES ($1, $2, $3, $4, $5, $6, 'current', $7, $7) RETURNING memory_id",
            )
            .bind(group.get())
            .bind(statement.subject.map(|s| s.get()))
            .bind(&statement.term)
            .bind(&statement.text)
            .bind(&key)
            .bind(statement.durability.as_str())
            .bind(statement.support.at.get())
            .fetch_one(&mut **tx)
            .await
            .map_err(backend)?,
        ),
    };
    support(tx, id, &statement.support).await?;
    Ok(id)
}

#[async_trait]
impl MemoryStore for PgMemoryStore {
    async fn apply(
        &self,
        group: GroupId,
        changes: &[Change],
    ) -> Result<Vec<MemoryId>, MemoryError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        // One writer of a group's memories at a time: two statements of one key would otherwise
        // both find no current memory and race on the unique index.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('memory:' || $1::text, 0))")
            .bind(group.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let mut touched = Vec::new();
        for change in changes {
            match change {
                Change::New(statement) => touched.push(remember(&mut tx, group, statement).await?),
                Change::Same {
                    id,
                    subject,
                    support: s,
                } => {
                    if current_of(&mut tx, group, *id, *subject).await? {
                        support(&mut tx, *id, s).await?;
                        touched.push(*id);
                    }
                }
                Change::Replace { id, with } => {
                    let replaced = current_of(&mut tx, group, *id, with.subject).await?;
                    let new = remember(&mut tx, group, with).await?;
                    touched.push(new);
                    if replaced && new != *id {
                        sqlx::query(
                            "UPDATE memory SET state = 'replaced', replaced_by = $2, closed_ms = $3 \
                             WHERE memory_id = $1",
                        )
                        .bind(id.get())
                        .bind(new.get())
                        .bind(with.support.at.get())
                        .execute(&mut *tx)
                        .await
                        .map_err(backend)?;
                    }
                }
                Change::End { id, subject, at } => {
                    if current_of(&mut tx, group, *id, *subject).await? {
                        sqlx::query(
                            "UPDATE memory SET state = 'ended', closed_ms = $2 WHERE memory_id = $1",
                        )
                        .bind(id.get())
                        .bind(at.get())
                        .execute(&mut *tx)
                        .await
                        .map_err(backend)?;
                    }
                }
            }
        }
        tx.commit().await.map_err(backend)?;
        touched.sort();
        touched.dedup();
        Ok(touched)
    }

    async fn about(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
    ) -> Result<Vec<Memory>, MemoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM memory WHERE group_id = $1 \
             AND subject_account IS NOT DISTINCT FROM $2 ORDER BY memory_id"
        ))
        .bind(group.get())
        .bind(subject.map(|s| s.get()))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        with_support(&self.pool, rows).await
    }

    async fn current(
        &self,
        group: GroupId,
        subjects: &[Option<AccountId>],
    ) -> Result<Vec<Memory>, MemoryError> {
        let accounts: Vec<i64> = subjects.iter().flatten().map(|a| a.get()).collect();
        let group_too = subjects.contains(&None);
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM memory WHERE group_id = $1 AND state = 'current' \
             AND (subject_account = ANY($2) OR ($3 AND subject_account IS NULL)) \
             ORDER BY memory_id"
        ))
        .bind(group.get())
        .bind(&accounts)
        .bind(group_too)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        with_support(&self.pool, rows).await
    }

    async fn get(&self, group: GroupId, id: MemoryId) -> Result<Option<Memory>, MemoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM memory WHERE group_id = $1 AND memory_id = $2"
        ))
        .bind(group.get())
        .bind(id.get())
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(with_support(&self.pool, rows).await?.pop())
    }

    async fn delete(&self, group: GroupId, id: MemoryId) -> Result<bool, MemoryError> {
        Ok(self.evict(group, &[id]).await? > 0)
    }

    async fn delete_about(&self, group: GroupId, subject: AccountId) -> Result<u64, MemoryError> {
        let gone = sqlx::query("DELETE FROM memory WHERE group_id = $1 AND subject_account = $2")
            .bind(group.get())
            .bind(subject.get())
            .execute(&self.pool)
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(gone)
    }

    async fn evict(&self, group: GroupId, ids: &[MemoryId]) -> Result<u64, MemoryError> {
        let ids: Vec<i64> = ids.iter().map(|id| id.get()).collect();
        let gone = sqlx::query("DELETE FROM memory WHERE group_id = $1 AND memory_id = ANY($2)")
            .bind(group.get())
            .bind(&ids)
            .execute(&self.pool)
            .await
            .map_err(backend)?
            .rows_affected();
        Ok(gone)
    }
}
