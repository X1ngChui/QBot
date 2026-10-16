//! A request that stalls (the provider accepts the connection and sends nothing) is cut off by
//! the attempt timeout and tried again, instead of using up the whole call deadline.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use qbot_llm::responses::sim::{SimServer, SimStep};
use qbot_llm::responses::{HttpResponse, ResponsesConfig, ResponsesProvider, Transport};
use qbot_llm::{LlmError, MemoryMeter, Provider};
use serde_json::Value;

/// Hangs on its first request, then answers from the simulator.
struct HangOnce {
    hung: AtomicBool,
    server: SimServer,
}

#[async_trait]
impl Transport for HangOnce {
    async fn post(&self, path: &str, body: &Value) -> Result<HttpResponse, LlmError> {
        if !self.hung.swap(true, Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        self.server.post(path, body).await
    }
}

#[tokio::test(start_paused = true)]
async fn a_stalled_request_is_cut_off_and_tried_again_within_the_call_deadline() {
    let mut cfg = ResponsesConfig::deepseek("m");
    cfg.timeout = Some(Duration::from_secs(120));
    cfg.retry = fast_retry();
    cfg.retry.attempt_timeout = Some(Duration::from_secs(40));
    let transport = Arc::new(HangOnce {
        hung: AtomicBool::new(false),
        server: SimServer::new(vec![SimStep::Reply(first_reply())]),
    });
    let meter = MemoryMeter::new();
    let provider = ResponsesProvider::new(cfg, transport)
        .unwrap()
        .with_meter(meter.clone());
    let (tools, conv) = (tools(), base_conversation());
    let started = tokio::time::Instant::now();
    let result = provider.respond(request(&conv, &tools, None)).await;
    assert!(result.is_ok(), "{:?}", result.err());
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(40) && waited < Duration::from_secs(45),
        "one attempt timeout, then the retry: {waited:?}"
    );
    assert_eq!(
        meter.calls()[0].requests,
        2,
        "the stalled request counts too"
    );
}
