use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use futures_util::{StreamExt, stream};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::capability::{
    Capabilities, ForcedToolChoice, ProviderId, ProviderInfo, Realization, ReasoningInfo,
};
use crate::conversation::Conversation;
use crate::error::LlmError;
use crate::provider::{EventStream, Provider, ReplayPlan, plan_replay};
use crate::request::{Request, validate_request};
use crate::response::{Continuation, Response, ResponseMeta, StreamEvent};

use super::parse::{Parsed, STATE_LOST, parse_response};
use super::retry::{post_with_retry, upload_with_retry};
use super::stream::{Folded, StreamFolder};
use super::transport::{ByteStream, HttpBody, HttpResponse, Transport, Upload};
use super::wire::{ImageSource, build_body, image_keys};
use super::{Flavor, ResponsesConfig, StateMode};
use eventsource_stream::{EventStream as SseStream, EventStreamError, Eventsource};

pub struct ResponsesProvider {
    info: ProviderInfo,
    cfg: ResponsesConfig,
    transport: Arc<dyn Transport>,
    /// SHA-256 of an image's bytes to its uploaded file id (DeepSeek), until shortly before the
    /// file expires. Keyed by content: a request's image keys name pictures only within its own
    /// media store, while this outlives every request.
    uploads: moka::future::Cache<String, String>,
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
            streaming: Realization::Native,
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
        })
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
        let (response, _) =
            upload_with_retry(&*self.transport, &self.cfg.retry, "/files", &upload).await?;
        let HttpBody::Full(bytes) = response.body else {
            return Err(LlmError::Protocol("expected a full body".into()));
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| LlmError::Protocol(format!("upload response is not JSON: {e}")))?;
        value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| LlmError::Protocol("the upload returned no file id".into()))
    }

    /// Plan, build and send. If the server has forgotten our state handle, resend in full once.
    async fn dispatch(
        &self,
        request: &Request<'_>,
        stream: bool,
    ) -> Result<(HttpResponse, u32), LlmError> {
        validate_request(&self.info, request)?;
        let mut plan = plan_replay(&self.info, request.conversation, request.continuation);
        let mut attempts_total = 0;
        loop {
            let images = self.load_images(request, &plan).await?;
            let body = build_body(&self.cfg, &self.info, request, &plan, stream, &images)?;
            match post_with_retry(
                &*self.transport,
                &self.cfg.retry,
                "/responses",
                &body,
                stream,
            )
            .await
            {
                Ok((response, attempts)) => return Ok((response, attempts_total + attempts)),
                Err(LlmError::InvalidRequest(message))
                    if message == STATE_LOST && matches!(plan, ReplayPlan::Delta { .. }) =>
                {
                    attempts_total += 1;
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
        attempts: u32,
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
                attempts,
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
            let (response, attempts) = self.dispatch(&request, false).await?;
            let HttpBody::Full(bytes) = response.body else {
                return Err(LlmError::Protocol("expected a full body".into()));
            };
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| LlmError::Protocol(format!("response is not JSON: {e}")))?;
            let parsed = parse_response(self.info.id.as_str(), &value, None, &self.info.model)?;
            Self::assemble(
                &self.info,
                self.cfg.state,
                request.conversation,
                parsed,
                started,
                attempts,
            )
        };
        match self.cfg.timeout {
            Some(limit) => tokio::time::timeout(limit, call)
                .await
                .map_err(|_| LlmError::Timeout)?,
            None => call.await,
        }
    }

    async fn stream(&self, request: Request<'_>) -> Result<EventStream, LlmError> {
        let started = Instant::now();
        let deadline = self
            .cfg
            .timeout
            .map(|limit| tokio::time::Instant::now() + limit);
        let dispatch = self.dispatch(&request, true);
        let (response, attempts) = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, dispatch)
                .await
                .map_err(|_| LlmError::Timeout)??,
            None => dispatch.await?,
        };
        let HttpBody::Stream(bytes) = response.body else {
            return Err(LlmError::Protocol("expected a streaming body".into()));
        };
        let state = Pump {
            events: bytes.eventsource(),
            folder: StreamFolder::default(),
            queue: VecDeque::new(),
            finished: false,
            deadline,
            info: self.info.clone(),
            provider_state: self.cfg.state,
            conversation: request.conversation.clone(),
            started,
            attempts,
        };
        Ok(Box::pin(stream::unfold(state, Pump::next)))
    }
}

/// Drives one SSE response to its terminal event.
struct Pump {
    events: SseStream<ByteStream>,
    folder: StreamFolder,
    queue: VecDeque<Result<StreamEvent, LlmError>>,
    finished: bool,
    deadline: Option<tokio::time::Instant>,
    info: ProviderInfo,
    provider_state: StateMode,
    conversation: Conversation,
    started: Instant,
    attempts: u32,
}

impl Pump {
    async fn next(mut self) -> Option<(Result<StreamEvent, LlmError>, Self)> {
        loop {
            if let Some(item) = self.queue.pop_front() {
                if item.is_err() {
                    self.finished = true;
                    self.queue.clear();
                }
                return Some((item, self));
            }
            if self.finished {
                return None;
            }
            let next = match self.deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, self.events.next())
                    .await
                    .map_err(|_| LlmError::Timeout),
                None => Ok(self.events.next().await),
            };
            let event = match next {
                Err(error) => Err(error),
                Ok(None) => Err(LlmError::StreamInterrupted),
                Ok(Some(Err(EventStreamError::Transport(error)))) => Err(error),
                Ok(Some(Err(other))) => Err(LlmError::Protocol(other.to_string())),
                Ok(Some(Ok(event))) => Ok(event),
            };
            let events = match event {
                Ok(event) => vec![event],
                Err(error) => {
                    self.queue.push_back(Err(error));
                    continue;
                }
            };
            for event in events {
                match self.folder.feed(&event.data) {
                    Ok(Folded::Events(items)) => self.queue.extend(items.into_iter().map(Ok)),
                    Ok(Folded::Terminal {
                        response,
                        status_hint,
                    }) => {
                        let result = parse_response(
                            self.info.id.as_str(),
                            &response,
                            Some(status_hint),
                            &self.info.model,
                        )
                        .and_then(|parsed| {
                            ResponsesProvider::assemble(
                                &self.info,
                                self.provider_state,
                                &self.conversation,
                                parsed,
                                self.started,
                                self.attempts,
                            )
                        })
                        .map(|response| StreamEvent::Completed(Box::new(response)));
                        self.queue.push_back(result);
                        self.finished = true;
                        break;
                    }
                    Err(error) => {
                        self.queue.push_back(Err(error));
                        break;
                    }
                }
            }
        }
    }
}
