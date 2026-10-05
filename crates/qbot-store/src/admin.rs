//! Read models for operator commands: who is blocked, the member roster, usage totals.

use std::sync::Arc;

use qbot_core::{AccountId, Clock, GroupId, UnixMillis};
use sqlx::{PgPool, Row};

use crate::error::StoreError;

#[derive(Clone)]
pub struct PgAdmin {
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for PgAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgAdmin").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRule {
    pub account: AccountId,
    pub until: Option<UnixMillis>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RosterRow {
    pub account: AccountId,
    pub number: u32,
    pub messages: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UsageTotals {
    pub runs: u64,
    pub model_calls: u64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub tool_calls: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopRow {
    pub account: AccountId,
    pub runs: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// What happened between two instants, for the daily report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReportData {
    pub runs: u64,
    pub runs_addressed: u64,
    pub runs_wake: u64,
    /// Runs by how they ended (`delivered`, `deadline`, `model_error`, ...), most common first.
    pub run_ends: Vec<(String, u64)>,
    /// Model error classes behind `model_error` runs, most common first.
    pub error_classes: Vec<(String, u64)>,
    pub model_calls: u64,
    pub failed_model_calls: u64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub tool_calls: u64,
    pub tool_calls_not_ok: u64,
    pub messages: u64,
    pub groups_active: u64,
    pub groups_new: u64,
    pub groups_muted: u64,
    pub episodes: u64,
    pub jobs_pending: u64,
    pub jobs_failed: u64,
    pub tasks_interrupted: u64,
}

fn count(row: &sqlx::postgres::PgRow, column: &str) -> u64 {
    u64::try_from(row.get::<i64, _>(column)).unwrap_or(0)
}

fn account(id: i64) -> Result<AccountId, StoreError> {
    AccountId::new(id).map_err(|e| StoreError::corrupt(e.to_string()))
}

impl PgAdmin {
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>) -> Self {
        Self { pool, clock }
    }

    pub async fn is_muted(&self, group: GroupId) -> Result<bool, StoreError> {
        let row = sqlx::query("SELECT muted FROM group_state WHERE group_id = $1")
            .bind(group.get())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some_and(|r| r.get::<bool, _>("muted")))
    }

    /// Blocks in force now, oldest first.
    pub async fn blocks(&self, group: GroupId) -> Result<Vec<BlockRule>, StoreError> {
        let rows = sqlx::query(
            "SELECT account_id, until_ms FROM group_block \
             WHERE group_id = $1 AND (until_ms IS NULL OR until_ms > $2) ORDER BY created_ms, account_id",
        )
        .bind(group.get())
        .bind(self.clock.now().get())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(BlockRule {
                    account: account(r.get("account_id"))?,
                    until: r.get::<Option<i64>, _>("until_ms").map(UnixMillis::new),
                })
            })
            .collect()
    }

    /// The group's member number for an account, if it has one.
    pub async fn member_number(
        &self,
        group: GroupId,
        who: AccountId,
    ) -> Result<Option<u32>, StoreError> {
        let row =
            sqlx::query("SELECT number FROM member_number WHERE group_id = $1 AND account_id = $2")
                .bind(group.get())
                .bind(who.get())
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|r| u32::try_from(r.get::<i32, _>("number")).ok()))
    }

    /// Messages the given accounts wrote in the group.
    pub async fn message_count(
        &self,
        group: GroupId,
        accounts: &[AccountId],
    ) -> Result<u64, StoreError> {
        let ids: Vec<i64> = accounts.iter().map(|a| a.get()).collect();
        let row = sqlx::query(
            "SELECT count(*)::bigint AS n FROM chat_line WHERE group_id = $1 AND account_id = ANY($2)",
        )
        .bind(group.get())
        .bind(&ids)
        .fetch_one(&self.pool)
        .await?;
        Ok(count(&row, "n"))
    }

    /// The most active members first, and how many members the group has in all.
    pub async fn roster(
        &self,
        group: GroupId,
        limit: i64,
    ) -> Result<(u64, Vec<RosterRow>), StoreError> {
        let total =
            sqlx::query("SELECT count(*)::bigint AS n FROM member_number WHERE group_id = $1")
                .bind(group.get())
                .fetch_one(&self.pool)
                .await?;
        let rows = sqlx::query(
            "SELECT m.account_id, m.number, count(l.seq)::bigint AS messages \
             FROM member_number m LEFT JOIN chat_line l \
               ON l.group_id = m.group_id AND l.account_id = m.account_id \
             WHERE m.group_id = $1 GROUP BY m.account_id, m.number \
             ORDER BY messages DESC, m.number LIMIT $2",
        )
        .bind(group.get())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let rows = rows
            .iter()
            .map(|r| {
                Ok(RosterRow {
                    account: account(r.get("account_id"))?,
                    number: u32::try_from(r.get::<i32, _>("number")).unwrap_or(0),
                    messages: count(r, "messages"),
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok((count(&total, "n"), rows))
    }

    /// Usage since `since`, for one group or (`None`) all of them.
    pub async fn usage(
        &self,
        group: Option<GroupId>,
        since: UnixMillis,
    ) -> Result<UsageTotals, StoreError> {
        let row = sqlx::query(
            "SELECT \
               count(*) FILTER (WHERE kind = 'run')::bigint AS runs, \
               count(*) FILTER (WHERE kind = 'model')::bigint AS model_calls, \
               coalesce(sum(input_tokens) FILTER (WHERE kind = 'model'), 0)::bigint AS input_tokens, \
               coalesce(sum(cached_tokens) FILTER (WHERE kind = 'model'), 0)::bigint AS cached_tokens, \
               coalesce(sum(output_tokens) FILTER (WHERE kind = 'model'), 0)::bigint AS output_tokens, \
               count(*) FILTER (WHERE kind = 'tool')::bigint AS tool_calls \
             FROM usage_event WHERE at_ms >= $1 AND ($2::bigint IS NULL OR group_id = $2)",
        )
        .bind(since.get())
        .bind(group.map(GroupId::get))
        .fetch_one(&self.pool)
        .await?;
        Ok(UsageTotals {
            runs: count(&row, "runs"),
            model_calls: count(&row, "model_calls"),
            input_tokens: count(&row, "input_tokens"),
            cached_tokens: count(&row, "cached_tokens"),
            output_tokens: count(&row, "output_tokens"),
            tool_calls: count(&row, "tool_calls"),
        })
    }

    /// Accounts whose messages started the most runs in the group since `since`.
    pub async fn top_requesters(
        &self,
        group: GroupId,
        since: UnixMillis,
        limit: i64,
    ) -> Result<Vec<TopRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT (trigger->>'sender')::bigint AS account_id, count(*)::bigint AS runs, \
                    coalesce(sum(input_tokens), 0)::bigint AS input_tokens, \
                    coalesce(sum(output_tokens), 0)::bigint AS output_tokens \
             FROM run WHERE group_id = $1 AND trigger_kind = 'addressed' AND started_ms >= $2 \
             GROUP BY 1 ORDER BY runs DESC, input_tokens DESC, account_id LIMIT $3",
        )
        .bind(group.get())
        .bind(since.get())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(TopRow {
                    account: account(r.get("account_id"))?,
                    runs: count(r, "runs"),
                    input_tokens: count(r, "input_tokens"),
                    output_tokens: count(r, "output_tokens"),
                })
            })
            .collect()
    }

    /// Every group the bot has seen.
    pub async fn groups(&self) -> Result<Vec<GroupId>, StoreError> {
        let rows = sqlx::query("SELECT group_id FROM group_state ORDER BY group_id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                GroupId::new(r.get("group_id")).map_err(|e| StoreError::corrupt(e.to_string()))
            })
            .collect()
    }

    /// Delete timers that are over (done, cancelled or interrupted) and were created before
    /// `before`. Returns how many.
    pub async fn delete_finished_timers(&self, before: UnixMillis) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "DELETE FROM timer WHERE state IN ('done', 'cancelled', 'interrupted') AND created_ms < $1",
        )
        .bind(before.get())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Delete finished runs (and their items) that started before `before`.
    pub async fn delete_runs(&self, before: UnixMillis) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM run WHERE ended_ms IS NOT NULL AND started_ms < $1")
            .bind(before.get())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Activity in `[from, to)`.
    pub async fn report(&self, from: UnixMillis, to: UnixMillis) -> Result<ReportData, StoreError> {
        let (from, to) = (from.get(), to.get());
        let pairs = |rows: Vec<sqlx::postgres::PgRow>| -> Vec<(String, u64)> {
            rows.iter()
                .map(|r| (r.get::<String, _>("label"), count(r, "n")))
                .collect()
        };
        let runs = sqlx::query(
            "SELECT count(*)::bigint AS runs, \
                    count(*) FILTER (WHERE trigger_kind = 'addressed')::bigint AS addressed, \
                    count(*) FILTER (WHERE trigger_kind = 'wake')::bigint AS wake, \
                    count(DISTINCT group_id)::bigint AS groups \
             FROM run WHERE started_ms >= $1 AND started_ms < $2",
        )
        .bind(from)
        .bind(to)
        .fetch_one(&self.pool)
        .await?;
        let run_ends = sqlx::query(
            "SELECT coalesce(end_reason, 'open') AS label, count(*)::bigint AS n FROM run \
             WHERE started_ms >= $1 AND started_ms < $2 GROUP BY 1 ORDER BY n DESC, label",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        let error_classes = sqlx::query(
            "SELECT error_class AS label, count(*)::bigint AS n FROM run \
             WHERE started_ms >= $1 AND started_ms < $2 AND error_class IS NOT NULL \
             GROUP BY 1 ORDER BY n DESC, label",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        let usage = sqlx::query(
            "SELECT count(*) FILTER (WHERE kind = 'model')::bigint AS model_calls, \
                    count(*) FILTER (WHERE kind = 'model' AND status <> 'ok')::bigint AS failed, \
                    coalesce(sum(input_tokens) FILTER (WHERE kind = 'model'), 0)::bigint AS input, \
                    coalesce(sum(cached_tokens) FILTER (WHERE kind = 'model'), 0)::bigint AS cached, \
                    coalesce(sum(output_tokens) FILTER (WHERE kind = 'model'), 0)::bigint AS output, \
                    count(*) FILTER (WHERE kind = 'tool')::bigint AS tools, \
                    count(*) FILTER (WHERE kind = 'tool' AND status <> 'ok')::bigint AS tools_not_ok \
             FROM usage_event WHERE at_ms >= $1 AND at_ms < $2",
        )
        .bind(from)
        .bind(to)
        .fetch_one(&self.pool)
        .await?;
        let messages = sqlx::query(
            "SELECT count(*)::bigint AS n FROM chat_line WHERE at_ms >= $1 AND at_ms < $2",
        )
        .bind(from)
        .bind(to)
        .fetch_one(&self.pool)
        .await?;
        let groups = sqlx::query(
            "SELECT count(*) FILTER (WHERE first_seen_ms >= $1 AND first_seen_ms < $2)::bigint AS fresh, \
                    count(*) FILTER (WHERE muted)::bigint AS muted FROM group_state",
        )
        .bind(from)
        .bind(to)
        .fetch_one(&self.pool)
        .await?;
        let episodes = sqlx::query(
            "SELECT count(*)::bigint AS n FROM episode WHERE created_ms >= $1 AND created_ms < $2",
        )
        .bind(from)
        .bind(to)
        .fetch_one(&self.pool)
        .await?;
        let jobs = sqlx::query(
            "SELECT count(*) FILTER (WHERE kind = 'job' AND state = 'pending')::bigint AS pending, \
                    count(*) FILTER (WHERE kind = 'job' AND state = 'done' AND outcome = 'job_failed' \
                                     AND created_ms >= $1 AND created_ms < $2)::bigint AS failed, \
                    count(*) FILTER (WHERE kind = 'wake' AND state = 'interrupted' \
                                     AND created_ms >= $1 AND created_ms < $2)::bigint AS interrupted \
             FROM timer",
        )
        .bind(from)
        .bind(to)
        .fetch_one(&self.pool)
        .await?;
        Ok(ReportData {
            runs: count(&runs, "runs"),
            runs_addressed: count(&runs, "addressed"),
            runs_wake: count(&runs, "wake"),
            run_ends: pairs(run_ends),
            error_classes: pairs(error_classes),
            model_calls: count(&usage, "model_calls"),
            failed_model_calls: count(&usage, "failed"),
            input_tokens: count(&usage, "input"),
            cached_tokens: count(&usage, "cached"),
            output_tokens: count(&usage, "output"),
            tool_calls: count(&usage, "tools"),
            tool_calls_not_ok: count(&usage, "tools_not_ok"),
            messages: count(&messages, "n"),
            groups_active: count(&runs, "groups"),
            groups_new: count(&groups, "fresh"),
            groups_muted: count(&groups, "muted"),
            episodes: count(&episodes, "n"),
            jobs_pending: count(&jobs, "pending"),
            jobs_failed: count(&jobs, "failed"),
            tasks_interrupted: count(&jobs, "interrupted"),
        })
    }
}
