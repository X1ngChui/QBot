//! Episodes and their embeddings on Postgres.

use std::sync::Arc;

use async_trait::async_trait;
use qbot_core::{AccountId, Clock, GroupId, MessageId, UnixMillis};
use qbot_memory::{
    Episode, EpisodeId, EpisodeStore, Evidence, Hit, MemoryError, NewEpisode, SliceLine,
};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

fn backend(error: impl std::fmt::Display) -> MemoryError {
    MemoryError::Backend(error.to_string())
}

/// `23P01` is exclusion_violation: the range overlaps another episode of the group.
fn insert_error(error: sqlx::Error) -> MemoryError {
    if let sqlx::Error::Database(db) = &error
        && db.code().as_deref() == Some("23P01")
    {
        return MemoryError::Overlap;
    }
    backend(error)
}

fn u64_of(value: i64) -> Result<u64, MemoryError> {
    u64::try_from(value).map_err(backend)
}

fn i64_of(value: u64) -> Result<i64, MemoryError> {
    i64::try_from(value).map_err(backend)
}

/// pgvector's text form: `[0.1,0.2,...]`.
fn vector_text(vector: &[f32]) -> String {
    let parts: Vec<String> = vector.iter().map(|v| v.to_string()).collect();
    format!("[{}]", parts.join(","))
}

const EPISODE_COLS: &str = "e.episode_id, e.group_id, e.first_batch, e.last_batch, e.first_ordinal, e.last_ordinal, e.batch_lines, \
    e.first_message, e.last_message, e.started_ms, e.ended_ms, e.line_count, e.participants, e.title, e.summary, \
    e.evidence, e.method, e.model, e.created_ms, e.findings";

fn to_episode(row: &PgRow) -> Result<Episode, MemoryError> {
    let participants = row
        .get::<Vec<i64>, _>("participants")
        .into_iter()
        .map(|a| AccountId::new(a).map_err(backend))
        .collect::<Result<Vec<_>, _>>()?;
    let evidence: Vec<Evidence> = serde_json::from_value(row.get("evidence")).map_err(backend)?;
    Ok(Episode {
        id: EpisodeId::new(row.get("episode_id")),
        created: UnixMillis::new(row.get("created_ms")),
        episode: NewEpisode {
            group: GroupId::new(row.get("group_id")).map_err(backend)?,
            first_batch: u64_of(row.get("first_batch"))?,
            last_batch: u64_of(row.get("last_batch"))?,
            first_ordinal: u64_of(row.get("first_ordinal"))?,
            last_ordinal: u64_of(row.get("last_ordinal"))?,
            batch_lines: u32::try_from(row.get::<i32, _>("batch_lines")).map_err(backend)?,
            first_message: MessageId::new(row.get("first_message")).map_err(backend)?,
            last_message: MessageId::new(row.get("last_message")).map_err(backend)?,
            started: UnixMillis::new(row.get("started_ms")),
            ended: UnixMillis::new(row.get("ended_ms")),
            line_count: u32::try_from(row.get::<i32, _>("line_count")).map_err(backend)?,
            participants,
            title: row.get("title"),
            summary: row.get("summary"),
            evidence,
            method: row.get("method"),
            model: row.get("model"),
            findings: serde_json::from_value(row.get("findings")).map_err(backend)?,
        },
    })
}

#[derive(Clone)]
pub struct PgEpisodeStore {
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for PgEpisodeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgEpisodeStore").finish_non_exhaustive()
    }
}

impl PgEpisodeStore {
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>) -> Self {
        Self { pool, clock }
    }
}

#[async_trait]
impl EpisodeStore for PgEpisodeStore {
    async fn lines(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<SliceLine>, MemoryError> {
        let rows = sqlx::query(
            "SELECT l.ordinal, l.message_id, l.account_id, l.at_ms, l.text, m.number \
             FROM chat_line l LEFT JOIN member_number m ON m.group_id = l.group_id AND m.account_id = l.account_id \
             WHERE l.group_id = $1 AND l.ordinal BETWEEN $2 AND $3 ORDER BY l.ordinal",
        )
        .bind(group.get())
        .bind(i64_of(first_ordinal)?)
        .bind(i64_of(last_ordinal)?)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter()
            .map(|row| {
                Ok(SliceLine {
                    ordinal: u64_of(row.get("ordinal"))?,
                    message: MessageId::new(row.get("message_id")).map_err(backend)?,
                    speaker: row
                        .get::<Option<i64>, _>("account_id")
                        .map(|a| AccountId::new(a).map_err(backend))
                        .transpose()?,
                    member_no: row
                        .get::<Option<i32>, _>("number")
                        .and_then(|n| u32::try_from(n).ok()),
                    at: UnixMillis::new(row.get("at_ms")),
                    text: row.get("text"),
                })
            })
            .collect()
    }

    async fn last_ordinal(&self, group: GroupId) -> Result<u64, MemoryError> {
        let last: Option<i64> =
            sqlx::query("SELECT max(ordinal) AS last FROM chat_line WHERE group_id = $1")
                .bind(group.get())
                .fetch_one(&self.pool)
                .await
                .map_err(backend)?
                .get("last");
        u64_of(last.unwrap_or(0))
    }

    async fn covered_through(&self, group: GroupId) -> Result<u64, MemoryError> {
        let last: Option<i64> =
            sqlx::query("SELECT max(last_ordinal) AS last FROM episode WHERE group_id = $1")
                .bind(group.get())
                .fetch_one(&self.pool)
                .await
                .map_err(backend)?
                .get("last");
        u64_of(last.unwrap_or(0))
    }

    async fn insert(
        &self,
        episode: &NewEpisode,
        vector: &[f32],
        embed_model: &str,
    ) -> Result<Episode, MemoryError> {
        if episode.first_ordinal > episode.last_ordinal || episode.first_batch > episode.last_batch
        {
            return Err(MemoryError::InvalidRange);
        }
        let mut tx = self.pool.begin().await.map_err(backend)?;
        // The exclusion constraint alone decides overlap, but concurrent inserts checking it can
        // deadlock on each other; one inserter per group at a time turns that into `Overlap`.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('episode:' || $1::text, 0))")
            .bind(episode.group.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let evidence = serde_json::to_value(&episode.evidence).map_err(backend)?;
        let participants: Vec<i64> = episode.participants.iter().map(|a| a.get()).collect();
        let row = sqlx::query(&format!(
            "WITH e AS (INSERT INTO episode (group_id, first_batch, last_batch, first_ordinal, last_ordinal, batch_lines, first_message, \
                last_message, started_ms, ended_ms, line_count, participants, title, summary, evidence, method, model, created_ms, findings) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19) RETURNING *) \
             SELECT {EPISODE_COLS} FROM e"
        ))
        .bind(episode.group.get())
        .bind(i64_of(episode.first_batch)?)
        .bind(i64_of(episode.last_batch)?)
        .bind(i64_of(episode.first_ordinal)?)
        .bind(i64_of(episode.last_ordinal)?)
        .bind(i32::try_from(episode.batch_lines).map_err(backend)?)
        .bind(episode.first_message.get())
        .bind(episode.last_message.get())
        .bind(episode.started.get())
        .bind(episode.ended.get())
        .bind(i32::try_from(episode.line_count).map_err(backend)?)
        .bind(participants)
        .bind(&episode.title)
        .bind(&episode.summary)
        .bind(evidence)
        .bind(&episode.method)
        .bind(&episode.model)
        .bind(self.clock.now().get())
        .bind(serde_json::to_value(&episode.findings).map_err(backend)?)
        .fetch_one(&mut *tx)
        .await
        .map_err(insert_error)?;
        let stored = to_episode(&row)?;
        sqlx::query("INSERT INTO episode_embedding (episode_id, model, dims, embedding) VALUES ($1, $2, $3, $4::vector)")
            .bind(stored.id.get())
            .bind(embed_model)
            .bind(i32::try_from(vector.len()).map_err(backend)?)
            .bind(vector_text(vector))
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(stored)
    }

    async fn unconsolidated(&self, group: GroupId) -> Result<Vec<Episode>, MemoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {EPISODE_COLS} FROM episode e WHERE e.group_id = $1 AND e.consolidated_ms IS NULL ORDER BY e.episode_id"
        ))
        .bind(group.get())
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(to_episode).collect()
    }

    async fn mark_consolidated(&self, id: EpisodeId) -> Result<(), MemoryError> {
        sqlx::query("UPDATE episode SET consolidated_ms = $2 WHERE episode_id = $1 AND consolidated_ms IS NULL")
            .bind(id.get())
            .bind(self.clock.now().get())
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn get(&self, group: GroupId, id: EpisodeId) -> Result<Option<Episode>, MemoryError> {
        let row = sqlx::query(&format!(
            "SELECT {EPISODE_COLS} FROM episode e WHERE e.group_id = $1 AND e.episode_id = $2"
        ))
        .bind(group.get())
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.as_ref().map(to_episode).transpose()
    }

    async fn within(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<Episode>, MemoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {EPISODE_COLS} FROM episode e WHERE e.group_id = $1 AND e.first_ordinal >= $2 AND e.last_ordinal <= $3 ORDER BY e.first_ordinal"
        ))
        .bind(group.get())
        .bind(i64_of(first_ordinal)?)
        .bind(i64_of(last_ordinal)?)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(to_episode).collect()
    }

    async fn ending_within(
        &self,
        group: GroupId,
        first_ordinal: u64,
        last_ordinal: u64,
    ) -> Result<Vec<Episode>, MemoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {EPISODE_COLS} FROM episode e WHERE e.group_id = $1 AND e.last_ordinal BETWEEN $2 AND $3 ORDER BY e.first_ordinal"
        ))
        .bind(group.get())
        .bind(i64_of(first_ordinal)?)
        .bind(i64_of(last_ordinal)?)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(to_episode).collect()
    }

    async fn search(
        &self,
        group: GroupId,
        embed_model: &str,
        query: &[f32],
        max_distance: f32,
    ) -> Result<Vec<Hit>, MemoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {EPISODE_COLS}, (m.embedding <=> $3::vector)::real AS distance \
             FROM episode_embedding m JOIN episode e ON e.episode_id = m.episode_id \
             WHERE e.group_id = $1 AND m.model = $2 AND m.dims = $4 AND (m.embedding <=> $3::vector) <= $5 \
             ORDER BY distance, e.episode_id"
        ))
        .bind(group.get())
        .bind(embed_model)
        .bind(vector_text(query))
        .bind(i32::try_from(query.len()).map_err(backend)?)
        .bind(f64::from(max_distance))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter()
            .map(|row| {
                Ok(Hit {
                    episode: to_episode(row)?,
                    distance: row.get("distance"),
                })
            })
            .collect()
    }
}
