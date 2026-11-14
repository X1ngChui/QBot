use async_trait::async_trait;

use crate::capability::ProviderInfo;
use crate::error::LlmError;
use crate::request::Request;
use crate::response::Response;

/// The single interface the agent runtime uses to reach a model.
///
/// A request carries the complete logical conversation, and every call replays it: the
/// provider keeps no state between calls. Callers never branch on the provider. The answer is one whole
/// turn: everything QBot does with a turn (send a message, run tools, store an episode) needs
/// all of it, so nothing is streamed.
#[async_trait]
pub trait Provider: Send + Sync {
    fn info(&self) -> &ProviderInfo;

    async fn respond(&self, request: Request<'_>) -> Result<Response, LlmError>;
}
