use std::collections::HashMap;

use qbot_core::{CallId, ItemSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{AppendError, LoadError};
use crate::item::{Item, Meta, Outcome, Summary, ToolResult};

#[derive(Debug, Clone, Copy)]
struct CallState {
    call_seq: ItemSeq,
    result_seq: Option<ItemSeq>,
}

/// Append-only log of one run. [`Transcript::append`] is the only mutation path and enforces
/// every invariant, so a transcript that exists is always well-formed.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    items: Vec<Item>,
    calls: HashMap<CallId, CallState>,
    pending: Vec<CallId>,
    summaries: Vec<(ItemSeq, Summary)>,
}

impl PartialEq for Transcript {
    fn eq(&self, other: &Self) -> bool {
        self.items == other.items
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a transcript by replaying stored items through [`Transcript::append`].
    pub fn from_items(items: Vec<Item>) -> Result<Self, LoadError> {
        let mut transcript = Self::new();
        for (index, item) in items.into_iter().enumerate() {
            transcript.append(item).map_err(|source| LoadError {
                at: ItemSeq::new(u32::try_from(index).unwrap_or(u32::MAX)),
                source,
            })?;
        }
        Ok(transcript)
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Calls issued but not yet resolved, in issue order.
    pub fn pending(&self) -> &[CallId] {
        &self.pending
    }

    pub fn is_quiescent(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn append(&mut self, item: Item) -> Result<ItemSeq, AppendError> {
        let seq = ItemSeq::new(u32::try_from(self.items.len()).map_err(|_| AppendError::TooLong)?);
        match &item {
            Item::Assistant(turn) => {
                self.require_quiescent()?;
                if turn.parts().is_empty() {
                    return Err(AppendError::EmptyTurn);
                }
                let mut issued: Vec<CallId> = Vec::new();
                for call in turn.calls() {
                    if self.calls.contains_key(&call.id) || issued.contains(&call.id) {
                        return Err(AppendError::DuplicateCallId(call.id.clone()));
                    }
                    issued.push(call.id.clone());
                }
                for id in issued {
                    self.calls.insert(
                        id.clone(),
                        CallState {
                            call_seq: seq,
                            result_seq: None,
                        },
                    );
                    self.pending.push(id);
                }
            }
            Item::ToolResult(result) => {
                let state = self
                    .calls
                    .get_mut(&result.call_id)
                    .ok_or_else(|| AppendError::UnknownCall(result.call_id.clone()))?;
                if state.result_seq.is_some() {
                    return Err(AppendError::AlreadyResolved(result.call_id.clone()));
                }
                state.result_seq = Some(seq);
                self.pending.retain(|id| id != &result.call_id);
            }
            Item::Instruction(_) | Item::Chat(_) => self.require_quiescent()?,
            Item::Summary(summary) => {
                self.require_quiescent()?;
                self.validate_summary(summary)?;
                self.summaries.push((seq, summary.clone()));
            }
            Item::Meta(_) => {}
        }
        self.items.push(item);
        Ok(seq)
    }

    /// Resolve every pending call as interrupted and record the resume. Deterministic, so a
    /// resumed run never leaves a request without a result. Returns how many calls were closed.
    pub fn close_interrupted(&mut self) -> u32 {
        let pending = self.pending.clone();
        let count = u32::try_from(pending.len()).unwrap_or(u32::MAX);
        for call_id in pending {
            let result = ToolResult {
                call_id,
                outcome: Outcome::Interrupted,
                content: Vec::new(),
            };
            // Cannot fail: each id was pending and is resolved exactly once.
            let _ = self.append(Item::ToolResult(result));
        }
        if count > 0 {
            let _ = self.append(Item::Meta(Meta::Resumed {
                interrupted_calls: count,
            }));
        }
        count
    }

    fn require_quiescent(&self) -> Result<(), AppendError> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(AppendError::ItemsWhilePending {
                pending: self.pending.len(),
            })
        }
    }

    fn validate_summary(&self, summary: &Summary) -> Result<(), AppendError> {
        let (from, to) = (summary.from.index(), summary.to.index());
        if from >= to || to > self.items.len() {
            return Err(AppendError::BadSummaryRange {
                from: summary.from.get(),
                to: summary.to.get(),
            });
        }
        let inside = |seq: ItemSeq| (from..to).contains(&seq.index());
        for item in &self.items[from..to] {
            match item {
                Item::Instruction(_) => return Err(AppendError::SummaryCoversInstruction),
                Item::Assistant(turn) => {
                    for call in turn.calls() {
                        let resolved_inside = self
                            .calls
                            .get(&call.id)
                            .and_then(|state| state.result_seq)
                            .is_some_and(inside);
                        if !resolved_inside {
                            return Err(AppendError::SummarySplitsCall(call.id.clone()));
                        }
                    }
                }
                Item::ToolResult(result) => {
                    let issued_inside = self
                        .calls
                        .get(&result.call_id)
                        .is_some_and(|s| inside(s.call_seq));
                    if !issued_inside {
                        return Err(AppendError::SummarySplitsCall(result.call_id.clone()));
                    }
                }
                _ => {}
            }
        }
        for (_, existing) in &self.summaries {
            let (ef, et) = (existing.from.index(), existing.to.index());
            let disjoint = to <= ef || et <= from;
            if disjoint {
                continue;
            }
            if ef <= from && to <= et {
                return Err(AppendError::SummaryAlreadyCovered);
            }
            if !(from <= ef && et <= to) {
                return Err(AppendError::SummaryOverlap);
            }
        }
        Ok(())
    }

    pub(crate) fn call_state(&self, id: &CallId) -> Option<(ItemSeq, Option<ItemSeq>)> {
        self.calls
            .get(id)
            .map(|state| (state.call_seq, state.result_seq))
    }

    pub(crate) fn summaries(&self) -> &[(ItemSeq, Summary)] {
        &self.summaries
    }
}

impl Serialize for Transcript {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.items.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Transcript {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let items = Vec::<Item>::deserialize(deserializer)?;
        Transcript::from_items(items).map_err(serde::de::Error::custom)
    }
}
