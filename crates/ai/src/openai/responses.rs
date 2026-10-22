//! OpenAI Responses API provider.

pub mod request;
pub mod stream;

use std::sync::Mutex;

use bytes::Bytes;
use futures_util::TryStreamExt;
use futures_util::stream::BoxStream;
use serde::Deserialize;

use self::request::{MessageBody, RequestBuilder};
use self::stream::StreamParser;
use crate::Error;
use crate::http::ExtraHeaders;
use crate::key::Credential;
use crate::openai::BodyCache;
use crate::providers::Compat;
use crate::types::{Text, ToolSpec};
use crate::{Provider, Request, StreamEvent};

/// The provider name recorded on assistant messages.
const PROVIDER: &str = "openai";

/// Separator that packs a Responses reasoning item id with its encrypted content.
const REASONING_ID_SEPARATOR: char = '\u{1f}';

/// Packs a reasoning item id and its encrypted content into one opaque signature.
pub(crate) fn pack_reasoning(id: Option<&str>, encrypted_content: &str) -> Text {
    match id {
        Some(id) => Text::from(format!("{id}{REASONING_ID_SEPARATOR}{encrypted_content}")),
        None => Text::from(encrypted_content),
    }
}

/// Splits a signature back into the reasoning item id and the encrypted content.
pub(crate) fn split_reasoning(signature: &str) -> (Option<&str>, &str) {
    match signature.split_once(REASONING_ID_SEPARATOR) {
        Some((id, encrypted_content)) => (Some(id), encrypted_content),
        None => (None, signature),
    }
}

/// The words for a ChatGPT plan error code, when the code is one these words
/// cover. `param` names what a refused capability asked for.
///
/// The words stand alone: no other part of the reply body reaches the user.
pub(crate) fn plan_error(code: &str, param: Option<&str>) -> Option<String> {
    let message = match code {
        "subscription_sharing_usage_limit_exceeded" => {
            "ChatGPT plan usage limit reached; see https://chatgpt.com/settings/usage".to_string()
        }
        "subscription_sharing_user_not_eligible" => {
            "this ChatGPT account cannot use its plan in other apps".to_string()
        }
        "subscription_sharing_invalid_user" => {
            "chatgpt login is no longer valid; run reagent login chatgpt again (add --as <provider> for a named account)".to_string()
        }
        "subscription_sharing_unsupported_capability" => format!(
            "the ChatGPT plan does not support {}",
            param.unwrap_or("this request")
        ),
        "subscription_sharing_route_not_supported"
        | "chatpass_v2_scope_not_authorized"
        | "chatpass_v2_invalid_authorization_context" => {
            format!("ChatGPT plan request refused: {code}")
        }
        _ => return None,
    };
    Some(message)
}

/// The error code a non-2xx reply body names: `error.code`, else `code`.
///
/// The rest of the body is read only to find the code and is not kept.
fn plan_error_of_body(body: &str) -> Option<String> {
    let parsed: ErrorBody<'_> = serde_json::from_str(body).ok()?;
    let error = parsed.error.as_ref();
    let code = error.and_then(|error| error.code).or(parsed.code)?;
    plan_error(code, error.and_then(|error| error.param))
}

/// A non-2xx reply, flattened so no other field of it is buffered.
#[derive(Deserialize)]
struct ErrorBody<'a> {
    #[serde(default, borrow)]
    error: Option<ErrorField<'a>>,
    #[serde(default, borrow)]
    code: Option<&'a str>,
}

#[derive(Deserialize)]
struct ErrorField<'a> {
    #[serde(default, borrow)]
    code: Option<&'a str>,
    #[serde(default, borrow)]
    param: Option<&'a str>,
}

/// Talks to the OpenAI Responses API over HTTP.
#[derive(Debug)]
pub struct OpenAiResponses {
    client: reqwest::Client,
    credential: Result<Credential, String>,
    base_url: String,
    compat: Compat,
    headers: ExtraHeaders,
    head: Mutex<Option<Head>>,
    bodies: Mutex<BodyCache<MessageBody>>,
}

#[derive(Debug)]
struct Head {
    model: String,
    max_tokens: u32,
    system: Option<Text>,
    reasoning: Option<String>,
    cache_key: Option<Text>,
    tools: Vec<ToolSpec>,
    builder: RequestBuilder,
}

impl OpenAiResponses {
    /// Creates a provider for `base_url`, usually `https://api.openai.com/v1`.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: crate::http::client(),
            credential: Credential::bearer(&api_key.into()),
            base_url: base_url.into(),
            compat: Compat::DEFAULT,
            headers: ExtraHeaders::default(),
            head: Mutex::new(None),
            bodies: Mutex::new(BodyCache::default()),
        }
    }

    /// Adds extra request headers to every request.
    ///
    /// A header that [`crate::check_header`] rejects is dropped, so an extra
    /// header never replaces the credential or the body encoding. `Debug`
    /// prints header names only, never values.
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = ExtraHeaders::new(headers);
        self
    }

    /// Replaces how the request proves who sends it, for example with a saved
    /// ChatGPT login that refreshes its token.
    pub fn with_credential(mut self, credential: Credential) -> Self {
        self.credential = Ok(credential);
        self
    }

    /// Replaces how the endpoint differs from the plain wire.
    pub fn with_compat(mut self, compat: Compat) -> Self {
        self.compat = compat;
        self.head = Mutex::new(None);
        self
    }

    /// Serializes the request body, reusing the bytes of unchanged messages.
    pub(crate) fn request_body(&self, request: &Request<'_>) -> Result<Bytes, Error> {
        let mut head = self.head.lock().expect("head cache lock");
        let reuse = head.as_ref().is_some_and(|cached| {
            cached.model == request.model
                && cached.max_tokens == request.max_tokens
                && cached.system.as_deref() == request.system.map(|system| &**system)
                && cached.reasoning.as_deref() == request.reasoning
                && cached.cache_key.as_deref() == request.cache_key.map(|key| &**key)
                && cached.tools.as_slice() == request.tools
        });
        if !reuse {
            *head = Some(Head {
                model: request.model.to_string(),
                max_tokens: request.max_tokens,
                system: request.system.cloned(),
                reasoning: request.reasoning.map(str::to_string),
                cache_key: request.cache_key.cloned(),
                tools: request.tools.to_vec(),
                builder: RequestBuilder::with_compat(
                    request.model,
                    request.max_tokens,
                    request.system.map(|system| &**system),
                    request.reasoning,
                    request.cache_key.map(|key| &**key),
                    request.tools,
                    self.compat,
                ),
            });
        }
        let builder = &head.as_ref().expect("head is cached").builder;

        let mut bodies = self.bodies.lock().expect("body cache lock");
        bodies.align(request.messages, |message| {
            MessageBody::new(message, PROVIDER)
        })?;
        Ok(builder.body(bodies.bodies()))
    }
}

impl Provider for OpenAiResponses {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    fn stream(&self, request: &Request<'_>) -> BoxStream<'static, Result<StreamEvent, Error>> {
        let body = match self.request_body(request) {
            Ok(body) => body,
            Err(error) => return Box::pin(futures_util::stream::iter([Err(error)])),
        };
        let url = format!("{}/responses", self.base_url);
        let client = self.client.clone();
        let headers = self.headers.clone();
        let credential = self.credential.clone();

        let connect = futures_util::stream::once(async move {
            let (key_name, key_value) = match credential {
                Ok(credential) => credential.header().await.map_err(Error::Protocol)?,
                Err(message) => return Err(Error::Protocol(message)),
            };
            let request = client
                .post(url)
                .header(key_name, key_value)
                .header("content-type", "application/json");
            let response = headers
                .apply(request)
                .body(body)
                .send()
                .await
                .map_err(|error| Error::Http(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let mut retry = crate::Retry::from_headers(response.headers());
                let text = response.text().await.unwrap_or_default();
                // A plan error says what to do about it, and a retry spends the
                // same limit again, so the provider's verdict is overridden.
                if let Some(message) = plan_error_of_body(&text) {
                    retry.should = Some(false);
                    return Err(Error::Status {
                        code: status.as_u16(),
                        body: message,
                        retry,
                    });
                }
                return Err(Error::Status {
                    code: status.as_u16(),
                    body: text,
                    retry,
                });
            }
            let bytes = response
                .bytes_stream()
                .map_err(|error| Error::Http(error.to_string()));
            Ok(bytes)
        })
        .try_flatten();

        parse_stream(connect)
    }
}

/// Turns response bytes into events, with at most one error per stream.
fn parse_stream<S>(connect: S) -> BoxStream<'static, Result<StreamEvent, Error>>
where
    S: futures_util::Stream<Item = Result<Bytes, Error>> + Send + 'static,
{
    crate::openai::event_stream(connect, StreamParser::new())
}

#[cfg(test)]
mod tests {
    use futures_util::{FutureExt, StreamExt};

    use super::*;

    #[test]
    fn a_transport_error_is_the_only_error() {
        let connect = futures_util::stream::iter([Err(Error::Http("refused".into()))]);
        let items: Vec<_> = parse_stream(connect)
            .collect::<Vec<_>>()
            .now_or_never()
            .unwrap();
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], Err(Error::Http(message)) if message == "refused"));
    }

    const EXTRA_REPLY: &str = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";

    /// The error one request gets from a server that answers `status` with
    /// `body`, and whether the loop would retry it.
    async fn plan_error_reply(
        status: u16,
        body: &'static str,
    ) -> (String, Option<Option<std::time::Duration>>) {
        let (base, _recorded) = crate::oauth::test_server::serve(status, body).await;
        let provider = OpenAiResponses::new("test-key", base);
        let request = crate::Request {
            model: "gpt-x",
            max_tokens: 16,
            system: None,
            tools: &[],
            messages: &[],
            reasoning: None,
            cache_key: None,
        };
        let mut events = provider.stream(&request);
        let error = events
            .next()
            .await
            .expect("one item")
            .expect_err("the request fails");
        (error.to_string(), error.retry_hint())
    }

    #[tokio::test]
    async fn plan_error_usage_limit_is_not_retried_and_names_the_usage_page() {
        let (message, retry) = plan_error_reply(
            429,
            r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"detail"}}"#,
        )
        .await;
        assert!(
            message.contains("https://chatgpt.com/settings/usage"),
            "{message}"
        );
        assert!(!message.contains("detail"), "{message}");
        assert_eq!(retry, None);
    }

    #[tokio::test]
    async fn plan_error_account_cannot_use_its_plan_in_other_apps() {
        let (message, retry) = plan_error_reply(
            403,
            r#"{"error":{"code":"subscription_sharing_user_not_eligible"}}"#,
        )
        .await;
        assert!(
            message.contains("this ChatGPT account cannot use its plan in other apps"),
            "{message}"
        );
        assert_eq!(retry, None);
    }

    #[tokio::test]
    async fn plan_error_invalid_user_asks_for_a_new_login() {
        let (message, retry) =
            plan_error_reply(401, r#"{"code":"subscription_sharing_invalid_user"}"#).await;
        assert!(
            message.contains("chatgpt login is no longer valid; run reagent login chatgpt"),
            "{message}"
        );
        assert_eq!(retry, None);
    }

    #[tokio::test]
    async fn plan_error_unavailable_is_retried_like_any_server_error() {
        let (message, retry) = plan_error_reply(
            503,
            r#"{"error":{"code":"subscription_sharing_usage_unavailable"}}"#,
        )
        .await;
        assert!(!message.is_empty());
        assert_eq!(retry, Some(None), "a 503 is retried: {message}");
    }

    #[tokio::test]
    async fn extra_headers_reach_the_responses_request() {
        let (base, recorded) = crate::http::test_server::serve(EXTRA_REPLY).await;
        let provider = OpenAiResponses::new("test-key", base)
            .with_headers(vec![("x-extra".into(), "yes".into())]);
        let request = crate::Request {
            model: "test-model",
            max_tokens: 16,
            system: None,
            tools: &[],
            messages: &[],
            reasoning: None,
            cache_key: None,
        };
        let events: Vec<_> = provider.stream(&request).collect().await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let recorded = recorded.await.expect("the server recorded the request");
        assert_eq!(recorded.request_line, "POST /responses HTTP/1.1");
        assert_eq!(recorded.header("x-extra"), Some("yes"));
        assert_eq!(recorded.header("authorization"), Some("Bearer test-key"));
    }
}
