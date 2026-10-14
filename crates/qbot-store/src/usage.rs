//! Usage accounting: informational rows written off the hot path.
//!
//! One table and one writer for everything measured: a run's end and its tool calls
//! ([`UsageSink`]) and every provider call of any work ([`Meter`]). Recording is synchronous and
//! cannot fail, so events go through a channel to a writer task that inserts them in batches. A
//! failed batch is logged and dropped: usage data never blocks or fails any work.

use std::sync::{Arc, Mutex, PoisonError};

use qbot_agent::{UsageDetail, UsageEvent, UsageSink};
use qbot_context::Outcome;
use qbot_core::{Clock, UnixMillis};
use qbot_llm::{Meter, ProviderCall};
use sqlx::{PgPool, Postgres, QueryBuilder};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const BATCH: usize = 256;

enum Event {
    Run(UsageEvent),
    Provider(ProviderCall),
}

struct Row {
    at: UnixMillis,
    event: Event,
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

impl PgUsageSink {
    fn send(&self, event: Event) {
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

impl UsageSink for PgUsageSink {
    fn record(&self, event: UsageEvent) {
        self.send(Event::Run(event));
    }
}

impl Meter for PgUsageSink {
    fn record(&self, call: ProviderCall) {
        self.send(Event::Provider(call));
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

/// One row's columns, in the order of the insert.
struct Columns {
    group: Option<i64>,
    run: Option<i64>,
    turn: Option<i32>,
    kind: &'static str,
    purpose: Option<&'static str>,
    provider: Option<String>,
    model: Option<String>,
    tool: Option<String>,
    status: String,
    usage: Option<qbot_llm::Usage>,
    requests: Option<i32>,
    count: Option<i32>,
    credits: Option<i64>,
    latency_ms: i64,
}

fn small(n: u32) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn columns(event: &Event) -> Columns {
    let millis = |d: std::time::Duration| i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
    match event {
        Event::Run(e) => {
            let (kind, tool, status, usage, count) = match &e.detail {
                UsageDetail::Tool { name, outcome } => (
                    "tool",
                    Some(name.clone()),
                    outcome_label(outcome),
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
                    end.as_str().to_owned(),
                    Some(*usage),
                    Some(small(*tool_calls)),
                ),
            };
            Columns {
                group: Some(e.group.get()),
                run: Some(i64::try_from(e.run.get()).unwrap_or(i64::MAX)),
                turn: Some(small(e.turn)),
                kind,
                purpose: None,
                provider: None,
                model: None,
                tool,
                status,
                usage,
                requests: None,
                count,
                credits: None,
                latency_ms: millis(e.latency),
            }
        }
        Event::Provider(call) => {
            let by = call.attribution;
            Columns {
                group: by.map(|a| a.group.get()),
                run: by
                    .and_then(|a| a.run)
                    .map(|r| i64::try_from(r.get()).unwrap_or(i64::MAX)),
                turn: by.and_then(|a| a.turn).map(small),
                kind: call.kind.as_str(),
                purpose: by.map(|a| a.purpose.as_str()),
                provider: Some(call.provider.clone()),
                model: Some(call.model.clone()),
                tool: None,
                status: call.error.unwrap_or("ok").to_owned(),
                usage: call.tokens,
                requests: Some(small(call.requests)),
                count: None,
                credits: call.credits.map(i64::from),
                latency_ms: millis(call.latency),
            }
        }
    }
}

async fn insert(pool: &PgPool, rows: &[Row]) -> Result<(), sqlx::Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "INSERT INTO usage_event (at_ms, group_id, run_id, turn, kind, purpose, provider, model, \
         tool, status, input_tokens, cached_tokens, output_tokens, reasoning_tokens, requests, \
         count, credits, latency_ms) ",
    );
    builder.push_values(rows, |mut b, row| {
        let c = columns(&row.event);
        let usage = c.usage;
        b.push_bind(row.at.get())
            .push_bind(c.group)
            .push_bind(c.run)
            .push_bind(c.turn)
            .push_bind(c.kind)
            .push_bind(c.purpose)
            .push_bind(c.provider)
            .push_bind(c.model)
            .push_bind(c.tool)
            .push_bind(c.status)
            .push_bind(usage.map(|u| i64::from(u.input_tokens)))
            .push_bind(usage.and_then(|u| u.cached_tokens()).map(i64::from))
            .push_bind(usage.map(|u| i64::from(u.output_tokens)))
            .push_bind(usage.and_then(|u| u.reasoning_tokens).map(i64::from))
            .push_bind(c.requests)
            .push_bind(c.count)
            .push_bind(c.credits)
            .push_bind(c.latency_ms);
    });
    builder.build().execute(pool).await?;
    Ok(())
}
