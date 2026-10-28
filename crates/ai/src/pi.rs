//! The pi-messages wire: one JSON context per request and a signature replay.
//!
//! A request carries the whole conversation as one `context` object, so every
//! message is serialized once and cached, and a turn reuses the bytes of the
//! messages before it. Signatures that a reply needs back on the next turn
//! travel in [`crate::AssistantMessage::replay`], which this wire writes and
//! reads.

pub mod request;
pub mod stream;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Mutex;

use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};
use serde::Deserialize;

use crate::Error;
use crate::http::ExtraHeaders;
use crate::key::Credential;
use crate::openai::{BodyCache, BodyParts, JsonStr};
use crate::types::{Text, ToolSpec};
use crate::{Provider, Request, StreamEvent};

use self::request::MessageBody;
use self::stream::StreamParser;

/// The provider name recorded on assistant messages.
const PROVIDER: &str = "pi-messages";

/// The `api` field of every assistant message this wire sends back.
const API: &str = "pi-messages";

/// The `provider` field of every assistant message this wire sends back.
const UPSTREAM: &str = "radius";

/// Talks to a pi-messages endpoint over HTTP.
#[derive(Debug)]
pub struct PiMessages {
    client: reqwest::Client,
    credential: Result<Credential, String>,
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
    reasoning: Option<String>,
    session: Option<Text>,
    tools: Vec<ToolSpec>,
    /// Whether images are sent. A change rebuilds every message body.
    images: bool,
    builder: request::RequestBuilder,
}

impl PiMessages {
    /// Creates a provider for `base_url`, for example `https://radius.pi.dev/v1`.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: crate::http::client(),
            credential: Credential::bearer(&api_key.into()),
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

    /// Replaces how the request proves who sends it.
    pub fn with_credential(mut self, credential: Credential) -> Self {
        self.credential = Ok(credential);
        self
    }

    /// The URL every request goes to.
    pub fn url(&self) -> String {
        format!("{}/messages", self.base_url)
    }

    /// Serializes the request body, reusing the bytes of unchanged messages.
    pub(crate) fn request_parts(&self, request: &Request<'_>) -> Result<BodyParts, Error> {
        let mut head = self.head.lock().expect("head cache lock");
        let reuse = head.as_ref().is_some_and(|cached| {
            cached.model == request.model
                && cached.max_tokens == request.max_tokens
                && cached.system.as_deref() == request.system.map(|system| &**system)
                && cached.reasoning.as_deref() == request.reasoning
                && cached.session.as_deref() == request.cache_key.map(|key| &**key)
                && cached.tools.as_slice() == request.tools
                && cached.images == request.images
        });
        if !reuse {
            *head = Some(Head {
                model: request.model.to_string(),
                max_tokens: request.max_tokens,
                system: request.system.cloned(),
                reasoning: request.reasoning.map(str::to_string),
                session: request.cache_key.cloned(),
                tools: request.tools.to_vec(),
                images: request.images,
                builder: request::RequestBuilder::new(
                    request.model,
                    request.max_tokens,
                    request.system.map(|system| &**system),
                    request.reasoning,
                    request.cache_key.map(|key| &**key),
                    request.tools,
                ),
            });
        }
        let builder = &head.as_ref().expect("head is cached").builder;

        let names = request::tool_names(request.messages);
        let images = request.images;
        let mut bodies = self.bodies.lock().expect("body cache lock");
        if !reuse {
            // A body holds the images of its message or the note that replaces
            // them, so a new head rebuilds all of them.
            *bodies = BodyCache::default();
        }
        bodies.align_at(request.messages, |index, message| {
            MessageBody::with_images(message, PROVIDER, &names, builder.timestamp(index), images)
        })?;
        Ok(builder.body_parts(bodies.bodies()))
    }

    /// The request body as one buffer, for the tests and the benches.
    #[cfg(test)]
    pub(crate) fn request_body(&self, request: &Request<'_>) -> Result<Bytes, Error> {
        self.request_parts(request).map(|parts| parts.join())
    }
}

impl Provider for PiMessages {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    fn stream(&self, request: &Request<'_>) -> BoxStream<'static, Result<StreamEvent, Error>> {
        let body = match self.request_parts(request) {
            Ok(body) => body,
            Err(error) => return Box::pin(futures_util::stream::iter([Err(error)])),
        };
        let length = body.len();
        let url = self.url();
        let client = self.client.clone();
        let headers = self.headers.clone();
        let credential = self.credential.clone();
        let reasoning = request.reasoning.map(str::to_string);

        let connect = futures_util::stream::once(async move {
            let (key_name, key_value) = match credential {
                Ok(credential) => credential.header().await.map_err(Error::Protocol)?,
                Err(message) => return Err(Error::Protocol(message)),
            };
            let request = client
                .post(url)
                .header(key_name, key_value)
                .header("accept", "text/event-stream")
                .header("content-type", "application/json");
            let response = headers
                .apply(request)
                // The body is streamed in pieces, so its length is stated here
                // and the request is not chunked.
                .header(reqwest::header::CONTENT_LENGTH, length)
                .body(body.into_body())
                .send()
                .await
                .map_err(|error| Error::Http(error.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let retry = crate::Retry::from_headers(response.headers());
                let text = response.text().await.unwrap_or_default();
                return Err(Error::Status {
                    code: status.as_u16(),
                    body: status_message(&text),
                    retry,
                });
            }
            let bytes = response
                .bytes_stream()
                .map_err(|error| Error::Http(error.to_string()));
            Ok(bytes)
        })
        .try_flatten();

        crate::openai::event_stream(connect, StreamParser::new(reasoning.as_deref()))
    }
}

/// The message of a non-success body, which this endpoint reports as
/// `{"error":{"message":..,"code":..}}`, or the whole body when it does not
/// follow that shape.
fn status_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct Reported<'a> {
        #[serde(default, borrow)]
        error: Option<Detail<'a>>,
    }

    #[derive(Deserialize)]
    struct Detail<'a> {
        #[serde(default, borrow)]
        message: Option<&'a str>,
    }

    match serde_json::from_str::<Reported>(body) {
        Ok(Reported {
            error: Some(Detail {
                message: Some(message),
            }),
        }) => message.to_string(),
        _ => body.to_string(),
    }
}

/// Runs `chunks` through the real stream glue and this wire's parser, as if
/// they came from the socket, and returns every item the stream yields. For
/// the tests and the allocation counts that need the parser alone.
pub fn parse_chunks(chunks: Vec<Bytes>) -> Vec<Result<StreamEvent, Error>> {
    use futures_util::FutureExt;

    let input = futures_util::stream::iter(chunks.into_iter().map(Ok));
    let events = crate::openai::event_stream(input, StreamParser::new(None));
    let items = events.collect::<Vec<_>>();
    items
        .now_or_never()
        .expect("chunks from memory are always ready")
}

/// The fields of an assistant reply that the next request sends back, as the
/// one JSON object the parser wrote into [`crate::AssistantMessage::replay`].
///
/// A string that arrived with an escape is unescaped once into its own text
/// (see [`JsonStr`]): a signature is a JSON object of its own, so every one of
/// them carries quotes and backslashes that a borrowed `&str` cannot hold.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Replay<'a> {
    #[serde(default, rename = "responseId", borrow)]
    pub(crate) response_id: Option<JsonStr<'a>>,
    #[serde(default, rename = "providerThinkingLevel", borrow)]
    pub(crate) thinking_level: Option<JsonStr<'a>>,
    /// The signature of each text block, by text block ordinal.
    #[serde(default, rename = "textSignatures", borrow)]
    pub(crate) text_signatures: BTreeMap<u32, JsonStr<'a>>,
    /// The signature of each tool call, by call id.
    #[serde(default, rename = "toolSignatures", borrow)]
    pub(crate) tool_signatures: BTreeMap<Cow<'a, str>, JsonStr<'a>>,
}
