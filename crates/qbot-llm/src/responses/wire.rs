//! Normalized request -> Responses request body.

use std::collections::HashMap;

use qbot_context::{AssistantPart, AssistantTurn};
use serde_json::{Value, json};

use crate::capability::{ForcedToolChoice, ProviderInfo};
use crate::conversation::{Content, ConvItem, Message, Role, ToolOutput};
use crate::error::LlmError;
use crate::request::{ReasoningEffort, Request, ToolChoice};

use super::ResponsesConfig;

/// The image keys of the conversation, each once.
pub(crate) fn image_keys(request: &Request<'_>) -> Vec<String> {
    let mut keys = Vec::new();
    for item in request.conversation.items() {
        let content: &[Content] = match item {
            ConvItem::Message(m) => &m.content,
            ConvItem::ToolResult(o) => &o.content,
            ConvItem::Assistant(_) => &[],
        };
        for part in content {
            if let Content::Image { key } = part
                && !keys.contains(key)
            {
                keys.push(key.clone());
            }
        }
    }
    keys
}

pub(crate) fn build_body(
    cfg: &ResponsesConfig,
    info: &ProviderInfo,
    request: &Request<'_>,
    images: &HashMap<String, String>,
) -> Result<Value, LlmError> {
    let caps = &info.capabilities;
    if matches!(
        request.tool_choice,
        ToolChoice::Required | ToolChoice::Named(_)
    ) {
        match caps.forced_tool_choice {
            ForcedToolChoice::Always => {}
            ForcedToolChoice::WithoutReasoning
                if request.reasoning == crate::ReasoningEffort::Off => {}
            ForcedToolChoice::WithoutReasoning => {
                return Err(LlmError::Unsupported(
                    "forced tool choice with reasoning on",
                ));
            }
            ForcedToolChoice::Never => return Err(LlmError::Unsupported("forced tool choice")),
        }
    }

    let mut input = Vec::new();
    for item in request.conversation.items() {
        convert_item(info, item, images, &mut input)?;
    }

    let mut body = json!({
        "model": cfg.model,
        "input": input,
        "stream": false,
        "store": false,
        "parallel_tool_calls": request.parallel_tool_calls,
    });
    let map = body
        .as_object_mut()
        .ok_or_else(|| LlmError::Protocol("body is not an object".into()))?;

    // `ToolChoice::None` is emulated by declaring no tools at all.
    if !matches!(request.tool_choice, ToolChoice::None) && !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.schema,
                    "strict": false,
                })
            })
            .collect();
        map.insert("tools".into(), Value::Array(tools));
        match &request.tool_choice {
            ToolChoice::Required => {
                map.insert("tool_choice".into(), json!("required"));
            }
            ToolChoice::Named(name) => {
                map.insert(
                    "tool_choice".into(),
                    json!({ "type": "function", "name": name }),
                );
            }
            ToolChoice::Auto | ToolChoice::None => {}
        }
    }
    let effort = match request.reasoning {
        ReasoningEffort::Off => "none",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "high",
        ReasoningEffort::High => "max",
    };
    map.insert("reasoning".into(), json!({ "effort": effort }));
    Ok(body)
}

fn convert_item(
    info: &ProviderInfo,
    item: &ConvItem,
    images: &HashMap<String, String>,
    out: &mut Vec<Value>,
) -> Result<(), LlmError> {
    match item {
        ConvItem::Message(message) => out.push(message_item(message, images)?),
        ConvItem::Assistant(turn) => assistant_items(info, turn, out)?,
        ConvItem::ToolResult(output) => out.push(output_item(output, images)?),
    }
    Ok(())
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        // DeepSeek has no developer role; the instruction is delivered as a user message.
        Role::User | Role::Developer => "user",
    }
}

/// `images` holds the uploaded file id of every image key.
fn content_part(content: &Content, images: &HashMap<String, String>) -> Result<Value, LlmError> {
    match content {
        Content::Text(text) => Ok(json!({ "type": "input_text", "text": text })),
        Content::Image { key } => match images.get(key) {
            Some(id) => Ok(json!({ "type": "input_image", "file_id": id })),
            None => Err(LlmError::InvalidRequest(format!(
                "image {key:?} was not loaded"
            ))),
        },
    }
}

fn message_item(message: &Message, images: &HashMap<String, String>) -> Result<Value, LlmError> {
    let role = role_name(message.role);
    if let [Content::Text(text)] = message.content.as_slice() {
        return Ok(json!({ "role": role, "content": text }));
    }
    let parts = message
        .content
        .iter()
        .map(|c| content_part(c, images))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "role": role, "content": parts }))
}

fn output_item(output: &ToolOutput, images: &HashMap<String, String>) -> Result<Value, LlmError> {
    let all_text = output.content.iter().all(|c| matches!(c, Content::Text(_)));
    let value = if all_text {
        let joined: Vec<&str> = output
            .content
            .iter()
            .filter_map(|c| {
                if let Content::Text(t) = c {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect();
        Value::String(joined.join("\n"))
    } else {
        Value::Array(
            output
                .content
                .iter()
                .map(|c| content_part(c, images))
                .collect::<Result<Vec<_>, _>>()?,
        )
    };
    Ok(
        json!({ "type": "function_call_output", "call_id": output.call_id.as_str(), "output": value }),
    )
}

/// Echo the provider's own output verbatim when we have it; otherwise rebuild the turn from the
/// normalized parts (for example after switching providers or models).
fn assistant_items(
    info: &ProviderInfo,
    turn: &AssistantTurn,
    out: &mut Vec<Value>,
) -> Result<(), LlmError> {
    if let Some(replay) = turn.replay()
        && replay.provider == info.id.as_str()
    {
        let Value::Array(items) = &replay.payload else {
            return Err(LlmError::Protocol(
                "native replay payload is not an array".into(),
            ));
        };
        out.extend(items.iter().cloned());
        return Ok(());
    }
    for part in turn.parts() {
        match part {
            AssistantPart::Reasoning(reasoning) => {
                if reasoning.provider == info.id.as_str() {
                    out.push(reasoning.payload.clone());
                }
            }
            AssistantPart::Text(text) => out.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text }],
            })),
            AssistantPart::Call(call) => out.push(json!({
                "type": "function_call",
                "call_id": call.id.as_str(),
                "name": call.name,
                "arguments": call.arguments.to_string(),
            })),
        }
    }
    Ok(())
}
