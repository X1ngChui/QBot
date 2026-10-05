use std::pin::Pin;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};

use crate::capability::{ProviderInfo, Realization};
use crate::conversation::Conversation;
use crate::error::LlmError;
use crate::request::Request;
use crate::response::{Continuation, Response, StreamEvent};

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>;

/// The single interface the agent runtime uses to reach a model.
///
/// Both methods take the complete logical conversation. Implementations decide how much of it
/// to put on the wire; callers never branch on the provider.
#[async_trait]
pub trait Provider: Send + Sync {
    fn info(&self) -> &ProviderInfo;

    async fn respond(&self, request: Request<'_>) -> Result<Response, LlmError>;

    /// Events end with exactly one [`StreamEvent::Completed`].
    async fn stream(&self, request: Request<'_>) -> Result<EventStream, LlmError>;
}

/// Drain a stream into its final response, enforcing the stream contract.
pub async fn collect(mut stream: EventStream) -> Result<Response, LlmError> {
    let mut completed: Option<Response> = None;
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::Completed(response) => {
                if completed.is_some() {
                    return Err(LlmError::Protocol("more than one terminal event".into()));
                }
                completed = Some(*response);
            }
            _ if completed.is_some() => {
                return Err(LlmError::Protocol("event after the terminal event".into()));
            }
            _ => {}
        }
    }
    completed.ok_or(LlmError::StreamInterrupted)
}

/// What an adapter should put on the wire for this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayPlan {
    /// Send the whole conversation.
    Full,
    /// The provider holds state up to `from`; send items `from..` and the handle.
    Delta { from: usize, handle: String },
}

/// Decide between full replay and a stateful delta. A delta is used only when the continuation
/// belongs to this provider and model and its covered prefix is still byte-identical in the
/// conversation (so elision or compaction of old items safely falls back to full replay).
pub fn plan_replay(
    info: &ProviderInfo,
    conversation: &Conversation,
    continuation: Option<&Continuation>,
) -> ReplayPlan {
    if info.capabilities.continuation != Realization::Native {
        return ReplayPlan::Full;
    }
    let Some(cont) = continuation else {
        return ReplayPlan::Full;
    };
    let Some(handle) = cont.handle.as_ref() else {
        return ReplayPlan::Full;
    };
    let applies = cont.provider == info.id
        && cont.model == info.model
        && conversation.len() > cont.covers
        && conversation.digest_prefix(cont.covers) == cont.digest;
    if applies {
        ReplayPlan::Delta {
            from: cont.covers,
            handle: handle.clone(),
        }
    } else {
        ReplayPlan::Full
    }
}
