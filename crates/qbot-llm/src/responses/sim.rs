//! An in-process stand-in for a Responses server, used to test the adapter end to end.
//!
//! It enforces the request rules real servers enforce (call/output pairing, reasoning followed
//! by its item, known `previous_response_id`), answers in JSON or SSE, simulates prompt-cache
//! usage, and can inject HTTP errors, in-stream errors and cut-off streams.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use serde_json::{Value, json};

use crate::error::LlmError;
use crate::fake::{FakePart, FakeReply};

use super::transport::{HttpBody, HttpResponse, Transport, Upload};

#[derive(Debug, Clone)]
pub enum SimStep {
    Reply(FakeReply),
    /// Reply with `status: incomplete` and the given reason; messages are cut off.
    Incomplete {
        reply: FakeReply,
        reason: &'static str,
    },
    /// A non-2xx HTTP answer.
    Http {
        status: u16,
        retry_after: Option<Duration>,
        code: &'static str,
        message: &'static str,
    },
    /// A 200 stream that carries an `error` event and ends.
    StreamError {
        code: &'static str,
    },
    /// A 200 stream that ends after `events` events with no terminal event.
    CutStream {
        reply: FakeReply,
        events: usize,
    },
    /// A `response.failed` terminal event.
    Failed {
        code: &'static str,
    },
    /// Reply with a hand-written response object (for malformed-output tests).
    Raw(Value),
}

impl From<FakeReply> for SimStep {
    fn from(reply: FakeReply) -> Self {
        SimStep::Reply(reply)
    }
}

#[derive(Default)]
struct Inner {
    script: VecDeque<SimStep>,
    requests: Vec<Value>,
    next_id: u64,
    responses: HashMap<String, u64>,
    last_input: Vec<Value>,
    uploads: Vec<SimUpload>,
}

/// One file the simulated server received: its id, its form fields and its size in bytes.
pub type SimUpload = (String, Vec<(String, String)>, usize);

pub struct SimServer {
    inner: Mutex<Inner>,
    chunk_size: usize,
}

impl std::fmt::Debug for SimServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimServer").finish_non_exhaustive()
    }
}

impl SimServer {
    pub fn new(script: impl IntoIterator<Item = SimStep>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                script: script.into_iter().collect(),
                ..Inner::default()
            }),
            chunk_size: 11,
        }
    }

    /// Bytes per network chunk in SSE answers; small values stress the decoder.
    pub fn with_chunk_size(mut self, size: usize) -> Self {
        self.chunk_size = size.max(1);
        self
    }

    pub fn requests(&self) -> Vec<Value> {
        self.lock().requests.clone()
    }

    /// Every upload: its file id, its form fields and its size in bytes.
    pub fn uploads(&self) -> Vec<SimUpload> {
        self.lock().uploads.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn error_response(
    status: u16,
    retry_after: Option<Duration>,
    code: &str,
    message: &str,
) -> HttpResponse {
    let body = json!({ "error": { "code": code, "message": message } });
    HttpResponse {
        status,
        retry_after,
        body: HttpBody::Full(Bytes::from(body.to_string())),
    }
}

fn tokens(value: &Value) -> u64 {
    (value.to_string().len() as u64 / 4).max(1)
}

/// The rules a real server applies to `input`.
fn check_input(body: &Value) -> Result<(), String> {
    let input = body
        .get("input")
        .and_then(Value::as_array)
        .ok_or("input must be an array")?;
    let resumed = body.get("previous_response_id").is_some();
    let kind = |item: &Value| {
        item.get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
            .to_owned()
    };
    for (index, item) in input.iter().enumerate() {
        match kind(item).as_str() {
            "reasoning" => {
                let next = input.get(index + 1).map(kind);
                if !matches!(next.as_deref(), Some("message" | "function_call")) {
                    return Err(
                        "reasoning item provided without its required following item".into(),
                    );
                }
            }
            "function_call" => {
                let id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let answered = input[index + 1..].iter().any(|later| {
                    kind(later) == "function_call_output"
                        && later.get("call_id").and_then(Value::as_str) == Some(id)
                });
                if !answered {
                    return Err(format!("no tool output found for function call {id}"));
                }
            }
            "function_call_output" => {
                let id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let called = input[..index].iter().any(|earlier| {
                    kind(earlier) == "function_call"
                        && earlier.get("call_id").and_then(Value::as_str) == Some(id)
                });
                if !called && !resumed {
                    return Err(format!("no function call found for output {id}"));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn output_items(inner: &mut Inner, reply: &FakeReply, incomplete: bool) -> Vec<Value> {
    let mut items = Vec::new();
    for part in reply.parts() {
        inner.next_id += 1;
        let n = inner.next_id;
        items.push(match part {
            FakePart::Reasoning(summary) => json!({
                "type": "reasoning", "id": format!("rs_{n}"),
                "summary": [{ "type": "summary_text", "text": summary }],
                "encrypted_content": format!("enc-{n}"),
            }),
            FakePart::Text(text) => json!({
                "type": "message", "id": format!("msg_{n}"), "role": "assistant",
                "status": if incomplete { "incomplete" } else { "completed" },
                "content": [{ "type": "output_text", "text": text }],
            }),
            FakePart::Call { name, arguments } => json!({
                "type": "function_call", "id": format!("fc_{n}"), "call_id": format!("call_{n}"),
                "name": name, "arguments": arguments.to_string(), "status": "completed",
            }),
        });
    }
    items
}

fn sse(events: &[Value]) -> Vec<u8> {
    let mut out = String::new();
    for event in events {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        out.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
    }
    out.into_bytes()
}

fn chunked(bytes: Vec<u8>, size: usize) -> HttpBody {
    let chunks: Vec<Result<Bytes, LlmError>> = bytes
        .chunks(size)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    HttpBody::Stream(Box::pin(stream::iter(chunks)))
}

fn deltas(items: &[Value]) -> Vec<Value> {
    let mut events = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                let text = item["summary"][0]["text"].as_str().unwrap_or_default();
                events.push(
                    json!({ "type": "response.reasoning_summary_text.delta", "delta": text }),
                );
            }
            Some("message") => {
                let text = item["content"][0]["text"].as_str().unwrap_or_default();
                let chars: Vec<char> = text.chars().collect();
                for chunk in chars.chunks(5) {
                    let delta: String = chunk.iter().collect();
                    events.push(json!({ "type": "response.output_text.delta", "delta": delta }));
                }
            }
            Some("function_call") => {
                events.push(json!({ "type": "response.output_item.added", "item": item }));
                let args = item["arguments"].as_str().unwrap_or_default();
                let chars: Vec<char> = args.chars().collect();
                for chunk in chars.chunks(4) {
                    let delta: String = chunk.iter().collect();
                    events.push(json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": item["id"], "delta": delta,
                    }));
                }
            }
            _ => {}
        }
    }
    events
}

#[async_trait]
impl Transport for SimServer {
    async fn upload(&self, path: &str, upload: &Upload) -> Result<HttpResponse, LlmError> {
        let mut inner = self.lock();
        if path != "/files" {
            return Ok(error_response(404, None, "not_found", "unknown path"));
        }
        let id = format!("file-{}", inner.uploads.len() + 1);
        inner
            .uploads
            .push((id.clone(), upload.fields.clone(), upload.bytes.len()));
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: HttpBody::Full(Bytes::from(
                json!({ "id": id, "object": "file" }).to_string(),
            )),
        })
    }

    async fn post(
        &self,
        path: &str,
        body: &Value,
        want_stream: bool,
    ) -> Result<HttpResponse, LlmError> {
        let mut inner = self.lock();
        inner.requests.push(body.clone());
        if path != "/responses" {
            return Ok(error_response(404, None, "not_found", "unknown path"));
        }
        if let Err(message) = check_input(body) {
            return Ok(error_response(400, None, "invalid_request_error", &message));
        }
        // A real server refuses a file it never received.
        let unknown = body
            .to_string()
            .split("\"file_id\":\"")
            .skip(1)
            .find_map(|rest| {
                let id = rest.split('"').next().unwrap_or_default();
                (!inner.uploads.iter().any(|(known, _, _)| known == id)).then(|| id.to_owned())
            });
        if let Some(id) = unknown {
            return Ok(error_response(
                400,
                None,
                "invalid_request_error",
                &format!("unknown file {id}"),
            ));
        }
        let previous = body.get("previous_response_id").and_then(Value::as_str);
        let carried: u64 = match previous {
            Some(id) => match inner.responses.get(id) {
                Some(size) if body.get("store") == Some(&json!(true)) => *size,
                Some(_) => {
                    return Ok(error_response(
                        400,
                        None,
                        "invalid_request_error",
                        "state was not stored",
                    ));
                }
                None => {
                    return Ok(error_response(
                        404,
                        None,
                        "previous_response_not_found",
                        "unknown response",
                    ));
                }
            },
            None => 0,
        };
        let Some(step) = inner.script.pop_front() else {
            return Ok(error_response(
                500,
                None,
                "server_error",
                "sim script exhausted",
            ));
        };

        let (reply, status, reason, mode) = match step {
            SimStep::Http {
                status,
                retry_after,
                code,
                message,
            } => {
                return Ok(error_response(status, retry_after, code, message));
            }
            SimStep::StreamError { code } => {
                let events = [json!({ "type": "error", "code": code, "message": "boom" })];
                return Ok(HttpResponse {
                    status: 200,
                    retry_after: None,
                    body: chunked(sse(&events), self.chunk_size),
                });
            }
            SimStep::Raw(value) => {
                let terminal = json!({ "type": "response.completed", "response": value });
                return Ok(if want_stream {
                    HttpResponse {
                        status: 200,
                        retry_after: None,
                        body: chunked(sse(&[terminal]), self.chunk_size),
                    }
                } else {
                    HttpResponse {
                        status: 200,
                        retry_after: None,
                        body: HttpBody::Full(Bytes::from(value.to_string())),
                    }
                });
            }
            SimStep::Failed { code } => {
                let response = json!({ "status": "failed", "error": { "code": code, "message": "failed" }, "output": [] });
                let terminal = json!({ "type": "response.failed", "response": response });
                return Ok(if want_stream {
                    HttpResponse {
                        status: 200,
                        retry_after: None,
                        body: chunked(sse(&[terminal]), self.chunk_size),
                    }
                } else {
                    HttpResponse {
                        status: 200,
                        retry_after: None,
                        body: HttpBody::Full(Bytes::from(response.to_string())),
                    }
                });
            }
            SimStep::Reply(reply) => (reply, "completed", None, None),
            SimStep::Incomplete { reply, reason } => (reply, "incomplete", Some(reason), None),
            SimStep::CutStream { reply, events } => (reply, "completed", None, Some(events)),
        };

        let input: Vec<Value> = body["input"].as_array().cloned().unwrap_or_default();
        let input_tokens = carried + tokens(&Value::Array(input.clone()));
        // A stateful server already holds the carried prefix; otherwise the cache is the common
        // prefix with the previous request's input.
        let hit: u64 = if carried > 0 {
            carried
        } else {
            input
                .iter()
                .zip(inner.last_input.iter())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| tokens(a))
                .sum()
        };
        inner.last_input = input;
        let items = output_items(&mut inner, &reply, status == "incomplete");
        inner.next_id += 1;
        let id = format!("resp_{}", inner.next_id);
        let output_tokens = tokens(&Value::Array(items.clone()));
        inner
            .responses
            .insert(id.clone(), input_tokens + output_tokens);

        let mut response = json!({
            "id": id, "object": "response", "status": status, "model": body["model"],
            "output": items,
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": { "cached_tokens": hit.min(input_tokens) },
                "output_tokens": output_tokens,
                "output_tokens_details": { "reasoning_tokens": 0 },
            },
        });
        if let Some(reason) = reason {
            response["incomplete_details"] = json!({ "reason": reason });
        }
        if !want_stream {
            return Ok(HttpResponse {
                status: 200,
                retry_after: None,
                body: HttpBody::Full(Bytes::from(response.to_string())),
            });
        }
        let mut events = deltas(&output_items_view(&response));
        if let Some(limit) = mode {
            events.truncate(limit);
        } else {
            let kind = if status == "incomplete" {
                "response.incomplete"
            } else {
                "response.completed"
            };
            events.push(json!({ "type": kind, "response": response }));
        }
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: chunked(sse(&events), self.chunk_size),
        })
    }
}

fn output_items_view(response: &Value) -> Vec<Value> {
    response["output"].as_array().cloned().unwrap_or_default()
}
