//! Responses reply -> normalized turn, and error mapping.

use std::collections::HashSet;
use std::time::Duration;

use qbot_context::{AssistantPart, AssistantTurn, NativeReplay, OpaqueReasoning, ToolCall};
use qbot_core::CallId;
use serde_json::Value;

use crate::error::LlmError;
use crate::response::{CacheUsage, FinishReason, Usage};

/// Marker message for a server that no longer knows our `previous_response_id`.
pub(crate) const STATE_LOST: &str = "previous_response_not_found";

pub(crate) struct Parsed {
    pub turn: AssistantTurn,
    pub finish: FinishReason,
    pub usage: Usage,
    pub model: String,
    pub response_id: Option<String>,
}

pub(crate) fn parse_response(
    provider: &str,
    value: &Value,
    status_hint: Option<&str>,
    fallback_model: &str,
) -> Result<Parsed, LlmError> {
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .or(status_hint)
        .unwrap_or("completed");
    if status == "failed" {
        return Err(error_from_object(value.get("error"), None, None));
    }
    let incomplete_reason = value
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str);
    match status {
        "completed" => {}
        "incomplete" => {
            if incomplete_reason == Some("content_filter") {
                return Err(LlmError::ContentFiltered);
            }
        }
        other => {
            return Err(LlmError::Protocol(format!(
                "unexpected response status {other:?}"
            )));
        }
    }
    let truncated = status == "incomplete";

    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Protocol("response has no output array".into()))?;

    let mut parts = Vec::new();
    let mut kept: Vec<Value> = Vec::new();
    let mut call_ids: HashSet<String> = HashSet::new();
    let mut item_ids: HashSet<String> = HashSet::new();
    for item in output {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let item_status = item.get("status").and_then(Value::as_str);
        let complete = item_status.is_none_or(|s| s == "completed");
        if !complete && !(truncated && kind == "message") {
            // A cut-off call or reasoning item cannot be replayed or executed; it is dropped
            // along with its native form, and the finish reason reports the truncation.
            if truncated && item_status == Some("incomplete") {
                continue;
            }
            return Err(LlmError::Protocol(format!(
                "output item {kind:?} has status {item_status:?}"
            )));
        }
        if let Some(id) = item.get("id").and_then(Value::as_str)
            && !item_ids.insert(id.to_owned())
        {
            return Err(LlmError::Protocol(format!(
                "duplicate output item id {id:?}"
            )));
        }
        match kind {
            "message" => {
                let text = message_text(item)?;
                if !text.is_empty() {
                    parts.push(AssistantPart::Text(text));
                }
            }
            "function_call" => {
                let raw_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let id = CallId::new(raw_id).map_err(|_| {
                    LlmError::Protocol("function_call has an invalid call_id".into())
                })?;
                if !call_ids.insert(raw_id.to_owned()) {
                    return Err(LlmError::Protocol(format!("duplicate call_id {raw_id:?}")));
                }
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                if name.is_empty() {
                    return Err(LlmError::Protocol("function_call has no name".into()));
                }
                let raw_args = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        LlmError::Protocol("function_call arguments are not a string".into())
                    })?;
                let arguments: Value =
                    serde_json::from_str(if raw_args.is_empty() { "{}" } else { raw_args })
                        .map_err(|e| {
                            // The start of what came, so the log shows what the model did.
                            let head: String = raw_args.chars().take(120).collect();
                            LlmError::Protocol(format!(
                                "function_call {name} arguments are not JSON ({e}): {head:?}"
                            ))
                        })?;
                if !arguments.is_object() {
                    return Err(LlmError::Protocol(
                        "function_call arguments are not an object".into(),
                    ));
                }
                parts.push(AssistantPart::Call(ToolCall {
                    id,
                    name: name.to_owned(),
                    arguments,
                }));
            }
            "reasoning" => parts.push(AssistantPart::Reasoning(OpaqueReasoning {
                provider: provider.to_owned(),
                payload: item.clone(),
                summary: reasoning_summary(item),
            })),
            _ => {}
        }
        kept.push(item.clone());
    }

    let turn = AssistantTurn::new(parts).with_replay(NativeReplay {
        provider: provider.to_owned(),
        payload: Value::Array(kept),
    });
    let has_calls = turn.calls().next().is_some();
    let finish = if has_calls {
        FinishReason::ToolCalls
    } else if truncated {
        match incomplete_reason {
            Some("max_output_tokens") | None => FinishReason::Length,
            Some(other) => FinishReason::Other(other.to_owned()),
        }
    } else {
        FinishReason::Stop
    };

    Ok(Parsed {
        turn,
        finish,
        usage: parse_usage(value.get("usage")),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(fallback_model)
            .to_owned(),
        response_id: value.get("id").and_then(Value::as_str).map(str::to_owned),
    })
}

fn message_text(item: &Value) -> Result<String, LlmError> {
    match item.get("content") {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => Ok(parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("output_text") => part.get("text").and_then(Value::as_str),
                Some("refusal") => part.get("refusal").and_then(Value::as_str),
                _ => None,
            })
            .collect()),
        None => Ok(String::new()),
        Some(_) => Err(LlmError::Protocol(
            "message content has an unexpected shape".into(),
        )),
    }
}

fn reasoning_summary(item: &Value) -> Option<String> {
    let texts: Vec<&str> = item
        .get("summary")?
        .as_array()?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

fn parse_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage.filter(|u| u.is_object()) else {
        return Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache: CacheUsage::NotReported,
            reasoning_tokens: None,
            reported: false,
        };
    };
    let get = |value: Option<&Value>| {
        value
            .and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    let cached = get(usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens")));
    Usage {
        input_tokens: get(usage.get("input_tokens")).unwrap_or(0),
        output_tokens: get(usage.get("output_tokens")).unwrap_or(0),
        cache: match cached {
            Some(hit_tokens) => CacheUsage::Reported { hit_tokens },
            None => CacheUsage::NotReported,
        },
        reasoning_tokens: get(usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))),
        reported: true,
    }
}

/// Map a provider error code (and, if known, the HTTP status) to the normalized taxonomy.
pub(crate) fn map_error(
    status: Option<u16>,
    code: &str,
    message: &str,
    retry_after: Option<Duration>,
) -> LlmError {
    match code {
        "context_length_exceeded" => return LlmError::ContextTooLong,
        "rate_limit_exceeded" => return LlmError::RateLimited { retry_after },
        "timeout" => return LlmError::Timeout,
        "internal_error" | "internal_server_error" | "server_error" | "temporarily_unavailable" => {
            return LlmError::Unavailable;
        }
        "content_filter" | "content_policy_violation" => return LlmError::ContentFiltered,
        "invalid_api_key" | "authentication_error" => return LlmError::Auth,
        "previous_response_not_found" => return LlmError::InvalidRequest(STATE_LOST.into()),
        _ => {}
    }
    match status {
        Some(401 | 403) => LlmError::Auth,
        // The account has no balance left (DeepSeek: "Insufficient Balance"): not transient.
        Some(402) => LlmError::QuotaExhausted(message.to_owned()),
        Some(408) => LlmError::Timeout,
        Some(429) => LlmError::RateLimited { retry_after },
        Some(400 | 404 | 422) => LlmError::InvalidRequest(message.to_owned()),
        Some(s) if s >= 500 => LlmError::Unavailable,
        Some(s) => LlmError::Other {
            status: s,
            message: message.to_owned(),
        },
        None => LlmError::Other {
            status: 0,
            message: message.to_owned(),
        },
    }
}

fn error_from_object(
    error: Option<&Value>,
    status: Option<u16>,
    retry_after: Option<Duration>,
) -> LlmError {
    let field = |name: &str| {
        error
            .and_then(|e| e.get(name))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let code = {
        let code = field("code");
        if code.is_empty() { field("type") } else { code }
    };
    map_error(status, &code, &field("message"), retry_after)
}

/// Error from a non-2xx HTTP reply body.
pub(crate) fn http_error(status: u16, body: &[u8], retry_after: Option<Duration>) -> LlmError {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let error = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .filter(|e| e.is_object());
    match error {
        Some(error) => error_from_object(Some(error), Some(status), retry_after),
        None => map_error(
            Some(status),
            "",
            &String::from_utf8_lossy(body),
            retry_after,
        ),
    }
}

/// Error from an in-stream `error` event (top-level or nested under `error`).
pub(crate) fn stream_error(event: &Value) -> LlmError {
    let nested = event.get("error").filter(|e| e.is_object());
    error_from_object(Some(nested.unwrap_or(event)), None, None)
}
