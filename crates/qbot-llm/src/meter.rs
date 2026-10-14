//! Accounting of provider calls: one record per call an adapter makes, whatever the work.
//!
//! Every adapter method that reaches a provider (a model turn, an embedding, a web search, a
//! page read) runs through [`measured`], which writes exactly one [`ProviderCall`] to the
//! adapter's [`Meter`] when the call ends, also when it fails or is dropped half-way. What the
//! call consumed is collected on the way, each fact where it is known:
//! - every HTTP request, retries and uploads included, is counted by the shared retry helper;
//! - tokens and credits are added where a response body is parsed, once per response.
//!
//! So a retried call is one record with every request it made, and a response is counted by
//! the one place that read it. Which work a call serves (a reply run, extraction, a picture) is
//! not the adapter's business: the work declares it with [`attributed`] around its calls, and
//! the record carries it. Nothing reads these records to refuse work.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use qbot_core::{GroupId, RunId};
use tokio::time::Instant;

use crate::error::LlmError;
use crate::response::Usage;

/// The work a provider call serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Purpose {
    /// A reply run: its model turns and the tools it calls (recall, web search, page reading).
    Reply,
    /// Memory: summarizing chat into episodes and embedding them for recall.
    Extraction,
    /// Describing a picture or sticker posted in a group.
    Picture,
}

impl Purpose {
    pub const ALL: [Purpose; 3] = [Purpose::Reply, Purpose::Extraction, Purpose::Picture];

    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Reply => "reply",
            Purpose::Extraction => "extraction",
            Purpose::Picture => "picture",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == text)
    }
}

/// What kind of provider operation a call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CallKind {
    /// A model turn (text or vision): tokens in and out.
    Model,
    /// Embedding texts: input tokens.
    Embedding,
    /// A web search: plan credits.
    Search,
    /// Reading a web page: plan credits.
    Extract,
}

impl CallKind {
    pub const ALL: [CallKind; 4] = [
        CallKind::Model,
        CallKind::Embedding,
        CallKind::Search,
        CallKind::Extract,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            CallKind::Model => "model",
            CallKind::Embedding => "embedding",
            CallKind::Search => "search",
            CallKind::Extract => "extract",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == text)
    }
}

/// Who a call is for: the work, its group, and within a reply run the run and its turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attribution {
    pub purpose: Purpose,
    pub group: GroupId,
    pub run: Option<RunId>,
    pub turn: Option<u32>,
}

impl Attribution {
    /// Background work of a group (extraction, a picture).
    pub fn group(purpose: Purpose, group: GroupId) -> Self {
        Self {
            purpose,
            group,
            run: None,
            turn: None,
        }
    }
}

/// One provider call as it ended.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCall {
    /// `None` for a call made outside any [`attributed`] work: a wiring mistake, logged.
    pub attribution: Option<Attribution>,
    pub kind: CallKind,
    pub provider: String,
    /// The model, or for a search provider the depth it was asked for.
    pub model: String,
    /// `None` when the call succeeded; the error class (`LlmError::class`) when it failed,
    /// `"cancelled"` when it was dropped before it ended.
    pub error: Option<&'static str>,
    /// HTTP requests the call made: retries, batches and uploads each count.
    pub requests: u32,
    /// Tokens of every response read, `None` when none reported any. Cached input is typed:
    /// `CacheUsage::NotReported` where the provider does not say.
    pub tokens: Option<Usage>,
    /// Plan credits of every response read, `None` when none reported any.
    pub credits: Option<u32>,
    pub latency: Duration,
}

/// Where provider calls are recorded. Informational only: nothing reads the records to refuse
/// work.
pub trait Meter: Send + Sync {
    fn record(&self, call: ProviderCall);
}

/// What the call has consumed so far.
#[derive(Debug, Default)]
struct Ledger {
    requests: u32,
    tokens: Option<Usage>,
    credits: Option<u32>,
}

tokio::task_local! {
    static ATTRIBUTION: Attribution;
    static LEDGER: Arc<Mutex<Ledger>>;
}

/// Run `work` with its provider calls attributed to `attribution`. Calls in tasks it spawns
/// are not covered; such a task declares its own.
pub async fn attributed<F: Future>(attribution: Attribution, work: F) -> F::Output {
    ATTRIBUTION.scope(attribution, work).await
}

fn with_ledger(update: impl FnOnce(&mut Ledger)) {
    let _ = LEDGER
        .try_with(|ledger| update(&mut ledger.lock().unwrap_or_else(PoisonError::into_inner)));
}

/// Count one HTTP request of the call in progress.
pub(crate) fn count_request() {
    with_ledger(|l| l.requests = l.requests.saturating_add(1));
}

/// Add the tokens one response reported.
pub(crate) fn add_tokens(usage: Usage) {
    if usage.reported {
        with_ledger(|l| match &mut l.tokens {
            Some(total) => *total += usage,
            None => l.tokens = Some(usage),
        });
    }
}

/// Add the plan credits one response reported.
pub(crate) fn add_credits(credits: u32) {
    with_ledger(|l| l.credits = Some(l.credits.unwrap_or(0).saturating_add(credits)));
}

/// A call under way. Records it when dropped, so a call cut off by a deadline or a shutdown is
/// recorded too, with what it had consumed.
struct Pending {
    meter: Arc<dyn Meter>,
    attribution: Option<Attribution>,
    kind: CallKind,
    provider: String,
    model: String,
    ledger: Arc<Mutex<Ledger>>,
    started: Instant,
    error: Option<&'static str>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        let ledger =
            std::mem::take(&mut *self.ledger.lock().unwrap_or_else(PoisonError::into_inner));
        self.meter.record(ProviderCall {
            attribution: self.attribution,
            kind: self.kind,
            provider: std::mem::take(&mut self.provider),
            model: std::mem::take(&mut self.model),
            error: self.error,
            requests: ledger.requests,
            tokens: ledger.tokens,
            credits: ledger.credits,
            latency: self.started.elapsed(),
        });
    }
}

/// Run one provider call and record it once on `meter`. Without a meter the call just runs.
pub(crate) async fn measured<T>(
    meter: Option<&Arc<dyn Meter>>,
    kind: CallKind,
    provider: &str,
    model: &str,
    call: impl Future<Output = Result<T, LlmError>>,
) -> Result<T, LlmError> {
    let Some(meter) = meter else {
        return call.await;
    };
    let attribution = ATTRIBUTION.try_with(|a| *a).ok();
    if attribution.is_none() {
        tracing::warn!(
            kind = kind.as_str(),
            provider,
            "a provider call outside any attributed work"
        );
    }
    let ledger = Arc::new(Mutex::new(Ledger::default()));
    let mut pending = Pending {
        meter: Arc::clone(meter),
        attribution,
        kind,
        provider: provider.to_owned(),
        model: model.to_owned(),
        ledger: Arc::clone(&ledger),
        started: Instant::now(),
        error: Some("cancelled"),
    };
    let result = LEDGER.scope(ledger, call).await;
    pending.error = result.as_ref().err().map(LlmError::class);
    drop(pending);
    result
}

/// Records calls in memory, for tests.
#[derive(Debug, Default)]
pub struct MemoryMeter {
    calls: Mutex<Vec<ProviderCall>>,
}

impl MemoryMeter {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn calls(&self) -> Vec<ProviderCall> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Meter for MemoryMeter {
    fn record(&self, call: ProviderCall) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }
}
