use qbot_core::{CallId, ItemSeq};

/// Why an item cannot be appended to a transcript.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AppendError {
    #[error("{pending} tool call(s) are still unresolved")]
    ItemsWhilePending { pending: usize },
    #[error("an assistant turn must contain at least one part")]
    EmptyTurn,
    #[error("call id {0:?} is already used in this transcript")]
    DuplicateCallId(CallId),
    #[error("result for call {0:?}, which was never issued")]
    UnknownCall(CallId),
    #[error("call {0:?} already has a result")]
    AlreadyResolved(CallId),
    #[error("summary range {from}..{to} is empty or beyond the end of the transcript")]
    BadSummaryRange { from: u32, to: u32 },
    #[error("summary range overlaps an existing summary without containing it")]
    SummaryOverlap,
    #[error("summary range is already covered by an existing summary")]
    SummaryAlreadyCovered,
    #[error("summary range would split call {0:?} from its result")]
    SummarySplitsCall(CallId),
    #[error("summary range would hide an instruction")]
    SummaryCoversInstruction,
    #[error("transcript is too long to index")]
    TooLong,
}

/// A stored transcript failed validation while being reloaded.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("item {at:?} is invalid: {source}")]
pub struct LoadError {
    pub at: ItemSeq,
    #[source]
    pub source: AppendError,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProjectError {
    #[error("cannot project a transcript with {pending} unresolved tool call(s)")]
    PendingCalls { pending: usize },
}
