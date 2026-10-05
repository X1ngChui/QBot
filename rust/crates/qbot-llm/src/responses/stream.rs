//! SSE events -> normalized stream events. Deltas are previews; the terminal event carries the
//! authoritative response object.

use std::collections::HashMap;

use qbot_core::CallId;
use serde_json::Value;

use crate::error::LlmError;
use crate::response::StreamEvent;

use super::parse::stream_error;

pub(crate) enum Folded {
    Events(Vec<StreamEvent>),
    Terminal {
        response: Value,
        status_hint: &'static str,
    },
}

#[derive(Debug, Default)]
pub(crate) struct StreamFolder {
    calls_by_item: HashMap<String, CallId>,
}

impl StreamFolder {
    pub(crate) fn feed(&mut self, data: &str) -> Result<Folded, LlmError> {
        if data.trim() == "[DONE]" {
            return Ok(Folded::Events(Vec::new()));
        }
        let data: Value = serde_json::from_str(data)
            .map_err(|e| LlmError::Protocol(format!("stream event is not JSON: {e}")))?;
        let kind = data.get("type").and_then(Value::as_str).unwrap_or_default();
        let text = |name: &str| {
            data.get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        match kind {
            "error" => Err(stream_error(&data)),
            "response.completed" | "response.failed" | "response.incomplete" => {
                let response = data
                    .get("response")
                    .filter(|r| r.is_object())
                    .cloned()
                    .ok_or_else(|| LlmError::Protocol("terminal event has no response".into()))?;
                let status_hint = match kind {
                    "response.completed" => "completed",
                    "response.failed" => "failed",
                    _ => "incomplete",
                };
                Ok(Folded::Terminal {
                    response,
                    status_hint,
                })
            }
            "response.output_text.delta" => Ok(one(StreamEvent::TextDelta(text("delta")))),
            "response.reasoning_summary_text.delta" => {
                Ok(one(StreamEvent::ReasoningDelta(text("delta"))))
            }
            "response.output_item.added" => {
                let item = data.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return Ok(Folded::Events(Vec::new()));
                }
                let call_id = CallId::new(
                    item.get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
                .map_err(|_| LlmError::Protocol("function_call has an invalid call_id".into()))?;
                if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                    self.calls_by_item
                        .insert(item_id.to_owned(), call_id.clone());
                }
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Ok(one(StreamEvent::CallStarted { id: call_id, name }))
            }
            "response.function_call_arguments.delta" => {
                match self.calls_by_item.get(&text("item_id")) {
                    Some(id) => Ok(one(StreamEvent::CallArguments {
                        id: id.clone(),
                        fragment: text("delta"),
                    })),
                    None => Ok(Folded::Events(Vec::new())),
                }
            }
            _ => Ok(Folded::Events(Vec::new())),
        }
    }
}

fn one(event: StreamEvent) -> Folded {
    Folded::Events(vec![event])
}
