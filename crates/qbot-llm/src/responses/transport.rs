//! HTTP boundary. Adapters talk to a [`Transport`]; production uses [`ReqwestTransport`] and
//! tests use a simulator, so the adapter's wire handling runs without a network.

use crate::net::{Route, client_builder};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::error::LlmError;

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, LlmError>> + Send>>;

pub enum HttpBody {
    Full(Bytes),
    Stream(ByteStream),
}

impl std::fmt::Debug for HttpBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpBody::Full(bytes) => write!(f, "Full({} bytes)", bytes.len()),
            HttpBody::Stream(_) => write!(f, "Stream"),
        }
    }
}

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub retry_after: Option<Duration>,
    /// Successful streaming requests yield `Stream`; everything else is `Full`.
    pub body: HttpBody,
}

/// A file sent as `multipart/form-data`, with plain form fields beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    pub file_name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    pub fields: Vec<(String, String)>,
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
        want_stream: bool,
    ) -> Result<HttpResponse, LlmError>;

    /// POST a file. Only adapters whose vendor takes uploads call this.
    async fn upload(&self, path: &str, upload: &Upload) -> Result<HttpResponse, LlmError> {
        let _ = (path, upload);
        Err(LlmError::Unsupported("file uploads"))
    }
}

/// Resolves an API key at call time, so a rotated key (a replaced mounted secret file, say) is
/// picked up without a restart. The configuration layer supplies the implementation.
pub trait KeyResolver: Send + Sync {
    fn resolve(&self) -> Result<String, LlmError>;
}

/// Where the API key comes from. Resolved on every call.
#[derive(Clone)]
pub enum KeySource {
    Static(String),
    Resolver(Arc<dyn KeyResolver>),
}

impl std::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeySource::Static(_) => f.write_str("KeySource::Static(..)"),
            KeySource::Resolver(_) => f.write_str("KeySource::Resolver(..)"),
        }
    }
}

impl KeySource {
    pub fn resolve(&self) -> Result<String, LlmError> {
        match self {
            KeySource::Static(key) if !key.trim().is_empty() => Ok(key.clone()),
            KeySource::Static(_) => Err(LlmError::Auth),
            KeySource::Resolver(resolver) => resolver.resolve(),
        }
    }
}

pub struct ReqwestTransport {
    client: reqwest::Client,
    base_url: String,
    key: KeySource,
}

impl std::fmt::Debug for ReqwestTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestTransport")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl ReqwestTransport {
    /// A transport to `base_url` on `route` (see [`crate::net`]). Only establishing the
    /// connection is bounded here; the caller's deadline bounds the rest.
    pub fn new(
        base_url: impl Into<String>,
        key: KeySource,
        route: &Route,
    ) -> Result<Self, LlmError> {
        let client = client_builder(route)?
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            key,
        })
    }
}

fn map_reqwest(error: &reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::Timeout
    } else {
        LlmError::Network(error.to_string())
    }
}

/// Retry-After, as whole seconds or as an HTTP-date (RFC 9110); a date in the past means now.
pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(
        at.duration_since(std::time::SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
        want_stream: bool,
    ) -> Result<HttpResponse, LlmError> {
        let key = self.key.resolve()?;
        let accept = if want_stream {
            "text/event-stream"
        } else {
            "application/json"
        };
        let response = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(key)
            .header("accept", accept)
            .json(body)
            .send()
            .await
            .map_err(|e| map_reqwest(&e))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        if want_stream && status.is_success() {
            let stream = response
                .bytes_stream()
                .map(|chunk| chunk.map_err(|e| map_reqwest(&e)));
            return Ok(HttpResponse {
                status: status.as_u16(),
                retry_after,
                body: HttpBody::Stream(Box::pin(stream)),
            });
        }
        let bytes = response.bytes().await.map_err(|e| map_reqwest(&e))?;
        Ok(HttpResponse {
            status: status.as_u16(),
            retry_after,
            body: HttpBody::Full(bytes),
        })
    }
    async fn upload(&self, path: &str, upload: &Upload) -> Result<HttpResponse, LlmError> {
        let key = self.key.resolve()?;
        let part = reqwest::multipart::Part::bytes(upload.bytes.clone())
            .file_name(upload.file_name.clone())
            .mime_str(&upload.mime)
            .map_err(|e| LlmError::InvalidRequest(e.to_string()))?;
        let mut form = reqwest::multipart::Form::new().part("file", part);
        for (name, value) in &upload.fields {
            form = form.text(name.clone(), value.clone());
        }
        let response = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| map_reqwest(&e))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        let bytes = response.bytes().await.map_err(|e| map_reqwest(&e))?;
        Ok(HttpResponse {
            status: status.as_u16(),
            retry_after,
            body: HttpBody::Full(bytes),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        assert_eq!(parse_retry_after(" 7 "), Some(Duration::from_secs(7)));
        // A date in the past means now; a future date means until then.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::ZERO)
        );
        let later = std::time::SystemTime::now() + Duration::from_secs(120);
        let waited = parse_retry_after(&httpdate::fmt_http_date(later)).unwrap();
        assert!(
            waited > Duration::from_secs(100) && waited <= Duration::from_secs(120),
            "{waited:?}"
        );
        assert_eq!(parse_retry_after("soon"), None);
    }

    #[test]
    fn empty_static_key_is_an_auth_error() {
        assert_eq!(
            KeySource::Static("  ".into()).resolve(),
            Err(LlmError::Auth)
        );
        assert_eq!(KeySource::Static("k".into()).resolve(), Ok("k".to_owned()));
    }
}
