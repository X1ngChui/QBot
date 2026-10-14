use async_trait::async_trait;

use crate::capability::{ProviderInfo, Realization};
use crate::conversation::Conversation;
use crate::error::LlmError;
use crate::request::Request;
use crate::response::{Continuation, Response};

/// The single interface the agent runtime uses to reach a model.
///
/// A request carries the complete logical conversation; the implementation decides how much of
/// it to put on the wire, and callers never branch on the provider. The answer is one whole
/// turn: everything QBot does with a turn (send a message, run tools, store an episode) needs
/// all of it, so nothing is streamed.
#[async_trait]
pub trait Provider: Send + Sync {
    fn info(&self) -> &ProviderInfo;

    async fn respond(&self, request: Request<'_>) -> Result<Response, LlmError>;
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
    let Some((handle, digest)) = cont.state.as_ref() else {
        return ReplayPlan::Full;
    };
    let applies = cont.provider == info.id
        && cont.model == info.model
        && conversation.len() > cont.covers
        && conversation.digest_prefix(cont.covers) == *digest;
    if applies {
        ReplayPlan::Delta {
            from: cont.covers,
            handle: handle.clone(),
        }
    } else {
        ReplayPlan::Full
    }
}
