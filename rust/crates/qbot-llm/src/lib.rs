//! The provider contract: one normalized interface to every model backend.
//!
//! The agent runtime depends only on [`Provider`]. Everything a vendor lacks is absorbed in its
//! adapter, either by emulation (for example replaying the full conversation to a stateless
//! API) or by an explicitly typed optional field (for example cache metrics that a provider
//! does not report). There is no vendor-specific branch above this layer.
//!
//! Layout:
//! - [`capability`]: what a provider offers, and whether natively or by emulation.
//! - [`conversation`]: the provider-facing conversation, lowered from a `qbot_context::View`.
//! - [`request`] and [`response`]: one call in, one normalized result out (or a stream).
//! - [`error`]: the single error taxonomy.
//! - [`provider`]: the trait and the continuation planner shared by all adapters.
//! - [`fake`]: a scripted provider implementing the whole contract, for offline tests.

pub mod capability;
pub mod conversation;
pub mod embedding;
pub mod error;
pub mod fake;
pub mod provider;
pub mod request;
pub mod response;
pub mod responses;
pub mod schema;
pub mod search;

pub use capability::{
    Capabilities, ForcedToolChoice, ProviderId, ProviderInfo, Realization, ReasoningInfo,
};
pub use conversation::{
    Content, ConvDigest, ConvItem, Conversation, ConversationError, Message, PlainRenderer,
    Renderer, Role, ToolOutput, ToolStatus,
};
pub use embedding::{Embedder, EmbedderInfo, Embeddings, FakeEmbedder, HttpEmbedder};
pub use error::LlmError;
pub use provider::{EventStream, Provider, ReplayPlan, collect, plan_replay};
pub use request::{
    LoadedMedia, MediaStore, Params, ReasoningEffort, Request, ToolChoice, ToolSpec,
    validate_request,
};
pub use response::{
    CacheUsage, Continuation, FinishReason, Response, ResponseMeta, StreamEvent, Usage,
};
