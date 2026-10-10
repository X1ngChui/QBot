//! Runs the tool calls of one model turn with deterministic ordering.
//!
//! Reads run concurrently; writes then run one at a time in call order. A batch is
//! never rejected as a whole: each call gets its own result, so the model always sees what
//! happened to everything it asked for.

use qbot_wording::{Text, say};
use std::time::Duration;

use futures_util::future::join_all;
use qbot_context::{ErrorKind, Outcome, Part, ToolCall, ToolResult};
use tokio::time::Instant;

use crate::tool::{Effect, ErasedTool, ToolCx, ToolError, ToolOutput, ToolSet};

/// Bounds on one run. Only `max_turns` is local: it stops a model that never stops calling
/// tools from looping for the whole deadline. Output size, call counts and context size are the
/// provider's and the tools' concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    pub max_turns: u32,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self { max_turns: 20 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Executed {
    pub name: String,
    pub result: ToolResult,
    pub ends_run: bool,
    pub latency: Duration,
}

pub async fn execute_turn(tools: &ToolSet, calls: &[ToolCall], cx: &ToolCx<'_>) -> Vec<Executed> {
    let mut slots: Vec<Option<Executed>> = vec![None; calls.len()];
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        match tools.get(&call.name) {
            None => {
                slots[index] = Some(failed(
                    call,
                    ErrorKind::InvalidArguments,
                    say(Text::OutcomeUnknownTool {
                        name: call.name.clone(),
                    }),
                ));
            }
            Some(tool) if tool.effect() == Effect::Read => reads.push((index, tool)),
            Some(tool) => writes.push((index, tool)),
        }
    }

    let read_results = join_all(
        reads
            .iter()
            .map(|(i, tool)| run_one(tool.as_ref(), &calls[*i], cx)),
    )
    .await;
    for ((i, _), executed) in reads.into_iter().zip(read_results) {
        slots[i] = Some(executed);
    }
    for (i, tool) in writes {
        slots[i] = Some(run_one(tool.as_ref(), &calls[i], cx).await);
    }
    slots.into_iter().flatten().collect()
}

async fn run_one(tool: &dyn ErasedTool, call: &ToolCall, cx: &ToolCx<'_>) -> Executed {
    let started = Instant::now();
    let outcome = tool.call(cx, call.arguments.clone()).await;
    let latency = started.elapsed();
    let mut executed = match outcome {
        Ok(output) => success(call, output),
        Err(ToolError::InvalidArguments(message)) => {
            failed(call, ErrorKind::InvalidArguments, message)
        }
        Err(ToolError::Refused { reason, message }) => {
            result(call, Outcome::Refused(reason), message)
        }
        Err(ToolError::Failed(message)) => failed(call, ErrorKind::Execution, message),
        Err(ToolError::Unavailable(message)) => failed(call, ErrorKind::Unavailable, message),
    };
    executed.latency = latency;
    executed
}

fn result(call: &ToolCall, outcome: Outcome, text: String) -> Executed {
    Executed {
        name: call.name.clone(),
        result: ToolResult {
            call_id: call.id.clone(),
            outcome,
            content: vec![Part::Text(text)],
        },
        ends_run: false,
        latency: Duration::ZERO,
    }
}

fn failed(call: &ToolCall, kind: ErrorKind, text: String) -> Executed {
    result(call, Outcome::Error(kind), text)
}

fn success(call: &ToolCall, output: ToolOutput) -> Executed {
    Executed {
        name: call.name.clone(),
        result: ToolResult {
            call_id: call.id.clone(),
            outcome: Outcome::Ok,
            content: output.content,
        },
        ends_run: output.ends_run,
        latency: Duration::ZERO,
    }
}
