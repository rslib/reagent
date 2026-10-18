//! OpenAI Responses API provider.

pub mod request;
pub mod stream;

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};

use self::request::{MessageBody, RequestBuilder};
use self::stream::StreamParser;
use crate::Error;
use crate::openai::BodyCache;
use crate::sse::SseSplitter;
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
    api_key: String,
    base_url: String,
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
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            head: Mutex::new(None),
            bodies: Mutex::new(BodyCache::default()),
        }
    }

    /// Serializes the request body, reusing the bytes of unchanged messages.
    pub(crate) fn request_body(&self, request: &Request) -> Result<Bytes, Error> {
        let reasoning = request.reasoning;
        let mut head = self.head.lock().expect("head cache lock");
        let reuse = head.as_ref().is_some_and(|cached| {
            cached.model == request.model
                && cached.max_tokens == request.max_tokens
                && cached.system == request.system
                && cached.reasoning == reasoning
                && cached.cache_key == request.cache_key
                && cached.tools == request.tools
        });
        if !reuse {
            *head = Some(Head {
                model: request.model.clone(),
                max_tokens: request.max_tokens,
                system: request.system.clone(),
                reasoning,
                cache_key: request.cache_key.clone(),
                tools: request.tools.clone(),
                builder: RequestBuilder::new(
                    &request.model,
                    request.max_tokens,
                    request.system.as_deref(),
                    reasoning,
                    request.cache_key.as_deref(),
                    &request.tools,
                ),
            });
        }
        let builder = &head.as_ref().expect("head is cached").builder;

        let mut bodies = self.bodies.lock().expect("body cache lock");
        bodies.align(&request.messages, |message| {
            MessageBody::new(message, PROVIDER)
        })?;
        Ok(builder.body(bodies.bodies()))
    }
}

impl Provider for OpenAiResponses {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    fn stream(&self, request: &Request) -> BoxStream<'static, Result<StreamEvent, Error>> {
        let body = match self.request_body(request) {
            Ok(body) => body,
            Err(error) => return Box::pin(futures_util::stream::iter([Err(error)])),
        };
        let url = format!("{}/responses", self.base_url);
        let api_key = self.api_key.clone();
        let client = self.client.clone();

        let connect = futures_util::stream::once(async move {
            let response = client
                .post(url)
                .header("authorization", format!("Bearer {api_key}"))
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .map_err(|error| Error::Http(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(Error::Status(status.as_u16(), text));
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
    let parser = Arc::new(Mutex::new(StreamParser::new()));
    let mut splitter = SseSplitter::new();
    let parser_for_events = Arc::clone(&parser);
    let events = connect.flat_map(move |item| {
        let mut out = Vec::new();
        match item {
            Ok(chunk) => {
                let mut lines = splitter.push(&chunk);
                while let Some(line) = lines.next_line() {
                    match parser_for_events
                        .lock()
                        .expect("parser lock")
                        .push_line(line)
                    {
                        Ok(events) => out.extend(events.into_iter().map(Ok)),
                        Err(error) => out.push(Err(error)),
                    }
                }
            }
            Err(error) => {
                parser_for_events.lock().expect("parser lock").fail();
                out.push(Err(error));
            }
        }
        futures_util::stream::iter(out)
    });

    let tail = futures_util::stream::once(async move {
        match parser.lock().expect("parser lock").finish() {
            Ok(Some(event)) => Some(Ok(event)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    })
    .flat_map(futures_util::stream::iter);

    Box::pin(events.chain(tail))
}

#[cfg(test)]
mod tests {
    use futures_util::FutureExt;

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
}
