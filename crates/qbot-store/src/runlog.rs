//! The durable run record: start, every item, and end. Items are append-only.

use std::sync::Arc;

use async_trait::async_trait;
use qbot_agent::{EnvError, RunLog, RunSummary, Trigger};
use qbot_context::{Item, Meta, RunEnd, Transcript};
use qbot_core::{Clock, GroupId, ItemSeq, RunId, UnixMillis};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use crate::error::StoreError;

#[derive(Clone)]
pub struct PgRunLog {
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for PgRunLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgRunLog").finish_non_exhaustive()
    }
}

/// What started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    Addressed,
    Wake,
}

impl TriggerKind {
    fn of(trigger: &Trigger) -> Self {
        match trigger {
            Trigger::Addressed { .. } => TriggerKind::Addressed,
            Trigger::Wake { .. } => TriggerKind::Wake,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TriggerKind::Addressed => "addressed",
            TriggerKind::Wake => "wake",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "addressed" => Some(TriggerKind::Addressed),
            "wake" => Some(TriggerKind::Wake),
            _ => None,
        }
    }
}

/// The row of one run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub run: RunId,
    pub group: GroupId,
    pub trigger_kind: TriggerKind,
    pub started: UnixMillis,
    pub ended: Option<UnixMillis>,
    pub end: Option<RunEnd>,
    pub error_class: Option<String>,
    pub input_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub turns: Option<i32>,
    pub tool_calls: Option<i32>,
    pub sends: Option<i32>,
}

fn trigger_json(trigger: &Trigger) -> Value {
    match trigger {
        Trigger::Addressed { message, sender } => {
            json!({ "message": message.get(), "sender": sender.get() })
        }
        Trigger::Wake {
            timer,
            intent,
            chain,
        } => {
            json!({ "timer": timer.get(), "intent": intent, "chain": chain.id.get(), "depth": chain.depth })
        }
    }
}

fn env(error: impl std::fmt::Display) -> EnvError {
    EnvError(error.to_string())
}

const RUN_COLS: &str = "run_id, group_id, trigger_kind, started_ms, ended_ms, end_reason, error_class, \
    input_tokens, cached_tokens, output_tokens, turns, tool_calls, sends";

fn to_record(row: &sqlx::postgres::PgRow) -> Result<RunRecord, StoreError> {
    let end: Option<String> = row.get("end_reason");
    Ok(RunRecord {
        run: RunId::new(
            u64::try_from(row.get::<i64, _>("run_id"))
                .map_err(|_| StoreError::corrupt("negative run id"))?,
        ),
        group: GroupId::new(row.get("group_id")).map_err(|e| StoreError::corrupt(e.to_string()))?,
        trigger_kind: {
            let kind: String = row.get("trigger_kind");
            TriggerKind::parse(&kind)
                .ok_or_else(|| StoreError::corrupt(format!("unknown trigger kind {kind:?}")))?
        },
        started: UnixMillis::new(row.get("started_ms")),
        ended: row.get::<Option<i64>, _>("ended_ms").map(UnixMillis::new),
        end: end
            .as_deref()
            .map(|e| {
                RunEnd::parse(e)
                    .ok_or_else(|| StoreError::corrupt(format!("unknown end reason {e:?}")))
            })
            .transpose()?,
        error_class: row.get("error_class"),
        input_tokens: row.get("input_tokens"),
        cached_tokens: row.get("cached_tokens"),
        output_tokens: row.get("output_tokens"),
        turns: row.get("turns"),
        tool_calls: row.get("tool_calls"),
        sends: row.get("sends"),
    })
}

impl PgRunLog {
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>) -> Self {
        Self { pool, clock }
    }

    pub async fn record(&self, run: RunId) -> Result<Option<RunRecord>, StoreError> {
        let row = sqlx::query(&format!("SELECT {RUN_COLS} FROM run WHERE run_id = $1"))
            .bind(i64::try_from(run.get()).map_err(|_| StoreError::corrupt("run id out of range"))?)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(to_record).transpose()
    }

    /// The group's latest runs, newest first.
    pub async fn recent(&self, group: GroupId, limit: i64) -> Result<Vec<RunRecord>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {RUN_COLS} FROM run WHERE group_id = $1 ORDER BY run_id DESC LIMIT $2"
        ))
        .bind(group.get())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(to_record).collect()
    }

    /// Every item of a run in order. The sequence numbers must be dense from zero.
    pub async fn load_items(&self, run: RunId) -> Result<Vec<Item>, StoreError> {
        load_items(&self.pool, run).await
    }

    /// The run's transcript, rebuilt through the transcript invariants. This is the exact record
    /// of what the model was given and did, for inspection and debugging.
    pub async fn load_transcript(&self, run: RunId) -> Result<Transcript, StoreError> {
        let items = self.load_items(run).await?;
        Transcript::from_items(items).map_err(|e| StoreError::corrupt(e.to_string()))
    }

    /// Close every run a previous process left open: unresolved tool calls are recorded as
    /// interrupted and the run ends as `interrupted`. Safe only while this process holds the
    /// exclusive runtime lease. Returns how many runs were closed.
    pub async fn recover_open_runs(&self) -> Result<usize, StoreError> {
        let open: Vec<i64> =
            sqlx::query("SELECT run_id FROM run WHERE ended_ms IS NULL ORDER BY run_id")
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|row| row.get("run_id"))
                .collect();
        for run_id in &open {
            let run =
                RunId::new(u64::try_from(*run_id).map_err(|_| StoreError::corrupt("run id"))?);
            let items = self.load_items(run).await?;
            let recorded_end = items.iter().find_map(|item| match item {
                Item::Meta(Meta::RunEnded(end)) => Some(*end),
                _ => None,
            });
            let mut end = recorded_end.unwrap_or(RunEnd::Interrupted);
            let mut new_items = Vec::new();
            if recorded_end.is_none() {
                match Transcript::from_items(items.clone()) {
                    Ok(mut transcript) => {
                        let before = transcript.len();
                        transcript.close_interrupted();
                        let _ = transcript.append(Item::Meta(Meta::RunEnded(RunEnd::Interrupted)));
                        new_items = transcript.items()[before..].to_vec();
                    }
                    Err(error) => {
                        // A log that fails its own invariants cannot be extended; close the row.
                        tracing::warn!(run = run_id, %error, "open run has an invalid log; closing it as is");
                        end = RunEnd::Interrupted;
                    }
                }
            }
            let mut tx = self.pool.begin().await?;
            for (offset, item) in new_items.iter().enumerate() {
                let seq = i32::try_from(items.len() + offset)
                    .map_err(|_| StoreError::corrupt("too many items"))?;
                insert_item(&mut tx, *run_id, seq, item).await?;
            }
            sqlx::query("UPDATE run SET ended_ms = $2, end_reason = $3 WHERE run_id = $1 AND ended_ms IS NULL")
                .bind(run_id)
                .bind(self.clock.now().get())
                .bind(end.as_str())
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        Ok(open.len())
    }
}

async fn load_items(pool: &PgPool, run: RunId) -> Result<Vec<Item>, StoreError> {
    let rows = sqlx::query("SELECT seq, payload FROM run_item WHERE run_id = $1 ORDER BY seq")
        .bind(i64::try_from(run.get()).map_err(|_| StoreError::corrupt("run id out of range"))?)
        .fetch_all(pool)
        .await?;
    let mut items = Vec::with_capacity(rows.len());
    for (expected, row) in rows.iter().enumerate() {
        let seq: i32 = row.get("seq");
        if usize::try_from(seq).ok() != Some(expected) {
            return Err(StoreError::corrupt(format!(
                "run {} has a gap in its items at {expected}",
                run.get()
            )));
        }
        let payload: Value = row.get("payload");
        items
            .push(serde_json::from_value(payload).map_err(|e| StoreError::corrupt(e.to_string()))?);
    }
    Ok(items)
}

async fn insert_item(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: i64,
    seq: i32,
    item: &Item,
) -> Result<(), StoreError> {
    let payload = serde_json::to_value(item).map_err(|e| StoreError::corrupt(e.to_string()))?;
    sqlx::query("INSERT INTO run_item (run_id, seq, kind, payload) VALUES ($1, $2, $3, $4)")
        .bind(run_id)
        .bind(seq)
        .bind(item.kind())
        .bind(payload)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[async_trait]
impl RunLog for PgRunLog {
    async fn begin(&self, group: GroupId, trigger: &Trigger) -> Result<RunId, EnvError> {
        let (kind, payload) = (TriggerKind::of(trigger).as_str(), trigger_json(trigger));
        let row = sqlx::query(
            "INSERT INTO run (group_id, trigger_kind, trigger, started_ms) VALUES ($1, $2, $3, $4) \
             RETURNING run_id",
        )
        .bind(group.get())
        .bind(kind)
        .bind(payload)
        .bind(self.clock.now().get())
        .fetch_one(&self.pool)
        .await
        .map_err(env)?;
        let id: i64 = row.get("run_id");
        Ok(RunId::new(u64::try_from(id).map_err(env)?))
    }

    async fn append(
        &self,
        _group: GroupId,
        run: RunId,
        seq: ItemSeq,
        item: &Item,
    ) -> Result<(), EnvError> {
        let payload = serde_json::to_value(item).map_err(env)?;
        sqlx::query("INSERT INTO run_item (run_id, seq, kind, payload) VALUES ($1, $2, $3, $4)")
            .bind(i64::try_from(run.get()).map_err(env)?)
            .bind(i32::try_from(seq.get()).map_err(env)?)
            .bind(item.kind())
            .bind(payload)
            .execute(&self.pool)
            .await
            .map_err(env)?;
        Ok(())
    }

    async fn finish(
        &self,
        group: GroupId,
        run: RunId,
        summary: &RunSummary,
    ) -> Result<(), EnvError> {
        let cached = summary.usage.cached_tokens().map(i64::from);
        let result = sqlx::query(
            "UPDATE run SET ended_ms = $3, end_reason = $4, error_class = $5, input_tokens = $6, \
                    cached_tokens = $7, output_tokens = $8, turns = $9, tool_calls = $10, sends = $11 \
             WHERE run_id = $1 AND group_id = $2 AND ended_ms IS NULL",
        )
        .bind(i64::try_from(run.get()).map_err(env)?)
        .bind(group.get())
        .bind(self.clock.now().get())
        .bind(summary.end.as_str())
        .bind(summary.error)
        .bind(i64::from(summary.usage.input_tokens))
        .bind(cached)
        .bind(i64::from(summary.usage.output_tokens))
        .bind(i32::try_from(summary.turns).map_err(env)?)
        .bind(i32::try_from(summary.tool_calls).map_err(env)?)
        .bind(i32::try_from(summary.sends).map_err(env)?)
        .execute(&self.pool)
        .await
        .map_err(env)?;
        if result.rows_affected() == 0 {
            return Err(EnvError(format!(
                "run {} is unknown or already finished",
                run.get()
            )));
        }
        Ok(())
    }
}
