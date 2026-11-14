//! The provider contract: one normalized interface to every model backend.
//!
//! The agent runtime depends only on [`Provider`]. What the vendor lacks is absorbed in its
//! adapter (a developer message sent as a user message, images sent as uploaded files) or is
//! an explicitly typed field (cache metrics a provider does not report, when a tool call can be
//! forced). There is no vendor-specific branch above this layer.
//!
//! Layout:
//! - [`capability`]: what callers decide by.
//! - [`conversation`]: the provider-facing conversation, lowered from a `qbot_context::View`.
//! - [`request`] and [`response`]: one call in, one normalized result out.
//! - [`error`]: the single error taxonomy.
//! - [`provider`]: the trait.
//! - [`meter`]: one record per provider call, whatever the work.
//! - [`fake`]: a scripted provider implementing the whole contract, for offline tests.

pub mod capability;
pub mod conversation;
pub mod embedding;
pub mod error;
pub mod fake;
pub mod meter;
pub mod net;
pub mod provider;
pub mod request;
pub mod response;
pub mod responses;
pub mod schema;
pub mod search;

pub use capability::{Capabilities, ForcedToolChoice, ProviderId, ProviderInfo, ReasoningInfo};
pub use conversation::{
    Content, ConvItem, Conversation, ConversationError, Message, PlainRenderer, Renderer, Role,
    ToolOutput, ToolStatus,
};
pub use embedding::{Embedder, EmbedderInfo, Embeddings, FakeEmbedder, HttpEmbedder};
pub use error::LlmError;
pub use meter::{Attribution, CallKind, MemoryMeter, Meter, ProviderCall, Purpose, attributed};
pub use provider::Provider;
pub use request::{
    LoadedMedia, MediaStore, ReasoningEffort, Request, ToolChoice, ToolSpec, validate_request,
};
pub use response::{CacheUsage, FinishReason, Response, ResponseMeta, Usage};
