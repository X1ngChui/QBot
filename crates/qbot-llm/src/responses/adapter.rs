use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::capability::{
    Capabilities, ForcedToolChoice, ProviderId, ProviderInfo, Realization, ReasoningInfo,
};
use crate::conversation::Conversation;
use crate::error::LlmError;
use crate::meter::{self, CallKind, Meter};
use crate::provider::{Provider, ReplayPlan, plan_replay};
use crate::request::{Request, validate_request};
use crate::response::{Continuation, Response, ResponseMeta};

use super::parse::{Parsed, STATE_LOST, parse_response, parse_usage};
use super::retry::{post_with_retry, upload_with_retry};
use super::transport::{HttpResponse, Transport, Upload};
use super::wire::{ImageSource, build_body, image_keys};
use super::{Flavor, ResponsesConfig, StateMode};

pub struct ResponsesProvider {
    info: ProviderInfo,
    cfg: ResponsesConfig,
    transport: Arc<dyn Transport>,
    /// SHA-256 of an image's bytes to its uploaded file id (DeepSeek), until shortly before the
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
    pub fn new(cfg: ResponsesConfig, transport: Arc<dyn Transport>) -> Result<Self, LlmError> {
        if cfg.flavor == Flavor::DeepSeek && cfg.state == StateMode::ServerState {
            return Err(LlmError::Unsupported(
                "server-side state on the DeepSeek dialect",
            ));
        }
        let deepseek = cfg.flavor == Flavor::DeepSeek;
        let capabilities = Capabilities {
            tool_calls: Realization::Native,
            parallel_tool_calls: Realization::Native,
            continuation: if cfg.state == StateMode::ServerState {
                Realization::Native
            } else {
                Realization::Emulated
            },
            developer_role: if deepseek {
                Realization::Emulated
            } else {
                Realization::Native
            },
            reasoning: ReasoningInfo {
                produces: true,
                replay_required: true,
                visible_summary: false,
                effort_control: true,
            },
            // DeepSeek takes images only as uploaded files; the adapter uploads them.
            image_input: Some(if deepseek {
                Realization::Emulated
            } else {
                Realization::Native
            }),
            cache_metrics: true,
            // Measured against the live API: DeepSeek refuses forced tool choice in thinking mode.
            forced_tool_choice: if deepseek {
                ForcedToolChoice::WithoutReasoning
            } else {
                ForcedToolChoice::Always
            },
        };
        let id = ProviderId::new(if deepseek { "deepseek" } else { "openai" });
        let info = ProviderInfo {
            id,
            model: cfg.model.clone(),
            capabilities,
        };
        Ok(Self {
            info,
            cfg,
            transport,
            uploads: moka::future::Cache::builder()
                .time_to_live(UPLOAD_REUSE)
                .build(),
            meter: None,
        })
    }

    /// Record every call on `meter`.
    pub fn with_meter(mut self, meter: Arc<dyn Meter>) -> Self {
        self.meter = Some(meter);
        self
    }

    async fn load_images(
        &self,
        request: &Request<'_>,
        plan: &ReplayPlan,
    ) -> Result<HashMap<String, ImageSource>, LlmError> {
        let mut out = HashMap::new();
        for key in image_keys(request, plan) {
            let media = request
                .media
                .ok_or_else(|| LlmError::InvalidRequest("images require a media store".into()))?;
            let loaded = media.load(&key).await?;
            let source = if self.cfg.flavor == Flavor::DeepSeek {
                let digest = format!("{:x}", Sha256::digest(&loaded.bytes));
                // Concurrent requests with the same picture share one upload.
                let id = self
                    .uploads
                    .try_get_with(digest, self.upload(&loaded))
                    .await
                    .map_err(|error| (*error).clone())?;
                ImageSource::File(id)
            } else {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&loaded.bytes);
                ImageSource::Url(format!("data:{};base64,{encoded}", loaded.mime))
            };
            out.insert(key, source);
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

    /// Plan, build and send. If the server has forgotten our state handle, resend in full once.
    async fn dispatch(&self, request: &Request<'_>) -> Result<HttpResponse, LlmError> {
        validate_request(&self.info, request)?;
        let mut plan = plan_replay(&self.info, request.conversation, request.continuation);
        loop {
            let images = self.load_images(request, &plan).await?;
            let body = build_body(&self.cfg, &self.info, request, &plan, &images)?;
            match post_with_retry(&*self.transport, &self.cfg.retry, "/responses", &body).await {
                Ok(response) => return Ok(response),
                Err(LlmError::InvalidRequest(message))
                    if message == STATE_LOST && matches!(plan, ReplayPlan::Delta { .. }) =>
                {
                    plan = ReplayPlan::Full;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn assemble(
        info: &ProviderInfo,
        state: StateMode,
        conversation: &Conversation,
        parsed: Parsed,
        started: Instant,
    ) -> Result<Response, LlmError> {
        let handle = match state {
            StateMode::Stateless => None,
            StateMode::ServerState => Some(
                parsed
                    .response_id
                    .clone()
                    .ok_or_else(|| LlmError::Protocol("server-state response has no id".into()))?,
            ),
        };
        let continuation = Continuation::after(
            info.id.clone(),
            info.model.clone(),
            conversation,
            &parsed.turn,
            handle,
        );
        Ok(Response {
            turn: parsed.turn,
            finish: parsed.finish,
            usage: parsed.usage,
            continuation,
            meta: ResponseMeta {
                model: parsed.model,
                provider_response_id: parsed.response_id,
                latency: started.elapsed(),
            },
        })
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
            let response = self.dispatch(&request).await?;
            let value: Value = serde_json::from_slice(&response.body)
                .map_err(|e| LlmError::Protocol(format!("response is not JSON: {e}")))?;
            // Billed whether or not the output is usable, so counted before it is checked.
            meter::add_tokens(parse_usage(value.get("usage")));
            let parsed = parse_response(self.info.id.as_str(), &value, &self.info.model)?;
            Self::assemble(
                &self.info,
                self.cfg.state,
                request.conversation,
                parsed,
                started,
            )
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
