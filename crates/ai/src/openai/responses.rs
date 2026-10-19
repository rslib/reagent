//! OpenAI Responses API provider.

pub mod request;
pub mod stream;

use std::sync::Mutex;

use bytes::Bytes;
use futures_util::TryStreamExt;
use futures_util::stream::BoxStream;

use self::request::{MessageBody, RequestBuilder};
use self::stream::StreamParser;
use crate::Error;
use crate::http::ExtraHeaders;
use crate::key::ApiKey;
use crate::openai::BodyCache;
use crate::types::{ReasoningLevel, Text, ToolSpec};
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

/// Talks to the OpenAI Responses API over HTTP.
#[derive(Debug)]
pub struct OpenAiResponses {
    client: reqwest::Client,
    api_key: ApiKey,
    base_url: String,
    headers: ExtraHeaders,
    head: Mutex<Option<Head>>,
    bodies: Mutex<BodyCache<MessageBody>>,
}

#[derive(Debug)]
struct Head {
    model: String,
    max_tokens: u32,
    system: Option<Text>,
    reasoning: Option<ReasoningLevel>,
    cache_key: Option<Text>,
    tools: Vec<ToolSpec>,
    builder: RequestBuilder,
}

impl OpenAiResponses {
    /// Creates a provider for `base_url`, usually `https://api.openai.com/v1`.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: crate::http::client(),
            api_key: ApiKey::new(api_key.into()),
            base_url: base_url.into(),
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

    /// Serializes the request body, reusing the bytes of unchanged messages.
    pub(crate) fn request_body(&self, request: &Request<'_>) -> Result<Bytes, Error> {
        let reasoning = request.reasoning;
        let mut head = self.head.lock().expect("head cache lock");
        let reuse = head.as_ref().is_some_and(|cached| {
            cached.model == request.model
                && cached.max_tokens == request.max_tokens
                && cached.system.as_deref() == request.system.map(|system| &**system)
                && cached.reasoning == reasoning
                && cached.cache_key.as_deref() == request.cache_key.map(|key| &**key)
                && cached.tools.as_slice() == request.tools
        });
        if !reuse {
            *head = Some(Head {
                model: request.model.to_string(),
                max_tokens: request.max_tokens,
                system: request.system.cloned(),
                reasoning,
                cache_key: request.cache_key.cloned(),
                tools: request.tools.to_vec(),
                builder: RequestBuilder::new(
                    request.model,
                    request.max_tokens,
                    request.system.map(|system| &**system),
                    reasoning,
                    request.cache_key.map(|key| &**key),
                    request.tools,
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
        let api_key = self.api_key.reveal().to_owned();
        let client = self.client.clone();
        let headers = self.headers.clone();

        let connect = futures_util::stream::once(async move {
            let request = client
                .post(url)
                .header("authorization", format!("Bearer {api_key}"))
                .header("content-type", "application/json");
            let response = headers
                .apply(request)
                .body(body)
                .send()
                .await
                .map_err(|error| Error::Http(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let retry = crate::Retry::from_headers(response.headers());
                let text = response.text().await.unwrap_or_default();
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
