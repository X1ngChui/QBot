//! Bounded retries shared by every adapter that talks HTTP.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::LlmError;

use super::RetryPolicy;
use super::parse::http_error;
use super::transport::{HttpBody, HttpResponse, Transport, Upload};

/// POST with bounded retries for transient failures that happen before any body arrives.
/// Returns the successful response and the number of attempts it took.
pub async fn post_with_retry(
    transport: &dyn Transport,
    policy: &RetryPolicy,
    path: &str,
    body: &Value,
    want_stream: bool,
) -> Result<(HttpResponse, u32), LlmError> {
    with_retry(policy, || transport.post(path, body, want_stream)).await
}

/// Upload a file with the same retry rules as [`post_with_retry`].
pub async fn upload_with_retry(
    transport: &dyn Transport,
    policy: &RetryPolicy,
    path: &str,
    upload: &Upload,
) -> Result<(HttpResponse, u32), LlmError> {
    with_retry(policy, || transport.upload(path, upload)).await
}

async fn with_retry<F, Fut>(
    policy: &RetryPolicy,
    mut send: F,
) -> Result<(HttpResponse, u32), LlmError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<HttpResponse, LlmError>>,
{
    let max_attempts = policy.retries.saturating_add(1);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let error = match send().await {
            Ok(response) if (200..300).contains(&response.status) => {
                return Ok((response, attempt));
            }
            Ok(response) => {
                let bytes = match response.body {
                    HttpBody::Full(bytes) => bytes,
                    HttpBody::Stream(_) => bytes::Bytes::new(),
                };
                http_error(response.status, &bytes, response.retry_after)
            }
            Err(error) => error,
        };
        if error.is_transient() && attempt < max_attempts {
            tokio::time::sleep(backoff(policy, attempt, &error)).await;
            continue;
        }
        return Err(error);
    }
}

fn backoff(policy: &RetryPolicy, attempt: u32, error: &LlmError) -> Duration {
    let exponent = attempt.saturating_sub(1).min(16);
    let mut delay = policy.base.saturating_mul(1u32 << exponent);
    if let LlmError::RateLimited {
        retry_after: Some(after),
    } = error
    {
        delay = delay.max(*after);
    }
    if policy.jitter {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let fraction = f64::from(nanos % 1000) / 1000.0 * 0.25;
        delay += delay.mul_f64(fraction);
    }
    delay
}
