//! Timers on Postgres. Claims use `FOR UPDATE SKIP LOCKED`; the per-group pending bound is
//! checked under an advisory lock in the same transaction as the insert.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::Chain;
use qbot_context::RunEnd;
use qbot_core::{ChainId, Clock, GroupId, TimerId, UnixMillis};
use qbot_sched::{
    JobKind, NewWake, Origin, Recovery, StoreError as TimerStoreError, Timer, TimerKind,
    TimerOutcome, TimerState, TimerStore, WakeEdit, WakeSpec,
};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use crate::error::StoreError;

const COLS: &str = "id, kind, due_ms, state, attempts, lease_until_ms, outcome, outcome_detail, \
                    group_id, intent, chain_id, chain_depth, origin, job_kind";

#[derive(Clone)]
pub struct PgTimerStore {
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for PgTimerStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgTimerStore").finish_non_exhaustive()
    }
}

fn backend(error: impl std::fmt::Display) -> TimerStoreError {
    TimerStoreError::Backend(error.to_string())
}

fn job_kind_text(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Extract => "extract",
        JobKind::Nightly => "nightly",
        JobKind::Decay => "decay",
        JobKind::Backup => "backup",
        JobKind::Report => "report",
        JobKind::Cleanup => "cleanup",
    }
}

fn job_kind_parse(text: &str) -> Result<JobKind, StoreError> {
    Ok(match text {
        "extract" => JobKind::Extract,
        "nightly" => JobKind::Nightly,
        "decay" => JobKind::Decay,
        "backup" => JobKind::Backup,
        "report" => JobKind::Report,
        "cleanup" => JobKind::Cleanup,
        other => return Err(StoreError::corrupt(format!("unknown job kind {other:?}"))),
    })
}

fn outcome_columns(outcome: &TimerOutcome) -> (&'static str, Option<String>) {
    match outcome {
        TimerOutcome::Ran(end) => ("ran", Some(end.as_str().to_owned())),
        TimerOutcome::SkippedMuted => ("skipped_muted", None),
        TimerOutcome::JobOk => ("job_ok", None),
        TimerOutcome::JobFailed(message) => ("job_failed", Some(message.clone())),
    }
}

fn outcome_parse(kind: &str, detail: Option<String>) -> Result<TimerOutcome, StoreError> {
    Ok(match (kind, detail) {
        ("ran", Some(end)) => TimerOutcome::Ran(
            RunEnd::parse(&end)
                .ok_or_else(|| StoreError::corrupt(format!("unknown run end {end:?}")))?,
        ),
        ("skipped_muted", _) => TimerOutcome::SkippedMuted,
        ("job_ok", _) => TimerOutcome::JobOk,
        ("job_failed", detail) => TimerOutcome::JobFailed(detail.unwrap_or_default()),
        (other, _) => return Err(StoreError::corrupt(format!("unknown outcome {other:?}"))),
    })
}

fn to_timer(row: &PgRow) -> Result<Timer, StoreError> {
    let id = TimerId::new(
        u64::try_from(row.get::<i64, _>("id")).map_err(|_| StoreError::corrupt("timer id"))?,
    );
    let state = match row.get::<String, _>("state").as_str() {
        "pending" => TimerState::Pending,
        "claimed" => TimerState::Claimed {
            lease_until: row
                .get::<Option<i64>, _>("lease_until_ms")
                .map(UnixMillis::new),
        },
        "done" => {
            let kind: Option<String> = row.get("outcome");
            let kind = kind.ok_or_else(|| StoreError::corrupt("done timer without an outcome"))?;
            TimerState::Done(outcome_parse(&kind, row.get("outcome_detail"))?)
        }
        "cancelled" => TimerState::Cancelled,
        "interrupted" => TimerState::Interrupted,
        other => {
            return Err(StoreError::corrupt(format!(
                "unknown timer state {other:?}"
            )));
        }
    };
    let kind = match row.get::<String, _>("kind").as_str() {
        "wake" => {
            let origin = match row.get::<Option<String>, _>("origin").as_deref() {
                Some("model") => Origin::Model,
                Some("owner") => Origin::Owner,
                other => return Err(StoreError::corrupt(format!("unknown origin {other:?}"))),
            };
            let group = GroupId::new(row.get::<Option<i64>, _>("group_id").unwrap_or(0))
                .map_err(|e| StoreError::corrupt(e.to_string()))?;
            let chain_id = row.get::<Option<i64>, _>("chain_id").unwrap_or(0);
            let depth = row.get::<Option<i32>, _>("chain_depth").unwrap_or(0);
            TimerKind::Wake(WakeSpec {
                group,
                intent: row.get::<Option<String>, _>("intent").unwrap_or_default(),
                chain: Chain {
                    id: ChainId::new(
                        u64::try_from(chain_id).map_err(|_| StoreError::corrupt("chain id"))?,
                    ),
                    depth: u32::try_from(depth).map_err(|_| StoreError::corrupt("chain depth"))?,
                },
                origin,
            })
        }
        "job" => {
            let kind =
                job_kind_parse(&row.get::<Option<String>, _>("job_kind").unwrap_or_default())?;
            let group = row
                .get::<Option<i64>, _>("group_id")
                .map(|g| GroupId::new(g).map_err(|e| StoreError::corrupt(e.to_string())))
                .transpose()?;
            TimerKind::Job { kind, group }
        }
        other => return Err(StoreError::corrupt(format!("unknown timer kind {other:?}"))),
    };
    Ok(Timer {
        id,
        due_at: UnixMillis::new(row.get("due_ms")),
        kind,
        state,
        attempts: u32::try_from(row.get::<i32, _>("attempts"))
            .map_err(|_| StoreError::corrupt("attempts"))?,
    })
}

fn timer(row: &PgRow) -> Result<Timer, TimerStoreError> {
    to_timer(row).map_err(backend)
}

fn id_i64(id: TimerId) -> Result<i64, TimerStoreError> {
    i64::try_from(id.get()).map_err(backend)
}

impl PgTimerStore {
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>) -> Self {
        Self { pool, clock }
    }

    /// Why an update that matched nothing did not apply: unknown (or foreign), or no longer in
    /// the required state.
    async fn explain_miss(&self, id: TimerId, group: Option<GroupId>) -> TimerStoreError {
        let row = sqlx::query(&format!("SELECT {COLS} FROM timer WHERE id = $1"))
            .bind(match id_i64(id) {
                Ok(id) => id,
                Err(error) => return error,
            })
            .fetch_optional(&self.pool)
            .await;
        match row {
            Err(error) => backend(error),
            Ok(None) => TimerStoreError::NotFound,
            Ok(Some(row)) => match timer(&row) {
                Err(error) => error,
                Ok(found) => {
                    let foreign = group.is_some_and(|g| found.kind.group() != Some(g))
                        || (group.is_some() && found.wake().is_none());
                    if foreign {
                        TimerStoreError::NotFound
                    } else {
                        TimerStoreError::NotPending(found.state)
                    }
                }
            },
        }
    }
}

#[async_trait]
impl TimerStore for PgTimerStore {
    async fn insert_wake(
        &self,
        due_at: UnixMillis,
        new: NewWake,
        max_pending: usize,
    ) -> Result<Timer, TimerStoreError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('timer:' || $1::text, 0))")
            .bind(new.group.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let pending: i64 = sqlx::query(
            "SELECT count(*) AS n FROM timer WHERE kind = 'wake' AND group_id = $1 AND state = 'pending'",
        )
        .bind(new.group.get())
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?
        .get("n");
        if usize::try_from(pending).unwrap_or(usize::MAX) >= max_pending {
            return Err(TimerStoreError::TooManyPending { limit: max_pending });
        }
        let sql = format!(
            "WITH n AS (SELECT nextval(pg_get_serial_sequence('timer', 'id')) AS id) \
             INSERT INTO timer (id, kind, due_ms, state, created_ms, group_id, intent, chain_id, chain_depth, origin) \
             SELECT n.id, 'wake', $1, 'pending', $2, $3, $4, COALESCE($5::bigint, n.id), $6, $7 FROM n \
             RETURNING {COLS}"
        );
        let (chain_id, depth) = match new.chain {
            Some(chain) => (
                Some(i64::try_from(chain.id.get()).map_err(backend)?),
                i32::try_from(chain.depth).map_err(backend)?,
            ),
            None => (None, 0),
        };
        let row = sqlx::query(&sql)
            .bind(due_at.get())
            .bind(self.clock.now().get())
            .bind(new.group.get())
            .bind(&new.intent)
            .bind(chain_id)
            .bind(depth)
            .bind(match new.origin {
                Origin::Model => "model",
                Origin::Owner => "owner",
            })
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        timer(&row)
    }

    async fn insert_job(
        &self,
        due_at: UnixMillis,
        kind: JobKind,
        group: Option<GroupId>,
    ) -> Result<Timer, TimerStoreError> {
        let sql = format!(
            "INSERT INTO timer (kind, due_ms, state, created_ms, group_id, job_kind) \
             VALUES ('job', $1, 'pending', $2, $3, $4) RETURNING {COLS}"
        );
        let row = sqlx::query(&sql)
            .bind(due_at.get())
            .bind(self.clock.now().get())
            .bind(group.map(GroupId::get))
            .bind(job_kind_text(kind))
            .fetch_one(&self.pool)
            .await
            .map_err(backend)?;
        timer(&row)
    }

    async fn get(&self, id: TimerId) -> Result<Option<Timer>, TimerStoreError> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM timer WHERE id = $1"))
            .bind(id_i64(id)?)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(timer).transpose()
    }

    async fn list_active_wakes(
        &self,
        group: GroupId,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Timer>, TimerStoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM timer WHERE kind = 'wake' AND group_id = $1 AND state IN ('pending', 'claimed') \
             ORDER BY due_ms, id OFFSET $2 LIMIT $3"
        ))
        .bind(group.get())
        .bind(i64::try_from(offset).map_err(backend)?)
        .bind(i64::try_from(limit).map_err(backend)?)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(timer).collect()
    }

    async fn edit_pending_wake(
        &self,
        group: GroupId,
        id: TimerId,
        edit: WakeEdit,
    ) -> Result<Timer, TimerStoreError> {
        let row = sqlx::query(&format!(
            "UPDATE timer SET intent = COALESCE($3, intent), due_ms = COALESCE($4, due_ms) \
             WHERE id = $1 AND group_id = $2 AND kind = 'wake' AND state = 'pending' RETURNING {COLS}"
        ))
        .bind(id_i64(id)?)
        .bind(group.get())
        .bind(edit.intent)
        .bind(edit.due_at.map(UnixMillis::get))
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match row {
            Some(row) => timer(&row),
            None => Err(self.explain_miss(id, Some(group)).await),
        }
    }

    async fn cancel_pending_wake(
        &self,
        group: GroupId,
        id: TimerId,
    ) -> Result<Timer, TimerStoreError> {
        let row = sqlx::query(&format!(
            "UPDATE timer SET state = 'cancelled' \
             WHERE id = $1 AND group_id = $2 AND kind = 'wake' AND state = 'pending' RETURNING {COLS}"
        ))
        .bind(id_i64(id)?)
        .bind(group.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match row {
            Some(row) => timer(&row),
            None => Err(self.explain_miss(id, Some(group)).await),
        }
    }

    async fn claim_due_wake(
        &self,
        now: UnixMillis,
        exclude: &HashSet<GroupId>,
    ) -> Result<Option<Timer>, TimerStoreError> {
        let excluded: Vec<i64> = exclude.iter().map(|g| g.get()).collect();
        let row = sqlx::query(&format!(
            "UPDATE timer SET state = 'claimed', attempts = attempts + 1 WHERE id = ( \
                SELECT id FROM timer WHERE kind = 'wake' AND state = 'pending' AND due_ms <= $1 \
                  AND NOT (group_id = ANY($2)) \
                ORDER BY due_ms, id FOR UPDATE SKIP LOCKED LIMIT 1) \
             RETURNING {COLS}"
        ))
        .bind(now.get())
        .bind(excluded)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.as_ref().map(timer).transpose()
    }

    async fn claim_due_job(
        &self,
        now: UnixMillis,
        lease: Duration,
    ) -> Result<Option<Timer>, TimerStoreError> {
        let until = now.plus(lease);
        let row = sqlx::query(&format!(
            "UPDATE timer SET state = 'claimed', attempts = attempts + 1, lease_until_ms = $2 WHERE id = ( \
                SELECT id FROM timer WHERE kind = 'job' AND \
                  ((state = 'pending' AND due_ms <= $1) OR (state = 'claimed' AND lease_until_ms <= $1)) \
                ORDER BY due_ms, id FOR UPDATE SKIP LOCKED LIMIT 1) \
             RETURNING {COLS}"
        ))
        .bind(now.get())
        .bind(until.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.as_ref().map(timer).transpose()
    }

    async fn finish(&self, id: TimerId, outcome: TimerOutcome) -> Result<(), TimerStoreError> {
        let (kind, detail) = outcome_columns(&outcome);
        let result = sqlx::query(
            "UPDATE timer SET state = 'done', outcome = $2, outcome_detail = $3, lease_until_ms = NULL \
             WHERE id = $1 AND state = 'claimed'",
        )
        .bind(id_i64(id)?)
        .bind(kind)
        .bind(detail)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        if result.rows_affected() == 0 {
            return Err(self.explain_miss(id, None).await);
        }
        Ok(())
    }

    async fn retry_job(&self, id: TimerId, at: UnixMillis) -> Result<(), TimerStoreError> {
        let result = sqlx::query(
            "UPDATE timer SET state = 'pending', due_ms = $2, lease_until_ms = NULL \
             WHERE id = $1 AND kind = 'job' AND state = 'claimed'",
        )
        .bind(id_i64(id)?)
        .bind(at.get())
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        if result.rows_affected() == 0 {
            return Err(self.explain_miss(id, None).await);
        }
        Ok(())
    }

    async fn extend_lease(&self, id: TimerId, until: UnixMillis) -> Result<(), TimerStoreError> {
        let result = sqlx::query(
            "UPDATE timer SET lease_until_ms = $2 WHERE id = $1 AND kind = 'job' AND state = 'claimed'",
        )
        .bind(id_i64(id)?)
        .bind(until.get())
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        if result.rows_affected() == 0 {
            return Err(self.explain_miss(id, None).await);
        }
        Ok(())
    }

    async fn fire_recurrence(
        &self,
        name: &str,
        occurrence: UnixMillis,
        kind: JobKind,
    ) -> Result<bool, TimerStoreError> {
        let result = sqlx::query(
            "WITH advanced AS ( \
                INSERT INTO recurrence (name, last_fired_ms) VALUES ($1, $2) \
                ON CONFLICT (name) DO UPDATE SET last_fired_ms = EXCLUDED.last_fired_ms \
                  WHERE recurrence.last_fired_ms < EXCLUDED.last_fired_ms \
                RETURNING 1) \
             INSERT INTO timer (kind, due_ms, state, created_ms, job_kind) \
             SELECT 'job', $2, 'pending', $3, $4 FROM advanced",
        )
        .bind(name)
        .bind(occurrence.get())
        .bind(self.clock.now().get())
        .bind(job_kind_text(kind))
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(result.rows_affected() > 0)
    }

    async fn seed_recurrence(
        &self,
        name: &str,
        occurrence: UnixMillis,
    ) -> Result<(), TimerStoreError> {
        sqlx::query("INSERT INTO recurrence (name, last_fired_ms) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING")
            .bind(name)
            .bind(occurrence.get())
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn next_due(&self) -> Result<Option<UnixMillis>, TimerStoreError> {
        let row = sqlx::query(
            "SELECT min(t) AS next FROM ( \
                SELECT due_ms AS t FROM timer WHERE state = 'pending' \
                UNION ALL \
                SELECT lease_until_ms FROM timer WHERE kind = 'job' AND state = 'claimed') x",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(backend)?;
        Ok(row.get::<Option<i64>, _>("next").map(UnixMillis::new))
    }

    async fn recover(&self) -> Result<Recovery, TimerStoreError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let interrupted = sqlx::query(
            "UPDATE timer SET state = 'interrupted' WHERE kind = 'wake' AND state = 'claimed'",
        )
        .execute(&mut *tx)
        .await
        .map_err(backend)?
        .rows_affected();
        let requeued = sqlx::query(
            "UPDATE timer SET state = 'pending', lease_until_ms = NULL WHERE kind = 'job' AND state = 'claimed'",
        )
        .execute(&mut *tx)
        .await
        .map_err(backend)?
        .rows_affected();
        tx.commit().await.map_err(backend)?;
        Ok(Recovery {
            interrupted: usize::try_from(interrupted).unwrap_or(usize::MAX),
            requeued: usize::try_from(requeued).unwrap_or(usize::MAX),
        })
    }
}
