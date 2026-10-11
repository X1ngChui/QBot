use qbot_core::CallId;
use serde::Serialize;

use crate::chat::ChatBatch;
use crate::error::ProjectError;
use crate::item::{AssistantTurn, Instruction, Item, ToolResult};
use crate::transcript::Transcript;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ViewItem {
    Instruction(Instruction),
    Chat(ChatBatch),
    Summary(String),
    Assistant(AssistantTurn),
    ToolResult(ToolResult),
}

/// Exactly what the model is shown, before provider serialization.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct View {
    pub items: Vec<ViewItem>,
}

/// Derive the model's view. Calls are emitted with their results immediately after, in call
/// order, regardless of the order the results were appended in.
pub fn project(transcript: &Transcript) -> Result<View, ProjectError> {
    if !transcript.is_quiescent() {
        return Err(ProjectError::PendingCalls {
            pending: transcript.pending().len(),
        });
    }
    let items = transcript.items();
    let summaries = transcript.summaries();

    let mut hidden = vec![false; items.len()];
    for (_, summary) in summaries {
        for flag in &mut hidden[summary.from.index()..summary.to.index()] {
            *flag = true;
        }
    }
    // A summary is active unless a later one covers it entirely.
    let active: Vec<_> = summaries
        .iter()
        .filter(|(seq, s)| {
            !summaries
                .iter()
                .any(|(later, outer)| later > seq && outer.from <= s.from && s.to <= outer.to)
        })
        .collect();

    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if let Some((_, summary)) = active.iter().find(|(_, s)| s.from.index() == index) {
            out.push(ViewItem::Summary(summary.text.clone()));
        }
        if hidden[index] {
            continue;
        }
        match item {
            Item::Instruction(instruction) => out.push(ViewItem::Instruction(instruction.clone())),
            Item::Chat(batch) => out.push(ViewItem::Chat(batch.clone())),
            Item::Assistant(turn) => {
                out.push(ViewItem::Assistant(turn.clone()));
                for call in turn.calls() {
                    out.push(ViewItem::ToolResult(
                        result_for(transcript, &call.id).clone(),
                    ));
                }
            }
            Item::ToolResult(_) | Item::Meta(_) | Item::Summary(_) => {}
        }
    }
    Ok(View { items: out })
}

fn result_for<'a>(transcript: &'a Transcript, id: &CallId) -> &'a ToolResult {
    // A quiescent transcript has a result for every call, so this lookup always succeeds.
    transcript
        .call_state(id)
        .and_then(|(_, result_seq)| result_seq)
        .and_then(|seq| match &transcript.items()[seq.index()] {
            Item::ToolResult(result) => Some(result),
            _ => None,
        })
        .unwrap_or_else(|| unreachable!("quiescent transcript has a result for every call"))
}
