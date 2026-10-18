//! Message types, tool specs and streaming LLM providers.

pub mod anthropic;
pub(crate) mod cross_provider;
#[cfg(feature = "testing")]
pub mod fake;
mod http;
mod key;
pub mod openai;
pub mod sse;
pub mod types;

pub use types::{
    AssistantMessage, Content, Message, ReasoningLevel, Role, StopReason, Text, ToolCall,
    ToolResult, ToolSpec, Usage,
};

use futures_util::stream::BoxStream;

/// One event of a streaming assistant reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    /// A piece of visible answer text.
    TextDelta(Text),
    /// A piece of the model's private reasoning.
    ThinkingDelta(Text),
    /// The opaque signature that closes a thinking block.
    ThinkingSignature(Text),
    /// A redacted thinking block, delivered whole.
    RedactedThinking(Text),
    /// The model started one tool call.
    ToolCallStart { id: Text, name: Text },
    /// A piece of one tool call's JSON arguments.
    ToolCallDelta { id: Text, partial_json: Text },
    /// The model finished one tool call.
    ToolCallEnd { id: Text },
    /// Running token counts for the reply.
    Usage(Usage),
    /// The reply ended, with the reason the provider gave.
    Stop(StopReason),
}

/// A provider failure.
#[derive(Debug)]
pub enum Error {
    /// The HTTP request failed before a response arrived.
    Http(String),
    /// The provider answered with a non-success status.
    Status(u16, String),
    /// A provider payload was not valid JSON.
    Json(serde_json::Error),
    /// The byte stream did not follow the provider protocol.
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Http(message) => write!(f, "http error: {message}"),
            Error::Status(code, body) => write!(f, "provider status {code}: {body}"),
            Error::Json(error) => write!(f, "json error: {error}"),
            Error::Protocol(message) => write!(f, "protocol error: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Json(error)
    }
}

/// One request to a provider.
///
/// The model id, the system prompt, the tools and the history are borrowed
/// from the caller, so a turn copies no message and no tool schema. A provider
/// builds its body inside [`Provider::stream`] and never holds the borrow past
/// that call.
#[derive(Debug)]
pub struct Request<'a> {
    /// The provider model id.
    pub model: &'a str,
    /// The cap on reply tokens, not counting the reasoning budget.
    pub max_tokens: u32,
    /// The system prompt, if any.
    pub system: Option<&'a Text>,
    /// Tools offered to the model.
    pub tools: &'a [ToolSpec],
    /// The conversation so far, oldest first.
    pub messages: &'a [Message],
    /// How much reasoning the provider may spend.
    pub reasoning: Option<ReasoningLevel>,
    /// A stable key that identifies the cached prompt prefix of this session.
    pub cache_key: Option<&'a Text>,
}

/// A streaming chat provider.
pub trait Provider: Send + Sync {
    /// The provider name recorded on assistant messages.
    fn name(&self) -> &'static str;

    /// Starts one assistant reply and streams its events.
    ///
    /// The body is built before the stream starts, so the history is never
    /// cloned and never held past this call.
    fn stream(&self, request: &Request<'_>) -> BoxStream<'static, Result<StreamEvent, Error>>;
}
