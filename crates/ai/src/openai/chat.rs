//! OpenAI-compatible Chat Completions provider.

use std::borrow::Cow;
use std::sync::Mutex;

use bytes::{BufMut, Bytes, BytesMut};
use futures_util::TryStreamExt;
use futures_util::stream::BoxStream;
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, value::RawValue};

use crate::Error;
use crate::http::ExtraHeaders;
use crate::key::Credential;
use crate::openai::{BodyCache, BodyParts, JsonStr};
use crate::providers::{Compat, MaxTokensField, ReasoningField};
use crate::request_images::{NO_IMAGES, data_url};
use crate::types::{
    AssistantMessage, Content, Image, Message, Reasoning, Role, StopReason, Text, ToolResult,
    ToolSpec,
};
use crate::{Provider, Request, StreamEvent, Usage};

/// The provider name recorded on assistant messages.
const PROVIDER: &str = "openai-chat";

/// Talks to an OpenAI-compatible Chat Completions endpoint over HTTP.
#[derive(Debug)]
pub struct OpenAiChat {
    client: reqwest::Client,
    credential: Result<Credential, String>,
    base_url: String,
    compat: Compat,
    headers: ExtraHeaders,
    session: Option<(HeaderName, HeaderValue)>,
    head: Mutex<Option<Head>>,
    bodies: Mutex<BodyCache<MessageBody>>,
}

#[derive(Debug)]
struct Head {
    model: String,
    max_tokens: u32,
    system: Option<Text>,
    reasoning: Option<String>,
    tools: Vec<ToolSpec>,
    /// Whether images are sent. A change rebuilds every message body.
    images: bool,
    builder: RequestBuilder,
}

impl OpenAiChat {
    /// Creates a provider for `base_url`, for example `http://localhost:11434/v1`.
    ///
    /// The cap field is `max_completion_tokens` for `api.openai.com` and
    /// `max_tokens` elsewhere, until [`OpenAiChat::with_compat`] sets it.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        let compat = Compat {
            max_tokens_field: if base_url.contains("api.openai.com") {
                MaxTokensField::MaxCompletionTokens
            } else {
                MaxTokensField::MaxTokens
            },
            ..Compat::DEFAULT
        };
        Self {
            client: crate::http::client(),
            credential: Credential::bearer(&api_key.into()),
            base_url,
            compat,
            headers: ExtraHeaders::default(),
            session: None,
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
    ///
    /// A compat that names a session header gets one id for the life of this
    /// provider value, so every request of the run carries the same id and the
    /// prompt cache stays warm.
    pub fn with_compat(mut self, compat: Compat) -> Self {
        self.compat = compat;
        self.session = compat.session_header.and_then(session_header);
        self.head = Mutex::new(None);
        self
    }

    /// The Chat Completions URL every request goes to.
    pub fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// Serializes the request body, reusing the bytes of unchanged messages.
    pub(crate) fn request_parts(&self, request: &Request<'_>) -> Result<BodyParts, Error> {
        let mut head = self.head.lock().expect("head cache lock");
        let reuse = head.as_ref().is_some_and(|cached| {
            cached.model == request.model
                && cached.max_tokens == request.max_tokens
                && cached.system.as_deref() == request.system.map(|system| &**system)
                && cached.reasoning.as_deref() == request.reasoning
                && cached.tools.as_slice() == request.tools
                && cached.images == request.images
        });
        if !reuse {
            *head = Some(Head {
                model: request.model.to_string(),
                max_tokens: request.max_tokens,
                system: request.system.cloned(),
                reasoning: request.reasoning.map(str::to_string),
                tools: request.tools.to_vec(),
                images: request.images,
                builder: RequestBuilder::with_compat(
                    request.model,
                    request.max_tokens,
                    request.system.map(|system| &**system),
                    request.reasoning,
                    request.tools,
                    self.compat,
                ),
            });
        }
        let builder = &head.as_ref().expect("head is cached").builder;

        let mut bodies = self.bodies.lock().expect("body cache lock");
        if !reuse {
            // A body holds the images of its message or the note that replaces
            // them, so a new head rebuilds all of them.
            *bodies = BodyCache::default();
        }
        let images = request.images;
        bodies.align(request.messages, |message| {
            MessageBody::with_images(message, PROVIDER, images)
        })?;
        let mut parts = builder.body_parts(bodies.bodies());
        if request.tool_choice == crate::ToolChoice::Off && !request.tools.is_empty() {
            parts.lead_with(br#""tool_choice":"none","#);
        }
        Ok(parts)
    }

    /// The request body as one buffer, for the tests and the cross-provider
    /// comparisons that need the bytes in one place.
    #[cfg(test)]
    pub(crate) fn request_body(&self, request: &Request<'_>) -> Result<Bytes, Error> {
        self.request_parts(request).map(|parts| parts.join())
    }
}

impl Provider for OpenAiChat {
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
        let session = self.session.clone();
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
            let request = match &session {
                Some((name, value)) if !headers.has(name) => request.header(name, value),
                _ => request,
            };
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

/// One message serialized in Chat Completions wire form, reusable across requests.
#[derive(Debug, Clone)]
pub struct MessageBody {
    bytes: Option<Bytes>,
    /// The role of the message it was built from.
    role: Role,
    /// The image parts a tool result adds to the user message that follows the
    /// tool messages of its turn, in call order. Empty when it has none.
    images: Vec<Bytes>,
}

impl MessageBody {
    /// Serializes one message for the provider named `provider`.
    ///
    /// Thinking from another provider is dropped. Tool arguments are passed
    /// through as the raw JSON string the model emitted; an empty argument
    /// string becomes an empty object. An assistant message with nothing kept
    /// produces no body at all. Images are sent.
    pub fn new(message: &Message, provider: &str) -> Result<Self, Error> {
        Self::with_images(message, provider, true)
    }

    /// Serializes one message for the provider named `provider`, sending its
    /// images only when `images` and writing the note of
    /// [`crate::request_images::NO_IMAGES`] in their place when not.
    pub fn with_images(message: &Message, provider: &str, images: bool) -> Result<Self, Error> {
        let (bytes, parts) = match message {
            Message::User { content } => (
                Some(to_bytes(&UserOut {
                    role: "user",
                    content: user_parts(content, images),
                })),
                Vec::new(),
            ),
            Message::Assistant(assistant) => (assistant_message(assistant, provider), Vec::new()),
            Message::ToolResult(result) => (
                Some(to_bytes(&ToolOut {
                    role: "tool",
                    tool_call_id: &result.call_id,
                    content: &tool_content(result, images),
                })),
                if images {
                    image_parts(&result.images)
                } else {
                    Vec::new()
                },
            ),
        };
        Ok(Self {
            bytes,
            role: message.role(),
            images: parts,
        })
    }
}

/// The content parts of one user message: its text, and each image as a
/// `data:` URL, or the note that says why the image is not sent.
fn user_parts(content: &[Content], images: bool) -> Vec<Part<'_>> {
    content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(Part::Text {
                text: text.as_ref(),
            }),
            Content::Image(image) if images => Some(image_part(image)),
            Content::Image(_) => Some(Part::Text { text: NO_IMAGES }),
            _ => None,
        })
        .collect()
}

/// The image parts of `images`, in call order, which the user message after
/// the tool messages of a turn writes.
fn image_parts(images: &[Image]) -> Vec<Bytes> {
    images
        .iter()
        .map(|image| to_bytes(&image_part(image)))
        .collect()
}

fn image_part(image: &Image) -> Part<'_> {
    Part::ImageUrl {
        image_url: ImageUrl {
            url: Cow::Owned(data_url(image)),
        },
    }
}

/// The text of one tool result: its text, and the note for every image the
/// model takes none of, because the tool message itself writes text only.
fn tool_content(result: &ToolResult, images: bool) -> Cow<'_, str> {
    if images || result.images.is_empty() {
        return Cow::Borrowed(result.content.as_ref());
    }
    let mut text =
        String::with_capacity(result.content.len() + result.images.len() * (NO_IMAGES.len() + 1));
    text.push_str(&result.content);
    for _ in &result.images {
        text.push('\n');
        text.push_str(NO_IMAGES);
    }
    Cow::Owned(text)
}

/// Builds a Chat Completions body from a stable head and reusable messages.
///
/// The head holds the model, the reasoning level, the token cap, the tools and
/// the system prompt. Tools are sorted by name.
#[derive(Debug)]
pub struct RequestBuilder {
    head: Bytes,
    system: Option<Bytes>,
}

impl RequestBuilder {
    /// Serializes the request head once, for the plain wire.
    pub fn new(
        model: &str,
        max_tokens: u32,
        system: Option<&str>,
        reasoning: Option<&str>,
        tools: &[ToolSpec],
        max_tokens_field: MaxTokensField,
        reasoning_field: ReasoningField,
    ) -> Self {
        Self::with_compat(
            model,
            max_tokens,
            system,
            reasoning,
            tools,
            Compat {
                max_tokens_field,
                reasoning_field,
                ..Compat::DEFAULT
            },
        )
    }

    /// Serializes the request head once, for an endpoint described by `compat`.
    pub fn with_compat(
        model: &str,
        max_tokens: u32,
        system: Option<&str>,
        reasoning: Option<&str>,
        tools: &[ToolSpec],
        compat: Compat,
    ) -> Self {
        let reasoning_field = compat.reasoning_field;
        let reasoning_off = compat.reasoning_off;
        let mut out = BytesMut::new();
        out.put_slice(b"{\"model\":");
        out.put_slice(json_string(model).as_bytes());
        if let Some(text) = reasoning {
            let reasoning = Reasoning::parse(text);
            if reasoning.is_off() {
                if let Some(word) = reasoning_off {
                    match reasoning_field {
                        ReasoningField::Effort => {
                            out.put_slice(b",\"reasoning_effort\":");
                            out.put_slice(json_string(word).as_bytes());
                        }
                        ReasoningField::EffortObject => {
                            out.put_slice(b",\"reasoning\":{\"effort\":");
                            out.put_slice(json_string(word).as_bytes());
                            out.put_slice(b"}");
                        }
                    }
                }
            } else {
                match reasoning_field {
                    ReasoningField::Effort => {
                        let effort = reasoning.text();
                        out.put_slice(b",\"reasoning_effort\":");
                        out.put_slice(json_string(&effort).as_bytes());
                    }
                    ReasoningField::EffortObject => {
                        out.put_slice(b",\"reasoning\":");
                        match reasoning {
                            Reasoning::Budget(budget) => {
                                out.put_slice(b"{\"max_tokens\":");
                                out.put_slice(budget.to_string().as_bytes());
                                out.put_slice(b"}");
                            }
                            Reasoning::Level(level) => {
                                out.put_slice(b"{\"effort\":");
                                out.put_slice(json_string(level).as_bytes());
                                out.put_slice(b"}");
                            }
                        }
                    }
                }
            }
        }
        match compat.max_tokens_field {
            MaxTokensField::MaxCompletionTokens => {
                out.put_slice(b",\"max_completion_tokens\":");
            }
            MaxTokensField::MaxTokens => out.put_slice(b",\"max_tokens\":"),
            MaxTokensField::Omit => {}
        }
        if compat.max_tokens_field != MaxTokensField::Omit {
            out.put_slice(max_tokens.to_string().as_bytes());
        }
        out.put_slice(b",\"stream\":true,\"stream_options\":{\"include_usage\":true}");
        if tools.is_empty() {
            out.put_slice(b",\"messages\":[");
        } else {
            out.put_slice(b",\"tools\":[");
            let mut sorted: Vec<&ToolSpec> = tools.iter().collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            for (index, tool) in sorted.iter().enumerate() {
                if index > 0 {
                    out.put_u8(b',');
                }
                out.put_slice(&to_bytes(&ChatTool {
                    kind: "function",
                    function: ChatFunction {
                        name: &tool.name,
                        description: &tool.description,
                        parameters: &tool.input_schema,
                    },
                }));
            }
            out.put_slice(b"],\"messages\":[");
        }
        Self {
            head: out.freeze(),
            system: system.map(|system| {
                to_bytes(&SystemOut {
                    role: "system",
                    content: system,
                })
            }),
        }
    }

    /// The prefix every body built from this request shares.
    pub fn head(&self) -> &Bytes {
        &self.head
    }

    /// Serializes a body from the head and the message bodies.
    pub fn body(&self, messages: &[MessageBody]) -> Bytes {
        self.body_parts(messages).join()
    }

    /// The body as its pieces: the head, the system prompt, each cached message
    /// and the separators between them, with no message copied.
    pub fn body_parts(&self, messages: &[MessageBody]) -> BodyParts {
        let visible = messages
            .iter()
            .filter(|message| message.bytes.is_some())
            .count();
        let images: usize = messages.iter().map(|message| message.images.len()).sum();
        let mut parts = BodyParts::with_capacity(
            2 + usize::from(self.system.is_some())
                + crate::openai::item_pieces(visible)
                + image_message_pieces(images),
        );
        parts.push(self.head.clone());
        let mut first = match &self.system {
            Some(system) => {
                parts.push(system.clone());
                false
            }
            None => true,
        };
        let mut index = 0;
        while index < messages.len() {
            if messages[index].role != Role::ToolResult {
                if let Some(bytes) = &messages[index].bytes {
                    if !first {
                        parts.push_static(COMMA);
                    }
                    first = false;
                    parts.push(bytes.clone());
                }
                index += 1;
                continue;
            }
            // The tool messages of one turn, and then one user message with the
            // images they carry, which the tool messages themselves cannot hold.
            let start = index;
            let mut carried = 0;
            while index < messages.len() && messages[index].role == Role::ToolResult {
                let message = &messages[index];
                carried += message.images.len();
                if let Some(bytes) = &message.bytes {
                    if !first {
                        parts.push_static(COMMA);
                    }
                    first = false;
                    parts.push(bytes.clone());
                }
                index += 1;
            }
            if carried == 0 {
                continue;
            }
            if !first {
                parts.push_static(COMMA);
            }
            first = false;
            parts.push_static(IMAGES_PREFIX);
            for part in messages[start..index]
                .iter()
                .flat_map(|message| &message.images)
            {
                parts.push_static(COMMA);
                parts.push(part.clone());
            }
            parts.push_static(IMAGES_END);
        }
        parts.push_static(END);
        debug_assert_eq!(
            parts.len(),
            self.body_len(messages),
            "the pieces hold the body the buffer held"
        );
        parts
    }

    /// The length of the body [`RequestBuilder::body_parts`] appends, so the
    /// pieces hold the bytes the buffer held, which a debug build checks.
    fn body_len(&self, messages: &[MessageBody]) -> usize {
        let mut len = self.head.len() + END.len();
        let mut first = true;
        if let Some(system) = &self.system {
            len += system.len();
            first = false;
        }
        let mut index = 0;
        while index < messages.len() {
            if messages[index].role != Role::ToolResult {
                if let Some(bytes) = &messages[index].bytes {
                    len += bytes.len() + usize::from(!first);
                    first = false;
                }
                index += 1;
                continue;
            }
            let start = index;
            let mut carried = 0;
            while index < messages.len() && messages[index].role == Role::ToolResult {
                let message = &messages[index];
                carried += message.images.len();
                if let Some(bytes) = &message.bytes {
                    len += bytes.len() + usize::from(!first);
                    first = false;
                }
                index += 1;
            }
            if carried == 0 {
                continue;
            }
            len += IMAGES_PREFIX.len() + IMAGES_END.len() + usize::from(!first) + carried;
            first = false;
            for part in messages[start..index]
                .iter()
                .flat_map(|message| &message.images)
            {
                len += part.len();
            }
        }
        len
    }
}

/// The pieces a user message of tool-result images takes: the comma before it,
/// the prefix that opens it, a comma before every image, and the bracket that
/// closes it.
fn image_message_pieces(images: usize) -> usize {
    if images == 0 { 0 } else { 2 * images + 3 }
}

const COMMA: &[u8] = b",";
const END: &[u8] = b"]}";
/// The user message that carries the images of the tool results above it. It
/// closes with [`IMAGES_END`], and its content starts with one text part.
const IMAGES_PREFIX: &[u8] = b"{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Images from the tool results above:\"}";
const IMAGES_END: &[u8] = b"]}";

#[derive(Serialize)]
struct ChatTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatFunction<'a>,
}

#[derive(Serialize)]
struct ChatFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Serialize)]
struct SystemOut<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct UserOut<'a> {
    role: &'static str,
    content: Vec<Part<'a>>,
}

/// One content part of a user message: text, or an image as a `data:` URL.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: ImageUrl<'a> },
}

/// The `image_url` object of one image part.
#[derive(Serialize)]
struct ImageUrl<'a> {
    url: Cow<'a, str>,
}

#[derive(Serialize)]
struct ToolOut<'a> {
    role: &'static str,
    tool_call_id: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct AssistantOut<'a> {
    role: &'static str,
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    /// The thinking signature replayed as the provider's own JSON array, so a
    /// signed reasoning item survives unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_details: Option<&'a RawValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCallOut<'a>>,
}

#[derive(Serialize)]
struct ToolCallOut<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionCallOut<'a>,
}

#[derive(Serialize)]
struct FunctionCallOut<'a> {
    name: &'a str,
    arguments: &'a str,
}

fn assistant_message(assistant: &AssistantMessage, provider: &str) -> Option<Bytes> {
    let keep_thinking = assistant.provider.as_ref() == provider;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut details: Option<&RawValue> = None;
    let mut tool_calls = Vec::new();
    for content in &assistant.content {
        match content {
            Content::Text { text: value } => text.push_str(value),
            Content::Thinking {
                thinking,
                signature,
            } => {
                if keep_thinking {
                    reasoning.push_str(thinking);
                    if details.is_none() {
                        details = signature.as_deref().and_then(details_of);
                    }
                }
            }
            Content::RedactedThinking { .. } => {}
            // A model's reply holds no image, and the wire has no place for
            // one in an assistant message.
            Content::Image(_) => {}
            Content::ToolCall(call) => tool_calls.push(ToolCallOut {
                id: &call.id,
                kind: "function",
                function: FunctionCallOut {
                    name: &call.name,
                    arguments: if call.arguments.trim().is_empty() {
                        "{}"
                    } else {
                        call.arguments.as_ref()
                    },
                },
            }),
        }
    }
    if text.is_empty() && reasoning.is_empty() && tool_calls.is_empty() && details.is_none() {
        return None;
    }
    let content = (!text.is_empty()).then_some(text.as_str());
    let reasoning_content = (!reasoning.is_empty()).then_some(reasoning);
    Some(to_bytes(&AssistantOut {
        role: "assistant",
        content,
        reasoning_content,
        reasoning_details: details,
        tool_calls,
    }))
}

/// The provider's reasoning details, when `signature` holds a JSON array of
/// them. Any other signature is not replayable and is skipped.
fn details_of(signature: &str) -> Option<&RawValue> {
    let details: &RawValue = serde_json::from_str(signature).ok()?;
    details.get().starts_with('[').then_some(details)
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a str serializes to a JSON string")
}

fn to_bytes(value: &impl Serialize) -> Bytes {
    Bytes::from(serde_json::to_vec(value).expect("request parts are serializable"))
}

/// Parses one Chat Completions SSE stream, line by line.
///
/// Tool call deltas carry an index instead of an id, so the parser maps each
/// index to the call id it first saw. Event fields borrow from the line handed
/// to [`StreamParser::push_line`]. Reasoning details leave as one signature
/// when the stream ends; text and summary pieces merge into one item first.
#[derive(Debug, Default)]
pub struct StreamParser {
    usage: Option<Usage>,
    tool_ids: Vec<(u32, Text)>,
    reasoning_details: Vec<ReasoningDetail>,
    stop_reason: Option<StopReason>,
    done: bool,
    failed: bool,
}

impl StreamParser {
    /// Creates a parser at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one SSE line and appends the events it carries to `out`.
    pub fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        if self.failed {
            return Ok(());
        }
        let Some(data) = data_of(line) else {
            return Ok(());
        };
        if data == b"[DONE]" {
            self.done = true;
            if let Some(usage) = self.usage.take() {
                out.push(StreamEvent::Usage(usage));
            }
            if let Some(signature) = self.take_details() {
                out.push(StreamEvent::ThinkingSignature(signature));
            }
            out.push(StreamEvent::Stop(
                self.stop_reason.take().unwrap_or(StopReason::EndTurn),
            ));
            return Ok(());
        }
        let data = match std::str::from_utf8(data) {
            Ok(data) => data,
            Err(error) => {
                self.failed = true;
                return Err(Error::Protocol(error.to_string()));
            }
        };
        let chunk: Chunk<'_> = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(error) => {
                self.failed = true;
                return Err(Error::Json(error));
            }
        };
        if let Some(error) = chunk.error {
            self.failed = true;
            let message = error.message.as_str().to_string();
            if error.kind.is_some_and(retryable) {
                return Err(Error::Transient(message));
            }
            return Err(Error::Protocol(message));
        }

        for choice in chunk.choices.into_choices() {
            let delta = choice.delta;
            if let Some(reasoning) = delta.reasoning_content.or(delta.reasoning)
                && !reasoning.as_str().is_empty()
            {
                out.push(StreamEvent::ThinkingDelta(reasoning.into_text()));
            }
            if let Some(details) = delta.reasoning_details {
                for detail in details {
                    self.accept_detail(detail);
                }
            }
            if let Some(content) = delta.content
                && !content.as_str().is_empty()
            {
                out.push(StreamEvent::TextDelta(content.into_text()));
            }
            for call in delta.tool_calls {
                self.accept_tool_call(call, out);
            }
            if let Some(reason) = choice.finish_reason {
                if matches!(reason, "tool_calls" | "function_call") {
                    for (_, id) in &self.tool_ids {
                        out.push(StreamEvent::ToolCallEnd { id: id.clone() });
                    }
                }
                self.stop_reason = Some(stop_reason(reason));
            }
        }
        // Some gateways send a running total in every chunk, so only the
        // last one is sent, at `[DONE]`.
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage_of(usage));
        }
        Ok(())
    }

    /// Ends the stream after an error from outside the parser, such as HTTP.
    pub fn fail(&mut self) {
        self.failed = true;
    }

    /// Fails when the stream ended without its `[DONE]` sentinel.
    ///
    /// After an earlier error the stream is over and this is quiet.
    pub fn finish(&self) -> Result<(), Error> {
        if self.done || self.failed {
            Ok(())
        } else {
            // A cut stream, not a bad one: the same request may finish on a retry.
            Err(Error::Transient("stream ended before [DONE]".into()))
        }
    }

    fn accept_tool_call(&mut self, call: ToolCallDelta<'_>, out: &mut Vec<StreamEvent>) {
        let index = match call.index {
            Some(index) => index,
            None => match call.id.as_ref().map(JsonStr::as_str) {
                Some(id) => self
                    .tool_ids
                    .iter()
                    .find(|(_, existing)| existing.as_ref() == id)
                    .map(|(index, _)| *index)
                    .unwrap_or_else(|| self.next_index()),
                None => self.tool_ids.last().map(|(index, _)| *index).unwrap_or(0),
            },
        };
        let id = self.id_for(index, call.id.as_ref().map(JsonStr::as_str));
        let Some(function) = call.function else {
            return;
        };
        if let Some(name) = function.name {
            out.push(StreamEvent::ToolCallStart {
                id: id.clone(),
                name: name.into_text(),
            });
        }
        if let Some(arguments) = function.arguments
            && !arguments.as_str().is_empty()
        {
            out.push(StreamEvent::ToolCallDelta {
                id,
                partial_json: arguments.into_text(),
            });
        }
    }

    /// Folds one streamed reasoning detail into the items held for the
    /// signature.
    ///
    /// Only a text or summary item is parsed, and only so the next piece of the
    /// same kind can fold into it. Every other item keeps its bytes.
    fn accept_detail(&mut self, raw: Box<RawValue>) {
        if self.fold_detail(&raw) {
            return;
        }
        let Some(kind) = detail_kind(&raw) else {
            self.reasoning_details.push(ReasoningDetail::Raw(raw));
            return;
        };
        let map = match serde_json::from_str::<Map<String, Value>>(raw.get()) {
            Ok(map) => map,
            Err(_) => {
                self.reasoning_details.push(ReasoningDetail::Raw(raw));
                return;
            }
        };
        if let Some(ReasoningDetail::Piece(last)) = self.reasoning_details.last_mut()
            && last.kind == kind
        {
            last.merge(map);
            return;
        }
        self.reasoning_details
            .push(ReasoningDetail::Piece(DetailPiece {
                kind,
                map,
                raw,
                merged: false,
            }));
    }

    /// Folds `raw` into the item it continues when the piece carries nothing
    /// the item does not already hold.
    ///
    /// A reasoning stream sends one small text piece per chunk, all of them for
    /// the same item, so this path parses such a piece once and copies only its
    /// text. It answers false for a piece that needs the fields themselves.
    fn fold_detail(&mut self, raw: &RawValue) -> bool {
        let Some(ReasoningDetail::Piece(target)) = self.reasoning_details.last() else {
            return false;
        };
        let Some(peek) = peek_detail(raw.get(), &target.map) else {
            return false;
        };
        let Some(kind) = peek.kind.and_then(DetailKind::of) else {
            return false;
        };
        if peek.extra || target.kind != kind {
            return false;
        }
        let content = match kind {
            DetailKind::Text => peek.text,
            DetailKind::Summary => peek.summary,
        };
        let Some(ReasoningDetail::Piece(piece)) = self.reasoning_details.last_mut() else {
            return false;
        };
        if let Some(content) = content {
            piece.append(content.as_str());
        }
        piece.merged = true;
        true
    }

    /// The reasoning details as one JSON array, taken off the parser.
    ///
    /// An item that never merged keeps the bytes the provider wrote, because a
    /// signed item only replays unchanged. A merged item is written again from
    /// its fields.
    fn take_details(&mut self) -> Option<Text> {
        let details = std::mem::take(&mut self.reasoning_details);
        if details.is_empty() {
            return None;
        }
        let capacity = details
            .iter()
            .map(|detail| detail.raw_len() + 1)
            .sum::<usize>()
            + 1;
        let mut out = String::with_capacity(capacity);
        out.push('[');
        for (index, detail) in details.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            detail.write_into(&mut out);
        }
        out.push(']');
        Some(Text::from(out))
    }

    fn next_index(&self) -> u32 {
        self.tool_ids
            .iter()
            .map(|(index, _)| index + 1)
            .max()
            .unwrap_or(0)
    }

    fn id_for(&mut self, index: u32, id: Option<&str>) -> Text {
        if let Some((_, existing)) = self.tool_ids.iter().find(|(seen, _)| *seen == index) {
            return existing.clone();
        }
        let id = match id {
            Some(id) => Text::from(id),
            None => Text::from(format!("call_{index}")),
        };
        self.tool_ids.push((index, id.clone()));
        id
    }
}

/// One reasoning detail held while the stream runs.
///
/// OpenRouter streams a text or summary item per small piece, so those merge
/// into one item as they arrive. Any other kind, such as a signed-encrypted
/// item, cannot be rebuilt and keeps the provider's bytes.
#[derive(Debug)]
enum ReasoningDetail {
    /// An item replayed byte for byte.
    Raw(Box<RawValue>),
    /// A text or summary item, with room for the pieces that follow it.
    Piece(DetailPiece),
}

impl ReasoningDetail {
    /// The length of the bytes this item started from, for sizing the array.
    fn raw_len(&self) -> usize {
        match self {
            Self::Raw(raw) => raw.get().len(),
            Self::Piece(piece) => piece.raw.get().len(),
        }
    }

    /// Appends this item's JSON to `out`.
    fn write_into(&self, out: &mut String) {
        match self {
            Self::Raw(raw) => out.push_str(raw.get()),
            Self::Piece(piece) if !piece.merged => out.push_str(piece.raw.get()),
            Self::Piece(piece) => out
                .push_str(&serde_json::to_string(&piece.map).expect("a parsed detail serializes")),
        }
    }
}

/// A text or summary item parsed into its fields.
#[derive(Debug)]
struct DetailPiece {
    kind: DetailKind,
    map: Map<String, Value>,
    raw: Box<RawValue>,
    /// Whether a later piece folded into this one.
    merged: bool,
}

impl DetailPiece {
    /// Appends the content a later piece carried to this item's own content.
    fn append(&mut self, more: &str) {
        let field = self.kind.field();
        match self.map.get_mut(field) {
            Some(Value::String(last)) => last.push_str(more),
            _ => {
                self.map
                    .insert(field.to_string(), Value::String(more.to_owned()));
            }
        }
    }

    /// Folds `next` into this item, the way pi folds streamed pieces.
    ///
    /// The content field grows. A field this item already holds wins, and a
    /// field it lacks is filled from `next`, so the signature of a later piece
    /// lands on the item that carries the whole text.
    fn merge(&mut self, mut next: Map<String, Value>) {
        let field = self.kind.field();
        if let Some(Value::String(more)) = next.remove(field) {
            self.append(&more);
        }
        for (key, value) in next {
            if self.map.get(&key).is_none_or(Value::is_null) {
                self.map.insert(key, value);
            }
        }
        self.merged = true;
    }
}

/// The kinds of reasoning detail that arrive as pieces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailKind {
    Text,
    Summary,
}

impl DetailKind {
    fn of(kind: &str) -> Option<Self> {
        match kind {
            "reasoning.text" => Some(Self::Text),
            "reasoning.summary" => Some(Self::Summary),
            _ => None,
        }
    }

    /// The field that carries the piece's content.
    fn field(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Summary => "summary",
        }
    }
}

/// The `type` of a reasoning detail, read without parsing the rest of it.
#[derive(Deserialize)]
struct DetailType<'a> {
    #[serde(default, borrow, rename = "type")]
    kind: Option<&'a str>,
}

fn detail_kind(raw: &RawValue) -> Option<DetailKind> {
    let detail: DetailType<'_> = serde_json::from_str(raw.get()).ok()?;
    DetailKind::of(detail.kind?)
}

/// The fields of one reasoning detail that folding it into an item needs.
#[derive(Default)]
struct DetailPeek<'a> {
    /// The `type` the detail named.
    kind: Option<&'a str>,
    /// The text the detail carried, when that field held a string.
    text: Option<JsonStr<'a>>,
    /// The summary the detail carried, when that field held a string.
    summary: Option<JsonStr<'a>>,
    /// Whether the detail holds a field the item it folds into lacks.
    extra: bool,
}

/// Reads one reasoning detail in one pass: its type, its text and its summary.
///
/// `known` is the item the detail may fold into. A field that item already
/// holds cannot reach it, so the value of such a field is not read at all, and
/// `extra` says whether the detail held one the item needs.
fn peek_detail<'a>(raw: &'a str, known: &Map<String, Value>) -> Option<DetailPeek<'a>> {
    struct Peek<'k>(&'k Map<String, Value>);

    impl<'de> serde::de::Visitor<'de> for Peek<'_> {
        type Value = DetailPeek<'de>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a reasoning detail")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut peek = DetailPeek::default();
            while let Some(key) = map.next_key::<&'de str>()? {
                match key {
                    "type" => peek.kind = map.next_value::<Option<&'de str>>()?,
                    "text" => peek.text = map.next_value::<Option<JsonStr<'de>>>()?,
                    "summary" => peek.summary = map.next_value::<Option<JsonStr<'de>>>()?,
                    _ => {
                        if self.0.get(key).is_none_or(Value::is_null) {
                            peek.extra = true;
                        }
                        map.next_value::<&'de RawValue>()?;
                    }
                }
            }
            // Only the field of the item's own kind is its content, so a detail
            // that carries both needs the values.
            if peek.text.is_some() && peek.summary.is_some() {
                peek.extra = true;
            }
            Ok(peek)
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(raw);
    let peek = serde::de::Deserializer::deserialize_map(&mut deserializer, Peek(known)).ok()?;
    deserializer.end().ok()?;
    Some(peek)
}

impl crate::openai::EventParser for StreamParser {
    fn push_line(&mut self, line: &[u8], out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        StreamParser::push_line(self, line, out)
    }

    fn fail(&mut self) {
        StreamParser::fail(self);
    }

    /// The reasoning details already left as one signature, with `[DONE]`.
    fn finish(&mut self, _out: &mut Vec<StreamEvent>) -> Result<(), Error> {
        StreamParser::finish(self)
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::Error,
    }
}

fn usage_of(json: UsageJson) -> Usage {
    let details = json.prompt_tokens_details;
    // Some gateways send several names at once with only one filled in, so the
    // largest count wins.
    let cache_read = [
        details.cached_tokens,
        details.cache_read_input_tokens,
        json.cache_read_tokens,
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or_default();
    let cache_write = [
        details.cache_write_tokens,
        details.cache_creation_input_tokens,
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or_default();
    // An OpenAI-style `prompt_tokens` counts the tokens that the cache served
    // and the tokens it wrote, so both come off the plain input count.
    Usage {
        input_tokens: json
            .prompt_tokens
            .unwrap_or_default()
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output_tokens: json.completion_tokens.unwrap_or_default(),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: json
            .completion_tokens_details
            .reasoning_tokens
            .unwrap_or_default(),
        cost: json.cost,
    }
}

/// Builds the one session header this provider sends, under `name`.
///
/// The id is the current time in nanoseconds and the process id as lowercase
/// hex. A name or an id the HTTP layer rejects drops the header.
fn session_header(name: &str) -> Option<(HeaderName, HeaderValue)> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let id = format!("reagent-{nanos:x}{:x}", std::process::id());
    Some((
        HeaderName::from_bytes(name.as_bytes()).ok()?,
        HeaderValue::from_str(&id).ok()?,
    ))
}

fn data_of(line: &[u8]) -> Option<&[u8]> {
    let rest = line.strip_prefix(b"data:")?;
    let rest = rest.strip_prefix(b" ").unwrap_or(rest);
    (!rest.is_empty()).then_some(rest)
}

#[derive(Deserialize)]
struct Chunk<'a> {
    #[serde(default, borrow)]
    choices: Choices<'a>,
    #[serde(default)]
    usage: Option<UsageJson>,
    #[serde(default, borrow)]
    error: Option<ApiError<'a>>,
}

/// The choices of one chunk.
///
/// The common reply carries one choice, so the first is held inline and a
/// chunk with a single choice allocates nothing.
#[derive(Default)]
struct Choices<'a> {
    first: Option<Choice<'a>>,
    rest: Vec<Choice<'a>>,
}

impl<'a> Choices<'a> {
    /// Consumes the choices in reply order.
    fn into_choices(self) -> impl Iterator<Item = Choice<'a>> {
        self.first.into_iter().chain(self.rest)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Choices<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ChoicesVisitor<'a>(std::marker::PhantomData<&'a ()>);

        impl<'de: 'a, 'a> serde::de::Visitor<'de> for ChoicesVisitor<'a> {
            type Value = Choices<'a>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a list of choices")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let first = seq.next_element::<Choice<'a>>()?;
                let mut rest = Vec::new();
                while let Some(choice) = seq.next_element::<Choice<'a>>()? {
                    rest.push(choice);
                }
                Ok(Choices { first, rest })
            }
        }

        deserializer.deserialize_seq(ChoicesVisitor(std::marker::PhantomData))
    }
}

#[derive(Deserialize)]
struct Choice<'a> {
    #[serde(default, borrow)]
    delta: Delta<'a>,
    #[serde(default, borrow)]
    finish_reason: Option<&'a str>,
}

#[derive(Default, Deserialize)]
struct Delta<'a> {
    #[serde(default, borrow)]
    content: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    reasoning_content: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    reasoning: Option<JsonStr<'a>>,
    #[serde(default)]
    reasoning_details: Option<Vec<Box<RawValue>>>,
    #[serde(default, borrow, deserialize_with = "null_as_default")]
    tool_calls: Vec<ToolCallDelta<'a>>,
}

#[derive(Deserialize)]
struct ToolCallDelta<'a> {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default, borrow)]
    id: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    function: Option<FunctionDelta<'a>>,
}

#[derive(Deserialize)]
struct FunctionDelta<'a> {
    #[serde(default, borrow)]
    name: Option<JsonStr<'a>>,
    #[serde(default, borrow)]
    arguments: Option<JsonStr<'a>>,
}

#[derive(Deserialize)]
struct ApiError<'a> {
    #[serde(default, borrow)]
    message: JsonStr<'a>,
    #[serde(default, borrow, rename = "type")]
    kind: Option<&'a str>,
}

/// Whether an OpenAI stream error is safe to retry: server or rate-limit
/// family types are transient, everything else is not.
fn retryable(kind: &str) -> bool {
    let kind = kind.to_ascii_lowercase();
    kind.contains("server") || kind.contains("rate_limit") || kind.contains("overload")
}

#[derive(Default, Deserialize)]
struct UsageJson {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    /// The cache read count some gateways put at the top level.
    #[serde(default)]
    cache_read_tokens: Option<u64>,
    /// The USD cost of the reply. OpenRouter reports it, most do not.
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default, deserialize_with = "null_as_default")]
    prompt_tokens_details: PromptDetails,
    #[serde(default, deserialize_with = "null_as_default")]
    completion_tokens_details: CompletionDetails,
}

/// Reads `null` as the type's default: some gateways send a null details
/// object instead of leaving it out.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// The prompt token counts, under every name a gateway uses for them.
///
/// Each count is an `Option`, so a missing field and an explicit null both
/// read as absent.
#[derive(Default, Deserialize)]
struct PromptDetails {
    /// The OpenAI name.
    #[serde(default)]
    cached_tokens: Option<u64>,
    /// The commandcode name for the same count.
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    /// The commandcode name for tokens written to the cache.
    #[serde(default)]
    cache_write_tokens: Option<u64>,
    /// The Anthropic name for tokens written to the cache.
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Default, Deserialize)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use futures_util::{FutureExt, StreamExt};

    use super::*;
    use crate::Request;

    const PROVIDER: &str = "openai-chat";

    fn golden_builder() -> RequestBuilder {
        let tools = vec![
            ToolSpec {
                name: "write".into(),
                description: "Write a file.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            ToolSpec {
                name: "read".into(),
                description: "Read a file.".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        ];
        RequestBuilder::new(
            "glm-4.6",
            1024,
            Some("You are a careful coding agent."),
            Some("medium"),
            &tools,
            MaxTokensField::MaxTokens,
            ReasoningField::Effort,
        )
    }

    fn golden_messages() -> Vec<MessageBody> {
        let mut assistant = AssistantMessage::new(
            PROVIDER,
            "glm-4.6",
            vec![
                Content::text("hi"),
                Content::ToolCall(crate::types::ToolCall {
                    id: "call_01".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                }),
            ],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        [
            Message::user("hello"),
            Message::assistant(assistant),
            Message::tool_result("call_01", "fn main() {}", false),
            Message::user("explain"),
        ]
        .iter()
        .map(|message| MessageBody::new(message, PROVIDER).unwrap())
        .collect()
    }

    fn body_of(messages: &[Message]) -> Bytes {
        let bodies: Vec<MessageBody> = messages
            .iter()
            .map(|message| MessageBody::new(message, PROVIDER).unwrap())
            .collect();
        golden_builder().body(&bodies)
    }

    fn parse_line(parser: &mut StreamParser, line: &[u8]) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        parser.push_line(line, &mut out).expect("line parses");
        out
    }

    fn parse(fixture: &str) -> Vec<StreamEvent> {
        let mut parser = StreamParser::new();
        let mut events = Vec::new();
        for line in fixture.lines() {
            parser
                .push_line(line.as_bytes(), &mut events)
                .expect("fixture parses");
        }
        parser.finish().expect("[DONE] was seen");
        events
    }

    /// The usage event one `data:` line carries, sent at `[DONE]`.
    fn usage_event(line: &[u8]) -> Usage {
        let mut parser = StreamParser::new();
        let mut events = parse_line(&mut parser, line);
        events.extend(parse_line(&mut parser, b"data: [DONE]"));
        events
            .into_iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(usage),
                _ => None,
            })
            .expect("a usage event")
    }

    fn sample_image() -> Image {
        Image {
            media_type: crate::types::ImageType::Png,
            width: 1,
            height: 1,
            data: "aGk=".into(),
        }
    }

    fn user_with_image() -> Message {
        Message::User {
            content: vec![Content::text("look"), Content::Image(sample_image())],
        }
    }

    fn result_with_image() -> Message {
        Message::tool_result_with_images("call_01", "one frame", false, vec![sample_image()])
    }

    /// The body of `messages`, with images sent or replaced by the note.
    fn body_of_images(messages: &[Message], images: bool) -> Bytes {
        let bodies: Vec<MessageBody> = messages
            .iter()
            .map(|message| MessageBody::with_images(message, PROVIDER, images).unwrap())
            .collect();
        golden_builder().body(&bodies)
    }

    const URL: &str = "data:image/png;base64,aGk=";
    const IMAGE_PART: &str =
        r#"{"type":"image_url","image_url":{"url":"data:image/png;base64,aGk="}}"#;

    #[test]
    fn an_image_body_writes_the_user_image_and_one_user_message_for_the_tool_images() {
        let body = body_of_images(
            &[
                Message::user("hello"),
                user_with_image(),
                result_with_image(),
            ],
            true,
        );
        let body = std::str::from_utf8(&body).unwrap();
        assert!(
            body.contains(&format!(
                r#"{{"role":"user","content":[{{"type":"text","text":"look"}},{IMAGE_PART}]}}"#
            )),
            "{body}"
        );
        assert!(
            body.contains(r#"{"role":"tool","tool_call_id":"call_01","content":"one frame"}"#),
            "a tool message stays text: {body}"
        );
        assert!(
            body.contains(&format!(
                r#"{{"role":"user","content":[{{"type":"text","text":"Images from the tool results above:"}},{IMAGE_PART}]}}"#
            )),
            "{body}"
        );
        assert_eq!(body.matches(URL).count(), 2, "{body}");
    }

    #[test]
    fn takes_no_images_writes_the_note_and_no_image_message() {
        let body = body_of_images(&[user_with_image(), result_with_image()], false);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("aGk="), "{body}");
        assert!(
            !body.contains("Images from the tool results above"),
            "no image user message: {body}"
        );
        assert!(
            body.contains(r#""text":"[image not sent: the model takes no images]""#),
            "{body}"
        );
        assert!(
            body.contains(r"one frame\n[image not sent: the model takes no images]"),
            "the note goes in the tool message: {body}"
        );
    }

    #[test]
    fn an_image_message_keeps_its_bytes_across_turns() {
        let provider = OpenAiChat::new("key", "http://localhost");
        let history = [
            Message::user("hello"),
            user_with_image(),
            result_with_image(),
        ];
        let first = provider
            .request_body(&request_for(&history[..2]))
            .expect("the body builds");
        let second = provider
            .request_body(&request_for(&history))
            .expect("the body builds");
        assert!(
            second.starts_with(&first[..first.len() - 2]),
            "the image message keeps its bytes:\n{}\n{}",
            String::from_utf8_lossy(&first),
            String::from_utf8_lossy(&second)
        );
    }

    /// A request over `history` for the local test server.
    fn request_for(history: &[Message]) -> Request<'_> {
        Request {
            model: "test-model",
            max_tokens: 16,
            system: None,
            tools: &[],
            tool_choice: crate::ToolChoice::Auto,
            messages: history,
            images: true,
            reasoning: None,
            cache_key: None,
        }
    }

    /// A compat that asks for a session header under `name`.
    fn session_compat(name: &'static str) -> Compat {
        Compat {
            session_header: Some(name),
            ..Compat::DEFAULT
        }
    }

    #[test]
    fn body_matches_the_golden_request() {
        let body = golden_builder().body(&golden_messages());
        let golden = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_chat_request.json"
        ));
        assert_eq!(std::str::from_utf8(&body).unwrap(), golden.trim_end());
    }

    /// A history with every shape a session holds: text, a reply with thinking
    /// and a tool call, that tool's result, and a text-only reply.
    fn mixed_messages() -> Vec<MessageBody> {
        let mut assistant = AssistantMessage::new(
            PROVIDER,
            "glm-4.6",
            vec![
                Content::Thinking {
                    thinking: "consider the file".into(),
                    signature: Some("sig_1".into()),
                },
                Content::text("reading"),
                Content::ToolCall(crate::types::ToolCall {
                    id: "call_02".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"b.rs\"}".into(),
                }),
            ],
        );
        assistant.stop_reason = Some(StopReason::ToolUse);
        [
            Message::user("hello"),
            Message::assistant(assistant),
            Message::tool_result("call_02", "fn b() {}", false),
            Message::assistant(AssistantMessage::new(
                PROVIDER,
                "glm-4.6",
                vec![Content::text("here it is")],
            )),
        ]
        .iter()
        .map(|message| MessageBody::new(message, PROVIDER).unwrap())
        .collect()
    }

    /// The pieces joined are the bytes the old single buffer wrote, and their
    /// length is the length of that body.
    #[test]
    fn body_pieces_join_to_the_single_buffer_body() {
        let builder = golden_builder();
        let bodies = mixed_messages();
        let parts = builder.body_parts(&bodies);
        let joined = parts.join();
        assert_eq!(
            parts.len(),
            joined.len(),
            "the length of the body is the sum of its pieces"
        );
        assert_eq!(
            joined,
            builder.body(&bodies),
            "the joined pieces are the body"
        );
        assert!(
            serde_json::from_slice::<serde_json::Value>(&joined).is_ok(),
            "the body of a mixed history is one JSON object"
        );

        let golden = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_chat_request.json"
        ));
        let pieces = builder.body_parts(&golden_messages()).join();
        assert_eq!(
            std::str::from_utf8(&pieces).unwrap(),
            golden.trim_end(),
            "the pieces write the bytes the old buffer wrote"
        );
    }

    /// Whether `piece` points into `block`, so the two hold one copy of the
    /// bytes and no copy was made.
    fn shares(piece: &Bytes, block: &Bytes) -> bool {
        let start = block.as_ptr() as usize;
        let at = piece.as_ptr() as usize;
        at >= start && at + piece.len() <= start + block.len()
    }

    /// Every cached message reaches the body as the piece that points into its
    /// bytes, so the body holds no second copy of any message.
    #[test]
    fn body_pieces_share_the_cached_message_bytes() {
        let builder = golden_builder();
        let bodies = mixed_messages();
        let parts = builder.body_parts(&bodies);
        for block in bodies.iter().filter_map(|body| body.bytes.as_ref()) {
            let pieces: Vec<&Bytes> = parts
                .parts()
                .iter()
                .filter(|piece| shares(piece, block))
                .collect();
            assert_eq!(pieces.len(), 1, "one piece holds the message, no copy");
            assert_eq!(
                pieces[0].as_ptr(),
                block.as_ptr(),
                "the piece starts the message"
            );
            assert_eq!(
                pieces[0].len(),
                block.len(),
                "the piece is the whole message"
            );
        }
    }

    #[test]
    fn provider_name_and_base_url_are_configurable() {
        let provider = OpenAiChat::new("key", "http://localhost:11434/v1");
        assert_eq!(provider.name(), PROVIDER);
        assert_eq!(provider.url(), "http://localhost:11434/v1/chat/completions");
    }

    #[test]
    fn the_same_history_gives_byte_identical_prefixes() {
        let builder = golden_builder();
        let messages = golden_messages();
        let first = builder.body(&messages);
        let second = builder.body(&messages);
        assert_eq!(first, second);
        assert!(first.starts_with(builder.head()));
    }

    #[test]
    fn appending_a_message_keeps_the_earlier_bytes() {
        let builder = golden_builder();
        let one = vec![MessageBody::new(&Message::user("hello"), PROVIDER).unwrap()];
        let mut two = one.clone();
        two.push(
            MessageBody::new(
                &Message::assistant(AssistantMessage::new(
                    PROVIDER,
                    "glm-4.6",
                    vec![Content::text("hi")],
                )),
                PROVIDER,
            )
            .unwrap(),
        );

        let short = builder.body(&one);
        let long = builder.body(&two);
        assert!(long.starts_with(&short[..short.len() - 2]));
    }

    #[test]
    fn foreign_thinking_is_dropped_and_own_thinking_round_trips() {
        let foreign = Message::assistant(AssistantMessage::new(
            "anthropic",
            "claude-sonnet-4-5",
            vec![Content::Thinking {
                thinking: "secret".into(),
                signature: Some("sig".into()),
            }],
        ));
        let body = body_of(&[foreign]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret"));
        assert!(!body.contains("reasoning_content"));

        let own = Message::assistant(AssistantMessage::new(
            PROVIDER,
            "glm-4.6",
            vec![
                Content::Thinking {
                    thinking: "kept".into(),
                    signature: None,
                },
                Content::text("visible"),
            ],
        ));
        let body = body_of(&[own]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""reasoning_content":"kept""#));
        assert!(body.contains(r#""content":"visible""#));
    }

    #[test]
    fn an_assistant_with_only_foreign_thinking_disappears() {
        let foreign = AssistantMessage::new(
            "anthropic",
            "claude-sonnet-4-5",
            vec![Content::Thinking {
                thinking: "secret".into(),
                signature: Some("sig".into()),
            }],
        );
        let body = body_of(&[Message::assistant(foreign)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret"));
        assert!(!body.contains("assistant"));
    }

    #[test]
    fn empty_tool_arguments_become_an_empty_object() {
        let assistant = AssistantMessage::new(
            PROVIDER,
            "glm-4.6",
            vec![Content::ToolCall(crate::types::ToolCall {
                id: "call_10".into(),
                name: "read".into(),
                arguments: "".into(),
            })],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(r#""arguments":"{}""#));
    }

    #[test]
    fn chatgpt_preset_chat_wire_writes_no_cap_field() {
        let builder = RequestBuilder::new(
            "m",
            128,
            None,
            None,
            &[],
            MaxTokensField::Omit,
            ReasoningField::Effort,
        );
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(!head.contains("max_tokens"), "{head}");
        assert!(!head.contains("max_completion_tokens"), "{head}");
        assert!(head.contains(r#""stream":true"#), "{head}");
    }

    #[test]
    fn a_request_without_reasoning_has_no_reasoning_fields() {
        let builder = RequestBuilder::new(
            "m",
            128,
            None,
            None,
            &[],
            MaxTokensField::MaxTokens,
            ReasoningField::Effort,
        );
        let head = std::str::from_utf8(builder.head()).unwrap();
        assert!(head.contains(r#""max_tokens":128"#));
        assert!(!head.contains("reasoning_effort"));
        assert!(!head.contains(r#""reasoning""#));
        assert!(!head.contains("tools"));
    }

    /// The head one request over `reasoning` and `field` sends, with no off word.
    fn head_for(reasoning: &str, field: ReasoningField) -> String {
        head_for_off(reasoning, field, None)
    }

    /// The head one request over `reasoning` and `field` sends, for an endpoint
    /// whose word for reasoning off is `off`.
    fn head_for_off(reasoning: &str, field: ReasoningField, off: Option<&'static str>) -> String {
        let builder = RequestBuilder::with_compat(
            "m",
            1024,
            None,
            Some(reasoning),
            &[],
            Compat {
                reasoning_field: field,
                reasoning_off: off,
                ..Compat::DEFAULT
            },
        );
        std::str::from_utf8(builder.head()).unwrap().to_string()
    }

    #[test]
    fn a_level_reasoning_text_is_written_as_reasoning_effort() {
        let head = head_for("xhigh", ReasoningField::Effort);
        assert!(head.contains(r#""reasoning_effort":"xhigh""#), "{head}");
        assert!(!head.contains(r#""reasoning":"#), "{head}");
        assert!(head.contains(r#""max_tokens":1024"#), "{head}");
    }

    #[test]
    fn a_budget_reasoning_text_is_written_as_the_effort_on_the_plain_field() {
        let head = head_for("4096", ReasoningField::Effort);
        assert!(head.contains(r#""reasoning_effort":"4096""#), "{head}");
        assert!(
            head.contains(r#""max_tokens":1024"#),
            "the cap does not grow: {head}"
        );
    }

    #[test]
    fn a_level_reasoning_text_is_written_as_an_effort_object() {
        let head = head_for("high", ReasoningField::EffortObject);
        assert!(head.contains(r#""reasoning":{"effort":"high"}"#), "{head}");
        assert!(!head.contains("reasoning_effort"), "{head}");
        assert!(head.contains(r#""max_tokens":1024"#), "{head}");
    }

    #[test]
    fn a_budget_reasoning_text_is_written_as_a_max_tokens_object() {
        let head = head_for("4096", ReasoningField::EffortObject);
        assert!(
            head.contains(r#""reasoning":{"max_tokens":4096}"#),
            "{head}"
        );
        assert!(
            head.contains(r#""max_tokens":1024"#),
            "the cap does not grow: {head}"
        );
    }

    #[test]
    fn a_none_reasoning_text_turns_reasoning_off() {
        let head = head_for_off("none", ReasoningField::EffortObject, Some("none"));
        assert!(head.contains(r#""reasoning":{"effort":"none"}"#), "{head}");
        let head = head_for("none", ReasoningField::Effort);
        assert!(!head.contains("reasoning"), "{head}");
        let head = head_for("off", ReasoningField::Effort);
        assert!(!head.contains("reasoning"), "{head}");
    }

    #[test]
    fn reasoning_off_effort_field_writes_the_off_word() {
        let head = head_for_off("off", ReasoningField::Effort, Some("off"));
        assert!(head.contains(r#""reasoning_effort":"off""#), "{head}");
        assert!(!head.contains(r#""reasoning":"#), "{head}");
    }

    #[test]
    fn reasoning_off_effort_field_without_a_word_writes_no_reasoning() {
        let head = head_for_off("none", ReasoningField::Effort, None);
        assert!(!head.contains("reasoning"), "{head}");
        assert!(head.contains(r#""max_tokens":1024"#), "{head}");
    }

    #[test]
    fn reasoning_off_effort_object_writes_the_off_word() {
        let head = head_for_off("0", ReasoningField::EffortObject, Some("none"));
        assert!(head.contains(r#""reasoning":{"effort":"none"}"#), "{head}");
        assert!(!head.contains("reasoning_effort"), "{head}");
    }

    #[test]
    fn reasoning_off_effort_object_without_a_word_writes_no_reasoning() {
        let head = head_for_off("off", ReasoningField::EffortObject, None);
        assert!(!head.contains(r#""reasoning""#), "{head}");
    }

    #[test]
    fn reasoning_off_does_not_change_a_level_that_is_on() {
        let head = head_for_off("high", ReasoningField::Effort, Some("off"));
        assert!(head.contains(r#""reasoning_effort":"high""#), "{head}");
    }

    #[test]
    fn a_quoted_reasoning_text_is_escaped() {
        let head = head_for(r#"a"b"#, ReasoningField::Effort);
        assert!(head.contains(r#""reasoning_effort":"a\"b""#), "{head}");
    }

    #[test]
    fn openai_base_url_uses_max_completion_tokens() {
        let history = vec![Message::user("hi")];
        let request = Request {
            model: "gpt-5",
            max_tokens: 128,
            system: None,
            tools: &[],
            tool_choice: crate::ToolChoice::Auto,
            messages: &history,
            images: true,
            reasoning: None,
            cache_key: None,
        };

        let openai = OpenAiChat::new("key", "https://api.openai.com/v1");
        assert!(openai.compat.max_tokens_field == MaxTokensField::MaxCompletionTokens);
        let bytes = openai.request_body(&request).unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains(r#""max_completion_tokens":128"#));
        assert!(!body.contains(r#""max_tokens""#));

        let other = OpenAiChat::new("key", "http://localhost:11434/v1");
        assert!(other.compat == Compat::DEFAULT);
        let bytes = other.request_body(&request).unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains(r#""max_tokens":128"#));
        assert!(!body.contains("max_completion_tokens"));
    }

    #[test]
    fn compat_max_completion_tokens_is_written() {
        let history = vec![Message::user("hi")];
        let request = Request {
            model: "m",
            max_tokens: 64,
            system: None,
            tools: &[],
            tool_choice: crate::ToolChoice::Auto,
            messages: &history,
            images: true,
            reasoning: None,
            cache_key: None,
        };
        let compat = Compat {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Compat::DEFAULT
        };
        let chat = OpenAiChat::new("key", "http://localhost:11434/v1").with_compat(compat);
        let bytes = chat.request_body(&request).unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains(r#""max_completion_tokens":64"#));
        assert!(!body.contains(r#""max_tokens""#));

        let plain =
            OpenAiChat::new("key", "http://localhost:11434/v1").with_compat(Compat::DEFAULT);
        let bytes = plain.request_body(&request).unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains(r#""max_tokens":64"#));
        assert!(!body.contains("max_completion_tokens"));
    }

    #[test]
    fn parses_tool_call_deltas_keyed_by_index() {
        let events = parse(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/openai_chat_tool.sse"
        )));
        let mut arguments = String::new();
        let mut started = None;
        let mut ended = None;
        let mut text = String::new();
        let mut usage = Usage::default();
        for event in &events {
            match event {
                StreamEvent::ToolCallStart { id, name } => {
                    started = Some((id.to_string(), name.to_string()));
                }
                StreamEvent::ToolCallDelta { partial_json, .. } => {
                    arguments.push_str(partial_json);
                }
                StreamEvent::ToolCallEnd { id } => ended = Some(id.to_string()),
                StreamEvent::TextDelta(delta) => text.push_str(delta),
                StreamEvent::Usage(seen) => usage = *seen,
                _ => {}
            }
        }
        assert_eq!(text, "I will read it.");
        assert_eq!(started, Some(("call_02".to_string(), "read".to_string())));
        assert_eq!(ended, Some("call_02".to_string()));
        assert_eq!(arguments, "{\"path\":\"a.rs\"}");
        assert_eq!(usage.input_tokens, 40);
        assert_eq!(usage.cache_read_tokens, 120);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(events.last(), Some(&StreamEvent::Stop(StopReason::ToolUse)));
    }

    #[test]
    fn usage_cost_is_parsed_from_the_chat_usage() {
        let mut parser = StreamParser::new();
        let mut events = parse_line(
            &mut parser,
            br#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"cost":0.0123}}"#,
        );
        events.extend(parse_line(&mut parser, b"data: [DONE]"));
        let usage = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(*usage),
                _ => None,
            })
            .expect("a usage event");
        assert_eq!(usage.cost, Some(0.0123));
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 2);
    }

    #[test]
    fn usage_cost_is_none_when_the_chat_usage_has_none() {
        let mut parser = StreamParser::new();
        let mut events = parse_line(
            &mut parser,
            br#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
        );
        events.extend(parse_line(&mut parser, b"data: [DONE]"));
        let usage = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(*usage),
                _ => None,
            })
            .expect("a usage event");
        assert_eq!(usage.cost, None);
    }

    #[test]
    fn gateway_usage_takes_the_filled_name_when_several_are_sent() {
        let usage = usage_event(
            br#"data: {"choices":[],"usage":{"prompt_tokens":200,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":0,"cache_read_input_tokens":120,"cache_write_tokens":30,"cache_creation_input_tokens":0}}}"#,
        );
        assert_eq!(usage.cache_read_tokens, 120);
        assert_eq!(usage.cache_write_tokens, 30);
        assert_eq!(usage.input_tokens, 50);
    }

    #[test]
    fn gateway_usage_reads_commandcode_cache_names() {
        let usage = usage_event(
            br#"data: {"choices":[],"usage":{"prompt_tokens":200,"completion_tokens":20,"prompt_tokens_details":{"cache_read_input_tokens":120,"cache_write_tokens":30}}}"#,
        );
        assert_eq!(usage.cache_read_tokens, 120);
        assert_eq!(usage.cache_write_tokens, 30);
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 20);
    }

    #[test]
    fn gateway_usage_reads_the_anthropic_cache_write_name() {
        let usage = usage_event(
            br#"data: {"choices":[],"usage":{"prompt_tokens":90,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":30,"cache_creation_input_tokens":20}}}"#,
        );
        assert_eq!(usage.cache_read_tokens, 30);
        assert_eq!(usage.cache_write_tokens, 20);
        assert_eq!(usage.input_tokens, 40);
    }

    #[test]
    fn gateway_usage_reads_a_top_level_cache_read() {
        let usage = usage_event(
            br#"data: {"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"cache_read_tokens":60}}"#,
        );
        assert_eq!(usage.cache_read_tokens, 60);
        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(usage.input_tokens, 40);
    }

    #[test]
    fn gateway_usage_accepts_null_cache_fields() {
        let usage = usage_event(
            br#"data: {"choices":[],"usage":{"prompt_tokens":15,"completion_tokens":8,"total_tokens":23,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":null},"completion_tokens_details":{"reasoning_tokens":8}}}"#,
        );
        assert_eq!(usage.input_tokens, 15);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(usage.reasoning_tokens, 8);
        assert_eq!(usage.cost, None);
    }

    #[test]
    fn gateway_usage_accepts_null_detail_objects() {
        let usage = usage_event(
            br#"data: {"choices":[],"usage":{"prompt_tokens":15,"completion_tokens":8,"prompt_tokens_details":null,"completion_tokens_details":null}}"#,
        );
        assert_eq!(usage.input_tokens, 15);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.reasoning_tokens, 0);
    }

    #[test]
    fn gateway_text_beside_a_null_tool_calls_list() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"choices":[{"index":0,"delta":{"role":null,"content":"Ok","reasoning_content":null,"tool_calls":null},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, line),
            vec![StreamEvent::TextDelta("Ok".into())]
        );
    }

    #[test]
    fn gateway_usage_in_every_chunk_counts_once() {
        let mut parser = StreamParser::new();
        let lines: [&[u8]; 4] = [
            br#"data: {"choices":[{"index":0,"delta":{"content":"O"}}],"usage":{"prompt_tokens":157,"completion_tokens":1}}"#,
            br#"data: {"choices":[{"index":0,"delta":{"content":"k"}}],"usage":{"prompt_tokens":157,"completion_tokens":2}}"#,
            br#"data: {"choices":[],"usage":{"prompt_tokens":157,"completion_tokens":3}}"#,
            b"data: [DONE]",
        ];
        let usages: Vec<Usage> = lines
            .into_iter()
            .flat_map(|line| parse_line(&mut parser, line))
            .filter_map(|event| match event {
                StreamEvent::Usage(usage) => Some(usage),
                _ => None,
            })
            .collect();
        assert_eq!(usages.len(), 1, "{usages:?}");
        assert_eq!(usages[0].input_tokens, 157);
        assert_eq!(usages[0].output_tokens, 3);
    }

    #[test]
    fn maps_reasoning_content_to_thinking_deltas() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"hmm"},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, line),
            vec![StreamEvent::ThinkingDelta("hmm".into())]
        );
    }

    /// Folds stream events into the thinking block an assistant message keeps.
    fn assembled_thinking(events: &[StreamEvent]) -> Content {
        let mut thinking = String::new();
        let mut signature = None;
        for event in events {
            match event {
                StreamEvent::ThinkingDelta(delta) => thinking.push_str(delta),
                StreamEvent::ThinkingSignature(sig) => signature = Some(sig.clone()),
                _ => {}
            }
        }
        Content::Thinking {
            thinking: thinking.into(),
            signature,
        }
    }

    /// The signature the stream left on its thinking block.
    fn signature_of(events: &[StreamEvent]) -> String {
        events
            .iter()
            .find_map(|event| match event {
                StreamEvent::ThinkingSignature(signature) => Some(signature.to_string()),
                _ => None,
            })
            .expect("the stream carried a signature")
    }

    /// The signature parsed back as its array of detail items.
    fn signature_items(events: &[StreamEvent]) -> Vec<serde_json::Value> {
        serde_json::from_str(&signature_of(events)).expect("the signature is a JSON array")
    }

    #[test]
    fn chat_thinking_reads_the_reasoning_field() {
        let mut parser = StreamParser::new();
        let line =
            br#"data: {"choices":[{"index":0,"delta":{"reasoning":"hm"},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, line),
            vec![StreamEvent::ThinkingDelta("hm".into())]
        );
    }

    #[test]
    fn chat_thinking_prefers_reasoning_content_when_both_are_present() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"choices":[{"index":0,"delta":{"reasoning":"loose","reasoning_content":"kept"},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, line),
            vec![StreamEvent::ThinkingDelta("kept".into())]
        );
    }

    #[test]
    fn chat_thinking_keeps_reasoning_details_as_the_signature() {
        let events = parse(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"hm\",\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"hm\",\"signature\":\"sig-1\",\"format\":\"openai\",\"index\":0}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.encrypted\",\"data\":\"abc\"}]},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        ));
        let signature = concat!(
            r#"[{"type":"reasoning.text","text":"hm","signature":"sig-1","format":"openai","index":0},"#,
            r#"{"type":"reasoning.encrypted","data":"abc"}]"#
        );
        assert_eq!(
            assembled_thinking(&events),
            Content::Thinking {
                thinking: "hm".into(),
                signature: Some(signature.into()),
            }
        );

        let events = parse(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.summary\",\"summary\":\"done\"}]},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        ));
        assert_eq!(
            assembled_thinking(&events),
            Content::Thinking {
                thinking: "".into(),
                signature: Some(r#"[{"type":"reasoning.summary","summary":"done"}]"#.into()),
            }
        );
    }

    #[test]
    fn chat_thinking_merges_text_pieces() {
        let events = parse(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"a\",\"index\":0}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"b\",\"index\":0}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"c\",\"index\":0,\"signature\":\"sig-1\"}]},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        ));
        let items = signature_items(&events);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0]["type"], "reasoning.text");
        assert_eq!(items[0]["text"], "abc");
        assert_eq!(items[0]["signature"], "sig-1");
        assert_eq!(items[0]["index"], 0);
    }

    #[test]
    fn chat_thinking_keeps_encrypted_items_apart() {
        let encrypted = r#"{"type":"reasoning.encrypted","data":"abc"}"#;
        let events = parse(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"a\"}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.encrypted\",\"data\":\"abc\"}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"b\"}]},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        ));
        let signature = signature_of(&events);
        let items = signature_items(&events);
        assert_eq!(items.len(), 3, "{signature}");
        assert_eq!(items[0]["text"], "a");
        assert_eq!(items[2]["text"], "b");
        assert!(
            signature.contains(encrypted),
            "the encrypted item keeps its bytes: {signature}"
        );
    }

    #[test]
    fn chat_thinking_merges_summary_pieces() {
        let events = parse(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.summary\",\"summary\":\"one\"}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.summary\",\"summary\":\" two\",\"format\":\"openai\"}]},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        ));
        let items = signature_items(&events);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0]["type"], "reasoning.summary");
        assert_eq!(items[0]["summary"], "one two");
        assert_eq!(items[0]["format"], "openai");
    }

    #[test]
    fn chat_thinking_replays_reasoning_details() {
        let signature = concat!(
            r#"[{"type":"reasoning.text","text":"hm","signature":"sig-1","format":"openai","index":0},"#,
            r#"{"type":"reasoning.encrypted","data":"abc"}]"#
        );
        let assistant = AssistantMessage::new(
            PROVIDER,
            "glm-4.6",
            vec![
                Content::Thinking {
                    thinking: "hm".into(),
                    signature: Some(signature.into()),
                },
                Content::text("hi"),
            ],
        );
        let body = body_of(&[Message::assistant(assistant)]);
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains(&format!(r#""reasoning_details":{signature}"#)));
        assert!(body.contains(r#""reasoning_content":"hm""#));
    }

    #[test]
    fn chat_thinking_skips_an_invalid_details_signature() {
        for signature in ["not json", r#"{"type":"reasoning.text"}"#] {
            let assistant = AssistantMessage::new(
                PROVIDER,
                "glm-4.6",
                vec![
                    Content::Thinking {
                        thinking: "hm".into(),
                        signature: Some(signature.into()),
                    },
                    Content::text("hi"),
                ],
            );
            let body = body_of(&[Message::assistant(assistant)]);
            let body = std::str::from_utf8(&body).unwrap();
            assert!(!body.contains("reasoning_details"), "{body}");
            assert!(body.contains(r#""reasoning_content":"hm""#));
        }
    }

    #[test]
    fn push_line_appends_to_the_callers_buffer() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let first =
            br#"data: {"choices":[{"index":0,"delta":{"content":"a"},"finish_reason":null}]}"#;
        let second =
            br#"data: {"choices":[{"index":0,"delta":{"content":"b"},"finish_reason":null}]}"#;
        parser.push_line(first, &mut out).unwrap();
        assert_eq!(out, vec![StreamEvent::TextDelta("a".into())]);
        parser.push_line(second, &mut out).unwrap();
        assert_eq!(
            out,
            vec![
                StreamEvent::TextDelta("a".into()),
                StreamEvent::TextDelta("b".into())
            ],
            "the second line extends the same buffer"
        );
    }

    /// Some hosts send an empty `reasoning_content` with every content chunk;
    /// an empty thinking delta would split the reply text in a client.
    #[test]
    fn an_empty_reasoning_field_gives_no_thinking_delta() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"","content":"Do"},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, line),
            vec![StreamEvent::TextDelta("Do".into())]
        );
        let line = br#"data: {"choices":[{"index":0,"delta":{"reasoning":"","content":"it"},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, line),
            vec![StreamEvent::TextDelta("it".into())]
        );
    }

    #[test]
    fn a_tool_call_without_an_index_uses_the_next_index() {
        let mut parser = StreamParser::new();
        let first = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_a","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, first),
            vec![StreamEvent::ToolCallStart {
                id: "call_a".into(),
                name: "read".into()
            }]
        );
        let second = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"{}"}}]},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, second),
            vec![StreamEvent::ToolCallDelta {
                id: "call_a".into(),
                partial_json: "{}".into()
            }]
        );
        let third = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_b","function":{"name":"write","arguments":""}}]},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, third),
            vec![StreamEvent::ToolCallStart {
                id: "call_b".into(),
                name: "write".into()
            }]
        );
    }

    #[test]
    fn a_tool_call_without_an_id_gets_a_synthetic_one() {
        let mut parser = StreamParser::new();
        let start = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, start),
            vec![StreamEvent::ToolCallStart {
                id: "call_0".into(),
                name: "read".into()
            }]
        );
        let done = br#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
        assert_eq!(
            parse_line(&mut parser, done),
            vec![StreamEvent::ToolCallEnd {
                id: "call_0".into()
            }]
        );
    }

    #[test]
    fn a_rate_limit_error_chunk_is_transient() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"error":{"message":"rate limited","type":"rate_limit_error"}}"#;
        let mut out = Vec::new();
        let error = parser.push_line(line, &mut out).unwrap_err();
        assert!(matches!(error, Error::Transient(message) if message == "rate limited"));
    }

    #[test]
    fn a_non_retryable_error_chunk_is_a_protocol_error() {
        let mut parser = StreamParser::new();
        let line = br#"data: {"error":{"message":"bad request","type":"invalid_request_error"}}"#;
        let mut out = Vec::new();
        let error = parser.push_line(line, &mut out).unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message == "bad request"));
    }

    #[test]
    fn an_error_ends_the_stream() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        let error = parser
            .push_line(br#"data: {"error":{"message":"boom"}}"#, &mut out)
            .unwrap_err();
        assert!(matches!(error, Error::Protocol(message) if message == "boom"));
        let mut late = Vec::new();
        parser
            .push_line(
                br#"data: {"choices":[{"index":0,"delta":{"content":"late"},"finish_reason":null}]}"#,
                &mut late,
            )
            .unwrap();
        assert!(late.is_empty(), "no event follows an error");
        parser.finish().expect("finish is quiet after an error");
    }

    #[test]
    fn two_tool_calls_keep_their_own_ids() {
        let mut parser = StreamParser::new();
        let first = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read","arguments":""}},{"index":1,"id":"call_b","function":{"name":"write","arguments":""}}]},"finish_reason":null}]}"#;
        let events = parse_line(&mut parser, first);
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallStart {
                    id: "call_a".into(),
                    name: "read".into()
                },
                StreamEvent::ToolCallStart {
                    id: "call_b".into(),
                    name: "write".into()
                },
            ]
        );
        let second = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{}"}}]},"finish_reason":null}]}"#;
        assert_eq!(
            parse_line(&mut parser, second),
            vec![StreamEvent::ToolCallDelta {
                id: "call_b".into(),
                partial_json: "{}".into()
            }]
        );
    }

    #[test]
    fn cut_stream_without_done_is_transient() {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        parser
            .push_line(
                br#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
                &mut out,
            )
            .unwrap();
        let error = parser.finish().unwrap_err();
        assert!(matches!(error, Error::Transient(message) if message.contains("[DONE]")));
    }

    #[test]
    fn request_body_is_stable_for_the_same_history() {
        let provider = OpenAiChat::new("key", "http://localhost");
        let history = vec![Message::user("one")];
        let request = Request {
            model: "glm-4.6",
            max_tokens: 128,
            system: None,
            tools: &[],
            tool_choice: crate::ToolChoice::Auto,
            messages: &history,
            images: true,
            reasoning: None,
            cache_key: None,
        };
        let first = provider.request_body(&request).unwrap();
        let second = provider.request_body(&request).unwrap();
        assert_eq!(first, second);
    }

    /// The body leaves in pieces, so its length is the `content-length` header
    /// and the request is not chunked.
    #[tokio::test]
    async fn body_pieces_send_the_same_bytes_with_their_length() {
        let (base, recorded) = crate::http::test_server::serve(EXTRA_REPLY).await;
        let provider = OpenAiChat::new("test-key", base);
        let history = vec![
            Message::user("hello"),
            Message::tool_result("call_01", "fn main() {}", false),
        ];
        let request = request_for(&history);
        let body = provider.request_body(&request).expect("the body builds");
        let events: Vec<_> = provider.stream(&request).collect().await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let recorded = recorded.await.expect("the server recorded the request");
        let length: usize = recorded
            .header("content-length")
            .expect("a content length")
            .parse()
            .expect("a number");
        assert_eq!(length, body.len(), "the length of the body that is sent");
        assert_eq!(recorded.body, body.as_ref(), "the bytes the server read");
        assert_eq!(
            recorded.header("transfer-encoding"),
            None,
            "a body with a known length is not chunked"
        );
    }

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

    const EXTRA_REPLY: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    #[tokio::test]
    async fn extra_headers_reach_the_chat_request() {
        let (base, recorded) = crate::http::test_server::serve(EXTRA_REPLY).await;
        let provider =
            OpenAiChat::new("test-key", base).with_headers(vec![("x-extra".into(), "yes".into())]);
        let request = Request {
            model: "test-model",
            max_tokens: 16,
            system: None,
            tools: &[],
            tool_choice: crate::ToolChoice::Auto,
            messages: &[],
            images: true,
            reasoning: None,
            cache_key: None,
        };
        let events: Vec<_> = provider.stream(&request).collect().await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let recorded = recorded.await.expect("the server recorded the request");
        assert_eq!(recorded.request_line, "POST /chat/completions HTTP/1.1");
        assert_eq!(recorded.header("x-extra"), Some("yes"));
        assert_eq!(recorded.header("authorization"), Some("Bearer test-key"));
    }

    #[tokio::test]
    async fn gateway_session_header_is_sent_and_stable() {
        let (base, mut recorded) = crate::http::test_server::serve_many(2, EXTRA_REPLY).await;
        let provider =
            OpenAiChat::new("test-key", base).with_compat(session_compat("x-opencode-session"));
        let history = vec![Message::user("hi")];
        let request = request_for(&history);
        for _ in 0..2 {
            let events: Vec<_> = provider.stream(&request).collect().await;
            assert!(events.iter().all(Result::is_ok), "{events:?}");
        }
        let mut values = Vec::new();
        for _ in 0..2 {
            let recorded = recorded
                .recv()
                .await
                .expect("the server recorded the request");
            let value = recorded
                .header("x-opencode-session")
                .expect("a session header")
                .to_owned();
            assert!(value.starts_with("reagent-"), "{value}");
            assert!(value.len() > "reagent-".len(), "{value}");
            values.push(value);
        }
        assert_eq!(values[0], values[1], "one id for the whole run");
    }

    #[tokio::test]
    async fn gateway_no_session_header_by_default() {
        let (base, recorded) = crate::http::test_server::serve(EXTRA_REPLY).await;
        let provider = OpenAiChat::new("test-key", base);
        let history = vec![Message::user("hi")];
        let request = request_for(&history);
        let events: Vec<_> = provider.stream(&request).collect().await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let recorded = recorded.await.expect("the server recorded the request");
        assert_eq!(recorded.header("x-opencode-session"), None);
    }

    #[tokio::test]
    async fn gateway_a_user_header_wins_over_the_session_id() {
        let (base, recorded) = crate::http::test_server::serve(EXTRA_REPLY).await;
        let provider = OpenAiChat::new("test-key", base)
            .with_headers(vec![("x-opencode-session".into(), "user-id".into())])
            .with_compat(session_compat("x-opencode-session"));
        let history = vec![Message::user("hi")];
        let request = request_for(&history);
        let events: Vec<_> = provider.stream(&request).collect().await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let recorded = recorded.await.expect("the server recorded the request");
        let values: Vec<&str> = recorded
            .headers
            .iter()
            .filter(|(name, _)| name == "x-opencode-session")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(values, vec!["user-id"]);
    }
}
