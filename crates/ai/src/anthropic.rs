//! Anthropic Messages API provider.

pub mod request;
pub mod stream;

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};

use self::request::{BodyCache, RequestBuilder};
use self::stream::StreamParser;
use crate::sse::SseSplitter;
use crate::{Error, Provider, ReasoningLevel, Request, StreamEvent, Text, ToolSpec};

/// The provider name recorded on assistant messages and used to keep thinking.
pub(crate) const PROVIDER: &str = "anthropic";

/// Talks to the Anthropic Messages API over HTTP.
#[derive(Debug)]
pub struct Anthropic {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    head: Mutex<Option<Head>>,
    bodies: Mutex<BodyCache>,
}

#[derive(Debug)]
struct Head {
    model: String,
    max_tokens: u32,
    system: Option<Text>,
    reasoning: Option<ReasoningLevel>,
    tools: Vec<ToolSpec>,
    builder: RequestBuilder,
}

impl Anthropic {
    /// Creates a provider for `base_url`, usually `https://api.anthropic.com`.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            head: Mutex::new(None),
            bodies: Mutex::new(BodyCache::default()),
        }
    }

    pub(crate) fn request_body(&self, request: &Request) -> Result<Bytes, Error> {
        let reasoning = crate::cross_provider::anthropic_reasoning(request);
        let mut head = self.head.lock().expect("head cache lock");
        let reuse = head.as_ref().is_some_and(|cached| {
            cached.model == request.model
                && cached.max_tokens == request.max_tokens
                && cached.system == request.system
                && cached.reasoning == reasoning
                && cached.tools == request.tools
        });
        if !reuse {
            *head = Some(Head {
                model: request.model.clone(),
                max_tokens: request.max_tokens,
                system: request.system.clone(),
                reasoning,
                tools: request.tools.clone(),
                builder: RequestBuilder::new(
                    &request.model,
                    request.max_tokens,
                    request.system.as_deref(),
                    reasoning,
                    &request.tools,
                ),
            });
        }
        let builder = &head.as_ref().expect("head is cached").builder;

        let mut bodies = self.bodies.lock().expect("body cache lock");
        bodies.align(&request.messages, self.name())?;
        Ok(builder.body(bodies.bodies()))
    }
}

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    fn stream(&self, request: &Request) -> BoxStream<'static, Result<StreamEvent, Error>> {
        let body = match self.request_body(request) {
            Ok(body) => body,
            Err(error) => return Box::pin(futures_util::stream::iter([Err(error)])),
        };
        let url = format!("{}/v1/messages", self.base_url);
        let api_key = self.api_key.clone();
        let client = self.client.clone();

        let connect = futures_util::stream::once(async move {
            let response = client
                .post(url)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
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
                        Ok(Some(event)) => out.push(Ok(event)),
                        Ok(None) => {}
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
        parser.lock().expect("parser lock").finish().err().map(Err)
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
        assert_eq!(items.len(), 1, "one error, not an extra finish error");
        assert!(matches!(&items[0], Err(Error::Http(message)) if message == "refused"));
    }
}
