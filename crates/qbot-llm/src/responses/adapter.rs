use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::capability::{Capabilities, ForcedToolChoice, ProviderId, ProviderInfo, ReasoningInfo};
use crate::error::LlmError;
use crate::meter::{self, CallKind, Meter};
use crate::provider::Provider;
use crate::request::{Request, validate_request};
use crate::response::{Response, ResponseMeta};

use super::ResponsesConfig;
use super::parse::{parse_response, parse_usage};
use super::retry::{post_with_retry, upload_with_retry};
use super::transport::{HttpResponse, Transport, Upload};
use super::wire::{build_body, image_keys};

pub struct ResponsesProvider {
    info: ProviderInfo,
    cfg: ResponsesConfig,
    transport: Arc<dyn Transport>,
    /// SHA-256 of an image's bytes to its uploaded file id, until shortly before the
    /// file expires. Keyed by content: a request's image keys name pictures only within its own
    /// media store, while this outlives every request.
    uploads: moka::future::Cache<String, String>,
    meter: Option<Arc<dyn Meter>>,
}

/// How long an uploaded image lives at DeepSeek, and how long its id is reused: a little less,
/// so a request never names a file that has just expired.
const UPLOAD_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
const UPLOAD_REUSE: Duration = Duration::from_secs(30 * 24 * 3600 - 300);

impl std::fmt::Debug for ResponsesProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponsesProvider")
            .field("id", &self.info.id)
            .field("model", &self.info.model)
            .finish_non_exhaustive()
    }
}

impl ResponsesProvider {
    pub fn new(cfg: ResponsesConfig, transport: Arc<dyn Transport>) -> Self {
        let capabilities = Capabilities {
            reasoning: ReasoningInfo {
                produces: true,
                replay_required: true,
                visible_summary: false,
                effort_control: true,
            },
            // Images go through the Files API; the adapter uploads them.
            image_input: true,
            cache_metrics: true,
            // Measured against the live API: a forced tool choice is refused in thinking mode.
            forced_tool_choice: ForcedToolChoice::WithoutReasoning,
        };
        let info = ProviderInfo {
            id: ProviderId::new("deepseek"),
            model: cfg.model.clone(),
            capabilities,
        };
        Self {
            info,
            cfg,
            transport,
            uploads: moka::future::Cache::builder()
                .time_to_live(UPLOAD_REUSE)
                .build(),
            meter: None,
        }
    }

    /// Record every call on `meter`.
    pub fn with_meter(mut self, meter: Arc<dyn Meter>) -> Self {
        self.meter = Some(meter);
        self
    }

    /// The uploaded file id of every image in the conversation.
    async fn load_images(
        &self,
        request: &Request<'_>,
    ) -> Result<HashMap<String, String>, LlmError> {
        let mut out = HashMap::new();
        for key in image_keys(request) {
            let media = request
                .media
                .ok_or_else(|| LlmError::InvalidRequest("images require a media store".into()))?;
            let loaded = media.load(&key).await?;
            let digest = format!("{:x}", Sha256::digest(&loaded.bytes));
            // Concurrent requests with the same picture share one upload.
            let id = self
                .uploads
                .try_get_with(digest, self.upload(&loaded))
                .await
                .map_err(|error| (*error).clone())?;
            out.insert(key, id);
        }
        Ok(out)
    }

    /// Upload one image through the Files API; returns its file id.
    async fn upload(&self, loaded: &crate::request::LoadedMedia) -> Result<String, LlmError> {
        let extension = loaded.mime.split_once('/').map_or("bin", |(_, sub)| sub);
        let upload = Upload {
            file_name: format!("image.{extension}"),
            mime: loaded.mime.clone(),
            bytes: loaded.bytes.clone(),
            fields: vec![
                ("purpose".into(), "user_data".into()),
                ("expires_after[anchor]".into(), "created_at".into()),
                (
                    "expires_after[seconds]".into(),
                    UPLOAD_TTL.as_secs().to_string(),
                ),
            ],
        };
        let response =
            upload_with_retry(&*self.transport, &self.cfg.retry, "/files", &upload).await?;
        let value: serde_json::Value = serde_json::from_slice(&response.body)
            .map_err(|e| LlmError::Protocol(format!("upload response is not JSON: {e}")))?;
        value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| LlmError::Protocol("the upload returned no file id".into()))
    }

    /// Build and send the whole conversation.
    async fn dispatch(&self, request: &Request<'_>) -> Result<HttpResponse, LlmError> {
        validate_request(&self.info, request)?;
        let images = self.load_images(request).await?;
        let body = build_body(&self.cfg, &self.info, request, &images)?;
        post_with_retry(&*self.transport, &self.cfg.retry, "/responses", &body).await
    }
}

#[async_trait]
impl Provider for ResponsesProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    async fn respond(&self, request: Request<'_>) -> Result<Response, LlmError> {
        let started = Instant::now();
        let call = async {
            // An answer whose tool call cannot be read is lost, not wrong: nothing ran, so the
            // same request is sent again, once, like any other failure that passes by itself.
            let mut asked_again = false;
            loop {
                let response = self.dispatch(&request).await?;
                let value: Value = serde_json::from_slice(&response.body)
                    .map_err(|e| LlmError::Protocol(format!("response is not JSON: {e}")))?;
                // Billed whether or not the output is usable, so counted before it is checked.
                meter::add_tokens(parse_usage(value.get("usage")));
                match parse_response(self.info.id.as_str(), &value, &self.info.model) {
                    Err(LlmError::MalformedCall(detail)) if !asked_again => {
                        tracing::info!(
                            model = %self.info.model,
                            %detail,
                            "the model's tool call could not be read; asking again"
                        );
                        asked_again = true;
                    }
                    parsed => {
                        let parsed = parsed?;
                        return Ok(Response {
                            turn: parsed.turn,
                            finish: parsed.finish,
                            usage: parsed.usage,
                            meta: ResponseMeta {
                                model: parsed.model,
                                provider_response_id: parsed.response_id,
                                latency: started.elapsed(),
                            },
                        });
                    }
                }
            }
        };
        let bounded = async {
            match self.cfg.timeout {
                Some(limit) => tokio::time::timeout(limit, call)
                    .await
                    .map_err(|_| LlmError::Timeout)?,
                None => call.await,
            }
        };
        meter::measured(
            self.meter.as_ref(),
            CallKind::Model,
            self.info.id.as_str(),
            &self.info.model,
            bounded,
        )
        .await
    }
}
