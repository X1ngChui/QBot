//! Usage accounting: informational rows written off the hot path.
//!
//! `UsageSink::record` is synchronous and cannot fail, so events go through a channel to a
//! writer task that inserts them in batches. A failed batch is logged and dropped: usage data
//! never blocks or fails a run.

use std::sync::{Arc, Mutex, PoisonError};

use qbot_agent::{UsageDetail, UsageEvent, UsageSink};
use qbot_context::Outcome;
use qbot_core::{Clock, UnixMillis};
use sqlx::{PgPool, Postgres, QueryBuilder};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const BATCH: usize = 256;

struct Row {
    at: UnixMillis,
    event: UsageEvent,
}

pub struct PgUsageSink {
    tx: Mutex<Option<mpsc::UnboundedSender<Row>>>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for PgUsageSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgUsageSink").finish_non_exhaustive()
    }
}

/// The sink plus its writer task. `shutdown` flushes everything recorded so far.
pub struct UsageRecorder {
    sink: Arc<PgUsageSink>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for UsageRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageRecorder").finish_non_exhaustive()
    }
}

impl PgUsageSink {
    pub fn start(pool: PgPool, clock: Arc<dyn Clock>) -> UsageRecorder {
        let (tx, mut rx) = mpsc::unbounded_channel::<Row>();
        let sink = Arc::new(PgUsageSink {
            tx: Mutex::new(Some(tx)),
            clock,
        });
        let task = tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                let mut batch = vec![first];
                while batch.len() < BATCH {
                    match rx.try_recv() {
                        Ok(row) => batch.push(row),
                        Err(_) => break,
                    }
                }
                if let Err(error) = insert(&pool, &batch).await {
                    tracing::warn!(%error, dropped = batch.len(), "usage events could not be stored");
                }
            }
        });
        UsageRecorder { sink, task }
    }
}

impl UsageRecorder {
    pub fn sink(&self) -> Arc<PgUsageSink> {
        Arc::clone(&self.sink)
    }

    /// Stop accepting events and wait until everything already recorded is written.
    pub async fn shutdown(self) {
        self.sink
            .tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let _ = self.task.await;
    }
}

impl UsageSink for PgUsageSink {
    fn record(&self, event: UsageEvent) {
        let at = self.clock.now();
        if let Some(tx) = self
            .tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = tx.send(Row { at, event });
        }
    }
}

fn outcome_label(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok => "ok".into(),
        Outcome::Error(kind) => format!("error:{kind:?}").to_lowercase(),
        Outcome::Refused(reason) => format!("refused:{reason:?}").to_lowercase(),
        Outcome::Interrupted => "interrupted".into(),
    }
}

async fn insert(pool: &PgPool, rows: &[Row]) -> Result<(), sqlx::Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "INSERT INTO usage_event (at_ms, group_id, run_id, turn, kind, provider, model, tool, status, \
         input_tokens, cached_tokens, output_tokens, reasoning_tokens, attempts, count, latency_ms) ",
    );
    builder.push_values(rows, |mut b, row| {
        let e = &row.event;
        let latency = i64::try_from(e.latency.as_millis()).unwrap_or(i64::MAX);
        let (kind, provider, model, tool, status, usage, attempts, count) = match &e.detail {
            UsageDetail::Model {
                provider,
                model,
                usage,
                attempts,
                error,
            } => (
                "model",
                Some(provider.clone()),
                Some(model.clone()),
                None,
                (*error).unwrap_or("ok").to_owned(),
                *usage,
                Some(i32::try_from(*attempts).unwrap_or(i32::MAX)),
                None,
            ),
            UsageDetail::Tool { name, outcome } => (
                "tool",
                None,
                None,
                Some(name.clone()),
                outcome_label(outcome),
                None,
                None,
                None,
            ),
            UsageDetail::RunEnded {
                end,
                usage,
                tool_calls,
                ..
            } => (
                "run",
                None,
                None,
                None,
                end.as_str().to_owned(),
                Some(*usage),
                None,
                Some(i32::try_from(*tool_calls).unwrap_or(i32::MAX)),
            ),
        };
        b.push_bind(row.at.get())
            .push_bind(e.group.get())
            .push_bind(i64::try_from(e.run.get()).unwrap_or(i64::MAX))
            .push_bind(i32::try_from(e.turn).unwrap_or(i32::MAX))
            .push_bind(kind)
            .push_bind(provider)
            .push_bind(model)
            .push_bind(tool)
            .push_bind(status)
            .push_bind(usage.map(|u| i64::from(u.input_tokens)))
            .push_bind(usage.and_then(|u| u.cached_tokens()).map(i64::from))
            .push_bind(usage.map(|u| i64::from(u.output_tokens)))
            .push_bind(usage.and_then(|u| u.reasoning_tokens).map(i64::from))
            .push_bind(attempts)
            .push_bind(count)
            .push_bind(latency);
    });
    builder.build().execute(pool).await?;
    Ok(())
}
