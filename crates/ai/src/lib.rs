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
pub mod record;
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
use std::borrow::Cow;

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
    /// The plan limits the reply's headers named, sent once, before the other
    /// events of the reply.
    Limits(Box<Limits>),
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

/// The plan limits a ChatGPT backend sends with its reply, from the
/// `x-codex` headers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    /// The shorter window, when the reply named one.
    pub primary: Option<Window>,
    /// The longer window, when the reply named one.
    pub secondary: Option<Window>,
}

/// One limit window of a plan.
///
/// The fields are 32-bit to keep the event that carries them small. A value
/// that does not fit is absent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    /// How much of the window the account has used, from 0 to 100.
    pub used_percent: f32,
    /// The width of the window in minutes, when the reply named one.
    pub window_minutes: Option<u32>,
    /// When the window resets, in Unix seconds, when the reply named it.
    pub resets_at: Option<u32>,
}

impl Limits {
    /// Reads the plan limits of an HTTP response, borrowing `headers` for the
    /// call only.
    ///
    /// `None` when the response named no window. A window whose used-percent
    /// header is missing or does not parse is absent, and so is a window that
    /// has used nothing, names no width and no reset: an older backend sends
    /// three empty headers.
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Option<Self> {
        let limits = Self {
            primary: Window::from_headers(headers, "primary"),
            secondary: Window::from_headers(headers, "secondary"),
        };
        (limits.primary.is_some() || limits.secondary.is_some()).then_some(limits)
    }
}

// `f32` has no `Eq`, so the markers are written out by hand: a provider never
// reports a `NaN` percent, and a limit is compared only in tests.
impl Eq for Limits {}

impl Eq for Window {}

impl Window {
    /// Reads one window of `headers`, whose names end in `-{name}-...`.
    fn from_headers(headers: &reqwest::header::HeaderMap, name: &str) -> Option<Self> {
        let used_percent = header_of(headers, name, "used-percent")?
            .trim()
            .parse::<f32>()
            .ok()?;
        let window_minutes = header_of(headers, name, "window-minutes")
            .and_then(|value| value.trim().parse::<u32>().ok());
        let resets_at =
            header_of(headers, name, "reset-at").and_then(|value| value.trim().parse::<u32>().ok());
        // A window nothing is known about is no window: an empty set of
        // headers would otherwise read as a limit of zero.
        if used_percent == 0.0 && window_minutes.unwrap_or(0) == 0 && resets_at.is_none() {
            return None;
        }
        Some(Self {
            used_percent,
            window_minutes,
            resets_at,
        })
    }
}

/// The `x-codex-{name}-{field}` header of `headers`, when it is text.
fn header_of<'a>(
    headers: &'a reqwest::header::HeaderMap,
    name: &str,
    field: &str,
) -> Option<&'a str> {
    headers
        .get(format!("x-codex-{name}-{field}"))
        .and_then(|value| value.to_str().ok())
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

/// The message of a provider error body, parsed without copying it.
#[derive(serde::Deserialize)]
struct ErrorBody<'a> {
    #[serde(borrow)]
    error: Option<ErrorField<'a>>,
    #[serde(borrow)]
    message: Option<Cow<'a, str>>,
}

/// The `error` field of a provider error body: an object or a string.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ErrorField<'a> {
    /// An object with an optional message.
    Object {
        #[serde(borrow)]
        message: Option<Cow<'a, str>>,
    },
    /// The message itself.
    Text(#[serde(borrow)] Cow<'a, str>),
}

/// The message of a provider error body, else the body as it is.
fn status_message(body: &str) -> Cow<'_, str> {
    let Ok(ErrorBody { error, message }) = serde_json::from_str::<ErrorBody<'_>>(body) else {
        return Cow::Borrowed(body);
    };
    let error = error.map(|error| match error {
        ErrorField::Object { message } => message,
        ErrorField::Text(text) => Some(text),
    });
    let message = match error.flatten().or(message) {
        Some(Cow::Borrowed(text)) => Cow::Borrowed(text.trim()),
        Some(Cow::Owned(text)) => Cow::Owned(text.trim().to_string()),
        None => Cow::Borrowed(body),
    };
    if message.is_empty() {
        return Cow::Borrowed(body);
    }
    message
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Http(message) => write!(f, "http error: {message}"),
            Error::Status { code, body, .. } => {
                write!(f, "provider status {code}: {}", status_message(body))
            }
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

/// Whether a request lets the model call its tools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolChoice {
    /// The model decides.
    #[default]
    Auto,
    /// The model may not call a tool. The definitions stay in the request, but
    /// a host may still miss the prompt cache or reject the request. The
    /// pi-messages wire has no such field and ignores this, so a caller still
    /// handles a call in the reply.
    Off,
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
    /// Whether the model may call the offered tools.
    pub tool_choice: ToolChoice,
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

    fn limits(used: f32, minutes: &str, reset: &str) -> Vec<(&'static str, String)> {
        vec![
            ("x-codex-primary-used-percent", used.to_string()),
            ("x-codex-primary-window-minutes", minutes.to_string()),
            ("x-codex-primary-reset-at", reset.to_string()),
        ]
    }

    fn limited(pairs: &[(&'static str, String)]) -> Option<Limits> {
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        Limits::from_headers(&headers(&pairs))
    }

    #[test]
    fn limits_of_both_windows_read_their_three_headers() {
        let mut pairs = limits(30.0, "300", "1763050000");
        pairs.extend([
            ("x-codex-secondary-used-percent", "75.5".to_string()),
            ("x-codex-secondary-window-minutes", "10080".to_string()),
            ("x-codex-secondary-reset-at", "1763140000".to_string()),
        ]);
        assert_eq!(
            limited(&pairs),
            Some(Limits {
                primary: Some(Window {
                    used_percent: 30.0,
                    window_minutes: Some(300),
                    resets_at: Some(1_763_050_000),
                }),
                secondary: Some(Window {
                    used_percent: 75.5,
                    window_minutes: Some(10_080),
                    resets_at: Some(1_763_140_000),
                }),
            })
        );
    }

    #[test]
    fn limits_of_the_primary_window_alone_leave_the_secondary_out() {
        assert_eq!(
            limited(&limits(12.25, "60", "1763050000")),
            Some(Limits {
                primary: Some(Window {
                    used_percent: 12.25,
                    window_minutes: Some(60),
                    resets_at: Some(1_763_050_000),
                }),
                secondary: None,
            })
        );
    }

    #[test]
    fn limits_of_no_headers_are_none() {
        assert_eq!(Limits::from_headers(&headers(&[])), None);
    }

    #[test]
    fn limits_of_a_window_of_zeros_are_none() {
        assert_eq!(limited(&limits(0.0, "0", "")), None);
    }

    #[test]
    fn limits_of_an_unparsable_percent_drop_the_window() {
        // A percent that does not parse drops the window, whatever the rest
        // of its headers say.
        let mut pairs = limits(30.0, "300", "1763050000");
        pairs[0].1 = "lots".to_string();
        assert_eq!(limited(&pairs), None);
        // A missing percent header drops it too, where the width and the
        // reset alone would have named a window.
        assert_eq!(
            limited(&limits(30.0, "300", "1763050000")),
            Some(Limits {
                primary: Some(Window {
                    used_percent: 30.0,
                    window_minutes: Some(300),
                    resets_at: Some(1_763_050_000),
                }),
                secondary: None,
            })
        );
        let mut pairs = limits(30.0, "300", "1763050000");
        pairs.remove(0);
        assert_eq!(limited(&pairs), None);
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

    fn status_body(body: &str) -> String {
        Error::Status {
            code: 400,
            body: body.into(),
            retry: Retry::default(),
        }
        .to_string()
    }

    #[test]
    fn status_message_reads_error_message() {
        let body = r#"{"error":{"message":"m is not a valid model ID","code":400},"user_id":"u"}"#;
        assert_eq!(
            status_body(body),
            "provider status 400: m is not a valid model ID"
        );
    }

    #[test]
    fn status_message_reads_a_top_level_message() {
        assert_eq!(
            status_body(r#"{"message":"bad"}"#),
            "provider status 400: bad"
        );
    }

    #[test]
    fn status_message_reads_a_string_error() {
        assert_eq!(
            status_body(r#"{"error":"bad key"}"#),
            "provider status 400: bad key"
        );
    }

    #[test]
    fn status_message_keeps_a_plain_body() {
        assert_eq!(
            status_body("upstream timed out"),
            "provider status 400: upstream timed out"
        );
    }

    #[test]
    fn status_message_keeps_a_body_with_no_message() {
        assert_eq!(
            status_body(r#"{"detail":"x"}"#),
            r#"provider status 400: {"detail":"x"}"#
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
