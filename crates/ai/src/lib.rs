//! Message types, tool specs and streaming LLM providers.

pub mod anthropic;
pub mod catalog;
pub(crate) mod cross_provider;
#[cfg(feature = "testing")]
pub mod fake;
mod http;
pub mod key;
pub mod levels;
pub mod oauth;
pub mod openai;
pub mod pi;
pub mod providers;
mod request_images;
pub mod sse;
pub mod types;

pub use http::check_header;
pub use key::Credential;
pub use providers::{
    Auth, Compat, CompatOverride, MaxTokensField, Models, Preset, ReasoningField, Route, Wire,
};

pub use types::{
    AssistantMessage, Content, Image, ImageType, Message, Reasoning, Role, StopReason, Text,
    ToolCall, ToolResult, ToolSpec, Usage,
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
    /// The wire's own fields for sending the reply back, kept as
    /// [`AssistantMessage::replay`].
    Replay(Text),
}

/// Retry metadata a provider sent with an error, captured from the response
/// headers of a non-success status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Retry {
    /// The `x-should-retry` header value, when the provider sent one.
    pub should: Option<bool>,
    /// Wait milliseconds the provider asked for, from `retry-after-ms`, else
    /// from `retry-after` seconds, when it sent either.
    pub wait_ms: Option<u64>,
}

impl Retry {
    /// Reads the retry headers of an HTTP response. `headers` is borrowed only
    /// for the call.
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        let should = headers
            .get("x-should-retry")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| match value.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            });
        let wait_ms = parse_retry_after_ms(headers).or_else(|| {
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .and_then(|seconds| seconds.checked_mul(1000))
        });
        Self { should, wait_ms }
    }
}

fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// A provider failure.
#[derive(Debug)]
pub enum Error {
    /// The HTTP request failed before a response arrived.
    Http(String),
    /// The provider answered with a non-success status.
    Status {
        /// The HTTP status code.
        code: u16,
        /// The response body.
        body: String,
        /// Retry metadata from the response headers.
        retry: Retry,
    },
    /// A provider payload was not valid JSON.
    Json(serde_json::Error),
    /// A stream-level error from the provider that is safe to retry, such as
    /// Anthropic's `overloaded_error` or an OpenAI server or rate-limit error.
    Transient(String),
    /// The byte stream did not follow the provider protocol.
    Protocol(String),
}

impl Error {
    /// Whether the error is safe to retry, and the wait the provider asked
    /// for.
    ///
    /// `None` means the error is not retryable. `Some(None)` means it is
    /// retryable with no provider-mandated wait. `Some(Some(delay))` is a
    /// retryable error that names a `retry-after` delay.
    pub fn retry_hint(&self) -> Option<Option<std::time::Duration>> {
        fn wait(retry: &Retry) -> Option<Option<std::time::Duration>> {
            Some(retry.wait_ms.map(std::time::Duration::from_millis))
        }
        match self {
            // A transport error, before the reply or in the middle of it. The
            // loop records a turn only once its stream ends, so a retry never
            // sends a half reply again.
            Error::Http(_) => Some(None),
            Error::Status { code, retry, .. } => {
                // The provider's own verdict overrides the status rule.
                if let Some(should) = retry.should {
                    return should.then(|| wait(retry)).flatten();
                }
                let retryable = matches!(code, 408 | 409 | 429) || *code >= 500;
                retryable.then(|| wait(retry)).flatten()
            }
            Error::Json(_) => None,
            Error::Transient(_) => Some(None),
            Error::Protocol(_) => None,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Http(message) => write!(f, "http error: {message}"),
            Error::Status { code, body, .. } => write!(f, "provider status {code}: {body}"),
            Error::Json(error) => write!(f, "json error: {error}"),
            Error::Transient(message) => write!(f, "transient provider error: {message}"),
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
    /// Whether the model takes images, so the wires send them. When it is
    /// false, each wire writes `[image not sent: the model takes no images]` in
    /// their place.
    pub images: bool,
    /// How much reasoning the provider may spend, in its own words: a level
    /// text or a token budget. See [`Reasoning::parse`].
    pub reasoning: Option<&'a str>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn headers(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                reqwest::header::HeaderValue::from_str(value).expect("header value"),
            );
        }
        map
    }

    fn status(code: u16) -> Error {
        Error::Status {
            code,
            body: "boom".into(),
            retry: Retry::default(),
        }
    }

    #[test]
    fn a_transport_error_is_retryable_with_default_wait() {
        assert_eq!(Error::Http("refused".into()).retry_hint(), Some(None));
    }

    #[test]
    fn retryable_statuses_are_retried() {
        for code in [408, 409, 429, 500, 503, 529] {
            assert_eq!(status(code).retry_hint(), Some(None), "{code}");
        }
    }

    #[test]
    fn other_statuses_are_not_retried() {
        for code in [400, 401, 403, 404, 422, 451] {
            assert_eq!(status(code).retry_hint(), None, "{code}");
        }
    }

    #[test]
    fn x_should_retry_header_overrides_the_status_rule() {
        let no = Error::Status {
            code: 503,
            body: "x".into(),
            retry: Retry {
                should: Some(false),
                wait_ms: None,
            },
        };
        assert_eq!(no.retry_hint(), None);
        let yes = Error::Status {
            code: 400,
            body: "x".into(),
            retry: Retry {
                should: Some(true),
                wait_ms: None,
            },
        };
        assert_eq!(yes.retry_hint(), Some(None));
    }

    #[test]
    fn a_json_or_protocol_error_is_not_retried() {
        assert_eq!(
            Error::Json(serde_json::from_str::<()>("nope").unwrap_err()).retry_hint(),
            None
        );
        assert_eq!(Error::Protocol("boom".into()).retry_hint(), None);
    }

    #[test]
    fn a_transient_stream_error_is_retried() {
        assert_eq!(
            Error::Transient("overloaded".into()).retry_hint(),
            Some(None)
        );
    }

    #[test]
    fn retry_after_ms_names_the_wait_in_milliseconds() {
        let error = Error::Status {
            code: 429,
            body: "x".into(),
            retry: Retry {
                should: None,
                wait_ms: Some(2500),
            },
        };
        assert_eq!(error.retry_hint(), Some(Some(Duration::from_millis(2500))));
    }

    #[test]
    fn from_headers_reads_should_retry_and_retry_after_ms() {
        let map = headers(&[
            ("x-should-retry", "true"),
            ("retry-after-ms", "1500"),
            ("retry-after", "999"),
        ]);
        let retry = Retry::from_headers(&map);
        assert_eq!(retry.should, Some(true));
        assert_eq!(
            retry.wait_ms,
            Some(1500),
            "retry-after-ms wins over retry-after"
        );
    }

    #[test]
    fn from_headers_falls_back_to_retry_after_seconds() {
        let map = headers(&[("retry-after", "3")]);
        let retry = Retry::from_headers(&map);
        assert_eq!(retry.should, None);
        assert_eq!(retry.wait_ms, Some(3000));
    }

    #[test]
    fn from_headers_ignores_a_non_integer_retry_after() {
        let map = headers(&[("retry-after", "Thu, 01 Jan 2030 00:00:00 GMT")]);
        assert_eq!(Retry::from_headers(&map).wait_ms, None);
    }
}
