//! Normalized request -> Responses request body.

use std::collections::HashMap;

use qbot_context::{AssistantPart, AssistantTurn};
use serde_json::{Value, json};

use crate::capability::{ForcedToolChoice, ProviderInfo};
use crate::conversation::{Content, ConvItem, Message, Role, ToolOutput};
use crate::error::LlmError;
use crate::provider::ReplayPlan;
use crate::request::{ReasoningEffort, Request, ToolChoice};

use super::{Flavor, ResponsesConfig, StateMode};

/// How an image goes on the wire: inline as a data URL, or as a file uploaded earlier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageSource {
    Url(String),
    File(String),
}

/// Image keys that appear in the items that will actually be sent.
pub(crate) fn image_keys(request: &Request<'_>, plan: &ReplayPlan) -> Vec<String> {
    let from = match plan {
        ReplayPlan::Full => 0,
        ReplayPlan::Delta { from, .. } => *from,
    };
    let mut keys = Vec::new();
    for item in &request.conversation.items()[from..] {
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
    plan: &ReplayPlan,
    stream: bool,
    images: &HashMap<String, ImageSource>,
) -> Result<Value, LlmError> {
    let caps = &info.capabilities;
    if matches!(
        request.tool_choice,
        ToolChoice::Required | ToolChoice::Named(_)
    ) {
        match caps.forced_tool_choice {
            ForcedToolChoice::Always => {}
            ForcedToolChoice::WithoutReasoning
                if request.params.reasoning == crate::ReasoningEffort::Off => {}
            ForcedToolChoice::WithoutReasoning => {
                return Err(LlmError::Unsupported(
                    "forced tool choice with reasoning on",
                ));
            }
            ForcedToolChoice::Never => return Err(LlmError::Unsupported("forced tool choice")),
        }
    }
    if request.params.temperature.is_some() && !caps.temperature {
        return Err(LlmError::Unsupported("temperature"));
    }

    let (from, previous) = match plan {
        ReplayPlan::Full => (0, None),
        ReplayPlan::Delta { from, handle } => (*from, Some(handle.as_str())),
    };
    let mut input = Vec::new();
    for item in &request.conversation.items()[from..] {
        convert_item(cfg, info, item, images, &mut input)?;
    }

    let mut body = json!({
        "model": cfg.model,
        "input": input,
        "stream": stream,
        "store": cfg.state == StateMode::ServerState,
        "parallel_tool_calls": request.parallel_tool_calls,
        "max_output_tokens": request.params.max_output_tokens,
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
    if let Some(temperature) = request.params.temperature {
        map.insert("temperature".into(), json!(temperature));
    }
    if let Some(id) = previous {
        map.insert("previous_response_id".into(), json!(id));
    }
    match cfg.flavor {
        Flavor::DeepSeek => {
            let effort = match request.params.reasoning {
                ReasoningEffort::Off => "none",
                ReasoningEffort::Low => "low",
                ReasoningEffort::Medium => "high",
                ReasoningEffort::High => "max",
            };
            map.insert("reasoning".into(), json!({ "effort": effort }));
        }
        Flavor::Standard => {
            let effort = match request.params.reasoning {
                ReasoningEffort::Off => None,
                ReasoningEffort::Low => Some("low"),
                ReasoningEffort::Medium => Some("medium"),
                ReasoningEffort::High => Some("high"),
            };
            if let Some(effort) = effort {
                map.insert("reasoning".into(), json!({ "effort": effort }));
            }
            if cfg.state == StateMode::Stateless {
                map.insert("include".into(), json!(["reasoning.encrypted_content"]));
            }
        }
    }
    Ok(body)
}

fn convert_item(
    cfg: &ResponsesConfig,
    info: &ProviderInfo,
    item: &ConvItem,
    images: &HashMap<String, ImageSource>,
    out: &mut Vec<Value>,
) -> Result<(), LlmError> {
    match item {
        ConvItem::Message(message) => out.push(message_item(cfg, message, images)?),
        ConvItem::Assistant(turn) => assistant_items(info, turn, out)?,
        ConvItem::ToolResult(output) => out.push(output_item(output, images)?),
    }
    Ok(())
}

fn role_name(cfg: &ResponsesConfig, role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        // DeepSeek has no developer role; the instruction is delivered as a user message.
        Role::Developer if cfg.flavor == Flavor::DeepSeek => "user",
        Role::Developer => "developer",
    }
}

fn content_part(
    content: &Content,
    images: &HashMap<String, ImageSource>,
) -> Result<Value, LlmError> {
    match content {
        Content::Text(text) => Ok(json!({ "type": "input_text", "text": text })),
        Content::Image { key } => match images.get(key) {
            Some(ImageSource::Url(url)) => Ok(json!({ "type": "input_image", "image_url": url })),
            Some(ImageSource::File(id)) => Ok(json!({ "type": "input_image", "file_id": id })),
            None => Err(LlmError::InvalidRequest(format!(
                "image {key:?} was not loaded"
            ))),
        },
    }
}

fn message_item(
    cfg: &ResponsesConfig,
    message: &Message,
    images: &HashMap<String, ImageSource>,
) -> Result<Value, LlmError> {
    let role = role_name(cfg, message.role);
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

fn output_item(
    output: &ToolOutput,
    images: &HashMap<String, ImageSource>,
) -> Result<Value, LlmError> {
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
