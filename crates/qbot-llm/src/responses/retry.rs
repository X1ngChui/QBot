//! Bounded retries shared by every adapter that talks HTTP.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::LlmError;

use super::RetryPolicy;
use super::parse::http_error;
use super::transport::{HttpResponse, Transport, Upload};

/// POST with bounded retries for transient failures: connection errors and the HTTP statuses
/// that say to try again. Every request is counted for the call in progress ([`crate::meter`]).
pub async fn post_with_retry(
    transport: &dyn Transport,
    policy: &RetryPolicy,
    path: &str,
    body: &Value,
) -> Result<HttpResponse, LlmError> {
    with_retry(policy, || transport.post(path, body)).await
}

/// Upload a file with the same retry rules as [`post_with_retry`].
pub async fn upload_with_retry(
    transport: &dyn Transport,
    policy: &RetryPolicy,
    path: &str,
    upload: &Upload,
) -> Result<HttpResponse, LlmError> {
    with_retry(policy, || transport.upload(path, upload)).await
}

async fn with_retry<F, Fut>(policy: &RetryPolicy, mut send: F) -> Result<HttpResponse, LlmError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<HttpResponse, LlmError>>,
{
    let max_attempts = policy.retries.saturating_add(1);
    let mut attempt = 0;
    loop {
        attempt += 1;
        crate::meter::count_request();
        let sent = match policy.attempt_timeout {
            Some(limit) => tokio::time::timeout(limit, send())
                .await
                .unwrap_or(Err(LlmError::Timeout)),
            None => send().await,
        };
        let error = match sent {
            Ok(response) if (200..300).contains(&response.status) => {
                return Ok(response);
            }
            Ok(response) => http_error(response.status, &response.body, response.retry_after),
            Err(error) => error,
        };
        if error.is_transient() && attempt < max_attempts && !waits_too_long(&error) {
            tokio::time::sleep(backoff(policy, attempt, &error)).await;
            continue;
        }
        return Err(error);
    }
}

/// The longest a provider's Retry-After is waited out. A provider asking for longer is, for this
/// call, unavailable: waiting would hold a job's lease or a media permit for as long as it asks
/// (a day, say) on a call that has no deadline of its own, so it fails as rate-limited instead.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

fn waits_too_long(error: &LlmError) -> bool {
    matches!(error, LlmError::RateLimited { retry_after: Some(after) } if *after > MAX_RETRY_AFTER)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retry_after_beyond_the_cap_is_not_waited_out() {
        let asks = |secs| LlmError::RateLimited {
            retry_after: Some(Duration::from_secs(secs)),
        };
        assert!(!waits_too_long(&asks(30)));
        assert!(!waits_too_long(&asks(60)));
        assert!(waits_too_long(&asks(86_400)));
        assert!(!waits_too_long(&LlmError::RateLimited {
            retry_after: None
        }));
    }
}
